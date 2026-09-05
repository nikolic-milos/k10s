//! Field extraction, caps, the document, 404/403 classification, and the
//! live-true empty table: a served group with zero objects is Some, not
//! Absent. A cluster is not required.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

fn gatewayclass_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "traefik" },
        "spec": { "controllerName": "traefik.io/gateway-controller" },
        "status": {
            "conditions": [{ "type": "Accepted", "status": "True" }]
        }
    })
}

fn gateway_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "web", "namespace": "prod" },
        "spec": {
            "gatewayClassName": "traefik",
            "listeners": [{
                "name": "https",
                "port": 443,
                "protocol": "HTTPS",
                "hostname": "web.example.com"
            }]
        },
        "status": {
            "addresses": [{ "type": "IPAddress", "value": "10.0.0.8" }],
            "conditions": [
                { "type": "Accepted", "status": "True" },
                { "type": "Programmed", "status": "True" }
            ]
        }
    })
}

fn httproute_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "app", "namespace": "prod" },
        "spec": {
            "parentRefs": [{ "name": "web", "namespace": "prod" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "backendRefs": [{ "name": "app-svc", "port": 80 }]
            }]
        },
        "status": {
            "parents": [{
                "conditions": [{ "type": "Accepted", "status": "True" }]
            }]
        }
    })
}

fn grpcroute_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "rpc", "namespace": "prod" },
        "spec": {
            "parentRefs": [{ "name": "web" }],
            "hostnames": ["rpc.example.com"],
            "rules": [{
                "backendRefs": [{ "name": "rpc-svc" }]
            }]
        }
    })
}

fn tlsroute_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "passthrough", "namespace": "prod" },
        "spec": {
            "parentRefs": [{ "name": "web" }],
            "hostnames": ["tls.example.com"],
            "rules": [{
                "backendRefs": [{ "name": "tls-svc" }]
            }]
        }
    })
}

fn tcproute_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "db", "namespace": "prod" },
        "spec": {
            "parentRefs": [{ "name": "web", "sectionName": "postgres" }],
            "rules": [{
                "backendRefs": [{ "name": "db-svc", "port": 5432 }]
            }]
        },
        "status": {
            "parents": [{
                "conditions": [{ "type": "Accepted", "status": "True" }]
            }]
        }
    })
}

fn udproute_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "dns", "namespace": "prod" },
        "spec": {
            "parentRefs": [{ "name": "web" }],
            "rules": [{
                "backendRefs": [{ "name": "dns-svc", "port": 53 }]
            }]
        },
        "status": {
            "parents": [{
                "conditions": [{ "type": "Accepted", "status": "True" }]
            }]
        }
    })
}

fn referencegrant_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "from-other", "namespace": "prod" },
        "spec": {
            "from": [{
                "group": "gateway.networking.k8s.io",
                "kind": "HTTPRoute",
                "namespace": "other"
            }],
            "to": [{ "group": "", "kind": "Service" }]
        }
    })
}

fn backendtls_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "origin", "namespace": "prod" },
        "spec": {
            "targetRefs": [{ "kind": "Service", "name": "origin-svc" }],
            "validation": { "hostname": "origin.example.com" }
        }
    })
}

fn listenerset_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "extra", "namespace": "prod" },
        "spec": {
            "parentRef": { "name": "web", "namespace": "prod", "kind": "Gateway" },
            "listeners": [{
                "name": "http",
                "port": 80,
                "protocol": "HTTP",
                "hostname": "extra.example.com"
            }]
        },
        "status": {
            "conditions": [{ "type": "Accepted", "status": "True" }]
        }
    })
}

fn resource_from(kind: Kind, value: serde_json::Value) -> Resource {
    parse_item(kind, "v1", value).expect("the fixture is a Gateway API object")
}

fn empty_served() -> Inventory {
    Inventory {
        served: true,
        gateway_classes: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        gateways: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        http_routes: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        grpc_routes: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        tls_routes: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        tcp_routes: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        udp_routes: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        reference_grants: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        backend_tls_policies: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        listener_sets: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
    }
}

#[test]
fn a_gatewayclass_keeps_controller_and_accepted() {
    let resource = resource_from(Kind::GatewayClass, gatewayclass_json());
    assert_eq!(resource.name, "traefik");
    assert!(resource.namespace.is_empty());
    assert_eq!(resource.class, "traefik.io/gateway-controller");
    assert_eq!(resource.accepted, "True");
}

