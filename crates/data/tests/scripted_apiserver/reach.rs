//! Tool reach: Services matched, proxy preferred, tokens never on the proxy.

use crate::*;
use k10s_data::reach::{ReachSettings, ToolKind, ToolReach, bind};

fn service_page(token: &str) -> String {
    serde_json::json!({
        "apiVersion":"v1","kind":"ServiceList","metadata":{"continue":token},"items":[]
    })
    .to_string()
}

fn assert_service_lists(script: &Script, count: usize) {
    let requests = script.requests_for("/api/v1/services?");
    assert_eq!(requests.len(), count);
    for request in requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.accept, "");
        assert_eq!(request.content_type, "");
        assert!(request.path.contains("limit=200"));
        assert!(request.body.is_empty());
    }
    assert!(script.requests_for("/proxy/").is_empty());
    assert!(script.requests_for("secrets").is_empty());
}

#[test]
fn repeated_service_continuations_refuse_partial_discovery() {
    let script = Script::default();
    for _ in 0..2 {
        script.route("GET", "/api/v1/services?", 200, service_page("same"));
    }
    let answer = runtime().block_on(async {
        k10s_data::reach::find_services(&script.client(), ToolKind::Prometheus).await
    });
    assert!(
        matches!(answer, k10s_data::read::Fetched::Failed { why, .. } if why.contains("repeated a continuation token"))
    );
    assert_service_lists(&script, 2);
    assert!(
        script.requests_for("/api/v1/services?")[1]
            .path
            .contains("continue=same")
    );
}

#[test]
fn empty_service_pages_have_a_finite_request_ceiling() {
    let script = Script::default();
    for index in 0..16 {
        script.route(
            "GET",
            "/api/v1/services?",
            200,
            service_page(&format!("page-{index}")),
        );
    }
    let answer = runtime().block_on(async {
        k10s_data::reach::find_services(&script.client(), ToolKind::Prometheus).await
    });
    assert!(
        matches!(answer, k10s_data::read::Fetched::Failed { why, .. } if why.contains("16 pages"))
    );
    assert_service_lists(&script, 16);
}

#[test]
fn service_pages_share_an_aggregate_body_budget() {
    let script = Script::default();
    let mut first = service_page("next");
    first.push_str(&" ".repeat((4 << 20) - first.len()));
    script.route("GET", "/api/v1/services?", 200, first);
    script.route_bytes("GET", "/api/v1/services?", 200, vec![b' '; (4 << 20) + 1]);
    let answer = runtime().block_on(async {
        k10s_data::reach::find_services(&script.client(), ToolKind::Prometheus).await
    });
    assert!(
        matches!(answer, k10s_data::read::Fetched::Failed { why, .. } if why.contains("4194304 bytes"))
    );
    assert_service_lists(&script, 2);
}

fn grafana_service() -> String {
    r#"{"metadata":{"name":"grafana","uid":"uid-graf","namespace":"monitoring","resourceVersion":"1",
        "labels":{"app.kubernetes.io/name":"grafana"}},
       "spec":{"ports":[{"name":"http","port":3000,"targetPort":3000}]}}"#
        .into()
}

#[test]
fn grafana_is_bound_through_the_service_proxy_when_health_answers() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        format!(
            r#"{{"kind":"ServiceList","apiVersion":"v1","items":[{}]}}"#,
            grafana_service()
        ),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http/proxy/api/health",
        200,
        r#"{"database":"ok"}"#,
    );
    let runtime = runtime();
    let reach = runtime.block_on(async {
        bind(
            &script.client(),
            ToolKind::Grafana,
            &ReachSettings::default(),
        )
        .await
    });
    match reach {
        ToolReach::Bound(bound) => {
            assert!(matches!(
                bound.transport,
                k10s_data::reach::Transport::Proxy { .. }
            ));
            let hits = script.requests_for("/proxy/api/health");
            assert_eq!(
                hits.len(),
                1,
                "the probe is the proxy, not a scrape of Secrets"
            );
        }
        other => panic!("grafana should bind: {other:?}"),
    }
}

