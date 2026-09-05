//! The node capacity table: allocatable against requests and usage, pods
//! counted under an exhausted disruption budget, and the three degradations --
//! unreadable budgets hide the column rather than undercounting it, a
//! cluster with no metrics server hides usage rather than breaking, and a
//! node whose pods do not fit one page says `?` rather than summing a page.

use crate::*;

const NODE_LIST_JSON: &str = r#"{"kind":"NodeList","apiVersion":"v1","metadata":{},"items":[
    {"metadata":{"name":"n1","uid":"uid-n1",
                 "labels":{"node-role.kubernetes.io/control-plane":"","kubernetes.io/hostname":"n1"}},
     "spec":{"taints":[{"key":"dedicated","value":"infra","effect":"NoSchedule"}]},
     "status":{"allocatable":{"cpu":"4","memory":"16Gi","pods":"110"},
               "conditions":[{"type":"Ready","status":"True"},
                             {"type":"MemoryPressure","status":"False"}],
               "nodeInfo":{"kubeletVersion":"v1.32.3"}}}
]}"#;
#[test]
fn the_node_table_measures_allocatable_requests_and_usage() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/nodes?", 200, NODE_LIST_JSON);
    script.route(
        "GET",
        "/api/v1/pods?fieldSelector=spec.nodeName%3Dn1",
        200,
        NODE_PODS_JSON,
    );
    script.route(
        "GET",
        "/apis/metrics.k8s.io/v1beta1/nodes?",
        200,
        r#"{"kind":"NodeMetricsList","apiVersion":"metrics.k8s.io/v1beta1","items":[
            {"metadata":{"name":"n1"},"usage":{"cpu":"250m","memory":"8Gi"}}]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_node_table(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the node table must resolve");
    };
    assert_eq!(
        page.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Name",
            "Status",
            "Roles",
            "Version",
            "OS",
            "Address",
            "Pods",
            "CPU req",
            "Memory req",
            "CPU use",
            "Memory use",
            "Taints",
        ]
    );
    assert_eq!(page.rows.len(), 1);
    let row = &page.rows[0];
    assert_eq!(row.name, "n1");
    assert_eq!(row.uid, "uid-n1");
    assert_eq!(
        row.cells,
        [
            "n1",
            "Ready",
            "control-plane",
            "v1.32.3",
            "",
            "",
            "2/110 (2%)",
            "1600m/4 (40%)",
            "64Mi/16.0Gi (0%)",
            "250m/4 (6%)",
            "8.0Gi/16.0Gi (50%)",
            "1",
        ],
        "the sidecar accumulates and the init floor is honoured"
    );
    assert!(!page.truncated);

    let pod_scan = &script.requests_for("fieldSelector=spec.nodeName")[0];
    assert!(
        pod_scan.path.contains("status.phase%21%3DSucceeded"),
        "terminated pods do not hold requests: {}",
        pod_scan.path
    );

    drop(runtime);
}
const LABELLED_NODE_PODS_JSON: &str = r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[
    {"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod","labels":{"app":"api"}},
     "spec":{"nodeName":"n1","containers":[{"name":"app"}]},
     "status":{"phase":"Running"}},
    {"metadata":{"name":"web-1","uid":"uid-pod-2","namespace":"prod","labels":{"app":"web"}},
     "spec":{"nodeName":"n1","containers":[{"name":"app"}]},
     "status":{"phase":"Running"}},
    {"metadata":{"name":"api-other","uid":"uid-pod-3","namespace":"team-a","labels":{"app":"api"}},
     "spec":{"nodeName":"n1","containers":[{"name":"app"}]},
     "status":{"phase":"Running"}}
]}"#;
const PDB_LIST_JSON: &str = r#"{"kind":"PodDisruptionBudgetList","apiVersion":"policy/v1","metadata":{},"items":[
    {"metadata":{"name":"api-pdb","namespace":"prod","uid":"uid-pdb-1"},
     "spec":{"minAvailable":1,"selector":{"matchLabels":{"app":"api"}}},
     "status":{"disruptionsAllowed":0,"currentHealthy":1,"desiredHealthy":1,"expectedPods":1}},
    {"metadata":{"name":"web-pdb","namespace":"prod","uid":"uid-pdb-2"},
     "spec":{"minAvailable":1,"selector":{"matchLabels":{"app":"web"}}},
     "status":{"disruptionsAllowed":1,"currentHealthy":2,"desiredHealthy":1,"expectedPods":2}}
]}"#;
#[test]
fn the_node_table_counts_pods_under_an_exhausted_disruption_budget() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/nodes?", 200, NODE_LIST_JSON);
    script.route(
        "GET",
        "/api/v1/pods?fieldSelector=spec.nodeName%3Dn1",
        200,
        LABELLED_NODE_PODS_JSON,
    );
    script.route(
        "GET",
        "/apis/policy/v1/poddisruptionbudgets?",
        200,
        PDB_LIST_JSON,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_node_table(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the node table must resolve");
    };
    let pdb_at = page
        .columns
        .iter()
        .position(|c| c.name == "PDB blocked")
        .expect("the column exists when the budgets are readable");
    assert_eq!(
        page.rows[0].cells[pdb_at], "1",
        "only prod/api-1 sits under the exhausted budget: web-pdb has headroom, and \
         team-a/api-other matches the labels but not the namespace: {:?}",
        page.rows[0].cells
    );

    let pdb_request = &script.requests_for("/apis/policy/v1/poddisruptionbudgets")[0];
    assert!(
        pdb_request.path.contains("limit=1000"),
        "the budget list is bounded: {}",
        pdb_request.path
    );

    drop(runtime);
}
#[test]
fn unreadable_disruption_budgets_hide_the_column_rather_than_undercounting() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/nodes?", 200, NODE_LIST_JSON);
    script.route(
        "GET",
        "/api/v1/pods?fieldSelector=spec.nodeName%3Dn1",
        200,
        LABELLED_NODE_PODS_JSON,
    );
    script.route(
        "GET",
        "/apis/policy/v1/poddisruptionbudgets?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"poddisruptionbudgets is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_node_table(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the node table must still resolve");
    };
    assert!(
        page.columns.iter().all(|c| c.name != "PDB blocked"),
        "a denied budget list makes the column invisible, never wrong: {:?}",
        page.columns
    );

    drop(runtime);
}
#[test]
fn a_node_whose_pods_do_not_fit_one_page_reports_unknown_rather_than_a_partial_sum() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/nodes?", 200, NODE_LIST_JSON);
    script.route(
        "GET",
        "/api/v1/pods?fieldSelector=spec.nodeName%3Dn1",
        200,
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{"continue":"there-are-more"},"items":[
            {"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod"},
             "spec":{"nodeName":"n1","containers":[{"name":"app",
                "resources":{"requests":{"cpu":"100m","memory":"64Mi"}}}]},
             "status":{"phase":"Running"}}
        ]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_node_table(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the node table must still resolve");
    };
    let cell = |name: &str| {
        let at = page
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("the {name} column exists: {:?}", page.columns));
        page.rows[0].cells[at].as_str()
    };
    for column in ["Pods", "CPU req", "Memory req"] {
        assert_eq!(
            cell(column),
            "?",
            "one page of a node's pods is not the node's load: {:?}",
            page.rows[0].cells
        );
    }

    drop(runtime);
}
#[test]
fn a_cluster_without_metrics_server_hides_usage_rather_than_breaking() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/nodes?", 200, NODE_LIST_JSON);
    script.route(
        "GET",
        "/api/v1/pods?fieldSelector=spec.nodeName%3Dn1",
        200,
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_node_table(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the node table must resolve");
    };
    assert!(
        page.columns.iter().all(|c| !c.name.contains("use")),
        "absent metrics-server means invisible, not broken: {:?}",
        page.columns
    );
    let pods_at = page
        .columns
        .iter()
        .position(|column| column.name == "Pods")
        .expect("the Pods column stays after OS and Address");
    assert_eq!(page.rows[0].cells[pods_at], "0/110 (0%)");

    drop(runtime);
}
