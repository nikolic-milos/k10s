//! Famous ingress-controller CRs, only if the cluster already serves them.
//!
//! Contour, Envoy Gateway, HAProxy, Kong, NGINX Plus/IC (`k8s.nginx.org`),
//! and Ambassador/Emissary. Each controller is one [`KindSet`]: a 404 on
//! every group that controller uses is [`KindSet::NotServed`], a 403 is
//! Denied. Core Ingress lives in a different module. Traefik CRs, Istio
//! Gateway, Gateway API objects, and Envoy admin are not listed here.
//!
//! HAProxy's current groups are `ingress.v1.haproxy.org` and
//! `ingress.v3.haproxy.org`; `core.haproxy.org` (v1alpha1/v1alpha2) is the
//! pre-1.11 group, probed and skipped on 404.
//! `nginx.ingress.kubernetes.io` is annotations on Ingress, not a CR group,
//! and `nginx.org/…` is an annotation prefix, not a group either.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;
use crate::served::{GroupAnswer, ListErr, after_group, after_list, group_url, order_versions};

pub const CONTOUR_GROUP: &str = "projectcontour.io";
pub const ENVOY_GATEWAY_GROUP: &str = "gateway.envoyproxy.io";
pub const HAPROXY_V1_GROUP: &str = "ingress.v1.haproxy.org";
pub const HAPROXY_V3_GROUP: &str = "ingress.v3.haproxy.org";
pub const HAPROXY_LEGACY_GROUP: &str = "core.haproxy.org";
pub const KONG_GROUP: &str = "configuration.konghq.com";
pub const NGINX_GROUP: &str = "k8s.nginx.org";
pub const AMBASSADOR_GROUP: &str = "getambassador.io";
pub const AMBASSADOR_GATEWAY_GROUP: &str = "gateway.getambassador.io";

const PAGE_LIMIT: u32 = 200;
pub const MAX_OBJECTS: usize = 2_000;
pub const MAX_FIELD_CHARS: usize = 200;
const MAX_HOSTS: usize = 32;
const MAX_BACKENDS: usize = 32;
const MAX_TLS_SECRETS: usize = 16;

/// One ingress-controller family. Independently droppable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Controller {
    Contour,
    EnvoyGateway,
    Haproxy,
    Kong,
    Nginx,
    Ambassador,
}

impl Controller {
    pub fn as_str(self) -> &'static str {
        match self {
            Controller::Contour => "Contour",
            Controller::EnvoyGateway => "Envoy Gateway",
            Controller::Haproxy => "HAProxy",
            Controller::Kong => "Kong",
            Controller::Nginx => "NGINX",
            Controller::Ambassador => "Ambassador",
        }
    }

    pub fn what(self) -> &'static str {
        match self {
            Controller::Contour => "contour",
            Controller::EnvoyGateway => "envoy gateway",
            Controller::Haproxy => "haproxy",
            Controller::Kong => "kong",
            Controller::Nginx => "nginx virtualservers",
            Controller::Ambassador => "ambassador",
        }
    }

    fn groups(self) -> &'static [&'static str] {
        match self {
            Controller::Contour => &[CONTOUR_GROUP],
            Controller::EnvoyGateway => &[ENVOY_GATEWAY_GROUP],
            Controller::Haproxy => &[HAPROXY_V1_GROUP, HAPROXY_V3_GROUP, HAPROXY_LEGACY_GROUP],
            Controller::Kong => &[KONG_GROUP],
            Controller::Nginx => &[NGINX_GROUP],
            Controller::Ambassador => &[AMBASSADOR_GROUP, AMBASSADOR_GATEWAY_GROUP],
        }
    }

    fn kinds(self) -> &'static [Kind] {
        match self {
            Controller::Contour => &[Kind::HttpProxy, Kind::TlsCertificateDelegation],
            Controller::EnvoyGateway => &[
                Kind::EnvoyProxy,
                Kind::EnvoyPatchPolicy,
                Kind::BackendTrafficPolicy,
                Kind::SecurityPolicy,
                Kind::HttpRouteFilter,
            ],
            Controller::Haproxy => &[
                Kind::HaproxyBackend,
                Kind::HaproxyDefaults,
                Kind::HaproxyGlobal,
                Kind::HaproxyTcp,
            ],
            Controller::Kong => &[
                Kind::KongPlugin,
                Kind::KongClusterPlugin,
                Kind::KongConsumer,
                Kind::KongIngress,
                Kind::TcpIngress,
                Kind::UdpIngress,
            ],
            Controller::Nginx => &[
                Kind::VirtualServer,
                Kind::VirtualServerRoute,
                Kind::TransportServer,
                Kind::NginxPolicy,
                Kind::GlobalConfiguration,
            ],
            Controller::Ambassador => &[Kind::Host, Kind::Mapping],
        }
    }
}

