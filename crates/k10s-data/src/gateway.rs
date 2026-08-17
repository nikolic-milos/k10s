//! Gateway API inventory from the CRs the cluster already serves.
//!
//! This is `gateway.networking.k8s.io`, not Istio's `networking.istio.io`
//! Gateway. The two share a kind name and nothing else; mesh.rs owns the
//! Istio object.
//!
//! A cluster that does not serve the group answers 404 and
//! [`table_page`] is `None`. A cluster that serves the group with zero
//! objects (this k3s: preferred `v1`, also `v1beta1`, every kind below
//! present, no instances) is [`Inventory::served`] = true and
//! [`table_page`] is `Some` with zero rows. A 403 is Denied. Nothing is
//! installed to find them, and no sample Gateway is applied.
//!
//! ListenerSet is Standard channel since Gateway API 1.5 and is served on
//! this k3s. TCPRoute and UDPRoute are Standard channel at `v1` since
//! Gateway API 1.6. All three are inventoried with the other standard
//! kinds.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;

const GROUP: &str = "gateway.networking.k8s.io";

/// The ten CRs this inventory reads when the group document names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    GatewayClass,
    Gateway,
    HTTPRoute,
    GRPCRoute,
    TLSRoute,
    TCPRoute,
    UDPRoute,
    ReferenceGrant,
    BackendTLSPolicy,
    ListenerSet,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::GatewayClass => "GatewayClass",
            Kind::Gateway => "Gateway",
            Kind::HTTPRoute => "HTTPRoute",
            Kind::GRPCRoute => "GRPCRoute",
            Kind::TLSRoute => "TLSRoute",
            Kind::TCPRoute => "TCPRoute",
            Kind::UDPRoute => "UDPRoute",
            Kind::ReferenceGrant => "ReferenceGrant",
            Kind::BackendTLSPolicy => "BackendTLSPolicy",
            Kind::ListenerSet => "ListenerSet",
        }
    }

    pub fn group(self) -> &'static str {
        GROUP
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::GatewayClass => "gatewayclasses",
            Kind::Gateway => "gateways",
            Kind::HTTPRoute => "httproutes",
            Kind::GRPCRoute => "grpcroutes",
            Kind::TLSRoute => "tlsroutes",
            Kind::TCPRoute => "tcproutes",
            Kind::UDPRoute => "udproutes",
            Kind::ReferenceGrant => "referencegrants",
            Kind::BackendTLSPolicy => "backendtlspolicies",
            Kind::ListenerSet => "listenersets",
        }
    }

    /// The version we try when the group document names none. On the live
    /// k3s this inventory was written against, every kind below is served
    /// at `v1`; `v1beta1` still names GatewayClass, Gateway, HTTPRoute, and
    /// ReferenceGrant.
    pub fn version(self) -> &'static str {
        "v1"
    }

    pub fn namespaced(self) -> bool {
        !matches!(self, Kind::GatewayClass)
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::GatewayClass => "gateway gatewayclasses",
            Kind::Gateway => "gateway gateways",
            Kind::HTTPRoute => "gateway httproutes",
            Kind::GRPCRoute => "gateway grpcroutes",
            Kind::TLSRoute => "gateway tlsroutes",
            Kind::TCPRoute => "gateway tcproutes",
            Kind::UDPRoute => "gateway udproutes",
            Kind::ReferenceGrant => "gateway referencegrants",
            Kind::BackendTLSPolicy => "gateway backendtlspolicies",
            Kind::ListenerSet => "gateway listenersets",
        }
    }
}

/// One CR, reduced to what an inventory shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub class: String,
    pub addresses: String,
    pub accepted: String,
    pub programmed: String,
    pub parent_refs: String,
    pub hostnames: String,
    pub backends: String,
}

/// What one kind's list answered.
///
/// A 404 on the group is [`KindSet::NotServed`] and [`Inventory::served`]
/// false. A 403 is [`KindSet::Denied`]. An empty list on a served group is
/// [`KindSet::Served`] with no items: that is this k3s, not absence.
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    /// False only when GET `/apis/gateway.networking.k8s.io` answered 404.
    /// Empty objects on a served group stay true: [`table_page`] is `Some`.
    pub served: bool,
    pub gateway_classes: KindSet,
    pub gateways: KindSet,
    pub http_routes: KindSet,
    pub grpc_routes: KindSet,
    pub tls_routes: KindSet,
    pub tcp_routes: KindSet,
    pub udp_routes: KindSet,
    pub reference_grants: KindSet,
    pub backend_tls_policies: KindSet,
    pub listener_sets: KindSet,
}

