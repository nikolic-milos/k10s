//! Traefik routing CRs already served on `traefik.io`.
//!
//! A 404 on the group is [`GroupState::NotServed`]: invisible, not broken. A
//! 403 is [`GroupState::Denied`]. Those stay distinct on purpose; collapsing
//! them would say Traefik is absent when the account was refused. When the
//! group is served, [`table_page`] is `Some` even with zero rows: that is a
//! cluster that has the CRDs and no routes, not absence.
//!
//! Inventory fields are names, matchers, and backends. Middleware specs can
//! hold basicAuth users and auth secret names; those are dropped at parse, so
//! [`Resource`] has nowhere to put them. TLS is a Secret *name* only. Hub
//! (`hub.traefik.io`) is not inventoried: that is API management, and this
//! module does not rebuild it. The dashboard API is not fetched; the CRs are
//! the source.
//!
//! If a default IngressClass uses Traefik's controller, that name is noted on
//! the inventory. An Ingress object is not required.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;
use crate::served::{GroupAnswer, ListErr, after_group, after_list, order_versions};

pub const GROUP: &str = "traefik.io";
pub const VERSION: &str = "v1alpha1";
pub const INGRESS_CONTROLLER: &str = "traefik.io/ingress-controller";
pub const DEFAULT_CLASS_ANNOTATION: &str = "ingressclass.kubernetes.io/is-default-class";

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;
const MAX_LIST_ITEMS: usize = 32;

const INGRESS_CLASSES: &str = "/apis/networking.k8s.io/v1/ingressclasses";

/// The ten `traefik.io` routing kinds this inventory reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    IngressRoute,
    IngressRouteTCP,
    IngressRouteUDP,
    Middleware,
    MiddlewareTCP,
    ServersTransport,
    ServersTransportTCP,
    TLSOption,
    TLSStore,
    TraefikService,
}

impl Kind {
    pub const ALL: &'static [Kind] = &[
        Kind::IngressRoute,
        Kind::IngressRouteTCP,
        Kind::IngressRouteUDP,
        Kind::Middleware,
        Kind::MiddlewareTCP,
        Kind::ServersTransport,
        Kind::ServersTransportTCP,
        Kind::TLSOption,
        Kind::TLSStore,
        Kind::TraefikService,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::IngressRoute => "IngressRoute",
            Kind::IngressRouteTCP => "IngressRouteTCP",
            Kind::IngressRouteUDP => "IngressRouteUDP",
            Kind::Middleware => "Middleware",
            Kind::MiddlewareTCP => "MiddlewareTCP",
            Kind::ServersTransport => "ServersTransport",
            Kind::ServersTransportTCP => "ServersTransportTCP",
            Kind::TLSOption => "TLSOption",
            Kind::TLSStore => "TLSStore",
            Kind::TraefikService => "TraefikService",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::IngressRoute => "ingressroutes",
            Kind::IngressRouteTCP => "ingressroutetcps",
            Kind::IngressRouteUDP => "ingressrouteudps",
            Kind::Middleware => "middlewares",
            Kind::MiddlewareTCP => "middlewaretcps",
            Kind::ServersTransport => "serverstransports",
            Kind::ServersTransportTCP => "serverstransporttcps",
            Kind::TLSOption => "tlsoptions",
            Kind::TLSStore => "tlsstores",
            Kind::TraefikService => "traefikservices",
        }
    }

    pub fn version(self) -> &'static str {
        VERSION
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::IngressRoute => "traefik ingressroutes",
            Kind::IngressRouteTCP => "traefik ingressroutetcps",
            Kind::IngressRouteUDP => "traefik ingressrouteudps",
            Kind::Middleware => "traefik middlewares",
            Kind::MiddlewareTCP => "traefik middlewaretcps",
            Kind::ServersTransport => "traefik serverstransports",
            Kind::ServersTransportTCP => "traefik serverstransporttcps",
            Kind::TLSOption => "traefik tlsoptions",
            Kind::TLSStore => "traefik tlsstores",
            Kind::TraefikService => "traefik traefikservices",
        }
    }
}

