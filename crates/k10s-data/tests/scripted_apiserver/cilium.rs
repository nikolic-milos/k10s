//! Cilium CRs listed through kube Request. A missing cilium.io group is
//! invisible; a 403 is Denied. Hubble Relay is never dialed.

use crate::*;
use k10s_data::cilium::{self, Completeness, Decision, EndpointRef, KindSet, Protocol, Traffic};
use k10s_data::read::Fetched;

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn cilium_group() -> String {
    r#"{"kind":"APIGroup","name":"cilium.io",
        "versions":[{"groupVersion":"cilium.io/v2","version":"v2"}],
        "preferredVersion":{"groupVersion":"cilium.io/v2","version":"v2"}}"#
        .to_string()
}

fn cnp_item() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumNetworkPolicy",
        "metadata": { "name": "allow-web", "namespace": "prod", "uid": "uid-cnp" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "toPorts": [{
                    "ports": [{ "port": "80", "protocol": "TCP" }],
                    "rules": { "http": [{ "method": "GET", "path": "/public" }] }
                }]
            }]
        }
    })
}

fn empty_list() -> String {
    r#"{"kind":"List","apiVersion":"cilium.io/v2","metadata":{},"items":[]}"#.to_string()
}

fn list_path(plural: &str) -> String {
    format!("/apis/cilium.io/v2/{plural}?")
}

fn isolating_cnp(name: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "CiliumNetworkPolicy",
        "metadata": { "name": name, "namespace": "prod", "uid": format!("uid-{name}") },
        "spec": { "endpointSelector": { "matchLabels": { "app": "api" } }, "ingress": [] }
    })
}

fn app_endpoint(namespace: &str, name: &str, app: &str) -> EndpointRef {
    EndpointRef {
        name: name.to_string(),
        namespace: namespace.to_string(),
        uid: String::new(),
        labels: std::iter::once(("app".to_string(), app.to_string())).collect(),
        identity_id: None,
        ips: Vec::new(),
        ports: Vec::new(),
    }
}

fn tcp_80() -> Traffic {
    Traffic {
        protocol: Protocol::Tcp,
        port: 80,
        l7: None,
    }
}

fn script_empty_kinds(script: &Script) {
    for plural in [
        "ciliumclusterwidenetworkpolicies",
        "ciliumidentities",
        "ciliumendpoints",
        "ciliumnodes",
    ] {
        script.route("GET", &list_path(plural), 200, empty_list());
    }
}

