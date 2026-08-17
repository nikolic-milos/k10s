//! Field extraction, caps and paging, the empty served table, 404/403, the
//! rendered words, and the middleware secret drop. A cluster is not required.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

fn ingressroute_json() -> Value {
    json!({
        "apiVersion": "traefik.io/v1alpha1",
        "kind": "IngressRoute",
        "metadata": {
            "name": "hplane-dns",
            "namespace": "k10s-hplane",
            "uid": "42e2e2bf-23a4-4a43-88d2-adfc1beb0f0d"
        },
        "spec": {
            "entryPoints": ["web"],
            "routes": [{
                "kind": "Rule",
                "match": "Host(`hplane.k10s.lab`) && PathPrefix(`/dns`)",
                "middlewares": [{ "name": "strip" }],
                "services": [{
                    "name": "kube-dns",
                    "namespace": "kube-system",
                    "port": 9153
                }]
            }],
            "tls": { "secretName": "hplane-tls" }
        }
    })
}

fn middleware_with_planted_password() -> Value {
    json!({
        "metadata": { "name": "auth", "namespace": "prod", "uid": "uid-mw" },
        "spec": {
            "basicAuth": {
                "users": ["admin:$apr1$SUPERSECRET-PLANTED-htpasswd"],
                "secret": "htpasswd-secret"
            },
            "headers": {
                "customRequestHeaders": {
                    "Authorization": "Bearer SUPERSECRET-PLANTED-htpasswd"
                }
            },
            "plugin": {
                "dummy": { "token": "SUPERSECRET-PLANTED-htpasswd" }
            }
        }
    })
}

fn resource_from(kind: Kind, value: Value) -> Resource {
    parse_item(kind, value).expect("the fixture is a Traefik object")
}

fn served(items: Vec<Resource>) -> KindSet {
    KindSet::Served {
        items,
        truncated: false,
        unreadable: 0,
    }
}

fn inventory_with(kind: Kind, items: Vec<Resource>) -> Inventory {
    let mut inventory = Inventory {
        group: GroupState::Served,
        ..Inventory::default()
    };
    match kind {
        Kind::IngressRoute => inventory.ingress_routes = served(items),
        Kind::IngressRouteTCP => inventory.ingress_routes_tcp = served(items),
        Kind::IngressRouteUDP => inventory.ingress_routes_udp = served(items),
        Kind::Middleware => inventory.middlewares = served(items),
        Kind::MiddlewareTCP => inventory.middlewares_tcp = served(items),
        Kind::ServersTransport => inventory.servers_transports = served(items),
        Kind::ServersTransportTCP => inventory.servers_transports_tcp = served(items),
        Kind::TLSOption => inventory.tls_options = served(items),
        Kind::TLSStore => inventory.tls_stores = served(items),
        Kind::TraefikService => inventory.traefik_services = served(items),
    }
    inventory
}

fn leak_needles() -> [&'static str; 3] {
    [
        "SUPERSECRET-PLANTED-htpasswd",
        "admin:$apr1",
        "Bearer SUPERSECRET",
    ]
}

fn assert_no_planted_secret(text: &str) {
    for needle in leak_needles() {
        assert!(
            !text.contains(needle),
            "a planted middleware secret must not appear: {text}"
        );
    }
}

#[test]
fn an_ingressroute_keeps_host_pathprefix_entrypoints_and_backend_port() {
    let resource = resource_from(Kind::IngressRoute, ingressroute_json());
    assert_eq!(resource.name, "hplane-dns");
    assert_eq!(resource.namespace, "k10s-hplane");
    assert_eq!(resource.uid, "42e2e2bf-23a4-4a43-88d2-adfc1beb0f0d");
    assert_eq!(resource.entrypoints, vec!["web"]);
    assert_eq!(
        resource.routes,
        vec!["Host(`hplane.k10s.lab`) && PathPrefix(`/dns`)"]
    );
    assert_eq!(
        resource.services,
        vec![Backend {
            name: "kube-dns".into(),
            namespace: "kube-system".into(),
            port: "9153".into(),
        }]
    );
    assert_eq!(resource.middlewares, vec!["strip"]);
    assert_eq!(resource.tls_secret, "hplane-tls");
}

