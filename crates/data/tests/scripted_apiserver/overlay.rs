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
            "GET",
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
            200,
            EMPTY_MATRIX,
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/prometheus:http/proxy/api/v1/query_range?",
            200,
            EMPTY_MATRIX,
        );
    }
}

fn fetch_overlay(sync: &Sync, kind: overlay::Kind) -> Fetched<overlay::Frame> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync
        .reader
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
            .is_some_and(|note| note.contains("no joinable mesh workload links")),
        "{:?}\n{:?}",
        frame.note,
        seen_paths(&script)
    );

    let queries = script.requests_for("/proxy/api/v1/query_range");
    assert_eq!(
        queries.len(),
        3,
        "Istio, Hubble, and Linkerd are three queries: {:?}\n{:?}",
        queries,
        seen_paths(&script)
    );
    let params: Vec<&str> = queries.iter().map(|seen| seen.path.as_str()).collect();
    assert!(
        params
            .iter()
            .any(|body| body.contains("istio_requests_total")),
        "{params:?}"
    );
    assert!(
        params
            .iter()
            .any(|body| body.contains("hubble_flows_processed_total")),
        "{params:?}"
    );
    assert!(
        params.iter().any(|body| body.contains("response_total")),
        "{params:?}"
    );
    assert!(
        params.iter().all(|body| !body.contains("container_cpu")),
        "cadvisor CPU is not a mesh observation: {params:?}"
    );
    assert!(
        script.seen().iter().all(|seen| !seen.path.contains("4245")
            && !seen.path.contains("hubble-relay")
            && !seen.path.contains("/api/v1/observe")),
        "Hubble itself is never scraped: {:?}",
        script.seen()
    );
    assert_mesh_query_requests(&script);

    drop(runtime);
}

const MESH_ENCODED_QUERIES: [&str; 3] = [
    "sum+by+%28source_workload%2C+destination_workload%2C+destination_service%2C+namespace%29+%28rate%28istio_requests_total%5B5m%5D%29%29",
    "sum+by+%28source%2C+destination%29+%28rate%28hubble_flows_processed_total%5B5m%5D%29%29",
    "sum+by+%28client%2C+dst%2C+authority%29+%28rate%28response_total%5B5m%5D%29%29",
];
const MESH_QUERY_SOURCES: [&str; 3] = ["Istio query", "Hubble query", "Linkerd query"];
const MESH_QUERY_ROUTE: &str =
    "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?query=";

fn script_mesh_query(script: &Script, source: usize, status: u16, body: &str) {
    script.route(
        "GET",
        &format!("{MESH_QUERY_ROUTE}{}&start=", MESH_ENCODED_QUERIES[source]),
        status,
        body,
    );
}

fn assert_mesh_query_requests(script: &Script) {
    let requests = script.requests_for("/proxy/api/v1/query_range?");
    assert_eq!(requests.len(), 3, "{requests:?}");
    for query in MESH_ENCODED_QUERIES {
        let prefix = format!("{MESH_QUERY_ROUTE}{query}&start=");
        let matching: Vec<_> = requests
            .iter()
            .filter(|request| request.path.starts_with(&prefix))
            .collect();
        assert_eq!(matching.len(), 1, "{query}: {requests:?}");
        let request = matching[0];
        assert_eq!(request.method, "GET");
        assert_eq!(request.accept, "application/json");
        assert_eq!(request.content_type, "");
        assert!(request.body_collected);
        assert!(request.body.is_empty());
        let (start, rest) = request.path[prefix.len()..]
            .split_once("&end=")
            .expect("start and end are separate query parameters");
        let (end, rest) = rest
            .split_once("&step=")
            .expect("the range has an explicit resolution");
        let start: f64 = start.parse().expect("finite start timestamp");
        let end: f64 = end.parse().expect("finite end timestamp");
        assert!(start.is_finite() && end.is_finite());
        assert_eq!(end - start, overlay::RANGE_SECS);
        assert_eq!(rest, "30s&limit=256");
    }
}

fn observed_mesh_matrix(source: usize) -> String {
    let (from, to) = match source {
        0 => ("source_workload", "destination_workload"),
        1 => ("source", "destination"),
        2 => ("client", "dst"),
        _ => unreachable!("there are three mesh queries"),
    };
    serde_json::json!({
        "status": "success",
        "data": {
            "resultType": "matrix",
            "result": [{
                "metric": {(from): "prod/frontend", (to): "prod/backend"},
                "values": [[1, "2"]],
            }],
        },
    })
    .to_string()
}

fn assert_observed_mesh_stamps(frame: &overlay::Frame) {
    let names: Vec<_> = frame
        .stamps
        .iter()
        .map(|stamp| (stamp.namespace.as_str(), stamp.name.as_str()))
        .collect();
    assert_eq!(names, [("prod", "backend"), ("prod", "frontend")]);
    assert!(!frame.truncated);
}

#[test]
fn mesh_observed_keeps_empty_success_beside_named_denials_and_failures() {
    for success in 0..3 {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script_prometheus_bind(&script);
        let denied = (success + 1) % 3;
        let mut expected = Vec::new();
        for (source, name) in MESH_QUERY_SOURCES.iter().enumerate() {
            if source == success {
                script_mesh_query(&script, source, 200, EMPTY_MATRIX);
            } else if source == denied {
                script_mesh_query(&script, source, 403, r#"{"message":"query is forbidden"}"#);
                expected.push(format!(
                    "{name}: prometheus: access denied for this account"
                ));
            } else {
                script_mesh_query(
                    &script,
                    source,
                    500,
                    r#"{"message":"mesh storage unavailable"}"#,
                );
                expected.push(format!("{name}: prometheus: mesh storage unavailable"));
            }
        }
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
            panic!("permitted empty data must retain the other query outcomes");
        };
        assert!(frame.stamps.is_empty());
        assert!(!frame.truncated);
        assert_eq!(
            frame.note,
            Some(format!(
                "Mesh observations are incomplete; {}",
                expected.join("; ")
            ))
        );
        assert_mesh_query_requests(&script);
    }
}