/// One upstream Service or TraefikService. Port is text because the CRD is
/// an int-or-string (a number or a named port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    pub name: String,
    pub namespace: String,
    pub port: String,
}

/// One CR, reduced to what an inventory shows.
///
/// Middleware auth users and auth secret names are not fields here. A planted
/// password therefore cannot appear in Debug or in a table cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub entrypoints: Vec<String>,
    /// Route matchers, or for Middleware the spec type keys only.
    pub routes: Vec<String>,
    pub services: Vec<Backend>,
    /// Middleware *names* referenced by a route. Never a spec body.
    pub middlewares: Vec<String>,
    /// TLS Secret name only. Never data.
    pub tls_secret: String,
}

/// What one kind's list answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KindSet {
    Served {
        items: Vec<Resource>,
        truncated: bool,
        unreadable: usize,
    },
    #[default]
    NotServed,
    Denied,
}

impl KindSet {
    /// False when this kind's group (or list) answered 404.
    pub fn served(&self) -> bool {
        !matches!(self, KindSet::NotServed)
    }

    pub fn items(&self) -> &[Resource] {
        match self {
            KindSet::Served { items, .. } => items,
            KindSet::NotServed | KindSet::Denied => &[],
        }
    }
}

/// 404 on `/apis/traefik.io` is [`GroupState::NotServed`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GroupState {
    Served,
    #[default]
    NotServed,
    Denied,
}

impl GroupState {
    pub fn is_served(&self) -> bool {
        matches!(self, GroupState::Served)
    }
}

/// The cluster's default IngressClass, when that class is Traefik's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultIngressClass {
    pub name: String,
    pub controller: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub group: GroupState,
    pub ingress_routes: KindSet,
    pub ingress_routes_tcp: KindSet,
    pub ingress_routes_udp: KindSet,
    pub middlewares: KindSet,
    pub middlewares_tcp: KindSet,
    pub servers_transports: KindSet,
    pub servers_transports_tcp: KindSet,
    pub tls_options: KindSet,
    pub tls_stores: KindSet,
    pub traefik_services: KindSet,
    pub default_ingress_class: Option<DefaultIngressClass>,
}

impl Inventory {
    /// False only when the group answered 404. Denied is visible.
    pub fn served(&self) -> bool {
        !matches!(self.group, GroupState::NotServed)
    }

    pub fn sets(&self) -> [(&KindSet, Kind); 10] {
        [
            (&self.ingress_routes, Kind::IngressRoute),
            (&self.ingress_routes_tcp, Kind::IngressRouteTCP),
            (&self.ingress_routes_udp, Kind::IngressRouteUDP),
            (&self.middlewares, Kind::Middleware),
            (&self.middlewares_tcp, Kind::MiddlewareTCP),
            (&self.servers_transports, Kind::ServersTransport),
            (&self.servers_transports_tcp, Kind::ServersTransportTCP),
            (&self.tls_options, Kind::TLSOption),
            (&self.tls_stores, Kind::TLSStore),
            (&self.traefik_services, Kind::TraefikService),
        ]
    }

    fn denied() -> Inventory {
        Inventory {
            group: GroupState::Denied,
            ingress_routes: KindSet::Denied,
            ingress_routes_tcp: KindSet::Denied,
            ingress_routes_udp: KindSet::Denied,
            middlewares: KindSet::Denied,
            middlewares_tcp: KindSet::Denied,
            servers_transports: KindSet::Denied,
            servers_transports_tcp: KindSet::Denied,
            tls_options: KindSet::Denied,
            tls_stores: KindSet::Denied,
            traefik_services: KindSet::Denied,
            default_ingress_class: None,
        }
    }
}

#[derive(Deserialize, Default)]
struct WireGroup {
    #[serde(default)]
    versions: Vec<WireGroupVersion>,
    #[serde(default, rename = "preferredVersion")]
    preferred: WireGroupVersion,
}