#[test]
fn grafana_discovery_probes_the_matching_tcp_port() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        r#"{"kind":"ServiceList","apiVersion":"v1","items":[
            {"metadata":{"name":"grafana","namespace":"monitoring"},
             "spec":{"ports":[
               {"name":"datagram","port":3000,"protocol":"UDP"},
               {"name":"web","port":3000,"protocol":"TCP"}
             ]}}
        ]}"#,
    );
    let path = "/api/v1/namespaces/monitoring/services/grafana:web/proxy/api/health";
    script.route("GET", path, 200, r#"{"database":"ok"}"#);
    let reach = runtime().block_on(async {
        bind(
            &script.client(),
            ToolKind::Grafana,
            &ReachSettings::default(),
        )
        .await
    });
    let ToolReach::Bound(bound) = reach else {
        panic!("{reach:?}");
    };
    let found = bound.found.expect("discovered service");
    assert_eq!(found.port, 3000);
    assert_eq!(found.port_name.as_deref(), Some("web"));
    let lists = script.requests_for("/api/v1/services?");
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].method, "GET");
    assert_eq!(lists[0].accept, "");
    assert!(lists[0].body.is_empty());
    let probes = script.requests_for("/proxy/");
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].method, "GET");
    assert_eq!(probes[0].path, path);
    assert_eq!(probes[0].accept, "application/json");
    assert!(probes[0].body.is_empty());
}

#[test]
fn proxy_refuses_a_parseable_partial_response_instead_of_an_empty_catalog() {
    use k10s_data::reach::{Bound, ToolAuth, Transport, tool_get};
    use k10s_data::read::Fetched;

    let script = Script::default();
    let path = "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search";
    script.route("GET", path, 206, "[]");
    let bound = Bound {
        kind: ToolKind::Grafana,
        found: None,
        transport: Transport::Proxy {
            namespace: "monitoring".into(),
            service: "grafana".into(),
            port: 3000,
        },
        auth: ToolAuth::Anonymous,
    };
    let answer =
        runtime().block_on(async { tool_get(&script.client(), &bound, "api/search").await });
    assert!(matches!(
        answer,
        Fetched::Failed { what: "grafana", why } if why.contains("206 Partial Content")
    ));
    let requests = script.requests_for(path);
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
fn matching_services_without_tcp_ports_are_labelled_for_binding_and_inventory() {
    for spec in [
        serde_json::json!({}),
        serde_json::json!({"ports":[{"port":3000,"protocol":"UDP"}]}),
    ] {
        let script = Script::default();
        let body = serde_json::json!({
            "kind":"ServiceList", "apiVersion":"v1",
            "items":[{"metadata":{"name":"grafana","namespace":"monitoring"},"spec":spec}]
        })
        .to_string();
        for _ in 0..2 {
            script.route("GET", "/api/v1/services?", 200, body.clone());
        }
        let (binding, inventory) = runtime().block_on(async {
            let client = script.client();
            (
                bind(&client, ToolKind::Grafana, &ReachSettings::default()).await,
                k10s_data::reach::find_services(&client, ToolKind::Grafana).await,
            )
        });
        let ToolReach::Unbound(unbound) = binding else {
            panic!("{binding:?}");
        };
        assert_eq!(
            unbound.why,
            "Grafana Service monitoring/grafana has no usable TCP port"
        );
        let k10s_data::read::Fetched::Failed { what, why } = inventory else {
            panic!("{inventory:?}");
        };
        assert_eq!(what, "services");
        assert!(why.contains("Grafana Service monitoring/grafana has no usable TCP port"));
        assert!(why.contains("no partial source list returned"));
        assert_service_lists(&script, 2);
    }
}

#[test]
fn usable_and_unusable_matches_do_not_select_a_source_or_hide_inventory_failure() {
    let script = Script::default();
    let body = r#"{"kind":"ServiceList","apiVersion":"v1","items":[
        {"metadata":{"name":"a-grafana","namespace":"monitoring"},
         "spec":{"ports":[{"port":3000}]}},
        {"metadata":{"name":"b-grafana","namespace":"monitoring"},
         "spec":{"ports":[{"port":3000,"protocol":"UDP"}]}}
    ]}"#;
    for _ in 0..2 {
        script.route("GET", "/api/v1/services?", 200, body);
    }
    let (binding, inventory) = runtime().block_on(async {
        let client = script.client();
        (
            bind(&client, ToolKind::Grafana, &ReachSettings::default()).await,
            k10s_data::reach::find_services(&client, ToolKind::Grafana).await,
        )
    });
    let ToolReach::Unbound(unbound) = binding else {
        panic!("{binding:?}");
    };
    assert!(unbound.why.contains("Grafana matches 2 Services"));
    assert!(
        unbound
            .why
            .contains("Grafana Service monitoring/b-grafana has no usable TCP port")
    );
    assert!(unbound.why.contains("without usable TCP ports: 1"));
    let k10s_data::read::Fetched::Failed { what, why } = inventory else {
        panic!("{inventory:?}");
    };
    assert_eq!(what, "services");
    assert!(why.contains("Grafana Service monitoring/b-grafana has no usable TCP port"));
    assert!(why.contains("without usable TCP ports: 1"));
    assert!(why.contains("no partial source list returned"));
    assert_service_lists(&script, 2);
}

