use std::sync::Arc;

use k8s_openapi::api::core::v1::{ContainerStatus, PersistentVolumeClaim, Pod, PodSpec, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k10s_core::{BUILTIN_TOOLS, KindId, Role, Severity, ToolId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub severity: Severity,
    pub display: Arc<str>,
}

impl Reason {
    fn new(severity: Severity, display: &str) -> Reason {
        Reason {
            severity,
            display: display.into(),
        }
    }

    fn reported(display: &str) -> Reason {
        Reason::new(severity_of_reason(display), display)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachRef {
    pub kind: KindId,
    pub name: Arc<str>,
}

pub type Labels = Vec<(Arc<str>, Arc<str>)>;

#[derive(Debug, Clone, PartialEq)]
pub enum Detail {
    Scope,
    Owner {
        tool: ToolId,
    },
    Instance {
        reason: Reason,
        labels: Labels,
        refs: Vec<AttachRef>,
    },
    Attached {
        detail: Arc<str>,
        selector: Labels,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Controller {
    pub uid: Arc<str>,
    pub kind: Arc<str>,
    pub name: Arc<str>,
    pub api_version: Arc<str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Staged {
    pub kind: KindId,
    pub role: Role,
    pub uid: Arc<str>,
    pub namespace: Arc<str>,
    pub name: Arc<str>,
    pub resource_version: u64,
    pub controller: Option<Controller>,
    pub detail: Detail,
}

pub fn resource_version(meta: &ObjectMeta) -> u64 {
    meta.resource_version
        .as_deref()
        .and_then(|rv| rv.parse::<u64>().ok())
        .unwrap_or(0)
}

pub fn controller_of(meta: &ObjectMeta) -> Option<Controller> {
    meta.owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|r| r.controller.unwrap_or(false))
        .map(|r| Controller {
            uid: r.uid.as_str().into(),
            kind: r.kind.as_str().into(),
            name: r.name.as_str().into(),
            api_version: r.api_version.as_str().into(),
        })
}

fn labels_of(meta: &ObjectMeta) -> Labels {
    let mut out: Labels = meta
        .labels
        .iter()
        .flatten()
        .map(|(k, v)| (Arc::from(k.as_str()), Arc::from(v.as_str())))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

const REASON_SEVERITY: &[(&str, Severity)] = &[
    ("Running", Severity::Ok),
    ("Completed", Severity::Ok),
    ("Succeeded", Severity::Ok),
    ("PodCompleted", Severity::Ok),
    ("ContainerReady", Severity::Ok),
    ("Pending", Severity::Warn),
    ("ContainerCreating", Severity::Warn),
    ("PodInitializing", Severity::Warn),
    ("Progressing", Severity::Warn),
    ("Terminating", Severity::Warn),
    ("NotReady", Severity::Warn),
    ("Unschedulable", Severity::Warn),
    ("SchedulingGated", Severity::Warn),
    ("Terminated", Severity::Warn),
    ("CrashLoopBackOff", Severity::Err),
    ("ImagePullBackOff", Severity::Err),
    ("ErrImagePull", Severity::Err),
    ("ErrImageNeverPull", Severity::Err),
    ("InvalidImageName", Severity::Err),
    ("ImageInspectError", Severity::Err),
    ("RegistryUnavailable", Severity::Err),
    ("CreateContainerConfigError", Severity::Err),
    ("CreateContainerError", Severity::Err),
    ("RunContainerError", Severity::Err),
    ("PostStartHookError", Severity::Err),
    ("StartError", Severity::Err),
    ("ContainerCannotRun", Severity::Err),
    ("ContainerStatusUnknown", Severity::Err),
    ("OOMKilled", Severity::Err),
    ("Error", Severity::Err),
    ("DeadlineExceeded", Severity::Err),
    ("Evicted", Severity::Err),
    ("Preempting", Severity::Err),
    ("NodeLost", Severity::Err),
    ("NodeShutdown", Severity::Err),
    ("NodeAffinity", Severity::Err),
    ("Shutdown", Severity::Err),
    ("UnexpectedAdmissionError", Severity::Err),
    ("Failed", Severity::Err),
    ("Unknown", Severity::Unknown),
];

pub fn known_reasons() -> impl Iterator<Item = &'static str> {
    REASON_SEVERITY.iter().map(|(name, _)| *name)
}

pub fn severity_of_reason(reason: &str) -> Severity {
    REASON_SEVERITY
        .iter()
        .find(|(name, _)| *name == reason)
        .map(|(_, severity)| *severity)
        .unwrap_or(Severity::Unknown)
}

pub fn pod_reason(pod: &Pod) -> Reason {
    if pod.metadata.deletion_timestamp.is_some() {
        return Reason::reported("Terminating");
    }

    let Some(status) = &pod.status else {
        return Reason::reported("Pending");
    };

    if let Some(reason) = status.reason.as_deref()
        && !reason.is_empty()
    {
        return Reason::reported(reason);
    }

    let phase = status.phase.as_deref().unwrap_or("");

    let mut worst: Option<Reason> = None;
    for cs in container_statuses(status) {
        if let Some(candidate) = container_reason(cs, phase)
            && worst
                .as_ref()
                .is_none_or(|w| candidate.severity > w.severity)
        {
            worst = Some(candidate);
        }
    }
    if let Some(worst) = worst
        && worst.severity.is_unhealthy()
    {
        return worst;
    }

    match phase {
        "Succeeded" => Reason::reported("Succeeded"),
        "Failed" => Reason::reported("Failed"),
        "Pending" => Reason::reported("Pending"),
        "Running" => {
            if pod_is_ready(status) {
                Reason::reported("Running")
            } else {
                Reason::reported("NotReady")
            }
        }
        _ => Reason::new(Severity::Unknown, "Unknown"),
    }
}

fn container_statuses(
    status: &k8s_openapi::api::core::v1::PodStatus,
) -> impl Iterator<Item = &ContainerStatus> {
    status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten())
        .chain(status.ephemeral_container_statuses.iter().flatten())
}

fn container_reason(cs: &ContainerStatus, phase: &str) -> Option<Reason> {
    let state = cs.state.as_ref()?;
    if let Some(waiting) = &state.waiting {
        let reason = waiting.reason.as_deref().filter(|r| !r.is_empty())?;
        return Some(Reason::reported(reason));
    }
    if let Some(terminated) = &state.terminated {
        let reason = terminated
            .reason
            .as_deref()
            .filter(|r| !r.is_empty())
            .unwrap_or(if terminated.exit_code == 0 {
                "Completed"
            } else {
                "Error"
            });
        if reason == "Completed" && (phase == "Succeeded" || cs.restart_count == 0) {
            return None;
        }
        return Some(Reason::reported(reason));
    }
    None
}

fn pod_is_ready(status: &k8s_openapi::api::core::v1::PodStatus) -> bool {
    if let Some(conditions) = &status.conditions
        && let Some(ready) = conditions.iter().find(|c| c.type_ == "Ready")
    {
        return ready.status == "True";
    }
    status
        .container_statuses
        .iter()
        .flatten()
        .all(|cs| cs.ready)
}

pub fn tool_of(meta: &ObjectMeta) -> ToolId {
    const KEYS: &[&str] = &[
        "app.kubernetes.io/name",
        "app.kubernetes.io/part-of",
        "app.kubernetes.io/component",
        "app",
        "k8s-app",
    ];
    let labels = meta.labels.as_ref();
    for key in KEYS {
        let Some(value) = labels.and_then(|l| l.get(*key)) else {
            continue;
        };
        if let Some(tool) = tool_from_value(value) {
            return tool;
        }
    }
    ToolId::NONE
}

fn tool_from_value(value: &str) -> Option<ToolId> {
    let normal = normalize(value);
    if let Some(tool) = builtin_tool(&normal) {
        return Some(tool);
    }
    if let Some(alias) = TOOL_ALIASES
        .iter()
        .find(|(from, _)| *from == normal)
        .map(|(_, to)| *to)
    {
        return builtin_tool(alias);
    }
    value
        .split(['-', '_', '.', '/'])
        .map(normalize)
        .find_map(|token| builtin_tool(&token))
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn builtin_tool(normal: &str) -> Option<ToolId> {
    if normal.is_empty() || normal == "none" {
        return None;
    }
    BUILTIN_TOOLS
        .iter()
        .position(|t| t.slug == normal)
        .map(|i| ToolId(i as u16))
}

const TOOL_ALIASES: &[(&str, &str)] = &[
    ("postgresql", "postgres"),
    ("pgbouncer", "postgres"),
    ("timescaledb", "postgres"),
    ("mongo", "mongodb"),
    ("elastic", "elasticsearch"),
    ("kubeprometheusstack", "prometheus"),
    ("prometheusoperator", "prometheus"),
    ("alertmanager", "prometheus"),
    ("thanos", "prometheus"),
    ("otelcollector", "opentelemetry"),
    ("otel", "opentelemetry"),
    ("opentelemetrycollector", "opentelemetry"),
    ("ingressnginx", "nginx"),
    ("nginxingress", "nginx"),
    ("argo", "argocd"),
    ("argocdserver", "argocd"),
    ("fluxcd", "flux"),
    ("istiod", "istio"),
    ("envoyproxy", "envoy"),
    ("hashicorpvault", "vault"),
    ("rabbit", "rabbitmq"),
];

pub fn stage_meta(kind: KindId, role: Role, meta: &ObjectMeta) -> Option<Staged> {
    let uid: Arc<str> = meta.uid.as_deref()?.into();
    let name: Arc<str> = meta.name.as_deref()?.into();
    let detail = match role {
        Role::Scope => Detail::Scope,
        Role::Owner => Detail::Owner {
            tool: tool_of(meta),
        },
        Role::Instance => Detail::Instance {
            reason: Reason::new(Severity::Unknown, "Unknown"),
            labels: labels_of(meta),
            refs: Vec::new(),
        },
        Role::Attached => Detail::Attached {
            detail: Arc::from(""),
            selector: Vec::new(),
        },
    };
    Some(Staged {
        kind,
        role,
        uid,
        namespace: meta.namespace.as_deref().unwrap_or("").into(),
        name,
        resource_version: resource_version(meta),
        controller: controller_of(meta),
        detail,
    })
}

pub fn stage_pod(kind: KindId, attach_kinds: &AttachKinds, pod: &Pod) -> Option<Staged> {
    let mut staged = stage_meta(kind, Role::Instance, &pod.metadata)?;
    staged.detail = Detail::Instance {
        reason: pod_reason(pod),
        labels: labels_of(&pod.metadata),
        refs: pod
            .spec
            .as_ref()
            .map(|spec| attachment_refs(spec, attach_kinds))
            .unwrap_or_default(),
    };
    Some(staged)
}

#[derive(Debug, Clone, Copy)]
pub struct AttachKinds {
    pub config_map: KindId,
    pub secret: KindId,
    pub volume: KindId,
}

impl Default for AttachKinds {
    fn default() -> Self {
        AttachKinds {
            config_map: KindId::CONFIG_MAP,
            secret: KindId::SECRET,
            volume: KindId::VOLUME,
        }
    }
}

pub fn attachment_refs(spec: &PodSpec, kinds: &AttachKinds) -> Vec<AttachRef> {
    let mut out: Vec<AttachRef> = Vec::new();
    let mut push = |kind: KindId, name: &str| {
        if name.is_empty() {
            return;
        }
        let r = AttachRef {
            kind,
            name: name.into(),
        };
        if !out.contains(&r) {
            out.push(r);
        }
    };

    for volume in spec.volumes.iter().flatten() {
        if let Some(cm) = &volume.config_map {
            push(kinds.config_map, &cm.name);
        }
        if let Some(secret) = &volume.secret
            && let Some(name) = &secret.secret_name
        {
            push(kinds.secret, name);
        }
        if let Some(pvc) = &volume.persistent_volume_claim {
            push(kinds.volume, &pvc.claim_name);
        }
        for source in volume
            .projected
            .iter()
            .flat_map(|p| p.sources.iter().flatten())
        {
            if let Some(cm) = &source.config_map {
                push(kinds.config_map, &cm.name);
            }
            if let Some(secret) = &source.secret {
                push(kinds.secret, &secret.name);
            }
        }
    }

    let env_sources = spec
        .containers
        .iter()
        .chain(spec.init_containers.iter().flatten())
        .map(|container| (&container.env_from, &container.env))
        .chain(
            spec.ephemeral_containers
                .iter()
                .flatten()
                .map(|container| (&container.env_from, &container.env)),
        );
    for (env_from, env) in env_sources {
        for from in env_from.iter().flatten() {
            if let Some(cm) = &from.config_map_ref {
                push(kinds.config_map, &cm.name);
            }
            if let Some(secret) = &from.secret_ref {
                push(kinds.secret, &secret.name);
            }
        }
        for env in env.iter().flatten() {
            let Some(from) = &env.value_from else {
                continue;
            };
            if let Some(cm) = &from.config_map_key_ref {
                push(kinds.config_map, &cm.name);
            }
            if let Some(secret) = &from.secret_key_ref {
                push(kinds.secret, &secret.name);
            }
        }
    }

    for pull in spec.image_pull_secrets.iter().flatten() {
        push(kinds.secret, &pull.name);
    }

    out
}

pub fn stage_service(kind: KindId, svc: &Service) -> Option<Staged> {
    let mut staged = stage_meta(kind, Role::Attached, &svc.metadata)?;
    let spec = svc.spec.as_ref();
    let mut selector: Labels = spec
        .and_then(|s| s.selector.as_ref())
        .into_iter()
        .flatten()
        .map(|(k, v)| (Arc::from(k.as_str()), Arc::from(v.as_str())))
        .collect();
    selector.sort_by(|a, b| a.0.cmp(&b.0));
    staged.detail = Detail::Attached {
        detail: service_detail(svc),
        selector,
    };
    Some(staged)
}

pub fn service_detail(svc: &Service) -> Arc<str> {
    let Some(spec) = &svc.spec else {
        return Arc::from("");
    };
    let kind = spec.type_.as_deref().unwrap_or("ClusterIP");
    let ports: Vec<String> = spec
        .ports
        .iter()
        .flatten()
        .map(|p| p.port.to_string())
        .take(3)
        .collect();
    if ports.is_empty() {
        Arc::from(kind)
    } else {
        Arc::from(format!("{kind} {}", ports.join(",")).as_str())
    }
}

pub fn stage_pvc(kind: KindId, pvc: &PersistentVolumeClaim) -> Option<Staged> {
    let mut staged = stage_meta(kind, Role::Attached, &pvc.metadata)?;
    staged.detail = Detail::Attached {
        detail: pvc_detail(pvc),
        selector: Vec::new(),
    };
    Some(staged)
}

pub fn pvc_detail(pvc: &PersistentVolumeClaim) -> Arc<str> {
    let bound = pvc
        .status
        .as_ref()
        .and_then(|s| s.capacity.as_ref())
        .and_then(|c| c.get("storage"))
        .map(|q| q.0.clone());
    let requested = pvc
        .spec
        .as_ref()
        .and_then(|s| s.resources.as_ref())
        .and_then(|r| r.requests.as_ref())
        .and_then(|r| r.get("storage"))
        .map(|q| q.0.clone());
    match bound.or(requested) {
        Some(size) => Arc::from(size.as_str()),
        None => Arc::from(""),
    }
}

#[cfg(test)]
#[path = "mapping_test.rs"]
mod tests;
