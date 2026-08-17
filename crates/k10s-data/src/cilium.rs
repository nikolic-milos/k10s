//! Cilium inventory from the CRs the cluster already serves.
//!
//! CiliumNetworkPolicy, CiliumClusterwideNetworkPolicy, CiliumIdentity,
//! CiliumEndpoint, and CiliumNode live on `cilium.io`. The group is not in
//! `k8s-openapi`; listing goes through [`kube::api::Request`] the same way
//! Flux and PolicyReport do. A cluster that does not serve the group answers
//! 404 and the pane stays invisible, not broken. A 403 is Denied. Nothing is
//! installed to find them, and Hubble UI is not rebuilt.
//!
//! Declared reachability is compiled from the policy CRs ([`Declared`]).
//! Observed traffic is Prometheus series a caller already fetched
//! (`hubble_flows_processed_total`, [`crate::overlay::HUBBLE_EXPR`]). Those
//! two answers are different types on purpose: mixing them is a correctness
//! bug.
//!
//! Hubble Relay is gRPC (`cilium.observer.Observer` / GetFlows). This module
//! does not speak it. tonic, prost, and protobuf are outside the package
//! ceiling. A missing gRPC client is Tool-unreachable: labelled, never a
//! fake topology. Observed edges come from series labels, not from GetFlows.
//!
//! Hubble Service fingerprints for `reach.rs` (coordinator; do not grow
//! `k10s_core::ToolId`, that table re-lays theme arrays):
//!
//! hubble-relay:
//!   names: hubble-relay
//!   needles: hubble-relay
//!   ports: 80, 4245
//!   port_names: grpc, peer, observer
//!   protocol: gRPC Observer. An HTTP probe will not speak it.
//!   bind: Unbound / Tool-unreachable. Do not invent flows.
//!
//! hubble-ui:
//!   names: hubble-ui
//!   needles: hubble-ui
//!   ports: 80, 8081
//!   port_names: http, http-ui
//!   Hubble UI is a browser app. No webview. `browser_url` only.

use std::collections::BTreeMap;

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::mesh::{ObservedReach, TelemetryExporter, TelemetryReason};
use crate::prom::QueryResult;
use crate::read::Fetched;

#[path = "cilium_policy.rs"]
mod cilium_policy;
pub use cilium_policy::{
    CidrRule, CiliumPolicy, Completeness, Decision, Declared, DeclaredL7, Direction, EndpointRef,
    Entity, LabelExpression, LabelSelector, MAX_POLICIES, NamedPort, PolicyRule, PortRule,
    Protocol, Traffic, Verdict, VerdictReason, declare, parse_policy_document, selector_matches,
};

pub const GROUP: &str = "cilium.io";
pub const PAGE_LIMIT: u32 = 200;
pub const MAX_OBJECTS: usize = 2_000;
pub const MAX_FIELD_CHARS: usize = 200;
pub const MAX_PAGE_BYTES: usize = 8 << 20;
pub const MAX_LABELS: usize = 32;
pub const MAX_OBSERVED_EDGES: usize = 512;

const FALLBACK_VERSION: &str = "v2";

const NS_LABEL: &str = "io.kubernetes.pod.namespace";

/// The five CRs this inventory reads. Cilium serves more; those are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    CiliumNetworkPolicy,
    CiliumClusterwideNetworkPolicy,
    CiliumIdentity,
    CiliumEndpoint,
    CiliumNode,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::CiliumNetworkPolicy => "CiliumNetworkPolicy",
            Kind::CiliumClusterwideNetworkPolicy => "CiliumClusterwideNetworkPolicy",
            Kind::CiliumIdentity => "CiliumIdentity",
            Kind::CiliumEndpoint => "CiliumEndpoint",
            Kind::CiliumNode => "CiliumNode",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::CiliumNetworkPolicy => "ciliumnetworkpolicies",
            Kind::CiliumClusterwideNetworkPolicy => "ciliumclusterwidenetworkpolicies",
            Kind::CiliumIdentity => "ciliumidentities",
            Kind::CiliumEndpoint => "ciliumendpoints",
            Kind::CiliumNode => "ciliumnodes",
        }
    }

    pub fn namespaced(self) -> bool {
        matches!(self, Kind::CiliumNetworkPolicy | Kind::CiliumEndpoint)
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::CiliumNetworkPolicy => "cilium networkpolicies",
            Kind::CiliumClusterwideNetworkPolicy => "cilium clusterwide networkpolicies",
            Kind::CiliumIdentity => "cilium identities",
            Kind::CiliumEndpoint => "cilium endpoints",
            Kind::CiliumNode => "cilium nodes",
        }
    }
}

