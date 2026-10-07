//! Grafana dashboard fetch: the service proxy, provisioned ConfigMaps, and
//! catalog failures that must survive a successful health probe.

use crate::*;
use k10s_data::integrations::grafana::{
    fetch_dashboard, fetch_provisioned_from_configmaps, fetch_search,
};
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
        r#"{"dashboard":{"uid":"k8s","title":"Cluster","panels":[{
            "id":1,"title":"Pods","type":"timeseries","targets":[{
                "refId":"A","expr":"kube_pod_info",
                "datasource":{"uid":"$datasource","type":"prometheus"}
            }]
        }]}}"#,
    );
    let runtime = runtime();
    let fetched =
        runtime.block_on(async { fetch_dashboard(&script.client(), &proxy_bound(), "k8s").await });
    let Fetched::Ok(dash) = fetched else {
        panic!("the dashboard must resolve: {fetched:?}");
    };
    assert_eq!(dash.uid, "k8s");
    assert_eq!(dash.title, "Cluster");
    assert_eq!(dash.panels.len(), 1);
    assert_eq!(dash.panels[0].queries.len(), 1);
    let query = &dash.panels[0].queries[0];
    assert_eq!(query.expr, "kube_pod_info");
    assert_eq!(query.datasource.as_deref(), Some("$datasource"));
    assert_eq!(
        query.dialect,
        k10s_data::integrations::grafana::QueryDialect::PromQL
    );
    let seen = script.requests_for("/proxy/api/dashboards/uid/k8s");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        seen[0].path,
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/k8s"
    );
    assert_eq!(seen[0].accept, "application/json");
    assert_eq!(seen[0].content_type, "");
    assert!(seen[0].body.is_empty());
}

#[test]
fn a_different_or_missing_dashboard_uid_is_refused_after_one_get() {
    for dashboard in [
        r#"{"uid":"other","title":"Other dashboard","panels":[]}"#,
        r#"{"title":"No UID","panels":[]}"#,
        r#"{"uid":10,"title":"Wrong UID type","panels":[]}"#,
    ] {
        let script = Script::default();
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/k8s",
            200,
            format!(r#"{{"dashboard":{dashboard}}}"#),
        );
        let runtime = runtime();
        let fetched = runtime
            .block_on(async { fetch_dashboard(&script.client(), &proxy_bound(), "k8s").await });
        let Fetched::Failed {
            what: "grafana",
            why,
        } = fetched
        else {
            panic!("a dashboard must have the requested identity: {fetched:?}");
        };
        assert!(why.starts_with("Grafana returned dashboard UID "), "{why}");
        assert!(why.ends_with("for requested UID \"k8s\""), "{why}");
        assert_eq!(script.seen().len(), 1);
        assert_proxy_get(&script, "api/dashboards/uid/k8s");
    }
}

#[test]
fn a_matching_dashboard_uid_with_malformed_panels_is_refused_after_one_get() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/k8s",
        200,
        r#"{"dashboard":{"uid":"k8s","title":"Cluster","panels":{}}}"#,
    );
    assert!(matches!(
        runtime().block_on(async {
            fetch_dashboard(&script.client(), &proxy_bound(), "k8s").await
        }),
        Fetched::Failed { what: "grafana", why } if why == "Grafana panels member is not an array",
    ));
    assert_eq!(script.seen().len(), 1);
    assert_proxy_get(&script, "api/dashboards/uid/k8s");
}