#[test]
fn multiple_unusable_matches_are_counted_with_a_named_example() {
    let script = Script::default();
    let body = r#"{"kind":"ServiceList","apiVersion":"v1","items":[
        {"metadata":{"name":"a-grafana","namespace":"monitoring"},"spec":{}},
        {"metadata":{"name":"b-grafana","namespace":"monitoring"},
         "spec":{"ports":[{"port":3000,"protocol":"UDP"}]}}
    ]}"#;
    for _ in 0..2 {
        script.route("GET", "/api/v1/services?", 200, body);
    }
    let (binding, inventory) = runtime().block_on(async {
        let client = script.client();
        (
            bind(&client, ToolKind::Grafana, &ReachSettings::default()).await,
            k10s_data::reach::find_services(&client, ToolKind::Grafana).await,
        )
    });
    let ToolReach::Unbound(unbound) = binding else {
        panic!("{binding:?}");
    };
    assert!(unbound.why.contains("Grafana matches 2 Services"));
    assert!(
        unbound
            .why
            .contains("monitoring/a-grafana has no usable TCP port")
    );
    assert!(unbound.why.contains("without usable TCP ports: 2"));
    let k10s_data::read::Fetched::Failed { what, why } = inventory else {
        panic!("{inventory:?}");
    };
    assert_eq!(what, "services");
    assert!(why.contains("monitoring/a-grafana has no usable TCP port"));
    assert!(why.contains("without usable TCP ports: 2"));
    assert_service_lists(&script, 2);
}

#[test]
fn multiple_usable_headless_services_are_ambiguous_but_remain_inventory_entries() {
    let script = Script::default();
    let body = r#"{"kind":"ServiceList","apiVersion":"v1","items":[
        {"metadata":{"name":"a-grafana","namespace":"monitoring"},
         "spec":{"clusterIP":"None","ports":[{"port":3000}]}},
        {"metadata":{"name":"b-grafana","namespace":"monitoring"},
         "spec":{"clusterIP":"None","ports":[{"port":3000}]}}
    ]}"#;
    for _ in 0..2 {
        script.route("GET", "/api/v1/services?", 200, body);
    }
    let (binding, inventory) = runtime().block_on(async {
        let client = script.client();
        (
            bind(&client, ToolKind::Grafana, &ReachSettings::default()).await,
            k10s_data::reach::find_services(&client, ToolKind::Grafana).await,
        )
    });
    let ToolReach::Unbound(unbound) = binding else {
        panic!("{binding:?}");
    };
    assert!(unbound.why.contains("Grafana matches 2 Services"));
    let k10s_data::read::Fetched::Ok(found) = inventory else {
        panic!("{inventory:?}");
    };
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].name, "a-grafana");
    assert_eq!(found[1].name, "b-grafana");
    assert_service_lists(&script, 2);
}