#[derive(Deserialize, Default)]
struct WireGroupVersion {
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
struct WireList {
    #[serde(default)]
    metadata: WireListMeta,
    #[serde(default)]
    items: Vec<Value>,
}

#[derive(Deserialize, Default)]
struct WireListMeta {
    #[serde(default, rename = "continue")]
    cont: String,
}

#[derive(Deserialize, Default)]
struct WireObject {
    #[serde(default)]
    metadata: WireMeta,
    #[serde(default)]
    spec: Value,
}

#[derive(Deserialize, Default)]
struct WireMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    uid: String,
}

fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn port_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .map(|item| clipped(item.to_string()))
                .take(MAX_LIST_ITEMS)
                .collect()
        })
        .unwrap_or_default()
}

fn push_backend(out: &mut Vec<Backend>, value: &Value) {
    let Some(name) = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        return;
    };
    if out.len() >= MAX_LIST_ITEMS {
        return;
    }
    out.push(Backend {
        name: clipped(name.to_string()),
        namespace: clipped(
            value
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        port: clipped(port_text(value.get("port"))),
    });
}

fn backends_of(kind: Kind, spec: &Value) -> Vec<Backend> {
    let mut out = Vec::new();
    match kind {
        Kind::IngressRoute | Kind::IngressRouteTCP | Kind::IngressRouteUDP => {
            let Some(routes) = spec.get("routes").and_then(Value::as_array) else {
                return out;
            };
            for route in routes {
                let Some(services) = route.get("services").and_then(Value::as_array) else {
                    continue;
                };
                for service in services {
                    push_backend(&mut out, service);
                }
            }
        }
        Kind::TraefikService => {
            for path in ["/weighted/services", "/highestRandomWeight/services"] {
                if let Some(services) = spec.pointer(path).and_then(Value::as_array) {
                    for service in services {
                        push_backend(&mut out, service);
                    }
                }
            }
            if let Some(mirroring) = spec.get("mirroring") {
                push_backend(&mut out, mirroring);
                if let Some(mirrors) = mirroring.get("mirrors").and_then(Value::as_array) {
                    for service in mirrors {
                        push_backend(&mut out, service);
                    }
                }
            }
            if let Some(failover) = spec.get("failover") {
                if let Some(service) = failover.get("service") {
                    push_backend(&mut out, service);
                }
                if let Some(fallback) = failover.get("fallback") {
                    push_backend(&mut out, fallback);
                }
            }
        }
        Kind::Middleware
        | Kind::MiddlewareTCP
        | Kind::ServersTransport
        | Kind::ServersTransportTCP
        | Kind::TLSOption
        | Kind::TLSStore => {}
    }
    out
}

fn middleware_type_names(spec: &Value) -> Vec<String> {
    let Some(object) = spec.as_object() else {
        return Vec::new();
    };
    object
        .keys()
        .filter(|key| !key.is_empty())
        .map(|key| clipped(key.to_string()))
        .take(MAX_LIST_ITEMS)
        .collect()
}

fn matchers_of(kind: Kind, spec: &Value) -> Vec<String> {
    match kind {
        Kind::Middleware | Kind::MiddlewareTCP => middleware_type_names(spec),
        Kind::IngressRoute | Kind::IngressRouteTCP => spec
            .get("routes")
            .and_then(Value::as_array)
            .map(|routes| {
                routes
                    .iter()
                    .filter_map(|route| route.get("match").and_then(Value::as_str))
                    .filter(|item| !item.is_empty())
                    .map(|item| clipped(item.to_string()))
                    .take(MAX_LIST_ITEMS)
                    .collect()
            })
            .unwrap_or_default(),
        Kind::IngressRouteUDP
        | Kind::ServersTransport
        | Kind::ServersTransportTCP
        | Kind::TLSOption
        | Kind::TLSStore
        | Kind::TraefikService => Vec::new(),
    }
}

fn push_middleware_name(out: &mut Vec<String>, value: &Value) {
    // A MiddlewareRef carries a namespace; `strip` and `infra/strip` are two
    // different Middleware objects and must not collapse into one cell.
    let (name, namespace) = match value {
        Value::String(name) => (name.as_str(), ""),
        other => (
            other.get("name").and_then(Value::as_str).unwrap_or(""),
            other.get("namespace").and_then(Value::as_str).unwrap_or(""),
        ),
    };
    if name.is_empty() || out.len() >= MAX_LIST_ITEMS {
        return;
    }
    let label = if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}/{name}")
    };
    if !out.iter().any(|have| have == &label) {
        out.push(clipped(label));
    }
}