#[test]
fn mesh_observed_keeps_permitted_stamps_when_other_queries_fail() {
    for success in 0..3 {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script_prometheus_bind(&script);
        let mut expected = Vec::new();
        for (source, name) in MESH_QUERY_SOURCES.iter().enumerate() {
            if source == success {
                script_mesh_query(&script, source, 200, &observed_mesh_matrix(source));
            } else if source == (success + 1) % 3 {
                script_mesh_query(&script, source, 403, r#"{"message":"query is forbidden"}"#);
                expected.push(format!(
                    "{name}: prometheus: access denied for this account"
                ));
            } else {
                script_mesh_query(
                    &script,
                    source,
                    503,
                    r#"{"message":"mesh backend is restarting"}"#,
                );
                expected.push(format!("{name}: prometheus: mesh backend is restarting"));
            }
        }
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
            panic!("permitted observations must survive another query's failure");
        };
        assert_observed_mesh_stamps(&frame);
        assert_eq!(
            frame.note,
            Some(format!(
                "Mesh observations are incomplete; {}",
                expected.join("; ")
            ))
        );
        assert_mesh_query_requests(&script);
    }
}

#[test]
fn mesh_observed_keeps_every_named_error_when_no_query_succeeds() {
    let sentence = format!(
        "mesh storage rejected the request: {} complete",
        "details é ".repeat(80)
    );
    for statuses in [[403, 500, 503], [500, 503, 500], [403, 403, 403]] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script_prometheus_bind(&script);
        let mut expected = Vec::new();
        for (source, status) in statuses.into_iter().enumerate() {
            let why = format!("{}: {sentence}", MESH_QUERY_SOURCES[source]);
            script_mesh_query(
                &script,
                source,
                status,
                &serde_json::json!({"message": &why}).to_string(),
            );
            expected.push(if status == 403 {
                format!(
                    "{}: prometheus: access denied for this account",
                    MESH_QUERY_SOURCES[source]
                )
            } else {
                format!("{}: prometheus: {why}", MESH_QUERY_SOURCES[source])
            });
        }
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
            panic!("all query failures must be retained together");
        };
        assert!(frame.stamps.is_empty());
        assert!(!frame.truncated);
        assert_eq!(
            frame.note,
            Some(format!(
                "Mesh observations are unavailable; {}",
                expected.join("; ")
            ))
        );
        assert_mesh_query_requests(&script);
    }
}

#[test]
fn mesh_observed_complete_success_keeps_its_existing_stamps_and_note() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script_prometheus_bind(&script);
    for source in 0..3 {
        script_mesh_query(&script, source, 200, &observed_mesh_matrix(source));
    }
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
        panic!("three successful queries must retain their original output");
    };
    assert_observed_mesh_stamps(&frame);
    assert_eq!(frame.note, None);
    assert_mesh_query_requests(&script);
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

    let queries = script.requests_for("/proxy/api/v1/query_range");
    assert_eq!(queries.len(), 1, "{queries:?}\n{:?}", seen_paths(&script));
    assert!(
        queries[0].path.contains("node_cpu_seconds_total"),
        "the provisioned dashboard's expr is the one sent: {}",
        queries[0].path
    );
    assert!(
        !queries[0]
            .path
            .contains("container_cpu_usage_seconds_total"),
        "cadvisor CPU is the fallback, not the named expr: {}",
        queries[0].path
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

    let queries = script.requests_for("/proxy/api/v1/query_range");
    assert_eq!(queries.len(), 1, "{queries:?}\n{:?}", seen_paths(&script));
    assert!(
        queries[0]
            .path
            .contains("container_cpu_usage_seconds_total"),
        "CPU_EXPR is the fallback: {}",
        queries[0].path
    );

    assert!(script.requests_for("/proxy/api/search").is_empty());
    assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
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

const PROVISIONING_LIST: &str =
    "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200";
const PROVISIONING_ROUTE: &str =
    "/api/v1/configmaps?labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200";

fn assert_metric_requests(script: &Script, provisioning: usize, named: bool) {
    let lists = script.requests_for(PROVISIONING_LIST);
    assert_eq!(lists.len(), provisioning);
    for (index, request) in lists.into_iter().enumerate() {
        assert_eq!(request.method, "GET");
        if index == 0 {
            assert_eq!(request.path, PROVISIONING_LIST);
        } else {
            assert_eq!(request.path, format!("{PROVISIONING_LIST}&continue=next"));
        }
        assert_eq!(request.accept, "");
        assert_eq!(request.content_type, "");
        assert!(request.body.is_empty());
    }
    let queries = script.requests_for("/proxy/api/v1/query_range?");
    assert_eq!(queries.len(), 1);
    let request = &queries[0];
    assert_eq!(request.method, "GET");
    assert!(request.path.starts_with(
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?"
    ));
    assert_eq!(request.accept, "application/json");
    assert_eq!(request.content_type, "");
    assert!(request.body.is_empty());
    assert!(request.path.contains("limit=256"));
    assert!(request.path.contains("step=30s"));
    assert!(request.path.contains(if named {
        "node_cpu_seconds_total"
    } else {
        "container_cpu_usage_seconds_total"
    }));
}

fn wait_overlay_condition(runtime: &tokio::runtime::Runtime, condition: impl Fn() -> bool) {
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("overlay request or scheduler state within the budget");
    });
}

async fn await_overlay_condition(description: &str, condition: impl Fn() -> bool) {
    for _ in 0..1_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("overlay did not reach {description}");
}

