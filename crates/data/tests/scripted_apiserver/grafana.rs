//! Grafana dashboard fetch: the service proxy, provisioned ConfigMaps, and
//! catalog failures that must survive a successful health probe.

use crate::*;
use k10s_data::integrations::grafana::{
    CatalogHitReason, fetch_dashboard, fetch_provisioned_from_configmaps, fetch_search,
};
use k10s_data::reach::{Bound, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;

#[test]
fn watches_keep_catalog_list_routes_and_explicit_watch_routes_separate() {
    use futures::FutureExt;
    use kube::api::{ListParams, WatchParams};
    use kube::core::Request;

    for (path, metadata) in [("/api/v1/configmaps", true), ("/api/v1/services", false)] {
        for explicit in [false, true] {
            let script = Script::default();
            let list_route = format!("{path}?");
            let watch_route = format!("{path}?watch=true");
            script.route("GET", &list_route, 200, "{}");
            if explicit {
                script.route("GET", &watch_route, 204, "");
            }
            let request = Request::new(path.to_string());
            let watch = if metadata {
                request.watch_metadata(&WatchParams::default(), "1000")
            } else {
                request.watch(&WatchParams::default(), "1000")
            }
            .expect("fixed watch parameters")
            .map(Body::from);
            let mut server = script.clone();
            let response = server.call(watch).now_or_never();
            assert_eq!(response.is_some(), explicit, "{path}, explicit {explicit}");
            if let Some(response) = response {
                assert_eq!(response.expect("scripted watch").status(), 204);
            }
            assert_eq!(script.answered("GET", &list_route), (1, 0));
            if explicit {
                assert_eq!(script.answered("GET", &watch_route), (1, 1));
            }

            let mut params = ListParams::default().limit(200);
            if metadata {
                params = params.labels("grafana_dashboard in (1,true)");
            }
            let list = request
                .list(&params)
                .expect("fixed list parameters")
                .map(Body::from);
            let response = server
                .call(list)
                .now_or_never()
                .expect("the list answers without waiting")
                .expect("scripted list");
            assert_eq!(response.status(), 200);
            assert_eq!(script.answered("GET", &list_route), (1, 1));
            let seen = script.seen();
            assert_eq!(seen.len(), 2);
            assert_eq!(
                seen[0].path,
                format!(
                    "{path}?&watch=true&timeoutSeconds=290&allowWatchBookmarks=true&resourceVersion=1000"
                )
            );
            assert_eq!(
                seen[0].accept,
                if metadata {
                    "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1"
                } else {
                    ""
                },
            );
            assert_eq!(
                seen[0].content_type,
                if metadata { "application/json" } else { "" }
            );
            assert_eq!(
                seen[1].path,
                if metadata {
                    "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200"
                } else {
                    "/api/v1/services?&limit=200"
                },
            );
            assert_eq!(seen[1].accept, "");
            assert_eq!(seen[1].content_type, "");
            for request in seen {
                assert_eq!(request.method, "GET");
                assert!(request.body.is_empty());
            }
        }
    }
}

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
    let Fetched::Ok(page) = fetched else {
        panic!("search must resolve: {fetched:?}");
    };
    assert_eq!(page.hits.len(), 1);
    assert_eq!(page.hits[0].uid, "k8s");
    assert_eq!(page.hits[0].folder_title, "Kubernetes");
    assert!(!page.page_full);

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
fn search_page_fullness_precedes_folder_filtering() {
    for count in [99, 100, 101] {
        let script = Script::default();
        let hits: Vec<_> = (0..count)
            .map(|index| {
                serde_json::json!({
                    "uid":format!("item-{index}"),"title":"Dashboard",
                    "folderTitle":"Other","type":"dash-db"
                })
            })
            .collect();
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            200,
            serde_json::to_string(&hits).expect("search fixture"),
        );
        let fetched = runtime().block_on(async {
            fetch_search(&script.client(), &proxy_bound(), &["Wanted".into()]).await
        });
        let Fetched::Ok(page) = fetched else {
            panic!("search must resolve: {fetched:?}");
        };
        assert!(page.hits.is_empty());
        assert_eq!(page.page_full, count >= 100);
        assert_eq!(script.seen().len(), 1);
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    }
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
fn unsupported_dashboard_uids_are_refused_before_a_request() {
    let runtime = runtime();
    for uid in [
        "".to_string(),
        "cluster/pods".to_string(),
        "../api/search".to_string(),
        "cluster?orgId=2".to_string(),
        "cluster#ignored".to_string(),
        "%2e%2e%2fapi%2fsearch".to_string(),
        "%2E%2E%2Fapi%2Fsearch".to_string(),
        "cluster%2fpods".to_string(),
        "cluster\\pods".to_string(),
        "cluster pods".to_string(),
        "cluster\npods".to_string(),
        "cluster\rpods".to_string(),
        "cluster\0pods".to_string(),
        "cl\u{fc}ster".to_string(),
        "\u{202e}cluster".to_string(),
        "a".repeat(41),
        "a".repeat(8_192),
    ] {
        let script = Script::default();
        let fetched = runtime
            .block_on(async { fetch_dashboard(&script.client(), &proxy_bound(), &uid).await });
        let Fetched::Failed {
            what: "grafana",
            why,
        } = fetched
        else {
            panic!("an unsupported dashboard UID is refused: {fetched:?}");
        };
        assert_eq!(
            why,
            "unsupported Grafana dashboard UID; expected 1 to 40 ASCII letters, digits, '-' or '_'",
            "{uid:?}",
        );
        assert!(why.is_ascii());
        assert!(!why.chars().any(char::is_control));
        assert!(script.seen().is_empty(), "{uid:?}: {:?}", script.seen());
    }
}

#[test]
fn short_and_forty_byte_dashboard_uids_remain_exact_path_segments() {
    let runtime = runtime();
    for uid in [
        "a".to_string(),
        "0".to_string(),
        "-".to_string(),
        "_".to_string(),
        "k8s".to_string(),
        format!("{}Az09", "Az09_-".repeat(6)),
    ] {
        let script = Script::default();
        let rest = format!("api/dashboards/uid/{uid}");
        script.route(
            "GET",
            &format!("/api/v1/namespaces/monitoring/services/grafana:3000/proxy/{rest}"),
            200,
            serde_json::json!({"dashboard":{"uid":uid,"title":"Cluster","panels":[]}}).to_string(),
        );
        let fetched = runtime
            .block_on(async { fetch_dashboard(&script.client(), &proxy_bound(), &uid).await });
        let Fetched::Ok(dashboard) = fetched else {
            panic!("a supported dashboard UID remains readable: {fetched:?}");
        };
        assert_eq!(dashboard.uid, uid);
        assert_eq!(script.seen().len(), 1);
        assert_proxy_get(&script, &rest);
    }
}

#[test]
fn invalid_search_uids_never_label_errors_consume_fetches_or_become_browser_hits() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_reader(&runtime, &script, false);
    let invalid = [
        "cluster/pods".to_string(),
        "cluster?orgId=2".to_string(),
        "cluster#ignored".to_string(),
        "%2e%2e%2fapi%2fsearch".to_string(),
        "cluster\n\u{202e}pods".to_string(),
        "a".repeat(8_192),
        String::new(),
    ];
    let mut hits: Vec<_> = invalid
        .iter()
        .map(|uid| serde_json::json!({"uid":uid,"type":"dash-db"}))
        .collect();
    for index in 0..k10s_data::integrations::grafana::MAX_NAMED_FROM_SEARCH {
        let uid = format!("valid-{index}");
        hits.push(serde_json::json!({"uid":uid,"type":"dash-db"}));
        script.route(
            "GET",
            &format!(
                "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/{uid}"
            ),
            200,
            serde_json::json!({"dashboard":{"uid":uid,"title":"Healthy","panels":[{
                "id":1,"title":"Up","type":"timeseries",
                "datasource":{"type":"prometheus","uid":"prom-main"},
                "targets":[{"refId":"A","expr":"up"}]
            }]}})
            .to_string(),
        );
    }
    hits.extend(
        invalid
            .iter()
            .map(|uid| serde_json::json!({"uid":uid,"type":"dash-db"})),
    );
    hits.push(serde_json::json!({"uid":"browser-only","title":"Browser","type":"dash-db"}));
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        serde_json::Value::Array(hits).to_string(),
    );
    let fetched = catalog_reply(&sync);
    let Fetched::Ok(catalog) = fetched else {
        panic!("valid dashboard rows remain independently readable: {fetched:?}");
    };
    assert_eq!(catalog.dashboards.len(), 16);
    for (index, dashboard) in catalog.dashboards.iter().enumerate() {
        assert_eq!(dashboard.uid, format!("valid-{index}"));
        assert_eq!(dashboard.panels.len(), 1);
        assert!(dashboard.panels[0].unsupported.is_none());
        assert_eq!(dashboard.panels[0].queries[0].expr, "up");
        assert!(dashboard.panels[0].queries[0].unsupported.is_none());
        assert_proxy_get(&script, &format!("api/dashboards/uid/valid-{index}"));
    }
    assert!(catalog.served);
    assert!(catalog.truncated);
    assert_eq!(catalog.extra_hits.len(), 1);
    assert_eq!(catalog.extra_hits[0].search.uid, "browser-only");
    assert_eq!(catalog.extra_hits[0].reason, CatalogHitReason::FetchLimit);
    assert_eq!(
        catalog.errors,
        ["unsupported Grafana dashboard UID; expected 1 to 40 ASCII letters, digits, '-' or '_'"],
    );
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert_configmap_lists(&script, 1);
    assert_eq!(script.requests_for("/proxy/").len(), 18);
    assert_eq!(script.requests_for("/proxy/api/dashboards/").len(), 16);
}