fn middleware_refs_of(kind: Kind, spec: &Value) -> Vec<String> {
    match kind {
        Kind::IngressRoute | Kind::IngressRouteTCP => {
            let mut out = Vec::new();
            let Some(routes) = spec.get("routes").and_then(Value::as_array) else {
                return out;
            };
            for route in routes {
                if let Some(middlewares) = route.get("middlewares").and_then(Value::as_array) {
                    for middleware in middlewares {
                        push_middleware_name(&mut out, middleware);
                    }
                }
                if let Some(services) = route.get("services").and_then(Value::as_array) {
                    for service in services {
                        if let Some(middlewares) =
                            service.get("middlewares").and_then(Value::as_array)
                        {
                            for middleware in middlewares {
                                push_middleware_name(&mut out, middleware);
                            }
                        }
                    }
                }
            }
            out
        }
        Kind::Middleware
        | Kind::MiddlewareTCP
        | Kind::IngressRouteUDP
        | Kind::ServersTransport
        | Kind::ServersTransportTCP
        | Kind::TLSOption
        | Kind::TLSStore
        | Kind::TraefikService => Vec::new(),
    }
}

fn first_string_in_array(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|item| !item.is_empty())
        .map(str::to_string)
}

fn first_named_secret(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("secretName").and_then(Value::as_str))
        .find(|item| !item.is_empty())
        .map(str::to_string)
}

// A rootCAs entry names either a Secret or a ConfigMap; only the Secret name
// belongs in a column documented as a Secret name.
fn first_root_ca_secret(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("secret").and_then(Value::as_str))
        .find(|item| !item.is_empty())
        .map(str::to_string)
}

fn tls_secret_of(kind: Kind, spec: &Value) -> String {
    match kind {
        Kind::Middleware | Kind::MiddlewareTCP | Kind::IngressRouteUDP | Kind::TraefikService => {
            String::new()
        }
        Kind::TLSStore => spec
            .pointer("/defaultCertificate/secretName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(|name| clipped(name.to_string()))
            .or_else(|| first_named_secret(spec.get("certificates")).map(clipped))
            .unwrap_or_default(),
        Kind::ServersTransport => first_string_in_array(spec.get("certificatesSecrets"))
            .or_else(|| first_string_in_array(spec.get("rootCAsSecrets")))
            .or_else(|| first_root_ca_secret(spec.get("rootCAs")))
            .map(clipped)
            .unwrap_or_default(),
        // ServersTransportTCP nests the same options under spec.tls.
        Kind::ServersTransportTCP => {
            first_string_in_array(spec.pointer("/tls/certificatesSecrets"))
                .or_else(|| first_string_in_array(spec.pointer("/tls/rootCAsSecrets")))
                .or_else(|| first_root_ca_secret(spec.pointer("/tls/rootCAs")))
                .map(clipped)
                .unwrap_or_default()
        }
        Kind::TLSOption => first_string_in_array(spec.pointer("/clientAuth/secretNames"))
            .map(clipped)
            .unwrap_or_default(),
        Kind::IngressRoute | Kind::IngressRouteTCP => spec
            .pointer("/tls/secretName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(|name| clipped(name.to_string()))
            .unwrap_or_default(),
    }
}

fn from_wire(kind: Kind, wire: WireObject) -> Option<Resource> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    Some(Resource {
        kind,
        name: clipped(wire.metadata.name),
        namespace: clipped(wire.metadata.namespace),
        uid: clipped(wire.metadata.uid),
        entrypoints: string_list(wire.spec.get("entryPoints")),
        routes: matchers_of(kind, &wire.spec),
        services: backends_of(kind, &wire.spec),
        middlewares: middleware_refs_of(kind, &wire.spec),
        tls_secret: tls_secret_of(kind, &wire.spec),
    })
}

/// Reduce one Traefik object. Nameless JSON is not a row.
pub fn parse_item(kind: Kind, value: Value) -> Option<Resource> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_wire(kind, wire)
}