/// One CR, reduced to what an inventory shows. No secret-bearing fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub identity_id: Option<i64>,
    pub address: String,
    pub detail: String,
    /// Identity labels as Cilium spelled them (`k8s:io.kubernetes.pod.namespace`).
    pub labels: BTreeMap<String, String>,
}

/// What one kind's list answered.
///
/// A 404 on the group is [`KindSet::NotServed`]: invisible, not broken. A 403
/// is [`KindSet::Denied`]. Those are different states on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KindSet {
    Served {
        items: Vec<Resource>,
        truncated: bool,
        /// Some object carried more than [`MAX_LABELS`] labels and shows
        /// only its first slice. Separate from `truncated` on purpose: a
        /// clipped label set must never stop pagination or claim the
        /// object-count ceiling was hit.
        labels_clipped: bool,
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

    fn truncated(&self) -> bool {
        match self {
            KindSet::Served { truncated, .. } => *truncated,
            KindSet::NotServed | KindSet::Denied => false,
        }
    }

    fn unreadable(&self) -> usize {
        match self {
            KindSet::Served { unreadable, .. } => *unreadable,
            KindSet::NotServed | KindSet::Denied => 0,
        }
    }

    fn labels_clipped(&self) -> bool {
        match self {
            KindSet::Served { labels_clipped, .. } => *labels_clipped,
            KindSet::NotServed | KindSet::Denied => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub network_policies: KindSet,
    pub cluster_policies: KindSet,
    pub identities: KindSet,
    pub endpoints: KindSet,
    pub nodes: KindSet,
    /// Compiled from the policy CRs this fetch kept. Declared, not observed.
    pub declared: Declared,
}

impl Inventory {
    /// False when the cilium.io group answered 404 (or every kind 404d).
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Kind); 5] {
        [
            (&self.network_policies, Kind::CiliumNetworkPolicy),
            (&self.cluster_policies, Kind::CiliumClusterwideNetworkPolicy),
            (&self.identities, Kind::CiliumIdentity),
            (&self.endpoints, Kind::CiliumEndpoint),
            (&self.nodes, Kind::CiliumNode),
        ]
    }
}

/// Overlay join of a CiliumEndpoint to a CiliumIdentity. Empty uid cannot join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityJoin {
    pub uid: String,
    pub namespace: String,
    pub name: String,
    pub identity_id: i64,
    pub labels: BTreeMap<String, String>,
}

/// Observed traffic. Built from Prometheus series already in hand.
/// Never a policy stand-in. Hubble Relay is not spoken to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedEdge {
    pub from: String,
    pub to: String,
    pub because: TelemetryReason,
}

impl From<&ObservedReach> for ObservedEdge {
    fn from(edge: &ObservedReach) -> Self {
        ObservedEdge {
            from: edge.from.clone(),
            to: edge.to.clone(),
            because: edge.because.clone(),
        }
    }
}