/// The CRs this inventory will list when a group document serves them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    HttpProxy,
    TlsCertificateDelegation,
    EnvoyProxy,
    EnvoyPatchPolicy,
    BackendTrafficPolicy,
    SecurityPolicy,
    HttpRouteFilter,
    HaproxyBackend,
    HaproxyDefaults,
    HaproxyGlobal,
    HaproxyTcp,
    KongPlugin,
    KongClusterPlugin,
    KongConsumer,
    KongIngress,
    TcpIngress,
    UdpIngress,
    VirtualServer,
    VirtualServerRoute,
    TransportServer,
    NginxPolicy,
    GlobalConfiguration,
    Host,
    Mapping,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::HttpProxy => "HTTPProxy",
            Kind::TlsCertificateDelegation => "TLSCertificateDelegation",
            Kind::EnvoyProxy => "EnvoyProxy",
            Kind::EnvoyPatchPolicy => "EnvoyPatchPolicy",
            Kind::BackendTrafficPolicy => "BackendTrafficPolicy",
            Kind::SecurityPolicy => "SecurityPolicy",
            Kind::HttpRouteFilter => "HTTPRouteFilter",
            Kind::HaproxyBackend => "Backend",
            Kind::HaproxyDefaults => "Defaults",
            Kind::HaproxyGlobal => "Global",
            Kind::HaproxyTcp => "TCP",
            Kind::KongPlugin => "KongPlugin",
            Kind::KongClusterPlugin => "KongClusterPlugin",
            Kind::KongConsumer => "KongConsumer",
            Kind::KongIngress => "KongIngress",
            Kind::TcpIngress => "TCPIngress",
            Kind::UdpIngress => "UDPIngress",
            Kind::VirtualServer => "VirtualServer",
            Kind::VirtualServerRoute => "VirtualServerRoute",
            Kind::TransportServer => "TransportServer",
            Kind::NginxPolicy => "Policy",
            Kind::GlobalConfiguration => "GlobalConfiguration",
            Kind::Host => "Host",
            Kind::Mapping => "Mapping",
        }
    }

    pub fn controller(self) -> Controller {
        match self {
            Kind::HttpProxy | Kind::TlsCertificateDelegation => Controller::Contour,
            Kind::EnvoyProxy
            | Kind::EnvoyPatchPolicy
            | Kind::BackendTrafficPolicy
            | Kind::SecurityPolicy
            | Kind::HttpRouteFilter => Controller::EnvoyGateway,
            Kind::HaproxyBackend
            | Kind::HaproxyDefaults
            | Kind::HaproxyGlobal
            | Kind::HaproxyTcp => Controller::Haproxy,
            Kind::KongPlugin
            | Kind::KongClusterPlugin
            | Kind::KongConsumer
            | Kind::KongIngress
            | Kind::TcpIngress
            | Kind::UdpIngress => Controller::Kong,
            Kind::VirtualServer
            | Kind::VirtualServerRoute
            | Kind::TransportServer
            | Kind::NginxPolicy
            | Kind::GlobalConfiguration => Controller::Nginx,
            Kind::Host | Kind::Mapping => Controller::Ambassador,
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::HttpProxy => "httpproxies",
            Kind::TlsCertificateDelegation => "tlscertificatedelegations",
            Kind::EnvoyProxy => "envoyproxies",
            Kind::EnvoyPatchPolicy => "envoypatchpolicies",
            Kind::BackendTrafficPolicy => "backendtrafficpolicies",
            Kind::SecurityPolicy => "securitypolicies",
            Kind::HttpRouteFilter => "httproutefilters",
            Kind::HaproxyBackend => "backends",
            Kind::HaproxyDefaults => "defaults",
            Kind::HaproxyGlobal => "globals",
            Kind::HaproxyTcp => "tcps",
            Kind::KongPlugin => "kongplugins",
            Kind::KongClusterPlugin => "kongclusterplugins",
            Kind::KongConsumer => "kongconsumers",
            Kind::KongIngress => "kongingresses",
            Kind::TcpIngress => "tcpingresses",
            Kind::UdpIngress => "udpingresses",
            Kind::VirtualServer => "virtualservers",
            Kind::VirtualServerRoute => "virtualserverroutes",
            Kind::TransportServer => "transportservers",
            Kind::NginxPolicy => "policies",
            Kind::GlobalConfiguration => "globalconfigurations",
            Kind::Host => "hosts",
            Kind::Mapping => "mappings",
        }
    }

    /// Cluster-scoped kinds must never get `/namespaces/{ns}` in their URL.
    pub fn namespaced(self) -> bool {
        !matches!(self, Kind::KongClusterPlugin)
    }

    /// The version we try when the group document names none.
    pub fn version(self) -> &'static str {
        match self {
            Kind::TcpIngress | Kind::UdpIngress => "v1beta1",
            Kind::EnvoyProxy
            | Kind::EnvoyPatchPolicy
            | Kind::BackendTrafficPolicy
            | Kind::SecurityPolicy
            | Kind::HttpRouteFilter => "v1alpha1",
            Kind::Host | Kind::Mapping => "v3alpha1",
            _ => "v1",
        }
    }
}