fn versions_for(kind: Kind, group_versions: &[String]) -> Vec<String> {
    let mut out = group_versions.to_vec();
    let fallback = kind.version().to_string();
    if !out.iter().any(|have| have == &fallback) {
        out.push(fallback);
    }
    out
}

fn collection_url(kind: Kind, version: &str, namespace: Option<&str>) -> String {
    let mut path = format!("/apis/{GROUP}/{version}");
    if let Some(namespace) = namespace {
        path.push_str("/namespaces/");
        path.push_str(namespace);
    }
    path.push('/');
    path.push_str(kind.plural());
    path
}

fn group_url() -> String {
    format!("/apis/{GROUP}")
}

async fn probe_group(client: &Client) -> GroupAnswer {
    let request = match http::Request::get(group_url()).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    match client.request::<WireGroup>(request).await {
        Ok(group) => {
            let versions = order_versions(&group.preferred.version, {
                group
                    .versions
                    .into_iter()
                    .map(|item| item.version)
                    .collect()
            });
            GroupAnswer::Served(versions)
        }
        Err(error) => after_group(&error),
    }
}

async fn list_at_version(
    client: &Client,
    kind: Kind,
    version: &str,
    namespace: Option<&str>,
) -> Result<KindSet, ListErr> {
    let path = collection_url(kind, version, namespace);
    let mut items = Vec::new();
    let mut unreadable = 0usize;
    let mut token: Option<String> = None;
    let mut truncated = false;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = match Request::new(path.clone()).list(&params) {
            Ok(request) => request,
            Err(error) => return Err(ListErr::Failed(error.to_string())),
        };
        let page = match client.request::<WireList>(request).await {
            Ok(page) => page,
            Err(error) if items.is_empty() && unreadable == 0 => return Err(after_list(&error)),
            Err(error) => {
                return Err(ListErr::Failed(crate::connect::describe(
                    &error as &(dyn std::error::Error + 'static),
                )));
            }
        };
        for value in page.items {
            if items.len() == MAX_OBJECTS {
                truncated = true;
                break;
            }
            match parse_item(kind, value) {
                Some(resource) => items.push(resource),
                None => unreadable += 1,
            }
        }
        token = (!page.metadata.cont.is_empty() && !truncated).then_some(page.metadata.cont);
        if token.is_none() {
            break;
        }
    }
    Ok(KindSet::Served {
        items,
        truncated,
        unreadable,
    })
}

async fn list_kind(
    client: &Client,
    kind: Kind,
    group_versions: &[String],
    namespace: Option<&str>,
) -> Result<KindSet, Fetched<Inventory>> {
    for version in versions_for(kind, group_versions) {
        match list_at_version(client, kind, &version, namespace).await {
            Ok(set) => return Ok(set),
            Err(ListErr::NotFound) => continue,
            Err(ListErr::Denied) => return Ok(KindSet::Denied),
            Err(ListErr::Failed(why)) => {
                return Err(Fetched::Failed {
                    what: kind.what(),
                    why,
                });
            }
        }
    }
    Ok(KindSet::NotServed)
}

fn read_default_class(value: &Value) -> Option<DefaultIngressClass> {
    let meta = value.get("metadata")?;
    let name = meta
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())?;
    let is_default = meta
        .get("annotations")
        .and_then(Value::as_object)
        .and_then(|annotations| annotations.get(DEFAULT_CLASS_ANNOTATION))
        .and_then(Value::as_str)
        == Some("true");
    if !is_default {
        return None;
    }
    let controller = value
        .pointer("/spec/controller")
        .and_then(Value::as_str)
        .unwrap_or("");
    if controller != INGRESS_CONTROLLER {
        return None;
    }
    Some(DefaultIngressClass {
        name: clipped(name.to_string()),
        controller: clipped(controller.to_string()),
    })
}