#[test]
fn a_named_service_port_is_kept_as_text() {
    let mut value = ingressroute_json();
    value["spec"]["routes"][0]["services"][0]["port"] = json!("metrics");
    let resource = resource_from(Kind::IngressRoute, value);
    assert_eq!(resource.services[0].port, "metrics");
}

#[test]
fn a_middleware_keeps_type_keys_and_drops_auth_users() {
    let resource = resource_from(Kind::Middleware, middleware_with_planted_password());
    assert_eq!(resource.name, "auth");
    assert!(resource.routes.iter().any(|item| item == "basicAuth"));
    assert!(resource.tls_secret.is_empty());
    assert!(resource.middlewares.is_empty());
    assert_no_planted_secret(&format!("{resource:?}"));
}

#[test]
fn a_planted_middleware_password_does_not_appear_in_debug_or_table_cells() {
    let resource = resource_from(Kind::Middleware, middleware_with_planted_password());
    let inventory = inventory_with(Kind::Middleware, vec![resource]);
    let page = table_page(&inventory).expect("a served middleware is a table");
    let cells = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert_no_planted_secret(&cells);
    assert_no_planted_secret(&format!("{inventory:?}"));
    assert_no_planted_secret(&render(&inventory).join("\n"));
    assert!(
        cells.contains("basicAuth"),
        "the type key stays, the users do not: {cells}"
    );
}

#[test]
fn a_tlsstore_keeps_the_certificate_secret_name_only() {
    let resource = resource_from(
        Kind::TLSStore,
        json!({
            "metadata": { "name": "default", "namespace": "kube-system" },
            "spec": { "defaultCertificate": { "secretName": "default-cert" } }
        }),
    );
    assert_eq!(resource.tls_secret, "default-cert");
}

#[test]
fn a_serverstransporttcp_reads_its_tls_secrets_under_spec_tls() {
    let resource = resource_from(
        Kind::ServersTransportTCP,
        json!({
            "metadata": { "name": "mtls", "namespace": "prod" },
            "spec": { "tls": { "serverName": "example.org", "certificatesSecrets": ["supersecret"] } }
        }),
    );
    assert_eq!(resource.tls_secret, "supersecret");
    let resource = resource_from(
        Kind::ServersTransportTCP,
        json!({
            "metadata": { "name": "verify", "namespace": "prod" },
            "spec": { "tls": { "rootCAsSecrets": ["ca-secret"] } }
        }),
    );
    assert_eq!(resource.tls_secret, "ca-secret");
    let resource = resource_from(
        Kind::ServersTransportTCP,
        json!({
            "metadata": { "name": "modern", "namespace": "prod" },
            "spec": { "tls": { "rootCAs": [{ "configMap": "ca-cm" }, { "secret": "ca-secret" }] } }
        }),
    );
    assert_eq!(
        resource.tls_secret, "ca-secret",
        "a ConfigMap ref is not a Secret name"
    );
}

#[test]
fn a_serverstransport_falls_back_to_the_modern_rootcas_secret_ref() {
    let resource = resource_from(
        Kind::ServersTransport,
        json!({
            "metadata": { "name": "mtls", "namespace": "prod" },
            "spec": { "certificatesSecrets": ["supersecret"] }
        }),
    );
    assert_eq!(resource.tls_secret, "supersecret");
    let resource = resource_from(
        Kind::ServersTransport,
        json!({
            "metadata": { "name": "modern", "namespace": "prod" },
            "spec": { "rootCAs": [{ "configMap": "ca-cm" }, { "secret": "ca-secret" }] }
        }),
    );
    assert_eq!(
        resource.tls_secret, "ca-secret",
        "a ConfigMap ref is not a Secret name"
    );
}

