use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use k10s_core::{Capability, IngestEvent, KindId, Op, Payload, ResourceEvent, Severity};
use k10s_data::{IngestMetrics, Options, Sync, sync_from};
use kube::client::Body;
use tower::Service;

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    accept: String,
}

struct Route {
    method: &'static str,
    matches: String,
    accept_contains: Option<&'static str>,
    status: u16,
    body: String,
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
        let accept = req
            .headers()
            .get(http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let mut state = self.state.lock().expect("script lock");
        state.seen.push(Seen {
            method: method.clone(),
            path: path.clone(),
            accept: accept.clone(),
        });

        let routable = path.replacen("?&", "?", 1);
        let hit = state.routes.iter_mut().find(|r| {
            !r.used
                && r.method == method
                && routable.starts_with(&r.matches)
                && r.accept_contains.is_none_or(|want| accept.contains(want))
        });
        let (status, body) = match hit {
            Some(route) => {
                route.used = true;
                (route.status, route.body.clone())
            }
            None if path.contains("watch=true") => return Box::pin(std::future::pending()),
            None => (
                404,
                r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"unscripted"}"#
                    .to_string(),
            ),
        };
        Box::pin(std::future::ready(Ok(http::Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.into_bytes()))
            .expect("a response"))))
    }
}

const SERVICE_JSON: &str = r#"{"metadata":{"name":"api","uid":"uid-svc","namespace":"prod","resourceVersion":"900"},
    "spec":{"type":"ClusterIP","selector":{"app":"api"},"ports":[{"port":80}]}}"#;

const CLAIM_JSON: &str = r#"{"metadata":{"name":"api-data","uid":"uid-pvc","namespace":"prod","resourceVersion":"900"},
    "spec":{"resources":{"requests":{"storage":"8Gi"}}},
    "status":{"phase":"Bound","capacity":{"storage":"8Gi"}}}"#;

const POD_IN_PROD_JSON: &str = r#"{"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod","resourceVersion":"900",
      "ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"api","uid":"uid-dep","controller":true}]},
    "status":{"phase":"Running","containerStatuses":[
      {"name":"app","ready":true,"restartCount":0,"image":"nginx","imageID":"","state":{"running":{}}}]}}"#;

const METADATA_LIST_ACCEPT: &str = "as=PartialObjectMetadataList";

const EXPIRED_WATCH_EVENT: &str = r#"{"type":"ERROR","object":{"kind":"Status","apiVersion":"v1","status":"Failure","code":410,"reason":"Expired","message":"too old resource version: 900 (1200)"}}
"#;

