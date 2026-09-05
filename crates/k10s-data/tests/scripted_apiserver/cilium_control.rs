//! cilium.io control-plane CRs listed through kube Request. Paths, 404/403,
//! and the listing cap are proven on the wire. Declared CNP policy is not.

use crate::*;
use k10s_data::cilium_control::{self, GroupState, Kind, KindSet};
use k10s_data::read::Fetched;

const PLANTED: &str = "PLANTED_SECRET_do_not_leak";

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn group_doc() -> String {
    r#"{"kind":"APIGroup","name":"cilium.io",
        "versions":[
            {"groupVersion":"cilium.io/v2","version":"v2"},
            {"groupVersion":"cilium.io/v2alpha1","version":"v2alpha1"}
        ],
        "preferredVersion":{"groupVersion":"cilium.io/v2","version":"v2"}}"#
        .to_string()
}

fn v2_resources(extra: &str) -> String {
    format!(
        r#"{{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"cilium.io/v2","resources":[
            {{"name":"ciliumenvoyconfigs","singularName":"ciliumenvoyconfig","namespaced":true,"kind":"CiliumEnvoyConfig","verbs":["get","list","watch"]}},
            {{"name":"ciliumnetworkpolicies","singularName":"ciliumnetworkpolicy","namespaced":true,"kind":"CiliumNetworkPolicy","verbs":["get","list","watch"]}},
            {{"name":"ciliumclusterwidenetworkpolicies","singularName":"ciliumclusterwidenetworkpolicy","namespaced":false,"kind":"CiliumClusterwideNetworkPolicy","verbs":["get","list","watch"]}},
            {{"name":"ciliumendpoints","singularName":"ciliumendpoint","namespaced":true,"kind":"CiliumEndpoint","verbs":["get","list","watch"]}},
            {{"name":"ciliumidentities","singularName":"ciliumidentity","namespaced":false,"kind":"CiliumIdentity","verbs":["get","list","watch"]}},
            {{"name":"ciliumnodes","singularName":"ciliumnode","namespaced":false,"kind":"CiliumNode","verbs":["get","list","watch"]}}
            {extra}
        ]}}"#
    )
}

fn v2alpha1_resources() -> String {
    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"cilium.io/v2alpha1","resources":[
        {"name":"ciliumcidrgroups","singularName":"ciliumcidrgroup","namespaced":false,"kind":"CiliumCIDRGroup","verbs":["get","list","watch"]},
        {"name":"ciliumloadbalancerippools","singularName":"ciliumloadbalancerippool","namespaced":false,"kind":"CiliumLoadBalancerIPPool","verbs":["get","list","watch"]}
    ]}"#
    .to_string()
}

fn cec_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "edge", "namespace": "prod" },
        "spec": {
            "services": [{ "name": "frontend", "namespace": "prod" }],
            "resources": [{
                "typed_config": { "private_key": PLANTED }
            }]
        }
    })
}

fn list(kind: &str, items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": format!("{kind}List"),
        "apiVersion": "cilium.io/v2",
        "metadata": {},
        "items": items
    })
    .to_string()
}

fn forbidden_paths(seen: &[Seen]) -> bool {
    seen.iter().any(|seen| {
        seen.path.contains("ciliumnetworkpolicies")
            || seen.path.contains("ciliumclusterwidenetworkpolicies")
            || seen.path.contains("ciliumendpoints")
            || seen.path.contains("ciliumidentities")
            || seen.path.contains("ciliumnodes")
            || seen.path.contains("gateway.networking.k8s.io")
    })
}

