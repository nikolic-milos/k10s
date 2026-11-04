//! Native source reuse, identity verification and cancellation on the wire.

use crate::*;
use k10s_core::ServiceIdentity;
use k10s_data::integrations::{grafana, loki, prom, traces};
use k10s_data::read::{Fetched, ObserveTools, Reader};
use k10s_data::telemetry::{RequestMetrics, request_metrics};

const SOURCE_LIST: &str = "/api/v1/services?";
const SOURCE_GET: &str = "/api/v1/namespaces/observe/services/prometheus";
const SOURCE_PROBE: &str = "/api/v1/namespaces/observe/services/prometheus:9090/proxy/-/ready";
const QUERY: &str = "/api/v1/namespaces/observe/services/prometheus:9090/proxy/api/v1/query_range?";
const METADATA: &str = "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1";
const RESULT: &str = r#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#;
const TRACE_ID: &str = "0123456789abcdef0123456789abcdef";
const TEMPO_GET: &str = "/api/v1/namespaces/observe/services/tempo";
const TEMPO_READY: &str = "/api/v1/namespaces/observe/services/tempo:3200/proxy/ready";
const TEMPO_TRACE: &str = "/api/v1/namespaces/observe/services/tempo:3200/proxy/api/v2/traces/0123456789abcdef0123456789abcdef";
const JAEGER_GET: &str = "/api/v1/namespaces/observe/services/jaeger";
const JAEGER_READY: &str = "/api/v1/namespaces/observe/services/jaeger:16686/proxy/";
const JAEGER_TRACE: &str = "/api/v1/namespaces/observe/services/jaeger:16686/proxy/api/traces/0123456789abcdef0123456789abcdef";

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

fn trace_services() -> Vec<serde_json::Value> {
    vec![
        service("tempo", "tempo-uid", "10", 3200),
        service("jaeger", "jaeger-uid", "20", 16686),
    ]
}

fn healthy_jaeger(script: &Script) {
    script.route("GET", JAEGER_READY, 200, "ready");
    script.route_accepting(
        "GET", JAEGER_GET, METADATA, 200,
        r#"{"metadata":{"name":"jaeger","namespace":"observe","uid":"jaeger-uid","resourceVersion":"20"}}"#,
    );
    script.route(
        "GET",
        JAEGER_TRACE,
        200,
        serde_json::json!({"data":[{"traceID":TRACE_ID,"spans":[{
            "traceID":TRACE_ID,"spanID":"0123456789abcdef","operationName":"request",
            "startTime":1,"duration":1
        }],"processes":{}}]})
        .to_string(),
    );
}

fn tempo_trace(script: &Script, empty: bool) {
    script.route(
        "GET",
        TEMPO_TRACE,
        200,
        if empty {
            r#"{"trace":{}}"#.to_string()
        } else {
            serde_json::json!({"resourceSpans":[{"scopeSpans":[{"spans":[{
                "traceId":TRACE_ID,"spanId":"0123456789abcdef","name":"request",
                "startTimeUnixNano":"1000","endTimeUnixNano":"2000"
            }]}]}]})
            .to_string()
        },
    );
}

fn verified_tempo(script: &Script) {
    script.route_accepting(
        "GET", TEMPO_GET, METADATA, 200,
        r#"{"metadata":{"name":"tempo","namespace":"observe","uid":"tempo-uid","resourceVersion":"10"}}"#,
    );
}

async fn trace_reply(reader: &Reader, legacy: bool) -> Fetched<Option<traces::Trace>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let stop = if legacy {
        reader.lookup_trace(TRACE_ID.into(), move |answer| {
            let _ = tx.send(answer);
        });
        None
    } else {
        Some(reader.observe_trace(TRACE_ID.into(), move |answer| {
            let _ = tx.send(answer);
        }))
    };
    let reply = rx.await.expect("trace callback");
    drop(stop);
    reply
}

fn assert_trace_gets(rows: &[Seen], expected: &[(&str, &str)]) {
    assert_eq!(rows.len(), expected.len(), "{rows:?}");
    for (row, (path, accept)) in rows.iter().zip(expected) {
        assert_eq!(
            row.path,
            if *path == SOURCE_LIST {
                "/api/v1/services?&limit=200"
            } else {
                *path
            }
        );
        assert_get(row, accept);
    }
}

#[tokio::test(start_paused = true)]
async fn native_probe_and_cached_query_keep_unusable_and_ambiguous_sources_visible() {
    let cases: [(Vec<serde_json::Value>, &[&str]); 3] = [
        (
            vec![serde_json::json!({
                "metadata":{"name":"prometheus","namespace":"observe"},
                "spec":{}
            })],
            &["Prometheus Service observe/prometheus has no usable TCP port"],
        ),
        (
            vec![serde_json::json!({
                "metadata":{"name":"prometheus","namespace":"observe"},
                "spec":{"ports":[{"port":9090,"protocol":"UDP"}]}
            })],
            &["Prometheus Service observe/prometheus has no usable TCP port"],
        ),
        (
            vec![
                service("prometheus", "source-uid", "10", 9090),
                serde_json::json!({
                    "metadata":{"name":"other-prometheus","namespace":"observe"},
                    "spec":{"ports":[{"port":9090,"protocol":"UDP"}]}
                }),
            ],
            &[
                "Prometheus matches 2 Services",
                "Prometheus Service observe/other-prometheus has no usable TCP port",
                "without usable TCP ports: 1",
            ],
        ),
    ];
    for (services, expected) in cases {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        listed(&script, services);
        let before = script.seen().len();

        let tools = probe(&sync.reader).await;
        assert_eq!(tools.prometheus, k10s_data::read::Seen::Unbound);
        assert_eq!(tools.errors.len(), 1, "{tools:?}");
        let why = failure(query(&sync.reader, "up").await.0);
        for &part in expected {
            assert!(tools.errors[0].contains(part), "{tools:?}");
            assert!(why.contains(part), "{why}");
        }
        assert_trace_gets(&after_sync(&script, before), &[(SOURCE_LIST, "")]);
    }
}

