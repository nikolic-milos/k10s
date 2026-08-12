//! The scripted API server, and the suites driven against it.
//!
//! `Script` is a `tower::Service` answering routed requests, so the whole data
//! plane can be driven without a cluster: no containers, no kubelet, nothing
//! installed. It stays at the crate root of this binary rather than moving into
//! a module of its own, because a private root item is visible to every
//! descendant -- so each suite reaches the harness with `use crate::*;` and not
//! one fixture has to be made `pub` to be shared.
//!
//! Cargo builds only the top-level files in `tests/` as binaries, so the suites
//! below are modules of this one binary and share a single compiled harness.

#[path = "scripted_apiserver/conformance.rs"]
mod conformance;
#[path = "scripted_apiserver/describe.rs"]
mod describe;
#[path = "scripted_apiserver/forwards.rs"]
mod forwards;
#[path = "scripted_apiserver/helm.rs"]
mod helm;
#[path = "scripted_apiserver/inspector.rs"]
mod inspector;
#[path = "scripted_apiserver/logs.rs"]
mod logs;
#[path = "scripted_apiserver/manifest.rs"]
mod manifest;
#[path = "scripted_apiserver/metrics.rs"]
mod metrics;
#[path = "scripted_apiserver/nodes.rs"]
mod nodes;
#[path = "scripted_apiserver/rbac.rs"]
mod rbac;
#[path = "scripted_apiserver/schema.rs"]
mod schema;
#[path = "scripted_apiserver/tables.rs"]
mod tables;

use k10s_core::{Capability, IngestEvent, KindId, Op, Payload, ResourceEvent, Severity};
use k10s_data::{IngestMetrics, Options, Sync, sync_from};
use kube::client::Body;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tower::Service;
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    accept: String,
    // The write half of the wire. A dry-run apply is only correct if its
    // content type and its bytes are, and neither is visible from the path.
    content_type: String,
    body: String,
}
struct Route {
    method: &'static str,
    matches: String,
    accept_contains: Option<&'static str>,
    status: u16,
    body: String,
    hang: bool,
    used: bool,
}
#[derive(Default)]
struct State {
    routes: Vec<Route>,
    seen: Vec<Seen>,
}
#[derive(Clone, Default)]
struct Script {
    state: Arc<Mutex<State>>,
}
impl Script {
    fn route(
        &self,
        method: &'static str,
        matches: &str,
        status: u16,
        body: impl Into<String>,
    ) -> &Self {
        self.state.lock().expect("script lock").routes.push(Route {
            method,
            matches: matches.to_string(),
            accept_contains: None,
            status,
            body: body.into(),
            hang: false,
            used: false,
        });
        self
    }

    fn route_accepting(
        &self,
        method: &'static str,
        matches: &str,
        accept_contains: &'static str,
        status: u16,
        body: impl Into<String>,
    ) -> &Self {
        self.state.lock().expect("script lock").routes.push(Route {
            method,
            matches: matches.to_string(),
            accept_contains: Some(accept_contains),
            status,
            body: body.into(),
            hang: false,
            used: false,
        });
        self
    }

    // A route that never answers: the connection stays open, like a real
    // API server holding a follow. What a caller can prove against it is
    // that cancellation works while a request is in flight.
    fn route_hanging(&self, method: &'static str, matches: &str) -> &Self {
        self.state.lock().expect("script lock").routes.push(Route {
            method,
            matches: matches.to_string(),
            accept_contains: None,
            status: 0,
            body: String::new(),
            hang: true,
            used: false,
        });
        self
    }

    fn seen(&self) -> Vec<Seen> {
        self.state.lock().expect("script lock").seen.clone()
    }

    fn requests_for(&self, needle: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.path.contains(needle))
            .collect()
    }

    fn client(&self) -> kube::Client {
        kube::Client::new(self.clone(), "default")
    }
}
impl Service<http::Request<Body>> for Script {
    type Response = http::Response<Body>;
    type Error = tower::BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let method = req.method().to_string();
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default();
        let accept = header(&req, http::header::ACCEPT);
        let content_type = header(&req, http::header::CONTENT_TYPE);

