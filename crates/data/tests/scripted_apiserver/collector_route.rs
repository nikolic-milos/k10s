//! The focused Collector document retains metadata safety and request ownership.

use crate::*;
use k10s_data::browse::TablePage;
use k10s_data::read::Fetched;

const GROUP: &str = "/apis/opentelemetry.io";
const LIST: &str = "/apis/opentelemetry.io/v1beta1/opentelemetrycollectors?limit=200";
const SERVICES: &str = "/api/v1/services?limit=200";
const HEALTH: &str = "/api/v1/namespaces/observe/services/collector:13133/proxy/";
const METADATA: &str = "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1";

async fn connected(interactive: &Script) -> (Sync, EventReceiver) {
    let bootstrap = Script::default();
    script_discovery(&bootstrap);
    script_rules_review(&bootstrap);
    script_access_reviews(&bootstrap, true, 32);
    script_lists(&bootstrap);
    let (tx, rx) = crossbeam_channel::unbounded();
    let sync = sync_from(
        Clients {
            sync: bootstrap.client(),
            interactive: interactive.client(),
        },
        "prod",
        &options(),
        tx,
        Arc::new(IngestMetrics::default()),
    )
    .await
    .expect("the Collector route cluster");
    (sync, rx)
}

fn group(script: &Script) {
    script.route(
        "GET",
        GROUP,
        200,
        r#"{"versions":[{"version":"v1beta1"}],"preferredVersion":{"version":"v1beta1"}}"#,
    );
}

fn page(script: &Script, items: Vec<serde_json::Value>) {
    script.route_accepting(
        "GET",
        LIST,
        METADATA,
        200,
        serde_json::json!({
            "apiVersion":"meta.k8s.io/v1", "kind":"PartialObjectMetadataList",
            "metadata":{}, "items":items,
        })
        .to_string(),
    );
}

fn service(name: &str, port: u16, collector: bool) -> serde_json::Value {
    serde_json::json!({
        "metadata":{"name":name,"namespace":"observe","uid":format!("uid-{name}"),
            "labels":{"app.kubernetes.io/name":if collector { "opentelemetry-collector" } else { name }}},
        "spec":{"ports":[{"name":if collector { "health" } else { "http" },"port":port}]},
    })
}

fn services(script: &Script, collectors: usize) {
    let mut items = vec![
        service("prometheus", 9090, false),
        service("grafana", 3000, false),
        service("loki", 3100, false),
        service("tempo", 3200, false),
    ];
    for index in 0..collectors {
        let name = if collectors == 1 {
            "collector".to_string()
        } else {
            format!("collector-{index:02}")
        };
        items.push(service(&name, 13133, true));
    }
    script.route(
        "GET",
        SERVICES,
        200,
        serde_json::json!({
            "apiVersion":"v1", "kind":"ServiceList", "metadata":{}, "items":items,
        })
        .to_string(),
    );
}

fn error(code: u16, sentence: &str) -> String {
    serde_json::json!({"apiVersion":"v1", "kind":"Status", "status":"Failure", "code":code,
        "reason":match code { 403 => "Forbidden", 404 => "NotFound", 406 => "NotAcceptable", _ => "InternalError" },
        "message":sentence})
    .to_string()
}

fn assert_get(row: &Seen, path: &str, accept: &str) {
    assert_eq!(row.method, "GET");
    assert_eq!(row.path.replacen("?&", "?", 1), path);
    assert_eq!(row.accept, accept);
    assert_eq!(row.content_type, "");
    assert!(row.body.is_empty());
}

async fn fetch(sync: &Sync) -> Fetched<Option<TablePage>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _stop = sync.reader.fetch_collectors(move |answer| {
        tx.send(answer).expect("Collector callback");
    });
    rx.await.expect("Collector reply")
}

async fn reached(script: &Script, count: usize) {
    for _ in 0..1_000 {
        if script.seen().len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("Collector requests: {:?}", script.seen());
}

async fn capacity(sync: &Sync, expected: usize) {
    for _ in 0..1_000 {
        if sync.reader.observe_schedule_counts().admitted == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "Collector admission: {:?}",
        sync.reader.observe_schedule_counts()
    );
}

#[tokio::test(start_paused = true)]
async fn collectors_read_only_their_metadata_and_named_process_health_on_the_interactive_client() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    group(&script);
    page(
        &script,
        vec![
            serde_json::json!({"apiVersion":"meta.k8s.io/v1", "kind":"PartialObjectMetadata",
        "metadata":{"name":"declared","namespace":"observe","uid":"collector-cr"}}),
        ],
    );
    services(&script, 1);
    script.route("GET", HEALTH, 200, "process extension answered");
    let Fetched::Ok(Some(page)) = fetch(&sync).await else {
        panic!("Collector inventory must resolve");
    };
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0].name, "declared");
    assert_eq!(page.rows[0].cells[2], "Metadata only");
    assert_eq!(page.rows[0].cells[3], "Unknown");
    assert_eq!(page.rows[0].cells[4], "Not fetched");
    assert!(page.rows[0].cells[5].is_empty());
    assert_eq!(page.rows[1].uid, "service:observe/collector");
    assert_eq!(page.rows[1].cells[4], "Process extension answered ");
    assert!(page.rows[1].cells[5].contains("observe/collector:13133"));
    let rows = script.seen();
    assert_eq!(rows.len(), 4, "no other family or backend is fetched");
    assert_get(&rows[0], GROUP, "application/json");
    assert_get(&rows[1], LIST, METADATA);
    assert_get(&rows[2], SERVICES, "");
    assert_get(&rows[3], HEALTH, "application/json");
}