#[test]
fn a_mismatched_missing_or_malformed_dashboard_uid_is_refused_after_one_get() {
    for (dashboard, expected) in [
        (
            r#"{"uid":"other","title":"Other dashboard","panels":[]}"#,
            "Grafana returned dashboard UID \"other\" for requested UID \"k8s\"",
        ),
        (
            r#"{"title":"No UID","panels":[]}"#,
            "Grafana returned dashboard UID \"\" for requested UID \"k8s\"",
        ),
        (
            r#"{"uid":10,"title":"Wrong UID type","panels":[]}"#,
            "unsupported Grafana dashboard UID; expected 1 to 40 ASCII letters, digits, '-' or '_'",
        ),
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
        assert_eq!(why, expected);
        assert_eq!(script.seen().len(), 1);
        assert_proxy_get(&script, "api/dashboards/uid/k8s");
    }
}

#[test]
fn an_invalid_returned_dashboard_uid_is_not_echoed() {
    for uid in ["../unsafe?key=value".to_string(), "x".repeat(8_192)] {
        let script = Script::default();
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/k8s",
            200,
            serde_json::json!({"dashboard":{"uid":uid,"title":"Dashboard","panels":[]}})
                .to_string(),
        );
        let fetched = runtime()
            .block_on(async { fetch_dashboard(&script.client(), &proxy_bound(), "k8s").await });
        let Fetched::Failed {
            what: "grafana",
            why,
        } = fetched
        else {
            panic!("a malformed response identity must be refused");
        };
        assert_eq!(
            why,
            "unsupported Grafana dashboard UID; expected 1 to 40 ASCII letters, digits, '-' or '_'"
        );
        assert!(!why.contains(&uid));
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
fn catalog_reports_dashboard_target_limits_alongside_failed_sources() {
    use k10s_data::integrations::grafana::MAX_QUERIES_PER_PANEL;

    for provisioned in [false, true] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_connection(&runtime, &script);
        let targets = vec![serde_json::json!({"expr":"up"}); MAX_QUERIES_PER_PANEL + 1];
        let dashboard = serde_json::json!({
            "uid":"large","title":"Large","panels":[{
                "id":1,"title":"Up","type":"timeseries","targets":targets
            }]
        });
        let items = if provisioned {
            serde_json::json!([{
                "metadata":{"name":"dashboards","namespace":"monitoring"},
                "data":{"large.json":dashboard.to_string()}
            }])
        } else {
            serde_json::json!([])
        };
        script.route(
            "GET",
            "/api/v1/configmaps?",
            200,
            serde_json::json!({"apiVersion":"v1","kind":"ConfigMapList","items":items}).to_string(),
        );
        catalog_source(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            200,
            r#"[{"uid":"large","type":"dash-db"},{"uid":"broken","type":"dash-db"}]"#,
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/large",
            200,
            serde_json::json!({"dashboard":dashboard}).to_string(),
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/broken",
            500,
            r#"{"message":"dashboard storage unavailable"}"#,
        );
        let Fetched::Ok(catalog) = catalog_reply(&sync) else {
            panic!("the readable dashboard and failed source must survive together");
        };
        assert!(catalog.truncated);
        assert_eq!(catalog.dashboards.len(), 1);
        for dashboard in &catalog.dashboards {
            assert!(dashboard.truncated);
            assert_eq!(dashboard.panels[0].queries.len(), MAX_QUERIES_PER_PANEL);
        }
        assert_eq!(
            catalog.errors,
            [
                "Grafana dashboard broken: dashboard storage unavailable",
                "Grafana dashboard large: panel or target records were not fully inspected after local limits"
            ]
        );
        assert_configmap_lists(&script, 1);
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
        assert_proxy_get(&script, "api/dashboards/uid/broken");
        assert_proxy_get(&script, "api/dashboards/uid/large");
    }
}