#[tokio::test(start_paused = true)]
async fn healthy_tempo_and_empty_success_keep_the_actual_source_without_jaeger_reads() {
    for empty in [false, true] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        healthy_jaeger(&script);
        listed(&script, trace_services());
        script.route("GET", TEMPO_READY, 200, "ready");
        verified_tempo(&script);
        let before = script.seen().len();
        for _ in 0..2 {
            tempo_trace(&script, empty);
            let answer = trace_reply(&sync.reader, false).await;
            let Fetched::Ok(Some(trace)) = answer else {
                panic!("{answer:?}");
            };
            assert_eq!(trace.source, "Tempo / observe/tempo:3200");
            assert_eq!(trace.spans.is_empty(), empty);
            assert_eq!(trace.trace_id, if empty { "" } else { TRACE_ID });
        }
        let rows = after_sync(&script, before);
        assert_trace_gets(
            &rows,
            &[
                (SOURCE_LIST, ""),
                (TEMPO_READY, "application/json"),
                (TEMPO_TRACE, "application/json"),
                (TEMPO_GET, METADATA),
                (TEMPO_TRACE, "application/json"),
            ],
        );
    }
}

#[tokio::test(start_paused = true)]
async fn absent_tempo_still_uses_the_verified_jaeger_source() {
    for legacy in [false, true] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        healthy_jaeger(&script);
        listed(&script, vec![service("jaeger", "jaeger-uid", "20", 16686)]);
        if legacy {
            listed(&script, vec![service("jaeger", "jaeger-uid", "20", 16686)]);
        }
        let before = script.seen().len();
        let answer = trace_reply(&sync.reader, legacy).await;
        let Fetched::Ok(Some(trace)) = answer else {
            panic!("{answer:?}");
        };
        assert_eq!(trace.source, "Jaeger / observe/jaeger:16686");
        assert_eq!(trace.trace_id, TRACE_ID);
        assert_eq!(trace.spans.len(), 1);
        let rows = after_sync(&script, before);
        if legacy {
            assert_trace_gets(
                &rows,
                &[
                    (SOURCE_LIST, ""),
                    (SOURCE_LIST, ""),
                    (JAEGER_READY, "application/json"),
                    (JAEGER_TRACE, "application/json"),
                ],
            );
        } else {
            assert_trace_gets(
                &rows,
                &[
                    (SOURCE_LIST, ""),
                    (JAEGER_GET, METADATA),
                    (JAEGER_READY, "application/json"),
                    (JAEGER_TRACE, "application/json"),
                ],
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn an_expired_known_tempo_disappearing_cannot_fall_back_before_refresh() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    healthy_jaeger(&script);
    listed(&script, trace_services());
    script.route("GET", TEMPO_READY, 200, "ready");
    tempo_trace(&script, false);
    assert!(matches!(
        trace_reply(&sync.reader, false).await,
        Fetched::Ok(Some(trace)) if trace.source == "Tempo / observe/tempo:3200"
    ));

    tokio::time::advance(Duration::from_secs(30)).await;
    verified_tempo(&script);
    listed(&script, vec![service("jaeger", "jaeger-uid", "20", 16686)]);
    let before = script.seen().len();
    let Fetched::Failed { what, why } = trace_reply(&sync.reader, false).await else {
        panic!("a formerly verified Tempo must not be treated as initial absence");
    };
    assert_eq!(what, "tempo");
    assert!(why.contains("refresh sources"), "{why}");
    assert_trace_gets(
        &after_sync(&script, before),
        &[(TEMPO_GET, METADATA), (SOURCE_LIST, "")],
    );

    let before = script.seen().len();
    assert!(matches!(
        trace_reply(&sync.reader, false).await,
        Fetched::Failed { what: "tempo", .. }
    ));
    assert!(after_sync(&script, before).is_empty());

    listed(&script, vec![service("jaeger", "jaeger-uid", "20", 16686)]);
    let before = script.seen().len();
    assert_eq!(
        probe(&sync.reader).await.traces,
        k10s_data::read::Seen::Bound
    );
    assert!(matches!(
        trace_reply(&sync.reader, false).await,
        Fetched::Ok(Some(trace)) if trace.source == "Jaeger / observe/jaeger:16686"
    ));
    assert_trace_gets(
        &after_sync(&script, before),
        &[
            (SOURCE_LIST, ""),
            (JAEGER_READY, "application/json"),
            (JAEGER_GET, METADATA),
            (JAEGER_TRACE, "application/json"),
        ],
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_tempo_binding_cannot_be_hidden_by_a_healthy_jaeger() {
    for legacy in [false, true] {
        for status in [0, 403, 500] {
            let script = Script::default();
            let (sync, _events) = connected(&script).await;
            healthy_jaeger(&script);
            let mut services = trace_services();
            if status == 0 {
                services.push(service("other-tempo", "other-uid", "30", 3200));
            } else {
                script.route(
                    "GET",
                    TEMPO_READY,
                    status,
                    r#"{"message":"Tempo storage is unavailable"}"#,
                );
            }
            listed(&script, services);
            let before = script.seen().len();
            let answer = trace_reply(&sync.reader, legacy).await;
            let Fetched::Failed { what, why } = answer else {
                panic!("{answer:?}");
            };
            assert_eq!(what, "tempo");
            if status == 500 {
                assert_eq!(why, "Tempo storage is unavailable");
            }
            if status == 403 {
                assert!(
                    why.starts_with("Tempo proxy: tempo access denied for this account"),
                    "{why}"
                );
            }
            if status == 0 {
                assert!(why.contains("matches 2 Services"), "{why}");
            }
            let expected = if status == 0 {
                vec![(SOURCE_LIST, "")]
            } else {
                vec![(SOURCE_LIST, ""), (TEMPO_READY, "application/json")]
            };
            assert_trace_gets(&after_sync(&script, before), &expected);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn tempo_ambiguity_and_probe_failures_never_probe_the_healthy_jaeger() {
    for legacy in [false, true] {
        for status in [0, 403, 500] {
            let script = Script::default();
            let (sync, _events) = connected(&script).await;
            healthy_jaeger(&script);
            let mut services = trace_services();
            if status == 0 {
                services.push(service("other-tempo", "other-uid", "30", 3200));
            } else {
                script.route(
                    "GET",
                    TEMPO_READY,
                    status,
                    r#"{"message":"Tempo storage is unavailable"}"#,
                );
            }
            listed(&script, services);
            let before = script.seen().len();
            let tools = if legacy {
                let (tx, rx) = tokio::sync::oneshot::channel();
                sync.reader.probe_observe_tools(move |tools| {
                    let _ = tx.send(tools);
                });
                rx.await.expect("legacy source probe")
            } else {
                probe(&sync.reader).await
            };
            assert_eq!(tools.traces, k10s_data::read::Seen::Unbound, "{tools:?}");
            assert_eq!(tools.errors.len(), 1, "{tools:?}");
            assert!(tools.errors[0].starts_with("Tempo:"), "{tools:?}");
            if status == 0 {
                assert!(tools.errors[0].contains("matches 2 Services"));
            }
            if status == 500 {
                assert!(tools.errors[0].contains("Tempo storage is unavailable"));
            }
            if status == 403 {
                assert!(tools.errors[0].contains("tempo access denied for this account"));
            }
            let rows = after_sync(&script, before);
            let expected = if status == 0 {
                vec![(SOURCE_LIST, "")]
            } else {
                vec![(SOURCE_LIST, ""), (TEMPO_READY, "application/json")]
            };
            assert_trace_gets(&rows, &expected);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_changed_tempo_uid_or_metadata_failure_stops_before_any_jaeger_read() {
    for status in [200, 403, 500] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        healthy_jaeger(&script);
        listed(&script, trace_services());
        script.route("GET", TEMPO_READY, 200, "ready");
        tempo_trace(&script, false);
        let warm = trace_reply(&sync.reader, false).await;
        assert!(
            matches!(
                &warm,
                Fetched::Ok(Some(trace)) if trace.source == "Tempo / observe/tempo:3200"
            ),
            "warm Tempo lookup: {warm:?}"
        );
        let before = script.seen().len();
        if status == 200 {
            script.route_accepting("GET", TEMPO_GET, METADATA, 200,
                r#"{"metadata":{"name":"tempo","namespace":"observe","uid":"replacement-uid","resourceVersion":"11"}}"#);
        } else {
            script.route_accepting(
                "GET",
                TEMPO_GET,
                METADATA,
                status,
                r#"{"message":"Tempo Service cannot be read"}"#,
            );
            if status == 403 {
                script.route(
                    "GET",
                    SOURCE_LIST,
                    403,
                    r#"{"message":"services are forbidden"}"#,
                );
            }
        }
        let answer = trace_reply(&sync.reader, false).await;
        let Fetched::Failed { what, why } = answer else {
            panic!("{answer:?}");
        };
        assert_eq!(what, "tempo");
        if status == 200 {
            assert_eq!(
                why,
                "Tempo source Service observe/tempo was recreated or changed; refresh sources before querying"
            );
        }
        if status == 500 {
            assert_eq!(why, "Tempo Service cannot be read");
        }
        if status == 403 {
            assert_eq!(why, "services: access denied for this account");
        }
        let rows = after_sync(&script, before);
        let expected = if status == 403 {
            vec![(TEMPO_GET, METADATA), (SOURCE_LIST, "")]
        } else {
            vec![(TEMPO_GET, METADATA)]
        };
        assert_trace_gets(&rows, &expected);
    }
}

#[tokio::test(start_paused = true)]
async fn tempo_query_errors_keep_their_state_and_get_denial_keeps_list_authority() {
    for status in [0, 403, 500] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        healthy_jaeger(&script);
        listed(&script, trace_services());
        script.route("GET", TEMPO_READY, 200, "ready");
        tempo_trace(&script, false);
        let warm = trace_reply(&sync.reader, false).await;
        assert!(
            matches!(
                &warm,
                Fetched::Ok(Some(trace)) if trace.source == "Tempo / observe/tempo:3200"
            ),
            "warm Tempo lookup: {warm:?}"
        );
        let before = script.seen().len();
        if status == 0 {
            script.route_accepting(
                "GET",
                TEMPO_GET,
                METADATA,
                403,
                r#"{"message":"get is not granted"}"#,
            );
            listed(&script, trace_services());
            script.route("GET", TEMPO_READY, 200, "ready");
            tempo_trace(&script, false);
        } else {
            verified_tempo(&script);
            script.route(
                "GET",
                TEMPO_TRACE,
                status,
                r#"{"message":"trace storage is unavailable"}"#,
            );
        }
        let answer = trace_reply(&sync.reader, false).await;
        if status == 0 {
            let Fetched::Ok(Some(trace)) = answer else {
                panic!("authorized LIST and proxy access remain usable: {answer:?}");
            };
            assert_eq!(trace.source, "Tempo / observe/tempo:3200");
            assert_trace_gets(
                &after_sync(&script, before),
                &[
                    (TEMPO_GET, METADATA),
                    (SOURCE_LIST, ""),
                    (TEMPO_READY, "application/json"),
                    (TEMPO_TRACE, "application/json"),
                ],
            );
        } else {
            assert_eq!(
                answer,
                if status == 403 {
                    Fetched::Denied { what: "tempo" }
                } else {
                    Fetched::Failed {
                        what: "tempo",
                        why: "trace storage is unavailable".into(),
                    }
                }
            );
            assert_trace_gets(
                &after_sync(&script, before),
                &[(TEMPO_GET, METADATA), (TEMPO_TRACE, "application/json")],
            );
        }
    }
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
            assert!(tools.errors.iter().any(|why| why.contains("2 Services")));
        } else {
            tokio::time::advance(Duration::from_secs(30)).await;
            verified(&script, "source-uid", "10");
        }
        assert!(failure(query(&sync.reader, "up").await.0).contains("2 Services"));
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
async fn a_query_error_keeps_the_verified_source_without_extra_discovery_or_probes() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    discovered(&script, "source-a", "10");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));

    verified(&script, "source-a", "10");
    script.route("GET", QUERY, 500, r#"{"message":"query unavailable"}"#);
    let before = script.seen().len();
    assert_eq!(
        failure(query(&sync.reader, "up").await.0),
        "query unavailable"
    );
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_trace_gets(&rows[..1], &[(SOURCE_GET, METADATA)]);
    assert_query(&rows[1], "up");

    verified(&script, "source-a", "10");
    script.route("GET", QUERY, 200, RESULT);
    let before = script.seen().len();
    let (answer, metrics) = query(&sync.reader, "up").await;
    assert!(matches!(answer, Fetched::Ok(Some(_))));
    assert_eq!(metrics.requests, 2);
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_trace_gets(&rows[..1], &[(SOURCE_GET, METADATA)]);
    assert_query(&rows[1], "up");
}

#[tokio::test(start_paused = true)]
async fn query_and_metadata_errors_cannot_forget_a_known_source_before_retry() {
    for metadata_failure in [false, true] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        discovered(&script, "source-a", "10");
        script.route("GET", QUERY, 200, RESULT);
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));

        let before = script.seen().len();
        if metadata_failure {
            script.route_accepting(
                "GET",
                SOURCE_GET,
                METADATA,
                500,
                r#"{"message":"Service metadata unavailable"}"#,
            );
        } else {
            verified(&script, "source-a", "10");
            script.route("GET", QUERY, 500, r#"{"message":"query unavailable"}"#);
        }
        let why = failure(query(&sync.reader, "up").await.0);
        assert_eq!(
            why,
            if metadata_failure {
                "Service metadata unavailable"
            } else {
                "query unavailable"
            }
        );
        let rows = after_sync(&script, before);
        assert_eq!(rows.len(), if metadata_failure { 1 } else { 2 });
        assert_eq!(rows[0].path, SOURCE_GET);
        assert_get(&rows[0], METADATA);
        if !metadata_failure {
            assert_query(&rows[1], "up");
        }

        verified(&script, "source-b", "20");
        discovered(&script, "source-b", "20");
        script.route("GET", QUERY, 200, RESULT);
        let before = script.seen().len();
        let answer = query(&sync.reader, "up").await.0;
        assert_trace_gets(&after_sync(&script, before), &[(SOURCE_GET, METADATA)]);
        assert!(failure(answer).contains("refresh sources"));

        let before = script.seen().len();
        assert_eq!(
            probe(&sync.reader).await.prometheus,
            k10s_data::read::Seen::Bound
        );
        verified(&script, "source-b", "20");
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));
        let rows = after_sync(&script, before);
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_trace_gets(
            &rows[..3],
            &[
                (SOURCE_LIST, ""),
                (SOURCE_PROBE, "application/json"),
                (SOURCE_GET, METADATA),
            ],
        );
        assert_query(&rows[3], "up");
    }
}

#[tokio::test(start_paused = true)]
async fn missing_or_ambiguous_expiry_cannot_forget_a_known_source_before_retry() {
    for ambiguous in [false, true] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        discovered(&script, "source-a", "10");
        script.route("GET", QUERY, 200, RESULT);
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));

        tokio::time::advance(Duration::from_secs(30)).await;
        verified(&script, "source-a", "10");
        listed(
            &script,
            if ambiguous {
                vec![
                    service("prometheus", "source-a", "10", 9090),
                    service("other-prometheus", "competing", "30", 9090),
                ]
            } else {
                Vec::new()
            },
        );
        let before = script.seen().len();
        let answer = query(&sync.reader, "up").await.0;
        let why = failure(answer);
        assert!(why.contains("refresh sources"), "{why}");
        if ambiguous {
            assert!(why.contains("2 Services"), "{why}");
        }
        assert_trace_gets(
            &after_sync(&script, before),
            &[(SOURCE_GET, METADATA), (SOURCE_LIST, "")],
        );

        discovered(&script, "source-b", "20");
        script.route("GET", QUERY, 200, RESULT);
        tokio::time::advance(Duration::from_secs(30)).await;
        let before = script.seen().len();
        let answer = query(&sync.reader, "up").await.0;
        assert!(
            after_sync(&script, before).is_empty(),
            "an unresolved source must require an explicit refresh"
        );
        assert!(failure(answer).contains("refresh sources"));

        let before = script.seen().len();
        assert_eq!(
            probe(&sync.reader).await.prometheus,
            k10s_data::read::Seen::Bound
        );
        verified(&script, "source-b", "20");
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));
        let rows = after_sync(&script, before);
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_trace_gets(
            &rows[..3],
            &[
                (SOURCE_LIST, ""),
                (SOURCE_PROBE, "application/json"),
                (SOURCE_GET, METADATA),
            ],
        );
        assert_query(&rows[3], "up");
    }
}