#[tokio::test(start_paused = true)]
async fn absent_collectors_and_an_empty_served_group_have_distinct_outcomes() {
    for served in [false, true] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        if served {
            group(&script);
            page(&script, Vec::new());
        } else {
            script.route(
                "GET",
                GROUP,
                404,
                error(404, "Collector group is not served"),
            );
        }
        services(&script, 0);
        match fetch(&sync).await {
            Fetched::Ok(Some(page)) if served => {
                assert!(page.rows.is_empty());
                assert!(!page.truncated);
            }
            Fetched::Ok(None) if !served => {}
            other => panic!("served={served}: {other:?}"),
        }
        let rows = script.seen();
        assert_eq!(rows.len(), if served { 3 } else { 2 });
        assert_get(&rows[0], GROUP, "application/json");
        if served {
            assert_get(&rows[1], LIST, METADATA);
        }
        assert_get(rows.last().unwrap(), SERVICES, "");
    }
}

#[tokio::test(start_paused = true)]
async fn collector_cr_and_service_denials_remain_named_and_never_become_absence() {
    for denied in ["group", "list", "services", "health"] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        if denied == "group" {
            script.route(
                "GET",
                GROUP,
                403,
                error(403, "Collector group is forbidden"),
            );
        } else {
            group(&script);
        }
        if denied == "list" {
            script.route_accepting(
                "GET",
                LIST,
                METADATA,
                403,
                error(403, "Collector metadata is forbidden"),
            );
        } else if denied != "group" {
            page(&script, Vec::new());
        }
        if denied == "services" {
            script.route("GET", SERVICES, 403, error(403, "services is forbidden"));
        } else {
            services(&script, usize::from(denied == "health"));
        }
        if denied == "health" {
            script.route(
                "GET",
                HEALTH,
                403,
                error(403, "services/proxy is forbidden"),
            );
        }
        let answer = fetch(&sync).await;
        if denied == "services" {
            assert_eq!(answer, Fetched::Denied { what: "services" });
        } else {
            let Fetched::Ok(Some(page)) = answer else {
                panic!("named Collector denial: {answer:?}");
            };
            let row = page.rows.first().expect("the denial is visible");
            assert!(row.cells[4].contains("access denied"), "{row:?}");
            if denied != "health" {
                assert_eq!(row.name, "OpenTelemetryCollector");
            }
        }
        let rows = script.seen();
        assert_get(&rows[0], GROUP, "application/json");
        let mut index = 1;
        if denied != "group" {
            assert_get(&rows[index], LIST, METADATA);
            index += 1;
        }
        assert_get(&rows[index], SERVICES, "");
        index += 1;
        if denied == "health" {
            assert_get(&rows[index], HEALTH, "application/json");
            index += 1;
        }
        assert_eq!(rows.len(), index);
    }
}

