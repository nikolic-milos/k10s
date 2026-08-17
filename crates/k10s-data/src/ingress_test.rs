//! Field extraction, the default class mark, caps, 403, and the empty
//! Ingress list this cluster actually has.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

fn class_json() -> serde_json::Value {
    json!({
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

fn ingress_json() -> serde_json::Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "Ingress",
        "metadata": {
            "name": "www",
            "namespace": "prod",
            "uid": "ing-1"
        },
        "spec": {
            "ingressClassName": "traefik",
            "defaultBackend": {
                "service": { "name": "fallback", "port": { "number": 8080 } }
            },
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
                "hosts": ["example.com"],
                "secretName": "edge-tls",
                "data": {
                    "tls.crt": "PLANTED_CERT",
                    "tls.key": "PLANTED_KEY"
                }
            }]
        },
        "status": {
            "loadBalancer": {
                "ingress": [{ "ip": "127.0.0.1" }]
            }
        }
    })
}

fn class_from_json(value: serde_json::Value) -> Class {
    let object: ApiClass = serde_json::from_value(value).expect("IngressClass fixture");
    from_class(object).expect("named class")
}

fn ingress_from_json(value: serde_json::Value) -> Ingress {
    let object: ApiIngress = serde_json::from_value(value).expect("Ingress fixture");
    from_ingress(object).expect("named ingress")
}

#[test]
fn a_default_ingressclass_keeps_its_controller_and_is_marked() {
    let class = class_from_json(class_json());
    assert_eq!(class.name, "traefik");
    assert_eq!(class.uid, "cb11c217-f131-486f-8ced-043f8522781d");
    assert_eq!(class.controller, "traefik.io/ingress-controller");
    assert!(class.is_default);
}

#[test]
fn an_ingress_keeps_class_hosts_paths_backend_tls_name_and_address() {
    let ingress = ingress_from_json(ingress_json());
    assert_eq!(ingress.name, "www");
    assert_eq!(ingress.namespace, "prod");
    assert_eq!(ingress.class, "traefik");
    assert_eq!(ingress.hosts, ["example.com"]);
    assert_eq!(ingress.tls_secrets, ["edge-tls"]);
    assert_eq!(ingress.address, "127.0.0.1");
    assert_eq!(ingress.paths[0].backend.service, "fallback");
    assert_eq!(ingress.paths[0].backend.port, "8080");
    assert_eq!(ingress.paths[1].host, "example.com");
    assert_eq!(ingress.paths[1].path, "/");
    assert_eq!(ingress.paths[1].backend.service, "web");
    assert_eq!(ingress.paths[1].backend.port, "80");
}

#[test]
fn planted_tls_bytes_never_enter_the_inventory_or_its_debug() {
    let ingress = ingress_from_json(ingress_json());
    assert_eq!(ingress.tls_secrets, ["edge-tls"]);
    let debug = format!("{ingress:?}");
    assert!(
        !debug.contains("PLANTED_CERT") && !debug.contains("PLANTED_KEY"),
        "certificate bytes must not be carried: {debug}"
    );
    let page = table_page(&Inventory {
        ingresses: vec![ingress],
        ..Inventory::default()
    })
    .expect("a fetched inventory always has a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("edge-tls"), "{text}");
    assert!(!text.contains("PLANTED"), "{text}");
}

#[test]
fn class_falls_back_to_the_legacy_annotation() {
    let ingress = ingress_from_json(json!({
        "metadata": {
            "name": "legacy",
            "namespace": "prod",
            "annotations": { "kubernetes.io/ingress.class": "nginx" }
        },
        "spec": {
            "rules": [{
                "http": {
                    "paths": [{
                        "path": "/api",
                        "pathType": "Prefix",
                        "backend": {
                            "service": { "name": "api", "port": { "name": "http" } }
                        }
                    }]
                }
            }]
        }
    }));
    assert_eq!(ingress.class, "nginx");
    assert_eq!(ingress.paths[0].backend.port, "http");
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let ingress = ingress_from_json(json!({
        "metadata": { "name": huge, "namespace": huge },
        "spec": {
            "ingressClassName": huge,
            "rules": [{
                "host": huge,
                "http": {
                    "paths": [{
                        "path": huge,
                        "pathType": "Prefix",
                        "backend": { "service": { "name": huge, "port": { "number": 80 } } }
                    }]
                }
            }],
            "tls": [{ "secretName": huge }]
        },
        "status": { "loadBalancer": { "ingress": [{ "ip": huge }] } }
    }));
    for field in [
        &ingress.name,
        &ingress.namespace,
        &ingress.class,
        &ingress.address,
        &ingress.hosts[0],
        &ingress.paths[0].path,
        &ingress.paths[0].backend.service,
        &ingress.tls_secrets[0],
    ] {
        assert!(
            field.chars().count() <= MAX_FIELD_CHARS + 1,
            "every field is clipped where it is carried: {} chars",
            field.chars().count()
        );
        assert!(field.ends_with('\u{2026}'), "and looks clipped");
    }
}

#[test]
fn an_empty_inventory_still_has_a_table() {
    let page = table_page(&Inventory::default()).expect("core kinds always have a table");
    assert!(page.rows.is_empty());
    assert!(!page.truncated);
}

#[test]
fn a_default_class_is_a_marked_row_even_with_zero_ingresses() {
    let page = table_page(&Inventory {
        classes: vec![class_from_json(class_json())],
        ..Inventory::default()
    })
    .expect("one class is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "traefik");
    assert_eq!(page.rows[0].cells[0], "IngressClass");
    assert!(
        page.rows[0].cells[3].contains("default"),
        "{}",
        page.rows[0].cells[3]
    );
    assert_eq!(page.rows[0].cells[8], "default");
}

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
}

struct Route {
    method: &'static str,
    matches: String,
    status: u16,
    body: String,
    used: bool,
}

#[derive(Default)]
struct State {
    routes: Vec<Route>,
    seen: Vec<Seen>,
}

#[derive(Clone, Default)]
struct Script {
    state: Arc<Mutex<State>>,
}

impl Script {
    fn route(
        &self,
        method: &'static str,
        matches: &str,
        status: u16,
        body: impl Into<String>,
    ) -> &Self {
        self.state.lock().expect("script lock").routes.push(Route {
            method,
            matches: matches.to_string(),
            status,
            body: body.into(),
            used: false,
        });
        self
    }

    fn seen(&self) -> Vec<Seen> {
        self.state.lock().expect("script lock").seen.clone()
    }

    fn client(&self) -> kube::Client {
        kube::Client::new(self.clone(), "default")
    }
}

impl Service<http::Request<Body>> for Script {
    type Response = http::Response<Body>;
    type Error = tower::BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let method = req.method().to_string();
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default();
        let answer = {
            let mut state = self.state.lock().expect("script lock");
            state.seen.push(Seen {
                method: method.clone(),
                path: path.clone(),
            });
            let routable = path.replacen("?&", "?", 1);
            let hit = state.routes.iter_mut().find(|route| {
                !route.used && route.method == method && routable.starts_with(&route.matches)
            });
            match hit {
                Some(route) => {
                    route.used = true;
                    (route.status, route.body.clone())
                }
                None => (
                    404,
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"unscripted"}"#
                        .to_string(),
                ),
            }
        };
        Box::pin(async move {
            Ok(http::Response::builder()
                .status(answer.0)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(answer.1.into_bytes()))
                .expect("a response"))
        })
    }
}