#[tokio::test(start_paused = true)]
async fn failed_discovery_refresh_cannot_forget_a_known_source_before_retry() {
    for explicit in [false, true] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        discovered(&script, "source-a", "10");
        script.route("GET", QUERY, 200, RESULT);
        assert!(matches!(
            query(&sync.reader, "up").await.0,
            Fetched::Ok(Some(_))
        ));

        if !explicit {
            tokio::time::advance(Duration::from_secs(30)).await;
            verified(&script, "source-a", "10");
        }
        script.route(
            "GET",
            SOURCE_LIST,
            500,
            r#"{"message":"Service discovery unavailable"}"#,
        );
        let before = script.seen().len();
        if explicit {
            let tools = probe(&sync.reader).await;
            assert_eq!(tools.prometheus, k10s_data::read::Seen::Unbound);
            assert!(
                tools
                    .errors
                    .iter()
                    .all(|why| why.contains("Service discovery unavailable"))
            );
        } else {
            assert_eq!(
                failure(query(&sync.reader, "up").await.0),
                "Service discovery unavailable"
            );
        }
        let rows = after_sync(&script, before);
        if explicit {
            assert_trace_gets(&rows, &[(SOURCE_LIST, "")]);
        } else {
            assert_trace_gets(&rows, &[(SOURCE_GET, METADATA), (SOURCE_LIST, "")]);
        }

        verified(&script, "source-b", "20");
        discovered(&script, "source-b", "20");
        script.route("GET", QUERY, 200, RESULT);
        let before = script.seen().len();
        let answer = query(&sync.reader, "up").await.0;
        assert_trace_gets(&after_sync(&script, before), &[(SOURCE_GET, METADATA)]);
        assert!(failure(answer).contains("refresh sources"));
    }
}

