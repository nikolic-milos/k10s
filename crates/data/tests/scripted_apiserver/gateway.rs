//! Gateway API CRs listed through kube Request. A served group with zero
//! objects is a table, not absence. Paths stay on gateway.networking.k8s.io.

use crate::*;
use k10s_data::gateway::{self, KindSet};
use k10s_data::read::Fetched;

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn gateway_group() -> String {
    r#"{"kind":"APIGroup","name":"gateway.networking.k8s.io",
        "versions":[
            {"groupVersion":"gateway.networking.k8s.io/v1","version":"v1"},
            {"groupVersion":"gateway.networking.k8s.io/v1beta1","version":"v1beta1"}
        ],
        "preferredVersion":{"groupVersion":"gateway.networking.k8s.io/v1","version":"v1"}}"#
        .to_string()
}

fn empty_list(kind: &str) -> String {
    serde_json::json!({
        "kind": format!("{kind}List"),
        "apiVersion": "gateway.networking.k8s.io/v1",
        "items": []
    })
    .to_string()
}

fn script_empty_served(script: &Script) {
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io",
        200,
        gateway_group(),
    );
    for (plural, kind) in [
        ("gatewayclasses", "GatewayClass"),
        ("gateways", "Gateway"),
        ("httproutes", "HTTPRoute"),
        ("grpcroutes", "GRPCRoute"),
        ("tlsroutes", "TLSRoute"),
        ("tcproutes", "TCPRoute"),
        ("udproutes", "UDPRoute"),
        ("referencegrants", "ReferenceGrant"),
        ("backendtlspolicies", "BackendTLSPolicy"),
        ("listenersets", "ListenerSet"),
    ] {
        script.route(
            "GET",
            &format!("/apis/gateway.networking.k8s.io/v1/{plural}?"),
            200,
            empty_list(kind),
        );
    }
}

#[test]
fn a_404_gateway_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { gateway::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served);
    assert!(gateway::table_page(&inventory).is_none());
    assert!(
        script.requests_for("gateways").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_gateway_group_is_denied_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { gateway::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(inventory.served);
    assert!(matches!(inventory.gateways, KindSet::Denied));
    assert!(gateway::table_page(&inventory).is_some());
    drop(runtime);
}

#[test]
fn a_served_empty_gateway_group_is_a_table_with_zero_rows() {
    let script = Script::default();
    script_empty_served(&script);
    let runtime = runtime();
    let fetched = runtime.block_on(async { gateway::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served empty listing must resolve: {fetched:?}");
    };
    assert!(
        inventory.served,
        "this k3s: gateway.networking.k8s.io/v1 is served with zero objects"
    );
    let page = gateway::table_page(&inventory).expect("empty served is Some, not Absent");
    assert!(page.rows.is_empty(), "zero objects, still a table");
    assert!(matches!(
        inventory.gateways,
        KindSet::Served {
            ref items,
            ..
        } if items.is_empty()
    ));
    let seen = script.seen();
    assert!(
        seen.iter()
            .any(|item| item.path == "/apis/gateway.networking.k8s.io"
                || item.path.starts_with("/apis/gateway.networking.k8s.io?")),
        "the group probe is the live document: {seen:?}"
    );
    assert!(
        seen.iter().any(|item| item
            .path
            .starts_with("/apis/gateway.networking.k8s.io/v1/gatewayclasses")),
        "GatewayClass is cluster-scoped on v1: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|item| !item.path.contains("networking.istio.io")),
        "Istio Gateway is mesh.rs: {seen:?}"
    );
    assert!(
        seen.iter().any(|item| item
            .path
            .starts_with("/apis/gateway.networking.k8s.io/v1/tcproutes"))
            && seen.iter().any(|item| item
                .path
                .starts_with("/apis/gateway.networking.k8s.io/v1/udproutes")),
        "TCPRoute and UDPRoute are Standard since Gateway API 1.6: {seen:?}"
    );
    drop(runtime);
}

#[test]
fn a_403_on_one_gateway_kind_is_denied_and_does_not_hide_the_others() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io",
        200,
        gateway_group(),
    );
    for (plural, kind) in [
        ("gatewayclasses", "GatewayClass"),
        ("gateways", "Gateway"),
        ("grpcroutes", "GRPCRoute"),
        ("tlsroutes", "TLSRoute"),
        ("tcproutes", "TCPRoute"),
        ("udproutes", "UDPRoute"),
        ("referencegrants", "ReferenceGrant"),
        ("backendtlspolicies", "BackendTLSPolicy"),
        ("listenersets", "ListenerSet"),
    ] {
        script.route(
            "GET",
            &format!("/apis/gateway.networking.k8s.io/v1/{plural}?"),
            200,
            empty_list(kind),
        );
    }
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/httproutes?",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { gateway::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("one forbidden kind is Denied on that kind, not a whole-fetch failure: {fetched:?}");
    };
    assert!(matches!(inventory.http_routes, KindSet::Denied));
    assert!(
        matches!(inventory.gateways, KindSet::Served { ref items, .. } if items.is_empty()),
        "one refused kind must not hide the readable ones"
    );
    let page = gateway::table_page(&inventory).expect("Denied is served, so the table exists");
    let denied_row = page
        .rows
        .iter()
        .find(|row| row.uid == "denied:HTTPRoute")
        .expect("the refused kind stays a labelled row");
    assert!(
        denied_row
            .cells
            .iter()
            .any(|cell| cell == "access denied for this account"),
        "{denied_row:?}"
    );
    let text = gateway::render(&inventory).join("\n");
    assert!(
        text.contains("gateway httproutes: access denied for this account"),
        "a 403 is a labelled denial, not an absent kind: {text}"
    );
    drop(runtime);
}

#[test]
fn gateway_objects_are_listed_from_the_crs() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io",
        200,
        gateway_group(),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/gateways?",
        200,
        serde_json::json!({
            "kind": "GatewayList",
            "items": [{
                "metadata": { "name": "web", "namespace": "prod" },
                "spec": { "gatewayClassName": "traefik" },
                "status": {
                    "addresses": [{ "value": "10.0.0.8" }],
                    "conditions": [
                        { "type": "Accepted", "status": "True" },
                        { "type": "Programmed", "status": "True" }
                    ]
                }
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/httproutes?",
        200,
        serde_json::json!({
            "kind": "HTTPRouteList",
            "items": [{
                "metadata": { "name": "app", "namespace": "prod" },
                "spec": {
                    "parentRefs": [{ "name": "web" }],
                    "hostnames": ["app.example.com"],
                    "rules": [{ "backendRefs": [{ "name": "app-svc" }] }]
                }
            }]
        })
        .to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { gateway::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve");
    };
    assert!(inventory.served);
    let gateway = &inventory.gateways.items()[0];
    assert_eq!(gateway.name, "web");
    assert_eq!(gateway.class, "traefik");
    assert_eq!(gateway.addresses, "10.0.0.8");
    assert_eq!(gateway.programmed, "True");
    let route = &inventory.http_routes.items()[0];
    assert_eq!(route.hostnames, "app.example.com");
    assert_eq!(route.backends, "app-svc");
    drop(runtime);
}