const STATUS_403: &str = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"forbidden"}"#;

fn class_list(items: &[serde_json::Value]) -> String {
    json!({
        "kind": "IngressClassList",
        "apiVersion": "networking.k8s.io/v1",
        "metadata": {},
        "items": items
    })
    .to_string()
}

fn ingress_list(items: &[serde_json::Value]) -> String {
    json!({
        "kind": "IngressList",
        "apiVersion": "networking.k8s.io/v1",
        "metadata": {},
        "items": items
    })
    .to_string()
}

fn script_core(script: &Script, classes: &[serde_json::Value], ingresses: &[serde_json::Value]) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cluster_with_traefik_class_and_zero_ingresses_is_served() {
    let script = Script::default();
    script_core(&script, &[class_json()], &[]);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("core kinds on a normal cluster must resolve");
    };
    assert_eq!(inventory.classes.len(), 1);
    assert!(inventory.classes[0].is_default);
    assert_eq!(inventory.classes[0].name, "traefik");
    assert!(inventory.ingresses.is_empty());
    let page = table_page(&inventory).expect("core kinds always have a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].cells[0], "IngressClass");
    let seen = script.seen();
    assert!(
        seen.iter()
            .any(|seen| seen.path.contains("/ingressclasses")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|seen| seen.path.contains("/ingresses?")),
        "the empty Ingress list is still asked for: {seen:?}"
    );
    assert!(
        seen.iter().all(|seen| seen.method == "GET"),
        "an Ingress fetch only reads: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ingress_list_extracts_tls_names_and_never_the_planted_bytes() {
    let script = Script::default();
    script_core(&script, &[class_json()], &[ingress_json()]);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    assert_eq!(inventory.ingresses[0].tls_secrets, ["edge-tls"]);
    let debug = format!("{inventory:?}");
    assert!(!debug.contains("PLANTED_CERT"), "{debug}");
    assert!(!debug.contains("PLANTED_KEY"), "{debug}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_ingressclass_keeps_the_readable_ingresses() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        403,
        STATUS_403,
    );
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingresses?",
        200,
        ingress_list(&[ingress_json()]),
    );
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("one denied side must not hide the readable one");
    };
    assert!(inventory.classes_denied);
    assert!(!inventory.ingresses_denied);
    assert_eq!(inventory.ingresses.len(), 1);
    let page = table_page(&inventory).expect("core kinds always have a table");
    assert!(
        page.rows.iter().any(|row| {
            row.cells[0] == "IngressClass"
                && row
                    .cells
                    .contains(&"access denied for this account".to_string())
        }),
        "the denial stays a labelled row: {:?}",
        page.rows
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_ingress_keeps_the_readable_classes() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        200,
        class_list(&[class_json()]),
    );
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingresses?",
        403,
        STATUS_403,
    );
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("one denied side must not hide the readable one");
    };
    assert!(inventory.ingresses_denied);
    assert_eq!(inventory.classes.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_both_core_lists_is_denied() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        403,
        STATUS_403,
    );
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingresses?",
        403,
        STATUS_403,
    );
    assert_eq!(
        fetch(&script.client(), None).await,
        Fetched::Denied { what: WHAT }
    );
}