#[test]
fn a_broken_provisioned_file_keeps_healthy_catalog_rows_and_its_named_failure() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
            "apiVersion":"v1", "kind":"ConfigMapList", "metadata":{},
            "items":[{
                "metadata":{"name":"cluster-dash","namespace":"monitoring"},
                "data":{
                    "broken.json": "{",
                    "good.json": r#"{"uid":"healthy","title":"Healthy","panels":[]}"#,
                    "notes": "not a dashboard"
                }
            }]
        })
        .to_string(),
    );
    catalog_source(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        "[]",
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("healthy dashboard rows must survive a broken sibling file");
    };
    assert_eq!(catalog.dashboards.len(), 1);
    assert_eq!(catalog.dashboards[0].uid, "healthy");
    assert_eq!(catalog.errors.len(), 1);
    assert!(catalog.errors[0].starts_with(
        "provisioned dashboards: ConfigMap monitoring/cluster-dash key \"broken.json\": dashboard JSON did not parse:",
    ));
    assert!(!catalog.truncated);
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert!(script.requests_for("/proxy/api/dashboards/uid/").is_empty());
    for request in script.requests_for("/secrets") {
        assert!(
            request.accept.contains("as=PartialObjectMetadata"),
            "{request:?}",
        );
        assert!(request.body.is_empty());
    }
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
    let synced = catalog_connection(runtime, script);
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
    catalog_source(script);
    synced
}

fn catalog_connection(runtime: &tokio::runtime::Runtime, script: &Script) -> (Sync, EventReceiver) {
    script_discovery(script);
    script_rules_review(script);
    script_access_reviews(script, true, 32);
    script_lists(script);
    sync_on(runtime, script)
}

fn catalog_source(script: &Script) {
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
    let seen: Vec<_> = script
        .requests_for(&path)
        .into_iter()
        .filter(|request| request.path == path)
        .collect();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, path);
    assert_eq!(seen[0].accept, "application/json");
    assert_eq!(seen[0].content_type, "");
    assert_eq!(seen[0].body, "");
}

#[test]
fn a_failed_search_keeps_healthy_rows_and_its_named_catalog_error() {
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
        let Fetched::Ok(catalog) = catalog_reply(&sync) else {
            panic!("a served catalog keeps its partial state");
        };
        assert_eq!(catalog.dashboards.len(), usize::from(provisioned));
        assert_eq!(
            catalog.errors,
            ["Grafana search: search database unavailable"]
        );
        assert!(catalog.served);
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
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
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("healthy provisioned rows survive a failed named dashboard");
    };
    assert_eq!(catalog.dashboards.len(), 1);
    assert!(catalog.extra_hits.is_empty());
    assert_eq!(
        catalog.errors,
        ["Grafana dashboard broken: dashboard storage unavailable"]
    );
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert_proxy_get(&script, "api/dashboards/uid/broken");
}

#[test]
fn a_mismatched_dashboard_keeps_healthy_rows_and_its_named_identity_failure() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_reader(&runtime, &script, true);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        r#"[{"uid":"requested","title":"Requested dashboard","type":"dash-db"}]"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/requested",
        200,
        r#"{"dashboard":{"uid":"other","title":"Wrong dashboard","panels":[]}}"#,
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("healthy provisioned rows survive a mismatched live dashboard");
    };
    assert_eq!(catalog.dashboards.len(), 1);
    assert_eq!(catalog.dashboards[0].uid, "provisioned");
    assert!(catalog.extra_hits.is_empty());
    assert_eq!(
        catalog.errors,
        [
            "Grafana dashboard requested: Grafana returned dashboard UID \"other\" for requested UID \"requested\""
        ]
    );
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert_proxy_get(&script, "api/dashboards/uid/requested");
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
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
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
        assert!(catalog.errors.is_empty());
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    }
}

fn configmap_page(uid: Option<&str>, token: &str) -> String {
    let items: Vec<_> = uid.into_iter().map(|uid| serde_json::json!({
        "metadata":{"name":"owned-dashboard","namespace":"monitoring"},
        "data":{"dashboard.json":serde_json::json!({"uid":uid,"title":"Healthy","panels":[]}).to_string()}
    })).collect();
    serde_json::json!({
        "apiVersion":"v1","kind":"ConfigMapList","metadata":{"continue":token},"items":items
    })
    .to_string()
}