#[test]
fn matching_uids_keep_file_provenance_and_fetch_the_live_dashboard_once() {
    use k10s_data::integrations::grafana::DashboardSource;

    for status in [200, 403, 500] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_connection(&runtime, &script);
        let document = |title: &str, expr: &str| {
            serde_json::json!({"uid":"shared","title":title,"panels":[{
                "id":1,"title":"CPU","type":"timeseries",
                "datasource":{"type":"prometheus","uid":"prom"},
                "targets":[{"refId":"A","expr":expr}]
            }]})
            .to_string()
        };
        script.route(
            "GET",
            "/api/v1/configmaps?",
            200,
            serde_json::json!({
                "apiVersion":"v1","kind":"ConfigMapList","items":[{
                    "metadata":{"name":"dashboards","namespace":"monitoring"},
                    "data":{"first.json":document("First file", "file_one"),
                            "second.json":document("Second file", "file_two")}
                }]
            })
            .to_string(),
        );
        catalog_source(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            200,
            r#"[{"uid":"shared","type":"dash-db"},{"uid":"shared","type":"dash-db"}]"#,
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/shared",
            status,
            if status == 200 {
                document("Live", "live_query")
            } else {
                r#"{"message":"dashboard store unavailable"}"#.to_string()
            },
        );
        let Fetched::Ok(catalog) = catalog_reply(&sync) else {
            panic!("independent file definitions remain readable");
        };
        let files: Vec<_> = catalog
            .dashboards
            .iter()
            .filter(|dashboard| dashboard.source == DashboardSource::ConfigMap)
            .collect();
        assert_eq!(files.len(), if status == 200 { 0 } else { 2 });
        if status != 200 {
            assert_eq!(files[0].panels[0].queries[0].expr, "file_one");
            assert_eq!(files[1].panels[0].queries[0].expr, "file_two");
        }
        let live: Vec<_> = catalog
            .dashboards
            .iter()
            .filter(|dashboard| dashboard.source == DashboardSource::Grafana)
            .collect();
        assert_eq!(live.len(), usize::from(status == 200));
        if let Some(dashboard) = live.first() {
            assert_eq!(dashboard.panels[0].queries[0].expr, "live_query");
        }
        let expected = match status {
            200 => Vec::new(),
            403 => vec!["Grafana dashboard shared: access denied for this account"],
            _ => vec!["Grafana dashboard shared: dashboard store unavailable"],
        };
        assert_eq!(catalog.errors, expected);
        assert_eq!(catalog.extra_hits.len(), usize::from(status != 200));
        if status != 200 {
            assert_eq!(catalog.extra_hits[0].search.uid, "shared");
            assert_eq!(
                catalog.extra_hits[0].reason,
                if status == 403 {
                    CatalogHitReason::Denied
                } else {
                    CatalogHitReason::Failed
                }
            );
        }
        assert_configmap_lists(&script, 1);
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
        assert_proxy_get(&script, "api/dashboards/uid/shared");
    }
}