#[test]
fn a_404_cilium_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium_control::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert_eq!(inventory.group, GroupState::NotServed);
    assert!(matches!(inventory.envoy_configs, KindSet::NotServed));
    assert!(
        cilium_control::table_page(&inventory).is_none(),
        "table_page is None only when the group is not served"
    );
    assert!(
        script.requests_for("ciliumenvoyconfigs").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script.requests_for("/apis/cilium.io/").is_empty(),
        "a 404 group must not be chased into a version document: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_cilium_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 403, status(403, "Forbidden"));
    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium_control::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied, not a whole-fetch failure: {fetched:?}");
    };
    assert_eq!(inventory.group, GroupState::Denied);
    assert!(inventory.served(), "403 is Denied, not served: false");
    let page = cilium_control::table_page(&inventory).expect("Denied still has a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("access denied for this account"), "{text}");
    assert!(
        script.requests_for("ciliumenvoyconfigs").is_empty(),
        "a 403 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn listed_paths_follow_the_version_document_and_skip_reserved_kinds() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, group_doc());
    script.route("GET", "/apis/cilium.io/v2", 200, v2_resources(""));
    script.route("GET", "/apis/cilium.io/v2alpha1", 200, v2alpha1_resources());
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumenvoyconfigs?",
        200,
        list("CiliumEnvoyConfig", &[cec_item()]),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2alpha1/ciliumcidrgroups?",
        200,
        serde_json::json!({
            "kind": "CiliumCIDRGroupList",
            "items": [{
                "metadata": { "name": "office" },
                "spec": { "externalCIDRs": ["10.0.0.0/8", "192.168.0.0/16"] }
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2alpha1/ciliumloadbalancerippools?",
        200,
        serde_json::json!({
            "kind": "CiliumLoadBalancerIPPoolList",
            "items": [{
                "metadata": { "name": "first" },
                "spec": { "disabled": true, "blocks": [{ "cidr": "10.10.10.0/24" }] }
            }]
        })
        .to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium_control::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    assert_eq!(inventory.group, GroupState::Served);
    assert_eq!(
        inventory.kinds(),
        vec![
            Kind::CiliumEnvoyConfig,
            Kind::CiliumCIDRGroup,
            Kind::CiliumLoadBalancerIPPool
        ]
    );
    let cec = &inventory.envoy_configs.items()[0];
    assert_eq!(cec.name, "edge");
    assert_eq!(cec.note, "prod/frontend");
    assert_eq!(inventory.cidr_groups.items()[0].note, "2 CIDRs");
    let pool = &inventory.load_balancer_ip_pools.items()[0];
    assert!(pool.note.contains("10.10.10.0/24"), "{}", pool.note);
    assert!(pool.note.contains("disabled"), "{}", pool.note);
    assert!(matches!(
        inventory.local_redirect_policies,
        KindSet::NotServed
    ));

    let seen = script.seen();
    assert!(
        seen.iter().any(|s| s.path == "/apis/cilium.io"),
        "group probe: {seen:?}"
    );
    assert!(
        seen.iter().any(|s| s.path == "/apis/cilium.io/v2"),
        "v2 document: {seen:?}"
    );
    assert!(
        seen.iter().any(|s| s.path == "/apis/cilium.io/v2alpha1"),
        "v2alpha1 document: {seen:?}"
    );
    assert!(
        !forbidden_paths(&seen),
        "CNP/identity/endpoint/node and Gateway API are not listed: {seen:?}"
    );
    assert!(
        !format!("{inventory:?}").contains(PLANTED),
        "the planted typed_config must not leak through Debug"
    );
    let page = cilium_control::table_page(&inventory).expect("served");
    for row in &page.rows {
        for cell in &row.cells {
            assert!(!cell.contains(PLANTED), "table cell leaked: {cell}");
        }
    }
    drop(runtime);
}

#[test]
fn a_kind_absent_from_the_document_is_skipped_not_listed() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, group_doc());
    script.route("GET", "/apis/cilium.io/v2", 200, v2_resources(""));
    script.route(
        "GET",
        "/apis/cilium.io/v2alpha1",
        200,
        r#"{"kind":"APIResourceList","groupVersion":"cilium.io/v2alpha1","resources":[]}"#,
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumenvoyconfigs?",
        200,
        list("CiliumEnvoyConfig", &[]),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium_control::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served group with a subset of kinds is Ok: {fetched:?}");
    };
    assert!(inventory.served());
    assert!(cilium_control::table_page(&inventory).is_some());
    assert!(matches!(
        inventory.egress_gateway_policies,
        KindSet::NotServed
    ));
    assert!(
        script
            .requests_for("ciliumegressgatewaypolicies")
            .is_empty(),
        "a kind the document did not name must not be listed: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_on_one_named_kind_is_denied_and_does_not_hide_the_others() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, group_doc());
    script.route("GET", "/apis/cilium.io/v2", 200, v2_resources(""));
    script.route("GET", "/apis/cilium.io/v2alpha1", 200, v2alpha1_resources());
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumenvoyconfigs?",
        403,
        status(403, "Forbidden"),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2alpha1/ciliumcidrgroups?",
        200,
        serde_json::json!({
            "kind": "CiliumCIDRGroupList",
            "items": [{ "metadata": { "name": "office" }, "spec": { "externalCIDRs": ["10.0.0.0/8"] } }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2alpha1/ciliumloadbalancerippools?",
        200,
        serde_json::json!({ "kind": "CiliumLoadBalancerIPPoolList", "items": [] }).to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium_control::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a denied kind is not a whole-fetch failure: {fetched:?}");
    };
    assert!(matches!(inventory.envoy_configs, KindSet::Denied));
    assert_eq!(inventory.cidr_groups.items()[0].name, "office");
    let page = cilium_control::table_page(&inventory).expect("served");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("CiliumEnvoyConfig"), "{text}");
    assert!(text.contains("access denied for this account"), "{text}");
    drop(runtime);
}

#[test]
fn a_403_version_document_beside_a_served_one_leaves_its_kinds_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/cilium.io", 200, group_doc());
    script.route("GET", "/apis/cilium.io/v2", 403, status(403, "Forbidden"));
    script.route("GET", "/apis/cilium.io/v2alpha1", 200, v2alpha1_resources());
    script.route(
        "GET",
        "/apis/cilium.io/v2alpha1/ciliumcidrgroups?",
        200,
        serde_json::json!({
            "kind": "CiliumCIDRGroupList",
            "items": [{ "metadata": { "name": "office" }, "spec": { "externalCIDRs": ["10.0.0.0/8"] } }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2alpha1/ciliumloadbalancerippools?",
        200,
        serde_json::json!({ "kind": "CiliumLoadBalancerIPPoolList", "items": [] }).to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium_control::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("one denied version document is not a whole-fetch failure: {fetched:?}");
    };
    assert_eq!(
        inventory.group,
        GroupState::Served,
        "v2alpha1 answered, so the group is served"
    );
    assert_eq!(inventory.cidr_groups.items()[0].name, "office");
    assert!(
        matches!(inventory.envoy_configs, KindSet::Denied),
        "a kind only the denied v2 document could have named is Denied, \
         never NotServed: {:?}",
        inventory.envoy_configs
    );
    let page = cilium_control::table_page(&inventory).expect("served");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("CiliumEnvoyConfig"), "{text}");
    assert!(text.contains("access denied for this account"), "{text}");
    assert!(
        script.requests_for("ciliumenvoyconfigs").is_empty(),
        "a kind no served document named must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn the_listing_follows_a_continue_token_and_states_the_object_cap() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/cilium.io",
        200,
        r#"{"kind":"APIGroup","name":"cilium.io",
            "versions":[{"groupVersion":"cilium.io/v2","version":"v2"}],
            "preferredVersion":{"groupVersion":"cilium.io/v2","version":"v2"}}"#,
    );
    script.route("GET", "/apis/cilium.io/v2", 200, v2_resources(""));
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumenvoyconfigs?",
        200,
        serde_json::json!({
            "kind": "CiliumEnvoyConfigList",
            "metadata": { "continue": "page-2" },
            "items": [cec_item()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/cilium.io/v2/ciliumenvoyconfigs?",
        200,
        list(
            "CiliumEnvoyConfig",
            &[serde_json::json!({
                "metadata": { "name": "mesh", "namespace": "prod" },
                "spec": { "services": [{ "name": "reviews" }] }
            })],
        ),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { cilium_control::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a paged listing must resolve: {fetched:?}");
    };
    let names: Vec<_> = inventory
        .envoy_configs
        .items()
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(names, vec!["edge", "mesh"]);

    let lists = script.requests_for("ciliumenvoyconfigs");
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[0].path.contains("limit=200"),
        "the first page is capped: {}",
        lists[0].path
    );
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
    drop(runtime);
}
