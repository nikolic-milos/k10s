//! Collector CR parse, config never in Debug, 404/403 classification, and
//! health-path refusal for a metrics-only bind.

use super::*;
use crate::reach::{Bound, FoundService, ToolAuth, ToolKind, Transport};

const CONFIG_TOKEN: &str = "SECRET_EXPORTER_TOKEN_do_not_print";

fn collector_cr() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "opentelemetry.io/v1beta1",
        "kind": "OpenTelemetryCollector",
        "metadata": {
            "name": "simplest",
            "namespace": "observability"
        },
        "spec": {
            "mode": "deployment",
            "replicas": 2,
            "image": "ghcr.io/open-telemetry/opentelemetry-collector-releases/opentelemetry-collector:0.96.0",
            "config": {
                "receivers": { "otlp": { "protocols": { "http": {} } } },
                "exporters": {
                    "otlphttp": {
                        "endpoint": "https://example.invalid",
                        "headers": { "Authorization": format!("Bearer {CONFIG_TOKEN}") }
                    }
                },
                "service": {
                    "pipelines": {
                        "traces": {
                            "receivers": ["otlp"],
                            "exporters": ["otlphttp"]
                        }
                    }
                }
            }
        },
        "status": {
            "image": "ghcr.io/open-telemetry/opentelemetry-collector-releases/opentelemetry-collector:0.96.0",
            "conditions": [
                { "type": "Ready", "status": "True" }
            ]
        }
    })
}

fn sidecar_cr() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "app-sidecar", "namespace": "prod" },
        "spec": {
            "mode": "sidecar",
            "config": "exporters:\n  debug: {}\n"
        },
        "status": {
            "conditions": [{ "type": "Available", "status": "True" }]
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

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn a_collector_keeps_name_mode_replicas_ready_and_image() {
    let collector = parse_collector(&collector_cr()).expect("the fixture is a collector");
    assert_eq!(collector.name, "simplest");
    assert_eq!(collector.namespace, "observability");
    assert_eq!(collector.mode, "deployment");
    assert_eq!(collector.replicas, Some(2));
    assert_eq!(collector.ready, "True");
    assert!(
        collector.image.contains("opentelemetry-collector:0.96.0"),
        "{}",
        collector.image
    );
}

#[test]
fn a_sidecar_has_no_replicas_and_falls_back_to_available() {
    let collector = parse_collector(&sidecar_cr()).expect("sidecar");
    assert_eq!(collector.mode, "sidecar");
    assert_eq!(collector.replicas, None);
    assert_eq!(collector.ready, "True");
    assert!(collector.image.is_empty());
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_collector(&serde_json::json!({})).is_none());
}

#[test]
fn config_bytes_never_appear_in_debug() {
    let collector = parse_collector(&collector_cr()).expect("collector");
    let debug = format!("{collector:?}");
    assert!(
        !debug.contains(CONFIG_TOKEN),
        "Debug must not print exporter tokens: {debug}"
    );
    assert!(
        !debug.contains("exporters"),
        "Debug must not print spec.config: {debug}"
    );
    assert!(
        !debug.contains("Authorization"),
        "Debug must not print exporter headers: {debug}"
    );

    let inventory = Inventory {
        collectors: KindSet::Served {
            items: vec![collector],
            truncated: false,
            unreadable: 0,
        },
    };
    let debug = format!("{inventory:?}");
    assert!(!debug.contains(CONFIG_TOKEN), "{debug}");
    assert!(!debug.contains("exporters"), "{debug}");
}

#[test]
fn a_long_image_is_clipped_where_it_is_carried() {
    let huge = "x".repeat(MAX_FIELD_CHARS + 40);
    let value = serde_json::json!({
        "metadata": { "name": huge, "namespace": huge },
        "spec": { "mode": huge, "image": huge }
    });
    let collector = parse_collector(&value).expect("clipped");
    for field in [
        &collector.name,
        &collector.namespace,
        &collector.mode,
        &collector.image,
    ] {
        assert!(
            field.chars().count() <= MAX_FIELD_CHARS + 1,
            "clipped where carried: {} chars",
            field.chars().count()
        );
        assert!(field.ends_with('\u{2026}'));
    }
}

