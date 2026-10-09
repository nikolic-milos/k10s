//! Span-origin log queries retain their recorded resource, trace and time on the wire.

use crate::*;
use k10s_data::integrations::loki::{Logs, RangeQuery};
use k10s_data::read::{Fetched, Seen as SourceSeen};

const SOURCE: &str = "/api/v1/services?";
const READY: &str = "/api/v1/namespaces/observe/services/loki:3100/proxy/ready";
const IDENTITY: &str = "/api/v1/namespaces/observe/services/loki";
const METADATA: &str = "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1";
const TRACE: &str = "5b8efff798038103d269b633813fc700";
const SPAN: &str = "a9a1b2c3d4e5f678";
const QUERY: &str = "{k8s_namespace_name=\"prod\",k8s_pod_name=\"producer\"} | k8s_pod_uid=\"recorded-pod\" | trace_id=\"5b8efff798038103d269b633813fc700\"";
const PATH: &str = "/api/v1/namespaces/observe/services/loki:3100/proxy/loki/api/v1/query_range?query=%7Bk8s_namespace_name%3D%22prod%22%2Ck8s_pod_name%3D%22producer%22%7D+%7C+k8s_pod_uid%3D%22recorded-pod%22+%7C+trace_id%3D%225b8efff798038103d269b633813fc700%22&start=1699999999000000000&end=1700000001003000000&limit=200&direction=BACKWARD";

fn services(script: &Script, has_loki: bool) {
    let items: Vec<_> = [
        ("loki", 3100),
        ("prometheus", 9090),
        ("tempo", 3200),
        ("grafana", 3000),
    ]
    .into_iter()
    .filter(|(name, _)| has_loki || *name != "loki")
    .map(|(name, port)| {
        serde_json::json!({
            "metadata":{"name":name, "namespace":"observe", "uid":format!("{name}-source"), "resourceVersion":"10"},
            "spec":{"ports":[{"port":port}]}
        })
    })
    .collect();
    script.route(
        "GET",
        SOURCE,
        200,
        serde_json::json!({"apiVersion":"v1", "kind":"ServiceList", "items":items}).to_string(),
    );
}

fn source(script: &Script) {
    services(script, true);
    script.route("GET", READY, 200, "ready");
}

fn connected(script: &Script) -> (tokio::runtime::Runtime, Sync) {
    script_discovery(script);
    script_rules_review(script);
    script_access_reviews(script, true, 32);
    script_lists(script);
    let runtime = runtime();
    let (sync, _events) = sync_on(&runtime, script);
    (runtime, sync)
}

fn query(sync: &Sync, expression: &str) -> Fetched<Option<Logs>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.observe_loki(
        RangeQuery {
            query: expression.to_string(),
            start_ns: 1_699_999_999_000_000_000,
            end_ns: 1_700_000_001_003_000_000,
            limit: 200,
        },
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    wait(&rx)
}

fn assert_get(row: &Seen, path: &str, accept: &str) {
    assert_eq!(row.method, "GET");
    assert_eq!(row.path.replacen("?&", "?", 1), path);
    assert_eq!(row.accept, accept);
    assert_eq!(row.content_type, "");
    assert!(row.body.is_empty());
}

#[test]
fn a_span_origin_log_query_keeps_resource_trace_and_window_without_rebinding_namespace() {
    let script = Script::default();
    let (_runtime, sync) = connected(&script);
    let before = script.seen().len();
    source(&script);
    script.route(
        "GET",
        PATH,
        200,
        serde_json::json!({
            "status":"success", "data":{"resultType":"streams", "result":[{
                "stream":{"k8s_namespace_name":"prod", "k8s_pod_name":"producer"},
                "values":[["1700000000002468000", "completed operation", {
                    "k8s_pod_uid":"recorded-pod", "trace_id":TRACE, "span_id":SPAN
                }]]
            }]}
        })
        .to_string(),
    );
    let Fetched::Ok(Some(logs)) = query(&sync, QUERY) else {
        panic!("the recorded resource's trace log must answer");
    };
    assert_eq!(logs.source, "observe/loki:3100");
    assert!(!logs.truncated);
    assert!(!logs.limit_reached);
    assert_eq!(logs.streams.len(), 1);
    assert_eq!(
        logs.streams[0].labels,
        [
            ("k8s_namespace_name".to_string(), "prod".to_string()),
            ("k8s_pod_name".to_string(), "producer".to_string()),
        ]
    );
    assert_eq!(logs.streams[0].lines.len(), 1);
    let line = &logs.streams[0].lines[0];
    assert_eq!(line.ts_ns, 1_700_000_000_002_468_000);
    for (key, value) in [
        ("k8s_pod_uid", "recorded-pod"),
        ("trace_id", TRACE),
        ("span_id", SPAN),
    ] {
        assert!(
            line.metadata
                .iter()
                .any(|pair| pair.0 == key && pair.1 == value)
        );
    }
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 3);
    assert_get(&rows[0], "/api/v1/services?limit=200", "");
    assert_get(&rows[1], READY, "application/json");
    assert_get(&rows[2], PATH, "application/json");

    let before = script.seen().len();
    script.route_accepting(
        "GET",
        IDENTITY,
        METADATA,
        200,
        r#"{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadata","metadata":{"name":"loki","namespace":"observe","uid":"loki-source","resourceVersion":"10"}}"#,
    );
    let other_path = PATH.replace("%22prod%22", "%22other%22");
    script.route(
        "GET",
        &other_path,
        200,
        r#"{"status":"success","data":{"resultType":"streams","result":[]}}"#,
    );
    let Fetched::Ok(Some(logs)) = query(&sync, &QUERY.replace("\"prod\"", "\"other\"")) else {
        panic!("the other recorded namespace must remain an explicit query");
    };
    assert!(logs.streams.is_empty());
    assert_eq!(logs.source, "observe/loki:3100");
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 2);
    assert_get(&rows[0], IDENTITY, METADATA);
    assert_get(&rows[1], &other_path, "application/json");
}