#[tokio::test(start_paused = true)]
async fn stalled_overlays_expire_after_queue_and_work_and_release_capacity() {
    const POLICY_GROUP_PATH: &str = "/apis/wgpolicyk8s.io";

    let sync_script = Script::default();
    script_overlay_base(&sync_script);
    let interactive = Script::default();
    for _ in 0..3 {
        interactive.route_hanging("GET", POLICY_GROUP_PATH);
    }
    let (events, _events_rx) = crossbeam_channel::unbounded();
    let sync = sync_from(
        Clients {
            sync: sync_script.client(),
            interactive: interactive.client(),
        },
        "prod",
        &options(),
        events,
        Arc::new(IngestMetrics::default()),
    )
    .await
    .expect("the scripted overlay cluster");

    let submit = || {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let stop = sync.reader.fetch_overlay(
            overlay::Kind::Policy,
            ReachSettings::default(),
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        (stop, rx)
    };
    let deadline = k10s_data::connect::INTERACTIVE_DEADLINE;
    assert_eq!(deadline, Duration::from_secs(35));
    let started = tokio::time::Instant::now();
    let (_first_stop, mut first) = submit();
    let (_second_stop, mut second) = submit();
    await_overlay_condition("two active policy group reads", || {
        let requests = interactive.requests_for(POLICY_GROUP_PATH);
        requests.len() == 2
            && requests.iter().all(|request| request.body_collected)
            && sync.reader.observe_schedule_counts().active == 2
    })
    .await;
    assert_eq!(sync.reader.observe_schedule_counts().admitted, 2);

    let queued_at = Duration::from_secs(10);
    tokio::time::advance(queued_at).await;
    let (_queued_stop, mut queued) = submit();
    await_overlay_condition("the third read queued behind two active reads", || {
        let counts = sync.reader.observe_schedule_counts();
        counts.admitted == 3 && counts.active == 2
    })
    .await;
    assert_eq!(interactive.requests_for(POLICY_GROUP_PATH).len(), 2);

    tokio::time::advance(deadline - queued_at - Duration::from_secs(1)).await;
    for reply in [&mut first, &mut second, &mut queued] {
        assert!(matches!(
            reply.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    let expired = || Fetched::Failed {
        what: "telemetry",
        why: "the request did not finish before its deadline".to_string(),
    };
    assert_eq!(first.await.expect("first deadline reply"), expired());
    assert_eq!(second.await.expect("second deadline reply"), expired());
    assert_eq!(started.elapsed(), deadline);
    await_overlay_condition(
        "the queued read active after the first two deadlines",
        || {
            let counts = sync.reader.observe_schedule_counts();
            let requests = interactive.requests_for(POLICY_GROUP_PATH);
            requests.len() == 3
                && requests.iter().all(|request| request.body_collected)
                && counts.admitted == 1
                && counts.active == 1
        },
    )
    .await;

    tokio::time::advance(queued_at - Duration::from_secs(1)).await;
    assert!(matches!(
        queued.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(queued.await.expect("queued deadline reply"), expired());
    assert_eq!(started.elapsed(), deadline + queued_at);
    await_overlay_condition("all timed-out capacity released", || {
        let counts = sync.reader.observe_schedule_counts();
        counts.admitted == 0 && counts.active == 0
    })
    .await;

    assert!(sync_script.requests_for(POLICY_GROUP_PATH).is_empty());
    let requests = interactive.requests_for(POLICY_GROUP_PATH);
    assert_eq!(requests.len(), 3);
    for request in requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, POLICY_GROUP_PATH);
        assert_eq!(request.accept, "");
        assert_eq!(request.content_type, "");
        assert!(request.body_collected);
        assert!(request.body.is_empty());
    }
    assert!(interactive.requests_for("/apis/openreports.io").is_empty());
    assert!(interactive.requests_for("/proxy/").is_empty());
}

#[test]
fn overlay_requests_share_bounded_admission_and_cancel_before_later_reads() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    for _ in 0..2 {
        script.route_hanging("GET", PROVISIONING_ROUTE);
    }
    script_lists(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let mut stops = Vec::new();
    let mut replies = Vec::new();
    for _ in 0..6 {
        let (tx, rx) = std::sync::mpsc::channel();
        stops.push(sync.reader.fetch_overlay(
            overlay::Kind::Metrics,
            ReachSettings::default(),
            move |answer| {
                let _ = tx.send(answer);
            },
        ));
        replies.push(rx);
        if stops.len() == 2 {
            wait_overlay_condition(&runtime, || {
                script.requests_for(PROVISIONING_LIST).len() == 2
            });
        }
    }
    wait_overlay_condition(&runtime, || {
        sync.reader.observe_schedule_counts().admitted == 6
            && script.requests_for(PROVISIONING_LIST).len() == 2
    });
    assert_eq!(sync.reader.observe_schedule_counts().active, 2);

    let (tx, rx) = std::sync::mpsc::channel();
    let rejected = sync.reader.fetch_overlay(
        overlay::Kind::Metrics,
        ReachSettings::default(),
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    assert_eq!(
        wait(&rx),
        Fetched::Failed {
            what: "telemetry",
            why: "telemetry is busy: request capacity is full; try again shortly".to_string(),
        }
    );
    drop(rejected);

    drop(stops.split_off(2));
    wait_overlay_condition(&runtime, || {
        sync.reader.observe_schedule_counts().admitted == 2
    });
    for reply in &replies[2..] {
        assert!(matches!(
            reply.recv_timeout(Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
    drop(stops);
    wait_overlay_condition(&runtime, || {
        sync.reader.observe_schedule_counts().admitted == 0
    });
    for reply in &replies[..2] {
        assert!(matches!(
            reply.recv_timeout(Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
    let counts = sync.reader.observe_schedule_counts();
    assert_eq!(counts.active, 0);
    assert_eq!(counts.peak_active, 2);
    assert_eq!(counts.peak_admitted, 6);
    assert_eq!(counts.rejected, 1);
    let requests = script.requests_for(PROVISIONING_LIST);
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, PROVISIONING_LIST);
        assert_eq!(request.accept, "");
        assert_eq!(request.content_type, "");
        assert!(request.body.is_empty());
    }
    assert!(script.requests_for("/proxy/").is_empty());
}

#[test]
fn cancelling_an_overlay_during_the_prometheus_query_releases_its_slot() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script.route("GET", PROVISIONING_ROUTE, 200, joinable_provisioned());
    script.route_hanging(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
    );
    script_lists(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script_prometheus_bind(&script);
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.fetch_overlay(
        overlay::Kind::Metrics,
        ReachSettings::default(),
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    wait_overlay_condition(&runtime, || {
        !script.requests_for("/proxy/api/v1/query_range?").is_empty()
    });
    assert_eq!(sync.reader.observe_schedule_counts().active, 1);
    drop(stop);
    wait_overlay_condition(&runtime, || {
        sync.reader.observe_schedule_counts().admitted == 0
    });
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
    assert_eq!(sync.reader.observe_schedule_counts().active, 0);
    assert_metric_requests(&script, 1, true);
}

fn metric_matrix(series: usize) -> String {
    serde_json::json!({"status":"success","data":{"resultType":"matrix",
        "result":(0..series).map(|index|serde_json::json!({
            "metric":{"namespace":"prod","pod":format!("pod-{index}")},
            "values":[[1,"2"]]
        })).collect::<Vec<_>>()
    }})
    .to_string()
}

#[test]
fn metrics_keep_named_data_and_report_denied_dashboard_continuations_separately() {
    for data_truncated in [false, true] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        let mut first: serde_json::Value =
            serde_json::from_str(joinable_provisioned()).expect("provisioned fixture");
        first["metadata"]["continue"] = "next".into();
        script.route("GET", PROVISIONING_ROUTE, 200, first.to_string());
        script.route(
            "GET",
            &format!("{PROVISIONING_ROUTE}&continue=next"),
            403,
            r#"{"message":"the next ConfigMap page is forbidden"}"#,
        );
        script_prometheus_bind(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
            200,
            metric_matrix(if data_truncated { 257 } else { 1 }),
        );
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
            panic!("readable metrics survive an incomplete dashboard selection");
        };
        assert_eq!(frame.stamps.len(), if data_truncated { 256 } else { 1 });
        assert_eq!(frame.stamps[0].namespace, "prod");
        assert_eq!(frame.stamps[0].name, "pod-0");
        assert_eq!(frame.stamps[0].samples, [(1000, 2.0)]);
        assert_eq!(frame.truncated, data_truncated);
        let note = frame.note.expect("source and partial selection notices");
        assert!(note.starts_with(if data_truncated {
            "Prometheus query: 1 returned series omitted locally; overlay data is incomplete"
        } else {
            "PromQL named by Grafana"
        }));
        assert!(note.contains("Provisioned ConfigMap dashboard selection is incomplete"));
        assert!(
            note.contains("provisioned dashboards: configmaps: access denied for this account")
        );
        assert_metric_requests(&script, 2, true);
        assert!(script.requests_for("/proxy/api/search").is_empty());
    }
}

#[test]
fn metrics_fallback_keeps_provisioning_denial_beside_each_prometheus_outcome() {
    for status in [200, 403, 500] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script.route(
            "GET",
            PROVISIONING_ROUTE,
            403,
            r#"{"message":"provisioning is forbidden"}"#,
        );
        script_prometheus_bind(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
            status,
            if status == 200 {
                metric_matrix(1)
            } else {
                r#"{"message":"Prometheus storage unavailable"}"#.to_string()
            },
        );
        let note = match fetch_overlay(&sync, overlay::Kind::Metrics) {
            Fetched::Ok(frame) if status != 403 => {
                assert_eq!(frame.stamps.len(), usize::from(status == 200));
                assert!(!frame.truncated);
                let note = frame.note.expect("both source outcomes are named");
                assert!(note.starts_with(if status == 200 {
                    "cadvisor CPU; Grafana has not named a PromQL"
                } else {
                    "Prometheus storage unavailable"
                }));
                note
            }
            Fetched::Failed { what, why } if status == 403 => {
                assert_eq!(what, "prometheus");
                assert!(why.starts_with("prometheus: access denied for this account"));
                why
            }
            other => panic!("the metrics result and source diagnostic must survive: {other:?}"),
        };
        assert!(
            note.contains("provisioned dashboards: configmaps: access denied for this account")
        );
        assert_metric_requests(&script, 1, false);
    }
}

#[test]
fn an_ordinary_metrics_denial_keeps_its_existing_denied_outcome() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script.route("GET", PROVISIONING_ROUTE, 200, empty_configmaps());
    script_prometheus_bind(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        403,
        r#"{"message":"Prometheus query is forbidden"}"#,
    );
    assert!(matches!(
        fetch_overlay(&sync, overlay::Kind::Metrics),
        Fetched::Denied { what: "prometheus" }
    ));
    assert_metric_requests(&script, 1, false);
}

#[test]
fn metric_source_diagnostics_keep_every_bounded_input_error() {
    for unicode in [false, true] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        let broken: serde_json::Map<String, serde_json::Value> = (0..10)
            .map(|index| (format!("{}-{index}.json", "k".repeat(245)), "{".into()))
            .collect();
        script.route(
            "GET",
            PROVISIONING_ROUTE,
            200,
            serde_json::json!({"apiVersion":"v1","kind":"ConfigMapList","items":[{
                "metadata":{
                    "name":if unicode {"界".repeat(500)} else {"x".repeat(253)},
                    "namespace":"n".repeat(63)
                },
                "data":broken
            }]})
            .to_string(),
        );
        script_prometheus_bind(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
            200,
            metric_matrix(1),
        );
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
            panic!("a bounded diagnostic accompanies the fallback samples");
        };
        assert_eq!(frame.stamps.len(), 1);
        assert!(!frame.truncated);
        let note = frame.note.expect("source diagnostics");
        assert!(note.starts_with("cadvisor CPU; Grafana has not named a PromQL"));
        let details: Vec<_> = note
            .split("; ")
            .filter(|part| part.starts_with("provisioned dashboards: "))
            .collect();
        assert_eq!(details.len(), 10);
        for detail in details {
            assert!(detail.starts_with("provisioned dashboards: ConfigMap "));
            assert!(detail.contains("dashboard JSON did not parse"));
        }
        assert!(!note.contains("source notices omitted"));
        assert!(note.len() < 15_000);
        assert_metric_requests(&script, 1, false);
    }
}

fn script_metric_sources_with_grafana(script: &Script) {
    let prometheus: serde_json::Value =
        serde_json::from_str(&prometheus_service()).expect("Prometheus fixture");
    let services = serde_json::json!({"apiVersion":"v1","kind":"ServiceList","items":[
        prometheus,
        {"metadata":{"name":"grafana","namespace":"monitoring"},
         "spec":{"ports":[{"name":"http-web","port":3000}]}}
    ]})
    .to_string();
    for _ in 0..8 {
        script.route("GET", "/api/v1/services", 200, services.clone());
    }
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:http/proxy/-/ready",
        200,
        "ready",
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/-/ready",
        200,
        "ready",
    );
}

fn assert_grafana_request(script: &Script, rest: &str, health: bool) {
    let path = format!(
        "/api/v1/namespaces/monitoring/services/grafana:{}/proxy/{rest}",
        if health { "http-web" } else { "3000" }
    );
    let requests: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|request| request.path == path)
        .collect();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, path);
    assert_eq!(request.accept, "application/json");
    assert_eq!(request.content_type, "");
    assert!(request.body_collected);
    assert!(request.body.is_empty());
}

#[test]
fn a_denied_grafana_health_proxy_is_named_without_a_search() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script.route("GET", PROVISIONING_ROUTE, 200, empty_configmaps());
    script_metric_sources_with_grafana(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
        403,
        r#"{"message":"Grafana proxy is forbidden"}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        metric_matrix(1),
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("a denied Grafana bind must not remove permitted Prometheus data");
    };
    assert_eq!(frame.stamps.len(), 1);
    assert!(!frame.truncated);
    assert_eq!(
        frame.note.as_deref(),
        Some(
            "cadvisor CPU; Grafana has not named a PromQL; Grafana binding: Grafana proxy: grafana access denied for this account"
        )
    );
    assert_metric_requests(&script, 1, false);
    assert_grafana_request(&script, "api/health", true);
    assert!(script.requests_for("/proxy/api/search").is_empty());
    assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
}

#[test]
fn grafana_search_denial_and_failure_remain_visible_beside_fallback_samples() {
    for status in [403, 500] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script.route("GET", PROVISIONING_ROUTE, 200, empty_configmaps());
        script_metric_sources_with_grafana(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
            200,
            "ready",
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?",
            status,
            r#"{"message":"dashboard search storage unavailable"}"#,
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
            200,
            metric_matrix(1),
        );
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
            panic!("the independent Prometheus fallback remains readable");
        };
        assert_eq!(frame.stamps.len(), 1);
        assert!(!frame.truncated);
        assert_eq!(
            frame.note.as_deref(),
            Some(if status == 403 {
                "cadvisor CPU; Grafana has not named a PromQL; Grafana search: grafana: access denied for this account"
            } else {
                "cadvisor CPU; Grafana has not named a PromQL; Grafana search: dashboard search storage unavailable"
            })
        );
        assert_metric_requests(&script, 1, false);
        assert_grafana_request(&script, "api/health", true);
        assert_grafana_request(&script, "api/search?type=dash-db&limit=100", false);
        assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
    }
}