#[test]
fn a_missing_group_is_invisible_and_a_forbidden_one_is_denied() {
    assert!(matches!(
        after_group(&api_error(404)),
        GroupAnswer::NotServed
    ));
    assert!(
        matches!(after_group(&api_error(403)), GroupAnswer::Denied),
        "a 403 is Denied, never an empty inventory that looks like the operator is absent"
    );
    assert!(matches!(after_group(&api_error(401)), GroupAnswer::Denied));
    assert!(matches!(
        after_group(&api_error(500)),
        GroupAnswer::Failed(_)
    ));
}

#[test]
fn an_unserved_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "a 404 group is absence, not an empty list"
    );
}

#[test]
fn a_denied_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        collectors: KindSet::Denied,
    })
    .expect("Denied is served, so the table exists");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("access denied for this account"),
        "a 403 stays labelled: {text}"
    );
}

#[test]
fn a_served_fixture_is_one_row_per_collector() {
    let collector = parse_collector(&collector_cr()).expect("collector");
    let page = table_page(&Inventory {
        collectors: KindSet::Served {
            items: vec![collector],
            truncated: false,
            unreadable: 0,
        },
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "simplest");
    assert_eq!(page.rows[0].cells[2], "deployment");
    assert_eq!(page.rows[0].cells[3], "2");
    assert_eq!(page.rows[0].cells[4], "Ready");
}

#[test]
fn an_unreadable_count_is_a_labelled_row_not_a_silently_shorter_list() {
    let collector = parse_collector(&collector_cr()).expect("collector");
    let page = table_page(&Inventory {
        collectors: KindSet::Served {
            items: vec![collector],
            truncated: false,
            unreadable: 2,
        },
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 2);
    let text = page.rows[1].cells.join(" ");
    assert!(
        text.contains("2 collectors could not be decoded and are not shown"),
        "a partial inventory says what is missing and why: {text}"
    );
}

#[test]
fn a_fully_readable_inventory_has_no_unreadable_row() {
    let collector = parse_collector(&collector_cr()).expect("collector");
    let page = table_page(&Inventory {
        collectors: KindSet::Served {
            items: vec![collector],
            truncated: false,
            unreadable: 0,
        },
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
}

#[test]
fn health_path_on_the_extension_ports() {
    assert_eq!(
        health_path(&bound_on(HEALTH_PORT, None)).expect("health"),
        ""
    );
    assert_eq!(
        health_path(&bound_on(ZPAGES_PORT, None)).expect("zpages"),
        "debug/servicez"
    );
    assert_eq!(
        health_path(&bound_on(4317, Some("health"))).expect("named health"),
        ""
    );
    assert_eq!(
        health_path(&bound_on(80, Some("zpages"))).expect("named zpages"),
        "debug/servicez"
    );
}

#[test]
fn a_metrics_bind_is_not_a_health_signal() {
    let err = health_path(&bound_on(METRICS_PORT, Some("metrics"))).expect_err("metrics");
    assert!(err.contains("metrics"), "{err}");
    assert!(err.contains("not a health signal"), "{err}");
}

#[test]
fn an_otlp_bind_is_not_a_health_signal() {
    let err = health_path(&bound_on(OTLP_HTTP_PORT, Some("otlp-http"))).expect_err("otlp");
    assert!(err.contains("OTLP"), "{err}");
}

#[test]
fn a_settings_url_on_the_health_port_is_the_extension() {
    let bound = Bound {
        kind: ToolKind::OtelCollector,
        found: None,
        transport: Transport::Url {
            base: "http://127.0.0.1:13133".into(),
        },
        auth: ToolAuth::Anonymous,
    };
    assert_eq!(health_path(&bound).expect("url health"), "");
}
