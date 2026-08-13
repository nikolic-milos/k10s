//! Scale, rollout restart/pause/resume, delete with grace, evict, cordon,
//! drain, and an ephemeral debug container.
//!
//! These are the clicks that are not apply. Each one is a single, named
//! Kubernetes request (or a short, bounded sequence of them) whose outcome
//! is a labelled state: it happened, the account may not, the server
//! refused the document, it failed, or it needs a confirmation sized to
//! the blast radius. Nothing here mutates until that confirmation is
//! passed in, and nothing here fires a request the caller has already
//! said this account cannot make.
//!
//! Capability is not discovered here. The caller passes whether patch,
//! delete, and create are allowed; a denial is returned with a reason
//! and the wire is left untouched. That is the same split apply uses
//! between "the server serves no patch verb" and "this account may not
//! patch": the first is a property of the kind, the second is a property
//! of the session.
//!
//! Rollout undo is not implemented. `kubectl rollout undo` walks ReplicaSet
//! controller history, and Helm rollback walks Helm's own Secrets. Pretending
//! either from a merge-patch would be a fake of both. The outcome says so.
//!
//! Drain cordons, then evicts at most [`MAX_DRAIN_PODS`] pods on that node,
//! skipping DaemonSet pods unless asked to force, and labels a PDB 429
//! rather than retrying it forever. A truncated flag means there was more
//! to evict than this press would touch.

use kube::Client;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Request};
use kube::core::Status;
use serde::Deserialize;

use crate::discover::KindTarget;
use crate::read::collection_path;

pub const MAX_DRAIN_PODS: usize = 16;
pub const DEBUG_CONTAINER: &str = "k10s-debug";
pub const RESTART_ANNOTATION: &str = "kubectl.kubernetes.io/restartedAt";