fn assert_configmap_lists(script: &Script, count: usize) {
    let requests: Vec<_> = script
        .requests_for("/api/v1/configmaps?")
        .into_iter()
        .filter(|request| request.path.contains("grafana_dashboard"))
        .collect();
    assert_eq!(requests.len(), count);
    for request in requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.accept, "");
        assert_eq!(request.content_type, "");
        assert!(request.body.is_empty());
        assert!(request.path.contains("limit=200"));
        assert!(
            request
                .path
                .contains("labelSelector=grafana_dashboard+in+%281%2Ctrue%29"),
            "{}",
            request.path
        );
    }
    for request in script.requests_for("secrets") {
        assert!(
            request.accept.contains("PartialObjectMetadata"),
            "a provisioning read never falls back to Secret values: {request:?}"
        );
    }
}

#[test]
fn oversized_provisioning_is_refused_without_a_secret_fallback() {
    let script = Script::default();
    script.route_bytes(
        "GET",
        "/api/v1/configmaps?",
        200,
        vec![b' '; k10s_data::reach::MAX_BODY_BYTES + 1],
    );
    let answer =
        runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await });
    assert!(
        matches!(answer, Fetched::Failed { why, .. } if why.contains("bytes") && why.contains("refused"))
    );
    assert_configmap_lists(&script, 1);
}

#[test]
fn provisioning_continuations_cannot_loop_or_exceed_the_page_ceiling() {
    for repeated in [false, true] {
        let script = Script::default();
        let pages = if repeated { 2 } else { 16 };
        for index in 0..pages {
            script.route(
                "GET",
                "/api/v1/configmaps?",
                200,
                configmap_page(
                    None,
                    &if repeated {
                        "same".to_string()
                    } else {
                        format!("page-{index}")
                    },
                ),
            );
        }
        let answer =
            runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await });
        assert!(
            matches!(answer, Fetched::Failed { why, .. } if why.contains(if repeated { "repeated a continuation token" } else { "16 pages" }))
        );
        assert_configmap_lists(&script, pages);
    }
}

#[test]
fn nonconsecutive_provisioning_tokens_refuse_before_repeating_a_page() {
    const LIST: &str =
        "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200";
    for healthy in [false, true] {
        let script = Script::default();
        for (uid, token) in [
            (healthy.then_some("healthy"), "A"),
            (None, "B"),
            (None, "A"),
            (None, ""),
        ] {
            script.route(
                "GET",
                "/api/v1/configmaps?",
                200,
                configmap_page(uid, token),
            );
        }
        let answer =
            runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await });
        let why = "provisioned dashboards repeated a continuation token";
        if healthy {
            let Fetched::Ok(provisioned) = answer else {
                panic!("healthy earlier pages remain available with their failure");
            };
            assert_eq!(provisioned.dashboards.len(), 1);
            assert_eq!(provisioned.dashboards[0].uid, "healthy");
            assert_eq!(provisioned.errors, [why]);
            assert!(provisioned.truncated);
        } else {
            assert_eq!(
                answer,
                Fetched::Failed {
                    what: "grafana",
                    why: why.to_string(),
                }
            );
        }
        assert_configmap_lists(&script, 3);
        let paths: Vec<_> = script
            .requests_for("/api/v1/configmaps?")
            .into_iter()
            .map(|request| request.path)
            .collect();
        assert_eq!(
            paths,
            [
                LIST.to_string(),
                format!("{LIST}&continue=A"),
                format!("{LIST}&continue=B"),
            ]
        );
    }
}

#[test]
fn provisioning_pages_share_their_response_body_budget() {
    let script = Script::default();
    let mut first = configmap_page(None, "next");
    first.push_str(&" ".repeat((4 << 20) - first.len()));
    script.route("GET", "/api/v1/configmaps?", 200, first);
    script.route_bytes("GET", "/api/v1/configmaps?", 200, vec![b' '; (4 << 20) + 1]);
    let answer =
        runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await });
    assert!(matches!(answer, Fetched::Failed { why, .. } if why.contains("4194304 bytes")));
    assert_configmap_lists(&script, 2);
}

