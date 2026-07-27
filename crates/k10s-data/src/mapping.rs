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

    for container in spec
        .containers
        .iter()
        .chain(spec.init_containers.iter().flatten())
    {
        for from in container.env_from.iter().flatten() {
            if let Some(cm) = &from.config_map_ref {
                push(kinds.config_map, &cm.name);
            }
            if let Some(secret) = &from.secret_ref {
                push(kinds.secret, &secret.name);
            }
        }
        for env in container.env.iter().flatten() {
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
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStateWaiting, PodCondition, PodStatus,
    };
    use std::collections::BTreeMap;

    fn meta(name: &str, uid: &str) -> ObjectMeta {
        ObjectMeta {
            name: Some(name.into()),
            uid: Some(uid.into()),
            namespace: Some("prod".into()),
            resource_version: Some("1234".into()),
            ..Default::default()
        }
    }

    fn labeled(name: &str, uid: &str, labels: &[(&str, &str)]) -> ObjectMeta {
        let mut m = meta(name, uid);
        m.labels = Some(
            labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        );
        m
    }

    fn waiting(reason: &str) -> ContainerStatus {
        ContainerStatus {
            name: "app".into(),
            ready: false,
            restart_count: 3,
            image: "img".into(),
            image_id: String::new(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some(reason.into()),
                    message: Some("some detail".into()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn terminated(reason: &str, exit_code: i32, restarts: i32) -> ContainerStatus {
        ContainerStatus {
            name: "app".into(),
            ready: false,
            restart_count: restarts,
            image: "img".into(),
            image_id: String::new(),
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    reason: Some(reason.into()),
                    exit_code,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn running(ready: bool) -> ContainerStatus {
        ContainerStatus {
            name: "app".into(),
            ready,
            restart_count: 0,
            image: "img".into(),
            image_id: String::new(),
            state: Some(ContainerState {
                running: Some(Default::default()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pod(phase: &str, statuses: Vec<ContainerStatus>) -> Pod {
        Pod {
            metadata: meta("api-1", "uid-pod-1"),
            spec: None,
            status: Some(PodStatus {
                phase: Some(phase.into()),
                container_statuses: Some(statuses),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn crashloopbackoff_is_its_own_reason_at_error_severity() {
        let r = pod_reason(&pod("Running", vec![waiting("CrashLoopBackOff")]));
        assert_eq!(&*r.display, "CrashLoopBackOff");
        assert_eq!(r.severity, Severity::Err);
        let failed = pod_reason(&pod("Failed", vec![]));
        assert_ne!(r.display, failed.display);
        assert_eq!(failed.severity, Severity::Err);
    }

    #[test]
    fn a_running_pod_with_a_broken_sidecar_is_not_running() {
        let r = pod_reason(&pod(
            "Running",
            vec![running(true), waiting("ImagePullBackOff")],
        ));
        assert_eq!(&*r.display, "ImagePullBackOff");
        assert_eq!(r.severity, Severity::Err);
    }

    #[test]
    fn an_init_container_failure_beats_the_pod_phase() {
        let mut p = pod("Pending", vec![running(false)]);
        p.status.as_mut().unwrap().init_container_statuses =
            Some(vec![waiting("CreateContainerConfigError")]);
        let r = pod_reason(&p);
        assert_eq!(&*r.display, "CreateContainerConfigError");
        assert_eq!(
            r.severity,
            Severity::Err,
            "a reason with no compiled-in id must still rate as an error"
        );
    }

    #[test]
    fn a_completed_init_container_is_not_a_problem() {
        let mut p = pod("Running", vec![running(true)]);
        p.status.as_mut().unwrap().init_container_statuses =
            Some(vec![terminated("Completed", 0, 0)]);
        let r = pod_reason(&p);
        assert_eq!(&*r.display, "Running");
        assert_eq!(r.severity, Severity::Ok);
    }

    #[test]
    fn a_finished_job_pod_is_ok_and_a_failed_one_is_not() {
        let done = pod_reason(&pod("Succeeded", vec![terminated("Completed", 0, 0)]));
        assert_eq!(&*done.display, "Succeeded");
        assert_eq!(done.severity, Severity::Ok);

        let oom = pod_reason(&pod("Failed", vec![terminated("OOMKilled", 137, 0)]));
        assert_eq!(&*oom.display, "OOMKilled");
        assert_eq!(oom.severity, Severity::Err);
    }

    #[test]
    fn readiness_not_phase_decides_a_running_pod() {
        let ready = pod_reason(&pod("Running", vec![running(true)]));
        assert_eq!(&*ready.display, "Running");
        let not_ready = pod_reason(&pod("Running", vec![running(false)]));
        assert_eq!(&*not_ready.display, "NotReady");
        assert_eq!(not_ready.severity, Severity::Warn);

        let mut p = pod("Running", vec![running(false)]);
        p.status.as_mut().unwrap().conditions = Some(vec![PodCondition {
            type_: "Ready".into(),
            status: "True".into(),
            ..Default::default()
        }]);
        assert_eq!(&*pod_reason(&p).display, "Running");
    }

    #[test]
    fn a_deleting_pod_is_terminating_whatever_its_containers_say() {
        let mut p = pod("Running", vec![running(true)]);
        p.metadata.deletion_timestamp = Some(
            serde_json::from_value(serde_json::json!("2024-01-01T00:00:00Z"))
                .expect("a timestamp the API server would send"),
        );
        let r = pod_reason(&p);
        assert_eq!(&*r.display, "Terminating");
        assert_eq!(r.severity, Severity::Warn);
    }

    #[test]
    fn an_evicted_pod_reports_the_pod_level_reason() {
        let mut p = pod("Failed", vec![running(true)]);
        p.status.as_mut().unwrap().reason = Some("Evicted".into());
        let r = pod_reason(&p);
        assert_eq!(&*r.display, "Evicted");
        assert_eq!(r.severity, Severity::Err);
    }

    #[test]
    fn a_pod_with_no_status_is_pending_and_one_with_no_phase_is_unknown() {
        let bare = Pod {
            metadata: meta("api-1", "uid-1"),
            spec: None,
            status: None,
        };
        assert_eq!(&*pod_reason(&bare).display, "Pending");

        let mut odd = pod("", vec![]);
        odd.status.as_mut().unwrap().phase = None;
        let r = pod_reason(&odd);
        assert_eq!(&*r.display, "Unknown");
        assert_eq!(r.severity, Severity::Unknown);
    }

    #[test]
    fn an_unrecognised_reason_is_unknown_and_never_ok() {
        assert_eq!(severity_of_reason("SomeFutureReason"), Severity::Unknown);
        assert_eq!(severity_of_reason(""), Severity::Unknown);
        let r = pod_reason(&pod("Running", vec![waiting("SomeFutureReason")]));
        assert_eq!(
            &*r.display, "NotReady",
            "unknown is not unhealthy, so the phase answer stands"
        );

        for reason in [
            "CrashLoopBackOff",
            "ImagePullBackOff",
            "ErrImagePull",
            "InvalidImageName",
            "CreateContainerConfigError",
            "RunContainerError",
            "OOMKilled",
            "DeadlineExceeded",
            "Evicted",
            "NodeAffinity",
        ] {
            assert_eq!(severity_of_reason(reason), Severity::Err, "{reason}");
        }
    }

    #[test]
    fn only_the_controlling_owner_reference_becomes_the_parent() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        let mut m = meta("api-1", "uid-1");
        m.owner_references = Some(vec![
            OwnerReference {
                api_version: "v1".into(),
                kind: "Service".into(),
                name: "api".into(),
                uid: "uid-svc".into(),
                controller: Some(false),
                block_owner_deletion: None,
            },
            OwnerReference {
                api_version: "apps/v1".into(),
                kind: "ReplicaSet".into(),
                name: "api-abc".into(),
                uid: "uid-rs".into(),
                controller: Some(true),
                block_owner_deletion: None,
            },
        ]);
        let c = controller_of(&m).expect("a controller reference");
        assert_eq!(&*c.uid, "uid-rs");
        assert_eq!(&*c.kind, "ReplicaSet");
        assert_eq!(&*c.name, "api-abc");
        assert_eq!(
            &*c.api_version, "apps/v1",
            "the reference has to carry a GVK, so an unwatched controller is still nameable"
        );

        m.owner_references.as_mut().unwrap()[1].controller = None;
        m.owner_references.as_mut().unwrap()[0].controller = None;
        assert_eq!(controller_of(&m), None);
    }

    #[test]
    fn resource_version_parses_or_reports_zero() {
        let mut m = meta("a", "u");
        assert_eq!(resource_version(&m), 1234);
        m.resource_version = Some("not-a-number".into());
        assert_eq!(resource_version(&m), 0);
        m.resource_version = None;
        assert_eq!(resource_version(&m), 0);
    }

    #[test]
    fn vendors_are_recognised_from_the_labels_charts_actually_set() {
        let cases: &[(&[(&str, &str)], ToolId)] = &[
            (
                &[("app.kubernetes.io/name", "postgresql")],
                ToolId::POSTGRES,
            ),
            (&[("app.kubernetes.io/name", "redis")], ToolId::REDIS),
            (&[("app", "grafana")], ToolId::GRAFANA),
            (&[("k8s-app", "kube-prometheus-stack")], ToolId::PROMETHEUS),
            (&[("app.kubernetes.io/part-of", "argo-cd")], ToolId::ARGO_CD),
            (
                &[("app.kubernetes.io/name", "my-redis-primary")],
                ToolId::REDIS,
            ),
            (&[("app.kubernetes.io/name", "checkout-api")], ToolId::NONE),
            (&[("unrelated", "redis")], ToolId::NONE),
            (&[], ToolId::NONE),
        ];
        for (labels, want) in cases {
            let m = labeled("x", "u", labels);
            assert_eq!(tool_of(&m), *want, "labels {labels:?}");
        }
    }

    #[test]
    fn a_vendor_never_lands_outside_the_compiled_in_table() {
        for value in ["totally-made-up", "", "none", "x", "13"] {
            let m = labeled("x", "u", &[("app", value)]);
            assert!(tool_of(&m).is_builtin(), "{value}");
        }
    }

    #[test]
    fn a_pod_spec_yields_every_attachment_it_names_once() {
        let spec: PodSpec = serde_json::from_value(serde_json::json!({
            "containers": [{
                "name": "app",
                "envFrom": [
                    {"configMapRef": {"name": "app-config"}},
                    {"secretRef": {"name": "app-secret"}}
                ],
                "env": [
                    {"name": "PW", "valueFrom": {"secretKeyRef": {"name": "db-secret", "key": "password"}}},
                    {"name": "HOST", "valueFrom": {"configMapKeyRef": {"name": "app-config", "key": "host"}}},
                    {"name": "PLAIN", "value": "literal"}
                ]
            }],
            "initContainers": [{
                "name": "migrate",
                "envFrom": [{"secretRef": {"name": "db-secret"}}]
            }],
            "volumes": [
                {"name": "data", "persistentVolumeClaim": {"claimName": "data-pvc"}},
                {"name": "cfg", "configMap": {"name": "app-config"}},
                {"name": "tls", "secret": {"secretName": "tls-cert"}},
                {"name": "proj", "projected": {"sources": [
                    {"configMap": {"name": "trust-bundle"}},
                    {"secret": {"name": "token-secret"}},
                    {"serviceAccountToken": {"path": "token"}}
                ]}},
                {"name": "scratch", "emptyDir": {}}
            ],
            "imagePullSecrets": [{"name": "registry-creds"}]
        }))
        .expect("a pod spec the API server could send");

        let refs = attachment_refs(&spec, &AttachKinds::default());
        let names: Vec<(KindId, &str)> = refs.iter().map(|r| (r.kind, &*r.name)).collect();
        for want in [
            (KindId::CONFIG_MAP, "app-config"),
            (KindId::CONFIG_MAP, "trust-bundle"),
            (KindId::SECRET, "app-secret"),
            (KindId::SECRET, "db-secret"),
            (KindId::SECRET, "tls-cert"),
            (KindId::SECRET, "token-secret"),
            (KindId::SECRET, "registry-creds"),
            (KindId::VOLUME, "data-pvc"),
        ] {
            assert!(names.contains(&want), "missing {want:?} in {names:?}");
        }
        assert_eq!(
            names
                .iter()
                .filter(|(k, n)| *k == KindId::CONFIG_MAP && *n == "app-config")
                .count(),
            1
        );
        assert_eq!(names.len(), 8, "no phantom references: {names:?}");
    }

    #[test]
    fn a_service_carries_its_selector_and_a_readable_detail() {
        let svc: Service = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "api", "uid": "uid-svc", "namespace": "prod", "resourceVersion": "7"},
            "spec": {
                "type": "ClusterIP",
                "selector": {"app": "api", "tier": "web"},
                "ports": [{"port": 80}, {"port": 443}]
            }
        }))
        .expect("a service");
        let staged = stage_service(KindId::SERVICE, &svc).expect("staged");
        assert_eq!(staged.role, Role::Attached);
        let Detail::Attached { detail, selector } = &staged.detail else {
            panic!("expected an attachment")
        };
        assert_eq!(&**detail, "ClusterIP 80,443");
        assert_eq!(
            selector,
            &vec![
                (Arc::from("app"), Arc::from("api")),
                (Arc::from("tier"), Arc::from("web"))
            ],
            "selectors must be sorted so matching is order-free"
        );
    }

    #[test]
    fn a_selectorless_service_is_staged_without_one() {
        let svc: Service = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "db", "uid": "uid-db", "namespace": "prod"},
            "spec": {"type": "ExternalName", "externalName": "db.example.com"}
        }))
        .expect("a service");
        let staged = stage_service(KindId::SERVICE, &svc).expect("staged");
        let Detail::Attached { detail, selector } = &staged.detail else {
            panic!("expected an attachment")
        };
        assert!(selector.is_empty());
        assert_eq!(&**detail, "ExternalName");
    }

    #[test]
    fn a_claim_reports_its_bound_size_then_its_request() {
        let bound: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "data", "uid": "uid-pvc", "namespace": "prod"},
            "spec": {"resources": {"requests": {"storage": "8Gi"}}},
            "status": {"phase": "Bound", "capacity": {"storage": "10Gi"}}
        }))
        .expect("a claim");
        assert_eq!(&*pvc_detail(&bound), "10Gi");

        let pending: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "data", "uid": "uid-pvc", "namespace": "prod"},
            "spec": {"resources": {"requests": {"storage": "8Gi"}}},
            "status": {"phase": "Pending"}
        }))
        .expect("a claim");
        assert_eq!(&*pvc_detail(&pending), "8Gi");

        let staged = stage_pvc(KindId::VOLUME, &bound).expect("staged");
        assert!(matches!(staged.detail, Detail::Attached { .. }));
    }

    #[test]
    fn a_secret_is_staged_from_metadata_and_carries_no_detail() {
        let staged = stage_meta(
            KindId::SECRET,
            Role::Attached,
            &labeled("db-password", "uid-secret", &[("app", "postgres")]),
        )
        .expect("staged");
        assert_eq!(staged.kind, KindId::SECRET);
        let Detail::Attached { detail, selector } = &staged.detail else {
            panic!("expected an attachment")
        };
        assert!(detail.is_empty(), "a secret detail must be empty");
        assert!(selector.is_empty());
        assert_eq!(&*staged.name, "db-password");
        assert_eq!(&*staged.namespace, "prod");
    }

    #[test]
    fn an_object_without_a_uid_is_not_staged() {
        let mut m = meta("a", "u");
        m.uid = None;
        assert!(stage_meta(KindId::POD, Role::Instance, &m).is_none());
        m.uid = Some("u".into());
        m.name = None;
        assert!(stage_meta(KindId::POD, Role::Instance, &m).is_none());
    }

    #[test]
    fn a_cluster_scoped_object_stages_with_an_empty_namespace() {
        let mut m = meta("prod", "uid-ns");
        m.namespace = None;
        let staged = stage_meta(KindId::NAMESPACE, Role::Scope, &m).expect("staged");
        assert!(staged.namespace.is_empty());
        assert_eq!(staged.detail, Detail::Scope);
    }

    #[test]
    fn staging_a_pod_keeps_labels_sorted_for_selector_matching() {
        let mut p = pod("Running", vec![running(true)]);
        p.metadata.labels = Some(BTreeMap::from([
            ("tier".to_string(), "web".to_string()),
            ("app".to_string(), "api".to_string()),
        ]));
        let staged = stage_pod(KindId::POD, &AttachKinds::default(), &p).expect("staged");
        let Detail::Instance { labels, .. } = &staged.detail else {
            panic!("expected an instance")
        };
        assert_eq!(labels[0].0.as_ref(), "app");
        assert_eq!(labels[1].0.as_ref(), "tier");
    }
}