/// One CR, reduced to hosts, backends, and TLS secret names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub group: String,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub hosts: Vec<String>,
    pub backends: Vec<String>,
    pub tls_secrets: Vec<String>,
    pub detail: String,
}

/// What one controller's groups answered.
///
/// A 404 on every group is [`KindSet::NotServed`]. A 403 is Denied. Those
/// stay distinct so a refused account is not reported as "not installed".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KindSet {
    Served {
        items: Vec<Resource>,
        truncated: bool,
        unreadable: usize,
        /// Sibling groups or kinds of this controller that answered 403.
        /// Kept as a count so a partial denial survives the merge instead
        /// of vanishing behind the kinds that did answer.
        denied: usize,
        /// Sibling groups or kinds whose fetch failed outright.
        failed: usize,
    },
    #[default]
    NotServed,
    Denied,
    /// This controller could not be asked (a 5xx or a transport error).
    /// Not absence and not denial — and never a reason to hide the other
    /// five controllers' answers.
    Failed {
        why: String,
    },
}

impl KindSet {
    pub fn served(&self) -> bool {
        !matches!(self, KindSet::NotServed)
    }

    pub fn items(&self) -> &[Resource] {
        match self {
            KindSet::Served { items, .. } => items,
            KindSet::NotServed | KindSet::Denied | KindSet::Failed { .. } => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub contour: KindSet,
    pub envoy_gateway: KindSet,
    pub haproxy: KindSet,
    pub kong: KindSet,
    pub nginx: KindSet,
    pub ambassador: KindSet,
}

impl Inventory {
    /// False when every proxy group answered 404.
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Controller); 6] {
        [
            (&self.contour, Controller::Contour),
            (&self.envoy_gateway, Controller::EnvoyGateway),
            (&self.haproxy, Controller::Haproxy),
            (&self.kong, Controller::Kong),
            (&self.nginx, Controller::Nginx),
            (&self.ambassador, Controller::Ambassador),
        ]
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
    #[serde(default)]
    plugin: String,
    #[serde(default)]
    username: String,
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

fn push_unique(out: &mut Vec<String>, text: String) {
    if text.is_empty() || out.iter().any(|have| have == &text) {
        return;
    }
    out.push(text);
}

fn take_secret(out: &mut Vec<String>, value: Option<&Value>) {
    if out.len() >= MAX_TLS_SECRETS {
        return;
    }
    let Some(value) = value else {
        return;
    };
    if let Some(name) = value.as_str().filter(|name| !name.is_empty()) {
        push_unique(out, clipped(name.to_string()));
        return;
    }
    if let Some(name) = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        push_unique(out, clipped(name.to_string()));
    }
}

fn tls_secrets_of(spec: &Value) -> Vec<String> {
    let mut out = Vec::new();
    take_secret(&mut out, spec.pointer("/virtualhost/tls/secretName"));
    take_secret(&mut out, spec.pointer("/tls/secretName"));
    take_secret(&mut out, spec.pointer("/tls/secret"));
    take_secret(&mut out, spec.pointer("/tlsSecret/name"));
    take_secret(&mut out, spec.get("tlsSecretName"));
    if let Some(items) = spec.get("tls").and_then(Value::as_array) {
        for item in items {
            take_secret(&mut out, item.get("secretName"));
            take_secret(&mut out, item.get("secret"));
        }
    }
    if let Some(items) = spec.get("delegations").and_then(Value::as_array) {
        for item in items {
            take_secret(&mut out, item.get("secretName"));
        }
    }
    out
}

fn push_str(out: &mut Vec<String>, value: Option<&Value>) {
    if out.len() >= MAX_HOSTS {
        return;
    }
    if let Some(text) = value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        push_unique(out, clipped(text.to_string()));
    }
}