#[test]
fn a_cluster_without_cilium_is_unserved_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.network_policies, KindSet::NotServed));
    assert!(
        cilium::table_page(&inventory).is_none(),
        "not served must not open a table"
    );
    assert!(
        script.requests_for("ciliumnetworkpolicies").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| seen.path == "/apis/cilium.io" || seen.path.starts_with("/apis/cilium.io")),
        "the only probe is the group document: {:?}",
        script.seen()
    );
    assert_eq!(
        script.seen().len(),
        1,
        "group probe only: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_404_group_is_unserved_and_a_403_group_is_denied() {
    let runtime = runtime();

    let missing = Script::default();
    missing.route("GET", "/apis/cilium.io", 404, status(404, "NotFound"));
    let fetched = runtime.block_on(async { cilium::fetch(&missing.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a 404 is unserved, not an error: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(cilium::table_page(&inventory).is_none());
    assert!(missing.requests_for("ciliumnetworkpolicies").is_empty());

    let denied = Script::default();
    denied.route("GET", "/apis/cilium.io", 403, status(403, "Forbidden"));
    let fetched = runtime.block_on(async { cilium::fetch(&denied.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on each kind: {fetched:?}");
    };
    assert!(inventory.served(), "403 is Denied, not served: false");
    assert!(matches!(inventory.network_policies, KindSet::Denied));
    assert!(matches!(inventory.identities, KindSet::Denied));
    let page = cilium::table_page(&inventory).expect("Denied opens a labelled table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("access denied for this account"), "{text}");
    assert!(
        denied.requests_for("ciliumnetworkpolicies").is_empty(),
        "a 403 group must not be chased into a list: {:?}",
        denied.seen()
    );
    drop(runtime);
}

#[test]
fn list_paths_use_the_discovered_group_version_and_plural() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, cilium_group());
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumnetworkpolicies?",
        200,
        serde_json::json!({
            "kind": "CiliumNetworkPolicyList",
            "apiVersion": "cilium.io/v2",
            "metadata": {},
            "items": [cnp_item()]
        })
        .to_string(),
    );
    script_empty_kinds(&script);

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    assert!(inventory.served());
    assert_eq!(inventory.network_policies.items().len(), 1);
    assert_eq!(inventory.network_policies.items()[0].name, "allow-web");
    assert!(
        inventory.network_policies.items()[0]
            .detail
            .contains("declared L7 HTTP")
    );

    let seen = script.seen();
    assert_eq!(seen[0].path, "/apis/cilium.io");
    let lists: Vec<_> = seen
        .iter()
        .filter(|seen| seen.path.contains("/apis/cilium.io/v2/"))
        .collect();
    assert!(
        lists.iter().any(|seen| seen
            .path
            .starts_with("/apis/cilium.io/v2/ciliumnetworkpolicies?")),
        "CNP list uses group, version, plural: {lists:?}"
    );
    for needle in [
        "ciliumclusterwidenetworkpolicies",
        "ciliumidentities",
        "ciliumendpoints",
        "ciliumnodes",
    ] {
        assert!(
            lists.iter().any(|seen| seen.path.contains(needle)),
            "listed {needle}: {lists:?}"
        );
    }
    assert!(
        lists.iter().all(|seen| seen.path.contains("cilium.io/v2/")),
        "the version came from the group document, not a hardcoded walk: {lists:?}"
    );
    drop(runtime);
}

#[test]
fn a_real_shaped_cnp_parses_and_table_page_is_some() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, cilium_group());
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumnetworkpolicies?",
        200,
        serde_json::json!({
            "kind": "CiliumNetworkPolicyList",
            "items": [cnp_item()]
        })
        .to_string(),
    );
    script_empty_kinds(&script);

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("the listing must resolve: {fetched:?}");
    };
    let policies = cilium::parse_policy_document(&cnp_item());
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].name, "allow-web");
    assert_eq!(
        policies[0].ingress.as_ref().unwrap()[0].http[0].method,
        "GET"
    );

    let page = cilium::table_page(&inventory).expect("served inventory opens a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "allow-web");
    assert_eq!(page.rows[0].namespace.as_deref(), Some("prod"));

    let empty = Script::default();
    empty.route("GET", "/apis/cilium.io", 200, cilium_group());
    empty.route(
        "GET",
        "/apis/cilium.io/v2/ciliumnetworkpolicies?",
        200,
        empty_list(),
    );
    script_empty_kinds(&empty);
    let fetched = runtime.block_on(async { cilium::fetch(&empty.client(), None).await });
    let Fetched::Ok(empty_inventory) = fetched else {
        panic!("an empty served group is still Ok: {fetched:?}");
    };
    assert!(empty_inventory.served());
    assert!(empty_inventory.network_policies.items().is_empty());
    let empty_page =
        cilium::table_page(&empty_inventory).expect("served and empty is Some, not None");
    assert!(empty_page.rows.is_empty());
    drop(runtime);
}

#[test]
fn debug_of_a_fetched_inventory_never_contains_a_secret_looking_field() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, cilium_group());
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumnetworkpolicies?",
        200,
        serde_json::json!({ "items": [cnp_item()] }).to_string(),
    );
    script_empty_kinds(&script);
    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("the listing must resolve: {fetched:?}");
    };
    let debug = format!("{inventory:?}").to_ascii_lowercase();
    for needle in [
        "password",
        "token",
        "secret",
        "stringdata",
        "bearer",
        "authorization",
    ] {
        assert!(
            !debug.contains(needle),
            "inventory Debug must not grow a secret-looking field: {needle} in {debug}"
        );
    }
    drop(runtime);
}