impl Inventory {
    fn sets(&self) -> [(&KindSet, Kind); 10] {
        [
            (&self.gateway_classes, Kind::GatewayClass),
            (&self.gateways, Kind::Gateway),
            (&self.http_routes, Kind::HTTPRoute),
            (&self.grpc_routes, Kind::GRPCRoute),
            (&self.tls_routes, Kind::TLSRoute),
            (&self.tcp_routes, Kind::TCPRoute),
            (&self.udp_routes, Kind::UDPRoute),
            (&self.reference_grants, Kind::ReferenceGrant),
            (&self.backend_tls_policies, Kind::BackendTLSPolicy),
            (&self.listener_sets, Kind::ListenerSet),
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
    items: Vec<serde_json::Value>,
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
    spec: WireSpec,
    #[serde(default)]
    status: WireStatus,
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

#[derive(Deserialize, Default)]
struct WireSpec {
    #[serde(default, rename = "controllerName")]
    controller_name: String,
    #[serde(default, rename = "gatewayClassName")]
    gateway_class_name: String,
    #[serde(default, rename = "parentRefs")]
    parent_refs: Vec<WireRef>,
    #[serde(default, rename = "parentRef")]
    parent_ref: WireRef,
    #[serde(default)]
    listeners: Vec<WireListener>,
    #[serde(default)]
    hostnames: Vec<String>,
    #[serde(default)]
    rules: Vec<WireRule>,
    #[serde(default)]
    from: Vec<WireGrantPeer>,
    #[serde(default)]
    to: Vec<WireGrantPeer>,
    #[serde(default, rename = "targetRefs")]
    target_refs: Vec<WireRef>,
    #[serde(default)]
    validation: WireValidation,
}

#[derive(Deserialize, Default)]
struct WireListener {
    #[serde(default)]
    hostname: String,
}

#[derive(Deserialize, Default)]
struct WireRef {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize, Default)]
struct WireRule {
    #[serde(default, rename = "backendRefs")]
    backend_refs: Vec<WireRef>,
}

#[derive(Deserialize, Default)]
struct WireGrantPeer {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireValidation {
    #[serde(default)]
    hostname: String,
}

#[derive(Deserialize, Default)]
struct WireStatus {
    #[serde(default)]
    conditions: Vec<WireCondition>,
    #[serde(default)]
    addresses: Vec<WireAddress>,
    #[serde(default)]
    parents: Vec<WireParentStatus>,
}

#[derive(Deserialize, Default)]
struct WireCondition {
    #[serde(default, rename = "type")]
    type_name: String,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize, Default)]
struct WireAddress {
    #[serde(default)]
    value: String,
}

#[derive(Deserialize, Default)]
struct WireParentStatus {
    #[serde(default)]
    conditions: Vec<WireCondition>,
}

enum GroupAnswer {
    Served(Vec<String>),
    NotServed,
    Denied,
    Failed(String),
}

enum ListErr {
    NotFound,
    Denied,
    Failed(String),
}

fn after_group(error: &kube::Error) -> GroupAnswer {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return GroupAnswer::Denied;
        }
        if response.code == 404 {
            return GroupAnswer::NotServed;
        }
    }
    GroupAnswer::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

fn after_list(error: &kube::Error) -> ListErr {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return ListErr::Denied;
        }
        if response.code == 404 {
            return ListErr::NotFound;
        }
    }
    ListErr::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn join_clipped(parts: &[String]) -> String {
    clipped(parts.join(", "))
}

fn format_ref(reference: &WireRef) -> String {
    if reference.name.is_empty() {
        return String::new();
    }
    match (reference.kind.as_str(), reference.namespace.as_str()) {
        ("", "") => reference.name.clone(),
        (kind, "") => format!("{kind}/{}", reference.name),
        ("", namespace) => format!("{namespace}/{}", reference.name),
        (kind, namespace) => format!("{kind}/{namespace}/{}", reference.name),
    }
}