const DRAIN_LIST_LIMIT: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caps {
    pub patch: bool,
    pub delete: bool,
    pub create: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub summary: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blast {
    Replicas {
        from: i32,
        to: i32,
    },
    Object {
        kind: String,
        namespace: Option<String>,
        name: String,
    },
    Namespace {
        name: String,
    },
    Node {
        name: String,
    },
    Drain {
        node: String,
        pods: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Day2Outcome {
    Applied(Applied),
    Denied { what: &'static str, why: String },
    Rejected { message: String },
    Failed { why: String },
    NeedsConfirm { blast: Blast, summary: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleRequest {
    pub namespace: Option<String>,
    pub name: String,
    pub current: i32,
    pub replicas: i32,
    pub confirm: bool,
    pub caps: Caps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolloutAction {
    Restart { restarted_at: String },
    Pause,
    Resume,
    Undo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutRequest {
    pub namespace: Option<String>,
    pub name: String,
    pub action: RolloutAction,
    pub confirm: bool,
    pub caps: Caps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRequest {
    pub namespace: Option<String>,
    pub name: String,
    pub grace_period_seconds: Option<u32>,
    pub confirm: bool,
    pub caps: Caps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictRequest {
    pub namespace: String,
    pub name: String,
    pub grace_period_seconds: Option<u32>,
    pub confirm: bool,
    pub caps: Caps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CordonRequest {
    pub name: String,
    pub unschedulable: bool,
    pub confirm: bool,
    pub caps: Caps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainRequest {
    pub name: String,
    pub force: bool,
    pub confirm: bool,
    pub caps: Caps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugRequest {
    pub namespace: String,
    pub name: String,
    pub image: String,
    pub confirm: bool,
    pub caps: Caps,
}

pub async fn scale(client: &Client, target: &KindTarget, request: &ScaleRequest) -> Day2Outcome {
    if let Some(denied) = require(request.caps.patch, "scale", "this account cannot patch") {
        return denied;
    }
    if !target.patchable {
        return unpatchable(target, "scaled");
    }
    if !scalable(target) {
        return Day2Outcome::Failed {
            why: format!(
                "scale is the apps/v1 scale subresource of a Deployment, StatefulSet, or ReplicaSet, not {}",
                target.kind()
            ),
        };
    }
    if request.replicas < 0 {
        return Day2Outcome::Rejected {
            message: "replicas must be zero or more".to_string(),
        };
    }
    let summary = format!(
        "scale {} from {} to {} replicas",
        named(target, request.namespace.as_deref(), &request.name),
        request.current,
        request.replicas,
    );
    let blast = Blast::Replicas {
        from: request.current,
        to: request.replicas,
    };
    if let Some(wait) = unless_confirmed(request.confirm, blast, summary.clone()) {
        return wait;
    }
    let path = match collection(target, request.namespace.as_deref()) {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };
    let body = serde_json::json!({ "spec": { "replicas": request.replicas } });
    let built = match merge_patch(path, &request.name, Some("scale"), body) {
        Ok(built) => built,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) = send(client, built, "scale").await {
        return outcome;
    }
    applied(summary)
}

pub async fn rollout(
    client: &Client,
    target: &KindTarget,
    request: &RolloutRequest,
) -> Day2Outcome {
    if matches!(request.action, RolloutAction::Undo) {
        return Day2Outcome::Failed {
            why: "rollout undo is ReplicaSet controller history; k10s does not fake kubectl \
                  rollout undo or Helm rollback"
                .to_string(),
        };
    }
    if let Some(denied) = require(request.caps.patch, "rollout", "this account cannot patch") {
        return denied;
    }
    if !target.patchable {
        return unpatchable(target, "rolled");
    }
    let (what, body, summary) = match &request.action {
        RolloutAction::Restart { restarted_at } => {
            if restarted_at.is_empty() {
                return Day2Outcome::Failed {
                    why: "a restart needs an RFC3339 kubectl.kubernetes.io/restartedAt timestamp"
                        .to_string(),
                };
            }
            if !restartable(target) {
                return Day2Outcome::Failed {
                    why: format!(
                        "rollout restart is a pod-template annotation on a Deployment, StatefulSet, \
                         or DaemonSet, not {}",
                        target.kind()
                    ),
                };
            }
            (
                "rollout",
                serde_json::json!({
                    "spec": {
                        "template": {
                            "metadata": {
                                "annotations": { RESTART_ANNOTATION: restarted_at }
                            }
                        }
                    }
                }),
                format!(
                    "restart {}, every replica will be replaced",
                    named(target, request.namespace.as_deref(), &request.name)
                ),
            )
        }
        RolloutAction::Pause => {
            if !pausable(target) {
                return Day2Outcome::Failed {
                    why: format!(
                        "rollout pause is spec.paused on a Deployment, not {}",
                        target.kind()
                    ),
                };
            }
            (
                "rollout",
                serde_json::json!({ "spec": { "paused": true } }),
                format!(
                    "pause {}",
                    named(target, request.namespace.as_deref(), &request.name)
                ),
            )
        }
        RolloutAction::Resume => {
            if !pausable(target) {
                return Day2Outcome::Failed {
                    why: format!(
                        "rollout resume is spec.paused on a Deployment, not {}",
                        target.kind()
                    ),
                };
            }
            (
                "rollout",
                serde_json::json!({ "spec": { "paused": false } }),
                format!(
                    "resume {}",
                    named(target, request.namespace.as_deref(), &request.name)
                ),
            )
        }
        RolloutAction::Undo => unreachable!("undo returns before this match"),
    };
    let blast = Blast::Object {
        kind: target.kind().to_string(),
        namespace: request.namespace.clone(),
        name: request.name.clone(),
    };
    if let Some(wait) = unless_confirmed(request.confirm, blast, summary.clone()) {
        return wait;
    }
    let path = match collection(target, request.namespace.as_deref()) {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };
    let built = match merge_patch(path, &request.name, None, body) {
        Ok(built) => built,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) = send(client, built, what).await {
        return outcome;
    }
    applied(summary)
}

pub async fn delete(client: &Client, target: &KindTarget, request: &DeleteRequest) -> Day2Outcome {
    if let Some(denied) = require(request.caps.delete, "delete", "this account cannot delete") {
        return denied;
    }
    let blast = delete_blast(target, request.namespace.as_deref(), &request.name);
    let mut summary = format!(
        "delete {}",
        named(target, request.namespace.as_deref(), &request.name)
    );
    match &blast {
        Blast::Namespace { .. } => {
            summary.push_str("; every namespaced object in it is in the blast");
        }
        Blast::Node { .. } => {
            summary.push_str("; this does not drain the node");
        }
        _ => {}
    }
    if let Some(secs) = request.grace_period_seconds {
        summary.push_str(&format!("; grace {secs}s"));
    }
    if let Some(wait) = unless_confirmed(request.confirm, blast, summary.clone()) {
        return wait;
    }
    let path = match collection(target, request.namespace.as_deref()) {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };
    let params = match request.grace_period_seconds {
        Some(secs) => DeleteParams::default().grace_period(secs),
        None => DeleteParams::default(),
    };
    let built = match Request::new(path).delete(&request.name, &params) {
        Ok(built) => built,
        Err(error) => {
            return Day2Outcome::Failed {
                why: error.to_string(),
            };
        }
    };
    if let Err(outcome) = send(client, built, "delete").await {
        return outcome;
    }
    applied(summary)
}

pub async fn evict(client: &Client, target: &KindTarget, request: &EvictRequest) -> Day2Outcome {
    if let Some(denied) = require(
        request.caps.create,
        "evict",
        "this account cannot create evictions",
    ) {
        return denied;
    }
    if !is_pod(target) {
        return Day2Outcome::Failed {
            why: format!("evict is a Pod subresource, not {}", target.kind()),
        };
    }
    let summary = format!("evict pod {}/{}", request.namespace, request.name);
    let blast = Blast::Object {
        kind: "Pod".to_string(),
        namespace: Some(request.namespace.clone()),
        name: request.name.clone(),
    };
    if let Some(wait) = unless_confirmed(request.confirm, blast, summary.clone()) {
        return wait;
    }
    match post_eviction(
        client,
        target,
        &request.namespace,
        &request.name,
        request.grace_period_seconds,
    )
    .await
    {
        Ok(()) => applied(summary),
        Err(outcome) => outcome,
    }
}

pub async fn cordon(client: &Client, target: &KindTarget, request: &CordonRequest) -> Day2Outcome {
    let what = if request.unschedulable {
        "cordon"
    } else {
        "uncordon"
    };
    if let Some(denied) = require(request.caps.patch, what, "this account cannot patch") {
        return denied;
    }
    if !target.patchable {
        return unpatchable(target, what);
    }
    if !is_node(target) {
        return Day2Outcome::Failed {
            why: format!(
                "{what} is a Node spec.unschedulable patch, not {}",
                target.kind()
            ),
        };
    }
    let summary = format!("{what} node {}", request.name);
    let blast = Blast::Node {
        name: request.name.clone(),
    };
    if let Some(wait) = unless_confirmed(request.confirm, blast, summary.clone()) {
        return wait;
    }
    match patch_unschedulable(client, target, &request.name, request.unschedulable).await {
        Ok(()) => applied(summary),
        Err(outcome) => outcome,
    }
}

pub async fn drain(client: &Client, target: &KindTarget, request: &DrainRequest) -> Day2Outcome {
    if !request.caps.patch {
        return Day2Outcome::Denied {
            what: "drain",
            why: "this account cannot patch, so the node cannot be cordoned".to_string(),
        };
    }
    if !request.caps.create {
        return Day2Outcome::Denied {
            what: "drain",
            why: "this account cannot create evictions".to_string(),
        };
    }
    if !target.patchable {
        return unpatchable(target, "drained");
    }
    if !is_node(target) {
        return Day2Outcome::Failed {
            why: format!("drain is a Node operation, not {}", target.kind()),
        };
    }
    let plan = match plan_drain(client, &request.name, request.force).await {
        Ok(plan) => plan,
        Err(outcome) => return outcome,
    };
    let mut summary = format!(
        "drain node {}: cordon, then evict {} {}",
        request.name,
        plan.evict.len(),
        if plan.evict.len() == 1 { "pod" } else { "pods" },
    );
    if plan.skipped_ds > 0 {
        summary.push_str(&format!(
            "; {} DaemonSet {} skipped",
            plan.skipped_ds,
            if plan.skipped_ds == 1 { "pod" } else { "pods" },
        ));
    }
    if plan.truncated {
        summary.push_str(&format!(
            "; more than {MAX_DRAIN_PODS} evictable pods, this press stops at {MAX_DRAIN_PODS}"
        ));
    }
    let blast = Blast::Drain {
        node: request.name.clone(),
        pods: plan.evict.len(),
    };
    if let Some(wait) = unless_confirmed(request.confirm, blast, summary.clone()) {
        return wait;
    }
    if let Err(outcome) = patch_unschedulable(client, target, &request.name, true).await {
        return outcome;
    }
    let mut evicted = 0usize;
    let mut pdb = 0usize;
    let mut failed = 0usize;
    for pod in &plan.evict {
        match post_eviction(client, &pod_target(), &pod.namespace, &pod.name, None).await {
            Ok(()) => evicted += 1,
            Err(Day2Outcome::Failed { why }) if why.contains("PodDisruptionBudget") => pdb += 1,
            Err(_) => failed += 1,
        }
    }
    let mut done = format!(
        "cordoned {}; evicted {evicted} {}",
        request.name,
        if evicted == 1 { "pod" } else { "pods" },
    );
    if pdb > 0 {
        done.push_str(&format!("; {pdb} blocked by a PodDisruptionBudget"));
    }
    if failed > 0 {
        done.push_str(&format!("; {failed} eviction(s) failed"));
    }
    if plan.skipped_ds > 0 {
        done.push_str(&format!(
            "; {} DaemonSet {} skipped",
            plan.skipped_ds,
            if plan.skipped_ds == 1 { "pod" } else { "pods" },
        ));
    }
    Day2Outcome::Applied(Applied {
        summary: done,
        truncated: plan.truncated,
    })
}

pub async fn debug(client: &Client, target: &KindTarget, request: &DebugRequest) -> Day2Outcome {
    if let Some(denied) = require(
        request.caps.create,
        "debug",
        "this account cannot create ephemeral containers",
    ) {
        return denied;
    }
    if !is_pod(target) {
        return Day2Outcome::Failed {
            why: format!(
                "an ephemeral debug container is a Pod subresource, not {}",
                target.kind()
            ),
        };
    }
    if request.image.is_empty() {
        return Day2Outcome::Rejected {
            message: "a debug container needs an image".to_string(),
        };
    }
    let summary = format!(
        "add ephemeral container {DEBUG_CONTAINER} from {} on pod {}/{}",
        request.image, request.namespace, request.name
    );
    let blast = Blast::Object {
        kind: "Pod".to_string(),
        namespace: Some(request.namespace.clone()),
        name: request.name.clone(),
    };
    if let Some(wait) = unless_confirmed(request.confirm, blast, summary.clone()) {
        return wait;
    }
    let path = match collection(target, Some(&request.namespace)) {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };
    let body = serde_json::json!({
        "apiVersion": "v1",
        "kind": "EphemeralContainers",
        "metadata": {
            "name": request.name,
            "namespace": request.namespace,
        },
        "ephemeralContainers": [{
            "name": DEBUG_CONTAINER,
            "image": request.image,
            "stdin": true,
            "tty": true,
        }],
    });
    let bytes = match serde_json::to_vec(&body) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Day2Outcome::Failed {
                why: error.to_string(),
            };
        }
    };
    let built = match Request::new(path).create_subresource(
        "ephemeralcontainers",
        &request.name,
        &PostParams::default(),
        bytes,
    ) {
        Ok(built) => built,
        Err(error) => {
            return Day2Outcome::Failed {
                why: error.to_string(),
            };
        }
    };
    if let Err(outcome) = send(client, built, "debug").await {
        return outcome;
    }
    applied(summary)
}

fn scalable(target: &KindTarget) -> bool {
    target.group() == "apps"
        && target.resource.version == "v1"
        && matches!(target.kind(), "Deployment" | "StatefulSet" | "ReplicaSet")
}

fn restartable(target: &KindTarget) -> bool {
    target.group() == "apps"
        && target.resource.version == "v1"
        && matches!(target.kind(), "Deployment" | "StatefulSet" | "DaemonSet")
}

fn pausable(target: &KindTarget) -> bool {
    target.group() == "apps" && target.resource.version == "v1" && target.kind() == "Deployment"
}

fn is_pod(target: &KindTarget) -> bool {
    target.group().is_empty() && target.kind() == "Pod"
}

fn is_node(target: &KindTarget) -> bool {
    target.group().is_empty() && target.kind() == "Node"
}

fn delete_blast(target: &KindTarget, namespace: Option<&str>, name: &str) -> Blast {
    if is_node(target) {
        return Blast::Node {
            name: name.to_string(),
        };
    }
    if target.kind() == "Namespace" && target.group().is_empty() {
        return Blast::Namespace {
            name: name.to_string(),
        };
    }
    Blast::Object {
        kind: target.kind().to_string(),
        namespace: namespace.map(str::to_string),
        name: name.to_string(),
    }
}

fn named(target: &KindTarget, namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) if !namespace.is_empty() => {
            format!("{} {}/{}", target.kind(), namespace, name)
        }
        _ => format!("{} {name}", target.kind()),
    }
}

fn collection(target: &KindTarget, namespace: Option<&str>) -> Result<String, Day2Outcome> {
    if target.namespaced && namespace.map(str::is_empty).unwrap_or(true) {
        return Err(Day2Outcome::Failed {
            why: format!(
                "{} is namespaced, so a namespace is required",
                target.kind()
            ),
        });
    }
    Ok(collection_path(target, namespace))
}

fn require(allowed: bool, what: &'static str, why: &'static str) -> Option<Day2Outcome> {
    if allowed {
        return None;
    }
    Some(Day2Outcome::Denied {
        what,
        why: why.to_string(),
    })
}

fn unless_confirmed(confirm: bool, blast: Blast, summary: String) -> Option<Day2Outcome> {
    if confirm {
        return None;
    }
    Some(Day2Outcome::NeedsConfirm { blast, summary })
}

fn unpatchable(target: &KindTarget, verb: &str) -> Day2Outcome {
    Day2Outcome::Failed {
        why: format!(
            "the server serves {} without a patch verb, so it cannot be {verb}",
            target.kind()
        ),
    }
}

fn applied(summary: String) -> Day2Outcome {
    Day2Outcome::Applied(Applied {
        summary,
        truncated: false,
    })
}

fn merge_patch(
    path: String,
    name: &str,
    subresource: Option<&str>,
    body: serde_json::Value,
) -> Result<http::Request<Vec<u8>>, Day2Outcome> {
    let patch = Patch::Merge(body);
    let params = PatchParams::default();
    let built = match subresource {
        Some(sub) => Request::new(path).patch_subresource(sub, name, &params, &patch),
        None => Request::new(path).patch(name, &params, &patch),
    };
    built.map_err(|error| Day2Outcome::Failed {
        why: error.to_string(),
    })
}

async fn send(
    client: &Client,
    request: http::Request<Vec<u8>>,
    what: &'static str,
) -> Result<(), Day2Outcome> {
    match client.request::<serde_json::Value>(request).await {
        Ok(_) => Ok(()),
        Err(error) => Err(classify(&error, what)),
    }
}

async fn patch_unschedulable(
    client: &Client,
    target: &KindTarget,
    name: &str,
    unschedulable: bool,
) -> Result<(), Day2Outcome> {
    let path = collection_path(target, None);
    let body = serde_json::json!({ "spec": { "unschedulable": unschedulable } });
    let built = merge_patch(path, name, None, body)?;
    send(
        client,
        built,
        if unschedulable { "cordon" } else { "uncordon" },
    )
    .await
}

async fn post_eviction(
    client: &Client,
    target: &KindTarget,
    namespace: &str,
    name: &str,
    grace_period_seconds: Option<u32>,
) -> Result<(), Day2Outcome> {
    let path = collection(target, Some(namespace))?;
    let body = eviction_body(namespace, name, grace_period_seconds);
    let bytes = serde_json::to_vec(&body).map_err(|error| Day2Outcome::Failed {
        why: error.to_string(),
    })?;
    let built = Request::new(path)
        .create_subresource("eviction", name, &PostParams::default(), bytes)
        .map_err(|error| Day2Outcome::Failed {
            why: error.to_string(),
        })?;
    send(client, built, "evict").await
}

fn eviction_body(namespace: &str, name: &str, grace: Option<u32>) -> serde_json::Value {
    match grace {
        Some(secs) => serde_json::json!({
            "apiVersion": "policy/v1",
            "kind": "Eviction",
            "metadata": { "name": name, "namespace": namespace },
            "deleteOptions": { "gracePeriodSeconds": secs },
        }),
        None => serde_json::json!({
            "apiVersion": "policy/v1",
            "kind": "Eviction",
            "metadata": { "name": name, "namespace": namespace },
        }),
    }
}

struct DrainPlan {
    evict: Vec<DrainPod>,
    skipped_ds: usize,
    truncated: bool,
}

struct DrainPod {
    namespace: String,
    name: String,
}

#[derive(Deserialize, Default)]
struct WireList {
    #[serde(default)]
    metadata: WireListMeta,
    #[serde(default)]
    items: Vec<WirePod>,
}

#[derive(Deserialize, Default)]
struct WireListMeta {
    #[serde(default, rename = "continue")]
    cont: String,
}

#[derive(Deserialize, Default)]
struct WirePod {
    #[serde(default)]
    metadata: WirePodMeta,
    #[serde(default)]
    status: WirePodStatus,
}

#[derive(Deserialize, Default)]
struct WirePodMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default, rename = "ownerReferences")]
    owner_references: Vec<WireOwner>,
}

#[derive(Deserialize, Default)]
struct WireOwner {
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize, Default)]
struct WirePodStatus {
    #[serde(default)]
    phase: String,
}

async fn plan_drain(client: &Client, node: &str, force: bool) -> Result<DrainPlan, Day2Outcome> {
    let params = ListParams::default()
        .fields(&format!("spec.nodeName={node}"))
        .limit(DRAIN_LIST_LIMIT);
    let built = Request::new("/api/v1/pods")
        .list(&params)
        .map_err(|error| Day2Outcome::Failed {
            why: error.to_string(),
        })?;
    let page = match client.request::<WireList>(built).await {
        Ok(page) => page,
        Err(error) => return Err(classify(&error, "drain")),
    };
    let more_pages = !page.metadata.cont.is_empty();
    let mut evict = Vec::new();
    let mut skipped_ds = 0usize;
    let mut extra = false;
    for pod in page.items {
        if pod.metadata.name.is_empty() {
            continue;
        }
        if matches!(pod.status.phase.as_str(), "Succeeded" | "Failed") {
            continue;
        }
        if !force
            && pod
                .metadata
                .owner_references
                .iter()
                .any(|o| o.kind == "DaemonSet")
        {
            skipped_ds += 1;
            continue;
        }
        if evict.len() == MAX_DRAIN_PODS {
            extra = true;
            continue;
        }
        evict.push(DrainPod {
            namespace: pod.metadata.namespace,
            name: pod.metadata.name,
        });
    }
    Ok(DrainPlan {
        truncated: extra || more_pages,
        evict,
        skipped_ds,
    })
}

fn pod_target() -> KindTarget {
    KindTarget {
        id: k10s_core::KindId::POD,
        resource: kube::discovery::ApiResource {
            group: String::new(),
            version: "v1".to_string(),
            api_version: "v1".to_string(),
            kind: "Pod".to_string(),
            plural: "pods".to_string(),
        },
        role: k10s_core::Role::Instance,
        namespaced: true,
        listable: true,
        watchable: true,
        patchable: true,
        status_subresource: true,
    }
}

fn classify(error: &kube::Error, what: &'static str) -> Day2Outcome {
    if let kube::Error::Api(status) = error {
        return match status.code {
            401 | 403 => Day2Outcome::Denied {
                what,
                why: message_of(status, what),
            },
            400 | 422 => Day2Outcome::Rejected {
                message: message_of(status, what),
            },
            404 if what == "debug" => Day2Outcome::Failed {
                why: "the ephemeralcontainers subresource is not served; this cluster is too old \
                      for debug containers"
                    .to_string(),
            },
            429 if what == "evict" || what == "drain" => Day2Outcome::Failed {
                why: format!(
                    "a PodDisruptionBudget refused this eviction: {}",
                    message_of(status, what)
                ),
            },
            _ => Day2Outcome::Failed {
                why: message_of(status, what),
            },
        };
    }
    let why = crate::connect::describe(error as &(dyn std::error::Error + 'static));
    Day2Outcome::Failed {
        why: format!(
            "{why}; the {what} may or may not have reached the cluster, so read the object again \
             before retrying"
        ),
    }
}

fn message_of(status: &Status, what: &'static str) -> String {
    if status.message.is_empty() {
        return format!(
            "the API server refused the {what} with status {}",
            status.code
        );
    }
    status.message.clone()
}

#[cfg(test)]
#[path = "day2_test.rs"]
mod tests;