fn api_resource(kind: &str, plural: &str, namespaced: bool) -> String {
    format!(
        r#"{{"name":"{plural}","singularName":"","namespaced":{namespaced},"kind":"{kind}","verbs":["get","list","watch"]}}"#
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
            r#"{{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"apps/v1","resources":[{},{}]}}"#,
            api_resource("Deployment", "deployments", true),
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
            r#"{{"kind":"APIResourceList","groupVersion":"v1","resources":[{},{},{},{},{},{}]}}"#,
            api_resource("Namespace", "namespaces", false),
            api_resource("Pod", "pods", true),
            api_resource("Service", "services", true),
            api_resource("ConfigMap", "configmaps", true),
            api_resource("Secret", "secrets", true),
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

fn script_unavailable_reviews_for_one_kind(script: &Script) {
    for _ in 0..2 {
        script.route(
            "POST",
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            503,
            r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":503,"reason":"ServiceUnavailable","message":"the server is currently unable to handle the request"}"#,
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

fn live_resources(live: &[IngestEvent]) -> Vec<&ResourceEvent> {
    live.iter()
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

#[test]
fn a_scripted_cluster_produces_a_conforming_initial_sync() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let (sync, live) = run_live(&script, options(), |_| {
        script.requests_for("watch=true").len() >= 10
    });
    let report = &sync.report;

    assert!(
        !report.aggregated_discovery,
        "this server has no aggregated document, so the fallback path ran"
    );
    assert_eq!(report.server_version.as_deref(), Some("v1.32.3"));
    assert_eq!(report.kinds_discovered, 10);
    assert_eq!(
        report.kinds_watched, 10,
        "every kind of the watch set this server serves"
    );
    assert_eq!(report.streams, 10, "cluster-wide, so one stream each");
    assert_eq!(report.namespaced_streams, 0);
    assert!(!report.probe_degraded);

    let watches = script.requests_for("watch=true");
    assert_eq!(watches.len(), 10, "one open watch per stream: {watches:?}");
    assert!(report.desyncs.is_empty(), "{:?}", report.desyncs);
    assert!(
        live.is_empty(),
        "a cluster that stops changing emits nothing: {live:?}"
    );

    let (input, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default(), "{stats:?}");
    assert_eq!(input.namespaces.len(), 1);
    let prod = &input.namespaces[0];
    assert_eq!(&*prod.name, "prod");
    assert_eq!(
        prod.workloads.len(),
        1,
        "the Deployment, not the ReplicaSet"
    );
    let api = &prod.workloads[0];
    assert_eq!(&*api.name, "api");
    assert_eq!(api.kind, KindId::DEPLOYMENT);
    assert_eq!(api.tool, k10s_core::ToolId::NGINX, "read from the labels");
    assert_eq!(api.pods.len(), 2);
    assert_eq!(
        api.sats.len(),
        4,
        "service by selector, config map and secret and claim by reference"
    );

    let crashing = resources(&sync)
        .into_iter()
        .find(|r| r.uid.as_ref() == "uid-pod-1")
        .expect("the crashing pod");
    let Payload::Instance { state } = crashing.payload else {
        panic!("expected an instance")
    };
    assert_eq!(state.severity, Severity::Err);
    assert_eq!(
        sync.catalog.reason_display(state.reason),
        "CrashLoopBackOff"
    );
    assert_eq!(state.reason, k10s_core::ReasonId::CRASH_LOOP_BACK_OFF);

    assert_eq!(synced(&sync).len(), 10);
    assert_eq!(capability(&sync, KindId::POD), Some(Capability::Watchable));
    assert_eq!(
        capability(&sync, KindId::SECRET),
        Some(Capability::Watchable)
    );
}

#[test]
fn a_410_mid_watch_relists_and_reaps_what_vanished() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/pods?watch=true", 200, EXPIRED_WATCH_EVENT);
    script.route(
        "GET",
        "/api/v1/pods?",
        200,
        list(&[pod_json("api-1", "uid-pod-1", true)], "Pod"),
    );

    let (sync, live) = run_live(&script, options(), |live| {
        live_resources(live).iter().any(|r| r.op == Op::Deleted)
    });

    assert_eq!(sync.report.assemble.instances, 2);

    let deleted: Vec<&ResourceEvent> = live_resources(&live)
        .into_iter()
        .filter(|r| r.op == Op::Deleted)
        .collect();
    assert_eq!(
        deleted.len(),
        1,
        "one delete, for the one pod the relist did not list: {live:?}"
    );
    assert_eq!(&*deleted[0].uid, "uid-pod-2");
    assert_eq!(
        deleted[0].kind,
        KindId::POD,
        "the kind comes from the copy that went away, not from a guess"
    );
    assert_eq!(
        deleted[0].parent.as_deref(),
        Some("uid-dep"),
        "a delete still says where the object was"
    );

    assert!(
        live_resources(&live)
            .iter()
            .any(|r| &*r.uid == "uid-pod-1" && r.op == Op::Modified),
        "{live:?}"
    );

    let lists = script
        .requests_for("/api/v1/pods?")
        .into_iter()
        .filter(|r| !r.path.contains("watch=true"))
        .count();
    assert_eq!(lists, 2, "the 410 has to relist, not resume");
    let expired = k10s_core::DesyncReason::Expired;
    assert!(
        sync.report.desyncs.contains(&(KindId::POD, expired))
            || live.iter().any(|e| matches!(
                e,
                IngestEvent::Desync { kind, reason } if *kind == KindId::POD && *reason == expired
            )),
        "a 410 is an Expired desync, not a reconnect: {:?} {live:?}",
        sync.report.desyncs
    );
}

#[test]
fn a_secret_is_requested_through_the_metadata_projection_and_a_pod_is_not() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let sync = run(&script, options());

    let secret_requests = script.requests_for("/secrets");
    assert!(
        !secret_requests.is_empty(),
        "the secret list has to have been requested at all"
    );
    for request in &secret_requests {
        assert!(
            request.accept.contains("PartialObjectMetadata"),
            "a Secret was requested as a whole object: {request:?}"
        );
    }

    let pod_requests = script.requests_for("/pods");
    assert!(!pod_requests.is_empty());
    for request in &pod_requests {
        assert!(
            !request.accept.contains("PartialObjectMetadata"),
            "a Pod must be fetched whole or its state is unknowable: {request:?}"
        );
    }

    for request in script.requests_for("/configmaps") {
        assert!(
            request.accept.contains("PartialObjectMetadata"),
            "{request:?}"
        );
    }

    let secret = resources(&sync)
        .into_iter()
        .find(|r| r.kind == KindId::SECRET)
        .expect("the secret is on the map");
    let Payload::Attached { detail, .. } = &secret.payload else {
        panic!("expected an attachment")
    };
    assert_eq!(&*secret.name, "api-token");
    assert!(detail.is_empty());
}

#[test]
fn the_probe_sends_one_rules_review_per_namespace_and_names_plural_resources() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],"resourceRules":[]}}"#,
    );
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let mut opts = options();
    opts.probe_namespaces = vec!["team-a".into(), "team-b".into()];
    let sync = run(&script, opts);

    let reviews = script.requests_for("selfsubjectrulesreviews");
    assert_eq!(reviews.len(), 2, "one per namespace, not one per kind");

    let access = script.requests_for("selfsubjectaccessreviews");
    assert_eq!(
        access.len(),
        20,
        "list and watch for each of ten watched kinds"
    );
    for request in reviews.iter().chain(access.iter()) {
        assert_eq!(request.method, "POST", "{request:?}");
    }
    assert!(sync.report.probe_requests >= 22);
}

