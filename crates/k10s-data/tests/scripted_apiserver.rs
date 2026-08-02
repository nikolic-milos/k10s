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
            Some(route) if route.hang => {
                route.used = true;
                return Box::pin(std::future::pending());
            }
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

#[test]
fn the_inspector_reads_events_and_logs_and_labels_a_denial() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        200,
        r#"{"kind":"EventList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"name":"e1","namespace":"prod"},"type":"Warning","reason":"BackOff",
             "message":"Back-off restarting failed container","count":7,
             "lastTimestamp":"2026-08-02T04:00:00Z",
             "involvedObject":{"kind":"Pod","name":"api-1","namespace":"prod"}},
            {"metadata":{"name":"e2","namespace":"prod"},"type":"Normal","reason":"Pulled",
             "message":"Container image pulled","count":1,
             "lastTimestamp":"2026-08-02T05:00:00Z",
             "involvedObject":{"kind":"Pod","name":"api-1","namespace":"prod"}}
        ]}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1/log?",
        200,
        "2026-08-02T05:00:00Z listening on :8080\n2026-08-02T05:00:01Z ready\n",
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"events is forbidden"}"#,
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let (tx, _rx) = crossbeam_channel::unbounded();
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

    let recv = |rx: futures::channel::oneshot::Receiver<InspectDetail>| {
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), rx).await })
            .expect("a reply within the budget")
            .expect("the fetch task replies")
    };

    let (reply, rx) = futures::channel::oneshot::channel();
    sync.inspector.fetch_events("prod", "api-1", move |detail| {
        let _ = reply.send(detail);
    });
    let InspectDetail::Events(events) = recv(rx) else {
        panic!("events must resolve");
    };
    assert_eq!(events.len(), 2);
    assert_eq!(
        (events[0].reason.as_str(), events[0].kind.as_str()),
        ("Pulled", "Normal"),
        "newest first: {events:?}"
    );
    assert_eq!(events[1].count, 7);
    let sent = script.requests_for("/events");
    assert!(
        sent[0]
            .path
            .contains("fieldSelector=involvedObject.name%3Dapi-1")
            || sent[0]
                .path
                .contains("fieldSelector=involvedObject.name=api-1"),
        "the query must scope to the object: {}",
        sent[0].path
    );

    let (reply, rx) = futures::channel::oneshot::channel();
    sync.inspector
        .fetch_log_tail("prod", &Arc::from("api-1"), move |detail| {
            let _ = reply.send(detail);
        });
    let InspectDetail::Log(tail) = recv(rx) else {
        panic!("logs must resolve");
    };
    assert_eq!(tail.lines.len(), 2);
    assert!(tail.lines[1].ends_with("ready"));
    let log_request = &script.requests_for("/pods/api-1/log")[0];
    assert!(
        log_request.path.contains("tailLines=200"),
        "the tail must be bounded: {}",
        log_request.path
    );

    let (reply, rx) = futures::channel::oneshot::channel();
    sync.inspector.fetch_events("prod", "api-1", move |detail| {
        let _ = reply.send(detail);
    });
    assert_eq!(
        recv(rx),
        InspectDetail::Denied { what: "events" },
        "a 403 is a labelled state, not an error string"
    );

    drop(runtime);
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
fn a_failed_rules_review_keeps_its_namespace_attempted_rather_than_invisible() {
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
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        503,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":503,"reason":"ServiceUnavailable","message":"etcdserver: request timed out"}"#,
    );
    script_access_reviews(&script, false, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/team-a/pods?",
        200,
        list(&[POD_IN_PROD_JSON.to_string()], "Pod"),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/team-b/pods?",
        200,
        list(&[], "Pod"),
    );
    for (path, kind) in [
        ("/apis/apps/v1/namespaces/team-b/deployments?", "Deployment"),
        ("/apis/apps/v1/namespaces/team-b/replicasets?", "ReplicaSet"),
        ("/apis/batch/v1/namespaces/team-b/jobs?", "Job"),
        ("/apis/batch/v1/namespaces/team-b/cronjobs?", "CronJob"),
        ("/api/v1/namespaces/team-b/services?", "Service"),
        ("/api/v1/namespaces/team-b/configmaps?", "ConfigMap"),
        ("/api/v1/namespaces/team-b/secrets?", "Secret"),
        (
            "/api/v1/namespaces/team-b/persistentvolumeclaims?",
            "PersistentVolumeClaim",
        ),
    ] {
        script.route("GET", path, 200, list(&[], kind));
    }

    let mut opts = options();
    opts.probe_namespaces = vec!["team-a".into(), "team-b".into()];
    let sync = run(&script, opts);
    let report = &sync.report;

    assert_eq!(report.probed_namespaces, vec!["team-a", "team-b"]);
    assert_eq!(
        report.namespaces_unanswered,
        vec!["team-b"],
        "the report must name the namespace it is guessing about"
    );
    assert!(
        !report.probe_degraded,
        "one failed rules review out of two is a gap, not a dead probe"
    );

    assert_eq!(capability(&sync, KindId::POD), Some(Capability::Watchable));
    assert!(
        !script.requests_for("/namespaces/team-a/pods").is_empty(),
        "the granted namespace lists pods"
    );
    assert!(
        !script.requests_for("/namespaces/team-b/pods").is_empty(),
        "the unanswered namespace must be attempted, not silently dropped"
    );

    assert!(
        !script
            .requests_for("/namespaces/team-b/deployments")
            .is_empty(),
        "a kind the answered namespace denies is still attempted where no answer exists"
    );
    assert!(
        script
            .requests_for("/namespaces/team-a/deployments")
            .is_empty(),
        "the answered namespace's denial still gates"
    );
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

