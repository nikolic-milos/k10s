//! Native source reuse, identity verification and cancellation on the wire.

use crate::*;
use k10s_data::integrations::{grafana, loki, prom};
use k10s_data::read::{Fetched, ObserveTools, Reader};
use k10s_data::telemetry::{RequestMetrics, request_metrics};

const SOURCE_LIST: &str = "/api/v1/services?";
const SOURCE_GET: &str = "/api/v1/namespaces/observe/services/prometheus";
const SOURCE_PROBE: &str = "/api/v1/namespaces/observe/services/prometheus:9090/proxy/-/ready";
const QUERY: &str = "/api/v1/namespaces/observe/services/prometheus:9090/proxy/api/v1/query_range?";
const METADATA: &str = "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1";
const RESULT: &str = r#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#;

async fn connected(script: &Script) -> (Sync, EventReceiver) {
    script_discovery(script);
    script_rules_review(script);
    script_access_reviews(script, true, 32);
    script_lists(script);
    let (tx, rx) = crossbeam_channel::unbounded();
    let sync = sync_from(
        script.clients(),
        "prod",
        &options(),
        tx,
        Arc::new(IngestMetrics::default()),
    )
    .await
    .expect("scripted cluster sync");
    (sync, rx)
}

fn service(name: &str, uid: &str, version: &str, port: u16) -> serde_json::Value {
    serde_json::json!({
        "metadata":{"name":name,"namespace":"observe","uid":uid,"resourceVersion":version},
        "spec":{"ports":[{"port":port}]}
    })
}

fn listed(script: &Script, items: Vec<serde_json::Value>) {
    script.route(
        "GET",
        SOURCE_LIST,
        200,
        serde_json::json!({"apiVersion":"v1","kind":"ServiceList","items":items}).to_string(),
    );
}

fn discovered(script: &Script, uid: &str, version: &str) {
    listed(script, vec![service("prometheus", uid, version, 9090)]);
    script.route("GET", SOURCE_PROBE, 200, "ready");
}

fn verified(script: &Script, uid: &str, version: &str) -> usize {
    let body = serde_json::json!({
        "apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadata",
        "metadata":{"name":"prometheus","namespace":"observe","uid":uid,"resourceVersion":version}
    })
    .to_string();
    let bytes = body.len();
    script.route_accepting("GET", SOURCE_GET, METADATA, 200, body);
    bytes
}

async fn query(
    reader: &Reader,
    expr: &str,
) -> (Fetched<Option<prom::QueryResult>>, RequestMetrics) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _stop = reader.observe_prometheus(expr.to_string(), 1.0, 3.0, "1s".into(), move |reply| {
        tx.send((reply, request_metrics().expect("admitted request meter")))
            .expect("query reply");
    });
    rx.await.expect("query callback")
}

async fn probe(reader: &Reader) -> ObserveTools {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _stop = reader.observe_probe(move |reply| {
        tx.send(reply).expect("probe reply");
    });
    let Fetched::Ok(tools) = rx.await.expect("probe callback") else {
        panic!("an admitted probe reports its individual source states");
    };
    tools
}

fn assert_get(row: &Seen, accept: &str) {
    assert_eq!(row.method, "GET");
    assert_eq!(row.accept, accept);
    assert_eq!(row.content_type, "");
    assert!(row.body.is_empty());
}

fn assert_query(row: &Seen, expr: &str) {
    assert_get(row, "application/json");
    assert!(row.path.starts_with(QUERY));
    assert!(row.path.contains(&format!("query={expr}")));
    for parameter in ["start=1", "end=3", "step=1s", "limit=256"] {
        assert!(row.path.contains(parameter), "{}", row.path);
    }
}

fn failure(answer: Fetched<Option<prom::QueryResult>>) -> String {
    let Fetched::Failed { why, .. } = answer else {
        panic!("source failure must be labelled: {answer:?}");
    };
    why
}

