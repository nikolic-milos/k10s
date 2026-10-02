//! The schema catalog over OpenAPI v3 and CRDs, including what a server that
//! does not serve v3 is allowed to be: a labelled failure, and a 403 a denial.

use crate::*;

const APPS_V1_SCHEMA_DOC: &str = r#"{"openapi":"3.0.0","components":{"schemas":{
    "io.k8s.api.apps.v1.Deployment":{"type":"object",
      "x-kubernetes-group-version-kind":[{"group":"apps","version":"v1","kind":"Deployment"}]}}}}"#;
#[test]
fn the_schema_catalog_maps_openapi_v3_and_documents_fetch_by_their_urls() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    // The more specific per-GV routes register first: `matches` is a prefix
    // test, so a bare /openapi/v3 route would swallow the document paths.
    script.route("GET", "/openapi/v3/apis/apps/v1", 200, APPS_V1_SCHEMA_DOC);
    script.route("GET", "/openapi/v3", 200, OPENAPI_INDEX_JSON);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_schema_catalog(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(sources) = wait(&rx) else {
        panic!("the catalog must resolve");
    };
    let names: Vec<&str> = sources.iter().map(|s| s.group_version.as_str()).collect();
    assert_eq!(
        names,
        ["apps/v1", "v1"],
        "group-versions are sorted and non-API paths dropped"
    );

    let url = sources[0].url.clone();
    assert_eq!(url, "/openapi/v3/apis/apps/v1?hash=bbb");
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_schema_document(url, move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(document) = wait(&rx) else {
        panic!("the document must resolve");
    };
    assert!(document.contains("io.k8s.api.apps.v1.Deployment"));
    let fetches = script.requests_for("/openapi/v3/apis/apps/v1");
    assert_eq!(fetches.len(), 1, "one document fetch: {fetches:?}");
    assert!(
        fetches[0].path.contains("hash=bbb"),
        "the hash-stamped URL rides the request: {}",
        fetches[0].path
    );

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_schema_document("/etc/passwd".to_string(), move |outcome| {
            let _ = tx.send(outcome);
        });
    let Fetched::Failed { why, .. } = wait(&rx) else {
        panic!("a URL outside /openapi/v3 must be refused");
    };
    assert!(why.contains("refused"), "{why}");
    assert!(
        script.requests_for("/etc/passwd").is_empty(),
        "the refused URL never reaches the wire"
    );

    drop(runtime);
}
#[test]
fn a_server_without_openapi_v3_is_a_labelled_failure_and_a_403_a_denial() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_schema_catalog(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Failed { why, .. } = wait(&rx) else {
        panic!("an absent endpoint is a labelled failure");
    };
    assert!(why.contains("does not serve /openapi/v3"), "{why}");

    let denied = Script::default();
    script_discovery(&denied);
    script_rules_review(&denied);
    script_access_reviews(&denied, true, 32);
    script_lists(&denied);
    denied.route(
        "GET",
        "/openapi/v3",
        403,
        r#"{"kind":"Status","apiVersion":"v1","code":403,"reason":"Forbidden","message":"denied"}"#,
    );
    let (sync, _live) = sync_on(&runtime, &denied);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_schema_catalog(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Denied { what } = wait(&rx) else {
        panic!("a 403 is a denial, never an error string");
    };
    assert_eq!(what, "schema catalog");

    drop(runtime);
}
#[test]
fn crd_schemas_fetch_and_an_absent_crd_api_degrades_to_an_empty_list() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/apis/apiextensions.k8s.io/v1/customresourcedefinitions",
        200,
        r#"{"kind":"CustomResourceDefinitionList","apiVersion":"apiextensions.k8s.io/v1",
            "items":[{"metadata":{"name":"widgets.example.com"},
              "spec":{"group":"example.com","names":{"kind":"Widget","plural":"widgets"},
                "versions":[{"name":"v1","served":true,
                  "schema":{"openAPIV3Schema":{"type":"object"}}}]}}]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_crd_schemas(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(text) = wait(&rx) else {
        panic!("the CRD list must resolve");
    };
    assert!(text.contains("widgets.example.com"));

    let bare = Script::default();
    script_discovery(&bare);
    script_rules_review(&bare);
    script_access_reviews(&bare, true, 32);
    script_lists(&bare);
    let (sync, _live) = sync_on(&runtime, &bare);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_crd_schemas(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(text) = wait(&rx) else {
        panic!("an absent CRD API degrades to empty, not broken");
    };
    assert_eq!(text, r#"{"items":[]}"#);

    drop(runtime);
}