#[test]
fn catalog_keeps_the_discovered_grafana_port_when_the_service_has_other_ports() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        r#"{"apiVersion":"v1","kind":"ConfigMapList","items":[]}"#,
    );
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        r#"{"apiVersion":"v1","kind":"ServiceList","items":[{
            "metadata":{"name":"grafana","namespace":"monitoring"},
            "spec":{"ports":[{"name":"metrics","port":9090},{"name":"http","port":3000}]}
        }]}"#,
    );
    let health = "/api/v1/namespaces/monitoring/services/grafana:http/proxy/api/health";
    script.route("GET", health, 200, r#"{"database":"ok"}"#);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        "[]",
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("the Grafana catalogue must retain its browser service");
    };
    assert_eq!(
        catalog.browser_service,
        Some(("monitoring".into(), "grafana".into(), 3000))
    );
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    let probes = script.requests_for(health);
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].method, "GET");
    assert_eq!(probes[0].path, health);
    assert_eq!(probes[0].accept, "application/json");
    assert_eq!(probes[0].content_type, "");
    assert!(probes[0].body.is_empty());
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
             "data":{"dashboard.json":"{\"uid\":\"provisioned\",\"title\":\"Provisioned\",\"panels\":[{\"id\":1,\"title\":\"Up\",\"type\":\"timeseries\",\"datasource\":{\"type\":\"prometheus\",\"uid\":\"prom-main\"},\"targets\":[{\"refId\":\"A\",\"expr\":\"up\"}]}]}"}}]"#
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
fn a_full_search_page_keeps_file_sources_and_fetches_a_bounded_live_prefix() {
    use k10s_data::integrations::grafana::MAX_NAMED_FROM_SEARCH;

    for count in [99, 100, 101] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_connection(&runtime, &script);
        let mut files = serde_json::Map::new();
        let mut hits = Vec::new();
        for index in 0..count {
            let uid = format!("item-{index}");
            let dashboard = serde_json::json!({"uid":uid,"title":"Dashboard","panels":[]});
            files.insert(
                format!("dashboard-{index:03}.json"),
                dashboard.to_string().into(),
            );
            hits.push(serde_json::json!({"uid":uid,"type":"dash-db"}));
        }
        script.route(
            "GET",
            "/api/v1/configmaps?",
            200,
            serde_json::json!({
                "apiVersion":"v1","kind":"ConfigMapList","items":[{
                    "metadata":{"name":"dashboards","namespace":"monitoring"},"data":files
                }]
            })
            .to_string(),
        );
        catalog_source(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            200,
            serde_json::to_string(&hits).expect("search fixture"),
        );
        for index in 0..MAX_NAMED_FROM_SEARCH {
            let uid = format!("item-{index}");
            script.route(
                "GET",
                &format!("/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/{uid}"),
                200,
                serde_json::json!({"uid":uid,"title":"Live","panels":[]}).to_string(),
            );
        }
        let Fetched::Ok(catalog) = catalog_reply(&sync) else {
            panic!("the provisioned catalogue remains readable");
        };
        assert!(catalog.served);
        assert_eq!(catalog.dashboards.len(), count);
        assert_eq!(
            catalog.extra_hits.len(),
            count.min(100) - MAX_NAMED_FROM_SEARCH
        );
        assert!(catalog.truncated);
        if count >= 100 {
            assert_eq!(
                catalog.errors,
                ["Grafana search returned its result limit; more dashboards may exist"]
            );
        } else {
            assert!(catalog.errors.is_empty());
        }
        assert_configmap_lists(&script, 1);
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
        for index in 0..MAX_NAMED_FROM_SEARCH {
            assert_proxy_get(&script, &format!("api/dashboards/uid/item-{index}"));
        }
        assert_eq!(
            script.requests_for("/proxy/api/dashboards/").len(),
            MAX_NAMED_FROM_SEARCH
        );
    }
}

#[test]
fn a_full_search_page_notice_precedes_many_provisioning_errors() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    let broken: serde_json::Map<String, serde_json::Value> = (0..10)
        .map(|index| (format!("broken-{index}.json"), "{".into()))
        .collect();
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMapList","items":[{
                "metadata":{"name":"dashboards","namespace":"monitoring"},"data":broken
            }]
        })
        .to_string(),
    );
    catalog_source(&script);
    let hits: Vec<_> = (0..100)
        .map(|index| serde_json::json!({"uid":format!("invalid/{index}"),"type":"dash-db"}))
        .collect();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        serde_json::to_string(&hits).expect("search fixture"),
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("a served catalogue retains its source notices");
    };
    assert_eq!(
        catalog.errors[0],
        "Grafana search returned its result limit; more dashboards may exist"
    );
    assert_eq!(catalog.errors.len(), 12);
    assert!(
        catalog.errors[1..=10]
            .iter()
            .all(|notice| notice.starts_with("provisioned dashboards: "))
    );
    assert!(catalog.dashboards.is_empty());
    assert!(catalog.extra_hits.is_empty());
    assert!(!catalog.truncated);
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
}