const DEPLOYMENT_TABLE_JSON: &str = r#"{"kind":"Table","apiVersion":"meta.k8s.io/v1",
    "metadata":{"resourceVersion":"1000","continue":"page-2"},
    "columnDefinitions":[
        {"name":"Name","type":"string","format":"name","priority":0},
        {"name":"Ready","type":"string","priority":0},
        {"name":"Replicas","type":"integer","priority":0},
        {"name":"Containers","type":"string","priority":1}],
    "rows":[{"cells":["api","1/1",1,"app"],
             "object":{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
                       "metadata":{"name":"api","namespace":"prod","uid":"uid-dep"}}}]}"#;

#[test]
fn any_discovered_kind_lists_as_a_server_side_table_with_bounded_pages() {
    use k10s_core::KindId;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route_accepting(
        "GET",
        "/apis/apps/v1/deployments?",
        "as=Table",
        200,
        DEPLOYMENT_TABLE_JSON,
    );
    script.route_accepting(
        "GET",
        "/api/v1/pods?",
        "as=Table",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"pods is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let kinds = sync.reader.kinds();
    assert_eq!(kinds.len(), 10, "every discovered listable kind is offered");
    let displays: Vec<&str> = kinds.iter().map(|k| k.display.as_str()).collect();
    let mut sorted = displays.clone();
    sorted.sort_unstable();
    assert_eq!(displays, sorted, "kinds arrive sorted for a picker");
    let deployments = kinds
        .iter()
        .find(|k| k.display == "deployments.apps")
        .expect("the group is part of the name");
    assert_eq!(deployments.kind, "Deployment");
    assert!(deployments.namespaced);
    assert_eq!(deployments.verdict, Some(Capability::Watchable));
    assert!(kinds.iter().any(|k| k.display == "pods"));

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_table(KindId::DEPLOYMENT, move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the table must resolve");
    };
    assert_eq!(
        page.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["Namespace", "Name", "Ready", "Replicas", "Containers"],
        "a cluster-wide list of a namespaced kind gains the namespace column"
    );
    assert!(page.columns[4].wide, "priority > 0 is a wide column");
    assert_eq!(page.rows[0].cells, ["prod", "api", "1/1", "1", "app"]);
    assert_eq!(page.rows[0].name, "api");
    assert_eq!(page.rows[0].namespace.as_deref(), Some("prod"));
    assert_eq!(page.rows[0].uid, "uid-dep");
    assert!(
        page.truncated,
        "a continue token surfaces, it is not chased"
    );

    let table_requests: Vec<Seen> = script
        .requests_for("/apis/apps/v1/deployments")
        .into_iter()
        .filter(|r| r.accept.contains("as=Table"))
        .collect();
    assert_eq!(table_requests.len(), 1, "{table_requests:?}");
    assert!(
        table_requests[0]
            .accept
            .contains("as=Table;v=v1;g=meta.k8s.io"),
        "the server renders the columns: {}",
        table_requests[0].accept
    );
    assert!(
        table_requests[0].path.contains("limit=500"),
        "a table page is bounded: {}",
        table_requests[0].path
    );

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_table(KindId::POD, move |outcome| {
        let _ = tx.send(outcome);
    });
    assert_eq!(
        wait(&rx),
        Fetched::Denied { what: "table" },
        "a 403 is a labelled state, not an error string"
    );

    drop(runtime);
}

#[test]
fn describe_renders_fields_walks_owners_and_joins_events_by_uid() {
    use k10s_core::KindId;
    use k10s_data::describe::DescribeRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        200,
        pod_json("api-1", "uid-pod-1", true),
    );
    script.route_accepting(
        "GET",
        "/apis/apps/v1/namespaces/prod/replicasets/api-7f9",
        "as=PartialObjectMetadata",
        200,
        r#"{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
            "metadata":{"name":"api-7f9","namespace":"prod","uid":"uid-rs",
              "ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"api","uid":"uid-dep","controller":true}]}}"#,
    );
    script.route_accepting(
        "GET",
        "/apis/apps/v1/namespaces/prod/deployments/api",
        "as=PartialObjectMetadata",
        200,
        r#"{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
            "metadata":{"name":"api","namespace":"prod","uid":"uid-dep"}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        200,
        r#"{"kind":"EventList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"name":"e1","namespace":"prod"},"type":"Warning","reason":"BackOff",
             "message":"Back-off restarting failed container","count":7,
             "lastTimestamp":"2026-08-02T04:00:00Z",
             "involvedObject":{"kind":"Pod","name":"api-1","namespace":"prod","uid":"uid-pod-1"}}
        ]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_describe(
        DescribeRequest {
            kind: KindId::POD,
            namespace: Some("prod".to_string()),
            name: "api-1".to_string(),
            uid: "uid-pod-1".to_string(),
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Ok(described) = wait(&rx) else {
        panic!("describe must resolve");
    };
    assert_eq!(described.title, "Pod api-1");
    let text = described.lines.join("\n");
    assert!(text.contains("kind: Pod"), "{text}");
    assert!(
        text.contains("reason: CrashLoopBackOff"),
        "field-level rendering reaches into status: {text}"
    );
    let rs_at = described
        .lines
        .iter()
        .position(|l| l.trim() == "ReplicaSet api-7f9")
        .unwrap_or_else(|| panic!("the direct owner is walked: {text}"));
    let dep_at = described
        .lines
        .iter()
        .position(|l| l.trim() == "Deployment api")
        .unwrap_or_else(|| panic!("the chain reaches the root: {text}"));
    assert!(rs_at < dep_at, "the chain reads upward");
    assert!(text.contains("BackOff x7"), "{text}");

    for request in script.requests_for("/replicasets/api-7f9") {
        assert!(
            request.accept.contains("as=PartialObjectMetadata"),
            "an owner hop needs metadata only: {request:?}"
        );
    }
    let events = script.requests_for("/namespaces/prod/events");
    assert!(
        events[0].path.contains("involvedObject.uid%3Duid-pod-1"),
        "events join by uid, not by name collision: {}",
        events[0].path
    );

    drop(runtime);
}

#[test]
fn a_secret_describe_is_metadata_only_by_construction() {
    use k10s_core::KindId;
    use k10s_data::describe::DescribeRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route_accepting(
        "GET",
        "/api/v1/namespaces/prod/secrets/api-token",
        "as=PartialObjectMetadata",
        200,
        r#"{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
            "metadata":{"name":"api-token","namespace":"prod","uid":"uid-sec",
                        "annotations":{"kubernetes.io/service-account.name":"api"}}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        200,
        r#"{"kind":"EventList","apiVersion":"v1","metadata":{},"items":[]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_describe(
        DescribeRequest {
            kind: KindId::SECRET,
            namespace: Some("prod".to_string()),
            name: "api-token".to_string(),
            uid: "uid-sec".to_string(),
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Ok(described) = wait(&rx) else {
        panic!("describe must resolve");
    };
    let text = described.lines.join("\n");
    assert!(
        described.lines[0].contains("values withheld"),
        "the document says what is missing: {text}"
    );
    assert!(text.contains("kind: Secret"), "{text}");
    assert!(
        !text.contains("PartialObjectMetadata"),
        "the wire shape is not the story: {text}"
    );
    assert!(text.contains("(none recorded)"), "{text}");

    let secret_requests = script.requests_for("/secrets/api-token");
    assert!(!secret_requests.is_empty());
    for request in &secret_requests {
        assert!(
            request.accept.contains("as=PartialObjectMetadata"),
            "a Secret describe must be structurally metadata-only: {request:?}"
        );
    }

    drop(runtime);
}

#[test]
fn a_log_follow_streams_lines_ends_labelled_and_cancels_mid_open() {
    use k10s_data::logs::{LogChunk, LogRequest};

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1/log?",
        200,
        "2026-08-02T05:00:00Z listening on :8080\n2026-08-02T05:00:01Z ready\n2026-08-02T05:00:02Z serving\n",
    );
    script.route_hanging("GET", "/api/v1/namespaces/prod/pods/api-2/log?");
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-3/log?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"pods/log is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let request = |pod: &str| LogRequest {
        namespace: "prod".to_string(),
        pod: pod.to_string(),
        container: None,
        previous: false,
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.follow_log(
        request("api-1"),
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );
    let mut lines: Vec<String> = Vec::new();
    loop {
        match wait(&rx) {
            LogChunk::Lines(batch) => lines.extend(batch),
            LogChunk::Ended { why } => {
                assert_eq!(why, "the stream ended");
                break;
            }
            other => panic!("unexpected chunk {other:?}"),
        }
    }
    assert_eq!(lines.len(), 3);
    assert!(lines[1].ends_with("ready"), "{lines:?}");
    drop(stop);
    let follow = &script.requests_for("/pods/api-1/log")[0];
    assert!(follow.path.contains("follow=true"), "{}", follow.path);
    assert!(follow.path.contains("tailLines=500"), "{}", follow.path);
    assert!(follow.path.contains("timestamps=true"), "{}", follow.path);

    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.follow_log(
        request("api-2"),
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "the held connection produces nothing"
    );
    drop(stop);
    assert_eq!(
        wait(&rx),
        LogChunk::Ended { why: "stopped" },
        "dropping the guard cancels a follow that is still opening"
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.follow_log(
        request("api-3"),
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );
    assert_eq!(wait(&rx), LogChunk::Denied { what: "logs" });

    drop(runtime);
}

#[test]
fn containers_come_from_the_pod_spec_in_run_order() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        200,
        r#"{"metadata":{"name":"api-1","namespace":"prod","uid":"uid-pod-1","resourceVersion":"900"},
            "spec":{"containers":[{"name":"app","image":"nginx"},{"name":"proxy","image":"envoy"}],
                    "initContainers":[{"name":"init-db","image":"flyway"}],
                    "ephemeralContainers":[{"name":"debug","image":"busybox"}]},
            "status":{"phase":"Running"}}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_containers("prod", "api-1", move |outcome| {
            let _ = tx.send(outcome);
        });
    assert_eq!(
        wait(&rx),
        Fetched::Ok(vec![
            "app".to_string(),
            "proxy".to_string(),
            "init-db".to_string(),
            "debug".to_string(),
        ])
    );

    drop(runtime);
}

const NODE_LIST_JSON: &str = r#"{"kind":"NodeList","apiVersion":"v1","metadata":{},"items":[
    {"metadata":{"name":"n1","uid":"uid-n1",
                 "labels":{"node-role.kubernetes.io/control-plane":"","kubernetes.io/hostname":"n1"}},
     "spec":{"taints":[{"key":"dedicated","value":"infra","effect":"NoSchedule"}]},
     "status":{"allocatable":{"cpu":"4","memory":"16Gi","pods":"110"},
               "conditions":[{"type":"Ready","status":"True"},
                             {"type":"MemoryPressure","status":"False"}],
               "nodeInfo":{"kubeletVersion":"v1.32.3"}}}
]}"#;

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