#[test]
fn a_kind_the_probe_denies_is_forbidden_and_never_requested() {
    let script = Script::default();
    script_discovery(&script);
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],
                      "resourceRules":[{"apiGroups":[""],"resources":["pods"],"verbs":["list","watch"]}]}}"#,
    );
    script_access_reviews(&script, false, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        list(&[POD_IN_PROD_JSON.to_string()], "Pod"),
    );

    let sync = run(&script, options());

    assert_eq!(capability(&sync, KindId::POD), Some(Capability::Watchable));
    assert!(
        sync.report.namespaced_streams > 0,
        "a namespace-scoped grant must produce a namespace-scoped stream"
    );
    assert_eq!(
        capability(&sync, KindId::SECRET),
        Some(Capability::Forbidden)
    );
    assert_eq!(
        capability(&sync, KindId::DEPLOYMENT),
        Some(Capability::Forbidden)
    );
    assert_eq!(
        capability(&sync, KindId::NAMESPACE),
        Some(Capability::Forbidden)
    );

    assert!(
        script.requests_for("/secrets").is_empty(),
        "a denied kind must not be asked for"
    );
    assert!(!synced(&sync).contains(&KindId::SECRET));

    let pod_requests = script.requests_for("/pods");
    assert!(!pod_requests.is_empty());
    assert!(
        pod_requests
            .iter()
            .all(|r| r.path.contains("/namespaces/prod/pods")),
        "{pod_requests:?}"
    );

    assert_eq!(sync.report.assemble.scopes, 0);
    assert!(
        sync.report.assemble.unknown_namespace > 0,
        "an object in an unreadable namespace has to be counted: {:?}",
        sync.report.assemble
    );
    assert_eq!(sync.report.assemble.instances, 0);
    let (_, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
}

