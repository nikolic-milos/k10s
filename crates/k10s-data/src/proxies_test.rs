//! Field extraction, 404/403 classification, planted TLS bytes, and the
//! table that stays hidden when every proxy group is absent.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

fn httpproxy_json() -> serde_json::Value {
    json!({
        "metadata": { "name": "www", "namespace": "prod", "uid": "hp-1" },
        "spec": {
            "virtualhost": {
                "fqdn": "example.com",
                "tls": {
                    "secretName": "edge-tls",
                    "data": {
                        "tls.crt": "PLANTED_CERT",
                        "tls.key": "PLANTED_KEY"
                    }
                }
            },
            "routes": [{
                "conditions": [{ "prefix": "/" }],
                "services": [{ "name": "web", "port": 80 }]
            }]
        }
    })
}

fn kong_plugin_json() -> serde_json::Value {
    json!({
        "metadata": { "name": "rate", "namespace": "prod" },
        "plugin": "rate-limiting",
        "config": { "minute": 5, "password": "PLANTED_PASSWORD" }
    })
}

fn resource_from(kind: Kind, group: &str, version: &str, value: serde_json::Value) -> Resource {
    parse_item(kind, group, version, value).expect("the fixture is a proxy object")
}

#[test]
fn a_contour_httpproxy_keeps_host_backend_and_tls_name() {
    let resource = resource_from(Kind::HttpProxy, CONTOUR_GROUP, "v1", httpproxy_json());
    assert_eq!(resource.name, "www");
    assert_eq!(resource.namespace, "prod");
    assert_eq!(resource.hosts, ["example.com"]);
    assert_eq!(resource.backends, ["web:80"]);
    assert_eq!(resource.tls_secrets, ["edge-tls"]);
}

#[test]
fn planted_tls_bytes_never_enter_the_inventory_or_its_debug() {
    let resource = resource_from(Kind::HttpProxy, CONTOUR_GROUP, "v1", httpproxy_json());
    let debug = format!("{resource:?}");
    assert_eq!(resource.tls_secrets, ["edge-tls"]);
    assert!(
        !debug.contains("PLANTED_CERT") && !debug.contains("PLANTED_KEY"),
        "certificate bytes must not be carried: {debug}"
    );
    let page = table_page(&Inventory {
        contour: KindSet::Served {
            items: vec![resource],
            truncated: false,
            unreadable: 0,
            denied: 0,
            failed: 0,
        },
        ..Inventory::default()
    })
    .expect("a served controller is a table");
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
fn a_kong_plugin_keeps_its_name_and_drops_config_credentials() {
    let resource = resource_from(Kind::KongPlugin, KONG_GROUP, "v1", kong_plugin_json());
    assert_eq!(resource.detail, "rate-limiting");
    let debug = format!("{resource:?}");
    assert!(
        !debug.contains("PLANTED_PASSWORD"),
        "plugin config is not inventory: {debug}"
    );
}

#[test]
fn nginx_virtualserver_and_ambassador_host_read_tls_secret_names() {
    let virtual_server = resource_from(
        Kind::VirtualServer,
        NGINX_GROUP,
        "v1",
        json!({
            "metadata": { "name": "www", "namespace": "prod" },
            "spec": {
                "host": "shop.example.com",
                "tls": { "secret": "shop-tls", "data": { "tls.crt": "PLANTED_CERT" } },
                "upstreams": [{ "name": "shop", "service": "shop-svc", "port": 80 }]
            }
        }),
    );
    assert_eq!(virtual_server.hosts, ["shop.example.com"]);
    assert_eq!(virtual_server.tls_secrets, ["shop-tls"]);
    assert_eq!(virtual_server.backends, ["shop-svc:80"]);
    assert!(!format!("{virtual_server:?}").contains("PLANTED"));

    let host = resource_from(
        Kind::Host,
        AMBASSADOR_GROUP,
        "v3alpha1",
        json!({
            "metadata": { "name": "edge", "namespace": "emissary" },
            "spec": {
                "hostname": "edge.example.com",
                "tlsSecret": { "name": "edge-tls", "data": { "tls.key": "PLANTED_KEY" } }
            }
        }),
    );
    assert_eq!(host.hosts, ["edge.example.com"]);
    assert_eq!(host.tls_secrets, ["edge-tls"]);
    assert!(!format!("{host:?}").contains("PLANTED"));
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let resource = resource_from(
        Kind::HttpProxy,
        CONTOUR_GROUP,
        "v1",
        json!({
            "metadata": { "name": huge, "namespace": huge },
            "spec": {
                "virtualhost": { "fqdn": huge, "tls": { "secretName": huge } },
                "routes": [{ "services": [{ "name": huge, "port": 80 }] }]
            }
        }),
    );
    for field in [
        &resource.name,
        &resource.namespace,
        &resource.hosts[0],
        &resource.backends[0],
        &resource.tls_secrets[0],
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
fn an_unserved_proxy_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "every group 404 is absence, not an empty list"
    );
}

#[test]
fn a_denied_controller_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        contour: KindSet::Denied,
        ..Inventory::default()
    })
    .expect("Denied is served, so the table exists");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("access denied for this account"),
        "a 403 stays labelled: {text}"
    );
    assert!(text.contains("Contour"), "{text}");
}