#[tokio::test(start_paused = true)]
async fn collector_failures_keep_server_sentences_and_refuse_full_metadata_fallback() {
    for stage in ["group", "list", "metadata", "health"] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        if stage == "group" {
            script.route(
                "GET",
                GROUP,
                500,
                error(500, "Collector discovery storage is unavailable"),
            );
        } else {
            group(&script);
        }
        if stage == "list" {
            script.route_accepting(
                "GET",
                LIST,
                METADATA,
                406,
                error(406, "metadata representation unavailable"),
            );
        }
        if stage == "metadata" {
            script.route_accepting(
                "GET",
                LIST,
                METADATA,
                200,
                r#"{"kind":"OpenTelemetryCollectorList","items":[]}"#,
            );
        }
        if stage == "health" {
            page(&script, Vec::new());
            services(&script, 1);
            script.route(
                "GET",
                HEALTH,
                500,
                error(500, "Collector process extension is unavailable"),
            );
        }
        let answer = fetch(&sync).await;
        let expected = match stage {
            "group" => "Collector discovery storage is unavailable",
            "list" => "metadata representation unavailable",
            "metadata" => {
                "otel collectors did not return PartialObjectMetadataList; no full-object inventory is used"
            }
            _ => "Collector process extension is unavailable",
        };
        if stage == "health" {
            let Fetched::Ok(Some(page)) = answer else {
                panic!("health failure keeps its named Service row");
            };
            assert_eq!(page.rows[0].cells[4], expected);
        } else {
            assert_eq!(
                answer,
                Fetched::Failed {
                    what: "otel collectors",
                    why: expected.to_string()
                }
            );
        }
        let rows = script.seen();
        assert_get(&rows[0], GROUP, "application/json");
        if stage != "group" {
            assert_get(&rows[1], LIST, METADATA);
        }
        if stage == "health" {
            assert_get(&rows[2], SERVICES, "");
            assert_get(&rows[3], HEALTH, "application/json");
        }
        assert_eq!(
            rows.len(),
            if stage == "group" {
                1
            } else if stage == "health" {
                4
            } else {
                2
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_collectors_releases_admission_at_every_exchange_stage() {
    for stage in ["group", "list", "services", "health"] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        if stage == "group" {
            script.route_hanging("GET", GROUP);
        } else {
            group(&script);
        }
        if stage == "list" {
            script.route_hanging("GET", LIST);
        } else if stage != "group" {
            page(&script, Vec::new());
        }
        if stage == "services" {
            script.route_hanging("GET", SERVICES);
        } else if stage == "health" {
            services(&script, 1);
            script.route_hanging("GET", HEALTH);
        }
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let stop = sync.reader.fetch_collectors(move |answer| {
            tx.send(answer)
                .expect("cancelled Collector request must not reply");
        });
        let count = match stage {
            "group" => 1,
            "list" => 2,
            "services" => 3,
            _ => 4,
        };
        reached(&script, count).await;
        drop(stop);
        capacity(&sync, 0).await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        let rows = script.seen();
        assert_eq!(rows.len(), count);
        assert_get(&rows[0], GROUP, "application/json");
        if stage != "group" {
            assert_get(&rows[1], LIST, METADATA);
        }
        if matches!(stage, "services" | "health") {
            assert_get(&rows[2], SERVICES, "");
        }
        if stage == "health" {
            assert_get(&rows[3], HEALTH, "application/json");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn collectors_share_inspector_and_observe_admission_without_queued_transport() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let mut guards = Vec::new();
    for uid in ["held-0", "held-1"] {
        script.route_hanging("GET", "/api/v1/namespaces/prod/events?");
        guards.push(
            sync.inspector
                .fetch_events("prod", uid, |_| panic!("held events must not reply")),
        );
    }
    reached(&script, 2).await;
    for _ in 0..4 {
        guards.push(
            sync.reader
                .fetch_collectors(|_| panic!("queued Collector reads must not reply")),
        );
    }
    capacity(&sync, 6).await;
    let answer = fetch(&sync).await;
    assert_eq!(
        answer,
        Fetched::Failed {
            what: "telemetry",
            why: "telemetry is busy: two requests are active and four are waiting".to_string()
        }
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _observe = sync.reader.observe_loki_probe(move |answer| {
        tx.send(answer).expect("busy Observe callback");
    });
    assert!(matches!(
        rx.await.expect("Observe shares the same queue"),
        Fetched::Failed {
            what: "telemetry",
            ..
        }
    ));
    drop(guards.pop());
    capacity(&sync, 5).await;
    drop(guards);
    capacity(&sync, 0).await;
    let counts = sync.reader.observe_schedule_counts();
    assert_eq!(counts.peak_active, 2);
    assert_eq!(counts.peak_admitted, 6);
    assert_eq!(counts.rejected, 2);
    let rows = script.seen();
    assert_eq!(rows.len(), 2);
    for (row, uid) in rows.iter().zip(["held-0", "held-1"]) {
        assert_get(
            row,
            &format!(
                "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3D{uid}&limit=64"
            ),
            "",
        );
    }
}

#[tokio::test(start_paused = true)]
async fn one_collector_deadline_bounds_successive_process_health_reads() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    script.route(
        "GET",
        GROUP,
        404,
        error(404, "Collector group is not served"),
    );
    services(&script, 13);
    for index in 0..13 {
        script.route_delayed(
            "GET",
            &format!("/api/v1/namespaces/observe/services/collector-{index:02}:13133/proxy/"),
            200,
            "",
            Duration::from_secs(3),
        );
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _stop = sync.reader.fetch_collectors(move |answer| {
        tx.send(answer).expect("Collector deadline callback");
    });
    reached(&script, 3).await;
    for count in 4..=14 {
        tokio::time::advance(Duration::from_secs(3)).await;
        reached(&script, count).await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(
        rx.await.expect("whole Collector deadline"),
        Fetched::Failed {
            what: "telemetry",
            why: "the request did not finish before its deadline".to_string()
        }
    );
    capacity(&sync, 0).await;
    let rows = script.seen();
    assert_eq!(rows.len(), 14, "no partial health table or next probe");
    assert_get(&rows[0], GROUP, "application/json");
    assert_get(&rows[1], SERVICES, "");
    for (index, row) in rows[2..].iter().enumerate() {
        assert_get(
            row,
            &format!("/api/v1/namespaces/observe/services/collector-{index:02}:13133/proxy/"),
            "application/json",
        );
    }
}