fn condition_of(conditions: &[WireCondition], type_name: &str) -> String {
    conditions
        .iter()
        .find(|condition| condition.type_name == type_name)
        .map(|condition| clipped(condition.status.clone()))
        .unwrap_or_default()
}

fn accepted_of(kind: Kind, status: &WireStatus) -> String {
    let from_object = condition_of(&status.conditions, "Accepted");
    if !from_object.is_empty()
        || !matches!(
            kind,
            Kind::HTTPRoute | Kind::GRPCRoute | Kind::TLSRoute | Kind::TCPRoute | Kind::UDPRoute
        )
    {
        return from_object;
    }
    status
        .parents
        .iter()
        .find_map(|parent| {
            let status = condition_of(&parent.conditions, "Accepted");
            (!status.is_empty()).then_some(status)
        })
        .unwrap_or_default()
}

fn class_of(kind: Kind, spec: &WireSpec) -> String {
    match kind {
        Kind::GatewayClass => clipped(spec.controller_name.clone()),
        Kind::Gateway => clipped(spec.gateway_class_name.clone()),
        _ => String::new(),
    }
}

fn addresses_of(status: &WireStatus) -> String {
    let values: Vec<String> = status
        .addresses
        .iter()
        .filter(|item| !item.value.is_empty())
        .map(|item| item.value.clone())
        .collect();
    join_clipped(&values)
}

fn parent_refs_of(kind: Kind, spec: &WireSpec) -> String {
    match kind {
        Kind::HTTPRoute
        | Kind::GRPCRoute
        | Kind::TLSRoute
        | Kind::TCPRoute
        | Kind::UDPRoute
        | Kind::ListenerSet => {
            let mut refs: Vec<String> = spec
                .parent_refs
                .iter()
                .map(format_ref)
                .filter(|item| !item.is_empty())
                .collect();
            let parent = format_ref(&spec.parent_ref);
            if !parent.is_empty() && !refs.iter().any(|have| have == &parent) {
                refs.push(parent);
            }
            join_clipped(&refs)
        }
        Kind::ReferenceGrant => {
            let peers: Vec<String> = spec
                .from
                .iter()
                .map(|peer| match (peer.kind.as_str(), peer.namespace.as_str()) {
                    ("", "") => String::new(),
                    (kind, "") => kind.to_string(),
                    ("", namespace) => namespace.to_string(),
                    (kind, namespace) => format!("{kind}/{namespace}"),
                })
                .filter(|item| !item.is_empty())
                .collect();
            join_clipped(&peers)
        }
        Kind::GatewayClass | Kind::Gateway | Kind::BackendTLSPolicy => String::new(),
    }
}

fn hostnames_of(kind: Kind, spec: &WireSpec) -> String {
    match kind {
        Kind::BackendTLSPolicy => clipped(spec.validation.hostname.clone()),
        Kind::Gateway | Kind::ListenerSet => {
            let names: Vec<String> = spec
                .listeners
                .iter()
                .map(|listener| listener.hostname.clone())
                .filter(|item| !item.is_empty())
                .collect();
            if names.is_empty() {
                join_clipped(&spec.hostnames)
            } else {
                join_clipped(&names)
            }
        }
        _ => join_clipped(&spec.hostnames),
    }
}

fn backends_of(kind: Kind, spec: &WireSpec) -> String {
    match kind {
        Kind::HTTPRoute | Kind::GRPCRoute | Kind::TLSRoute | Kind::TCPRoute | Kind::UDPRoute => {
            let names: Vec<String> = spec
                .rules
                .iter()
                .flat_map(|rule| rule.backend_refs.iter())
                .filter(|item| !item.name.is_empty())
                .map(|item| item.name.clone())
                .collect();
            join_clipped(&names)
        }
        Kind::BackendTLSPolicy => {
            let names: Vec<String> = spec
                .target_refs
                .iter()
                .map(format_ref)
                .filter(|item| !item.is_empty())
                .collect();
            join_clipped(&names)
        }
        Kind::ReferenceGrant => {
            let peers: Vec<String> = spec
                .to
                .iter()
                .map(|peer| {
                    if peer.name.is_empty() {
                        peer.kind.clone()
                    } else if peer.kind.is_empty() {
                        peer.name.clone()
                    } else {
                        format!("{}/{}", peer.kind, peer.name)
                    }
                })
                .filter(|item| !item.is_empty())
                .collect();
            join_clipped(&peers)
        }
        Kind::GatewayClass | Kind::Gateway | Kind::ListenerSet => String::new(),
    }
}