        // Recording and route selection stay synchronous, in `call`, because
        // routes are single-shot and the probe issues its reviews concurrently:
        // a suite that scripts one 503 ahead of thirty-two successes is asserting
        // *which* request gets the 503, and moving the choice behind the body's
        // first await made that the executor's decision instead of the
        // registration order's. Only the body -- which is a stream and cannot be
        // read here -- is filled in afterwards, by index.
        let (at, answer) = {
            let mut state = self.state.lock().expect("script lock");
            let at = state.seen.len();
            state.seen.push(Seen {
                method: method.clone(),
                path: path.clone(),
                accept: accept.clone(),
                content_type,
                body: String::new(),
            });

            let routable = path.replacen("?&", "?", 1);
            let hit = state.routes.iter_mut().find(|r| {
                !r.used
                    && r.method == method
                    && routable.starts_with(&r.matches)
                    && r.accept_contains.is_none_or(|want| accept.contains(want))
            });
            let answer = match hit {
                Some(route) if route.hang => {
                    route.used = true;
                    None
                }
                Some(route) => {
                    route.used = true;
                    Some((route.status, route.body.clone()))
                }
                None if path.contains("watch=true") => None,
                None => Some((
                    404,
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"unscripted"}"#
                        .to_string(),
                )),
            };
            (at, answer)
        };

        let shared = self.state.clone();
        let body = req.into_body();
        Box::pin(async move {
            let read = collect(body).await;
            if let Some(seen) = shared.lock().expect("script lock").seen.get_mut(at) {
                seen.body = read;
            }
            let Some((status, response)) = answer else {
                return std::future::pending().await;
            };
            Ok(http::Response::builder()
                .status(status)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(response.into_bytes()))
                .expect("a response"))
        })
    }
}
const SERVICE_JSON: &str = r#"{"metadata":{"name":"api","uid":"uid-svc","namespace":"prod","resourceVersion":"900"},
    "spec":{"type":"ClusterIP","selector":{"app":"api"},"ports":[{"port":80}]}}"#;
const CLAIM_JSON: &str = r#"{"metadata":{"name":"api-data","uid":"uid-pvc","namespace":"prod","resourceVersion":"900"},
    "spec":{"resources":{"requests":{"storage":"8Gi"}}},
    "status":{"phase":"Bound","capacity":{"storage":"8Gi"}}}"#;
