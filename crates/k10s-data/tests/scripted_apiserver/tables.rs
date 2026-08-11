//! Server-side tables: any discovered kind lists with bounded pages, and the
//! next page is fetched with the continue token only when it is asked for.

use crate::*;

const DEPLOYMENT_TABLE_JSON: &str = r#"{"kind":"Table","apiVersion":"meta.k8s.io/v1",
    "metadata":{"resourceVersion":"1000","continue":"page-2"},
    "columnDefinitions":[
        {"name":"Name","type":"string","format":"name","priority":0},
        {"name":"Ready","type":"string","priority":0},
        {"name":"Replicas","type":"integer","priority":0},
        {"name":"Containers","type":"string","priority":1}],
    "rows":[{"cells":["api","1/1",1,"app"],
             "object":{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
                       "metadata":{"name":"api","namespace":"prod","uid":"uid-dep"}}}]}"#;
#[test]
fn any_discovered_kind_lists_as_a_server_side_table_with_bounded_pages() {
    use k10s_core::KindId;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route_accepting(
        "GET",
        "/apis/apps/v1/deployments?",
        "as=Table",
        200,
        DEPLOYMENT_TABLE_JSON,
    );
    script.route_accepting(
        "GET",
        "/api/v1/pods?",
        "as=Table",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"pods is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let kinds = sync.reader.kinds();
    assert_eq!(kinds.len(), 10, "every discovered listable kind is offered");
    let displays: Vec<&str> = kinds.iter().map(|k| k.display.as_str()).collect();
    let mut sorted = displays.clone();
    sorted.sort_unstable();
    assert_eq!(displays, sorted, "kinds arrive sorted for a picker");
    let deployments = kinds
        .iter()
        .find(|k| k.display == "deployments.apps")
        .expect("the group is part of the name");
    assert_eq!(deployments.kind, "Deployment");
    assert!(deployments.namespaced);
    assert_eq!(deployments.verdict, Some(Capability::Watchable));
    assert!(kinds.iter().any(|k| k.display == "pods"));

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_table(KindId::DEPLOYMENT, None, move |outcome| {
            let _ = tx.send(outcome);
        });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the table must resolve");
    };
    assert_eq!(
        page.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["Namespace", "Name", "Ready", "Replicas", "Containers"],
        "a cluster-wide list of a namespaced kind gains the namespace column"
    );
    assert!(page.columns[4].wide, "priority > 0 is a wide column");
    assert_eq!(page.rows[0].cells, ["prod", "api", "1/1", "1", "app"]);
    assert_eq!(page.rows[0].name, "api");
    assert_eq!(page.rows[0].namespace.as_deref(), Some("prod"));
    assert_eq!(page.rows[0].uid, "uid-dep");
    assert!(
        page.truncated,
        "a continue token surfaces, it is not chased"
    );

    let table_requests: Vec<Seen> = script
        .requests_for("/apis/apps/v1/deployments")
        .into_iter()
        .filter(|r| r.accept.contains("as=Table"))
        .collect();
    assert_eq!(table_requests.len(), 1, "{table_requests:?}");
    assert!(
        table_requests[0]
            .accept
            .contains("as=Table;v=v1;g=meta.k8s.io"),
        "the server renders the columns: {}",
        table_requests[0].accept
    );
    assert!(
        table_requests[0].path.contains("limit=500"),
        "a table page is bounded: {}",
        table_requests[0].path
    );

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_table(KindId::POD, None, move |outcome| {
        let _ = tx.send(outcome);
    });
    assert_eq!(
        wait(&rx),
        Fetched::Denied { what: "table" },
        "a 403 is a labelled state, not an error string"
    );

    drop(runtime);
}
#[test]
fn the_next_table_page_is_fetched_with_the_continue_token_on_demand() {
    use k10s_core::KindId;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route_accepting(
        "GET",
        "/apis/apps/v1/deployments?",
        "as=Table",
        200,
        DEPLOYMENT_TABLE_JSON,
    );
    // The continuation page: real servers omit the column definitions they
    // already sent, and the token is gone when the list is done.
    script.route_accepting(
        "GET",
        "/apis/apps/v1/deployments?",
        "as=Table",
        200,
        r#"{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"1000"},
            "columnDefinitions":[],
            "rows":[{"cells":["web","2/2",2,"app"],
                     "object":{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
                               "metadata":{"name":"web","namespace":"prod","uid":"uid-web"}}}]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_table(KindId::DEPLOYMENT, None, move |outcome| {
            let _ = tx.send(outcome);
        });
    let Fetched::Ok(first) = wait(&rx) else {
        panic!("the first page must resolve");
    };
    assert_eq!(first.continue_token.as_deref(), Some("page-2"));

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_table(KindId::DEPLOYMENT, first.continue_token, move |outcome| {
            let _ = tx.send(outcome);
        });
    let Fetched::Ok(second) = wait(&rx) else {
        panic!("the second page must resolve");
    };
    assert_eq!(second.rows.len(), 1);
    assert_eq!(second.rows[0].name, "web");
    assert_eq!(second.continue_token, None, "the list is complete");
    assert!(!second.truncated);

    let table_requests: Vec<Seen> = script
        .requests_for("/apis/apps/v1/deployments")
        .into_iter()
        .filter(|r| r.accept.contains("as=Table"))
        .collect();
    assert_eq!(table_requests.len(), 2);
    assert!(
        !table_requests[0].path.contains("continue="),
        "{}",
        table_requests[0].path
    );
    assert!(
        table_requests[1].path.contains("continue=page-2"),
        "the token the first page carried names the second: {}",
        table_requests[1].path
    );
    assert!(
        table_requests[1].path.contains("limit=500"),
        "every page stays bounded: {}",
        table_requests[1].path
    );

    drop(runtime);
}