#[tokio::test(start_paused = true)]
async fn a_grafana_panel_error_cannot_forget_another_kinds_known_source() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    listed(
        &script,
        vec![
            service("prometheus", "prometheus-a", "10", 9090),
            service("grafana", "grafana-a", "20", 3000),
        ],
    );
    script.route("GET", SOURCE_PROBE, 200, "ready");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));

    verified_grafana(&script, "grafana-a", "20");
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    selected_catalog_rows(&script);
    let catalog = selected_catalog(&sync.reader).await;
    let target = catalog.dashboards[0].panels[0].queries[0].clone();
    verified_grafana(&script, "grafana-a", "20");
    script.route("GET", GRAFANA_DATASOURCE, 200, GRAFANA_DATASOURCE_JSON);
    script.route(
        "GET",
        GRAFANA_RANGE,
        500,
        r#"{"message":"Grafana query unavailable"}"#,
    );
    let before = script.seen().len();
    let why = failure(selected_query(&sync.reader, catalog.query_source.clone(), target).await);
    assert!(why.contains("Grafana query unavailable"), "{why}");
    assert_trace_gets(
        &after_sync(&script, before),
        &[
            (GRAFANA_IDENTITY, METADATA),
            (GRAFANA_DATASOURCE, "application/json"),
            (GRAFANA_RANGE, "application/json"),
        ],
    );

    verified(&script, "prometheus-b", "30");
    discovered(&script, "prometheus-b", "30");
    script.route("GET", QUERY, 200, RESULT);
    let before = script.seen().len();
    let answer = query(&sync.reader, "up").await.0;
    assert_trace_gets(&after_sync(&script, before), &[(SOURCE_GET, METADATA)]);
    assert!(failure(answer).contains("refresh sources"));
    assert!(
        sync.reader.grafana_source_is_current(
            catalog
                .query_source
                .as_deref()
                .expect("verified Grafana origin")
        )
    );
}