#[tokio::test(start_paused = true)]
async fn a_warm_query_verifies_metadata_and_reuses_discovery_and_probe() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let before = script.seen().len();
    discovered(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    let (answer, cold) = query(&sync.reader, "up").await;
    assert!(matches!(answer, Fetched::Ok(Some(_))));
    assert_eq!(cold.requests, 3);
    let metadata_bytes = verified(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    let (answer, warm) = query(&sync.reader.clone(), "process_cpu_seconds_total").await;
    assert!(matches!(answer, Fetched::Ok(Some(_))));
    assert_eq!(warm.requests, 2);
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 5);
    assert_get(&rows[0], "");
    assert!(rows[0].path.starts_with(SOURCE_LIST));
    assert!(rows[0].path.contains("limit=200"));
    assert_get(&rows[1], "application/json");
    assert_eq!(rows[1].path, SOURCE_PROBE);
    assert_query(&rows[2], "up");
    assert_get(&rows[3], METADATA);
    assert_eq!(rows[3].path, SOURCE_GET);
    assert_query(&rows[4], "process_cpu_seconds_total");
    assert_eq!(warm.bytes, RESULT.len() + metadata_bytes);
    assert!(rows.iter().all(|row| !row.path.contains("secrets")));
}

#[tokio::test(start_paused = true)]
async fn another_reader_never_reuses_a_contexts_source_snapshot() {
    let first = Script::default();
    let second = Script::default();
    let (first_sync, _first_events) = connected(&first).await;
    let (second_sync, _second_events) = connected(&second).await;
    let first_before = first.seen().len();
    let second_before = second.seen().len();
    for (script, uid, value) in [(&first, "first-uid", "11"), (&second, "second-uid", "22")] {
        discovered(script, uid, "10");
        script.route("GET", QUERY, 200, format!(
            r#"{{"status":"success","data":{{"resultType":"matrix","result":[{{"metric":{{}},"values":[[1,"{value}"]]}}]}}}}"#
        ));
    }
    for (reader, value) in [(&first_sync.reader, 11.0), (&second_sync.reader, 22.0)] {
        let (answer, metrics) = query(reader, "up").await;
        let Fetched::Ok(Some(result)) = answer else {
            panic!("{answer:?}")
        };
        assert_eq!(result.series[0].points[0].1, value);
        assert_eq!(metrics.requests, 3);
    }
    assert_eq!(after_sync(&first, first_before).len(), 3);
    assert_eq!(after_sync(&second, second_before).len(), 3);
}

#[tokio::test(start_paused = true)]
async fn absence_is_bounded_by_expiry_and_an_explicit_refresh() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let before = script.seen().len();
    listed(&script, Vec::new());
    for expected in [1, 0] {
        let (answer, metrics) = query(&sync.reader, "up").await;
        assert!(matches!(answer, Fetched::Ok(None)));
        assert_eq!(metrics.requests, expected);
    }
    tokio::time::advance(Duration::from_secs(30)).await;
    discovered(&script, "appeared-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));
    listed(&script, Vec::new());
    assert_eq!(
        probe(&sync.reader).await.prometheus,
        k10s_data::read::Seen::Absent
    );
    let (answer, metrics) = query(&sync.reader, "up").await;
    assert!(matches!(answer, Fetched::Ok(None)));
    assert_eq!(metrics.requests, 0);
    let rows = after_sync(&script, before);
    assert_eq!(
        rows.iter()
            .filter(|row| row.path.starts_with(SOURCE_LIST))
            .count(),
        3
    );
    for row in &rows {
        assert_get(
            row,
            if row.path.starts_with(SOURCE_LIST) {
                ""
            } else {
                "application/json"
            },
        );
    }
}