fn hosts_of(spec: &Value) -> Vec<String> {
    let mut out = Vec::new();
    push_str(&mut out, spec.pointer("/virtualhost/fqdn"));
    push_str(&mut out, spec.get("host"));
    push_str(&mut out, spec.get("hostname"));
    if let Some(hosts) = spec.get("hosts").and_then(Value::as_array) {
        for host in hosts {
            push_str(&mut out, Some(host));
        }
    }
    if let Some(rules) = spec.get("rules").and_then(Value::as_array) {
        for rule in rules {
            push_str(&mut out, rule.get("host"));
        }
    }
    out
}

fn port_text(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Object(map) => {
            if let Some(number) = map.get("number").and_then(Value::as_i64) {
                return Some(number.to_string());
            }
            map.get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        }
        _ => None,
    }
}

fn push_backend(out: &mut Vec<String>, value: &Value) {
    if out.len() >= MAX_BACKENDS {
        return;
    }
    // An NGINX upstream's `name` is an intra-document alias; `service` is
    // the Service it fronts. The Service reference wins so the column joins
    // to a workload, not an alias.
    let name = value
        .get("service")
        .and_then(Value::as_str)
        .or_else(|| value.get("serviceName").and_then(Value::as_str))
        .or_else(|| value.get("name").and_then(Value::as_str))
        .unwrap_or("");
    if name.is_empty() {
        return;
    }
    let port = value
        .get("port")
        .and_then(port_text)
        .or_else(|| value.get("servicePort").and_then(port_text))
        .unwrap_or_default();
    let text = if port.is_empty() {
        name.to_string()
    } else {
        format!("{name}:{port}")
    };
    push_unique(out, clipped(text));
}

fn backends_of(spec: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(service) = spec.get("service").and_then(Value::as_str) {
        push_unique(&mut out, clipped(service.to_string()));
    }
    if let Some(routes) = spec.get("routes").and_then(Value::as_array) {
        for route in routes {
            if let Some(services) = route.get("services").and_then(Value::as_array) {
                for service in services {
                    push_backend(&mut out, service);
                }
            }
        }
    }
    if let Some(upstreams) = spec.get("upstreams").and_then(Value::as_array) {
        for upstream in upstreams {
            push_backend(&mut out, upstream);
        }
    }
    if let Some(rules) = spec.get("rules").and_then(Value::as_array) {
        for rule in rules {
            if let Some(backend) = rule.get("backend") {
                push_backend(&mut out, backend);
            }
        }
    }
    out
}

fn target_ref_text(spec: &Value) -> String {
    let reference = spec.get("targetRef").or_else(|| {
        spec.get("targetRefs")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
    });
    let Some(reference) = reference else {
        if let Some(provider) = spec.pointer("/provider/type").and_then(Value::as_str) {
            return provider.to_string();
        }
        return String::new();
    };
    let kind = reference.get("kind").and_then(Value::as_str).unwrap_or("");
    let name = reference.get("name").and_then(Value::as_str).unwrap_or("");
    match (kind, name) {
        ("", "") => String::new(),
        (kind, "") => kind.to_string(),
        ("", name) => name.to_string(),
        (kind, name) => format!("{kind}/{name}"),
    }
}

fn delegation_targets(spec: &Value) -> String {
    let Some(items) = spec.get("delegations").and_then(Value::as_array) else {
        return String::new();
    };
    let mut names = Vec::new();
    for item in items {
        if let Some(targets) = item.get("targetNamespaces").and_then(Value::as_array) {
            for target in targets {
                push_str(&mut names, Some(target));
            }
        }
    }
    names.join(",")
}