const METADATA_LIST_ACCEPT: &str = "as=PartialObjectMetadataList";
fn header(request: &http::Request<Body>, name: http::header::HeaderName) -> String {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}
async fn collect(body: Body) -> String {
    use http_body_util::BodyExt;
    match body.collect().await {
        Ok(collected) => String::from_utf8_lossy(&collected.to_bytes()).to_string(),
        Err(_) => String::new(),
    }
}
fn api_resource(kind: &str, plural: &str, namespaced: bool) -> String {
    verbed(
        kind,
        plural,
        namespaced,
        "\"get\",\"list\",\"watch\",\"patch\"",
    )
}
// A kind the server serves without a patch verb: an apply has to refuse before
// the wire, and for a reason that is not permissions.
fn api_resource_without_patch(kind: &str, plural: &str, namespaced: bool) -> String {
    verbed(kind, plural, namespaced, "\"get\",\"list\",\"watch\"")
}
fn verbed(kind: &str, plural: &str, namespaced: bool, verbs: &str) -> String {
    format!(
        r#"{{"name":"{plural}","singularName":"","namespaced":{namespaced},"kind":"{kind}","verbs":[{verbs}]}}"#
    )
}
// Discovery reports a status subresource as a resource whose name carries a
// slash; kube folds it into the parent's capabilities.
fn api_status_subresource(kind: &str, plural: &str, namespaced: bool) -> String {
    format!(
        r#"{{"name":"{plural}/status","singularName":"","namespaced":{namespaced},"kind":"{kind}","verbs":["get","patch"]}}"#
    )
}
fn script_discovery(script: &Script) {
    script.route_accepting("GET", "/apis", "APIGroupDiscoveryList", 406, "{}");
    script.route_accepting("GET", "/api", "APIGroupDiscoveryList", 406, "{}");

    script.route(
        "GET",
        "/apis",
        200,
        r#"{"kind":"APIGroupList","apiVersion":"v1","groups":[
            {"name":"apps","versions":[{"groupVersion":"apps/v1","version":"v1"}],
             "preferredVersion":{"groupVersion":"apps/v1","version":"v1"}},
            {"name":"batch","versions":[{"groupVersion":"batch/v1","version":"v1"}],
             "preferredVersion":{"groupVersion":"batch/v1","version":"v1"}}
        ]}"#,
    );
    script.route(
        "GET",
        "/apis/apps/v1",
        200,
        format!(
            r#"{{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"apps/v1","resources":[{},{},{}]}}"#,
            api_resource("Deployment", "deployments", true),
            api_status_subresource("Deployment", "deployments", true),
            api_resource("ReplicaSet", "replicasets", true),
        ),
    );
    script.route(
        "GET",
        "/apis/batch/v1",
        200,
        format!(
            r#"{{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"batch/v1","resources":[{},{}]}}"#,
            api_resource("Job", "jobs", true),
            api_resource("CronJob", "cronjobs", true),
        ),
    );
    script.route(
        "GET",
        "/api",
        200,
        r#"{"kind":"APIVersions","versions":["v1"]}"#,
    );
    script.route(
        "GET",
        "/api/v1",
        200,
        format!(
            r#"{{"kind":"APIResourceList","groupVersion":"v1","resources":[{},{},{},{},{},{},{}]}}"#,
            api_resource("Namespace", "namespaces", false),
            api_resource("Pod", "pods", true),
            api_status_subresource("Pod", "pods", true),
            api_resource("Service", "services", true),
            api_resource("ConfigMap", "configmaps", true),
            // A ConfigMap has no status subresource and a Secret is the kind
            // this server serves without a patch verb, which is what lets one
            // discovery fixture prove both refusals.
            api_resource_without_patch("Secret", "secrets", true),
            api_resource("PersistentVolumeClaim", "persistentvolumeclaims", true),
        ),
    );
    script.route(
        "GET",
        "/version",
        200,
        r#"{"major":"1","minor":"32","gitVersion":"v1.32.3","gitCommit":"x","gitTreeState":"clean","buildDate":"","goVersion":"go1.23","compiler":"gc","platform":"linux/amd64"}"#,
    );
}
fn script_access_reviews(script: &Script, allowed: bool, times: usize) {
    let body = format!(
        r#"{{"kind":"SelfSubjectAccessReview","apiVersion":"authorization.k8s.io/v1","spec":{{}},"status":{{"allowed":{allowed}}}}}"#
    );
    for _ in 0..times {
        script.route(
            "POST",
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            201,
            body.clone(),
        );
    }
}
fn script_rules_review(script: &Script) {
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],
                      "resourceRules":[{"apiGroups":["*"],"resources":["*"],"verbs":["get","list","watch"]}]}}"#,
    );
}
fn list(items: &[String], kind: &str) -> String {
    format!(
        r#"{{"kind":"{kind}List","apiVersion":"v1","metadata":{{"resourceVersion":"1000"}},"items":[{}]}}"#,
        items.join(",")
    )
}
fn meta(name: &str, uid: &str, namespace: Option<&str>, extra: &str) -> String {
    let ns = namespace
        .map(|n| format!(r#""namespace":"{n}","#))
        .unwrap_or_default();
    format!(
        r#"{{"metadata":{{"name":"{name}","uid":"{uid}",{ns}"resourceVersion":"900"{extra}}}}}"#
    )
}
fn pod_json(name: &str, uid: &str, crashing: bool) -> String {
    let state = if crashing {
        r#"{"waiting":{"reason":"CrashLoopBackOff","message":"back-off 5m0s"}}"#
    } else {
        r#"{"running":{"startedAt":"2024-01-01T00:00:00Z"}}"#
    };
    format!(
        r#"{{"metadata":{{"name":"{name}","uid":"{uid}","namespace":"prod","resourceVersion":"900",
             "labels":{{"app":"api"}},
             "ownerReferences":[{{"apiVersion":"apps/v1","kind":"ReplicaSet","name":"api-7f9","uid":"uid-rs","controller":true}}]}},
           "spec":{{"containers":[{{"name":"app","image":"nginx",
             "envFrom":[{{"secretRef":{{"name":"api-token"}}}}]}}],
             "volumes":[{{"name":"cfg","configMap":{{"name":"api-config"}}}},
                        {{"name":"data","persistentVolumeClaim":{{"claimName":"api-data"}}}}]}},
           "status":{{"phase":"Running","containerStatuses":[
             {{"name":"app","ready":{ready},"restartCount":4,"image":"nginx","imageID":"","state":{state}}}]}}}}"#,
        ready = !crashing,
    )
}
fn script_lists(script: &Script) {
    script.route(
        "GET",
        "/api/v1/namespaces?",
        200,
        list(&[meta("prod", "uid-ns", None, "")], "Namespace"),
    );
    script.route(
        "GET",
        "/apis/apps/v1/deployments?",
        200,
        list(
            &[meta(
                "api",
                "uid-dep",
                Some("prod"),
                r#","labels":{"app.kubernetes.io/name":"nginx"}"#,
            )],
            "Deployment",
        ),
    );
    script.route(
        "GET",
        "/apis/apps/v1/replicasets?",
        200,
        list(
            &[meta(
                "api-7f9",
                "uid-rs",
                Some("prod"),
                r#","ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"api","uid":"uid-dep","controller":true}]"#,
            )],
            "ReplicaSet",
        ),
    );
    script.route("GET", "/apis/batch/v1/jobs?", 200, list(&[], "Job"));
    script.route("GET", "/apis/batch/v1/cronjobs?", 200, list(&[], "CronJob"));

    script.route(
        "GET",
        "/api/v1/pods?",
        200,
        list(
            &[
                pod_json("api-1", "uid-pod-1", true),
                pod_json("api-2", "uid-pod-2", false),
            ],
            "Pod",
        ),
    );
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        list(&[SERVICE_JSON.to_string()], "Service"),
    );
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        list(
            &[meta("api-config", "uid-cm", Some("prod"), "")],
            "ConfigMap",
        ),
    );
    script.route_accepting(
        "GET",
        "/api/v1/secrets?",
        METADATA_LIST_ACCEPT,
        200,
        list(
            &[meta("api-token", "uid-sec", Some("prod"), "")],
            "PartialObjectMetadata",
        ),
    );
    script.route(
        "GET",
        "/api/v1/persistentvolumeclaims?",
        200,
        list(&[CLAIM_JSON.to_string()], "PersistentVolumeClaim"),
    );
}
const LIVE_BUDGET: Duration = Duration::from_secs(10);
fn run_live(
    script: &Script,
    options: Options,
    settled: impl Fn(&[IngestEvent]) -> bool,
) -> (Sync, Vec<IngestEvent>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let sync = runtime
        .block_on(async {
            let client = script.client();
            sync_from(
                client,
                "prod",
                &options,
                tx,
                Arc::new(IngestMetrics::default()),
            )
            .await
        })
        .expect("the scripted cluster syncs");

    let mut live: Vec<IngestEvent> = Vec::new();
    let deadline = std::time::Instant::now() + LIVE_BUDGET;
    while !settled(&live) && std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(event) => live.push(event),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    live.extend(rx.try_iter());
    drop(runtime);
    (sync, live)
}
fn run(script: &Script, options: Options) -> Sync {
    run_live(script, options, |_| true).0
}
fn options() -> Options {
    Options {
        context: None,
        kubeconfig: None,
        probe_namespaces: vec!["prod".into()],
        sync_timeout: Duration::from_secs(5),
    }
}
fn resources(sync: &Sync) -> Vec<&ResourceEvent> {
    sync.events
        .iter()
        .filter_map(|e| match e {
            IngestEvent::Resource(r) => Some(r),
            _ => None,
        })
        .collect()
}
fn capability(sync: &Sync, kind: KindId) -> Option<Capability> {
    sync.events.iter().find_map(|e| match e {
        IngestEvent::Capability { kind: k, verdict } if *k == kind => Some(*verdict),
        _ => None,
    })
}
fn synced(sync: &Sync) -> Vec<KindId> {
    let mut out: Vec<KindId> = sync
        .events
        .iter()
        .filter_map(|e| match e {
            IngestEvent::Synced { kind } => Some(*kind),
            _ => None,
        })
        .collect();
    out.sort_by_key(|k| k.0);
    out
}
// ---------------------------------------------------------------------------
// Phase F: the read path. Tables, describe, log follow, containers, nodes --
// all driven against the same scripted API server, no cluster, no sockets.
// ---------------------------------------------------------------------------

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime")
}
fn sync_on(runtime: &tokio::runtime::Runtime, script: &Script) -> (Sync, EventReceiver) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let sync = runtime
        .block_on(async {
            sync_from(
                script.client(),
                "prod",
                &options(),
                tx,
                Arc::new(IngestMetrics::default()),
            )
            .await
        })
        .expect("the scripted cluster syncs");
    (sync, rx)
}
type EventReceiver = crossbeam_channel::Receiver<IngestEvent>;
fn wait<T: Send + 'static>(rx: &std::sync::mpsc::Receiver<T>) -> T {
    rx.recv_timeout(Duration::from_secs(5))
        .expect("a reply within the budget")
}
const NODE_PODS_JSON: &str = r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[
    {"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod"},
     "spec":{"nodeName":"n1",
             "containers":[{"name":"app","resources":{"requests":{"cpu":"500m","memory":"64Mi"}}}],
             "initContainers":[{"name":"sidecar-log","restartPolicy":"Always",
                                "resources":{"requests":{"cpu":"100m"}}}]},
     "status":{"phase":"Running"}},
    {"metadata":{"name":"api-2","uid":"uid-pod-2","namespace":"prod"},
     "spec":{"nodeName":"n1",
             "containers":[{"name":"app","resources":{"requests":{"cpu":"1","memory":"0"}}}]},
     "status":{"phase":"Running"}}
]}"#;
// ---------------------------------------------------------------------------
// Phase G: schema sources and the editable manifest. The catalog, per-GV
// documents, CRD schemas, and one object rendered as YAML -- all wire-proven
// against the scripted server; parsing lives in k10s-edit and is unit-tested
// there.
// ---------------------------------------------------------------------------