#[tokio::test(start_paused = true)]
async fn denied_discovery_is_retried_and_failed_body_bytes_are_counted() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let before = script.seen().len();
    let error = r#"{"message":"Service store unavailable"}"#;
    script.route("GET", SOURCE_LIST, 500, error);
    let (answer, metrics) = query(&sync.reader, "up").await;
    assert_eq!(failure(answer), "Service store unavailable");
    assert_eq!(metrics.bytes, error.len());
    script.route(
        "GET",
        SOURCE_LIST,
        403,
        r#"{"message":"cannot list services"}"#,
    );
    let (answer, metrics) = query(&sync.reader, "up").await;
    assert!(failure(answer).contains("services: access denied"));
    assert_eq!(metrics.bytes, 0, "denied bodies are never read");
    discovered(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 5);
    for row in &rows[..3] {
        assert_get(row, "");
    }
    assert_get(&rows[3], "application/json");
    assert_query(&rows[4], "up");
}

#[tokio::test(start_paused = true)]
async fn refresh_and_expiry_detect_competing_services_without_choosing_one() {
    for refresh in [false, true] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        discovered(&script, "source-uid", "10");
        script.route("GET", QUERY, 200, RESULT);
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));
        let before = script.seen().len();
        listed(
            &script,
            vec![
                service("prometheus", "source-uid", "10", 9090),
                service("prometheus-server", "second-uid", "20", 9090),
            ],
        );
        if refresh {
            let tools = probe(&sync.reader).await;
            assert_eq!(tools.prometheus, k10s_data::read::Seen::Unbound);
            assert!(
                tools
                    .errors
                    .iter()
                    .any(|why| why.contains("2 routed Services"))
            );
        } else {
            tokio::time::advance(Duration::from_secs(30)).await;
            verified(&script, "source-uid", "10");
        }
        assert!(failure(query(&sync.reader, "up").await.0).contains("2 routed Services"));
        let rows = after_sync(&script, before);
        assert_eq!(rows.len(), if refresh { 1 } else { 2 });
        for row in &rows {
            assert_get(
                row,
                if row.path.starts_with(SOURCE_LIST) {
                    ""
                } else {
                    METADATA
                },
            );
            assert!(!row.path.contains("/proxy/"));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn source_recreation_and_change_stop_the_query_until_sources_are_refreshed() {
    for (uid, version) in [("recreated-uid", "10"), ("source-uid", "11")] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        discovered(&script, "source-uid", "10");
        script.route("GET", QUERY, 200, RESULT);
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));
        let before = script.seen().len();
        verified(&script, uid, version);
        let why = failure(query(&sync.reader, "up").await.0);
        assert!(
            why.contains("observe/prometheus was recreated or changed"),
            "{why}"
        );
        assert!(why.contains("refresh sources"));
        let refused = after_sync(&script, before);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].path, SOURCE_GET);
        assert_get(&refused[0], METADATA);
        discovered(&script, uid, version);
        assert_eq!(
            probe(&sync.reader).await.prometheus,
            k10s_data::read::Seen::Bound
        );
        verified(&script, uid, version);
        script.route("GET", QUERY, 200, RESULT);
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn get_denial_uses_current_list_authority_and_does_not_redirect_recreated_sources() {
    for uid in ["source-uid", "recreated-uid"] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        discovered(&script, "source-uid", "10");
        script.route("GET", QUERY, 200, RESULT);
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));
        let before = script.seen().len();
        script.route(
            "GET",
            SOURCE_GET,
            403,
            r#"{"message":"get services is denied"}"#,
        );
        listed(&script, vec![service("prometheus", uid, "10", 9090)]);
        if uid == "source-uid" {
            script.route("GET", SOURCE_PROBE, 200, "ready");
            script.route("GET", QUERY, 200, RESULT);
            let (answer, metrics) = query(&sync.reader, "up").await;
            assert!(matches!(answer, Fetched::Ok(Some(_))));
            assert_eq!(metrics.requests, 4);
        } else {
            assert!(failure(query(&sync.reader, "up").await.0).contains("recreated or changed"));
        }
        let rows = after_sync(&script, before);
        assert_eq!(rows.len(), if uid == "source-uid" { 4 } else { 2 });
        assert_get(&rows[0], METADATA);
        assert_eq!(rows[0].path, SOURCE_GET);
        assert_get(&rows[1], "");
        if uid == "source-uid" {
            assert_get(&rows[2], "application/json");
            assert_eq!(rows[2].path, SOURCE_PROBE);
            assert_query(&rows[3], "up");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn source_get_failures_keep_the_server_sentence_and_invalidate_reuse() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    discovered(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));
    let before = script.seen().len();
    script.route(
        "GET",
        SOURCE_GET,
        404,
        r#"{"message":"Service prometheus was deleted"}"#,
    );
    assert_eq!(
        failure(query(&sync.reader, "up").await.0),
        "Service prometheus was deleted"
    );
    listed(&script, Vec::new());
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(None)
    ));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 2);
    assert_get(&rows[0], METADATA);
    assert_get(&rows[1], "");
}