fn detail_of(kind: Kind, spec: &Value, plugin: &str, username: &str) -> String {
    let text = match kind {
        Kind::KongPlugin | Kind::KongClusterPlugin => {
            if !plugin.is_empty() {
                plugin.to_string()
            } else {
                spec.get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            }
        }
        Kind::KongConsumer => {
            if !username.is_empty() {
                username.to_string()
            } else {
                spec.get("username")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            }
        }
        Kind::TlsCertificateDelegation => delegation_targets(spec),
        Kind::EnvoyProxy
        | Kind::EnvoyPatchPolicy
        | Kind::BackendTrafficPolicy
        | Kind::SecurityPolicy
        | Kind::HttpRouteFilter => target_ref_text(spec),
        _ => String::new(),
    };
    clipped(text)
}

fn from_wire(kind: Kind, group: &str, version: &str, wire: WireObject) -> Option<Resource> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    Some(Resource {
        kind,
        group: group.to_string(),
        version: version.to_string(),
        name: clipped(wire.metadata.name),
        namespace: clipped(wire.metadata.namespace),
        uid: clipped(wire.metadata.uid),
        hosts: hosts_of(&wire.spec),
        backends: backends_of(&wire.spec),
        tls_secrets: tls_secrets_of(&wire.spec),
        detail: detail_of(kind, &wire.spec, &wire.plugin, &wire.username),
    })
}

fn parse_item(kind: Kind, group: &str, version: &str, value: Value) -> Option<Resource> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_wire(kind, group, version, wire)
}

fn versions_for(kind: Kind, group_versions: &[String]) -> Vec<String> {
    let mut out = group_versions.to_vec();
    let fallback = kind.version().to_string();
    if !out.iter().any(|have| have == &fallback) {
        out.push(fallback);
    }
    out
}

fn collection_url(group: &str, version: &str, plural: &str, namespace: Option<&str>) -> String {
    let mut path = format!("/apis/{group}/{version}");
    if let Some(namespace) = namespace {
        path.push_str("/namespaces/");
        path.push_str(namespace);
    }
    path.push('/');
    path.push_str(plural);
    path
}

fn merge_sets(sets: Vec<KindSet>) -> KindSet {
    let mut items = Vec::new();
    let mut truncated = false;
    let mut unreadable = 0usize;
    let mut denied = 0usize;
    let mut failed_why: Option<String> = None;
    let mut failed = 0usize;
    let mut any_served = false;
    for set in sets {
        match set {
            KindSet::Served {
                items: more,
                truncated: cap,
                unreadable: bad,
                denied: refused,
                failed: broken,
            } => {
                any_served = true;
                truncated |= cap;
                unreadable += bad;
                denied += refused;
                failed += broken;
                for item in more {
                    if items.len() == MAX_OBJECTS {
                        truncated = true;
                        break;
                    }
                    items.push(item);
                }
            }
            KindSet::Denied => denied += 1,
            KindSet::Failed { why } => {
                failed += 1;
                failed_why.get_or_insert(why);
            }
            KindSet::NotServed => {}
        }
    }
    if any_served {
        KindSet::Served {
            items,
            truncated,
            unreadable,
            denied,
            failed,
        }
    } else if let Some(why) = failed_why {
        KindSet::Failed { why }
    } else if denied > 0 {
        KindSet::Denied
    } else {
        KindSet::NotServed
    }
}

async fn probe_group(client: &Client, group: &str) -> GroupAnswer {
    let request = match http::Request::get(group_url(group)).body(Vec::new()) {
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
    group: &str,
    version: &str,
    namespace: Option<&str>,
) -> Result<KindSet, ListErr> {
    let path = collection_url(group, version, kind.plural(), namespace);
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
            match parse_item(kind, group, version, value) {
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
        denied: 0,
        failed: 0,
    })
}

async fn list_kind(
    client: &Client,
    kind: Kind,
    group: &str,
    group_versions: &[String],
    namespace: Option<&str>,
) -> KindSet {
    for version in versions_for(kind, group_versions) {
        match list_at_version(client, kind, group, &version, namespace).await {
            Ok(set) => return set,
            Err(ListErr::NotFound) => continue,
            Err(ListErr::Denied) => return KindSet::Denied,
            Err(ListErr::Failed(why)) => return KindSet::Failed { why },
        }
    }
    KindSet::NotServed
}