async fn default_traefik_class(client: &Client) -> Option<DefaultIngressClass> {
    let mut token: Option<String> = None;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = Request::new(INGRESS_CLASSES.to_string())
            .list(&params)
            .ok()?;
        let page = client.request::<WireList>(request).await.ok()?;
        for value in page.items {
            if let Some(found) = read_default_class(&value) {
                return Some(found);
            }
        }
        if page.metadata.cont.is_empty() {
            return None;
        }
        token = Some(page.metadata.cont);
    }
}

fn assemble(
    group: GroupState,
    mut sets: Vec<KindSet>,
    default_ingress_class: Option<DefaultIngressClass>,
) -> Inventory {
    let mut next = sets.drain(..);
    Inventory {
        group,
        ingress_routes: next.next().unwrap_or_default(),
        ingress_routes_tcp: next.next().unwrap_or_default(),
        ingress_routes_udp: next.next().unwrap_or_default(),
        middlewares: next.next().unwrap_or_default(),
        middlewares_tcp: next.next().unwrap_or_default(),
        servers_transports: next.next().unwrap_or_default(),
        servers_transports_tcp: next.next().unwrap_or_default(),
        tls_options: next.next().unwrap_or_default(),
        tls_stores: next.next().unwrap_or_default(),
        traefik_services: next.next().unwrap_or_default(),
        default_ingress_class,
    }
}

/// List the ten routing kinds. A missing group is invisible; a forbidden one
/// is Denied and does not hide the IngressClass join.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    match probe_group(client).await {
        GroupAnswer::NotServed => Fetched::Ok(Inventory::default()),
        GroupAnswer::Denied => {
            let mut inventory = Inventory::denied();
            inventory.default_ingress_class = default_traefik_class(client).await;
            Fetched::Ok(inventory)
        }
        GroupAnswer::Failed(why) => Fetched::Failed {
            what: "traefik",
            why,
        },
        GroupAnswer::Served(versions) => {
            let mut sets = Vec::with_capacity(Kind::ALL.len());
            for kind in Kind::ALL {
                match list_kind(client, *kind, &versions, namespace).await {
                    Ok(set) => sets.push(set),
                    Err(failed) => return failed,
                }
            }
            let default_ingress_class = default_traefik_class(client).await;
            Fetched::Ok(assemble(GroupState::Served, sets, default_ingress_class))
        }
    }
}

fn join_clipped(parts: &[String]) -> String {
    clipped(parts.join(", "))
}

fn backend_label(backend: &Backend) -> String {
    match (backend.namespace.as_str(), backend.port.as_str()) {
        ("", "") => backend.name.clone(),
        ("", port) => format!("{}:{port}", backend.name),
        (namespace, "") => format!("{namespace}/{}", backend.name),
        (namespace, port) => format!("{namespace}/{}:{port}", backend.name),
    }
}

fn denial_row(name: &str) -> TableRow {
    TableRow {
        cells: vec![
            name.to_string(),
            String::new(),
            String::new(),
            String::new(),
            "access denied for this account".to_string(),
            String::new(),
            String::new(),
            String::new(),
        ],
        name: name.to_string(),
        namespace: None,
        uid: format!("denied:{name}"),
    }
}

/// Native list rows. `None` when the group answered 404. `Some` with zero
/// rows when the group is served and nothing is stored.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served() {
        return None;
    }
    let columns = [
        "Kind",
        "Name",
        "Namespace",
        "Entrypoints",
        "Routes",
        "Services",
        "Middlewares",
        "TLS",
    ]
    .iter()
    .map(|name| TableColumn {
        name: name.to_string(),
        wide: false,
    })
    .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    if matches!(inventory.group, GroupState::Denied) {
        rows.push(denial_row(GROUP));
        return Some(TablePage {
            columns,
            rows,
            truncated: false,
            continue_token: None,
        });
    }
    for (set, kind) in inventory.sets() {
        match set {
            KindSet::NotServed => {}
            KindSet::Denied => rows.push(denial_row(kind.as_str())),
            KindSet::Served {
                items,
                truncated: cap,
                ..
            } => {
                truncated |= *cap;
                for item in items {
                    let uid = if item.uid.is_empty() {
                        format!("{}/{}/{}", item.kind.as_str(), item.namespace, item.name)
                    } else {
                        item.uid.clone()
                    };
                    let services: Vec<String> = item.services.iter().map(backend_label).collect();
                    rows.push(TableRow {
                        cells: vec![
                            item.kind.as_str().to_string(),
                            item.name.clone(),
                            item.namespace.clone(),
                            join_clipped(&item.entrypoints),
                            join_clipped(&item.routes),
                            join_clipped(&services),
                            join_clipped(&item.middlewares),
                            item.tls_secret.clone(),
                        ],
                        name: item.name.clone(),
                        namespace: Some(item.namespace.clone()),
                        uid,
                    });
                }
            }
        }
    }
    Some(TablePage {
        columns,
        rows,
        truncated,
        continue_token: None,
    })
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

