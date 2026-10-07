use super::*;

#[test]
fn datasource_types_outrank_opaque_uids_and_target_identity_outranks_the_panel() {
    use serde_json::json;
    for (target_source, panel_source, expected, identity) in [
        (
            json!({"uid":"opaque", "type":"prometheus"}),
            json!({"uid":"logs", "type":"loki"}),
            QueryDialect::PromQL,
            "opaque",
        ),
        (
            json!({"uid":"prometheus-named", "type":"loki"}),
            json!(null),
            QueryDialect::LogQL,
            "prometheus-named",
        ),
        (
            json!(null),
            json!({"uid":"$datasource", "type":"prometheus"}),
            QueryDialect::PromQL,
            "$datasource",
        ),
        (
            json!(""),
            json!({"uid":"logs", "type":"loki"}),
            QueryDialect::LogQL,
            "logs",
        ),
        (
            json!({"uid":"opaque"}),
            json!({"uid":"metrics", "type":"prometheus"}),
            QueryDialect::Unknown,
            "opaque",
        ),
        (
            json!({"uid":"prometheus-named", "type":"influxdb"}),
            json!(null),
            QueryDialect::Unknown,
            "prometheus-named",
        ),
    ] {
        let target = json!({"expr":"up", "datasource":target_source});
        let query = read_query(&target, Some(&panel_source)).expect("a query");
        assert_eq!(query.dialect, expected, "{target}");
        assert_eq!(query.datasource.as_deref(), Some(identity));
    }
}

fn dashboard_json() -> &'static str {
    r#"{
      "uid": "k8s-resources",
      "title": "Kubernetes / Compute Resources / Cluster",
      "panels": [
        {
          "id": 1,
          "title": "CPU Utilisation",
          "type": "timeseries",
          "datasource": {"type": "prometheus", "uid": "prom"},
          "targets": [
            {"refId": "A", "expr": "1 - avg(rate(node_cpu_seconds_total{mode=\"idle\"}[5m]))"}
          ]
        },
        {
          "id": 2,
          "title": "A row",
          "type": "row",
          "panels": [
            {
              "id": 3,
              "title": "Logs",
              "type": "logs",
              "datasource": {"type": "loki", "uid": "loki"},
              "targets": [{"refId": "A", "expr": "{app=\"api\"}"}]
            }
          ]
        },
        {
          "id": 4,
          "title": "Worldmap",
          "type": "grafana-worldmap-panel",
          "targets": [{"refId": "A", "expr": "up"}]
        },
        {
          "id": 5,
          "title": "Transformed CPU",
          "type": "timeseries",
          "transformations": [{"id": "reduce"}],
          "targets": [{"refId": "A", "expr": "up"}]
        }
      ]
    }"#
}

#[test]
fn a_dashboard_keeps_panel_titles_and_walks_rows() {
    let dash = parse_dashboard(dashboard_json().as_bytes()).expect("json");
    assert_eq!(dash.uid, "k8s-resources");
    assert_eq!(dash.title, "Kubernetes / Compute Resources / Cluster");
    assert_eq!(dash.panels.len(), 4, "the row itself is not a panel");
    assert_eq!(dash.panels[0].title, "CPU Utilisation");
    assert_eq!(dash.panels[0].kind, PanelKind::Timeseries);
    assert_eq!(dash.panels[0].queries[0].dialect, QueryDialect::PromQL);
    assert_eq!(dash.panels[1].title, "Logs");
    assert_eq!(dash.panels[1].kind, PanelKind::Logs);
    assert_eq!(dash.panels[1].queries[0].dialect, QueryDialect::LogQL);
    assert_eq!(dash.panels[1].queries[0].expr, "{app=\"api\"}");
}

#[test]
fn a_plugin_panel_is_unsupported_and_still_keeps_its_query() {
    let dash = parse_dashboard(dashboard_json().as_bytes()).unwrap();
    let worldmap = dash
        .panels
        .iter()
        .find(|p| p.title == "Worldmap")
        .expect("worldmap");
    assert_eq!(worldmap.kind, PanelKind::Unsupported);
    assert_eq!(worldmap.queries[0].expr, "up");
}

