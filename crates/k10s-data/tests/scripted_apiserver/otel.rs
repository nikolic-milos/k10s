//! OpenTelemetryCollector CRs listed through kube Request, and a health GET
//! only when the bound port is the extension.

use crate::*;
use k10s_data::otel::{self, KindSet, health};
use k10s_data::reach::{Bound, FoundService, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn otel_group() -> String {
    r#"{"kind":"APIGroup","name":"opentelemetry.io",
        "versions":[{"groupVersion":"opentelemetry.io/v1beta1","version":"v1beta1"}],
        "preferredVersion":{"groupVersion":"opentelemetry.io/v1beta1","version":"v1beta1"}}"#
        .to_string()
}

fn collector_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "simplest", "namespace": "observability" },
        "spec": {
            "mode": "daemonset",
            "image": "otel/opentelemetry-collector:0.96.0",
            "config": {
                "exporters": {
                    "otlphttp": {
                        "headers": { "Authorization": "Bearer SECRET_EXPORTER_TOKEN_do_not_print" }
                    }
                }
            }
        },
        "status": {
            "conditions": [{ "type": "Ready", "status": "True" }]
        }
    })
}

fn bound_on(port: u16, port_name: Option<&str>) -> Bound {
    Bound {
        kind: ToolKind::OtelCollector,
        found: Some(FoundService {
            kind: ToolKind::OtelCollector,
            namespace: "observability".into(),
            name: "otel-collector".into(),
            port,
            port_name: port_name.map(str::to_string),
        }),
        transport: Transport::Proxy {
            namespace: "observability".into(),
            service: "otel-collector".into(),
            port,
        },
        auth: ToolAuth::Anonymous,
    }
}

#[test]
fn a_404_otel_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { otel::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.collectors, KindSet::NotServed));
    assert!(
        otel::table_page(&inventory).is_none(),
        "not served is no table"
    );
    assert!(
        script.requests_for("opentelemetrycollectors").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_otel_group_is_denied_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/opentelemetry.io",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { otel::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.collectors, KindSet::Denied));
    assert!(
        inventory.collectors.served(),
        "403 is Denied, not served: false"
    );
    let page = otel::table_page(&inventory).expect("Denied is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("access denied"), "{text}");
    drop(runtime);
}

#[test]
fn collectors_are_listed_from_the_crs_and_config_stays_off_the_row() {
    let script = Script::default();
    script.route("GET", "/apis/opentelemetry.io", 200, otel_group());
    script.route(
        "GET",
        "/apis/opentelemetry.io/v1beta1/opentelemetrycollectors?",
        200,
        serde_json::json!({
            "kind": "OpenTelemetryCollectorList",
            "items": [collector_item()]
        })
        .to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { otel::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    let collector = &inventory.collectors.items()[0];
    assert_eq!(collector.name, "simplest");
    assert_eq!(collector.mode, "daemonset");
    assert_eq!(collector.ready, "True");
    assert_eq!(collector.replicas, None);
    let debug = format!("{collector:?}");
    assert!(
        !debug.contains("SECRET_EXPORTER_TOKEN_do_not_print"),
        "{debug}"
    );
    assert!(!debug.contains("exporters"), "{debug}");

    let list = script.requests_for("opentelemetrycollectors");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].method, "GET");
    drop(runtime);
}

#[test]
fn an_undecodable_item_is_a_labelled_table_row_not_a_silent_gap() {
    let script = Script::default();
    script.route("GET", "/apis/opentelemetry.io", 200, otel_group());
    script.route(
        "GET",
        "/apis/opentelemetry.io/v1beta1/opentelemetrycollectors?",
        200,
        serde_json::json!({
            "kind": "OpenTelemetryCollectorList",
            "items": [collector_item(), { "spec": { "mode": "sidecar" } }]
        })
        .to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { otel::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    assert!(matches!(
        inventory.collectors,
        KindSet::Served { unreadable: 1, .. }
    ));
    let page = otel::table_page(&inventory).expect("a served kind is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("1 collector could not be decoded and is not shown"),
        "a partial listing says what is missing and why: {text}"
    );
    drop(runtime);
}

#[test]
fn health_on_the_extension_port_is_a_get_of_the_well_known_path() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observability/services/otel-collector:13133/proxy/",
        200,
        "",
    );

    let runtime = runtime();
    let outcome = runtime
        .block_on(async { health(&script.client(), &bound_on(13133, Some("health"))).await });
    let Fetched::Ok(answered) = outcome else {
        panic!("health extension must resolve: {outcome:?}");
    };
    assert!(answered.path.is_empty(), "health_check is GET /");
    let hits = script.requests_for("/proxy/");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].method, "GET");
    drop(runtime);
}

#[test]
fn zpages_are_a_get_of_debug_servicez() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observability/services/otel-collector:55679/proxy/debug/servicez",
        200,
        "<html></html>",
    );

    let runtime = runtime();
    let outcome = runtime
        .block_on(async { health(&script.client(), &bound_on(55679, Some("zpages"))).await });
    let Fetched::Ok(answered) = outcome else {
        panic!("zpages must resolve: {outcome:?}");
    };
    assert_eq!(answered.path, "debug/servicez");
    let hits = script.requests_for("/proxy/debug/servicez");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].method, "GET");
    drop(runtime);
}

#[test]
fn a_metrics_bind_fails_closed_and_does_not_get() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observability/services/otel-collector:8888/proxy/",
        200,
        "# HELP up\n",
    );

    let runtime = runtime();
    let outcome = runtime
        .block_on(async { health(&script.client(), &bound_on(8888, Some("metrics"))).await });
    let Fetched::Failed { why, what } = outcome else {
        panic!("metrics is Failed, not a fake healthy: {outcome:?}");
    };
    assert_eq!(what, "otel-collector");
    assert!(why.contains("metrics"), "{why}");
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "a metrics bind must not be probed as health: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_on_health_is_denied() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observability/services/otel-collector:13133/proxy/",
        403,
        status(403, "Forbidden"),
    );

    let runtime = runtime();
    let outcome =
        runtime.block_on(async { health(&script.client(), &bound_on(13133, None)).await });
    assert_eq!(
        outcome,
        Fetched::Denied {
            what: "otel-collector"
        }
    );
    drop(runtime);
}