#[test]
fn long_search_failure_survives_earlier_provisioning_errors() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let broken: serde_json::Map<String, serde_json::Value> = (0..10)
        .map(|index| (format!("broken-{index}.json"), "{".into()))
        .collect();
    script.route(
        "GET",
        PROVISIONING_ROUTE,
        200,
        serde_json::json!({"apiVersion":"v1","kind":"ConfigMapList","items":[{
            "metadata":{"name":"dashboards","namespace":"monitoring"},"data":broken
        }]})
        .to_string(),
    );
    script_metric_sources_with_grafana(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
        200,
        "ready",
    );
    let sentence = format!("dashboard storage unavailable: {} end", "界".repeat(1_000));
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?",
        500,
        serde_json::json!({"message":&sentence}).to_string(),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        metric_matrix(1),
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("permitted Prometheus data must retain both source failures");
    };
    assert_eq!(frame.stamps.len(), 1);
    let note = frame.note.expect("source diagnostics");
    for index in 0..10 {
        assert!(note.contains(&format!("broken-{index}.json")));
    }
    assert!(note.contains(&format!("Grafana search: {sentence}")));
    assert_metric_requests(&script, 1, false);
    assert_grafana_request(&script, "api/health", true);
    assert_grafana_request(&script, "api/search?type=dash-db&limit=100", false);
    assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
}