#[test]
fn a_tlsoption_keeps_its_clientauth_secret_name() {
    let resource = resource_from(
        Kind::TLSOption,
        json!({
            "metadata": { "name": "mtls", "namespace": "prod" },
            "spec": { "clientAuth": { "secretNames": ["secret-ca1"], "clientAuthType": "RequireAndVerifyClientCert" } }
        }),
    );
    assert_eq!(resource.tls_secret, "secret-ca1");
}

#[test]
fn a_cross_namespace_middleware_ref_keeps_its_namespace_and_is_not_collapsed() {
    let mut value = ingressroute_json();
    value["spec"]["routes"][0]["middlewares"] = json!([
        { "name": "strip", "namespace": "infra" },
        { "name": "strip" }
    ]);
    let resource = resource_from(Kind::IngressRoute, value);
    assert_eq!(
        resource.middlewares,
        vec!["infra/strip", "strip"],
        "two Middleware objects stay two references"
    );
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::IngressRoute, json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = json!({
        "metadata": { "name": huge, "namespace": "k10s-hplane" },
        "spec": {
            "entryPoints": [huge],
            "routes": [{ "match": huge, "services": [{ "name": huge, "port": huge }] }],
            "tls": { "secretName": huge }
        }
    });
    let resource = resource_from(Kind::IngressRoute, value);
    for field in [
        &resource.name,
        &resource.entrypoints[0],
        &resource.routes[0],
        &resource.services[0].name,
        &resource.services[0].port,
        &resource.tls_secret,
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
        "a 403 is Denied, never an empty inventory that looks like Traefik is absent"
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

#[test]
fn an_unserved_traefik_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "a 404 group is absence, not an empty list"
    );
}

#[test]
fn a_served_group_with_zero_rows_is_still_a_table() {
    let page = table_page(&Inventory {
        group: GroupState::Served,
        ingress_routes: served(Vec::new()),
        ..Inventory::default()
    })
    .expect("served plus zero rows is tonight's empty-CRD shape");
    assert!(page.rows.is_empty(), "empty means no objects, not Absent");
    assert_eq!(page.columns[0].name, "Kind");
}

#[test]
fn a_denied_group_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory::denied()).expect("Denied is visible");
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
    assert!(text.contains(GROUP), "{text}");
}

#[test]
fn a_default_traefik_ingressclass_is_noted_without_an_ingress() {
    let class = read_default_class(&json!({
        "metadata": {
            "name": "traefik",
            "annotations": { "ingressclass.kubernetes.io/is-default-class": "true" }
        },
        "spec": { "controller": "traefik.io/ingress-controller" }
    }))
    .expect("the live default class is Traefik");
    assert_eq!(class.name, "traefik");
    assert_eq!(class.controller, INGRESS_CONTROLLER);
    assert!(
        read_default_class(&json!({
            "metadata": {
                "name": "nginx",
                "annotations": { "ingressclass.kubernetes.io/is-default-class": "true" }
            },
            "spec": { "controller": "k8s.io/ingress-nginx" }
        }))
        .is_none(),
        "a default class that is not Traefik is not noted"
    );
}

#[test]
fn a_served_ingressroute_is_one_table_row() {
    let resource = resource_from(Kind::IngressRoute, ingressroute_json());
    let page = table_page(&inventory_with(Kind::IngressRoute, vec![resource]))
        .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "hplane-dns");
    assert_eq!(page.rows[0].cells[0], "IngressRoute");
    assert_eq!(page.rows[0].cells[3], "web");
    assert!(page.rows[0].cells[4].contains("Host(`hplane.k10s.lab`)"));
    assert!(page.rows[0].cells[4].contains("PathPrefix(`/dns`)"));
    assert_eq!(page.rows[0].cells[5], "kube-system/kube-dns:9153");
    assert_eq!(page.rows[0].cells[6], "strip");
    assert_eq!(page.rows[0].cells[7], "hplane-tls");
}

#[test]
fn an_unserved_render_names_what_it_would_have_read() {
    let text = render(&Inventory::default()).join("\n");
    assert!(
        text.contains("Traefik is not served by this cluster"),
        "{text}"
    );
    assert!(
        text.contains("IngressRoute") && text.contains("traefik.io"),
        "the empty answer names what was looked for and why it could be wrong: {text}"
    );
}