#[test]
fn a_gateway_keeps_class_addresses_and_programmed() {
    let resource = resource_from(Kind::Gateway, gateway_json());
    assert_eq!(resource.class, "traefik");
    assert_eq!(resource.addresses, "10.0.0.8");
    assert_eq!(resource.accepted, "True");
    assert_eq!(resource.programmed, "True");
    assert_eq!(
        resource.hostnames, "web.example.com",
        "a Gateway carries hostnames on its listeners, not on spec.hostnames"
    );
}

#[test]
fn an_httproute_keeps_parent_host_and_backend_service() {
    let resource = resource_from(Kind::HTTPRoute, httproute_json());
    assert_eq!(resource.parent_refs, "prod/web");
    assert_eq!(resource.hostnames, "app.example.com");
    assert_eq!(resource.backends, "app-svc");
    assert_eq!(resource.accepted, "True");
}

#[test]
fn grpc_and_tls_routes_keep_the_same_shape() {
    let grpc = resource_from(Kind::GRPCRoute, grpcroute_json());
    assert_eq!(grpc.backends, "rpc-svc");
    assert_eq!(grpc.hostnames, "rpc.example.com");
    let tls = resource_from(Kind::TLSRoute, tlsroute_json());
    assert_eq!(tls.backends, "tls-svc");
    assert_eq!(tls.hostnames, "tls.example.com");
}

#[test]
fn tcp_and_udp_routes_keep_parents_backends_and_parent_accepted() {
    let tcp = resource_from(Kind::TCPRoute, tcproute_json());
    assert_eq!(tcp.parent_refs, "web");
    assert_eq!(tcp.backends, "db-svc");
    assert_eq!(tcp.accepted, "True");
    let udp = resource_from(Kind::UDPRoute, udproute_json());
    assert_eq!(udp.parent_refs, "web");
    assert_eq!(udp.backends, "dns-svc");
    assert_eq!(udp.accepted, "True");
}

#[test]
fn a_referencegrant_and_backendtls_keep_peers_and_targets() {
    let grant = resource_from(Kind::ReferenceGrant, referencegrant_json());
    assert_eq!(grant.parent_refs, "HTTPRoute/other");
    assert_eq!(grant.backends, "Service");
    let policy = resource_from(Kind::BackendTLSPolicy, backendtls_json());
    assert_eq!(policy.backends, "Service/origin-svc");
    assert_eq!(policy.hostnames, "origin.example.com");
}

