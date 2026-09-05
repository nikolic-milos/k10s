//! Cilium control-plane CRs already published on `cilium.io`.
//!
//! This is inventory, not policy and not Hubble. CiliumNetworkPolicy,
//! CiliumClusterwideNetworkPolicy, CiliumEndpoint, CiliumIdentity, and
//! CiliumNode are listed elsewhere. Gateway API objects live on
//! `gateway.networking.k8s.io` and are not read here; CiliumGatewayClassConfig
//! is Cilium's own implementation config and is.
//!
//! Probe `/apis/cilium.io`, then each `/apis/cilium.io/<version>` document.
//! A kind that document does not name is skipped, not a failure. A 404 on the
//! group is [`GroupState::NotServed`]: invisible, not broken. A 403 is
//! [`GroupState::Denied`]. A 403 on one version document while another serves
//! leaves every kind still unanswered [`KindSet::Denied`], never absent.
//! Nothing is installed to find these CRs, and Envoy
//! `spec.resources` (including `typed_config`) is never copied into a
//! [`Resource`], an [`Inventory`] Debug line, or a table cell.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;
use crate::served::{GroupAnswer, ListErr, after_group, after_list};

pub const GROUP: &str = "cilium.io";

const PAGE_LIMIT: u32 = 200;
pub const MAX_OBJECTS: usize = 2_000;
pub const MAX_FIELD_CHARS: usize = 200;
pub const MAX_PAGE_BYTES: usize = 8 << 20;
const MAX_IDENTITY_REFS: usize = 8;
const FALLBACK_VERSIONS: [&str; 2] = ["v2", "v2alpha1"];

/// The cilium.io kinds this inventory will list when the version document
/// names them. Policy, identity, endpoint, and node kinds are not in this set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    CiliumEnvoyConfig,
    CiliumClusterwideEnvoyConfig,
    CiliumLocalRedirectPolicy,
    CiliumEgressGatewayPolicy,
    CiliumExternalWorkload,
    CiliumCIDRGroup,
    CiliumL2AnnouncementPolicy,
    CiliumLoadBalancerIPPool,
    CiliumPodIPPool,
    CiliumNodeConfig,
    CiliumEndpointSlice,
    CiliumBGPClusterConfig,
    CiliumBGPPeerConfig,
    CiliumBGPAdvertisement,
    CiliumBGPNodeConfig,
    CiliumBGPNodeConfigOverride,
    CiliumBGPPeeringPolicy,
    CiliumGatewayClassConfig,
}

