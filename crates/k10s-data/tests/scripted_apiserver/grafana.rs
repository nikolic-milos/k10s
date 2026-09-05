//! Grafana dashboard fetch: the API-server service proxy, and labelled ConfigMaps.

use crate::*;
use k10s_data::grafana::{fetch_dashboard, fetch_provisioned_from_configmaps, fetch_search};
use k10s_data::reach::{Bound, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;

fn proxy_bound() -> Bound {
    Bound {
        kind: ToolKind::Grafana,
        found: None,
        transport: Transport::Proxy {
            namespace: "monitoring".into(),
            service: "grafana".into(),
            port: 3000,
        },
        auth: ToolAuth::Anonymous,
    }
}

#[test]
fn search_goes_through_the_service_proxy() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        r#"[{"uid":"k8s","title":"Cluster","folderTitle":"Kubernetes","type":"dash-db"}]"#,
    );
    let runtime = runtime();
    let fetched =
        runtime.block_on(async { fetch_search(&script.client(), &proxy_bound(), &[]).await });
    let Fetched::Ok(hits) = fetched else {
        panic!("search must resolve: {fetched:?}");
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].uid, "k8s");
    assert_eq!(hits[0].folder_title, "Kubernetes");

    let seen = script.requests_for("/proxy/api/search");
    assert_eq!(seen.len(), 1, "search is the Grafana API through the proxy");
    assert!(
        seen[0].path.contains("/proxy/api/search"),
        "{}",
        seen[0].path
    );
    assert!(
        seen[0].path.contains("type=dash-db"),
        "dashboards only, not folders: {}",
        seen[0].path
    );
}

#[test]
fn a_dashboard_is_fetched_by_uid_through_the_same_proxy() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/k8s",
        200,
        r#"{"dashboard":{"uid":"k8s","title":"Cluster","panels":[]}}"#,
    );
    let runtime = runtime();
    let fetched =
        runtime.block_on(async { fetch_dashboard(&script.client(), &proxy_bound(), "k8s").await });
    let Fetched::Ok(dash) = fetched else {
        panic!("the dashboard must resolve: {fetched:?}");
    };
    assert_eq!(dash.uid, "k8s");
    assert_eq!(dash.title, "Cluster");
    let seen = script.requests_for("/proxy/api/dashboards/uid/k8s");
    assert_eq!(seen.len(), 1);
}

#[test]
fn provisioned_dashboards_come_from_labelled_configmaps_never_secrets() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        r#"{"kind":"ConfigMapList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"name":"cluster-dash","namespace":"monitoring",
                         "labels":{"grafana_dashboard":"1"}},
             "data":{"cluster.json":"{\"uid\":\"k8s\",\"title\":\"Cluster\",\"panels\":[]}",
                     "notes":"not a dashboard"}}
        ]}"#,
    );
    let runtime = runtime();
    let fetched =
        runtime.block_on(async { fetch_provisioned_from_configmaps(&script.client()).await });
    let Fetched::Ok(provisioned) = fetched else {
        panic!("provisioned dashboards must resolve: {fetched:?}");
    };
    assert_eq!(provisioned.dashboards.len(), 1);
    assert_eq!(provisioned.dashboards[0].uid, "k8s");
    assert!(!provisioned.truncated);

    let listed = script.requests_for("/api/v1/configmaps");
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].path.contains("labelSelector"),
        "the list is labelled, not a cluster-wide ConfigMap dump: {}",
        listed[0].path
    );
    assert!(
        listed[0].path.contains("grafana_dashboard"),
        "{}",
        listed[0].path
    );
    assert!(
        script.requests_for("/secrets").is_empty(),
        "the sidecar's Secret watch is not this path"
    );
}