#[tokio::test(start_paused = true)]
async fn a_query_denial_clears_the_binding_and_preserves_its_resource_label() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    discovered(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));
    let before = script.seen().len();
    verified(&script, "source-uid", "10");
    script.route("GET", QUERY, 403, r#"{"message":"query proxy is denied"}"#);
    assert_eq!(
        query(&sync.reader, "up").await.0,
        Fetched::Denied { what: "prometheus" }
    );
    discovered(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 5);
    assert_get(&rows[0], METADATA);
    assert_query(&rows[1], "up");
    assert_get(&rows[2], "");
    assert_get(&rows[3], "application/json");
    assert_query(&rows[4], "up");
}

#[tokio::test(start_paused = true)]
async fn cancelling_discovery_releases_the_shared_source_lock() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let before = script.seen().len();
    script.route_hanging("GET", SOURCE_LIST);
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let stop = sync
        .reader
        .observe_prometheus("up".into(), 1.0, 3.0, "1s".into(), move |reply| {
            tx.send(reply).expect("cancelled request must not reply");
        });
    for _ in 0..1_000 {
        if !after_sync(&script, before).is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        !after_sync(&script, before).is_empty(),
        "discovery reached the server"
    );
    drop(stop);
    for _ in 0..1_000 {
        if sync.reader.observe_schedule_counts().admitted == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(sync.reader.observe_schedule_counts().admitted, 0);
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    discovered(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 4);
    for row in &rows[..2] {
        assert_get(row, "");
    }
    assert_get(&rows[2], "application/json");
    assert_query(&rows[3], "up");
}

#[tokio::test(start_paused = true)]
async fn admitted_duplicates_share_binding_but_query_their_own_expressions() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let before = script.seen().len();
    discovered(&script, "source-uid", "10");
    verified(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    script.route("GET", QUERY, 200, RESULT);
    let (first, second) = tokio::join!(
        query(&sync.reader, "up"),
        query(&sync.reader, "process_cpu_seconds_total")
    );
    assert!(matches!(first.0, Fetched::Ok(Some(_))));
    assert!(matches!(second.0, Fetched::Ok(Some(_))));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows.iter()
            .filter(|row| row.path.starts_with(SOURCE_LIST))
            .count(),
        1
    );
    assert_eq!(
        rows.iter().filter(|row| row.path == SOURCE_PROBE).count(),
        1
    );
    assert_eq!(rows.iter().filter(|row| row.path == SOURCE_GET).count(), 1);
    assert_eq!(
        rows.iter()
            .filter(|row| row.path.starts_with(QUERY))
            .count(),
        2
    );
    for row in &rows {
        assert_get(
            row,
            if row.path.starts_with(SOURCE_LIST) {
                ""
            } else if row.path == SOURCE_GET {
                METADATA
            } else {
                "application/json"
            },
        );
    }
    assert!(
        rows.iter()
            .any(|row| row.path.starts_with(QUERY) && row.path.contains("query=up"))
    );
    assert!(
        rows.iter().any(|row| row.path.starts_with(QUERY)
            && row.path.contains("query=process_cpu_seconds_total"))
    );
}