const OPENAPI_INDEX_JSON: &str = r#"{"paths":{
    "api/v1":{"serverRelativeURL":"/openapi/v3/api/v1?hash=aaa"},
    "apis/apps/v1":{"serverRelativeURL":"/openapi/v3/apis/apps/v1?hash=bbb"},
    "logs":{"serverRelativeURL":"/logs"}}}"#;
// The last-applied annotation as the API server carries it: a JSON document
// inside a JSON string. It is what the object was declared to be, and the live
// object differs from it -- the cluster moved the image on.
fn pod_with_last_applied() -> String {
    let declared = r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"api-1","namespace":"prod"},"spec":{"containers":[{"image":"nginx:1.26","name":"app"}]}}"#;
    let escaped = declared.replace('"', "\\\"");
    format!(
        r#"{{"metadata":{{"name":"api-1","uid":"uid-pod-1","namespace":"prod","resourceVersion":"900",
             "managedFields":[{{"manager":"kubectl","operation":"Update"}}],
             "annotations":{{"kubectl.kubernetes.io/last-applied-configuration":"{escaped}","team":"platform"}}}},
           "spec":{{"containers":[{{"image":"nginx:1.27","name":"app"}}]}},
           "status":{{"phase":"Running"}}}}"#
    )
}
fn pod_request() -> k10s_data::describe::DescribeRequest {
    k10s_data::describe::DescribeRequest {
        kind: KindId::POD,
        namespace: Some("prod".to_string()),
        name: "api-1".to_string(),
        uid: "uid-pod-1".to_string(),
    }
}