#[test]
fn undecodable_objects_are_a_counted_marker_row_not_silence() {
    let page = table_page(&Inventory {
        contour: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 2,
            denied: 0,
            failed: 0,
        },
        kong: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 1,
            denied: 0,
            failed: 0,
        },
        ..Inventory::default()
    })
    .expect("a served controller is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("2 objects could not be decoded and are not shown"),
        "unparseable objects must not vanish without a labelled row: {text}"
    );
    assert!(
        text.contains("1 object could not be decoded and is not shown"),
        "a single unreadable object reads as one, not a plural: {text}"
    );
}

#[test]
fn a_missing_group_is_invisible_and_a_forbidden_one_is_denied() {
    assert!(matches!(
        after_group(&api_error(404)),
        GroupAnswer::NotServed
    ));
    assert!(
        matches!(after_group(&api_error(403)), GroupAnswer::Denied),
        "a 403 is Denied, never an empty inventory that looks like the controller is absent"
    );
    assert!(matches!(after_group(&api_error(401)), GroupAnswer::Denied));
    assert!(matches!(
        after_group(&api_error(500)),
        GroupAnswer::Failed(_)
    ));
    assert!(matches!(after_list(&api_error(404)), ListErr::NotFound));
    assert!(matches!(after_list(&api_error(403)), ListErr::Denied));
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
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

fn group_doc(name: &str, version: &str) -> String {
    format!(
        r#"{{"kind":"APIGroup","name":"{name}","versions":[{{"groupVersion":"{name}/{version}","version":"{version}"}}],"preferredVersion":{{"groupVersion":"{name}/{version}","version":"{version}"}}}}"#
    )
}

fn list(kind: &str, items: &[serde_json::Value]) -> String {
    json!({
        "kind": format!("{kind}List"),
        "metadata": {},
        "items": items
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_on_every_proxy_group_is_not_served_not_failed() {
    let script = Script::default();
    let fetched = fetch(&script.client(), None).await;
    let Fetched::Ok(inventory) = fetched else {
        panic!("a missing group is not a failure: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.contour, KindSet::NotServed));
    assert!(matches!(inventory.envoy_gateway, KindSet::NotServed));
    assert!(matches!(inventory.haproxy, KindSet::NotServed));
    assert!(matches!(inventory.kong, KindSet::NotServed));
    assert!(matches!(inventory.nginx, KindSet::NotServed));
    assert!(matches!(inventory.ambassador, KindSet::NotServed));
    assert!(table_page(&inventory).is_none());
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("httpproxies")
                && !seen.path.contains("kongplugins")
                && !seen.path.contains("virtualservers")),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script.seen().iter().all(|seen| seen.method == "GET"),
        "a proxy fetch only reads: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_a_proxy_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/projectcontour.io", 403, STATUS_403);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied on that controller, not a whole-fetch failure");
    };
    assert!(matches!(inventory.contour, KindSet::Denied));
    assert!(
        inventory.contour.served(),
        "403 is Denied, not served: false"
    );
    assert!(matches!(inventory.kong, KindSet::NotServed));
    let page = table_page(&inventory).expect("Denied is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("access denied for this account"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_contour_list_extracts_fields_and_skips_a_404_haproxy_legacy_group() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/projectcontour.io",
        200,
        group_doc(CONTOUR_GROUP, "v1"),
    );
    script.route(
        "GET",
        "/apis/projectcontour.io/v1/httpproxies?",
        200,
        list("HTTPProxy", &[httpproxy_json()]),
    );
    script.route(
        "GET",
        "/apis/projectcontour.io/v1/tlscertificatedelegations?",
        200,
        list("TLSCertificateDelegation", &[]),
    );
    script.route(
        "GET",
        "/apis/ingress.v1.haproxy.org",
        200,
        group_doc(HAPROXY_V1_GROUP, "v1"),
    );
    script.route(
        "GET",
        "/apis/ingress.v1.haproxy.org/v1/backends?",
        200,
        list(
            "Backend",
            &[json!({ "metadata": { "name": "api", "namespace": "prod" } })],
        ),
    );
    script.route(
        "GET",
        "/apis/ingress.v1.haproxy.org/v1/defaults?",
        200,
        list("Defaults", &[]),
    );
    script.route(
        "GET",
        "/apis/ingress.v1.haproxy.org/v1/globals?",
        200,
        list("Global", &[]),
    );
    script.route(
        "GET",
        "/apis/ingress.v1.haproxy.org/v1/tcps?",
        200,
        list("TCP", &[]),
    );

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    let proxy = &inventory.contour.items()[0];
    assert_eq!(proxy.name, "www");
    assert_eq!(proxy.tls_secrets, ["edge-tls"]);
    assert!(!format!("{proxy:?}").contains("PLANTED"));
    assert_eq!(inventory.haproxy.items()[0].name, "api");
    assert!(
        script
            .seen()
            .iter()
            .any(|seen| seen.path == "/apis/core.haproxy.org"),
        "the legacy group is probed: {:?}",
        script.seen()
    );
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("core.haproxy.org/")),
        "a 404 HAProxy group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(table_page(&inventory).is_some());
}