fn from_wire(kind: Kind, version: &str, wire: WireObject) -> Option<Resource> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    Some(Resource {
        kind,
        version: version.to_string(),
        name: clipped(wire.metadata.name),
        namespace: clipped(wire.metadata.namespace),
        uid: clipped(wire.metadata.uid),
        class: class_of(kind, &wire.spec),
        addresses: addresses_of(&wire.status),
        accepted: accepted_of(kind, &wire.status),
        programmed: condition_of(&wire.status.conditions, "Programmed"),
        parent_refs: parent_refs_of(kind, &wire.spec),
        hostnames: hostnames_of(kind, &wire.spec),
        backends: backends_of(kind, &wire.spec),
    })
}

fn parse_item(kind: Kind, version: &str, value: serde_json::Value) -> Option<Resource> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_wire(kind, version, wire)
}

fn order_versions(preferred: &str, versions: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    if !preferred.is_empty() {
        out.push(preferred.to_string());
    }
    for version in versions {
        if version.is_empty() || out.iter().any(|have| have == &version) {
            continue;
        }
        out.push(version);
    }
    out
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
    let mut path = format!("/apis/{}/{version}", kind.group());
    if kind.namespaced() {
        if let Some(namespace) = namespace {
            path.push_str("/namespaces/");
            path.push_str(namespace);
        }
    }
    path.push('/');
    path.push_str(kind.plural());
    path
}

