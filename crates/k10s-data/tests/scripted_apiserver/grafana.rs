//! Grafana dashboard fetch: the service proxy, provisioned ConfigMaps, and
//! catalog failures that must survive a successful health probe.

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

fn catalog_reader(
    runtime: &tokio::runtime::Runtime,
    script: &Script,
    provisioned: bool,
) -> (Sync, EventReceiver) {
    script_discovery(script);
    script_rules_review(script);
    script_access_reviews(script, true, 32);
    script_lists(script);
    let synced = sync_on(runtime, script);
    let items = if provisioned {
        r#"[{"metadata":{"name":"provisioned","namespace":"monitoring"},
             "data":{"dashboard.json":"{\"uid\":\"provisioned\",\"title\":\"Provisioned\",\"panels\":[]}"}}]"#
    } else {
        "[]"
    };
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        format!(r#"{{"apiVersion":"v1","kind":"ConfigMapList","items":{items}}}"#),
    );
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        r#"{"apiVersion":"v1","kind":"ServiceList","items":[
            {"metadata":{"name":"grafana","namespace":"monitoring"},
             "spec":{"ports":[{"name":"http","port":3000}]}}]}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:http/proxy/api/health",
        200,
        r#"{"database":"ok"}"#,
    );
    synced
}

fn catalog_reply(sync: &Sync) -> Fetched<k10s_data::read::GrafanaCatalog> {
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_grafana_catalog(move |answer| {
        let _ = tx.send(answer);
    });
    wait(&rx)
}

fn assert_proxy_get(script: &Script, rest: &str) {
    let path = format!("/api/v1/namespaces/monitoring/services/grafana:3000/proxy/{rest}");
    let seen = script.requests_for(&path);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, path);
    assert_eq!(seen[0].accept, "");
    assert_eq!(seen[0].body, "");
}

#[test]
fn a_failed_search_is_not_an_empty_or_partial_successful_catalog() {
    for provisioned in [false, true] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_reader(&runtime, &script, provisioned);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            500,
            r#"{"message":"search database unavailable"}"#,
        );
        let answer = catalog_reply(&sync);
        assert!(
            matches!(&answer, Fetched::Failed { what: "grafana", why }
                if why.contains("search database unavailable")),
            "{answer:?}"
        );
        assert_proxy_get(&script, "api/search?type=dash-db");
        assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
    }
}

#[test]
fn a_failed_dashboard_is_not_reduced_to_a_search_hit() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_reader(&runtime, &script, true);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        r#"[{"uid":"broken","title":"Unavailable","type":"dash-db"}]"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/broken",
        500,
        r#"{"message":"dashboard storage unavailable"}"#,
    );
    let answer = catalog_reply(&sync);
    assert!(
        matches!(&answer, Fetched::Failed { what: "grafana", why }
            if why.contains("dashboard storage unavailable")),
        "{answer:?}"
    );
    assert_proxy_get(&script, "api/search?type=dash-db");
    assert_proxy_get(&script, "api/dashboards/uid/broken");
}

#[test]
fn a_denied_search_remains_a_named_denial() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_reader(&runtime, &script, true);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        403,
        r#"{"apiVersion":"v1","kind":"Status","status":"Failure","code":403,
             "reason":"Forbidden","message":"search is forbidden"}"#,
    );
    assert!(matches!(
        catalog_reply(&sync),
        Fetched::Denied { what: "grafana" }
    ));
    assert_proxy_get(&script, "api/search?type=dash-db");
}

#[test]
fn a_successfully_empty_search_remains_a_served_catalog() {
    for provisioned in [false, true] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_reader(&runtime, &script, provisioned);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            200,
            "[]",
        );
        let Fetched::Ok(catalog) = catalog_reply(&sync) else {
            panic!("an empty search is successful");
        };
        assert!(catalog.served);
        assert_eq!(catalog.dashboards.len(), usize::from(provisioned));
        assert!(catalog.extra_hits.is_empty());
        assert!(!catalog.truncated);
        assert_proxy_get(&script, "api/search?type=dash-db");
    }
}
