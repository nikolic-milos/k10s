//! Overlay PromQL: Grafana names the metrics expr when it can; mesh observed
//! asks Istio, Hubble, and Linkerd as three Prometheus queries, never Hubble.

use crate::*;
use k10s_data::overlay;
use k10s_data::reach::ReachSettings;
use k10s_data::read::Fetched;

const EMPTY_MATRIX: &str = r#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#;

fn prometheus_service() -> String {
    r#"{"metadata":{"name":"prometheus","uid":"uid-prom","namespace":"monitoring","resourceVersion":"1",
        "labels":{"app.kubernetes.io/name":"prometheus"}},
       "spec":{"ports":[{"name":"http","port":9090,"targetPort":9090}]}}"#
        .into()
}

fn services_with_prometheus() -> String {
    format!(
        r#"{{"kind":"ServiceList","apiVersion":"v1","metadata":{{}},"items":[{}]}}"#,
        prometheus_service()
    )
}

fn empty_configmaps() -> &'static str {
    r#"{"kind":"ConfigMapList","apiVersion":"v1","metadata":{},"items":[]}"#
}

fn joinable_provisioned() -> &'static str {
    r#"{"kind":"ConfigMapList","apiVersion":"v1","metadata":{},"items":[
        {"metadata":{"name":"pods-dash","namespace":"monitoring",
                     "labels":{"grafana_dashboard":"1"}},
         "data":{"pods.json":"{\"uid\":\"pods\",\"title\":\"Pods\",\"panels\":[{\"id\":1,\"title\":\"CPU\",\"type\":\"timeseries\",\"datasource\":{\"type\":\"prometheus\",\"uid\":\"prom\"},\"targets\":[{\"refId\":\"A\",\"expr\":\"sum by (namespace, pod) (rate(node_cpu_seconds_total[5m]))\"}]}]}"}}
    ]}"#
}

fn script_overlay_base(script: &Script) {
    script_discovery(script);
    script_rules_review(script);
    script_access_reviews(script, true, 32);
    script_lists(script);
}

/// Routes the overlay bind issues after the watcher's initial list has already
/// consumed `script_lists`. Several copies so a background relist cannot steal
/// the only Service page before Prometheus is bound.
fn script_prometheus_bind(script: &Script) {
    for _ in 0..8 {
        script.route("GET", "/api/v1/services", 200, services_with_prometheus());
    }
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:http/proxy/-/ready",
        200,
        "Prometheus is Ready.\n",
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/-/ready",
        200,
        "Prometheus is Ready.\n",
    );
}

fn script_query_range(script: &Script, times: usize) {
    for _ in 0..times {
        script.route(
            "POST",
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range",
            200,
            EMPTY_MATRIX,
        );
        script.route(
            "POST",
            "/api/v1/namespaces/monitoring/services/prometheus:http/proxy/api/v1/query_range",
            200,
            EMPTY_MATRIX,
        );
    }
}

fn fetch_overlay(sync: &Sync, kind: overlay::Kind) -> Fetched<overlay::Frame> {
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_overlay(kind, ReachSettings::default(), move |fetched| {
            let _ = tx.send(fetched);
        });
    wait(&rx)
}

fn seen_paths(script: &Script) -> Vec<String> {
    script
        .seen()
        .into_iter()
        .map(|seen| format!("{} {}", seen.method, seen.path))
        .collect()
}

#[test]
fn mesh_observed_queries_istio_hubble_and_linkerd_as_their_own_promql() {
    let script = Script::default();
    script_overlay_base(&script);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script_prometheus_bind(&script);
    script_query_range(&script, 3);

    let fetched = fetch_overlay(&sync, overlay::Kind::MeshObserved);
    let Fetched::Ok(frame) = fetched else {
        panic!(
            "mesh observed must resolve: {fetched:?}\n{:?}",
            seen_paths(&script)
        );
    };
    assert!(
        frame
            .note
            .as_deref()
            .is_some_and(|note| note.contains("no Istio")),
        "{:?}\n{:?}",
        frame.note,
        seen_paths(&script)
    );

    let posts = script.requests_for("/proxy/api/v1/query_range");
    assert_eq!(
        posts.len(),
        3,
        "Istio, Hubble, and Linkerd are three queries: {:?}\n{:?}",
        posts,
        seen_paths(&script)
    );
    let bodies: Vec<&str> = posts.iter().map(|seen| seen.body.as_str()).collect();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("istio_requests_total")),
        "{bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("hubble_flows_processed_total")),
        "{bodies:?}"
    );
    assert!(
        bodies.iter().any(|body| body.contains("response_total")),
        "{bodies:?}"
    );
    assert!(
        bodies.iter().all(|body| !body.contains("container_cpu")),
        "cadvisor CPU is not a mesh observation: {bodies:?}"
    );
    assert!(
        script.seen().iter().all(|seen| !seen.path.contains("4245")
            && !seen.path.contains("hubble-relay")
            && !seen.path.contains("/api/v1/observe")),
        "Hubble itself is never scraped: {:?}",
        script.seen()
    );

    drop(runtime);
}

