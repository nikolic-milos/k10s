//! Tool reach: Services matched, proxy preferred, tokens never on the proxy.

use crate::*;
use k10s_data::reach::{ReachSettings, ToolKind, ToolReach, bind};

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
            assert_eq!(seen[1].accept, "");
            assert!(seen[1].body.is_empty());
        }
    }
}