#[test]
fn invalid_provisioned_uids_are_reported_without_losing_healthy_dashboards() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    let unsafe_uid = "../unsafe?key=value";
    let oversized_uid = "x".repeat(8_192);
    let dashboard = |uid: &str| {
        serde_json::json!({"uid":uid,"title":"Dashboard","panels":[{
            "id":1,"title":"Up","type":"timeseries",
            "datasource":{"type":"prometheus","uid":"prom-main"},
            "targets":[{"refId":"A","expr":"up"}]
        }]})
        .to_string()
    };
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMapList","items":[{
                "metadata":{"name":"dashboards","namespace":"monitoring"},
                "data":{
                    "bad.json":dashboard(unsafe_uid),
                    "non-string.json":r#"{"uid":42,"title":"Dashboard","panels":[]}"#,
                    "oversized":dashboard(&oversized_uid),
                    "healthy.json":dashboard("healthy"),
                    "uidless.json":dashboard("")
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
        r#"[{"uid":"healthy","type":"dash-db"}]"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/healthy",
        200,
        dashboard("healthy"),
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("healthy dashboard data survives invalid siblings");
    };
    assert!(catalog.served);
    assert!(!catalog.truncated);
    assert_eq!(catalog.dashboards.len(), 2);
    assert!(
        catalog
            .dashboards
            .iter()
            .any(|dashboard| dashboard.uid == "healthy")
    );
    assert!(
        catalog
            .dashboards
            .iter()
            .any(|dashboard| dashboard.uid.is_empty())
    );
    for dashboard in &catalog.dashboards {
        assert_readable_panel(dashboard);
    }
    assert!(catalog.extra_hits.is_empty());
    assert_eq!(catalog.errors.len(), 3);
    for (notice, key) in catalog
        .errors
        .iter()
        .zip(["bad.json", "non-string.json", "oversized"])
    {
        assert_eq!(
            notice,
            &format!(
                "provisioned dashboards: ConfigMap monitoring/dashboards key {key:?}: unsupported Grafana dashboard UID; expected 1 to 40 ASCII letters, digits, '-' or '_'"
            )
        );
        assert!(!notice.contains(unsafe_uid));
        assert!(!notice.contains(&oversized_uid));
    }
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert_proxy_get(&script, "api/dashboards/uid/healthy");
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
fn a_failed_dashboard_retains_its_search_hit_and_failure() {
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
    assert_eq!(catalog.extra_hits.len(), 1);
    assert_eq!(catalog.extra_hits[0].search.uid, "broken");
    assert_eq!(catalog.extra_hits[0].reason, CatalogHitReason::Failed);
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
    assert_eq!(catalog.extra_hits.len(), 1);
    assert_eq!(catalog.extra_hits[0].search.uid, "requested");
    assert_eq!(catalog.extra_hits[0].reason, CatalogHitReason::Failed);
    assert_eq!(
        catalog.errors,
        [
            "Grafana dashboard requested: Grafana returned dashboard UID \"other\" for requested UID \"requested\""
        ]
    );
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert_proxy_get(&script, "api/dashboards/uid/requested");
}

fn assert_readable_panel(dashboard: &k10s_data::integrations::grafana::Dashboard) {
    assert_eq!(dashboard.panels.len(), 1);
    let panel = &dashboard.panels[0];
    assert_eq!(
        panel.kind,
        k10s_data::integrations::grafana::PanelKind::Timeseries
    );
    assert!(panel.unsupported.is_none());
    assert_eq!(panel.queries.len(), 1);
    assert_eq!(panel.queries[0].ref_id, "A");
    assert_eq!(panel.queries[0].expr, "up");
    assert_eq!(panel.queries[0].datasource.as_deref(), Some("prom-main"));
    assert!(panel.queries[0].unsupported.is_none());
}

#[test]
fn a_denied_search_keeps_provisioned_rows_or_remains_a_named_denial() {
    for provisioned in [false, true] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_reader(&runtime, &script, provisioned);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            403,
            r#"{"apiVersion":"v1","kind":"Status","status":"Failure","code":403,
                 "reason":"Forbidden","message":"search is forbidden"}"#,
        );
        let answer = catalog_reply(&sync);
        if provisioned {
            let Fetched::Ok(catalog) = answer else {
                panic!("readable provisioned dashboards survive denied API search");
            };
            assert_eq!(catalog.dashboards[0].uid, "provisioned");
            assert_readable_panel(&catalog.dashboards[0]);
            assert!(catalog.served);
            assert_eq!(
                catalog.errors,
                ["Grafana search: access denied for this account"]
            );
        } else {
            assert!(matches!(
                answer,
                Fetched::Denied {
                    what: "Grafana search"
                }
            ));
        }
        assert_configmap_lists(&script, 1);
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
        assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
    }
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
    let items: Vec<_> = uid
        .into_iter()
        .map(|uid| {
            serde_json::json!({
                "metadata":{"name":"owned-dashboard","namespace":"monitoring"},
                "data":{"dashboard.json":serde_json::json!({
                    "uid":uid,"title":"Healthy","panels":[{
                        "id":1,"title":"Up","type":"timeseries",
                        "datasource":{"type":"prometheus","uid":"prom-main"},
                        "targets":[{"refId":"A","expr":"up"}]
                    }]
                }).to_string()}
            })
        })
        .collect();
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
fn initial_provisioning_failures_name_configmaps_and_keep_the_cause() {
    const LIST: &str =
        "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200";
    for (status, body, sentence) in [
        (
            500,
            r#"{"message":"provisioning store unavailable"}"#,
            Some("provisioning store unavailable"),
        ),
        (
            200,
            r#"{"apiVersion":"v1","kind":"ConfigMapList","items":{}}"#,
            None,
        ),
    ] {
        let script = Script::default();
        script.route("GET", &LIST.replacen("?&", "?", 1), status, body);
        let answer =
            runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await });
        let Fetched::Failed {
            what: "configmaps",
            why,
        } = answer
        else {
            panic!("a failed provisioning read names its Kubernetes resource: {answer:?}");
        };
        if let Some(sentence) = sentence {
            assert_eq!(why, sentence);
        } else {
            assert!(why.starts_with("provisioned dashboards returned an unsupported response: "));
        }
        assert_configmap_lists(&script, 1);
        let requests = script.seen();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, LIST);
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
        matches!(answer, Fetched::Failed { what: "configmaps", why } if why.contains("bytes") && why.contains("refused"))
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
            matches!(answer, Fetched::Failed { what: "configmaps", why } if why.contains(if repeated { "repeated a continuation token" } else { "16 pages" }))
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
                    what: "configmaps",
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
    assert!(
        matches!(answer, Fetched::Failed { what: "configmaps", why } if why.contains("4194304 bytes"))
    );
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
fn a_later_provisioning_denial_keeps_readable_rows_and_names_configmaps() {
    for healthy in [false, true] {
        let script = Script::default();
        script.route(
            "GET",
            "/api/v1/configmaps?",
            200,
            configmap_page(healthy.then_some("healthy"), "next"),
        );
        script.route(
            "GET",
            "/api/v1/configmaps?",
            403,
            r#"{"message":"provisioning list denied"}"#,
        );
        let answer =
            runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await });
        if healthy {
            let Fetched::Ok(provisioned) = answer else {
                panic!("a denied later page must not remove a readable earlier dashboard");
            };
            assert_eq!(provisioned.dashboards[0].uid, "healthy");
            assert_readable_panel(&provisioned.dashboards[0]);
            assert!(provisioned.truncated);
            assert_eq!(
                provisioned.errors,
                ["configmaps: access denied for this account"]
            );
        } else {
            assert!(matches!(answer, Fetched::Denied { what: "configmaps" }));
        }
        assert_configmap_lists(&script, 2);
        assert!(
            script.requests_for("/api/v1/configmaps?")[1]
                .path
                .contains("continue=next")
        );
    }
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
    let answer = catalog_reply(&sync);
    let Fetched::Ok(catalog) = answer else {
        panic!(
            "partial provisioning remains available with its error: {answer:?}; requests: {:?}",
            script.seen(),
        );
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
    assert_eq!(catalog.extra_hits.len(), 18);
    assert!(
        catalog.extra_hits[..16]
            .iter()
            .all(|hit| hit.reason == CatalogHitReason::Failed)
    );
    assert!(
        catalog.extra_hits[16..]
            .iter()
            .all(|hit| hit.reason == CatalogHitReason::FetchLimit)
    );
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
        matches!(answer, Fetched::Failed { what: "configmaps", why } if why == "configmaps did not answer within 4 seconds")
    );
    assert_eq!(started.elapsed(), Duration::from_secs(4));
    assert_configmap_lists(&script, 1);
}