#[test]
fn a_denied_render_stays_labelled_and_keeps_the_ingressclass_fact() {
    let text = render(&Inventory::denied()).join("\n");
    assert!(
        text.contains("traefik.io: access denied for this account"),
        "{text}"
    );
    let mut inventory = Inventory::denied();
    inventory.default_ingress_class = Some(DefaultIngressClass {
        name: "traefik".into(),
        controller: INGRESS_CONTROLLER.into(),
    });
    let text = render(&inventory).join("\n");
    assert!(
        text.contains("default IngressClass is traefik"),
        "the one fact a denied account still learned is not discarded: {text}"
    );
}

#[test]
fn a_mixed_render_counts_and_names_denials_truncation_and_decode_failures() {
    let resource = resource_from(Kind::IngressRoute, ingressroute_json());
    let inventory = Inventory {
        group: GroupState::Served,
        ingress_routes: KindSet::Served {
            items: vec![resource],
            truncated: true,
            unreadable: 1,
        },
        middlewares: KindSet::Denied,
        default_ingress_class: Some(DefaultIngressClass {
            name: "traefik".into(),
            controller: INGRESS_CONTROLLER.into(),
        }),
        ..Inventory::default()
    };
    let page = table_page(&inventory).expect("a served group is a table");
    assert!(page.truncated, "a capped kind marks the whole page");
    let text = render(&inventory).join("\n");
    assert!(text.starts_with("1 Traefik routing object"), "{text}");
    assert!(
        text.contains("traefik middlewares: access denied for this account"),
        "{text}"
    );
    assert!(text.contains("stopped at"), "{text}");
    assert!(text.contains("could not be decoded"), "{text}");
    assert!(
        text.contains("default IngressClass is traefik (traefik.io/ingress-controller)"),
        "{text}"
    );
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
                    Some((route.status, route.body.clone()))
                }
                None => Some((
                    404,
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"unscripted"}"#
                        .to_string(),
                )),
            }
        };
        Box::pin(async move {
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

fn traefik_group() -> String {
    r#"{"kind":"APIGroup","name":"traefik.io",
        "versions":[{"groupVersion":"traefik.io/v1alpha1","version":"v1alpha1"}],
        "preferredVersion":{"groupVersion":"traefik.io/v1alpha1","version":"v1alpha1"}}"#
        .to_string()
}

fn empty_list(kind: &str) -> String {
    format!(r#"{{"kind":"{kind}List","apiVersion":"traefik.io/v1alpha1","items":[]}}"#)
}

fn script_empty_kinds(script: &Script) {
    for kind in Kind::ALL {
        script.route(
            "GET",
            &format!("/apis/traefik.io/v1alpha1/{}?", kind.plural()),
            200,
            empty_list(kind.as_str()),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_on_the_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let fetched = fetch(&script.client(), None).await;
    let Fetched::Ok(inventory) = fetched else {
        panic!("a missing group is not a failure: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.group, GroupState::NotServed));
    assert!(table_page(&inventory).is_none());
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("ingressroutes")),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script.seen().iter().all(|seen| seen.method == "GET"),
        "a Traefik fetch only reads: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_the_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 403, STATUS_403);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied, not a whole-fetch failure");
    };
    assert!(matches!(inventory.group, GroupState::Denied));
    assert!(inventory.served(), "403 is Denied, not served: false");
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("ingressroutes")),
        "a 403 group must not be chased into a list: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_group_with_empty_lists_is_an_empty_table() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    script_empty_kinds(&script);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("empty served lists must resolve");
    };
    assert!(inventory.group.is_served());
    assert!(inventory.ingress_routes.served());
    assert!(inventory.ingress_routes.items().is_empty());
    let page = table_page(&inventory).expect("served plus zero rows is a table");
    assert!(page.rows.is_empty());
    let lists: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("/apis/traefik.io/v1alpha1/"))
        .collect();
    assert_eq!(lists.len(), Kind::ALL.len(), "{lists:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_extracts_the_live_ingressroute_shape() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    script.route(
        "GET",
        "/apis/traefik.io/v1alpha1/ingressroutes?",
        200,
        json!({
            "kind": "IngressRouteList",
            "items": [ingressroute_json()]
        })
        .to_string(),
    );
    for kind in Kind::ALL.iter().filter(|kind| **kind != Kind::IngressRoute) {
        script.route(
            "GET",
            &format!("/apis/traefik.io/v1alpha1/{}?", kind.plural()),
            200,
            empty_list(kind.as_str()),
        );
    }
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        200,
        json!({
            "kind": "IngressClassList",
            "items": [{
                "metadata": {
                    "name": "traefik",
                    "annotations": { "ingressclass.kubernetes.io/is-default-class": "true" }
                },
                "spec": { "controller": "traefik.io/ingress-controller" }
            }]
        })
        .to_string(),
    );

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    let item = &inventory.ingress_routes.items()[0];
    assert_eq!(item.name, "hplane-dns");
    assert_eq!(
        item.routes,
        vec!["Host(`hplane.k10s.lab`) && PathPrefix(`/dns`)"]
    );
    assert_eq!(item.services[0].port, "9153");
    let class = inventory
        .default_ingress_class
        .expect("the default class is Traefik");
    assert_eq!(class.name, "traefik");
    assert!(
        script.seen().iter().any(|seen| seen
            .path
            .starts_with("/apis/traefik.io/v1alpha1/ingressroutes?")),
        "the list uses the live group/version/plural: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_follows_a_continue_token_across_pages() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    script.route(
        "GET",
        "/apis/traefik.io/v1alpha1/ingressroutes?",
        200,
        json!({
            "kind": "IngressRouteList",
            "metadata": { "continue": "page-2" },
            "items": [ingressroute_json()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/traefik.io/v1alpha1/ingressroutes?",
        200,
        json!({
            "kind": "IngressRouteList",
            "items": [{
                "metadata": { "name": "tail", "namespace": "k10s-hplane" },
                "spec": {}
            }]
        })
        .to_string(),
    );
    for kind in Kind::ALL.iter().filter(|kind| **kind != Kind::IngressRoute) {
        script.route(
            "GET",
            &format!("/apis/traefik.io/v1alpha1/{}?", kind.plural()),
            200,
            empty_list(kind.as_str()),
        );
    }

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a paged listing must resolve");
    };
    assert_eq!(
        inventory
            .ingress_routes
            .items()
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["hplane-dns", "tail"]
    );
    let lists: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("/ingressroutes?"))
        .collect();
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_stops_at_the_object_cap_and_does_not_chase_the_token() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    let items: Vec<Value> = (0..=MAX_OBJECTS)
        .map(|n| {
            json!({
                "metadata": { "name": format!("route-{n}"), "namespace": "prod" },
                "spec": {}
            })
        })
        .collect();
    script.route(
        "GET",
        "/apis/traefik.io/v1alpha1/ingressroutes?",
        200,
        json!({
            "kind": "IngressRouteList",
            "metadata": { "continue": "page-2" },
            "items": items
        })
        .to_string(),
    );
    for kind in Kind::ALL.iter().filter(|kind| **kind != Kind::IngressRoute) {
        script.route(
            "GET",
            &format!("/apis/traefik.io/v1alpha1/{}?", kind.plural()),
            200,
            empty_list(kind.as_str()),
        );
    }

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a capped listing must resolve");
    };
    let KindSet::Served {
        items, truncated, ..
    } = &inventory.ingress_routes
    else {
        panic!("a capped list is still Served: {inventory:?}");
    };
    assert_eq!(items.len(), MAX_OBJECTS);
    assert!(*truncated, "the cap is said, not silent");
    let lists: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("/ingressroutes?"))
        .collect();
    assert_eq!(
        lists.len(),
        1,
        "the continue token must not be chased past the cap: {lists:?}"
    );
    let page = table_page(&inventory).expect("a served group is a table");
    assert!(page.truncated);
    assert!(render(&inventory).join("\n").contains("stopped at"));
}