#[test]
fn an_api_named_overlay_keeps_earlier_dashboard_denial_and_failure() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script.route("GET", PROVISIONING_ROUTE, 200, empty_configmaps());
    script_metric_sources_with_grafana(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
        200,
        "ready",
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?",
        200,
        r#"[{"uid":"denied","type":"dash-db"},{"uid":"failed","type":"dash-db"},{"uid":"joinable","type":"dash-db"},{"uid":"unneeded","type":"dash-db"}]"#,
    );
    for (uid, status, message) in [
        ("denied", 403, "dashboard is forbidden"),
        ("failed", 500, "dashboard storage unavailable"),
    ] {
        script.route(
            "GET",
            &format!(
                "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/{uid}"
            ),
            status,
            serde_json::json!({"message":message}).to_string(),
        );
    }
    let provisioned: serde_json::Value =
        serde_json::from_str(joinable_provisioned()).expect("provisioned fixture");
    let mut dashboard: serde_json::Value = serde_json::from_str(
        provisioned["items"][0]["data"]["pods.json"]
            .as_str()
            .expect("dashboard JSON"),
    )
    .expect("joinable dashboard");
    dashboard["uid"] = "joinable".into();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/joinable",
        200,
        dashboard.to_string(),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        metric_matrix(1),
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("the first readable joinable API dashboard names the live query");
    };
    assert_eq!(frame.stamps.len(), 1);
    assert!(!frame.truncated);
    assert_eq!(
        frame.note.as_deref(),
        Some(
            "PromQL named by Grafana; Grafana dashboard denied: grafana: access denied for this account; Grafana dashboard failed: dashboard storage unavailable"
        )
    );
    assert_metric_requests(&script, 1, true);
    assert_grafana_request(&script, "api/health", true);
    assert_grafana_request(&script, "api/search?type=dash-db&limit=100", false);
    for uid in ["denied", "failed", "joinable"] {
        assert_grafana_request(&script, &format!("api/dashboards/uid/{uid}"), false);
    }
    assert_eq!(script.requests_for("/proxy/api/dashboards/uid/").len(), 3);
    assert!(
        script
            .requests_for("/proxy/api/dashboards/uid/unneeded")
            .is_empty()
    );
}

#[test]
fn an_early_named_query_does_not_claim_a_wide_search_is_incomplete() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script.route("GET", PROVISIONING_ROUTE, 200, empty_configmaps());
    script_metric_sources_with_grafana(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
        200,
        "ready",
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?",
        200,
        serde_json::json!(
            (0..100)
                .map(|index| serde_json::json!({
                    "uid":format!("item-{index}"),"type":"dash-db"
                }))
                .collect::<Vec<_>>()
        )
        .to_string(),
    );
    let provisioned: serde_json::Value =
        serde_json::from_str(joinable_provisioned()).expect("provisioned fixture");
    let mut dashboard: serde_json::Value = serde_json::from_str(
        provisioned["items"][0]["data"]["pods.json"]
            .as_str()
            .expect("dashboard JSON"),
    )
    .expect("joinable dashboard");
    dashboard["uid"] = "item-0".into();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/item-0",
        200,
        dashboard.to_string(),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        metric_matrix(1),
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("the first dashboard completes the named-query selection");
    };
    assert_eq!(frame.stamps.len(), 1);
    assert!(!frame.truncated);
    assert_eq!(frame.note.as_deref(), Some("PromQL named by Grafana"));
    assert_metric_requests(&script, 1, true);
    assert_grafana_request(&script, "api/health", true);
    assert_grafana_request(&script, "api/search?type=dash-db&limit=100", false);
    assert_grafana_request(&script, "api/dashboards/uid/item-0", false);
    assert_eq!(script.requests_for("/proxy/api/dashboards/uid/").len(), 1);
    assert!(
        script
            .requests_for("/proxy/api/dashboards/uid/item-1")
            .is_empty()
    );
}