#[test]
fn denied_configmaps_do_not_block_a_served_native_api_catalog() {
    for populated in [false, true] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_connection(&runtime, &script);
        script.route(
            "GET",
            "/api/v1/configmaps?",
            403,
            r#"{"message":"provisioning list denied"}"#,
        );
        catalog_source(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            200,
            if populated {
                r#"[{"uid":"healthy","title":"Healthy","type":"dash-db"}]"#
            } else {
                "[]"
            },
        );
        if populated {
            script.route(
                "GET",
                "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/healthy",
                200,
                r#"{"uid":"healthy","title":"Healthy","panels":[{
                    "id":1,"title":"Pods","type":"timeseries",
                    "datasource":{"type":"prometheus","uid":"prom-main"},
                    "targets":[{"refId":"A","expr":"up"}]
                }]}"#,
            );
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let _stop = sync.reader.observe_grafana(move |answer| {
            let _ = tx.send(answer);
        });
        let Fetched::Ok(catalog) = wait(&rx) else {
            panic!("ConfigMap RBAC must not prevent the separately permitted API read");
        };
        assert!(catalog.served);
        assert_eq!(catalog.dashboards.len(), usize::from(populated));
        if populated {
            assert_eq!(catalog.dashboards[0].uid, "healthy");
            assert_readable_panel(&catalog.dashboards[0]);
        }
        assert_eq!(
            catalog.errors,
            ["provisioned dashboards: configmaps: access denied for this account"]
        );
        assert_configmap_lists(&script, 1);
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
        if populated {
            assert_proxy_get(&script, "api/dashboards/uid/healthy");
        } else {
            assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
        }
    }
}

#[test]
fn denied_configmaps_without_a_live_api_remain_a_named_denial() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        403,
        r#"{"message":"provisioning list denied"}"#,
    );
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        r#"{"apiVersion":"v1","kind":"ServiceList","items":[]}"#,
    );
    assert!(matches!(
        catalog_reply(&sync),
        Fetched::Denied { what: "configmaps" }
    ));
    assert_configmap_lists(&script, 1);
    assert!(script.requests_for("/proxy/").is_empty());
}

#[test]
fn denied_configmaps_and_search_remain_a_named_denial() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        403,
        r#"{"message":"provisioning list denied"}"#,
    );
    catalog_source(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        403,
        r#"{"message":"search is forbidden"}"#,
    );

    assert!(matches!(
        catalog_reply(&sync),
        Fetched::Denied {
            what: "provisioned dashboard configmaps and Grafana search"
        }
    ));
    assert_configmap_lists(&script, 1);
    let list = script
        .requests_for("/api/v1/configmaps?")
        .into_iter()
        .find(|request| request.path.contains("grafana_dashboard"))
        .expect("the provisioning list was requested");
    assert_eq!(
        list.path,
        "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200"
    );
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
}

#[test]
fn a_later_configmap_denial_keeps_a_provisioned_only_catalog_visible() {
    const LIST: &str =
        "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200";
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
        403,
        r#"{"message":"provisioning list denied"}"#,
    );
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        r#"{"apiVersion":"v1","kind":"ServiceList","items":[]}"#,
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("a partial ConfigMap catalogue works without a Grafana service");
    };
    assert!(catalog.served);
    assert!(catalog.truncated);
    assert_eq!(catalog.dashboards[0].uid, "healthy");
    assert_readable_panel(&catalog.dashboards[0]);
    assert_eq!(
        catalog.errors,
        ["provisioned dashboards: configmaps: access denied for this account"]
    );
    assert_configmap_lists(&script, 2);
    let paths: Vec<_> = script
        .requests_for("/api/v1/configmaps?")
        .into_iter()
        .filter(|request| request.path.contains("grafana_dashboard"))
        .map(|request| request.path)
        .collect();
    assert_eq!(paths, [LIST.to_string(), format!("{LIST}&continue=next")]);
    assert!(script.requests_for("/proxy/").is_empty());
}