async fn selected_query(
    reader: &Reader,
    source: Option<Arc<ServiceIdentity>>,
    target: grafana::PanelQuery,
) -> Fetched<Option<prom::QueryResult>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _stop = reader.observe_panel(
        source,
        target,
        grafana::Variables {
            namespace: "prod".into(),
            pods: vec!["api".into()],
            node: None,
            start_ms: 1000,
            end_ms: 3000,
            step_ms: 1000,
            min_interval: String::new(),
        },
        move |answer| {
            tx.send(answer).expect("selected panel reply");
        },
    );
    rx.await.expect("selected panel callback")
}

const GRAFANA_IDENTITY: &str = "/api/v1/namespaces/observe/services/grafana";
const GRAFANA_PROXY: &str = "/api/v1/namespaces/observe/services/grafana:3000/proxy/";
const GRAFANA_DATASOURCE: &str =
    "/api/v1/namespaces/observe/services/grafana:3000/proxy/api/datasources/uid/prom-main";
const GRAFANA_RANGE: &str = "/api/v1/namespaces/observe/services/grafana:3000/proxy/api/datasources/proxy/uid/prom-main/api/v1/query_range?query=up&start=1&end=3&step=1s&limit=256";
const GRAFANA_DATASOURCE_JSON: &str =
    r#"{"uid":"prom-main","type":"prometheus","jsonData":{"timeInterval":"1s"}}"#;

fn selected_catalog_rows(script: &Script) {
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        r#"{"apiVersion":"v1","kind":"ConfigMapList","items":[]}"#,
    );
    script.route(
        "GET",
        &format!("{GRAFANA_PROXY}api/search?"),
        200,
        r#"[{"uid":"selected","title":"Selected","type":"dash-db"}]"#,
    );
    script.route(
        "GET",
        &format!("{GRAFANA_PROXY}api/dashboards/uid/selected"),
        200,
        r#"{"dashboard":{"uid":"selected","title":"Selected","panels":[{"id":1,"title":"Up","type":"timeseries","datasource":{"type":"prometheus","uid":"prom-main"},"targets":[{"refId":"A","expr":"up"}]}]}}"#,
    );
}

fn verified_grafana(script: &Script, uid: &str, version: &str) {
    script.route_accepting(
        "GET", GRAFANA_IDENTITY, METADATA, 200,
        serde_json::json!({"metadata":{"name":"grafana","namespace":"observe","uid":uid,"resourceVersion":version}}).to_string(),
    );
}