#[test]
fn a_failed_later_provisioning_page_keeps_healthy_dashboards_with_the_server_sentence() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        configmap_page(Some("healthy"), "next"),
    );
    script.route(
        "GET",
        "/api/v1/configmaps?",
        500,
        r#"{"message":"provisioning store unavailable"}"#,
    );
    let Fetched::Ok(provisioned) =
        runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await })
    else {
        panic!("healthy earlier pages remain available with their failure");
    };
    assert_eq!(provisioned.dashboards[0].uid, "healthy");
    assert!(provisioned.truncated);
    assert_eq!(provisioned.errors, ["provisioning store unavailable"]);
    assert_configmap_lists(&script, 2);
    assert!(
        script.requests_for("/api/v1/configmaps?")[1]
            .path
            .contains("continue=next")
    );
}

#[test]
fn provisioning_denial_remains_explicit_even_after_a_healthy_page() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        configmap_page(Some("healthy"), "next"),
    );
    script.route(
        "GET",
        "/api/v1/configmaps?",
        403,
        r#"{"message":"provisioning list denied"}"#,
    );
    assert_eq!(
        runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await }),
        Fetched::Denied { what: "grafana" }
    );
    assert_configmap_lists(&script, 2);
}

#[test]
fn live_catalog_rows_keep_the_named_failure_of_provisioning() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        500,
        r#"{"message":"provisioning store unavailable"}"#,
    );
    catalog_source(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        r#"[{"uid":"healthy","title":"Healthy","type":"dash-db"}]"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/healthy",
        200,
        r#"{"uid":"healthy","title":"Healthy","panels":[]}"#,
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("healthy live data survives a provisioning failure");
    };
    assert_eq!(catalog.dashboards[0].uid, "healthy");
    assert_eq!(
        catalog.errors,
        ["provisioned dashboards: provisioning store unavailable"]
    );
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert_proxy_get(&script, "api/dashboards/uid/healthy");
}

#[test]
fn provisioned_catalog_rows_keep_a_failed_binding_without_becoming_an_empty_success() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        configmap_page(Some("healthy"), ""),
    );
    script.route(
        "GET",
        "/api/v1/services?",
        500,
        r#"{"message":"Service discovery unavailable"}"#,
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("healthy provisioned data survives a binding failure");
    };
    assert_eq!(catalog.dashboards[0].uid, "healthy");
    assert_eq!(
        catalog.errors,
        ["Grafana binding: Service discovery unavailable"]
    );
    assert_configmap_lists(&script, 1);
    assert!(script.requests_for("/proxy/").is_empty());
    let requests = script.requests_for("/api/v1/services?");
    let request = requests
        .iter()
        .find(|request| !request.path.contains("watch=true"))
        .expect("discovery list");
    assert_eq!(request.method, "GET");
    assert_eq!(request.accept, "");
    assert!(request.body.is_empty());
}

#[test]
fn partial_provisioning_errors_survive_the_catalog_projection() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        configmap_page(Some("healthy"), "next"),
    );
    script.route(
        "GET",
        "/api/v1/configmaps?",
        500,
        r#"{"message":"provisioning store unavailable"}"#,
    );
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        r#"{"apiVersion":"v1","kind":"ServiceList","items":[]}"#,
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("partial provisioning remains available with its error");
    };
    assert_eq!(catalog.dashboards[0].uid, "healthy");
    assert!(catalog.truncated);
    assert_eq!(
        catalog.errors,
        ["provisioned dashboards: provisioning store unavailable"]
    );
    assert_configmap_lists(&script, 2);
    assert!(script.requests_for("/proxy/").is_empty());
}

#[test]
fn named_live_dashboard_failures_keep_other_live_and_provisioned_rows() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_reader(&runtime, &script, true);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        r#"[{"uid":"broken","type":"dash-db"},{"uid":"healthy","type":"dash-db"}]"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/broken",
        500,
        r#"{"message":"dashboard storage unavailable"}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/healthy",
        200,
        r#"{"uid":"healthy","title":"Healthy","panels":[]}"#,
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("healthy catalog rows remain usable");
    };
    assert_eq!(catalog.dashboards.len(), 2);
    assert_eq!(
        catalog.errors,
        ["Grafana dashboard broken: dashboard storage unavailable"]
    );
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert_proxy_get(&script, "api/dashboards/uid/broken");
    assert_proxy_get(&script, "api/dashboards/uid/healthy");
}