#[test]
fn a_full_search_page_is_uncertain_after_invalid_uids_are_filtered() {
    for count in [99, 100] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script.route("GET", PROVISIONING_ROUTE, 200, empty_configmaps());
        script_metric_sources_with_grafana(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
            200,
            "ready",
        );
        let hits: Vec<_> = (0..count)
            .map(|index| serde_json::json!({"uid":format!("bad/{index}"),"type":"dash-db"}))
            .collect();
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?",
            200,
            serde_json::to_string(&hits).expect("search fixture"),
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
            200,
            metric_matrix(1),
        );
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
            panic!("the fallback remains available with a labelled source diagnostic");
        };
        assert_eq!(frame.stamps.len(), 1);
        assert!(!frame.truncated);
        let note = frame.note.expect("the invalid UIDs are reported");
        assert!(note.contains(
            "unsupported Grafana dashboard UID; expected 1 to 40 ASCII letters, digits, '-' or '_'"
        ));
        assert_eq!(
            note.contains("Grafana search returned its result limit; more dashboards may exist"),
            count == 100
        );
        assert_metric_requests(&script, 1, false);
        assert_grafana_request(&script, "api/health", true);
        assert_grafana_request(&script, "api/search?type=dash-db&limit=100", false);
        assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
    }
}

#[test]
fn a_full_search_page_notice_keeps_earlier_provisioning_errors() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let broken: serde_json::Map<String, serde_json::Value> = (0..10)
        .map(|index| (format!("broken-{index}.json"), "{".into()))
        .collect();
    script.route(
        "GET",
        PROVISIONING_ROUTE,
        200,
        serde_json::json!({"apiVersion":"v1","kind":"ConfigMapList","items":[{
            "metadata":{"name":"dashboards","namespace":"monitoring"},"data":broken
        }]})
        .to_string(),
    );
    script_metric_sources_with_grafana(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
        200,
        "ready",
    );
    let hits: Vec<_> = (0..100)
        .map(|index| serde_json::json!({"uid":format!("bad/{index}"),"type":"dash-db"}))
        .collect();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?",
        200,
        serde_json::to_string(&hits).expect("search fixture"),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        metric_matrix(1),
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("the fallback remains available with bounded diagnostics");
    };
    let note = frame.note.expect("source diagnostics");
    assert!(note.contains("Grafana search returned its result limit; more dashboards may exist"));
    for index in 0..10 {
        assert!(note.contains(&format!("broken-{index}.json")));
    }
    assert!(!note.contains("source notices omitted"));
    assert_metric_requests(&script, 1, false);
    assert_grafana_request(&script, "api/health", true);
    assert_grafana_request(&script, "api/search?type=dash-db&limit=100", false);
    assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
}

#[test]
fn an_unjoinable_api_search_keeps_the_fetch_bound_and_all_denials() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script.route("GET", PROVISIONING_ROUTE, 200, empty_configmaps());
    script_metric_sources_with_grafana(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
        200,
        "ready",
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?",
        200,
        serde_json::json!(
            (0..100)
                .map(|index| serde_json::json!({
                    "uid":format!("item-{index}"),"type":"dash-db"
                }))
                .collect::<Vec<_>>()
        )
        .to_string(),
    );
    for index in 0..16 {
        script.route(
            "GET",
            &format!("/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/item-{index}"),
            403,
            r#"{"message":"dashboard is forbidden"}"#,
        );
    }
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        metric_matrix(1),
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("the bounded selection falls back with its source diagnostics");
    };
    assert_eq!(frame.stamps.len(), 1);
    assert!(!frame.truncated);
    let note = frame.note.expect("bounded source notices");
    assert!(note.starts_with("cadvisor CPU; Grafana has not named a PromQL"));
    let full_page = note
        .find("Grafana search returned its result limit; more dashboards may exist")
        .expect("the search page limit is visible");
    let selection = note
        .find("Grafana dashboard search-hit selection is incomplete")
        .expect("the dashboard fetch limit is visible");
    let first_denial = note
        .find("Grafana dashboard item-0: grafana: access denied for this account")
        .expect("the first denial is visible");
    assert!(full_page < selection && selection < first_denial);
    for index in 0..16 {
        assert!(note.contains(&format!(
            "Grafana dashboard item-{index}: grafana: access denied for this account"
        )));
    }
    assert!(!note.contains("source notices omitted"));
    assert!(note.len() < 4400);
    assert_metric_requests(&script, 1, false);
    assert_grafana_request(&script, "api/health", true);
    assert_grafana_request(&script, "api/search?type=dash-db&limit=100", false);
    for index in 0..16 {
        assert_grafana_request(&script, &format!("api/dashboards/uid/item-{index}"), false);
    }
    assert_eq!(script.requests_for("/proxy/api/dashboards/uid/").len(), 16);
}

#[test]
fn invalid_search_uids_keep_named_metrics_and_a_fixed_notice() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script.route("GET", PROVISIONING_ROUTE, 200, empty_configmaps());
    script_metric_sources_with_grafana(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http-web/proxy/api/health",
        200,
        "ready",
    );
    let mut hits: Vec<_> = (0..16)
        .map(|index| serde_json::json!({"uid":format!("../bad-{index}"),"type":"dash-db"}))
        .collect();
    for uid in [
        "cluster/pods".to_string(),
        "cluster?orgId=2".to_string(),
        "cluster#ignored".to_string(),
        "%2e%2e%2fapi%2fsearch".to_string(),
        "cluster\n\u{202e}pods".to_string(),
        "a".repeat(8_192),
        String::new(),
    ] {
        hits.push(serde_json::json!({"uid":uid,"type":"dash-db"}));
    }
    hits.push(serde_json::json!({"uid":"joinable","type":"dash-db"}));
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?",
        200,
        serde_json::Value::Array(hits).to_string(),
    );
    let provisioned: serde_json::Value =
        serde_json::from_str(joinable_provisioned()).expect("provisioned fixture");
    let mut dashboard: serde_json::Value = serde_json::from_str(
        provisioned["items"][0]["data"]["pods.json"]
            .as_str()
            .expect("dashboard JSON"),
    )
    .expect("joinable dashboard");
    dashboard["uid"] = "joinable".into();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/joinable",
        200,
        dashboard.to_string(),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        metric_matrix(1),
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("invalid search hits do not hide an independently readable dashboard");
    };
    assert_eq!(frame.stamps.len(), 1);
    assert!(!frame.truncated);
    assert_eq!(
        frame.note.as_deref(),
        Some(
            "PromQL named by Grafana; unsupported Grafana dashboard UID; expected 1 to 40 ASCII letters, digits, '-' or '_'"
        ),
    );
    assert_metric_requests(&script, 1, true);
    assert_grafana_request(&script, "api/health", true);
    assert_grafana_request(&script, "api/search?type=dash-db&limit=100", false);
    assert_grafana_request(&script, "api/dashboards/uid/joinable", false);
    assert_eq!(script.requests_for("/proxy/api/dashboards/").len(), 1);
    assert_eq!(script.requests_for("/services/grafana:").len(), 3);
}