#[test]
fn a_listenerset_keeps_parent_gateway_and_listener_hostname() {
    let set = resource_from(Kind::ListenerSet, listenerset_json());
    assert_eq!(set.name, "extra");
    assert_eq!(set.namespace, "prod");
    assert_eq!(set.parent_refs, "Gateway/prod/web");
    assert_eq!(set.hostnames, "extra.example.com");
    assert_eq!(set.accepted, "True");
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::Gateway, "v1", serde_json::json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = serde_json::json!({
        "metadata": { "name": huge, "namespace": "prod" },
        "spec": {
            "gatewayClassName": huge,
            "hostnames": [huge],
            "parentRefs": [{ "name": huge }]
        },
        "status": { "addresses": [{ "value": huge }] }
    });
    let resource = resource_from(Kind::Gateway, value);
    for field in [
        &resource.name,
        &resource.class,
        &resource.addresses,
        &resource.hostnames,
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
fn a_missing_group_is_invisible_and_a_forbidden_one_is_denied() {
    assert!(matches!(
        after_group(&api_error(404)),
        GroupAnswer::NotServed
    ));
    assert!(
        matches!(after_group(&api_error(403)), GroupAnswer::Denied),
        "a 403 is Denied, never an empty inventory that looks like Gateway API is absent"
    );
    assert!(matches!(after_group(&api_error(401)), GroupAnswer::Denied));
    assert!(matches!(
        after_group(&api_error(500)),
        GroupAnswer::Failed(_)
    ));
    assert!(matches!(after_list(&api_error(404)), ListErr::NotFound));
    assert!(matches!(after_list(&api_error(403)), ListErr::Denied));
}

#[test]
fn collection_paths_scope_by_namespace_except_the_cluster_scoped_class() {
    assert_eq!(
        collection_url(Kind::GatewayClass, "v1", Some("prod")),
        "/apis/gateway.networking.k8s.io/v1/gatewayclasses",
        "GatewayClass is cluster-scoped: a namespace must not be appended"
    );
    assert_eq!(
        collection_url(Kind::Gateway, "v1", Some("prod")),
        "/apis/gateway.networking.k8s.io/v1/namespaces/prod/gateways"
    );
    assert_eq!(
        collection_url(Kind::GatewayClass, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/gatewayclasses"
    );
    assert_eq!(
        collection_url(Kind::Gateway, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/gateways"
    );
    assert_eq!(
        collection_url(Kind::HTTPRoute, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/httproutes"
    );
    assert_eq!(
        collection_url(Kind::GRPCRoute, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/grpcroutes"
    );
    assert_eq!(
        collection_url(Kind::TLSRoute, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/tlsroutes"
    );
    assert_eq!(
        collection_url(Kind::TCPRoute, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/tcproutes"
    );
    assert_eq!(
        collection_url(Kind::UDPRoute, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/udproutes"
    );
    assert_eq!(
        collection_url(Kind::ReferenceGrant, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/referencegrants"
    );
    assert_eq!(
        collection_url(Kind::BackendTLSPolicy, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/backendtlspolicies"
    );
    assert_eq!(
        collection_url(Kind::ListenerSet, "v1", None),
        "/apis/gateway.networking.k8s.io/v1/listenersets"
    );
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn an_unserved_gateway_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "a 404 group is absence, not an empty list"
    );
}

#[test]
fn a_served_empty_cluster_is_a_table_with_zero_rows() {
    let page = table_page(&empty_served()).expect("a served group is a table even with no objects");
    assert!(
        page.rows.is_empty(),
        "this k3s: served, zero objects, not Absent"
    );
    assert!(!page.columns.is_empty());
}

#[test]
fn a_denied_gateway_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        served: true,
        gateways: KindSet::Denied,
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
    assert!(text.contains("Gateway"), "{text}");
}

#[test]
fn a_served_gateway_fixture_is_one_row_per_object() {
    let gateway = resource_from(Kind::Gateway, gateway_json());
    let page = table_page(&Inventory {
        served: true,
        gateways: KindSet::Served {
            items: vec![gateway],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "web");
    assert_eq!(page.rows[0].cells[0], "Gateway");
    assert_eq!(page.rows[0].cells[3], "traefik");
    assert_eq!(page.rows[0].cells[4], "10.0.0.8");
    assert!(
        page.rows[0].cells[5].contains("Programmed=True"),
        "{:?}",
        page.rows[0].cells
    );
}

#[test]
fn a_missing_gateway_group_renders_as_not_installed_rather_than_empty() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served);
    assert_eq!(lines[0], "Gateway API is not served by this cluster");
    let text = lines.join("\n");
    assert!(text.contains("GatewayClass"), "{text}");
    assert!(text.contains("ListenerSet"), "{text}");
    assert!(text.contains("TCPRoute"), "{text}");
    assert!(text.contains("UDPRoute"), "{text}");
    assert!(
        text.contains("not Istio Gateway"),
        "the document refuses the mesh kind: {text}"
    );
}

#[test]
fn an_empty_served_cluster_renders_as_stored_nothing_not_absent() {
    let lines = render(&empty_served());
    assert_eq!(
        lines[0],
        "no Gateway API objects are stored in this cluster"
    );
    assert!(
        !lines.join("\n").contains("not served"),
        "zero objects on a served group is not absence"
    );
}

#[test]
fn a_history_renders_class_conditions_and_backends() {
    let gateway = resource_from(Kind::Gateway, gateway_json());
    let route = resource_from(Kind::HTTPRoute, httproute_json());
    let lines = render(&Inventory {
        served: true,
        gateways: KindSet::Served {
            items: vec![gateway],
            truncated: true,
            unreadable: 1,
        },
        http_routes: KindSet::Served {
            items: vec![route],
            truncated: false,
            unreadable: 0,
        },
        grpc_routes: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.starts_with("2 Gateway API objects"), "{text}");
    assert!(text.contains("prod/web"), "{text}");
    assert!(
        text.contains("Gateway  traefik  Accepted=True Programmed=True  10.0.0.8"),
        "{text}"
    );
    assert!(text.contains("app-svc"), "{text}");
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
    assert!(
        text.contains("gateway grpcroutes: access denied for this account"),
        "a 403 is a labelled denial, not an absent kind: {text}"
    );
}

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    content_type: String,
    body: String,
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
        let content_type = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let (at, answer) = {
            let mut state = self.state.lock().expect("script lock");
            let at = state.seen.len();
            state.seen.push(Seen {
                method: method.clone(),
                path: path.clone(),
                content_type,
                body: String::new(),
            });
            let routable = path.replacen("?&", "?", 1);
            let hit = state.routes.iter_mut().find(|route| {
                !route.used && route.method == method && routable.starts_with(&route.matches)
            });
            let answer = match hit {
                Some(route) => {
                    route.used = true;
                    Some((route.status, route.body.clone()))
                }
                None => Some((
                    404,
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"unscripted"}"#
                        .to_string(),
                )),
            };
            (at, answer)
        };
        let shared = self.state.clone();
        let body = req.into_body();
        Box::pin(async move {
            let read = match http_body_util::BodyExt::collect(body).await {
                Ok(collected) => String::from_utf8_lossy(&collected.to_bytes()).to_string(),
                Err(_) => String::new(),
            };
            if let Some(seen) = shared.lock().expect("script lock").seen.get_mut(at) {
                seen.body = read;
            }
            let (status, response) = answer.expect("every scripted call answers");
            Ok(http::Response::builder()
                .status(status)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(response.into_bytes()))
                .expect("a response"))
        })
    }
}

const STATUS_403: &str = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"forbidden"}"#;

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
        "metadata": {},
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
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/gatewayclasses?",
        200,
        empty_list("GatewayClass"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/gateways?",
        200,
        empty_list("Gateway"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/httproutes?",
        200,
        empty_list("HTTPRoute"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/grpcroutes?",
        200,
        empty_list("GRPCRoute"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/tlsroutes?",
        200,
        empty_list("TLSRoute"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/tcproutes?",
        200,
        empty_list("TCPRoute"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/udproutes?",
        200,
        empty_list("UDPRoute"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/referencegrants?",
        200,
        empty_list("ReferenceGrant"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/backendtlspolicies?",
        200,
        empty_list("BackendTLSPolicy"),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/listenersets?",
        200,
        empty_list("ListenerSet"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_on_the_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let fetched = fetch(&script.client(), None).await;
    let Fetched::Ok(inventory) = fetched else {
        panic!("a missing group is not a failure: {fetched:?}");
    };
    assert!(!inventory.served);
    assert!(table_page(&inventory).is_none());
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("gateways")),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| seen.method == "GET" && seen.content_type.is_empty()),
        "a Gateway API fetch only reads, and a read carries no body: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_the_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/gateway.networking.k8s.io", 403, STATUS_403);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied on that kind, not a whole-fetch failure");
    };
    assert!(inventory.served);
    assert!(matches!(inventory.gateways, KindSet::Denied));
    assert!(table_page(&inventory).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_empty_group_is_a_table_and_does_not_touch_istio() {
    let script = Script::default();
    script_empty_served(&script);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served empty listing must resolve");
    };
    assert!(
        inventory.served,
        "this k3s: the group is served with zero objects"
    );
    let page = table_page(&inventory).expect("empty served is Some, not Absent");
    assert!(page.rows.is_empty(), "zero objects: {page:?}");
    assert!(matches!(
        inventory.gateways,
        KindSet::Served { ref items, .. } if items.is_empty()
    ));
    let seen = script.seen();
    assert!(
        seen.iter().any(|item| item
            .path
            .starts_with("/apis/gateway.networking.k8s.io/v1/gatewayclasses")),
        "GatewayClass is cluster-scoped: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|item| !item.path.contains("networking.istio.io")),
        "Istio Gateway is mesh.rs: {seen:?}"
    );
    assert!(
        seen.iter().any(|item| item
            .path
            .starts_with("/apis/gateway.networking.k8s.io/v1/listenersets")),
        "ListenerSet is Standard since Gateway API 1.5: {seen:?}"
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_extracts_inventory_fields_and_follows_a_continue_token() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io",
        200,
        gateway_group(),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/gatewayclasses?",
        200,
        serde_json::json!({
            "kind": "GatewayClassList",
            "items": [gatewayclass_json()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/gateways?",
        200,
        serde_json::json!({
            "kind": "GatewayList",
            "metadata": { "continue": "page-2" },
            "items": [gateway_json()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/gateway.networking.k8s.io/v1/gateways?",
        200,
        serde_json::json!({
            "kind": "GatewayList",
            "items": [{
                "metadata": { "name": "edge", "namespace": "prod" },
                "spec": { "gatewayClassName": "traefik" }
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
            "items": [httproute_json()]
        })
        .to_string(),
    );

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    assert!(inventory.served);
    assert_eq!(
        inventory.gateway_classes.items()[0].class,
        "traefik.io/gateway-controller"
    );
    let gateways = inventory.gateways.items();
    assert_eq!(
        gateways
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["web", "edge"]
    );
    assert_eq!(gateways[0].addresses, "10.0.0.8");
    assert_eq!(inventory.http_routes.items()[0].backends, "app-svc");

    let lists: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("/gateways"))
        .collect();
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
}