async fn selected_catalog(reader: &Reader) -> k10s_data::read::GrafanaCatalog {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _stop = reader.observe_grafana(move |answer| {
        tx.send(answer).expect("selected catalog reply");
    });
    match rx.await.expect("selected catalog callback") {
        Fetched::Ok(catalog) => catalog,
        other => panic!("the scripted Grafana catalog must answer: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn selected_grafana_panel_cannot_rebind_after_source_changes_without_refresh() {
    const GRAFANA_GET: &str = "/api/v1/namespaces/observe/services/grafana";
    const PROXY: &str = "/api/v1/namespaces/observe/services/grafana:3000/proxy/";

    for (uid, version) in [("replacement-uid", "10"), ("source-uid", "11")] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        script.route(
            "GET",
            "/api/v1/configmaps?",
            200,
            r#"{"apiVersion":"v1","kind":"ConfigMapList","items":[]}"#,
        );
        listed(&script, vec![service("grafana", "source-uid", "10", 3000)]);
        script.route("GET", &format!("{PROXY}api/health"), 200, "ready");
        script.route(
            "GET",
            &format!("{PROXY}api/search?"),
            200,
            r#"[{"uid":"selected","title":"Selected","type":"dash-db"}]"#,
        );
        script.route(
            "GET",
            &format!("{PROXY}api/dashboards/uid/selected"),
            200,
            r#"{"dashboard":{"uid":"selected","title":"Selected","panels":[{"id":1,"title":"Up","type":"timeseries","datasource":{"type":"prometheus","uid":"prom-main"},"targets":[{"refId":"A","expr":"up"}]}]}}"#,
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        let stop = sync.reader.observe_grafana(move |answer| {
            tx.send(answer).expect("selected catalog reply");
        });
        let Fetched::Ok(catalog) = rx.await.expect("selected catalog callback") else {
            panic!("the original Grafana catalog is readable");
        };
        drop(stop);
        let dashboard = &catalog.dashboards[0];
        assert_eq!(dashboard.source, grafana::DashboardSource::Grafana);
        let selected = dashboard.panels[0].queries[0].clone();

        let datasource = format!("{PROXY}api/datasources/uid/prom-main");
        let range = format!(
            "{PROXY}api/datasources/proxy/uid/prom-main/api/v1/query_range?query=up&start=1&end=3&step=1s&limit=256"
        );
        script.route_accepting(
            "GET", GRAFANA_GET, METADATA, 200,
            r#"{"metadata":{"name":"grafana","namespace":"observe","uid":"source-uid","resourceVersion":"10"}}"#,
        );
        script.route(
            "GET",
            &datasource,
            200,
            r#"{"uid":"prom-main","type":"prometheus","jsonData":{"timeInterval":"1s"}}"#,
        );
        script.route("GET", &range, 200, RESULT);
        let before = script.seen().len();
        assert!(matches!(
            selected_query(&sync.reader, catalog.query_source.clone(), selected.clone()).await,
            Fetched::Ok(Some(_))
        ));
        assert_trace_gets(
            &after_sync(&script, before),
            &[
                (GRAFANA_GET, METADATA),
                (&datasource, "application/json"),
                (&range, "application/json"),
            ],
        );

        script.route_accepting(
            "GET", GRAFANA_GET, METADATA, 200,
            serde_json::json!({"metadata":{"name":"grafana","namespace":"observe","uid":uid,"resourceVersion":version}}).to_string(),
        );
        let before = script.seen().len();
        let why = failure(
            selected_query(&sync.reader, catalog.query_source.clone(), selected.clone()).await,
        );
        assert!(why.contains("Grafana source Service observe/grafana was recreated or changed"));
        assert_trace_gets(&after_sync(&script, before), &[(GRAFANA_GET, METADATA)]);

        listed(&script, vec![service("grafana", uid, version, 3000)]);
        script.route("GET", &format!("{PROXY}api/health"), 200, "ready");
        script.route(
            "GET",
            &datasource,
            200,
            r#"{"uid":"prom-main","type":"prometheus","jsonData":{"timeInterval":"1s"}}"#,
        );
        script.route("GET", &range, 200, RESULT);
        let before = script.seen().len();
        let retry = selected_query(&sync.reader, catalog.query_source.clone(), selected).await;
        let rows = after_sync(&script, before);
        assert!(
            rows.is_empty(),
            "an unchanged selected panel contacted the replacement without a source refresh: {rows:?}"
        );
        assert!(failure(retry).contains("refresh sources"));
    }
}

#[tokio::test(start_paused = true)]
async fn refreshing_sources_keeps_old_catalogue_rows_refused_without_invalidating_current_rows() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    listed(&script, vec![service("grafana", "source-a", "10", 3000)]);
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    selected_catalog_rows(&script);
    let old = selected_catalog(&sync.reader).await;
    let old_source = old
        .query_source
        .clone()
        .expect("the exact original binding");
    let old_target = old.dashboards[0].panels[0].queries[0].clone();
    assert!(sync.reader.grafana_source_is_current(&old_source));

    listed(&script, vec![service("grafana", "source-b", "20", 3000)]);
    let before = script.seen().len();
    let _ = probe(&sync.reader).await;
    assert_trace_gets(&after_sync(&script, before), &[(SOURCE_LIST, "")]);
    assert!(!sync.reader.grafana_source_is_current(&old_source));
    let before = script.seen().len();
    let why =
        failure(selected_query(&sync.reader, Some(old_source.clone()), old_target.clone()).await);
    assert!(why.contains("no longer matches this catalogue"));
    assert!(after_sync(&script, before).is_empty());

    verified_grafana(&script, "source-b", "20");
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    selected_catalog_rows(&script);
    let current = selected_catalog(&sync.reader).await;
    let current_source = current
        .query_source
        .clone()
        .expect("the refreshed exact binding");
    assert_ne!(old_source, current_source);
    assert_eq!(old_target, current.dashboards[0].panels[0].queries[0]);
    assert!(sync.reader.grafana_source_is_current(&current_source));

    let before = script.seen().len();
    assert!(
        failure(selected_query(&sync.reader, Some(old_source), old_target).await)
            .contains("no longer matches this catalogue")
    );
    assert!(after_sync(&script, before).is_empty());
    assert!(sync.reader.grafana_source_is_current(&current_source));

    verified_grafana(&script, "source-b", "20");
    script.route("GET", GRAFANA_DATASOURCE, 200, GRAFANA_DATASOURCE_JSON);
    script.route("GET", GRAFANA_RANGE, 200, RESULT);
    let before = script.seen().len();
    assert!(matches!(
        selected_query(
            &sync.reader,
            Some(current_source),
            current.dashboards[0].panels[0].queries[0].clone(),
        )
        .await,
        Fetched::Ok(Some(_))
    ));
    assert_trace_gets(
        &after_sync(&script, before),
        &[
            (GRAFANA_IDENTITY, METADATA),
            (GRAFANA_DATASOURCE, "application/json"),
            (GRAFANA_RANGE, "application/json"),
        ],
    );
}

#[tokio::test(start_paused = true)]
async fn an_older_grafana_query_failure_cannot_invalidate_a_refreshed_source() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    listed(&script, vec![service("grafana", "source-a", "10", 3000)]);
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    selected_catalog_rows(&script);
    let old = selected_catalog(&sync.reader).await;
    let old_source = old
        .query_source
        .clone()
        .expect("the exact original binding");
    let old_target = old.dashboards[0].panels[0].queries[0].clone();
    verified_grafana(&script, "source-a", "10");
    script.route("GET", GRAFANA_DATASOURCE, 200, GRAFANA_DATASOURCE_JSON);
    script.route_delayed(
        "GET",
        GRAFANA_RANGE,
        500,
        r#"{"message":"old Grafana query unavailable"}"#,
        Duration::from_secs(2),
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pending = sync.reader.observe_panel(
        Some(old_source),
        old_target,
        grafana::Variables {
            namespace: "prod".into(),
            pods: vec!["api".into()],
            node: None,
            start_ms: 1000,
            end_ms: 3000,
            step_ms: 1000,
            min_interval: String::new(),
        },
        move |answer| {
            tx.send(answer).expect("old query reply");
        },
    );
    while script.answered("GET", GRAFANA_RANGE).1 == 0 {
        tokio::task::yield_now().await;
    }

    listed(&script, vec![service("grafana", "source-b", "20", 3000)]);
    let _ = probe(&sync.reader).await;
    verified_grafana(&script, "source-b", "20");
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    selected_catalog_rows(&script);
    let current = selected_catalog(&sync.reader).await;
    let current_source = current
        .query_source
        .clone()
        .expect("the refreshed exact binding");
    assert!(sync.reader.grafana_source_is_current(&current_source));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        failure(rx.await.expect("old query callback")).contains("old Grafana query unavailable")
    );
    drop(pending);
    assert!(sync.reader.grafana_source_is_current(&current_source));

    verified_grafana(&script, "source-b", "20");
    script.route("GET", GRAFANA_DATASOURCE, 200, GRAFANA_DATASOURCE_JSON);
    script.route("GET", GRAFANA_RANGE, 200, RESULT);
    let before = script.seen().len();
    assert!(matches!(
        selected_query(
            &sync.reader,
            Some(current_source),
            current.dashboards[0].panels[0].queries[0].clone(),
        )
        .await,
        Fetched::Ok(Some(_))
    ));
    assert_trace_gets(
        &after_sync(&script, before),
        &[
            (GRAFANA_IDENTITY, METADATA),
            (GRAFANA_DATASOURCE, "application/json"),
            (GRAFANA_RANGE, "application/json"),
        ],
    );
}