#[test]
fn a_403_on_one_kind_is_denied_and_does_not_hide_the_others() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, cilium_group());
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumnetworkpolicies?",
        403,
        status(403, "Forbidden"),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumidentities?",
        200,
        serde_json::json!({
            "items": [{
                "metadata": {
                    "name": "12345",
                    "uid": "uid-cid",
                    "labels": { "k8s:io.kubernetes.pod.namespace": "prod" }
                }
            }]
        })
        .to_string(),
    );
    for plural in [
        "ciliumclusterwidenetworkpolicies",
        "ciliumendpoints",
        "ciliumnodes",
    ] {
        script.route("GET", &list_path(plural), 200, empty_list());
    }

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a denied kind is not a whole-fetch failure: {fetched:?}");
    };
    assert!(matches!(inventory.network_policies, KindSet::Denied));
    assert_eq!(inventory.identities.items().len(), 1);
    assert_eq!(inventory.identities.items()[0].identity_id, Some(12345));
    assert!(inventory.declared.truncated);
    drop(runtime);
}

#[test]
fn a_label_heavy_identity_clips_labels_without_stopping_pagination() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, cilium_group());
    script.route(
        "GET",
        &list_path("ciliumnetworkpolicies"),
        200,
        empty_list(),
    );
    let mut labels = serde_json::Map::new();
    for index in 0..40 {
        labels.insert(
            format!("label-{index:02}"),
            serde_json::Value::String("x".to_string()),
        );
    }
    let page_one = serde_json::json!({
        "kind": "List", "apiVersion": "cilium.io/v2",
        "metadata": { "continue": "page-two" },
        "items": [{
            "metadata": { "name": "10001", "uid": "uid-i1" },
            "security-labels": labels
        }]
    });
    let page_two = serde_json::json!({
        "kind": "List", "apiVersion": "cilium.io/v2",
        "metadata": {},
        "items": [{
            "metadata": { "name": "10002", "uid": "uid-i2" },
            "security-labels": { "k8s:app": "web" }
        }]
    });
    script.route(
        "GET",
        &list_path("ciliumidentities"),
        200,
        page_one.to_string(),
    );
    script.route(
        "GET",
        &list_path("ciliumidentities"),
        200,
        page_two.to_string(),
    );
    for plural in [
        "ciliumclusterwidenetworkpolicies",
        "ciliumendpoints",
        "ciliumnodes",
    ] {
        script.route("GET", &list_path(plural), 200, empty_list());
    }
    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("two clean pages are Ok: {fetched:?}");
    };
    let KindSet::Served {
        items,
        truncated,
        labels_clipped,
        ..
    } = &inventory.identities
    else {
        panic!("identities are served: {:?}", inventory.identities);
    };
    assert_eq!(
        items.len(),
        2,
        "one label-heavy object must not stop the listing: {items:?}"
    );
    assert!(
        !truncated,
        "clipped labels are not the object-count ceiling"
    );
    assert!(labels_clipped, "the clip is reported on its own channel");
    drop(runtime);
}

#[test]
fn a_truncated_policy_listing_answers_indeterminate_not_deny() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, cilium_group());
    // One more CNP than the walk keeps: the compiled set is missing at least
    // one policy that could have allowed the pair.
    let items: Vec<serde_json::Value> = (0..=cilium::MAX_OBJECTS)
        .map(|index| isolating_cnp(&format!("lock-{index}")))
        .collect();
    script.route(
        "GET",
        &list_path("ciliumnetworkpolicies"),
        200,
        serde_json::json!({ "kind": "CiliumNetworkPolicyList", "items": items }).to_string(),
    );
    script_empty_kinds(&script);

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a truncated listing is still Ok: {fetched:?}");
    };
    assert!(matches!(
        inventory.network_policies,
        KindSet::Served {
            truncated: true,
            ..
        }
    ));
    assert!(inventory.declared.truncated);
    assert!(matches!(
        inventory.declared.completeness(),
        Completeness::IncompleteInventory
    ));
    let verdict = inventory.declared.verdict(
        &app_endpoint("prod", "web", "web"),
        &app_endpoint("prod", "api", "api"),
        tcp_80(),
    );
    assert_eq!(
        verdict.decision,
        Decision::Indeterminate,
        "unread policies could have allowed this pair, so deny is not proven: {verdict:?}"
    );
    drop(runtime);
}