impl From<ObservedEdge> for ObservedReach {
    fn from(edge: ObservedEdge) -> Self {
        ObservedReach {
            from: edge.from,
            to: edge.to,
            because: edge.because,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observed {
    pub edges: Vec<ObservedEdge>,
    pub truncated: bool,
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

enum PageError {
    TooLarge,
    NotJson(String),
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

fn group_url() -> String {
    format!("/apis/{GROUP}")
}

fn collection_url(kind: Kind, version: &str, namespace: Option<&str>) -> String {
    let mut path = format!("/apis/{GROUP}/{version}");
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
    if out.is_empty() {
        out.push(FALLBACK_VERSION.to_string());
    }
    out
}

fn parse_list(text: &str) -> Result<WireList, PageError> {
    if text.len() > MAX_PAGE_BYTES {
        return Err(PageError::TooLarge);
    }
    serde_json::from_str(text).map_err(|error| PageError::NotJson(error.to_string()))
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn i64_field(value: &Value, key: &str) -> Option<i64> {
    let field = value.get(key)?;
    field
        .as_i64()
        .or_else(|| field.as_u64().and_then(|n| i64::try_from(n).ok()))
        .or_else(|| field.as_str()?.parse().ok())
}

fn ingest_labels(value: &Value) -> (BTreeMap<String, String>, bool) {
    let Some(map) = value.as_object() else {
        return (BTreeMap::new(), false);
    };
    let mut labels = BTreeMap::new();
    let mut truncated = false;
    for (key, item) in map {
        if labels.len() == MAX_LABELS {
            truncated = true;
            break;
        }
        let Some(value) = item.as_str() else {
            continue;
        };
        labels.insert(clipped(key.clone()), clipped(value.to_string()));
    }
    (labels, truncated)
}

fn labels_from_identity_status(status: &Value) -> (BTreeMap<String, String>, bool) {
    let Some(items) = status.pointer("/identity/labels").and_then(Value::as_array) else {
        return (BTreeMap::new(), false);
    };
    let mut labels = BTreeMap::new();
    let mut truncated = false;
    for item in items {
        if labels.len() == MAX_LABELS {
            truncated = true;
            break;
        }
        let Some(raw) = item.as_str() else {
            continue;
        };
        let (key, value) = match raw.split_once('=') {
            Some((key, value)) => (key, value),
            None => (raw, ""),
        };
        labels.insert(clipped(key.to_string()), clipped(value.to_string()));
    }
    (labels, truncated)
}

fn namespace_from_labels(labels: &BTreeMap<String, String>) -> String {
    labels
        .get(NS_LABEL)
        .or_else(|| labels.get(&format!("k8s:{NS_LABEL}")))
        .cloned()
        .unwrap_or_default()
}

fn selector_summary(spec: &Value) -> String {
    let selector = spec.get("endpointSelector").unwrap_or(&Value::Null);
    let Some(labels) = selector.get("matchLabels").and_then(Value::as_object) else {
        if selector
            .get("matchExpressions")
            .and_then(Value::as_array)
            .is_some_and(|e| !e.is_empty())
        {
            return "matchExpressions".to_string();
        }
        return "*".to_string();
    };
    if labels.is_empty() {
        return "*".to_string();
    }
    let mut parts: Vec<String> = labels
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| format!("{key}={value}")))
        .collect();
    parts.sort();
    clipped(parts.join(","))
}

fn rule_count(spec: &Value, key: &str) -> usize {
    spec.get(key)
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

fn declares_l7(spec: &Value) -> bool {
    spec.get("ingress")
        .and_then(Value::as_array)
        .into_iter()
        .chain(spec.get("egress").and_then(Value::as_array))
        .flatten()
        .any(|rule| {
            rule.pointer("/toPorts")
                .and_then(Value::as_array)
                .is_some_and(|ports| {
                    ports.iter().any(|port| {
                        port.pointer("/rules/http")
                            .and_then(Value::as_array)
                            .is_some_and(|http| !http.is_empty())
                    })
                })
        })
}

/// A CNP carries rules in `spec`, `specs`, or both, the same shape
/// [`parse_policy_document`] compiles. The row sums across all of them.
fn policy_detail(value: &Value) -> String {
    let specs: Vec<&Value> = value
        .get("spec")
        .into_iter()
        .chain(
            value
                .get("specs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
        .collect();
    let ingress: usize = specs.iter().map(|spec| rule_count(spec, "ingress")).sum();
    let egress: usize = specs.iter().map(|spec| rule_count(spec, "egress")).sum();
    let mut detail = format!("{ingress} ingress, {egress} egress");
    if specs.iter().any(|spec| declares_l7(spec)) {
        detail.push_str(", declared L7 HTTP");
    }
    let selector = specs
        .iter()
        .find(|spec| spec.get("endpointSelector").is_some())
        .copied()
        .or_else(|| specs.first().copied())
        .map(selector_summary)
        .unwrap_or_else(|| "*".to_string());
    if !selector.is_empty() {
        detail.push_str("  ");
        detail.push_str(&selector);
    }
    clipped(detail)
}

fn endpoint_address(status: &Value) -> String {
    let mut ips = Vec::new();
    if let Some(rows) = status
        .pointer("/networking/addressing")
        .and_then(Value::as_array)
    {
        for row in rows {
            for key in ["ipv4", "ipv6"] {
                let ip = str_field(row, key);
                if !ip.is_empty() && !ips.iter().any(|have: &String| have == ip) {
                    ips.push(ip.to_string());
                }
            }
        }
    }
    clipped(ips.join(","))
}

fn node_address(spec: &Value) -> String {
    let mut ips = Vec::new();
    if let Some(rows) = spec.get("addresses").and_then(Value::as_array) {
        for row in rows {
            let ip = str_field(row, "ip");
            if ip.is_empty() {
                continue;
            }
            if !ips.iter().any(|have: &String| have == ip) {
                ips.push(ip.to_string());
            }
            if ips.len() == 8 {
                break;
            }
        }
    }
    clipped(ips.join(","))
}

fn parse_item(kind: Kind, version: &str, value: &Value) -> Option<(Resource, bool)> {
    let meta = value.get("metadata")?;
    let name = clipped(str_field(meta, "name").to_string());
    if name.is_empty() {
        return None;
    }
    let uid = clipped(str_field(meta, "uid").to_string());
    let spec = value.get("spec").unwrap_or(&Value::Null);
    let status = value.get("status").unwrap_or(&Value::Null);
    let mut labels_truncated = false;
    let (namespace, identity_id, address, detail, labels) = match kind {
        Kind::CiliumNetworkPolicy | Kind::CiliumClusterwideNetworkPolicy => {
            let namespace = if kind == Kind::CiliumClusterwideNetworkPolicy {
                String::new()
            } else {
                clipped(str_field(meta, "namespace").to_string())
            };
            (
                namespace,
                None,
                String::new(),
                policy_detail(value),
                BTreeMap::new(),
            )
        }
        Kind::CiliumIdentity => {
            let source = value
                .get("security-labels")
                .filter(|labels| labels.is_object())
                .or_else(|| meta.get("labels"))
                .unwrap_or(&Value::Null);
            let (labels, truncated) = ingest_labels(source);
            labels_truncated = truncated;
            let identity_id = name.parse().ok();
            let namespace = clipped(namespace_from_labels(&labels));
            let detail = clipped(
                labels
                    .iter()
                    .take(4)
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            (namespace, identity_id, String::new(), detail, labels)
        }
        Kind::CiliumEndpoint => {
            let (labels, truncated) = labels_from_identity_status(status);
            labels_truncated = truncated;
            let identity_id = status.pointer("/identity/id").and_then(|id| {
                id.as_i64()
                    .or_else(|| id.as_u64().and_then(|n| i64::try_from(n).ok()))
                    .or_else(|| id.as_str()?.parse().ok())
            });
            (
                clipped(str_field(meta, "namespace").to_string()),
                identity_id,
                endpoint_address(status),
                clipped(str_field(status, "state").to_string()),
                labels,
            )
        }
        Kind::CiliumNode => {
            let identity_id = i64_field(spec, "nodeidentity");
            let address = node_address(spec);
            let detail = match identity_id {
                Some(id) => format!("node identity {id}"),
                None => String::new(),
            };
            (
                String::new(),
                identity_id,
                address,
                clipped(detail),
                BTreeMap::new(),
            )
        }
    };
    Some((
        Resource {
            kind,
            version: version.to_string(),
            name,
            namespace,
            uid,
            identity_id,
            address,
            detail,
            labels,
        },
        labels_truncated,
    ))
}

async fn probe_group(client: &Client) -> GroupAnswer {
    let request = match http::Request::get(group_url()).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    match client.request::<WireGroup>(request).await {
        Ok(group) => GroupAnswer::Served(order_versions(
            &group.preferred.version,
            group
                .versions
                .into_iter()
                .map(|item| item.version)
                .collect(),
        )),
        Err(error) => after_group(&error),
    }
}

async fn list_at_version(
    client: &Client,
    kind: Kind,
    version: &str,
    namespace: Option<&str>,
) -> Result<(KindSet, Vec<CiliumPolicy>), ListErr> {
    let path = collection_url(kind, version, namespace);
    let mut items = Vec::new();
    let mut policies = Vec::new();
    let mut unreadable = 0usize;
    let mut token: Option<String> = None;
    let mut truncated = false;
    let mut labels_clipped = false;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = match Request::new(path.clone()).list(&params) {
            Ok(request) => request,
            Err(error) => return Err(ListErr::Failed(error.to_string())),
        };
        let text = match client.request_text(request).await {
            Ok(text) => text,
            Err(error) if items.is_empty() && unreadable == 0 => return Err(after_list(&error)),
            Err(error) => {
                return Err(ListErr::Failed(crate::connect::describe(
                    &error as &(dyn std::error::Error + 'static),
                )));
            }
        };
        let page = match parse_list(&text) {
            Ok(page) => page,
            Err(PageError::TooLarge) => {
                return Err(ListErr::Failed(
                    "the list page is larger than 8 MiB; the page is not shown".to_string(),
                ));
            }
            Err(PageError::NotJson(why)) => {
                return Err(ListErr::Failed(format!("the list is not JSON: {why}")));
            }
        };
        for value in page.items {
            if items.len() == MAX_OBJECTS {
                truncated = true;
                break;
            }
            match parse_item(kind, version, &value) {
                Some((resource, labels_truncated)) => {
                    labels_clipped |= labels_truncated;
                    if matches!(
                        kind,
                        Kind::CiliumNetworkPolicy | Kind::CiliumClusterwideNetworkPolicy
                    ) {
                        policies.extend(parse_policy_document(&value));
                    }
                    items.push(resource);
                }
                None => unreadable += 1,
            }
        }
        token = (!page.metadata.cont.is_empty() && !truncated).then_some(page.metadata.cont);
        if token.is_none() {
            break;
        }
    }
    Ok((
        KindSet::Served {
            items,
            truncated,
            labels_clipped,
            unreadable,
        },
        policies,
    ))
}

async fn list_kind(
    client: &Client,
    kind: Kind,
    versions: &[String],
    namespace: Option<&str>,
) -> Result<(KindSet, Vec<CiliumPolicy>), Fetched<Inventory>> {
    for version in versions {
        match list_at_version(client, kind, version, namespace).await {
            Ok(set) => return Ok(set),
            Err(ListErr::NotFound) => continue,
            Err(ListErr::Denied) => return Ok((KindSet::Denied, Vec::new())),
            Err(ListErr::Failed(why)) => {
                return Err(Fetched::Failed {
                    what: kind.what(),
                    why,
                });
            }
        }
    }
    Ok((KindSet::NotServed, Vec::new()))
}

/// List the five Cilium kinds. A missing group is invisible; a forbidden one
/// is Denied on every kind and does not hide behind served: false.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let versions = match probe_group(client).await {
        GroupAnswer::NotServed => {
            return Fetched::Ok(Inventory::default());
        }
        GroupAnswer::Denied => {
            return Fetched::Ok(Inventory {
                network_policies: KindSet::Denied,
                cluster_policies: KindSet::Denied,
                identities: KindSet::Denied,
                endpoints: KindSet::Denied,
                nodes: KindSet::Denied,
                declared: Declared::default(),
            });
        }
        GroupAnswer::Failed(why) => {
            return Fetched::Failed {
                what: "cilium",
                why,
            };
        }
        GroupAnswer::Served(versions) => versions,
    };

    let kinds = [
        Kind::CiliumNetworkPolicy,
        Kind::CiliumClusterwideNetworkPolicy,
        Kind::CiliumIdentity,
        Kind::CiliumEndpoint,
        Kind::CiliumNode,
    ];
    let mut sets = Vec::with_capacity(kinds.len());
    let mut compiled = Vec::new();
    // A namespaced fetch lists CNPs in one namespace only, so the compiled
    // set structurally cannot be the whole cluster's policies.
    let mut policies_partial = namespace.is_some();
    for kind in kinds {
        let ns = if kind.namespaced() { namespace } else { None };
        match list_kind(client, kind, &versions, ns).await {
            Ok((set, policies)) => {
                if matches!(
                    kind,
                    Kind::CiliumNetworkPolicy | Kind::CiliumClusterwideNetworkPolicy
                ) {
                    policies_partial |= match &set {
                        KindSet::Denied => true,
                        KindSet::Served {
                            truncated,
                            unreadable,
                            ..
                        } => *truncated || *unreadable > 0,
                        KindSet::NotServed => false,
                    };
                    compiled.extend(policies);
                }
                sets.push(set);
            }
            Err(failed) => return failed,
        }
    }
    let mut sets = sets.into_iter();
    let mut declared = declare(&compiled);
    if policies_partial {
        declared.mark_incomplete();
    }
    Fetched::Ok(Inventory {
        network_policies: sets.next().unwrap_or_default(),
        cluster_policies: sets.next().unwrap_or_default(),
        identities: sets.next().unwrap_or_default(),
        endpoints: sets.next().unwrap_or_default(),
        nodes: sets.next().unwrap_or_default(),
        declared,
    })
}

/// CiliumEndpoint uid plus CiliumIdentity labels. Empty uid cannot join.
pub fn join_identities(inventory: &Inventory) -> Vec<IdentityJoin> {
    identity_joins(inventory.endpoints.items(), inventory.identities.items())
}

pub fn identity_joins(endpoints: &[Resource], identities: &[Resource]) -> Vec<IdentityJoin> {
    let mut out = Vec::new();
    for endpoint in endpoints {
        if endpoint.uid.is_empty() {
            continue;
        }
        let Some(identity_id) = endpoint.identity_id else {
            continue;
        };
        let Some(identity) = identities.iter().find(|identity| {
            identity.identity_id == Some(identity_id) || identity.name == identity_id.to_string()
        }) else {
            continue;
        };
        out.push(IdentityJoin {
            uid: endpoint.uid.clone(),
            namespace: endpoint.namespace.clone(),
            name: endpoint.name.clone(),
            identity_id,
            labels: identity.labels.clone(),
        });
    }
    out
}

/// Hubble-only edges from a Prometheus result already in hand. Does not scrape.
pub fn observed_from_query(result: &QueryResult) -> Observed {
    let mut edges = Vec::new();
    let mut truncated = result.truncated;
    for series in &result.series {
        if edges.len() == MAX_OBSERVED_EDGES {
            truncated = true;
            break;
        }
        let Some(edge) = edge_from_labels(&series.labels) else {
            continue;
        };
        edges.push(edge);
    }
    Observed { edges, truncated }
}

/// Accept [`ObservedReach`] mesh.rs already produced. Hubble only: Istio and
/// Linkerd stay on that module's type.
pub fn observed_from_reach(edges: &[ObservedReach]) -> Observed {
    let mut out = Vec::new();
    let mut truncated = false;
    for edge in edges {
        if edge.because.exporter != TelemetryExporter::Hubble {
            continue;
        }
        if out.len() == MAX_OBSERVED_EDGES {
            truncated = true;
            break;
        }
        out.push(ObservedEdge::from(edge));
    }
    Observed {
        edges: out,
        truncated,
    }
}

fn edge_from_labels(labels: &[(String, String)]) -> Option<ObservedEdge> {
    let metric = label(labels, "__name__");
    if let Some(name) = metric {
        if !name.starts_with("hubble_") {
            return None;
        }
    } else if !has_hubble_shape(labels) {
        return None;
    }
    let from = endpoint_label(
        labels,
        &["source", "source_pod", "source_workload", "source_identity"],
        "source_namespace",
        "source_pod",
        "source_identity",
    )?;
    let to = endpoint_label(
        labels,
        &[
            "destination",
            "destination_pod",
            "destination_workload",
            "destination_identity",
        ],
        "destination_namespace",
        "destination_pod",
        "destination_identity",
    )?;
    Some(ObservedEdge {
        from,
        to,
        because: TelemetryReason {
            metric: metric.unwrap_or("hubble_flows_processed_total").to_string(),
            exporter: TelemetryExporter::Hubble,
        },
    })
}

fn has_hubble_shape(labels: &[(String, String)]) -> bool {
    label(labels, "source_identity").is_some()
        || label(labels, "destination_identity").is_some()
        || (label(labels, "source").is_some() && label(labels, "destination").is_some())
}

fn endpoint_label(
    labels: &[(String, String)],
    keys: &[&str],
    namespace_key: &str,
    pod_key: &str,
    identity_key: &str,
) -> Option<String> {
    if let (Some(namespace), Some(pod)) = (label(labels, namespace_key), label(labels, pod_key)) {
        return Some(format!("{namespace}/{pod}"));
    }
    for key in keys {
        if let Some(value) = label(labels, key) {
            if *key == identity_key {
                return Some(format!("identity:{value}"));
            }
            return Some(value.to_string());
        }
    }
    None
}

fn label<'a>(labels: &'a [(String, String)], name: &str) -> Option<&'a str> {
    labels
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

/// Native list rows. `None` when the group is not served, so a UI stays
/// invisible rather than opening an empty pane. A served empty inventory is
/// `Some`. A denied kind is a labelled row, not absence.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served() {
        return None;
    }
    let columns = ["Kind", "Name", "Namespace", "Identity", "Address", "Detail"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    for (set, kind) in inventory.sets() {
        truncated |= set.truncated();
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
                    ],
                    name: kind.as_str().to_string(),
                    namespace: None,
                    uid: format!("denied:{}", kind.as_str()),
                });
            }
            KindSet::Served { items, .. } => {
                for item in items {
                    let uid = if item.uid.is_empty() {
                        format!("{}/{}/{}", item.kind.as_str(), item.namespace, item.name)
                    } else {
                        item.uid.clone()
                    };
                    let identity = item
                        .identity_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    rows.push(TableRow {
                        cells: vec![
                            item.kind.as_str().to_string(),
                            item.name.clone(),
                            item.namespace.clone(),
                            identity,
                            item.address.clone(),
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

/// The inventory as a document, rendered here so a test can gate the words.
pub fn render(inventory: &Inventory) -> Vec<String> {
    let sets = inventory.sets();
    if sets
        .iter()
        .all(|(set, _)| matches!(set, KindSet::NotServed))
    {
        return vec![
            "Cilium is not served by this cluster".to_string(),
            String::new(),
            "this reads CiliumNetworkPolicy, CiliumClusterwideNetworkPolicy, CiliumIdentity, \
             CiliumEndpoint and CiliumNode CRs the cluster already publishes; nothing is \
             installed to find them, so a cluster without Cilium shows as empty here"
                .to_string(),
        ];
    }

    let total: usize = sets.iter().map(|(set, _)| set.items().len()).sum();
    let unreadable: usize = sets.iter().map(|(set, _)| set.unreadable()).sum();
    let truncated = sets.iter().any(|(set, _)| set.truncated());
    let denied = sets
        .iter()
        .filter(|(set, _)| matches!(set, KindSet::Denied))
        .count();

    let mut lines = Vec::new();
    if total == 0 && unreadable == 0 && denied == 0 {
        lines.push("no Cilium objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no Cilium object could be read here, though some are stored: every object this \
             account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!("{} Cilium {}", total, plural(total, "object")));
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
    if sets.iter().any(|(set, _)| set.labels_clipped()) {
        lines.push(format!(
            "some objects carry more than {MAX_LABELS} labels and show only their first \
             {MAX_LABELS}",
        ));
    }
    if unreadable > 0 && total > 0 {
        lines.push(format!(
            "{} Cilium {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    lines.push(
        "declared Cilium policy is compiled from the CRs; observed traffic is Prometheus \
         series already in hand. those answers are not mixed"
            .to_string(),
    );
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            let head = if item.namespace.is_empty() {
                item.name.clone()
            } else {
                format!("{}/{}", item.namespace, item.name)
            };
            lines.push(head);
            let mut line = format!("  {}", item.kind.as_str());
            if let Some(id) = item.identity_id {
                line.push_str(&format!("  identity {id}"));
            }
            if !item.address.is_empty() {
                line.push_str("  ");
                line.push_str(&item.address);
            }
            if !item.detail.is_empty() {
                line.push_str("  ");
                line.push_str(&item.detail);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "cilium_test.rs"]
mod tests;