#[test]
fn a_cluster_with_no_matching_service_hides_the_section() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        r#"{"kind":"ServiceList","apiVersion":"v1","items":[]}"#,
    );
    let runtime = runtime();
    let reach = runtime.block_on(async {
        bind(&script.client(), ToolKind::Loki, &ReachSettings::default()).await
    });
    assert!(
        matches!(
            reach,
            ToolReach::Absent {
                kind: ToolKind::Loki
            }
        ),
        "{reach:?}"
    );
}

#[test]
fn a_403_listing_services_is_a_labelled_hole_not_an_empty_cluster() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/services?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"no"}"#,
    );
    let runtime = runtime();
    match runtime.block_on(async {
        bind(
            &script.client(),
            ToolKind::Prometheus,
            &ReachSettings::default(),
        )
        .await
    }) {
        ToolReach::Unbound(unbound) => {
            assert!(unbound.why.contains("denied") || unbound.why.contains("services"));
        }
        other => panic!("forbidden is Unbound, not {other:?}"),
    }
}

#[test]
fn a_denied_proxy_is_not_a_request_for_a_backend_token() {
    for status in [401, 403] {
        let script = Script::default();
        script.route(
            "GET",
            "/api/v1/services?",
            200,
            format!(r#"{{"items":[{}]}}"#, grafana_service()),
        );
        let path = "/api/v1/namespaces/monitoring/services/grafana:http/proxy/api/health";
        script.route("GET", path, status, r#"{"message":"proxy access refused"}"#);
        let result = runtime().block_on(async {
            bind(
                &script.client(),
                ToolKind::Grafana,
                &ReachSettings::default(),
            )
            .await
        });
        let ToolReach::Unbound(result) = result else {
            panic!("{result:?}")
        };
        assert!(result.why.contains("Grafana proxy") && result.why.contains("denied"));
        assert!(!result.why.contains("wants a token"));
        let seen = script.requests_for(path);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, path);
        assert_eq!(seen[0].accept, "application/json");
        assert_eq!(seen[0].content_type, "");
        assert!(seen[0].body.is_empty());
    }
}

#[test]
fn the_monitoring_charts_exporter_is_not_the_prometheus_endpoint() {
    use k10s_data::reach::Transport;
    for installed in [false, true] {
        let script = Script::default();
        let mut items = vec![serde_json::json!({
            "metadata":{"name":"monitoring-kube-prometheus-coredns","namespace":"kube-system",
                "labels":{"app":"coredns","helm.sh/chart":"kube-prometheus-stack-90.0.0","release":"prometheus"}},
            "spec":{"ports":[{"name":"http-metrics","port":9153}]}
        })];
        if installed {
            items.push(serde_json::json!({
                "metadata":{"name":"monitoring-kube-prometheus-prometheus","namespace":"observability",
                    "labels":{"app.kubernetes.io/name":"prometheus"}},
                "spec":{"ports":[{"name":"web","port":9090}]}
            }));
        }
        script.route(
            "GET",
            "/api/v1/services?",
            200,
            serde_json::json!({"kind":"ServiceList","apiVersion":"v1","items":items}).to_string(),
        );
        let path = "/api/v1/namespaces/observability/services/monitoring-kube-prometheus-prometheus:web/proxy/-/ready";
        script.route("GET", path, 200, "Prometheus Server is Ready.");
        let runtime = runtime();
        let outcome = runtime.block_on(async {
            bind(
                &script.client(),
                ToolKind::Prometheus,
                &ReachSettings::default(),
            )
            .await
        });
        if installed {
            let ToolReach::Bound(bound) = outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(
                bound.transport,
                Transport::Proxy {
                    namespace: "observability".into(),
                    service: "monitoring-kube-prometheus-prometheus".into(),
                    port: 9090,
                }
            );
        } else {
            assert!(matches!(
                outcome,
                ToolReach::Absent {
                    kind: ToolKind::Prometheus
                }
            ));
        }
        let seen = script.seen();
        assert_eq!(seen.len(), if installed { 2 } else { 1 });
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, "/api/v1/services?&limit=200");
        assert_eq!(seen[0].accept, "");
        assert!(seen[0].body.is_empty());
        if installed {
            assert_eq!(seen[1].method, "GET");
            assert_eq!(seen[1].path, path);
            assert_eq!(seen[1].accept, "application/json");
            assert!(seen[1].body.is_empty());
        }
    }
}