impl Kind {
    pub const ALL: [Kind; 18] = [
        Kind::CiliumEnvoyConfig,
        Kind::CiliumClusterwideEnvoyConfig,
        Kind::CiliumLocalRedirectPolicy,
        Kind::CiliumEgressGatewayPolicy,
        Kind::CiliumExternalWorkload,
        Kind::CiliumCIDRGroup,
        Kind::CiliumL2AnnouncementPolicy,
        Kind::CiliumLoadBalancerIPPool,
        Kind::CiliumPodIPPool,
        Kind::CiliumNodeConfig,
        Kind::CiliumEndpointSlice,
        Kind::CiliumBGPClusterConfig,
        Kind::CiliumBGPPeerConfig,
        Kind::CiliumBGPAdvertisement,
        Kind::CiliumBGPNodeConfig,
        Kind::CiliumBGPNodeConfigOverride,
        Kind::CiliumBGPPeeringPolicy,
        Kind::CiliumGatewayClassConfig,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::CiliumEnvoyConfig => "CiliumEnvoyConfig",
            Kind::CiliumClusterwideEnvoyConfig => "CiliumClusterwideEnvoyConfig",
            Kind::CiliumLocalRedirectPolicy => "CiliumLocalRedirectPolicy",
            Kind::CiliumEgressGatewayPolicy => "CiliumEgressGatewayPolicy",
            Kind::CiliumExternalWorkload => "CiliumExternalWorkload",
            Kind::CiliumCIDRGroup => "CiliumCIDRGroup",
            Kind::CiliumL2AnnouncementPolicy => "CiliumL2AnnouncementPolicy",
            Kind::CiliumLoadBalancerIPPool => "CiliumLoadBalancerIPPool",
            Kind::CiliumPodIPPool => "CiliumPodIPPool",
            Kind::CiliumNodeConfig => "CiliumNodeConfig",
            Kind::CiliumEndpointSlice => "CiliumEndpointSlice",
            Kind::CiliumBGPClusterConfig => "CiliumBGPClusterConfig",
            Kind::CiliumBGPPeerConfig => "CiliumBGPPeerConfig",
            Kind::CiliumBGPAdvertisement => "CiliumBGPAdvertisement",
            Kind::CiliumBGPNodeConfig => "CiliumBGPNodeConfig",
            Kind::CiliumBGPNodeConfigOverride => "CiliumBGPNodeConfigOverride",
            Kind::CiliumBGPPeeringPolicy => "CiliumBGPPeeringPolicy",
            Kind::CiliumGatewayClassConfig => "CiliumGatewayClassConfig",
        }
    }

    /// The kind name as the APIResourceList spells it, or none if this
    /// module does not inventory that kind.
    pub fn from_api_kind(name: &str) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|kind| kind.as_str() == name)
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::CiliumEnvoyConfig => "ciliumenvoyconfigs",
            Kind::CiliumClusterwideEnvoyConfig => "ciliumclusterwideenvoyconfigs",
            Kind::CiliumLocalRedirectPolicy => "ciliumlocalredirectpolicies",
            Kind::CiliumEgressGatewayPolicy => "ciliumegressgatewaypolicies",
            Kind::CiliumExternalWorkload => "ciliumexternalworkloads",
            Kind::CiliumCIDRGroup => "ciliumcidrgroups",
            Kind::CiliumL2AnnouncementPolicy => "ciliuml2announcementpolicies",
            Kind::CiliumLoadBalancerIPPool => "ciliumloadbalancerippools",
            Kind::CiliumPodIPPool => "ciliumpodippools",
            Kind::CiliumNodeConfig => "ciliumnodeconfigs",
            Kind::CiliumEndpointSlice => "ciliumendpointslices",
            Kind::CiliumBGPClusterConfig => "ciliumbgpclusterconfigs",
            Kind::CiliumBGPPeerConfig => "ciliumbgppeerconfigs",
            Kind::CiliumBGPAdvertisement => "ciliumbgpadvertisements",
            Kind::CiliumBGPNodeConfig => "ciliumbgpnodeconfigs",
            Kind::CiliumBGPNodeConfigOverride => "ciliumbgpnodeconfigoverrides",
            Kind::CiliumBGPPeeringPolicy => "ciliumbgppeeringpolicies",
            Kind::CiliumGatewayClassConfig => "ciliumgatewayclassconfigs",
        }
    }

    /// Fallback when a caller parses an object without a version document.
    /// Versions per the Cilium 1.18 registers: only the kinds v2's
    /// register.go does not name are still v2alpha1.
    pub fn version(self) -> &'static str {
        match self {
            Kind::CiliumL2AnnouncementPolicy
            | Kind::CiliumPodIPPool
            | Kind::CiliumEndpointSlice
            | Kind::CiliumBGPPeeringPolicy
            | Kind::CiliumGatewayClassConfig => "v2alpha1",
            _ => "v2",
        }
    }

    /// Scope per the upstream kubebuilder markers. CiliumEndpointSlice is
    /// `scope="Cluster"` even though the endpoints it groups are namespaced.
    pub fn namespaced(self) -> bool {
        matches!(
            self,
            Kind::CiliumEnvoyConfig | Kind::CiliumLocalRedirectPolicy | Kind::CiliumNodeConfig
        )
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::CiliumEnvoyConfig => "cilium envoyconfigs",
            Kind::CiliumClusterwideEnvoyConfig => "cilium clusterwide envoyconfigs",
            Kind::CiliumLocalRedirectPolicy => "cilium local redirect policies",
            Kind::CiliumEgressGatewayPolicy => "cilium egress gateway policies",
            Kind::CiliumExternalWorkload => "cilium external workloads",
            Kind::CiliumCIDRGroup => "cilium cidr groups",
            Kind::CiliumL2AnnouncementPolicy => "cilium l2 announcement policies",
            Kind::CiliumLoadBalancerIPPool => "cilium load balancer ip pools",
            Kind::CiliumPodIPPool => "cilium pod ip pools",
            Kind::CiliumNodeConfig => "cilium node configs",
            Kind::CiliumEndpointSlice => "cilium endpoint slices",
            Kind::CiliumBGPClusterConfig => "cilium bgp cluster configs",
            Kind::CiliumBGPPeerConfig => "cilium bgp peer configs",
            Kind::CiliumBGPAdvertisement => "cilium bgp advertisements",
            Kind::CiliumBGPNodeConfig => "cilium bgp node configs",
            Kind::CiliumBGPNodeConfigOverride => "cilium bgp node config overrides",
            Kind::CiliumBGPPeeringPolicy => "cilium bgp peering policies",
            Kind::CiliumGatewayClassConfig => "cilium gateway class configs",
        }
    }
}

/// One CR, reduced to what an inventory shows. Envoy resources and secret
/// values are not fields on this type, so they cannot appear in Debug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    /// Kind-specific line: selectors, counts, clipped blocks. Never JSON.
    pub note: String,
}

/// What one kind's list answered.
///
/// A kind the version document did not name stays [`KindSet::NotServed`]:
/// skip, not fail. A 403 on that list is [`KindSet::Denied`].
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

/// The `/apis/cilium.io` answer. Version documents and lists are finer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupState {
    Served,
    #[default]
    NotServed,
    Denied,
}