fn annotated_overlay_body(body: &str) -> (String, Vec<String>, Vec<String>) {
    let mut value: serde_json::Value = serde_json::from_str(body).expect("numeric overlay fixture");
    let mut warnings: Vec<String> = (0..9).map(|n| format!("server warning {n}")).collect();
    warnings.push(format!(
        "last server warning: {} exact end",
        "detail 界 ".repeat(200)
    ));
    let infos = vec!["the server retained informational context".to_string()];
    value["warnings"] = serde_json::json!(warnings);
    value["infos"] = serde_json::json!(infos);
    (value.to_string(), warnings, infos)
}

fn assert_metric_query_shape(script: &Script) {
    assert_metric_requests(script, 1, true);
    let requests = script.requests_for("/proxy/api/v1/query_range?");
    let request = &requests[0];
    let prefix = "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?query=sum+by+%28namespace%2C+pod%29+%28rate%28node_cpu_seconds_total%5B5m%5D%29%29&start=";
    assert!(request.path.starts_with(prefix));
    let (start, rest) = request.path[prefix.len()..].split_once("&end=").unwrap();
    let (end, rest) = rest.split_once("&step=").unwrap();
    let start: f64 = start.parse().unwrap();
    let end: f64 = end.parse().unwrap();
    assert!(start.is_finite() && end.is_finite());
    assert_eq!(end - start, overlay::RANGE_SECS);
    assert_eq!(rest, "30s&limit=256");
    assert!(request.body_collected);
}

#[test]
fn metric_overlay_retains_full_named_annotations_with_data_and_empty_success() {
    for count in [0, 1, 256, 257] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script.route("GET", PROVISIONING_ROUTE, 200, joinable_provisioned());
        script_prometheus_bind(&script);
        let (body, warnings, infos) = annotated_overlay_body(&metric_matrix(count));
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
            200,
            body,
        );
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
            panic!("annotated success retains its data")
        };
        assert_eq!(frame.stamps.len(), count.min(256));
        assert_eq!(frame.truncated, count > 256);
        let note = frame.note.expect("full diagnostics");
        assert_eq!(note.matches("Prometheus query warnings: ").count(), 1);
        assert_eq!(note.matches("Prometheus query info: ").count(), 1);
        for warning in warnings {
            assert!(note.contains(&warning));
        }
        for info in infos {
            assert!(note.contains(&info));
        }
        assert!(note.contains("PromQL named by Grafana"));
        assert_eq!(note.contains("completeness is unknown"), count >= 256);
        assert_eq!(
            note.contains("1 returned series omitted locally"),
            count > 256
        );
        assert!(!note.contains("not every object was stamped"));
        assert_metric_query_shape(&script);
    }
}

#[test]
fn metric_annotations_keep_other_source_failure_sentences_and_successful_stamps() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let mut page: serde_json::Value = serde_json::from_str(joinable_provisioned()).unwrap();
    page["metadata"]["continue"] = "next".into();
    script.route("GET", PROVISIONING_ROUTE, 200, page.to_string());
    script.route(
        "GET",
        &format!("{PROVISIONING_ROUTE}&continue=next"),
        500,
        r#"{"message":"the next dashboard page is temporarily unavailable"}"#,
    );
    script_prometheus_bind(&script);
    let (body, warnings, _) = annotated_overlay_body(&metric_matrix(1));
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        body,
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("data survives source failure")
    };
    assert_eq!(frame.stamps.len(), 1);
    let note = frame.note.unwrap();
    assert!(note.contains("the next dashboard page is temporarily unavailable"));
    assert!(note.contains(warnings.last().unwrap()));
    assert_metric_requests(&script, 2, true);
}

#[test]
fn mesh_partial_success_retains_each_named_full_annotation_beside_denial_and_failure() {
    for success in 0..3 {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script_prometheus_bind(&script);
        let (body, warnings, infos) = annotated_overlay_body(&observed_mesh_matrix(success));
        let denied = (success + 1) % 3;
        let failed = (success + 2) % 3;
        script_mesh_query(&script, success, 200, &body);
        script_mesh_query(
            &script,
            denied,
            403,
            r#"{"message":"denied body is not a successful observation"}"#,
        );
        script_mesh_query(
            &script,
            failed,
            503,
            r#"{"message":"the mesh backend is restarting"}"#,
        );
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
            panic!("partial mesh data survives")
        };
        assert_observed_mesh_stamps(&frame);
        let note = frame.note.unwrap();
        assert!(note.starts_with("Mesh observations are incomplete; "));
        assert!(note.contains(&format!("{} warnings: ", MESH_QUERY_SOURCES[success])));
        assert!(note.contains(&format!("{} info: ", MESH_QUERY_SOURCES[success])));
        assert!(note.contains(&format!(
            "{}: prometheus: access denied for this account",
            MESH_QUERY_SOURCES[denied]
        )));
        assert!(note.contains(&format!(
            "{}: prometheus: the mesh backend is restarting",
            MESH_QUERY_SOURCES[failed]
        )));
        for warning in warnings {
            assert!(note.contains(&warning));
        }
        for info in infos {
            assert!(note.contains(&info));
        }
        assert_mesh_query_requests(&script);
    }
}

#[test]
fn mesh_filtered_series_report_no_joinable_links_instead_of_claiming_no_returned_series() {
    for labels in [
        serde_json::json!({"__name__":"unsupported_exporter","source":"prod/a","destination":"prod/b"}),
        serde_json::json!({"__name__":"istio_requests_total","source":"prod/a"}),
    ] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script_prometheus_bind(&script);
        let raw = serde_json::json!({"status":"success","data":{"resultType":"matrix","result":[{"metric":labels,"values":[[1,"2"]]}]}}).to_string();
        let (body, warnings, _) = annotated_overlay_body(&raw);
        script_mesh_query(&script, 0, 200, &body);
        script_mesh_query(&script, 1, 200, EMPTY_MATRIX);
        script_mesh_query(&script, 2, 200, EMPTY_MATRIX);
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
            panic!("filtered labels are a labelled result")
        };
        assert!(frame.stamps.is_empty());
        assert!(!frame.truncated);
        let note = frame.note.unwrap();
        assert!(note.starts_with("no joinable mesh workload links could be derived"));
        assert!(note.contains(warnings.last().unwrap()));
        assert!(!note.contains("no Istio, Hubble, or Linkerd request series"));
        assert_mesh_query_requests(&script);
    }
}