#[tokio::test(start_paused = true)]
async fn catalogue_source_is_captured_before_its_dashboard_read_can_be_overtaken() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    listed(&script, vec![service("grafana", "source-a", "10", 3000)]);
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        r#"{"apiVersion":"v1","kind":"ConfigMapList","items":[]}"#,
    );
    script.route(
        "GET",
        &format!("{GRAFANA_PROXY}api/search?"),
        200,
        r#"[{"uid":"selected","title":"Selected","type":"dash-db"}]"#,
    );
    let dashboard = format!("{GRAFANA_PROXY}api/dashboards/uid/selected");
    script.route_delayed(
        "GET",
        &dashboard,
        200,
        r#"{"dashboard":{"uid":"selected","title":"Selected","panels":[]}}"#,
        Duration::from_secs(2),
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pending = sync.reader.observe_grafana(move |answer| {
        tx.send(answer).expect("older catalogue reply");
    });
    while script.answered("GET", &dashboard).1 == 0 {
        tokio::task::yield_now().await;
    }

    listed(&script, vec![service("grafana", "source-b", "20", 3000)]);
    let _ = probe(&sync.reader).await;
    verified_grafana(&script, "source-b", "20");
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    selected_catalog_rows(&script);
    let current = selected_catalog(&sync.reader).await;
    let current_source = current
        .query_source
        .expect("the replacement catalogue source");
    assert_eq!(current_source.uid, "source-b");

    tokio::time::advance(Duration::from_secs(2)).await;
    let Fetched::Ok(old) = rx.await.expect("older catalogue callback") else {
        panic!("the delayed original catalogue is still readable");
    };
    drop(pending);
    let old_source = old.query_source.expect("the original catalogue source");
    assert_eq!(old_source.uid, "source-a");
    assert_eq!(old_source.resource_version, "10");
    assert_ne!(old_source, current_source);
    assert!(!sync.reader.grafana_source_is_current(&old_source));
    assert!(sync.reader.grafana_source_is_current(&current_source));
}

#[tokio::test(start_paused = true)]
async fn unpinned_configmap_panels_stay_readable_without_blocking_direct_prometheus() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    script.route("GET", "/api/v1/configmaps?", 200,
        serde_json::json!({"apiVersion":"v1","kind":"ConfigMapList","items":[{
            "metadata":{"name":"dashboards","namespace":"observe"},
            "data":{"selected.json":r#"{"uid":"selected","title":"Selected","panels":[{"id":1,"title":"Up","type":"timeseries","datasource":{"type":"prometheus","uid":"prom-main"},"targets":[{"refId":"A","expr":"up"}]}]}"#}
        }]}).to_string());
    discovered(&script, "prometheus-uid", "10");
    let catalog = selected_catalog(&sync.reader).await;
    assert!(catalog.served);
    assert!(catalog.query_source.is_none());
    assert_eq!(
        catalog.dashboards[0].source,
        grafana::DashboardSource::ConfigMap
    );
    let target = catalog.dashboards[0].panels[0].queries[0].clone();
    let before = script.seen().len();
    let why = failure(selected_query(&sync.reader, None, target).await);
    assert!(why.contains("no verified Grafana Service identity"));
    assert!(after_sync(&script, before).is_empty());

    verified(&script, "prometheus-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    let before = script.seen().len();
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));
    assert_trace_gets(
        &after_sync(&script, before),
        &[
            (SOURCE_GET, METADATA),
            (SOURCE_PROBE, "application/json"),
            (
                &format!("{QUERY}query=up&start=1&end=3&step=1s&limit=256"),
                "application/json",
            ),
        ],
    );
}

