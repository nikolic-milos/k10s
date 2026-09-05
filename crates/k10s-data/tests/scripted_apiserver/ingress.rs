//! Ingress and IngressClass listed as core networking.k8s.io/v1 kinds.

use crate::*;
use k10s_data::ingress;
use k10s_data::read::Fetched;

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn class_item() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "IngressClass",
        "metadata": {
            "name": "traefik",
            "uid": "cb11c217-f131-486f-8ced-043f8522781d",
            "annotations": {
                "ingressclass.kubernetes.io/is-default-class": "true"
            }
        },
        "spec": { "controller": "traefik.io/ingress-controller" }
    })
}

fn ingress_item() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "Ingress",
        "metadata": { "name": "www", "namespace": "prod", "uid": "ing-1" },
        "spec": {
            "ingressClassName": "traefik",
            "rules": [{
                "host": "example.com",
                "http": {
                    "paths": [{
                        "path": "/",
                        "pathType": "Prefix",
                        "backend": {
                            "service": { "name": "web", "port": { "number": 80 } }
                        }
                    }]
                }
            }],
            "tls": [{
                "secretName": "edge-tls",
                "data": { "tls.crt": "PLANTED_CERT", "tls.key": "PLANTED_KEY" }
            }]
        },
        "status": { "loadBalancer": { "ingress": [{ "ip": "127.0.0.1" }] } }
    })
}

fn class_list(items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": "IngressClassList",
        "apiVersion": "networking.k8s.io/v1",
        "metadata": {},
        "items": items
    })
    .to_string()
}

fn ingress_list(items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": "IngressList",
        "apiVersion": "networking.k8s.io/v1",
        "metadata": {},
        "items": items
    })
    .to_string()
}

fn script_lists(script: &Script, classes: &[serde_json::Value], ingresses: &[serde_json::Value]) {
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        200,
        class_list(classes),
    );
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingresses?",
        200,
        ingress_list(ingresses),
    );
}

#[test]
fn ingressclass_and_empty_ingress_are_listed_on_the_core_paths() {
    let script = Script::default();
    script_lists(&script, &[class_item()], &[]);
    let runtime = runtime();
    let fetched = runtime.block_on(async { ingress::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("core kinds must resolve: {fetched:?}");
    };
    assert_eq!(inventory.classes.len(), 1);
    assert_eq!(inventory.classes[0].name, "traefik");
    assert!(inventory.classes[0].is_default);
    assert_eq!(
        inventory.classes[0].controller,
        "traefik.io/ingress-controller"
    );
    assert!(inventory.ingresses.is_empty());
    let page = ingress::table_page(&inventory).expect("core kinds always have a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].cells[0], "IngressClass");
    assert!(
        script.seen().iter().any(|seen| seen
            .path
            .contains("/apis/networking.k8s.io/v1/ingressclasses")),
        "{:?}",
        script.seen()
    );
    assert!(
        script
            .seen()
            .iter()
            .any(|seen| seen.path.contains("/apis/networking.k8s.io/v1/ingresses")),
        "the empty Ingress list is still asked for: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn planted_tls_bytes_are_not_in_the_inventory() {
    let script = Script::default();
    script_lists(&script, &[class_item()], &[ingress_item()]);
    let runtime = runtime();
    let fetched = runtime.block_on(async { ingress::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    assert_eq!(inventory.ingresses[0].tls_secrets, ["edge-tls"]);
    assert_eq!(inventory.ingresses[0].paths[0].backend.service, "web");
    assert_eq!(inventory.ingresses[0].address, "127.0.0.1");
    let debug = format!("{inventory:?}");
    assert!(!debug.contains("PLANTED_CERT"), "{debug}");
    assert!(!debug.contains("PLANTED_KEY"), "{debug}");
    let page = ingress::table_page(&inventory).expect("a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("edge-tls"), "{text}");
    assert!(!text.contains("PLANTED"), "{text}");
    drop(runtime);
}

#[test]
fn a_403_on_ingressclass_keeps_the_readable_ingresses() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        403,
        status(403, "Forbidden"),
    );
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingresses?",
        200,
        ingress_list(&[]),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { ingress::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("one denied side must not hide the readable one: {fetched:?}");
    };
    assert!(inventory.classes_denied);
    assert!(!inventory.ingresses_denied);
    drop(runtime);
}

#[test]
fn a_403_on_ingress_keeps_the_readable_classes() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        200,
        class_list(&[class_item()]),
    );
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingresses?",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { ingress::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("one denied side must not hide the readable one: {fetched:?}");
    };
    assert!(inventory.ingresses_denied);
    assert_eq!(inventory.classes.len(), 1);
    drop(runtime);
}