#[test]
fn a_denied_dashboard_keeps_other_readable_catalog_rows() {
    for provisioned in [false, true] {
        for live in [false, true] {
            let script = Script::default();
            let runtime = runtime();
            let (sync, _live) = catalog_reader(&runtime, &script, provisioned);
            script.route(
                "GET",
                "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
                200,
                if live {
                    r#"[{"uid":"denied","type":"dash-db"},{"uid":"healthy","type":"dash-db"}]"#
                } else {
                    r#"[{"uid":"denied","type":"dash-db"}]"#
                },
            );
            script.route(
                "GET",
                "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/denied",
                403,
                r#"{"message":"dashboard is forbidden"}"#,
            );
            if live {
                script.route(
                    "GET",
                    "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/healthy",
                    200,
                    r#"{"uid":"healthy","title":"Healthy","panels":[{
                        "id":1,"title":"Up","type":"timeseries",
                        "datasource":{"type":"prometheus","uid":"prom-main"},
                        "targets":[{"refId":"A","expr":"up"}]
                    }]}"#,
                );
            }
            let Fetched::Ok(catalog) = catalog_reply(&sync) else {
                panic!("the denied search hit and readable dashboards must remain available");
            };
            let uids: Vec<_> = catalog
                .dashboards
                .iter()
                .map(|dashboard| dashboard.uid.as_str())
                .collect();
            let mut expected = Vec::new();
            if provisioned {
                expected.push("provisioned");
            }
            if live {
                expected.push("healthy");
            }
            assert_eq!(uids, expected);
            for dashboard in &catalog.dashboards {
                assert_readable_panel(dashboard);
            }
            assert!(catalog.served);
            assert_eq!(catalog.extra_hits.len(), 1);
            assert_eq!(catalog.extra_hits[0].search.uid, "denied");
            assert_eq!(catalog.extra_hits[0].reason, CatalogHitReason::Denied);
            assert_eq!(
                catalog.errors,
                ["Grafana dashboard denied: access denied for this account"]
            );
            assert_configmap_lists(&script, 1);
            assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
            assert_proxy_get(&script, "api/dashboards/uid/denied");
            if live {
                assert_proxy_get(&script, "api/dashboards/uid/healthy");
            }
        }
    }
}

#[test]
fn failed_named_dashboards_in_an_empty_catalog_keep_all_causes() {
    for configmap_status in [200, 403] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_connection(&runtime, &script);
        let configmaps = if configmap_status == 200 {
            configmap_page(None, "")
        } else {
            r#"{"message":"provisioning is forbidden"}"#.to_string()
        };
        script.route("GET", "/api/v1/configmaps?", configmap_status, configmaps);
        catalog_source(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            200,
            r#"[{"uid":"locked","type":"dash-db"},{"uid":"broken","type":"dash-db"}]"#,
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/locked",
            403,
            r#"{"message":"dashboard is forbidden"}"#,
        );
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/broken",
            500,
            r#"{"message":"dashboard store unavailable"}"#,
        );
        let Fetched::Ok(catalog) = catalog_reply(&sync) else {
            panic!("readable search hits must survive their dashboard read failures");
        };
        assert!(catalog.dashboards.is_empty());
        assert_eq!(catalog.extra_hits.len(), 2);
        assert_eq!(catalog.extra_hits[0].search.uid, "locked");
        assert_eq!(catalog.extra_hits[0].reason, CatalogHitReason::Denied);
        assert_eq!(catalog.extra_hits[1].search.uid, "broken");
        assert_eq!(catalog.extra_hits[1].reason, CatalogHitReason::Failed);
        let mut errors = Vec::new();
        if configmap_status == 403 {
            errors.push("provisioned dashboards: configmaps: access denied for this account");
        }
        errors.push("Grafana dashboard locked: access denied for this account");
        errors.push("Grafana dashboard broken: dashboard store unavailable");
        assert_eq!(catalog.errors, errors);
        assert_configmap_lists(&script, 1);
        let list = script
            .requests_for("/api/v1/configmaps?")
            .into_iter()
            .find(|request| request.path.contains("grafana_dashboard"))
            .expect("the provisioning list was requested");
        assert_eq!(
            list.path,
            "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200"
        );
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
        assert_proxy_get(&script, "api/dashboards/uid/locked");
        assert_proxy_get(&script, "api/dashboards/uid/broken");
    }
}

#[test]
fn failed_named_dashboard_keeps_incomplete_provisioning_details() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMapList","metadata":{"continue":"next"},
            "items":[{
                "metadata":{"name":"cluster-dash","namespace":"monitoring"},
                "data":{"broken.json":"{"}
            }]
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
        r#"[{"uid":"locked","type":"dash-db"}]"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/locked",
        403,
        r#"{"message":"dashboard is forbidden"}"#,
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("the denied search hit must remain alongside provisioning errors");
    };
    assert!(catalog.truncated);
    assert_eq!(catalog.extra_hits.len(), 1);
    assert_eq!(catalog.extra_hits[0].search.uid, "locked");
    assert_eq!(catalog.extra_hits[0].reason, CatalogHitReason::Denied);
    let why = catalog.errors.join("; ");
    assert!(why.starts_with(
        "Provisioned dashboard selection is incomplete; provisioned dashboards: ConfigMap monitoring/cluster-dash key \"broken.json\": dashboard JSON did not parse:"
    ));
    assert!(why.contains("provisioned dashboards: provisioning store unavailable"));
    assert!(why.ends_with("Grafana dashboard locked: access denied for this account"));
    assert_configmap_lists(&script, 2);
    let paths: Vec<_> = script
        .requests_for("/api/v1/configmaps?")
        .into_iter()
        .filter(|request| request.path.contains("grafana_dashboard"))
        .map(|request| request.path)
        .collect();
    let list = "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200";
    assert_eq!(paths, [list.to_string(), format!("{list}&continue=next")]);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert_proxy_get(&script, "api/dashboards/uid/locked");
}