#[test]
fn an_unreadable_policy_object_keeps_the_verdict_indeterminate() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, cilium_group());
    script.route(
        "GET",
        &list_path("ciliumnetworkpolicies"),
        200,
        serde_json::json!({
            "kind": "CiliumNetworkPolicyList",
            "items": [{ "metadata": {} }, isolating_cnp("lock-api")]
        })
        .to_string(),
    );
    script_empty_kinds(&script);

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("one undecodable object is not a whole-fetch failure: {fetched:?}");
    };
    assert!(matches!(
        inventory.network_policies,
        KindSet::Served { unreadable: 1, .. }
    ));
    assert!(inventory.declared.truncated);
    let verdict = inventory.declared.verdict(
        &app_endpoint("prod", "web", "web"),
        &app_endpoint("prod", "api", "api"),
        tcp_80(),
    );
    assert_eq!(
        verdict.decision,
        Decision::Indeterminate,
        "the unreadable object could have been the allowing policy: {verdict:?}"
    );
    drop(runtime);
}

#[test]
fn a_namespaced_fetch_never_claims_the_whole_policy_set() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, cilium_group());
    script.route(
        "GET",
        "/apis/cilium.io/v2/namespaces/prod/ciliumnetworkpolicies?",
        200,
        serde_json::json!({
            "kind": "CiliumNetworkPolicyList",
            "items": [isolating_cnp("lock-api")]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2/namespaces/prod/ciliumendpoints?",
        200,
        empty_list(),
    );
    for plural in [
        "ciliumclusterwidenetworkpolicies",
        "ciliumidentities",
        "ciliumnodes",
    ] {
        script.route("GET", &list_path(plural), 200, empty_list());
    }

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), Some("prod")).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a namespaced listing must resolve: {fetched:?}");
    };
    assert_eq!(inventory.network_policies.items().len(), 1);
    assert!(matches!(
        inventory.network_policies,
        KindSet::Served {
            truncated: false,
            unreadable: 0,
            ..
        }
    ));
    assert!(
        inventory.declared.truncated,
        "one namespace's policies structurally cannot be the whole cluster's"
    );
    assert!(matches!(
        inventory.declared.completeness(),
        Completeness::IncompleteInventory
    ));
    let verdict = inventory.declared.verdict(
        &app_endpoint("prod", "web", "web"),
        &app_endpoint("prod", "api", "api"),
        tcp_80(),
    );
    assert_eq!(verdict.decision, Decision::Indeterminate, "{verdict:?}");
    drop(runtime);
}

#[test]
fn a_404_at_the_preferred_version_falls_back_to_the_next_served_version() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/cilium.io",
        200,
        r#"{"kind":"APIGroup","name":"cilium.io",
            "versions":[{"groupVersion":"cilium.io/v2alpha1","version":"v2alpha1"},
                        {"groupVersion":"cilium.io/v2","version":"v2"}],
            "preferredVersion":{"groupVersion":"cilium.io/v2alpha1","version":"v2alpha1"}}"#
            .to_string(),
    );
    for plural in [
        "ciliumnetworkpolicies",
        "ciliumclusterwidenetworkpolicies",
        "ciliumidentities",
        "ciliumendpoints",
        "ciliumnodes",
    ] {
        script.route(
            "GET",
            &format!("/apis/cilium.io/v2alpha1/{plural}?"),
            404,
            status(404, "NotFound"),
        );
        let body = if plural == "ciliumnetworkpolicies" {
            serde_json::json!({ "kind": "CiliumNetworkPolicyList", "items": [cnp_item()] })
                .to_string()
        } else {
            empty_list()
        };
        script.route("GET", &list_path(plural), 200, body);
    }

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("the second version serves the kind: {fetched:?}");
    };
    assert_eq!(inventory.network_policies.items().len(), 1);
    assert_eq!(inventory.network_policies.items()[0].version, "v2");
    assert!(
        script.seen().iter().any(|seen| seen
            .path
            .starts_with("/apis/cilium.io/v2alpha1/ciliumnetworkpolicies")),
        "the preferred version is tried first: {:?}",
        script.seen()
    );
    assert!(
        script.seen().iter().any(|seen| seen
            .path
            .starts_with("/apis/cilium.io/v2/ciliumnetworkpolicies")),
        "a 404 walks the ladder instead of declaring NotServed: {:?}",
        script.seen()
    );
    drop(runtime);
}