impl GroupState {
    pub fn is_served(self) -> bool {
        !matches!(self, GroupState::NotServed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub group: GroupState,
    pub envoy_configs: KindSet,
    pub clusterwide_envoy_configs: KindSet,
    pub local_redirect_policies: KindSet,
    pub egress_gateway_policies: KindSet,
    pub external_workloads: KindSet,
    pub cidr_groups: KindSet,
    pub l2_announcement_policies: KindSet,
    pub load_balancer_ip_pools: KindSet,
    pub pod_ip_pools: KindSet,
    pub node_configs: KindSet,
    pub endpoint_slices: KindSet,
    pub bgp_cluster_configs: KindSet,
    pub bgp_peer_configs: KindSet,
    pub bgp_advertisements: KindSet,
    pub bgp_node_configs: KindSet,
    pub bgp_node_config_overrides: KindSet,
    pub bgp_peering_policies: KindSet,
    pub gateway_class_configs: KindSet,
}

impl Inventory {
    /// False when `/apis/cilium.io` answered 404. A served group with none of
    /// our kinds in the version document is still served: the table is empty,
    /// not absent.
    pub fn served(&self) -> bool {
        self.group.is_served()
    }

    pub fn of(&self, kind: Kind) -> &KindSet {
        match kind {
            Kind::CiliumEnvoyConfig => &self.envoy_configs,
            Kind::CiliumClusterwideEnvoyConfig => &self.clusterwide_envoy_configs,
            Kind::CiliumLocalRedirectPolicy => &self.local_redirect_policies,
            Kind::CiliumEgressGatewayPolicy => &self.egress_gateway_policies,
            Kind::CiliumExternalWorkload => &self.external_workloads,
            Kind::CiliumCIDRGroup => &self.cidr_groups,
            Kind::CiliumL2AnnouncementPolicy => &self.l2_announcement_policies,
            Kind::CiliumLoadBalancerIPPool => &self.load_balancer_ip_pools,
            Kind::CiliumPodIPPool => &self.pod_ip_pools,
            Kind::CiliumNodeConfig => &self.node_configs,
            Kind::CiliumEndpointSlice => &self.endpoint_slices,
            Kind::CiliumBGPClusterConfig => &self.bgp_cluster_configs,
            Kind::CiliumBGPPeerConfig => &self.bgp_peer_configs,
            Kind::CiliumBGPAdvertisement => &self.bgp_advertisements,
            Kind::CiliumBGPNodeConfig => &self.bgp_node_configs,
            Kind::CiliumBGPNodeConfigOverride => &self.bgp_node_config_overrides,
            Kind::CiliumBGPPeeringPolicy => &self.bgp_peering_policies,
            Kind::CiliumGatewayClassConfig => &self.gateway_class_configs,
        }
    }

    /// Kinds the version document named, or whose list was denied.
    pub fn kinds(&self) -> Vec<Kind> {
        self.sets()
            .into_iter()
            .filter(|(set, _)| set.served())
            .map(|(_, kind)| kind)
            .collect()
    }

    fn of_mut(&mut self, kind: Kind) -> &mut KindSet {
        match kind {
            Kind::CiliumEnvoyConfig => &mut self.envoy_configs,
            Kind::CiliumClusterwideEnvoyConfig => &mut self.clusterwide_envoy_configs,
            Kind::CiliumLocalRedirectPolicy => &mut self.local_redirect_policies,
            Kind::CiliumEgressGatewayPolicy => &mut self.egress_gateway_policies,
            Kind::CiliumExternalWorkload => &mut self.external_workloads,
            Kind::CiliumCIDRGroup => &mut self.cidr_groups,
            Kind::CiliumL2AnnouncementPolicy => &mut self.l2_announcement_policies,
            Kind::CiliumLoadBalancerIPPool => &mut self.load_balancer_ip_pools,
            Kind::CiliumPodIPPool => &mut self.pod_ip_pools,
            Kind::CiliumNodeConfig => &mut self.node_configs,
            Kind::CiliumEndpointSlice => &mut self.endpoint_slices,
            Kind::CiliumBGPClusterConfig => &mut self.bgp_cluster_configs,
            Kind::CiliumBGPPeerConfig => &mut self.bgp_peer_configs,
            Kind::CiliumBGPAdvertisement => &mut self.bgp_advertisements,
            Kind::CiliumBGPNodeConfig => &mut self.bgp_node_configs,
            Kind::CiliumBGPNodeConfigOverride => &mut self.bgp_node_config_overrides,
            Kind::CiliumBGPPeeringPolicy => &mut self.bgp_peering_policies,
            Kind::CiliumGatewayClassConfig => &mut self.gateway_class_configs,
        }
    }

    fn sets(&self) -> [(&KindSet, Kind); 18] {
        [
            (&self.envoy_configs, Kind::CiliumEnvoyConfig),
            (
                &self.clusterwide_envoy_configs,
                Kind::CiliumClusterwideEnvoyConfig,
            ),
            (
                &self.local_redirect_policies,
                Kind::CiliumLocalRedirectPolicy,
            ),
            (
                &self.egress_gateway_policies,
                Kind::CiliumEgressGatewayPolicy,
            ),
            (&self.external_workloads, Kind::CiliumExternalWorkload),
            (&self.cidr_groups, Kind::CiliumCIDRGroup),
            (
                &self.l2_announcement_policies,
                Kind::CiliumL2AnnouncementPolicy,
            ),
            (&self.load_balancer_ip_pools, Kind::CiliumLoadBalancerIPPool),
            (&self.pod_ip_pools, Kind::CiliumPodIPPool),
            (&self.node_configs, Kind::CiliumNodeConfig),
            (&self.endpoint_slices, Kind::CiliumEndpointSlice),
            (&self.bgp_cluster_configs, Kind::CiliumBGPClusterConfig),
            (&self.bgp_peer_configs, Kind::CiliumBGPPeerConfig),
            (&self.bgp_advertisements, Kind::CiliumBGPAdvertisement),
            (&self.bgp_node_configs, Kind::CiliumBGPNodeConfig),
            (
                &self.bgp_node_config_overrides,
                Kind::CiliumBGPNodeConfigOverride,
            ),
            (&self.bgp_peering_policies, Kind::CiliumBGPPeeringPolicy),
            (&self.gateway_class_configs, Kind::CiliumGatewayClassConfig),
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

#[derive(Deserialize, Default)]
struct WireResourceList {
    #[serde(default)]
    resources: Vec<WireResource>,
}

#[derive(Deserialize, Default)]
struct WireResource {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    namespaced: bool,
    #[serde(default)]
    verbs: Vec<String>,
}

#[derive(Deserialize, Default)]
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

struct Named {
    kind: Kind,
    version: String,
    plural: String,
    namespaced: bool,
}

enum VersionAnswer {
    Served(Vec<WireResource>),
    NotFound,
    Denied,
    Failed(String),
}

fn after_version(error: &kube::Error) -> VersionAnswer {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return VersionAnswer::Denied;
        }
        if response.code == 404 {
            return VersionAnswer::NotFound;
        }
    }
    VersionAnswer::Failed(crate::connect::describe(
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

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn array_len(value: Option<&Value>) -> usize {
    value.and_then(Value::as_array).map(Vec::len).unwrap_or(0)
}

fn number_id(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(n) = value.as_u64() {
        return i64::try_from(n).ok();
    }
    value.as_str()?.parse().ok()
}

fn identity_of(endpoint: &Value) -> Option<i64> {
    number_id(endpoint.get("identityID"))
        .or_else(|| number_id(endpoint.get("id")))
        .or_else(|| number_id(endpoint.pointer("/identity/id")))
}

fn selector_text(value: Option<&Value>) -> String {
    let Some(selector) = value else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(labels) = selector.get("matchLabels").and_then(Value::as_object) {
        let mut pairs: Vec<String> = labels
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|value| format!("{key}={value}")))
            .collect();
        pairs.sort();
        parts.extend(pairs);
    }
    let expressions = array_len(selector.get("matchExpressions"));
    if expressions > 0 {
        parts.push(format!(
            "{expressions} {}",
            if expressions == 1 {
                "expression"
            } else {
                "expressions"
            }
        ));
    }
    clipped(parts.join(", "))
}

fn services_text(spec: &Value) -> String {
    let Some(services) = spec.get("services").and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for service in services {
        let name = str_field(service, "name");
        if name.is_empty() {
            continue;
        }
        let namespace = str_field(service, "namespace");
        if namespace.is_empty() {
            parts.push(name.to_string());
        } else {
            parts.push(format!("{namespace}/{name}"));
        }
    }
    clipped(parts.join(", "))
}

fn lrp_note(spec: &Value) -> String {
    let frontend = spec.get("redirectFrontend").unwrap_or(&Value::Null);
    if let Some(service) = frontend.get("serviceMatcher") {
        let name = str_field(service, "serviceName");
        if !name.is_empty() {
            let namespace = str_field(service, "namespace");
            return clipped(if namespace.is_empty() {
                name.to_string()
            } else {
                format!("{namespace}/{name}")
            });
        }
    }
    if let Some(address) = frontend.get("addressMatcher") {
        let ip = str_field(address, "ip");
        if !ip.is_empty() {
            return clipped(ip.to_string());
        }
    }
    String::new()
}

fn gateway_ip(spec: &Value, status: &Value) -> String {
    // Cilium declares the egress IP in the spec: `egressGateway.egressIP`,
    // or `egressGateways[0].egressIP` in the multi-gateway shape. The status
    // pointers are a fallback only; upstream publishes no status egress IP.
    for path in ["/egressGateway/egressIP", "/egressGateways/0/egressIP"] {
        if let Some(ip) = spec.pointer(path).and_then(Value::as_str)
            && !ip.is_empty()
        {
            return clipped(ip.to_string());
        }
    }
    for path in [
        "/egressIP",
        "/gatewayIP",
        "/egressGatewayIP",
        "/egressGateway/egressIP",
    ] {
        if let Some(ip) = status.pointer(path).and_then(Value::as_str)
            && !ip.is_empty()
        {
            return clipped(ip.to_string());
        }
    }
    String::new()
}

fn egp_note(spec: &Value, status: &Value) -> String {
    let mut parts = Vec::new();
    let selector = spec
        .pointer("/egressGateway/nodeSelector")
        .or_else(|| spec.pointer("/egressGateways/0/nodeSelector"))
        .or_else(|| spec.get("nodeSelector"));
    let nodes = selector_text(selector);
    if !nodes.is_empty() {
        parts.push(nodes);
    }
    if let Some(cidrs) = spec.get("destinationCIDRs").and_then(Value::as_array) {
        let text = cidrs
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if !text.is_empty() {
            parts.push(clipped(text));
        }
    }
    let ip = gateway_ip(spec, status);
    if !ip.is_empty() {
        parts.push(ip);
    }
    clipped(parts.join("  "))
}

fn external_note(spec: &Value, status: &Value) -> String {
    let ip = str_field(status, "ip");
    if !ip.is_empty() {
        return clipped(ip.to_string());
    }
    clipped(str_field(spec, "ipv4-alloc-cidr").to_string())
}

fn cidr_count_note(spec: &Value) -> String {
    let n = spec
        .get("externalCIDRs")
        .or_else(|| spec.get("cidrs"))
        .map(|value| array_len(Some(value)))
        .unwrap_or(0);
    if n == 1 {
        "1 CIDR".to_string()
    } else {
        format!("{n} CIDRs")
    }
}

fn l2_note(spec: &Value) -> String {
    let mut parts = Vec::new();
    let nodes = selector_text(spec.get("nodeSelector"));
    if !nodes.is_empty() {
        parts.push(nodes);
    }
    let services = selector_text(spec.get("serviceSelector"));
    if !services.is_empty() {
        parts.push(services);
    }
    if spec.get("externalIPs").and_then(Value::as_bool) == Some(true) {
        parts.push("externalIPs".to_string());
    }
    if spec.get("loadBalancerIPs").and_then(Value::as_bool) == Some(true) {
        parts.push("loadBalancerIPs".to_string());
    }
    clipped(parts.join("  "))
}

fn blocks_text(spec: &Value) -> String {
    let Some(blocks) = spec
        .get("blocks")
        .or_else(|| spec.get("cidrs"))
        .and_then(Value::as_array)
    else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in blocks {
        if let Some(cidr) = block.get("cidr").and_then(Value::as_str)
            && !cidr.is_empty()
        {
            parts.push(cidr.to_string());
            continue;
        }
        let start = str_field(block, "start");
        let stop = str_field(block, "stop");
        if !start.is_empty() && !stop.is_empty() {
            parts.push(format!("{start}-{stop}"));
        } else if !start.is_empty() {
            parts.push(start.to_string());
        }
    }
    clipped(parts.join(", "))
}

fn lb_pool_note(spec: &Value) -> String {
    let mut parts = Vec::new();
    let blocks = blocks_text(spec);
    if !blocks.is_empty() {
        parts.push(blocks);
    }
    if spec.get("disabled").and_then(Value::as_bool) == Some(true) {
        parts.push("disabled".to_string());
    }
    clipped(parts.join("  "))
}

fn pod_pool_note(spec: &Value) -> String {
    let v4 = array_len(spec.pointer("/ipv4/cidrs"));
    let v6 = array_len(spec.pointer("/ipv6/cidrs"));
    format!(
        "ipv4 {v4} {}, ipv6 {v6} {}",
        plural(v4, "CIDR"),
        plural(v6, "CIDR")
    )
}

fn endpoints_note(root: &Value) -> String {
    let Some(endpoints) = root
        .get("endpoints")
        .or_else(|| root.pointer("/spec/endpoints"))
        .and_then(Value::as_array)
    else {
        return "0 endpoints".to_string();
    };
    let count = endpoints.len();
    let mut ids = Vec::new();
    let mut more = false;
    for endpoint in endpoints {
        let Some(id) = identity_of(endpoint) else {
            continue;
        };
        if ids.contains(&id) {
            continue;
        }
        if ids.len() == MAX_IDENTITY_REFS {
            more = true;
            break;
        }
        ids.push(id);
    }
    let mut note = if count == 1 {
        "1 endpoint".to_string()
    } else {
        format!("{count} endpoints")
    };
    if !ids.is_empty() {
        note.push_str(", identities ");
        note.push_str(
            &ids.iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
        if more {
            note.push('\u{2026}');
        }
    }
    clipped(note)
}

fn bgp_cluster_note(spec: &Value) -> String {
    let mut parts = Vec::new();
    let nodes = selector_text(spec.get("nodeSelector"));
    if !nodes.is_empty() {
        parts.push(nodes);
    }
    if let Some(instances) = spec.get("bgpInstances").and_then(Value::as_array) {
        let mut asns = Vec::new();
        let mut peers = 0usize;
        for instance in instances {
            if let Some(asn) = instance.get("localASN").and_then(Value::as_i64) {
                asns.push(asn.to_string());
            }
            peers += array_len(instance.get("peers"));
        }
        if !asns.is_empty() {
            parts.push(format!("ASN {}", asns.join(", ")));
        }
        parts.push(format!(
            "{peers} {}",
            if peers == 1 { "peer" } else { "peers" }
        ));
    }
    clipped(parts.join("  "))
}

fn bgp_peer_note(spec: &Value) -> String {
    let mut parts = Vec::new();
    let secret = str_field(spec, "authSecretRef");
    if !secret.is_empty() {
        parts.push(format!("authSecretRef {secret}"));
    }
    if let Some(families) = spec.get("families").and_then(Value::as_array) {
        let fams: Vec<String> = families
            .iter()
            .filter_map(|family| {
                let afi = family.get("afi").and_then(Value::as_str)?;
                let safi = family.get("safi").and_then(Value::as_str).unwrap_or("");
                if safi.is_empty() {
                    Some(afi.to_string())
                } else {
                    Some(format!("{afi}/{safi}"))
                }
            })
            .collect();
        if !fams.is_empty() {
            parts.push(fams.join(", "));
        }
    }
    clipped(parts.join("  "))
}

fn bgp_adv_note(spec: &Value) -> String {
    let Some(advs) = spec.get("advertisements").and_then(Value::as_array) else {
        return String::new();
    };
    let types: Vec<String> = advs
        .iter()
        .filter_map(|item| {
            item.get("advertisementType")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    clipped(types.join(", "))
}

fn bgp_node_note(spec: &Value, status: &Value) -> String {
    if let Some(instances) = status.get("bgpInstances").and_then(Value::as_array) {
        let mut states = Vec::new();
        for instance in instances {
            let Some(peers) = instance.get("peers").and_then(Value::as_array) else {
                continue;
            };
            for peer in peers {
                let name = str_field(peer, "name");
                let state = str_field(peer, "peeringState");
                if !name.is_empty() && !state.is_empty() {
                    states.push(format!("{name}={state}"));
                } else if !state.is_empty() {
                    states.push(state.to_string());
                }
            }
        }
        if !states.is_empty() {
            return clipped(states.join(", "));
        }
    }
    let n = array_len(spec.get("bgpInstances"));
    if n == 0 {
        String::new()
    } else if n == 1 {
        "1 instance".to_string()
    } else {
        format!("{n} instances")
    }
}

fn bgp_node_override_note(spec: &Value) -> String {
    let Some(instances) = spec.get("bgpInstances").and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for instance in instances {
        let router_id = str_field(instance, "routerID");
        if !router_id.is_empty() {
            parts.push(format!("routerID {router_id}"));
        }
        if let Some(port) = instance.get("localPort").and_then(Value::as_i64) {
            parts.push(format!("localPort {port}"));
        }
    }
    clipped(parts.join(", "))
}

fn bgp_peering_note(spec: &Value) -> String {
    let mut parts = Vec::new();
    let nodes = selector_text(spec.get("nodeSelector"));
    if !nodes.is_empty() {
        parts.push(nodes);
    }
    if let Some(routers) = spec.get("virtualRouters").and_then(Value::as_array) {
        let mut asns = Vec::new();
        let mut neighbors = 0usize;
        for router in routers {
            if let Some(asn) = router.get("localASN").and_then(Value::as_i64) {
                asns.push(asn.to_string());
            }
            neighbors += array_len(router.get("neighbors"));
        }
        if !asns.is_empty() {
            parts.push(format!("ASN {}", asns.join(", ")));
        }
        parts.push(format!(
            "{neighbors} {}",
            if neighbors == 1 {
                "neighbor"
            } else {
                "neighbors"
            }
        ));
    }
    clipped(parts.join("  "))
}

fn gateway_class_note(spec: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(kind) = spec.pointer("/service/type").and_then(Value::as_str)
        && !kind.is_empty()
    {
        parts.push(kind.to_string());
    }
    let description = str_field(spec, "description");
    if !description.is_empty() {
        parts.push(description.to_string());
    }
    clipped(parts.join("  "))
}

fn note_of(kind: Kind, spec: &Value, status: &Value, root: &Value) -> String {
    match kind {
        Kind::CiliumEnvoyConfig | Kind::CiliumClusterwideEnvoyConfig => services_text(spec),
        Kind::CiliumLocalRedirectPolicy => lrp_note(spec),
        Kind::CiliumEgressGatewayPolicy => egp_note(spec, status),
        Kind::CiliumExternalWorkload => external_note(spec, status),
        Kind::CiliumCIDRGroup => cidr_count_note(spec),
        Kind::CiliumL2AnnouncementPolicy => l2_note(spec),
        Kind::CiliumLoadBalancerIPPool => lb_pool_note(spec),
        Kind::CiliumPodIPPool => pod_pool_note(spec),
        Kind::CiliumNodeConfig => selector_text(spec.get("nodeSelector")),
        Kind::CiliumEndpointSlice => endpoints_note(root),
        Kind::CiliumBGPClusterConfig => bgp_cluster_note(spec),
        Kind::CiliumBGPPeerConfig => bgp_peer_note(spec),
        Kind::CiliumBGPAdvertisement => bgp_adv_note(spec),
        Kind::CiliumBGPNodeConfig => bgp_node_note(spec, status),
        Kind::CiliumBGPNodeConfigOverride => bgp_node_override_note(spec),
        Kind::CiliumBGPPeeringPolicy => bgp_peering_note(spec),
        Kind::CiliumGatewayClassConfig => gateway_class_note(spec),
    }
}

/// Reduce one CR to inventory fields. `spec.resources` is never read.
pub fn parse_object(kind: Kind, version: &str, value: &Value) -> Option<Resource> {
    let meta = value.get("metadata")?;
    let name = str_field(meta, "name");
    if name.is_empty() {
        return None;
    }
    let spec = value.get("spec").unwrap_or(&Value::Null);
    let status = value.get("status").unwrap_or(&Value::Null);
    Some(Resource {
        kind,
        version: version.to_string(),
        name: clipped(name.to_string()),
        namespace: clipped(str_field(meta, "namespace").to_string()),
        uid: clipped(str_field(meta, "uid").to_string()),
        note: note_of(kind, spec, status, value),
    })
}

fn ingest_items(
    kind: Kind,
    version: &str,
    items: Vec<Value>,
    already: usize,
) -> (Vec<Resource>, usize, bool) {
    let mut out = Vec::new();
    let mut unreadable = 0usize;
    let mut truncated = false;
    for value in items {
        if already + out.len() == MAX_OBJECTS {
            truncated = true;
            break;
        }
        match parse_object(kind, version, &value) {
            Some(resource) => out.push(resource),
            None => unreadable += 1,
        }
    }
    (out, unreadable, truncated)
}

/// Kinds this module inventories that an APIResourceList named.
/// CiliumNetworkPolicy and the other reserved kinds are omitted.
pub fn named_kinds(doc: &Value) -> Vec<Kind> {
    named_from_resources(read_resources(doc), "")
        .into_iter()
        .map(|named| named.kind)
        .collect()
}

fn read_resources(doc: &Value) -> Vec<WireResource> {
    serde_json::from_value::<WireResourceList>(doc.clone())
        .map(|list| list.resources)
        .unwrap_or_default()
}

fn named_from_resources(resources: Vec<WireResource>, version: &str) -> Vec<Named> {
    let mut named = Vec::new();
    for resource in resources {
        if resource.name.contains('/') {
            continue;
        }
        if !resource.verbs.is_empty() && !resource.verbs.iter().any(|verb| verb == "list") {
            continue;
        }
        let Some(kind) = Kind::from_api_kind(&resource.kind) else {
            continue;
        };
        if named.iter().any(|have: &Named| have.kind == kind) {
            continue;
        }
        let plural = if resource.name.is_empty() {
            kind.plural().to_string()
        } else {
            resource.name
        };
        named.push(Named {
            kind,
            version: version.to_string(),
            plural,
            namespaced: resource.namespaced,
        });
    }
    named
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
        out.extend(FALLBACK_VERSIONS.iter().map(|version| version.to_string()));
    }
    out
}

fn collection_url(
    version: &str,
    plural: &str,
    namespaced: bool,
    namespace: Option<&str>,
) -> String {
    let mut path = format!("/apis/{GROUP}/{version}");
    if namespaced && let Some(namespace) = namespace {
        path.push_str("/namespaces/");
        path.push_str(namespace);
    }
    path.push('/');
    path.push_str(plural);
    path
}

fn group_url() -> String {
    format!("/apis/{GROUP}")
}

fn version_url(version: &str) -> String {
    format!("/apis/{GROUP}/{version}")
}

async fn probe_group(client: &Client) -> GroupAnswer {
    let request = match http::Request::get(group_url()).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    match client.request::<WireGroup>(request).await {
        Ok(group) => {
            let versions = order_versions(
                &group.preferred.version,
                group
                    .versions
                    .into_iter()
                    .map(|item| item.version)
                    .collect(),
            );
            GroupAnswer::Served(versions)
        }
        Err(error) => after_group(&error),
    }
}

async fn probe_version(client: &Client, version: &str) -> VersionAnswer {
    let request = match http::Request::get(version_url(version)).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return VersionAnswer::Failed(error.to_string()),
    };
    match client.request::<WireResourceList>(request).await {
        Ok(doc) => VersionAnswer::Served(doc.resources),
        Err(error) => after_version(&error),
    }
}

async fn collect_named(
    client: &Client,
    versions: &[String],
) -> Result<(Vec<Named>, bool), Fetched<Inventory>> {
    let mut named = Vec::new();
    let mut saw_denied = false;
    let mut saw_served = false;
    for version in versions {
        match probe_version(client, version).await {
            VersionAnswer::Served(resources) => {
                saw_served = true;
                for item in named_from_resources(resources, version) {
                    if named.iter().any(|have: &Named| have.kind == item.kind) {
                        continue;
                    }
                    named.push(item);
                }
            }
            VersionAnswer::NotFound => {}
            VersionAnswer::Denied => saw_denied = true,
            VersionAnswer::Failed(why) => {
                return Err(Fetched::Failed { what: GROUP, why });
            }
        }
    }
    if named.is_empty() && saw_denied && !saw_served {
        return Err(Fetched::Ok(Inventory {
            group: GroupState::Denied,
            ..Inventory::default()
        }));
    }
    Ok((named, saw_denied))
}

/// A denied version document could have named any kind the served documents
/// did not, so every kind still unanswered is [`KindSet::Denied`]: access
/// denied must never read as "not installed".
fn deny_unanswered(inventory: &mut Inventory) {
    for kind in Kind::ALL {
        let set = inventory.of_mut(kind);
        if matches!(set, KindSet::NotServed) {
            *set = KindSet::Denied;
        }
    }
}

async fn list_at(
    client: &Client,
    named: &Named,
    namespace: Option<&str>,
) -> Result<KindSet, ListErr> {
    let path = collection_url(&named.version, &named.plural, named.namespaced, namespace);
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
        let text = match client.request_text(request).await {
            Ok(text) => text,
            Err(error) if items.is_empty() && unreadable == 0 => return Err(after_list(&error)),
            Err(error) => {
                return Err(ListErr::Failed(crate::connect::describe(
                    &error as &(dyn std::error::Error + 'static),
                )));
            }
        };
        if text.len() > MAX_PAGE_BYTES {
            return Err(ListErr::Failed(
                "the list page is larger than 8 MiB; the page is not shown".to_string(),
            ));
        }
        let page: WireList = match serde_json::from_str(&text) {
            Ok(page) => page,
            Err(error) => {
                return Err(ListErr::Failed(format!("the list is not JSON: {error}")));
            }
        };
        let (page_items, page_unreadable, page_truncated) =
            ingest_items(named.kind, &named.version, page.items, items.len());
        truncated |= page_truncated;
        unreadable += page_unreadable;
        items.extend(page_items);
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

async fn list_named(
    client: &Client,
    named: Vec<Named>,
    namespace: Option<&str>,
) -> Result<Inventory, Fetched<Inventory>> {
    let mut inventory = Inventory {
        group: GroupState::Served,
        ..Inventory::default()
    };
    for item in named {
        match list_at(client, &item, namespace).await {
            Ok(set) => *inventory.of_mut(item.kind) = set,
            Err(ListErr::NotFound) => {}
            Err(ListErr::Denied) => *inventory.of_mut(item.kind) = KindSet::Denied,
            Err(ListErr::Failed(why)) => {
                return Err(Fetched::Failed {
                    what: item.kind.what(),
                    why,
                });
            }
        }
    }
    Ok(inventory)
}

/// List the cilium.io control-plane kinds the version document names.
///
/// A missing group is invisible. A forbidden group is Denied and does not
/// chase lists. Kinds reserved for the policy/identity module are never listed
/// even when the document names them.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let versions = match probe_group(client).await {
        GroupAnswer::NotServed => return Fetched::Ok(Inventory::default()),
        GroupAnswer::Denied => {
            return Fetched::Ok(Inventory {
                group: GroupState::Denied,
                ..Inventory::default()
            });
        }
        GroupAnswer::Failed(why) => {
            return Fetched::Failed { what: GROUP, why };
        }
        GroupAnswer::Served(versions) => versions,
    };
    let (named, version_denied) = match collect_named(client, &versions).await {
        Ok(collected) => collected,
        Err(fetched) => return fetched,
    };
    match list_named(client, named, namespace).await {
        Ok(mut inventory) => {
            if version_denied {
                deny_unanswered(&mut inventory);
            }
            Fetched::Ok(inventory)
        }
        Err(fetched) => fetched,
    }
}

fn denied_row(name: &str) -> TableRow {
    TableRow {
        cells: vec![
            name.to_string(),
            String::new(),
            String::new(),
            "access denied for this account".to_string(),
        ],
        name: name.to_string(),
        namespace: None,
        uid: format!("denied:{name}"),
    }
}

/// Native list rows. `None` only when the group is not served, so a UI stays
/// invisible rather than opening an empty pane. A served group with no named
/// kinds is an empty table. A denied group is a labelled row.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served() {
        return None;
    }
    let columns = ["Kind", "Name", "Namespace", "Detail"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    if matches!(inventory.group, GroupState::Denied) {
        return Some(TablePage {
            columns,
            rows: vec![denied_row(GROUP)],
            truncated: false,
            continue_token: None,
        });
    }
    let mut rows = Vec::new();
    let mut truncated = false;
    for (set, kind) in inventory.sets() {
        match set {
            KindSet::NotServed => {}
            KindSet::Denied => rows.push(denied_row(kind.as_str())),
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
                            item.note.clone(),
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

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

/// The inventory as a document, rendered here so a test can gate the words
/// rather than a screenshot.
pub fn render(inventory: &Inventory) -> Vec<String> {
    if matches!(inventory.group, GroupState::NotServed) {
        return vec![
            "Cilium control-plane CRs are not served by this cluster".to_string(),
            String::new(),
            "this reads cilium.io CRs the controllers already publish; nothing is installed \
             to find them, so a cluster without those CRDs shows as empty here"
                .to_string(),
        ];
    }
    if matches!(inventory.group, GroupState::Denied) {
        return vec![format!("{GROUP}: access denied for this account")];
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
        lines.push("no Cilium control-plane objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no Cilium control-plane object could be read here, though some are stored: every \
             object this account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!(
            "{} Cilium control-plane {}",
            total,
            plural(total, "object")
        ));
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
            "{} Cilium control-plane {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            if item.namespace.is_empty() {
                lines.push(item.name.clone());
            } else {
                lines.push(format!("{}/{}", item.namespace, item.name));
            }
            let mut line = format!("  {}", item.kind.as_str());
            if !item.note.is_empty() {
                line.push_str("  ");
                line.push_str(&item.note);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "cilium_control_test.rs"]
mod tests;