fn group_url(group: &str) -> String {
    format!("/apis/{group}")
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
            match parse_item(kind, version, value) {
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

const KINDS: [Kind; 10] = [
    Kind::GatewayClass,
    Kind::Gateway,
    Kind::HTTPRoute,
    Kind::GRPCRoute,
    Kind::TLSRoute,
    Kind::TCPRoute,
    Kind::UDPRoute,
    Kind::ReferenceGrant,
    Kind::BackendTLSPolicy,
    Kind::ListenerSet,
];

fn denied_inventory() -> Inventory {
    Inventory {
        served: true,
        gateway_classes: KindSet::Denied,
        gateways: KindSet::Denied,
        http_routes: KindSet::Denied,
        grpc_routes: KindSet::Denied,
        tls_routes: KindSet::Denied,
        tcp_routes: KindSet::Denied,
        udp_routes: KindSet::Denied,
        reference_grants: KindSet::Denied,
        backend_tls_policies: KindSet::Denied,
        listener_sets: KindSet::Denied,
    }
}

/// List Gateway API kinds. A missing group is invisible; a served group with
/// zero objects is a table, not absence.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let versions = match probe_group(client, GROUP).await {
        GroupAnswer::NotServed => return Fetched::Ok(Inventory::default()),
        GroupAnswer::Denied => return Fetched::Ok(denied_inventory()),
        GroupAnswer::Failed(why) => {
            return Fetched::Failed {
                what: "gateway api",
                why,
            };
        }
        GroupAnswer::Served(versions) => versions,
    };
    let mut sets = Vec::with_capacity(KINDS.len());
    for kind in KINDS {
        match list_kind(client, kind, &versions, namespace).await {
            Ok(set) => sets.push(set),
            Err(failed) => return failed,
        }
    }
    let mut sets = sets.into_iter();
    Fetched::Ok(Inventory {
        served: true,
        gateway_classes: sets.next().unwrap_or_default(),
        gateways: sets.next().unwrap_or_default(),
        http_routes: sets.next().unwrap_or_default(),
        grpc_routes: sets.next().unwrap_or_default(),
        tls_routes: sets.next().unwrap_or_default(),
        tcp_routes: sets.next().unwrap_or_default(),
        udp_routes: sets.next().unwrap_or_default(),
        reference_grants: sets.next().unwrap_or_default(),
        backend_tls_policies: sets.next().unwrap_or_default(),
        listener_sets: sets.next().unwrap_or_default(),
    })
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

fn conditions_label(item: &Resource) -> String {
    match (item.accepted.as_str(), item.programmed.as_str()) {
        ("", "") => String::new(),
        (accepted, "") => format!("Accepted={accepted}"),
        ("", programmed) => format!("Programmed={programmed}"),
        (accepted, programmed) => format!("Accepted={accepted} Programmed={programmed}"),
    }
}

/// Native list rows. `None` only when the group is not served. An empty
/// `Some` is a cluster that serves Gateway API and stores no objects.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served {
        return None;
    }
    let columns = [
        "Kind",
        "Name",
        "Namespace",
        "Class",
        "Addresses",
        "Conditions",
        "Hosts",
        "Backends",
    ]
    .iter()
    .map(|name| TableColumn {
        name: name.to_string(),
        wide: false,
    })
    .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    for (set, kind) in inventory.sets() {
        match set {
            KindSet::NotServed => {}
            KindSet::Denied => {
                rows.push(TableRow {
                    cells: vec![
                        kind.as_str().to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        "access denied for this account".to_string(),
                        String::new(),
                        String::new(),
                    ],
                    name: kind.as_str().to_string(),
                    namespace: None,
                    uid: format!("denied:{}", kind.as_str()),
                });
            }
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
                    rows.push(TableRow {
                        cells: vec![
                            item.kind.as_str().to_string(),
                            item.name.clone(),
                            item.namespace.clone(),
                            item.class.clone(),
                            item.addresses.clone(),
                            conditions_label(item),
                            item.hostnames.clone(),
                            item.backends.clone(),
                        ],
                        name: item.name.clone(),
                        namespace: if item.namespace.is_empty() {
                            None
                        } else {
                            Some(item.namespace.clone())
                        },
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

/// The inventory as a document, rendered here for the same reason a describe
/// is: one deterministic rendering is what makes it gateable by a test.
pub fn render(inventory: &Inventory) -> Vec<String> {
    if !inventory.served {
        return vec![
            "Gateway API is not served by this cluster".to_string(),
            String::new(),
            "this reads GatewayClass, Gateway, HTTPRoute, GRPCRoute, TLSRoute, \
             TCPRoute, UDPRoute, ReferenceGrant, BackendTLSPolicy and \
             ListenerSet CRs on gateway.networking.k8s.io; nothing is \
             installed to find them, and this is not Istio Gateway"
                .to_string(),
        ];
    }

    let sets = inventory.sets();
    let total: usize = sets.iter().map(|(set, _)| set.items().len()).sum();
    let unreadable: usize = sets
        .iter()
        .map(|(set, _)| match set {
            KindSet::Served { unreadable, .. } => *unreadable,
            KindSet::NotServed | KindSet::Denied => 0,
        })
        .sum();
    let truncated = sets.iter().any(|(set, _)| match set {
        KindSet::Served { truncated, .. } => *truncated,
        KindSet::NotServed | KindSet::Denied => false,
    });
    let denied = sets
        .iter()
        .filter(|(set, _)| matches!(set, KindSet::Denied))
        .count();

    let mut lines = Vec::new();
    if total == 0 && unreadable == 0 && denied == 0 {
        lines.push("no Gateway API objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no Gateway API object could be read here, though some are stored: every object \
             this account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!("{} Gateway API {}", total, plural(total, "object")));
    }
    for (set, kind) in &sets {
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
            "{} Gateway API {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            let identity = if item.namespace.is_empty() {
                item.name.clone()
            } else {
                format!("{}/{}", item.namespace, item.name)
            };
            lines.push(identity);
            let mut line = format!("  {}", item.kind.as_str());
            if !item.class.is_empty() {
                line.push_str("  ");
                line.push_str(&item.class);
            }
            let conditions = conditions_label(item);
            if !conditions.is_empty() {
                line.push_str("  ");
                line.push_str(&conditions);
            }
            if !item.addresses.is_empty() {
                line.push_str("  ");
                line.push_str(&item.addresses);
            }
            if !item.parent_refs.is_empty() {
                line.push_str("  parents ");
                line.push_str(&item.parent_refs);
            }
            if !item.hostnames.is_empty() {
                line.push_str("  ");
                line.push_str(&item.hostnames);
            }
            if !item.backends.is_empty() {
                line.push_str("  ");
                line.push_str(&item.backends);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "gateway_test.rs"]
mod tests;