#[test]
fn proxy_error_sentences_skip_unusable_message_fields_without_changing_the_request() {
    use k10s_data::reach::{Bound, ToolAuth, Transport, tool_get};
    use k10s_data::read::Fetched;
    for message in [
        serde_json::Value::Null,
        serde_json::json!(7),
        serde_json::json!(false),
        serde_json::json!([]),
        serde_json::json!({}),
        serde_json::json!(""),
        serde_json::json!("   "),
    ] {
        let script = Script::default();
        let path = "/api/v1/namespaces/observability/services/grafana:3000/proxy/api/health";
        script.route(
            "GET",
            path,
            500,
            serde_json::json!({"message": message, "error": "disk is full"}).to_string(),
        );
        let bound = Bound {
            kind: ToolKind::Grafana,
            found: None,
            transport: Transport::Proxy {
                namespace: "observability".into(),
                service: "grafana".into(),
                port: 3000,
            },
            auth: ToolAuth::Anonymous,
        };
        let answer =
            runtime().block_on(async { tool_get(&script.client(), &bound, "api/health").await });
        assert_eq!(
            answer,
            Fetched::Failed {
                what: "grafana",
                why: "disk is full".into()
            }
        );
        let requests = script.requests_for(path);
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, path);
        assert_eq!(request.accept, "application/json");
        assert_eq!(request.content_type, "");
        assert!(request.body_collected);
        assert!(request.body.is_empty());
        assert!(script.requests_for("secrets").is_empty());
    }
}

#[test]
fn proxy_error_sentence_selection_preserves_the_error_cap_and_immediate_denials() {
    use k10s_data::reach::{Bound, ToolAuth, Transport, tool_get};
    use k10s_data::read::Fetched;
    for (status, size) in [
        (500, 32 << 10),
        (500, (32 << 10) + 1),
        (401, (32 << 10) + 1),
        (403, (32 << 10) + 1),
    ] {
        let script = Script::default();
        let path = "/api/v1/namespaces/observability/services/grafana:3000/proxy/api/health";
        script.route_bytes("GET", path, status, vec![b'x'; size]);
        let bound = Bound {
            kind: ToolKind::Grafana,
            found: None,
            transport: Transport::Proxy {
                namespace: "observability".into(),
                service: "grafana".into(),
                port: 3000,
            },
            auth: ToolAuth::Anonymous,
        };
        let answer =
            runtime().block_on(async { tool_get(&script.client(), &bound, "api/health").await });
        if matches!(status, 401 | 403) {
            assert_eq!(answer, Fetched::Denied { what: "grafana" });
        } else {
            let Fetched::Failed {
                what: "grafana",
                why,
            } = answer
            else {
                panic!("a non-success status must retain its bounded failure")
            };
            if size == 32 << 10 {
                assert_eq!(why, "x".repeat(size));
            } else {
                assert!(why.contains("more than 32768 bytes"));
            }
        }
        let requests = script.requests_for(path);
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, path);
        assert_eq!(request.accept, "application/json");
        assert_eq!(request.content_type, "");
        assert!(request.body_collected);
        assert!(request.body.is_empty());
    }
}
