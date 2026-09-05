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