#[tokio::test(start_paused = true)]
async fn an_automatic_service_hint_cannot_bypass_the_pinned_browser_route() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({"apiVersion":"v1","kind":"ConfigMapList","items":[{
            "metadata":{"name":"dashboards","namespace":"observe"},
            "data":{"selected.json":r#"{"uid":"selected","title":"Selected","panels":[]}"#}
        }]})
        .to_string(),
    );
    listed(&script, vec![service("grafana", "source-a", "10", 3000)]);
    script.route(
        "GET",
        &format!("{GRAFANA_PROXY}api/health"),
        403,
        r#"{"message":"Grafana proxy denied"}"#,
    );
    let before = script.seen().len();
    let catalog = selected_catalog(&sync.reader).await;
    assert_eq!(
        catalog.dashboards[0].source,
        grafana::DashboardSource::ConfigMap
    );
    assert!(catalog.query_source.is_none());
    assert!(catalog.browser_service.is_none());
    assert!(catalog.browser_base.is_none());
    assert!(
        catalog
            .errors
            .iter()
            .any(|why| why.contains("access denied"))
    );
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 3);
    assert_get(&rows[0], "");
    assert!(rows[0].path.starts_with("/api/v1/configmaps?"));
    assert_trace_gets(
        &rows[1..],
        &[
            (SOURCE_LIST, ""),
            (&format!("{GRAFANA_PROXY}api/health"), "application/json"),
        ],
    );
}

#[tokio::test(start_paused = true)]
async fn a_late_catalogue_failure_cannot_evict_a_newer_verified_source() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    script.route(
        "GET",
        "/api/v1/configmaps?",
        403,
        r#"{"message":"ConfigMap read denied"}"#,
    );
    listed(&script, vec![service("grafana", "source-a", "10", 3000)]);
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    let search = format!("{GRAFANA_PROXY}api/search?");
    script.route_delayed(
        "GET",
        &search,
        500,
        r#"{"message":"old Grafana search unavailable"}"#,
        Duration::from_secs(2),
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pending = sync.reader.observe_grafana(move |answer| {
        tx.send(answer).expect("old catalogue reply");
    });
    while script.answered("GET", &search).1 == 0 {
        tokio::task::yield_now().await;
    }

    listed(&script, vec![service("grafana", "source-b", "20", 3000)]);
    let _ = probe(&sync.reader).await;
    verified_grafana(&script, "source-b", "20");
    script.route("GET", &format!("{GRAFANA_PROXY}api/health"), 200, "ready");
    selected_catalog_rows(&script);
    let current = selected_catalog(&sync.reader).await;
    let current_source = current
        .query_source
        .clone()
        .expect("the verified replacement");
    tokio::time::advance(Duration::from_secs(2)).await;
    let Fetched::Failed { why, .. } = rx.await.expect("old catalogue callback") else {
        panic!("both old catalogue acquisition paths failed");
    };
    assert!(why.contains("old Grafana search unavailable"));
    drop(pending);
    assert!(sync.reader.grafana_source_is_current(&current_source));

    verified_grafana(&script, "source-b", "20");
    script.route("GET", GRAFANA_DATASOURCE, 200, GRAFANA_DATASOURCE_JSON);
    script.route("GET", GRAFANA_RANGE, 200, RESULT);
    let before = script.seen().len();
    assert!(matches!(
        selected_query(
            &sync.reader,
            Some(current_source),
            current.dashboards[0].panels[0].queries[0].clone(),
        )
        .await,
        Fetched::Ok(Some(_))
    ));
    assert_trace_gets(
        &after_sync(&script, before),
        &[
            (GRAFANA_IDENTITY, METADATA),
            (GRAFANA_DATASOURCE, "application/json"),
            (GRAFANA_RANGE, "application/json"),
        ],
    );
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
async fn source_get_failures_keep_the_server_sentence_until_an_explicit_refresh() {
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
    script.route_accepting(
        "GET",
        SOURCE_GET,
        METADATA,
        404,
        r#"{"message":"Service prometheus was deleted"}"#,
    );
    assert_eq!(
        failure(query(&sync.reader, "up").await.0),
        "Service prometheus was deleted"
    );
    listed(&script, Vec::new());
    assert_eq!(
        probe(&sync.reader).await.prometheus,
        k10s_data::read::Seen::Absent
    );
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(None)
    ));
    assert_trace_gets(
        &after_sync(&script, before),
        &[
            (SOURCE_GET, METADATA),
            (SOURCE_GET, METADATA),
            (SOURCE_LIST, ""),
        ],
    );
}

#[tokio::test(start_paused = true)]
async fn a_query_denial_retains_the_binding_and_preserves_its_resource_label() {
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
    verified(&script, "source-uid", "10");
    script.route("GET", QUERY, 200, RESULT);
    assert!(matches!(
        query(&sync.reader, "up").await.0,
        Fetched::Ok(Some(_))
    ));
    let rows = after_sync(&script, before);
    assert_eq!(rows.len(), 4);
    assert_trace_gets(&rows[..1], &[(SOURCE_GET, METADATA)]);
    assert_query(&rows[1], "up");
    assert_trace_gets(&rows[2..3], &[(SOURCE_GET, METADATA)]);
    assert_query(&rows[3], "up");
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
                    Some(Arc::new(ServiceIdentity {
                        namespace: "observe".into(),
                        name: "grafana".into(),
                        port: 3000,
                        uid: "source-uid".into(),
                        resource_version: "10".into(),
                    })),
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