#[test]
fn failed_catalog_sources_keep_both_causes_when_neither_succeeds() {
    for (configmap_status, search_status, expected) in [
        (
            403,
            500,
            "provisioned dashboards: configmaps: access denied for this account; Grafana search: search database unavailable",
        ),
        (
            500,
            403,
            "provisioned dashboards: provisioning store unavailable; Grafana search: access denied for this account",
        ),
        (
            500,
            500,
            "provisioned dashboards: provisioning store unavailable; Grafana search: search database unavailable",
        ),
    ] {
        let script = Script::default();
        let runtime = runtime();
        let (sync, _live) = catalog_connection(&runtime, &script);
        script.route(
            "GET",
            "/api/v1/configmaps?",
            configmap_status,
            r#"{"message":"provisioning store unavailable"}"#,
        );
        catalog_source(&script);
        script.route(
            "GET",
            "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
            search_status,
            r#"{"message":"search database unavailable"}"#,
        );
        let Fetched::Failed { what, why } = catalog_reply(&sync) else {
            panic!("both unavailable catalog sources must remain a failure");
        };
        assert_eq!(what, "grafana");
        assert_eq!(why, expected);
        assert_configmap_lists(&script, 1);
        let list = script
            .requests_for("/api/v1/configmaps?")
            .into_iter()
            .find(|request| request.path.contains("grafana_dashboard"))
            .expect("the provisioning list was requested");
        assert_eq!(
            list.path,
            "/api/v1/configmaps?&labelSelector=grafana_dashboard+in+%281%2Ctrue%29&limit=200"
        );
        assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
        assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
    }
}

#[test]
fn a_search_denial_keeps_the_independent_provisioned_parse_failure() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_connection(&runtime, &script);
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMapList","items":[{
                "metadata":{"name":"cluster-dash","namespace":"monitoring"},
                "data":{"broken.json":"{"}
            }]
        })
        .to_string(),
    );
    catalog_source(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        403,
        r#"{"message":"search is forbidden"}"#,
    );
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("search denial must not erase a named provisioning parse failure");
    };
    assert!(catalog.served);
    assert!(catalog.dashboards.is_empty());
    assert_eq!(catalog.errors.len(), 2);
    assert!(catalog.errors[0].starts_with(
        "provisioned dashboards: ConfigMap monitoring/cluster-dash key \"broken.json\": dashboard JSON did not parse:"
    ));
    assert_eq!(
        catalog.errors[1],
        "Grafana search: access denied for this account"
    );
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    assert!(script.requests_for("/proxy/api/dashboards/").is_empty());
}

#[test]
fn browser_only_hits_survive_denied_named_dashboard_reads() {
    let script = Script::default();
    let runtime = runtime();
    let (sync, _live) = catalog_reader(&runtime, &script, false);
    let hits: Vec<_> = (0..17)
        .map(|index| {
            serde_json::json!({
                "uid":format!("item-{index}"),"title":format!("Dashboard {index}"),
                "type":"dash-db"
            })
        })
        .collect();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search",
        200,
        serde_json::to_string(&hits).expect("search fixture"),
    );
    for index in 0..16 {
        script.route(
            "GET",
            &format!(
                "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/dashboards/uid/item-{index}"
            ),
            403,
            r#"{"message":"dashboard is forbidden"}"#,
        );
    }
    let Fetched::Ok(catalog) = catalog_reply(&sync) else {
        panic!("permitted search hits remain browser links beside denied native reads");
    };
    assert!(catalog.served);
    assert!(catalog.truncated);
    assert!(catalog.dashboards.is_empty());
    assert_eq!(catalog.extra_hits.len(), 17);
    for (index, hit) in catalog.extra_hits[..16].iter().enumerate() {
        assert_eq!(hit.search.uid, format!("item-{index}"));
        assert_eq!(hit.reason, CatalogHitReason::Denied);
    }
    assert_eq!(catalog.extra_hits[16].search.uid, "item-16");
    assert_eq!(catalog.extra_hits[16].reason, CatalogHitReason::FetchLimit);
    assert_eq!(catalog.errors.len(), 16);
    for (index, error) in catalog.errors.iter().enumerate() {
        assert_eq!(
            error,
            &format!("Grafana dashboard item-{index}: access denied for this account")
        );
    }
    assert_configmap_lists(&script, 1);
    assert_proxy_get(&script, "api/search?type=dash-db&limit=100");
    for index in 0..16 {
        assert_proxy_get(&script, &format!("api/dashboards/uid/item-{index}"));
    }
    assert_eq!(script.requests_for("/proxy/api/dashboards/uid/").len(), 16);
}

#[test]
fn a_later_configmap_denial_preserves_parse_errors_without_healthy_rows() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/configmaps?",
        200,
        serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMapList","metadata":{"continue":"next"},
            "items":[{
                "metadata":{"name":"cluster-dash","namespace":"monitoring"},
                "data":{"broken.json":"{"}
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/api/v1/configmaps?",
        403,
        r#"{"message":"provisioning list denied"}"#,
    );
    let Fetched::Ok(provisioned) =
        runtime().block_on(async { fetch_provisioned_from_configmaps(&script.client()).await })
    else {
        panic!("the denied continuation must keep the earlier named parse failure");
    };
    assert!(provisioned.dashboards.is_empty());
    assert!(provisioned.truncated);
    assert_eq!(provisioned.errors.len(), 2);
    assert!(provisioned.errors[0].starts_with(
        "ConfigMap monitoring/cluster-dash key \"broken.json\": dashboard JSON did not parse:"
    ));
    assert_eq!(
        provisioned.errors[1],
        "configmaps: access denied for this account"
    );
    assert_configmap_lists(&script, 2);
    assert!(
        script.requests_for("/api/v1/configmaps?")[1]
            .path
            .contains("continue=next")
    );
}