#[test]
fn a_kind_the_probe_could_not_answer_for_is_attempted_rather_than_denied() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_unavailable_reviews_for_one_kind(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let sync = run(&script, options());
    let report = &sync.report;

    assert_eq!(report.kinds_unanswered, 1);
    assert!(!report.probe_degraded, "nine kinds were answered");
    assert_eq!(
        report.streams, 10,
        "every kind opens a stream, the unanswered one included"
    );
    assert_eq!(
        report.namespaced_streams, 0,
        "no answer is not a cluster-wide denial, so nothing falls back"
    );

    assert!(!script.requests_for("/api/v1/namespaces?").is_empty());
    assert_eq!(
        capability(&sync, KindId::NAMESPACE),
        Some(Capability::Watchable)
    );
    assert_eq!(report.assemble.scopes, 1);
    assert_eq!(report.assemble.instances, 2);
    let (_, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
}

#[test]
fn one_unanswered_kind_does_not_forbid_a_restricted_account_its_own_namespace() {
    let script = Script::default();
    script_discovery(&script);
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],
                      "resourceRules":[{"apiGroups":[""],"resources":["pods"],"verbs":["list","watch"]}]}}"#,
    );
    script_unavailable_reviews_for_one_kind(&script);
    script_access_reviews(&script, false, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        list(&[POD_IN_PROD_JSON.to_string()], "Pod"),
    );

    let sync = run(&script, options());
    let report = &sync.report;

    assert!(
        !report.probe_degraded,
        "nine kinds were answered, and denied"
    );
    assert_eq!(
        capability(&sync, KindId::SECRET),
        Some(Capability::Forbidden)
    );
    assert!(script.requests_for("/secrets").is_empty());
    assert_eq!(report.kinds_unanswered, 1);
    assert_eq!(
        capability(&sync, KindId::NAMESPACE),
        Some(Capability::Watchable)
    );
    assert_eq!(capability(&sync, KindId::POD), Some(Capability::Watchable));
    let pod_requests = script.requests_for("/pods");
    assert!(!pod_requests.is_empty());
    assert!(
        pod_requests
            .iter()
            .all(|r| r.path.contains("/namespaces/prod/pods")),
        "a cluster-wide pod list is the 403 the probe already knew about: {pod_requests:?}"
    );
    assert_eq!(
        report.streams, 2,
        "the namespace attempt and the pod fallback, and nothing else"
    );
    assert_eq!(report.namespaced_streams, 1);

    assert_eq!(report.assemble.scopes, 1);
    assert_eq!(report.assemble.instances, 1);
    let (_, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
}

#[test]
fn a_403_on_the_list_becomes_a_labelled_desync_rather_than_a_retry_loop() {
    let denied = Script::default();
    script_discovery(&denied);
    script_rules_review(&denied);
    script_access_reviews(&denied, true, 32);
    denied.route(
        "GET",
        "/api/v1/secrets?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"secrets is forbidden"}"#,
    );
    script_lists(&denied);

    let sync = run(&denied, options());

    assert!(
        sync.report
            .desyncs
            .iter()
            .any(|(kind, reason)| *kind == KindId::SECRET
                && *reason == k10s_core::DesyncReason::Forbidden),
        "a 403 has to surface as a Forbidden desync: {:?}",
        sync.report.desyncs
    );
    assert!(
        !k10s_core::DesyncReason::Forbidden.is_recoverable(),
        "and it must not look retryable"
    );
    assert_eq!(
        denied.requests_for("/secrets").len(),
        1,
        "a denied list must be attempted once"
    );
    assert!(!synced(&sync).contains(&KindId::SECRET));
    assert!(sync.report.assemble.instances >= 2);
    let (_, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
}

#[test]
fn a_server_that_does_not_serve_a_kind_makes_it_absent_rather_than_broken() {
    let script = Script::default();
    script.route_accepting("GET", "/apis", "APIGroupDiscoveryList", 406, "{}");
    script.route_accepting("GET", "/api", "APIGroupDiscoveryList", 406, "{}");
    script.route(
        "GET",
        "/apis",
        200,
        r#"{"kind":"APIGroupList","apiVersion":"v1","groups":[]}"#,
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
            r#"{{"kind":"APIResourceList","groupVersion":"v1","resources":[{},{}]}}"#,
            api_resource("Namespace", "namespaces", false),
            api_resource("Pod", "pods", true),
        ),
    );
    script.route("GET", "/version", 404, "{}");
    script_rules_review(&script);
    script_access_reviews(&script, true, 8);
    script.route(
        "GET",
        "/api/v1/namespaces?",
        200,
        list(&[meta("prod", "uid-ns", None, "")], "Namespace"),
    );
    script.route("GET", "/api/v1/pods?", 200, list(&[], "Pod"));

    let sync = run(&script, options());
    assert_eq!(sync.report.kinds_discovered, 2);
    assert_eq!(sync.report.kinds_watched, 2);
    assert_eq!(sync.report.server_version, None, "no /version, no version");
    assert!(script.requests_for("/cronjobs").is_empty());
    assert!(script.requests_for("/deployments").is_empty());
    assert_eq!(synced(&sync).len(), 2);

    assert_eq!(sync.report.assemble.scopes, 1);
    assert_eq!(sync.report.assemble.instances, 0);
    let (input, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
    assert_eq!(input.namespaces.len(), 1);
    assert_eq!(input.total_pods, 0);
}