#[test]
fn mesh_local_series_omissions_can_duplicate_a_link_and_do_not_prove_missing_objects() {
    for count in [256, 257] {
        let script = Script::default();
        script_overlay_base(&script);
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        script_prometheus_bind(&script);
        let mut value: serde_json::Value = serde_json::from_str(&observed_mesh_matrix(0)).unwrap();
        value["data"]["result"] =
            serde_json::Value::Array(vec![value["data"]["result"][0].clone(); count]);
        script_mesh_query(&script, 0, 200, &value.to_string());
        script_mesh_query(&script, 1, 200, EMPTY_MATRIX);
        script_mesh_query(&script, 2, 200, EMPTY_MATRIX);
        let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
            panic!("retained duplicate links remain usable")
        };
        assert_eq!(
            frame.stamps.len(),
            2,
            "every returned series names the same two workloads"
        );
        assert_eq!(frame.truncated, count > 256);
        let note = frame.note.unwrap();
        assert!(note.contains("Istio query: range query reached the requested 256-series limit; completeness is unknown"));
        assert_eq!(
            note.contains("Istio query: 1 returned series omitted locally"),
            count > 256
        );
        assert!(!note.contains("not every object was stamped"));
        assert_mesh_query_requests(&script);
    }
}

#[test]
fn empty_mesh_successes_retain_each_querys_complete_annotations_without_claiming_a_failure() {
    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script_prometheus_bind(&script);
    let (body, warnings, infos) = annotated_overlay_body(EMPTY_MATRIX);
    for source in 0..3 {
        script_mesh_query(&script, source, 200, &body);
    }
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
        panic!("empty annotated successes are observable")
    };
    assert!(frame.stamps.is_empty());
    assert!(!frame.truncated);
    let note = frame.note.unwrap();
    assert!(note.starts_with("no joinable mesh workload links could be derived"));
    assert!(
        !note.contains("Mesh observations are unavailable")
            && !note.contains("Mesh observations are incomplete")
    );
    for source in MESH_QUERY_SOURCES {
        assert!(note.contains(&format!("{source} warnings: ")));
        assert!(note.contains(&format!("{source} info: ")));
        for warning in &warnings {
            assert!(note.contains(warning.as_str()));
        }
        for info in &infos {
            assert!(note.contains(info.as_str()));
        }
    }
    assert_mesh_query_requests(&script);
}

#[test]
fn thousands_of_short_annotations_report_exact_omissions_without_repeating_source_labels() {
    use k10s_data::integrations::prom::MAX_ANNOTATIONS_PER_LEVEL;

    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script.route("GET", PROVISIONING_ROUTE, 200, joinable_provisioned());
    script_prometheus_bind(&script);
    let mut value: serde_json::Value = serde_json::from_str(EMPTY_MATRIX).unwrap();
    let mut warnings = vec![String::new(); 4096];
    warnings[MAX_ANNOTATIONS_PER_LEVEL - 1] = "last retained warning 界".into();
    *warnings.last_mut().unwrap() = "omitted final warning".into();
    let mut infos = vec![String::new(); 4096];
    infos[MAX_ANNOTATIONS_PER_LEVEL - 1] = "last retained info λ".into();
    *infos.last_mut().unwrap() = "omitted final info".into();
    value["warnings"] = serde_json::json!(warnings);
    value["infos"] = serde_json::json!(infos);
    let body = value.to_string();
    let body_bytes = body.len();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        body,
    );
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::Metrics) else {
        panic!("the retained annotations and omissions remain observable")
    };
    let note = frame.note.unwrap();
    assert_eq!(
        note,
        format!(
            "Prometheus query: 3840 warning annotations omitted by the local limit; Prometheus query: 3840 info annotations omitted by the local limit; Prometheus query warnings: {}; Prometheus query info: {}; PromQL named by Grafana",
            warnings[..MAX_ANNOTATIONS_PER_LEVEL].join("; "),
            infos[..MAX_ANNOTATIONS_PER_LEVEL].join("; ")
        )
    );
    assert!(
        note.len() <= body_bytes + 512,
        "source labels are added once per annotation group"
    );
    assert_eq!(note.matches("Prometheus query warnings: ").count(), 1);
    assert_eq!(note.matches("Prometheus query info: ").count(), 1);
    assert!(!note.contains("omitted final warning"));
    assert!(!note.contains("omitted final info"));
    assert_metric_query_shape(&script);
}

#[test]
fn mesh_annotation_omissions_remain_visible_beside_denials_failures_and_stamps() {
    use k10s_data::integrations::prom::MAX_ANNOTATIONS_PER_LEVEL;

    let script = Script::default();
    script_overlay_base(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    script_prometheus_bind(&script);
    let mut body: serde_json::Value = serde_json::from_str(&observed_mesh_matrix(0)).unwrap();
    body["warnings"] = serde_json::json!(vec!["retained warning"; MAX_ANNOTATIONS_PER_LEVEL + 2]);
    body["infos"] = serde_json::json!(vec!["retained info"; MAX_ANNOTATIONS_PER_LEVEL + 3]);
    script_mesh_query(&script, 0, 200, &body.to_string());
    script_mesh_query(&script, 1, 403, "denied");
    script_mesh_query(&script, 2, 500, r#"{"message":"complete backend failure"}"#);
    let Fetched::Ok(frame) = fetch_overlay(&sync, overlay::Kind::MeshObserved) else {
        panic!("partial mesh data survives")
    };
    assert_observed_mesh_stamps(&frame);
    let note = frame.note.expect("mesh outcomes and omissions");
    assert!(note.contains("Mesh observations are incomplete"));
    assert!(note.contains("Hubble query: prometheus: access denied for this account"));
    assert!(note.contains("Linkerd query: prometheus: complete backend failure"));
    assert!(note.contains("Istio query: 2 warning annotations omitted by the local limit"));
    assert!(note.contains("Istio query: 3 info annotations omitted by the local limit"));
    assert!(note.contains("retained warning"));
    assert!(note.contains("retained info"));
    assert_mesh_query_requests(&script);
}