/// The inventory as a document, rendered here so a test can gate the words.
pub fn render(inventory: &Inventory) -> Vec<String> {
    if matches!(inventory.group, GroupState::NotServed) {
        return vec![
            "Traefik is not served by this cluster".to_string(),
            String::new(),
            "this reads IngressRoute and the other traefik.io routing CRs the \
             controller already publishes; nothing is installed to find them, so a \
             cluster without Traefik shows as empty here"
                .to_string(),
        ];
    }
    if matches!(inventory.group, GroupState::Denied) {
        let mut lines = vec!["traefik.io: access denied for this account".to_string()];
        if let Some(class) = &inventory.default_ingress_class {
            lines.push(format!(
                "default IngressClass is {} ({}), so Traefik is installed even though its \
                 routing CRs cannot be listed by this account",
                class.name, class.controller
            ));
        }
        return lines;
    }

    let total: usize = inventory
        .sets()
        .iter()
        .map(|(set, _)| set.items().len())
        .sum();
    let unreadable: usize = inventory
        .sets()
        .iter()
        .map(|(set, _)| match set {
            KindSet::Served { unreadable, .. } => *unreadable,
            KindSet::NotServed | KindSet::Denied => 0,
        })
        .sum();
    let truncated = inventory.sets().iter().any(|(set, _)| match set {
        KindSet::Served { truncated, .. } => *truncated,
        KindSet::NotServed | KindSet::Denied => false,
    });
    let denied = inventory
        .sets()
        .iter()
        .filter(|(set, _)| matches!(set, KindSet::Denied))
        .count();

    let mut lines = Vec::new();
    if total == 0 && unreadable == 0 && denied == 0 {
        lines.push("no Traefik routing objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no Traefik object could be read here, though some are stored: every object this \
             account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!(
            "{} Traefik routing {}",
            total,
            plural(total, "object")
        ));
    }
    if let Some(class) = &inventory.default_ingress_class {
        lines.push(format!(
            "default IngressClass is {} ({})",
            class.name, class.controller
        ));
    }
    for (set, kind) in inventory.sets() {
        if matches!(set, KindSet::Denied) {
            lines.push(format!("{}: access denied for this account", kind.what()));
        }
    }
    if truncated {
        lines.push(format!(
            "the listing stopped at {MAX_OBJECTS} objects per kind, so this is some of them \
             rather than all",
        ));
    }
    if unreadable > 0 && total > 0 {
        lines.push(format!(
            "{} Traefik {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in inventory.sets() {
        for item in set.items() {
            lines.push(String::new());
            lines.push(format!("{}/{}", item.namespace, item.name));
            let mut line = format!("  {}", item.kind.as_str());
            if !item.entrypoints.is_empty() {
                line.push_str("  ");
                line.push_str(&item.entrypoints.join(","));
            }
            if !item.routes.is_empty() {
                line.push_str("  ");
                line.push_str(&item.routes.join(" | "));
            }
            if !item.services.is_empty() {
                line.push_str("  ");
                let services: Vec<String> = item.services.iter().map(backend_label).collect();
                line.push_str(&services.join(","));
            }
            if !item.middlewares.is_empty() {
                line.push_str("  mw ");
                line.push_str(&item.middlewares.join(","));
            }
            if !item.tls_secret.is_empty() {
                line.push_str("  tls ");
                line.push_str(&item.tls_secret);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "traefik_test.rs"]
mod tests;