#[test]
fn transformations_are_flagged_not_executed() {
    let dash = parse_dashboard(dashboard_json().as_bytes()).unwrap();
    let panel = dash
        .panels
        .iter()
        .find(|p| p.title == "Transformed CPU")
        .unwrap();
    assert!(panel.transformed);
    assert_eq!(panel.queries[0].expr, "up");
}

#[test]
fn the_api_envelope_unwraps_the_same_way_as_a_provisioned_file() {
    let envelope = format!(
        r#"{{"meta":{{"slug":"x"}},"dashboard":{}}}"#,
        dashboard_json()
    );
    let dash = parse_dashboard(envelope.as_bytes()).unwrap();
    assert_eq!(dash.uid, "k8s-resources");
    assert_eq!(dash.panels.len(), 4);
}

#[test]
fn an_oversize_body_is_refused_not_truncated() {
    let huge = vec![b'x'; MAX_DASHBOARD_BYTES + 1];
    match parse_dashboard(&huge) {
        Err(DashboardError::TooLarge { bytes }) => assert_eq!(bytes, MAX_DASHBOARD_BYTES + 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn folder_allowlist_drops_everything_outside_it_and_keeps_all_when_empty() {
    let hits = vec![
        SearchHit {
            uid: "a".into(),
            title: "API".into(),
            folder_title: "Team A".into(),
            kind: "dash-db".into(),
        },
        SearchHit {
            uid: "b".into(),
            title: "Other".into(),
            folder_title: "Team B".into(),
            kind: "dash-db".into(),
        },
        SearchHit {
            uid: "f".into(),
            title: "Team A".into(),
            folder_title: String::new(),
            kind: "dash-folder".into(),
        },
    ];
    assert_eq!(filter_folders(hits.clone(), &[]).len(), 2);
    let kept = filter_folders(hits, &["Team A".into()]);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].uid, "a");
}

#[test]
fn search_json_keeps_the_folder_grafana_named() {
    let hits =
        parse_search(br#"[{"uid":"a","title":"API","folderTitle":"Team A","type":"dash-db"}]"#)
            .expect("json");
    assert_eq!(hits[0].folder_title, "Team A");
}

#[test]
fn a_configmap_data_value_is_parsed_and_junk_keys_are_skipped() {
    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::ConfigMap;

    let mut data = BTreeMap::new();
    data.insert("cluster.json".into(), dashboard_json().into());
    data.insert("notes".into(), "not a dashboard".into());
    let cm = ConfigMap {
        data: Some(data),
        ..ConfigMap::default()
    };
    let mut provisioned = Provisioned::default();
    let mut scanned_keys = 0;
    collect_from_configmap(&cm, &mut provisioned, &mut scanned_keys);
    assert_eq!(provisioned.dashboards.len(), 1);
    assert_eq!(provisioned.dashboards[0].uid, "k8s-resources");
    assert_eq!(scanned_keys, 2);
    assert!(provisioned.errors.is_empty());
    assert!(!provisioned.truncated);
}

#[test]
fn binary_data_is_not_read_as_a_dashboard() {
    use std::collections::BTreeMap;

    use k8s_openapi::ByteString;
    use k8s_openapi::api::core::v1::ConfigMap;

    let cm = ConfigMap {
        binary_data: Some(BTreeMap::from([(
            "dash.json".into(),
            ByteString(br#"{"title":"hidden","panels":[]}"#.to_vec()),
        )])),
        ..ConfigMap::default()
    };
    let mut provisioned = Provisioned::default();
    let mut scanned_keys = 0;
    collect_from_configmap(&cm, &mut provisioned, &mut scanned_keys);
    assert!(provisioned.dashboards.is_empty());
    assert_eq!(scanned_keys, 0);
    assert!(provisioned.errors.is_empty());
}

fn dash_with(panels: &str) -> Dashboard {
    let json = format!(r#"{{"uid":"d","title":"d","panels":[{panels}]}}"#);
    parse_dashboard(json.as_bytes()).expect("dashboard")
}

fn timeseries_promql(title: &str, expr: &str) -> String {
    format!(
        r#"{{"title":"{title}","type":"timeseries","datasource":{{"type":"prometheus","uid":"prom"}},"targets":[{{"refId":"A","expr":{expr}}}]}}"#
    )
}

#[test]
fn a_node_cpu_panel_is_not_joinable_to_a_pod_cell() {
    let dash = parse_dashboard(dashboard_json().as_bytes()).unwrap();
    assert_eq!(
        name_overlay_promql(&[dash]),
        None,
        "Grafana's cluster CPU is not a pod-cell join"
    );
}

#[test]
fn sum_by_namespace_pod_is_copied_verbatim_and_preferred() {
    let mention = timeseries_promql(
        "by pod",
        r#""container_cpu_usage_seconds_total{namespace=\"prod\",pod=\"api\"}""#,
    );
    let preferred = timeseries_promql(
        "sum",
        r#""sum by (namespace, pod) (rate(container_cpu_usage_seconds_total[5m]))""#,
    );
    let dashboards = [dash_with(&format!("{mention},{preferred}"))];
    let named = name_overlay_promql(&dashboards).expect("joinable");
    assert_eq!(
        named, "sum by (namespace, pod) (rate(container_cpu_usage_seconds_total[5m]))",
        "the expr Grafana wrote is the one we run; sum-by wins over a matcher"
    );
}

#[test]
fn an_expr_that_names_namespace_and_pod_still_qualifies() {
    let dashboards = [dash_with(&timeseries_promql(
        "cpu",
        r#""rate(container_cpu_usage_seconds_total{namespace=~\".+\",pod=~\".+\"}[5m])""#,
    ))];
    let named = name_overlay_promql(&dashboards).expect("joinable");
    assert_eq!(
        named,
        r#"rate(container_cpu_usage_seconds_total{namespace=~".+",pod=~".+"}[5m])"#
    );
}

#[test]
fn logql_and_grafana_variables_are_not_promql_we_can_run() {
    let logs = r#"{"title":"logs","type":"logs","datasource":{"type":"loki","uid":"loki"},"targets":[{"refId":"A","expr":"{namespace=\"prod\",pod=\"api\"}"}]}"#;
    let vars = timeseries_promql(
        "cpu",
        r#""sum by (namespace, pod) (rate(container_cpu_usage_seconds_total{namespace=\"$namespace\"}[$__rate_interval]))""#,
    );
    let dash = dash_with(&format!("{logs},{vars}"));
    assert_eq!(
        name_overlay_promql(&[dash]),
        None,
        "LogQL and $variables stay Grafana's; CPU_EXPR remains the fallback"
    );
}

#[test]
fn unknown_dialect_and_up_are_not_a_pod_cell() {
    let cloudwatch = r#"{"title":"cw","type":"timeseries","datasource":{"type":"cloudwatch","uid":"cw"},"targets":[{"refId":"A","expr":"sum by (namespace, pod) (cpu)"}]}"#;
    let up = timeseries_promql("up", r#""up""#);
    let dash = dash_with(&format!("{cloudwatch},{up}"));
    assert_eq!(name_overlay_promql(&[dash]), None);
}

#[test]
fn sum_by_pod_namespace_order_and_postfix_by_both_count() {
    let postfix = timeseries_promql(
        "postfix",
        r#""sum (rate(container_cpu_usage_seconds_total[5m])) by (pod, namespace)""#,
    );
    let dashboards = [dash_with(&postfix)];
    assert_eq!(
        name_overlay_promql(&dashboards),
        Some("sum (rate(container_cpu_usage_seconds_total[5m])) by (pod, namespace)")
    );
}

#[test]
fn empty_dashboards_name_nothing() {
    assert_eq!(name_overlay_promql(&[]), None);
}

#[test]
fn provisioned_caps_are_stated_not_silent() {
    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::ConfigMap;

    let mut data = BTreeMap::new();
    for i in 0..=MAX_PROVISIONED {
        data.insert(
            format!("{i}.json"),
            format!(r#"{{"uid":"{i}","title":"{i}","panels":[]}}"#),
        );
    }
    let cm = ConfigMap {
        data: Some(data),
        ..ConfigMap::default()
    };
    let mut provisioned = Provisioned::default();
    let mut scanned_keys = 0;
    collect_from_configmap(&cm, &mut provisioned, &mut scanned_keys);
    assert_eq!(provisioned.dashboards.len(), MAX_PROVISIONED);
    assert_eq!(scanned_keys, MAX_PROVISIONED);
    assert!(provisioned.truncated);
}

#[test]
fn malformed_panel_arrays_cannot_impersonate_empty_dashboards() {
    for panels in ["null", "{}", "true", "1", "\"panels\""] {
        for document in [
            format!(r#"{{"uid":"d","title":"d","panels":{panels}}}"#),
            format!(r#"{{"uid":"d","title":"d","panels":[{{"type":"row","panels":{panels}}}]}}"#),
        ] {
            assert_eq!(
                parse_dashboard(document.as_bytes()),
                Err(DashboardError::InvalidPanels),
                "{document}",
            );
        }
    }
    for document in [
        br#"{"uid":"d","title":"d"}"#.as_slice(),
        br#"{"uid":"d","title":"d","panels":[]}"#.as_slice(),
        br#"{"uid":"d","title":"d","panels":[{"type":"row"}]}"#.as_slice(),
    ] {
        assert!(parse_dashboard(document).unwrap().panels.is_empty());
    }
}

#[test]
fn broken_provisioned_files_are_named_without_losing_healthy_files() {
    use k8s_openapi::api::core::v1::ConfigMap;
    use std::collections::BTreeMap;

    let cm = ConfigMap {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            namespace: Some("monitoring".into()),
            name: Some("cluster-dash".into()),
            ..Default::default()
        },
        data: Some(BTreeMap::from([
            ("broken.json".into(), "{".into()),
            ("good.json".into(), dashboard_json().into()),
            ("notes".into(), "not a dashboard".into()),
        ])),
        ..Default::default()
    };
    let mut provisioned = Provisioned::default();
    let mut scanned_keys = 0;
    collect_from_configmap(&cm, &mut provisioned, &mut scanned_keys);
    assert_eq!(provisioned.dashboards[0].uid, "k8s-resources");
    assert_eq!(provisioned.errors.len(), 1);
    assert!(provisioned.errors[0].starts_with(
        "ConfigMap monitoring/cluster-dash key \"broken.json\": dashboard JSON did not parse:",
    ));
    assert_eq!(scanned_keys, 3);
    assert!(!provisioned.truncated);
}

#[test]
fn failed_and_unrelated_provisioning_keys_consume_the_shared_scan_limit() {
    use k8s_openapi::api::core::v1::ConfigMap;
    use std::collections::BTreeMap;

    for suffix in ["json", "notes"] {
        let cm = ConfigMap {
            data: Some(
                (0..MAX_PROVISIONED)
                    .map(|i| (format!("{i:04}.{suffix}"), "{".into()))
                    .collect::<BTreeMap<_, _>>(),
            ),
            ..Default::default()
        };
        let mut provisioned = Provisioned::default();
        let mut scanned_keys = 0;
        collect_from_configmap(&cm, &mut provisioned, &mut scanned_keys);
        assert_eq!(scanned_keys, MAX_PROVISIONED);
        assert_eq!(
            provisioned.errors.len(),
            if suffix == "json" { MAX_PROVISIONED } else { 0 }
        );
        assert!(!provisioned.truncated);
        collect_from_configmap(&cm, &mut provisioned, &mut scanned_keys);
        assert!(provisioned.truncated);
        assert!(provisioned.dashboards.is_empty());
    }
}

#[test]
fn multiple_visible_targets_cannot_impersonate_one_complete_panel() {
    let panel = serde_json::json!({
        "type": "timeseries",
        "targets": [{"refId":"A", "expr":"up"}, {"refId":"B", "expr":"down"}]
    });
    let parsed = read_panel(&panel, 1);
    assert_eq!(parsed.queries.len(), 2);
    assert!(
        parsed
            .unsupported
            .as_deref()
            .is_some_and(|why| why.contains("multi-target"))
    );
    let mut single = panel;
    single["targets"][1]["hide"] = serde_json::json!(true);
    let parsed = read_panel(&single, 1);
    assert_eq!(parsed.queries.len(), 1);
    assert!(parsed.unsupported.is_none());
    single["targets"][0]["format"] = serde_json::json!("table");
    assert!(read_panel(&single, 1).queries[0].unsupported.is_some());
}