#[test]
fn broken_file_diagnostics_survive_a_later_provisioning_failure_and_healthy_live_rows() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
            "kind":"ConfigMapList","apiVersion":"v1","metadata":{"continue":"next"},
            "items":[{"metadata":{"name":"cluster-dash","namespace":"monitoring"},
                "data":{"broken.json":"{"}}]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/api/v1/configmaps?",
        500,
        r#"{"message":"provisioning store unavailable"}"#,
    );
    catalog_source(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        r#"[{"uid":"healthy","title":"Healthy","type":"dash-db"}]"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/healthy",
        200,
        r#"{"uid":"healthy","title":"Healthy","panels":[]}"#,
    );
    let before = script.seen().len();
    let answer = catalog_reply(&sync);
    let Fetched::Ok(catalog) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(catalog.dashboards.len(), 1);
    assert_eq!(catalog.dashboards[0].uid, "healthy");
    assert!(catalog.truncated);
    assert_eq!(catalog.errors.len(), 2);
    assert!(catalog.errors[0].starts_with(
        "provisioned dashboards: ConfigMap monitoring/cluster-dash key \"broken.json\": dashboard JSON did not parse:"
    ));
    assert_eq!(
        catalog.errors[1],
        "provisioned dashboards: provisioning store unavailable"
    );
    let rows = after_sync(&script, before);
    let expected = [
        (
            "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200",
            "",
        ),
        (
            "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200&continue=next",
            "",
        ),
        ("/api/v1/services?&limit=200", ""),
        (
            "/api/v1/namespaces/monitoring/services/grafana:http/proxy/api/health",
            "application/json",
        ),
        (
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search?type=dash-db&limit=100",
            "application/json",
        ),
        (
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/healthy",
            "application/json",
        ),
    ];
    assert_eq!(rows.len(), expected.len(), "{rows:?}");
    for (row, (path, accept)) in rows.iter().zip(expected) {
        assert_eq!(row.method, "GET");
        assert_eq!(row.path, path);
        assert_eq!(row.accept, accept);
        assert_eq!(row.content_type, "");
        assert!(row.body.is_empty(), "{row:?}");
    }
}

#[test]
fn failed_named_dashboards_still_consume_the_fixed_attempt_ceiling() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_reader(&runtime, &script, true);
    let hits: Vec<_> = (0..18)
        .map(|index| serde_json::json!({"uid":format!("item-{index}"),"type":"dash-db"}))
        .collect();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        serde_json::to_string(&hits).expect("hits"),
    );
    for index in 0..16 {
        script.route("GET", &format!("/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/item-{index}"), 500, r#"{"message":"dashboard storage unavailable"}"#);
    }
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("failed dashboards remain named notices");
    };
    assert_eq!(catalog.dashboards.len(), 1);
    assert_eq!(catalog.errors.len(), 16);
    assert_eq!(catalog.extra_hits.len(), 2);
    assert!(catalog.truncated);
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    for index in 0..16 {
        assert_proxy_get(&script, &format!("api/dashboards/uid/item-{index}"));
    }
    assert_eq!(script.requests_for("/proxy/api/dashboards/uid/").len(), 16);
}

#[tokio::test(start_paused = true)]
async fn provisioning_is_bounded_while_waiting_for_the_response() {
    let script = Script::default();
    script.route_hanging("GET", "/api/v1/configmaps?");
    let started = tokio::time::Instant::now();
    let answer = fetch_provisioned_from_configmaps(&script.client()).await;
    assert!(
        matches!(answer, Fetched::Failed { why, .. } if why == "grafana did not answer within 4 seconds")
    );
    assert_eq!(started.elapsed(), Duration::from_secs(4));
    assert_configmap_lists(&script, 1);
}