#[test]
fn span_origin_log_errors_remain_failures_or_denials_after_the_same_exact_get() {
    for status in [400, 403, 0] {
        let script = Script::default();
        let (_runtime, sync) = connected(&script);
        let before = script.seen().len();
        source(&script);
        if status == 0 {
            script.route_lost("GET", PATH);
        } else {
            script.route("GET", PATH, status, "recorded trace query refused");
        }
        let answer = query(&sync, QUERY);
        match (status, answer) {
            (403, Fetched::Denied { what: "loki" }) => {}
            (400, Fetched::Failed { what: "loki", why }) => {
                assert_eq!(why, "recorded trace query refused");
            }
            (0, Fetched::Failed { what: "loki", why }) => {
                assert!(why.contains("the scripted connection was lost"), "{why}");
            }
            (_, other) => panic!("status {status} must retain its state: {other:?}"),
        }
        let rows = after_sync(&script, before);
        assert_eq!(rows.len(), 3);
        assert_get(&rows[0], "/api/v1/services?limit=200", "");
        assert_get(&rows[1], READY, "application/json");
        assert_get(&rows[2], PATH, "application/json");
    }
}

fn probe(sync: &Sync) -> Fetched<SourceSeen> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.observe_loki_probe(move |answer| {
        let _ = tx.send(answer);
    });
    wait(&rx)
}

#[test]
fn a_loki_only_probe_reads_no_other_backend_and_reuses_the_verified_source() {
    let script = Script::default();
    let (_runtime, sync) = connected(&script);
    let before = script.seen().len();
    source(&script);
    assert_eq!(probe(&sync), Fetched::Ok(SourceSeen::Bound));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 2);
    assert_get(&rows[0], "/api/v1/services?limit=200", "");
    assert_get(&rows[1], READY, "application/json");

    let before = script.seen().len();
    script.route_accepting(
        "GET",
        IDENTITY,
        METADATA,
        200,
        r#"{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadata","metadata":{"name":"loki","namespace":"observe","uid":"loki-source","resourceVersion":"10"}}"#,
    );
    assert_eq!(probe(&sync), Fetched::Ok(SourceSeen::Bound));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 1);
    assert_get(&rows[0], IDENTITY, METADATA);
}

#[test]
fn an_absent_loki_probe_does_not_probe_visible_metrics_traces_or_dashboards() {
    let script = Script::default();
    let (_runtime, sync) = connected(&script);
    let before = script.seen().len();
    services(&script, false);
    assert_eq!(probe(&sync), Fetched::Ok(SourceSeen::Absent));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 1);
    assert_get(&rows[0], "/api/v1/services?limit=200", "");

    let before = script.seen().len();
    assert_eq!(probe(&sync), Fetched::Ok(SourceSeen::Absent));
    assert!(after_sync(&script, before).is_empty());
}

#[test]
fn a_denied_loki_probe_keeps_the_denied_resource_instead_of_claiming_absence() {
    for proxy in [false, true] {
        let script = Script::default();
        let (_runtime, sync) = connected(&script);
        let before = script.seen().len();
        let expected = if proxy {
            services(&script, true);
            script.route("GET", READY, 403, r#"{"message":"proxy refused"}"#);
            "Loki proxy: loki access denied for this account"
        } else {
            script.route(
                "GET",
                SOURCE,
                403,
                r#"{"message":"service inventory refused"}"#,
            );
            "services: access denied for this account"
        };
        assert_eq!(
            probe(&sync),
            Fetched::Failed {
                what: "Loki",
                why: expected.to_string()
            }
        );
        let rows = after_sync(&script, before);
        assert_eq!(rows.len(), if proxy { 2 } else { 1 });
        assert_get(&rows[0], "/api/v1/services?limit=200", "");
        if proxy {
            assert_get(&rows[1], READY, "application/json");
        }
    }
}

#[test]
fn a_failed_loki_probe_preserves_its_server_sentence_and_never_runs_a_query() {
    let script = Script::default();
    let (_runtime, sync) = connected(&script);
    let before = script.seen().len();
    services(&script, true);
    script.route(
        "GET",
        READY,
        500,
        r#"{"message":"Loki storage is unavailable"}"#,
    );
    assert_eq!(
        probe(&sync),
        Fetched::Failed {
            what: "Loki",
            why: "Loki storage is unavailable".to_string()
        }
    );
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 2);
    assert_get(&rows[0], "/api/v1/services?limit=200", "");
    assert_get(&rows[1], READY, "application/json");
}