async fn fetch_controller(
    client: &Client,
    controller: Controller,
    namespace: Option<&str>,
) -> KindSet {
    let mut parts = Vec::new();
    for group in controller.groups() {
        match probe_group(client, group).await {
            GroupAnswer::NotServed => parts.push(KindSet::NotServed),
            GroupAnswer::Denied => parts.push(KindSet::Denied),
            GroupAnswer::Failed(why) => parts.push(KindSet::Failed { why }),
            GroupAnswer::Served(versions) => {
                for kind in controller.kinds() {
                    let scope = if kind.namespaced() { namespace } else { None };
                    parts.push(list_kind(client, *kind, group, &versions, scope).await);
                }
            }
        }
    }
    merge_sets(parts)
}

/// Probe each controller's groups and list the CRs they already serve.
/// Controllers are independent in failure as well as in absence: one 5xx
/// or denial stays on its own row and never hides the other five. Only
/// when every controller failed — the API server itself is unreachable —
/// does the whole fetch fail.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let (contour, envoy_gateway, haproxy, kong, nginx, ambassador) = tokio::join!(
        fetch_controller(client, Controller::Contour, namespace),
        fetch_controller(client, Controller::EnvoyGateway, namespace),
        fetch_controller(client, Controller::Haproxy, namespace),
        fetch_controller(client, Controller::Kong, namespace),
        fetch_controller(client, Controller::Nginx, namespace),
        fetch_controller(client, Controller::Ambassador, namespace),
    );
    let inventory = Inventory {
        contour,
        envoy_gateway,
        haproxy,
        kong,
        nginx,
        ambassador,
    };
    let failures: Vec<String> = inventory
        .sets()
        .into_iter()
        .filter_map(|(set, _)| match set {
            KindSet::Failed { why } => Some(why.clone()),
            _ => None,
        })
        .collect();
    if failures.len() == 6 {
        return Fetched::Failed {
            what: "proxy controllers",
            why: failures.into_iter().next().unwrap_or_default(),
        };
    }
    Fetched::Ok(inventory)
}

fn join_clipped(parts: &[String]) -> String {
    clipped(parts.join(", "))
}

fn marker_row(controller: Controller, state: &str, detail: &str) -> TableRow {
    TableRow {
        cells: vec![
            controller.as_str().to_string(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            detail.to_string(),
        ],
        name: controller.as_str().to_string(),
        namespace: None,
        uid: format!("{state}:{}", controller.as_str()),
    }
}

/// `None` only when every proxy group answered 404. A denied controller is a
/// labelled row, not absence.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served() {
        return None;
    }
    let columns = [
        "Controller",
        "Kind",
        "Name",
        "Namespace",
        "Hosts",
        "Backends",
        "TLS",
        "Detail",
    ]
    .iter()
    .map(|name| TableColumn {
        name: (*name).to_string(),
        wide: false,
    })
    .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    for (set, controller) in inventory.sets() {
        match set {
            KindSet::NotServed => {}
            KindSet::Denied => {
                rows.push(marker_row(
                    controller,
                    "denied",
                    "access denied for this account",
                ));
            }
            KindSet::Failed { why } => {
                rows.push(marker_row(
                    controller,
                    "failed",
                    &clipped(format!("could not be asked: {why}")),
                ));
            }
            KindSet::Served {
                items,
                truncated: cap,
                unreadable,
                denied,
                failed,
            } => {
                truncated |= *cap;
                if *denied > 0 {
                    rows.push(marker_row(
                        controller,
                        "denied",
                        "some kinds are denied for this account",
                    ));
                }
                if *failed > 0 {
                    rows.push(marker_row(
                        controller,
                        "failed",
                        "some kinds could not be listed",
                    ));
                }
                if *unreadable > 0 {
                    rows.push(marker_row(
                        controller,
                        "unreadable",
                        &format!(
                            "{} object{} could not be decoded and {} not shown",
                            unreadable,
                            if *unreadable == 1 { "" } else { "s" },
                            if *unreadable == 1 { "is" } else { "are" },
                        ),
                    ));
                }
                for item in items {
                    let uid = if item.uid.is_empty() {
                        format!(
                            "{}/{}/{}/{}",
                            item.kind.as_str(),
                            item.group,
                            item.namespace,
                            item.name
                        )
                    } else {
                        item.uid.clone()
                    };
                    rows.push(TableRow {
                        cells: vec![
                            controller.as_str().to_string(),
                            item.kind.as_str().to_string(),
                            item.name.clone(),
                            item.namespace.clone(),
                            join_clipped(&item.hosts),
                            join_clipped(&item.backends),
                            join_clipped(&item.tls_secrets),
                            item.detail.clone(),
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

#[cfg(test)]
#[path = "proxies_test.rs"]
mod tests;