#[tokio::test(start_paused = true)]
async fn oversized_source_identity_is_refused_before_any_probe_or_query() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let before = script.seen().len();
    listed(
        &script,
        vec![service("prometheus", &"u".repeat(16 << 10), "10", 9090)],
    );
    assert!(failure(query(&sync.reader, "up").await.0).contains("source identity exceeds 16 KiB"));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 1);
    assert_get(&rows[0], "");
    assert!(rows[0].path.starts_with(SOURCE_LIST));
}

#[tokio::test(start_paused = true)]
async fn native_log_trace_and_panel_queries_reuse_their_own_service_identity() {
    const TRACE_ID: &str = "0123456789abcdef0123456789abcdef";
    for (name, port, ready, rest, body) in [
        (
            "loki",
            3100,
            "ready",
            "loki/api/v1/query_range?",
            r#"{"status":"success","data":{"resultType":"streams","result":[]}}"#.to_string(),
        ),
        (
            "tempo",
            3200,
            "ready",
            "api/v2/traces/0123456789abcdef0123456789abcdef",
            serde_json::json!({"resourceSpans":[{"scopeSpans":[{"spans":[{
                "traceId":TRACE_ID,"spanId":"0123456789abcdef","name":"request",
                "startTimeUnixNano":"1000","endTimeUnixNano":"2000"
            }]}]}]})
            .to_string(),
        ),
        (
            "jaeger",
            16686,
            "",
            "api/traces/0123456789abcdef0123456789abcdef",
            serde_json::json!({"data":[{"traceID":TRACE_ID,"spans":[{
                "traceID":TRACE_ID,"spanID":"0123456789abcdef","operationName":"request",
                "startTime":1,"duration":1
            }],"processes":{}}]})
            .to_string(),
        ),
        (
            "grafana",
            3000,
            "api/health",
            "api/datasources/proxy/uid/prom-main/api/v1/query_range?",
            RESULT.to_string(),
        ),
    ] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        let before = script.seen().len();
        listed(&script, vec![service(name, "source-uid", "10", port)]);
        let proxy = format!("/api/v1/namespaces/observe/services/{name}:{port}/proxy/");
        script.route("GET", &format!("{proxy}{ready}"), 200, "ready");
        let path = format!("{proxy}{rest}");
        let identity_path = format!("/api/v1/namespaces/observe/services/{name}");
        for warm in [false, true] {
            if warm || name == "jaeger" {
                script.route_accepting("GET", &identity_path, METADATA, 200,
                    serde_json::json!({"metadata":{"name":name,"namespace":"observe","uid":"source-uid","resourceVersion":"10"}}).to_string());
            }
            if name == "grafana" {
                script.route(
                    "GET",
                    &format!("{proxy}api/datasources/uid/prom-main"),
                    200,
                    r#"{"uid":"prom-main","type":"prometheus","jsonData":{"timeInterval":"1s"}}"#,
                );
            }
            script.route("GET", &path, 200, body.clone());
            let (tx, rx) = tokio::sync::oneshot::channel();
            let stop = match name {
                "loki" => sync.reader.observe_loki(
                    loki::RangeQuery {
                        query: "{app=\"api\"}".into(),
                        start_ns: 1,
                        end_ns: 3,
                        limit: 200,
                    },
                    move |answer| {
                        tx.send((
                            matches!(&answer, Fetched::Ok(Some(_))),
                            format!("{answer:?}"),
                        ))
                        .expect("log reply");
                    },
                ),
                "grafana" => sync.reader.observe_panel(
                    grafana::PanelQuery {
                        ref_id: "A".into(),
                        expr: "up".into(),
                        dialect: grafana::QueryDialect::PromQL,
                        datasource: Some("prom-main".into()),
                        min_step: String::new(),
                        unsupported: None,
                    },
                    grafana::Variables {
                        namespace: "prod".into(),
                        pods: Vec::new(),
                        node: None,
                        start_ms: 1000,
                        end_ms: 3000,
                        step_ms: 1000,
                        min_interval: String::new(),
                    },
                    move |answer| {
                        tx.send((
                            matches!(&answer, Fetched::Ok(Some(_))),
                            format!("{answer:?}"),
                        ))
                        .expect("panel reply");
                    },
                ),
                _ => sync.reader.observe_trace(TRACE_ID.into(), move |answer| {
                    tx.send((
                        matches!(&answer, Fetched::Ok(Some(_))),
                        format!("{answer:?}"),
                    ))
                    .expect("trace reply");
                }),
            };
            let (ok, outcome) = rx.await.expect("native query callback");
            assert!(ok, "{name} warm={warm}: {outcome}");
            drop(stop);
        }
        let rows = after_sync(&script, before);
        assert_eq!(
            rows.len(),
            match name {
                "grafana" => 7,
                "jaeger" => 6,
                _ => 5,
            },
            "{name}: {rows:?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.path.starts_with(SOURCE_LIST))
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.path == format!("{proxy}{ready}"))
                .count(),
            1
        );
        let metadata: Vec<_> = rows
            .iter()
            .filter(|row| row.path == identity_path)
            .collect();
        assert_eq!(metadata.len(), if name == "jaeger" { 2 } else { 1 });
        for row in metadata {
            assert_get(row, METADATA);
        }
        for row in rows.iter().filter(|row| row.path.contains("/proxy/")) {
            assert_get(row, "application/json");
        }
        let queries: Vec<_> = rows
            .iter()
            .filter(|row| row.path.starts_with(&path))
            .collect();
        assert_eq!(queries.len(), 2);
        for row in queries {
            if name == "loki" {
                for parameter in [
                    "start=1",
                    "end=3",
                    "limit=200",
                    "query=%7Bapp%3D%22api%22%7D",
                ] {
                    assert!(row.path.contains(parameter), "{}", row.path);
                }
            } else if name == "grafana" {
                for parameter in ["query=up", "start=1", "end=3", "step=1s", "limit=256"] {
                    assert!(row.path.contains(parameter), "{}", row.path);
                }
            } else {
                assert_eq!(row.path, path);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn native_catalog_refreshes_contents_while_reusing_the_grafana_binding() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let before = script.seen().len();
    let proxy = "/api/v1/namespaces/observe/services/grafana:3000/proxy/";
    listed(&script, vec![service("grafana", "source-uid", "10", 3000)]);
    script.route("GET", &format!("{proxy}api/health"), 200, "ready");
    for warm in [false, true] {
        script.route(
            "GET",
            "/api/v1/configmaps?",
            200,
            r#"{"apiVersion":"v1","kind":"ConfigMapList","items":[]}"#,
        );
        if warm {
            script.route_accepting("GET", "/api/v1/namespaces/observe/services/grafana", METADATA, 200,
                r#"{"metadata":{"name":"grafana","namespace":"observe","uid":"source-uid","resourceVersion":"10"}}"#);
        }
        script.route("GET", &format!("{proxy}api/search?"), 200, "[]");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _stop = sync.reader.observe_grafana(move |answer| {
            tx.send(answer).expect("catalog reply");
        });
        assert!(
            matches!(rx.await.expect("catalog callback"), Fetched::Ok(catalog) if catalog.served && catalog.dashboards.is_empty())
        );
    }
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 7);
    assert_eq!(
        rows.iter()
            .filter(|row| row.path.starts_with(SOURCE_LIST))
            .count(),
        1
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.path == format!("{proxy}api/health"))
            .count(),
        1
    );
    for row in &rows {
        if row.path.starts_with("/api/v1/configmaps?") {
            assert_get(row, "");
            assert!(
                row.path
                    .contains("labelSelector=grafana_dashboard+in+%281%2Ctrue%29"),
                "{}",
                row.path
            );
            assert!(row.path.contains("limit=200"));
        } else if row.path == "/api/v1/namespaces/observe/services/grafana" {
            assert_get(row, METADATA);
        } else {
            assert_get(
                row,
                if row.path.starts_with(SOURCE_LIST) {
                    ""
                } else {
                    "application/json"
                },
            );
        }
    }
}