#[test]
fn metrics_overlay_runs_promql_grafana_already_wrote() {
    let script = Script::default();
    script_overlay_base(&script);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    for _ in 0..4 {
        script.route("GET", "/api/v1/configmaps", 200, joinable_provisioned());
    }
    script_prometheus_bind(&script);
    script_query_range(&script, 1);

    let fetched = fetch_overlay(&sync, overlay::Kind::Metrics);
    let Fetched::Ok(frame) = fetched else {
        panic!(
            "metrics overlay must resolve: {fetched:?}\n{:?}",
            seen_paths(&script)
        );
    };
    assert_eq!(
        frame.note.as_deref(),
        Some("PromQL named by Grafana"),
        "{:?}\n{:?}",
        frame.note,
        seen_paths(&script)
    );

    let posts = script.requests_for("/proxy/api/v1/query_range");
    assert_eq!(posts.len(), 1, "{posts:?}\n{:?}", seen_paths(&script));
    assert!(
        posts[0].body.contains("node_cpu_seconds_total"),
        "the provisioned dashboard's expr is the one sent: {}",
        posts[0].body
    );
    assert!(
        !posts[0].body.contains("container_cpu_usage_seconds_total"),
        "cadvisor CPU is the fallback, not the named expr: {}",
        posts[0].body
    );
    assert!(
        script.requests_for("/proxy/api/search").is_empty(),
        "ConfigMaps already named PromQL, so Grafana was not bound"
    );

    drop(runtime);
}

#[test]
fn metrics_overlay_keeps_cadvisor_cpu_when_grafana_names_nothing() {
    let script = Script::default();
    script_overlay_base(&script);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    for _ in 0..4 {
        script.route("GET", "/api/v1/configmaps", 200, empty_configmaps());
    }
    script_prometheus_bind(&script);
    script_query_range(&script, 1);

    let fetched = fetch_overlay(&sync, overlay::Kind::Metrics);
    let Fetched::Ok(frame) = fetched else {
        panic!(
            "metrics overlay must resolve: {fetched:?}\n{:?}",
            seen_paths(&script)
        );
    };
    assert_eq!(
        frame.note.as_deref(),
        Some("cadvisor CPU; Grafana has not named a PromQL"),
        "{:?}\n{:?}",
        frame.note,
        seen_paths(&script)
    );

    let posts = script.requests_for("/proxy/api/v1/query_range");
    assert_eq!(posts.len(), 1, "{posts:?}\n{:?}", seen_paths(&script));
    assert!(
        posts[0].body.contains("container_cpu_usage_seconds_total"),
        "CPU_EXPR is the fallback: {}",
        posts[0].body
    );

    drop(runtime);
}

/// The Policy overlay answers about PolicyReports and nothing else. It used to
/// fall through to NetworkPolicy isolation whenever the reports were unserved
/// or clean, so picking `POLICY` painted netpol tints under the policy legend.
/// Isolation has its own overlay now; an unserved report group is a note.
#[test]
fn policy_overlay_never_answers_with_network_policy_isolation() {
    let script = Script::default();
    script_overlay_base(&script);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let fetched = fetch_overlay(&sync, overlay::Kind::Policy);
    let Fetched::Ok(frame) = fetched else {
        panic!(
            "an unserved report group is Ok with a note: {fetched:?}\n{:?}",
            seen_paths(&script)
        );
    };
    assert!(
        frame.stamps.is_empty(),
        "no reports are served, so nothing may be stamped: {:?}",
        frame.stamps
    );
    assert_eq!(
        frame.note.as_deref(),
        Some("PolicyReport CRDs are not served by this cluster"),
        "{:?}\n{:?}",
        frame.note,
        seen_paths(&script)
    );
    assert!(
        script.requests_for("networkpolicies").is_empty(),
        "the Policy overlay must not read NetworkPolicy: {:?}",
        seen_paths(&script)
    );

    drop(runtime);
}
