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