#[test]
fn the_node_table_measures_allocatable_requests_and_usage() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/nodes?", 200, NODE_LIST_JSON);
    script.route(
        "GET",
        "/api/v1/pods?fieldSelector=spec.nodeName%3Dn1",
        200,
        NODE_PODS_JSON,
    );
    script.route(
        "GET",
        "/apis/metrics.k8s.io/v1beta1/nodes?",
        200,
        r#"{"kind":"NodeMetricsList","apiVersion":"metrics.k8s.io/v1beta1","items":[
            {"metadata":{"name":"n1"},"usage":{"cpu":"250m","memory":"8Gi"}}]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_node_table(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the node table must resolve");
    };
    assert_eq!(
        page.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Name",
            "Status",
            "Roles",
            "Version",
            "Pods",
            "CPU req",
            "Memory req",
            "CPU use",
            "Memory use",
            "Taints",
        ]
    );
    assert_eq!(page.rows.len(), 1);
    let row = &page.rows[0];
    assert_eq!(row.name, "n1");
    assert_eq!(row.uid, "uid-n1");
    assert_eq!(
        row.cells,
        [
            "n1",
            "Ready",
            "control-plane",
            "v1.32.3",
            "2/110 (2%)",
            "1600m/4 (40%)",
            "64Mi/16.0Gi (0%)",
            "250m/4 (6%)",
            "8.0Gi/16.0Gi (50%)",
            "1",
        ],
        "the sidecar accumulates and the init floor is honoured"
    );
    assert!(!page.truncated);

    let pod_scan = &script.requests_for("fieldSelector=spec.nodeName")[0];
    assert!(
        pod_scan.path.contains("status.phase%21%3DSucceeded"),
        "terminated pods do not hold requests: {}",
        pod_scan.path
    );

    drop(runtime);
}

#[test]
fn a_cluster_without_metrics_server_hides_usage_rather_than_breaking() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/nodes?", 200, NODE_LIST_JSON);
    script.route(
        "GET",
        "/api/v1/pods?fieldSelector=spec.nodeName%3Dn1",
        200,
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_node_table(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the node table must resolve");
    };
    assert!(
        page.columns.iter().all(|c| !c.name.contains("use")),
        "absent metrics-server means invisible, not broken: {:?}",
        page.columns
    );
    assert_eq!(page.rows[0].cells[4], "0/110 (0%)");

    drop(runtime);
}
