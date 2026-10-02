//! Kubernetes NetworkPolicy as declared reachability.
//!
//! The result describes what the supplied policies declare. It does not
//! account for CNI implementation details, node-local traffic, service
//! translation, cloud firewalls, or packets observed on the network.

use std::{
    collections::{BTreeMap, HashMap},
    net::IpAddr,
};

use k8s_openapi::{
    api::{
        core::v1::{Container, Namespace, Pod},
        networking::v1::{
            NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer,
            NetworkPolicyPort,
        },
    },
    apimachinery::pkg::{apis::meta::v1::LabelSelector, util::intstr::IntOrString},
};
use kube::{Api, Client, Resource, api::ListParams};
use serde::de::DeserializeOwned;

use crate::read::Fetched;

/// The cap keeps malformed or unusually large snapshots from turning an
/// inspector query into an unbounded policy walk.
pub const MAX_POLICIES: usize = 2_000;

/// Pods dominate this inventory. The cap matches the largest scene size the
/// project benchmarks while a continuation token keeps larger clusters honest.
pub const MAX_PODS: usize = 50_000;

/// Namespace count is independently bounded because it is a separate API list.
pub const MAX_NAMESPACES: usize = 5_000;

const WHAT: &str = "network policy inventory";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
    Sctp,
}

impl Protocol {
    fn from_api(value: Option<&str>) -> Option<Self> {
        match value.unwrap_or("TCP") {
            "TCP" => Some(Self::Tcp),
            "UDP" => Some(Self::Udp),
            "SCTP" => Some(Self::Sctp),
            _ => None,
        }
    }
}

/// A named destination port exposed by a pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodPort {
    pub name: String,
    pub port: u16,
    pub protocol: Protocol,
}

/// A pod identity plus the data used by NetworkPolicy selectors and peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodRef {
    pub name: String,
    pub namespace: String,
    /// Empty when the list omitted it. Overlay join then uses namespace/name.
    pub uid: String,
    pub labels: BTreeMap<String, String>,
    pub ips: Vec<IpAddr>,
    pub ports: Vec<PodPort>,
}

/// A namespace identity plus the labels `namespaceSelector` matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceRef {
    pub name: String,
    pub labels: BTreeMap<String, String>,
}

/// The destination protocol and port evaluated by [`Declared::verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Traffic {
    pub protocol: Protocol,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Ingress,
    Egress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    Complete,
    Truncated {
        evaluated_policies: usize,
        total_policies: usize,
    },
    IncompleteInventory {
        policies: bool,
        pods: bool,
        namespaces: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListStatus {
    pub kept: usize,
    /// The server's estimate for items beyond this page, when it supplied one.
    pub remaining: Option<usize>,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryStatus {
    pub policies: ListStatus,
    pub pods: ListStatus,
    pub namespaces: ListStatus,
}

impl InventoryStatus {
    pub fn complete(&self) -> bool {
        !self.policies.incomplete && !self.pods.incomplete && !self.namespaces.incomplete
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictReason {
    SamePod,
    DefaultAllow {
        direction: Direction,
    },
    AllowedByPolicy {
        direction: Direction,
        namespace: String,
        name: String,
    },
    Isolated {
        direction: Direction,
        selecting_policies: usize,
    },
    PolicySetTruncated {
        evaluated_policies: usize,
        total_policies: usize,
    },
    InventoryIncomplete {
        policies: bool,
        pods: bool,
        namespaces: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct Verdict {
    pub decision: Decision,
    pub completeness: Completeness,
    pub reasons: Vec<VerdictReason>,
}

impl Verdict {
    /// `None` forces callers to keep an incomplete answer distinct from deny.
    pub fn allowed(&self) -> Option<bool> {
        match self.decision {
            Decision::Allow => Some(true),
            Decision::Deny => Some(false),
            Decision::Indeterminate => None,
        }
    }
}

/// A source the destination is declared to receive from on at least one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    Any,
    Pod { namespace: String, name: String },
    Cidr { cidr: String, except: Vec<String> },
}

/// A bounded compiled policy set ready for repeated connectivity queries.
#[derive(Debug, Clone)]
pub struct Declared {
    policies: Vec<CompiledPolicy>,
    pods: Vec<PodRef>,
    namespaces: HashMap<String, BTreeMap<String, String>>,
    completeness: Completeness,
    /// Kept for callers which only need to display a compact warning badge.
    pub truncated: bool,
}

/// One cacheable cluster-wide policy snapshot with no raw Kubernetes objects.
#[derive(Debug, Clone)]
pub struct Inventory {
    pub declared: Declared,
    pub status: InventoryStatus,
}

/// Whether one direction is left at Kubernetes' default or selected by policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectionPosture {
    pub direction: Direction,
    pub isolated: bool,
    pub selecting_policies: usize,
    pub policies: Vec<String>,
    pub policies_truncated: bool,
}

/// The policy posture of one pod without pretending a source or port was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodPosture {
    pub ingress: DirectionPosture,
    pub egress: DirectionPosture,
    pub completeness: Completeness,
}

/// Isolation and named ports for one pod. Not a traffic verdict: that needs a
/// source, protocol, and destination port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodInspection {
    pub found: bool,
    pub posture: Option<PodPosture>,
    pub ports: Vec<PodPort>,
}

impl PodInspection {
    pub fn from_inventory(inventory: &Inventory, namespace: &str, name: &str) -> PodInspection {
        match inventory.pod(namespace, name) {
            Some(pod) => PodInspection {
                found: true,
                posture: Some(inventory.declared.pod_posture(pod, 4)),
                ports: pod.ports.clone(),
            },
            None => PodInspection {
                found: false,
                posture: None,
                ports: Vec::new(),
            },
        }
    }
}

impl Inventory {
    pub fn pods(&self) -> &[PodRef] {
        &self.declared.pods
    }

    pub fn pod(&self, namespace: &str, name: &str) -> Option<&PodRef> {
        self.declared
            .pods
            .iter()
            .find(|pod| pod.namespace == namespace && pod.name == name)
    }

    pub fn namespace_labels(&self, namespace: &str) -> Option<&BTreeMap<String, String>> {
        self.declared.namespaces.get(namespace)
    }
}

#[derive(Debug, Clone)]
struct CompiledPolicy {
    name: String,
    namespace: String,
    pod_selector: LabelSelector,
    ingress: Option<Vec<Rule>>,
    egress: Option<Vec<Rule>>,
}

#[derive(Debug, Clone)]
struct Rule {
    peers: MatchSet<PeerMatch>,
    ports: MatchSet<PortMatch>,
}

#[derive(Debug, Clone)]
enum MatchSet<T> {
    Any,
    Listed(Vec<T>),
}

#[derive(Debug, Clone)]
enum PeerMatch {
    Select {
        namespace_selector: Option<LabelSelector>,
        pod_selector: Option<LabelSelector>,
    },
    Cidr(CidrMatch),
}

#[derive(Debug, Clone)]
struct CidrMatch {
    cidr: String,
    except: Vec<String>,
    network: IpNetwork,
    exclusions: Vec<IpNetwork>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpNetwork {
    V4 { network: u32, prefix: u8 },
    V6 { network: u128, prefix: u8 },
}

#[derive(Debug, Clone)]
enum PortMatch {
    Number {
        protocol: Protocol,
        first: u16,
        last: u16,
    },
    Named {
        protocol: Protocol,
        name: String,
    },
    Protocol {
        protocol: Protocol,
    },
}

#[derive(Debug, Clone)]
enum DirectionOutcome {
    DefaultAllow,
    AllowedByPolicy { namespace: String, name: String },
    Isolated { selecting_policies: usize },
}

struct BoundedList<T> {
    items: Vec<T>,
    status: ListStatus,
}

impl<T> BoundedList<T> {
    fn filter_map<U>(self, mut map: impl FnMut(T) -> Option<U>) -> BoundedList<U> {
        let received = self.items.len();
        let items: Vec<U> = self.items.into_iter().filter_map(&mut map).collect();
        let mut status = self.status;
        if items.len() != received {
            status.incomplete = true;
        }
        status.kept = items.len();
        BoundedList { items, status }
    }
}

enum ListOutcome<T> {
    Ok(BoundedList<T>),
    Denied,
    Failed(String),
}

impl<T> ListOutcome<T> {
    fn denied(&self) -> bool {
        matches!(self, Self::Denied)
    }

    fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed(why) => Some(why),
            _ => None,
        }
    }

    fn into_list(self) -> BoundedList<T> {
        match self {
            Self::Ok(list) => list,
            Self::Denied | Self::Failed(_) => unreachable!("fetch resolves wire errors first"),
        }
    }
}

/// Fetch policies, pods, and namespaces once across the cluster and compile
/// only the fields the inspector needs. The three independent lists run with a
/// fixed concurrency of three and no continuation chase.
pub async fn fetch(client: &Client) -> Fetched<Inventory> {
    let (policies, pods, namespaces) = tokio::join!(
        list_once::<NetworkPolicy>(client, MAX_POLICIES),
        list_once::<Pod>(client, MAX_PODS),
        list_once::<Namespace>(client, MAX_NAMESPACES),
    );

    if policies.denied() || pods.denied() || namespaces.denied() {
        return Fetched::Denied { what: WHAT };
    }
    if let Some(why) = policies
        .failure()
        .or_else(|| pods.failure())
        .or_else(|| namespaces.failure())
    {
        return Fetched::Failed {
            what: WHAT,
            why: why.to_string(),
        };
    }

    let policies = policies.into_list().filter_map(|policy| {
        (policy.metadata.namespace.is_some()
            && policy
                .spec
                .as_ref()
                .is_some_and(|spec| spec.pod_selector.is_some()))
        .then_some(policy)
    });
    let pods = pods.into_list().filter_map(pod_ref);
    let namespaces = namespaces.into_list().filter_map(namespace_ref);
    let status = InventoryStatus {
        policies: policies.status,
        pods: pods.status,
        namespaces: namespaces.status,
    };
    let mut declared = declare(&policies.items, &pods.items, &namespaces.items);
    if !status.complete() {
        declared.mark_inventory_incomplete(status);
    }
    Fetched::Ok(Inventory { declared, status })
}

async fn list_once<K>(client: &Client, cap: usize) -> ListOutcome<K>
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let api: Api<K> = Api::all(client.clone());
    let params = ListParams::default().limit(u32::try_from(cap).unwrap_or(u32::MAX));
    let mut page = match api.list(&params).await {
        Ok(page) => page,
        Err(error) => {
            return match crate::read::classify::<()>(WHAT, &error) {
                Fetched::Denied { .. } => ListOutcome::Denied,
                Fetched::Failed { why, .. } => ListOutcome::Failed(why),
                Fetched::Ok(()) => unreachable!("classify never succeeds"),
            };
        }
    };
    let continued = page
        .metadata
        .continue_
        .as_deref()
        .is_some_and(|token| !token.is_empty());
    let over_cap = page.items.len() > cap;
    page.items.truncate(cap);
    ListOutcome::Ok(BoundedList {
        status: ListStatus {
            kept: page.items.len(),
            remaining: page
                .metadata
                .remaining_item_count
                .and_then(|count| usize::try_from(count).ok()),
            incomplete: continued || over_cap,
        },
        items: page.items,
    })
}

fn pod_ref(pod: Pod) -> Option<PodRef> {
    let name = pod.metadata.name?;
    let namespace = pod.metadata.namespace?;
    let labels = pod.metadata.labels.unwrap_or_default();

    let mut ips = Vec::with_capacity(2);
    if let Some(status) = pod.status {
        if let Some(pod_ips) = status.pod_ips {
            ips.extend(
                pod_ips
                    .into_iter()
                    .filter_map(|pod_ip| pod_ip.ip.parse::<IpAddr>().ok()),
            );
        }
        if let Some(ip) = status.pod_ip.and_then(|ip| ip.parse::<IpAddr>().ok()) {
            ips.push(ip);
        }
    }
    ips.sort_unstable();
    ips.dedup();

    let mut ports = Vec::new();
    if let Some(spec) = pod.spec {
        for container in &spec.containers {
            push_named_ports(&mut ports, container);
        }
        for container in spec.init_containers.as_deref().unwrap_or_default() {
            if container.restart_policy.as_deref() == Some("Always") {
                push_named_ports(&mut ports, container);
            }
        }
    }
    ports
        .sort_unstable_by(|a, b| (&a.name, a.protocol, a.port).cmp(&(&b.name, b.protocol, b.port)));
    ports.dedup();

    Some(PodRef {
        name,
        namespace,
        uid: pod.metadata.uid.unwrap_or_default(),
        labels,
        ips,
        ports,
    })
}

fn push_named_ports(out: &mut Vec<PodPort>, container: &Container) {
    for port in container.ports.as_deref().unwrap_or_default() {
        let Some(name) = port.name.as_deref().filter(|name| !name.is_empty()) else {
            continue;
        };
        let Some(protocol) = Protocol::from_api(port.protocol.as_deref()) else {
            continue;
        };
        let Some(number) = u16::try_from(port.container_port)
            .ok()
            .filter(|number| *number != 0)
        else {
            continue;
        };
        out.push(PodPort {
            name: name.to_string(),
            port: number,
            protocol,
        });
    }
}

fn namespace_ref(namespace: Namespace) -> Option<NamespaceRef> {
    Some(NamespaceRef {
        name: namespace.metadata.name?,
        labels: namespace.metadata.labels.unwrap_or_default(),
    })
}

/// Compile the supplied cluster state without consulting policies past the cap.
pub fn declare(
    policies: &[NetworkPolicy],
    pods: &[PodRef],
    namespaces: &[NamespaceRef],
) -> Declared {
    let evaluated_policies = policies.len().min(MAX_POLICIES);
    let completeness = if policies.len() > MAX_POLICIES {
        Completeness::Truncated {
            evaluated_policies,
            total_policies: policies.len(),
        }
    } else {
        Completeness::Complete
    };
    let compiled = policies
        .iter()
        .take(MAX_POLICIES)
        .filter_map(compile)
        .collect();
    let mut ns_labels = HashMap::with_capacity(namespaces.len());
    for namespace in namespaces {
        ns_labels.insert(namespace.name.clone(), namespace.labels.clone());
    }
    Declared {
        policies: compiled,
        pods: pods.to_vec(),
        namespaces: ns_labels,
        completeness,
        truncated: !matches!(completeness, Completeness::Complete),
    }
}

impl Declared {
    pub fn completeness(&self) -> Completeness {
        self.completeness
    }

    /// Summarize which directions select a pod. An allow or deny needs a peer,
    /// protocol, and destination port, so this deliberately reports isolation
    /// rather than turning an incomplete traffic tuple into a verdict.
    pub fn pod_posture(&self, pod: &PodRef, policy_name_limit: usize) -> PodPosture {
        PodPosture {
            ingress: self.direction_posture(pod, Direction::Ingress, policy_name_limit),
            egress: self.direction_posture(pod, Direction::Egress, policy_name_limit),
            completeness: self.completeness,
        }
    }

    /// Evaluate source egress and destination ingress for one destination port.
    pub fn verdict(&self, src: &PodRef, dst: &PodRef, traffic: Traffic) -> Verdict {
        if same_pod(src, dst) {
            let mut reasons = vec![VerdictReason::SamePod];
            self.push_incomplete_reason(&mut reasons);
            return Verdict {
                decision: Decision::Allow,
                completeness: self.completeness,
                reasons,
            };
        }

        let ingress = self.evaluate_direction(Direction::Ingress, src, dst, traffic);
        let egress = self.evaluate_direction(Direction::Egress, src, dst, traffic);
        let ingress_allowed = ingress.allowed();
        let egress_allowed = egress.allowed();
        let decision = match self.completeness {
            Completeness::Complete => {
                if ingress_allowed && egress_allowed {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Completeness::Truncated { .. } | Completeness::IncompleteInventory { .. } => {
                if ingress.proves_allow() && egress.proves_allow() {
                    Decision::Allow
                } else {
                    Decision::Indeterminate
                }
            }
        };
        let mut reasons = vec![
            ingress.into_reason(Direction::Ingress),
            egress.into_reason(Direction::Egress),
        ];
        self.push_incomplete_reason(&mut reasons);
        Verdict {
            decision,
            completeness: self.completeness,
            reasons,
        }
    }

    /// Compatibility query for the ingress peer dimension only.
    ///
    /// Port and egress restrictions require [`Self::verdict`]. An incomplete
    /// set returns true rather than misrepresenting a provisional deny.
    pub fn can_receive(&self, dst: &PodRef, src: &PodRef) -> bool {
        if same_pod(src, dst) {
            return true;
        }
        if self.truncated {
            return true;
        }
        self.evaluate_ingress_peer(dst, src).allowed()
    }

    /// Sources which may reach `dst` on at least one declared ingress port.
    ///
    /// An incomplete set returns `Any` because omitted policies can introduce
    /// isolation or another allow, so a narrower list would look authoritative.
    pub fn allowed_peers(&self, dst: &PodRef) -> Vec<Peer> {
        if self.truncated {
            return vec![Peer::Any];
        }
        let selecting: Vec<_> = self
            .policies
            .iter()
            .filter(|policy| {
                policy.ingress.is_some()
                    && policy.namespace == dst.namespace
                    && selector_matches(&policy.pod_selector, &dst.labels)
            })
            .collect();
        if selecting.is_empty() {
            return vec![Peer::Any];
        }

        let mut any = false;
        let mut pods = Vec::new();
        let mut cidrs = Vec::new();
        push_pod(&mut pods, &dst.namespace, &dst.name);

        for policy in selecting {
            for rule in policy.ingress.as_deref().unwrap_or_default() {
                if !rule.ports.admits_any(dst) {
                    continue;
                }
                match &rule.peers {
                    MatchSet::Any => any = true,
                    MatchSet::Listed(peers) => {
                        for peer in peers {
                            if let PeerMatch::Cidr(block) = peer {
                                let candidate = Peer::Cidr {
                                    cidr: block.cidr.clone(),
                                    except: block.except.clone(),
                                };
                                if !cidrs.contains(&candidate) {
                                    cidrs.push(candidate);
                                }
                            }
                            for pod in &self.pods {
                                if peer_allows(policy, peer, pod, &self.namespaces) {
                                    push_pod(&mut pods, &pod.namespace, &pod.name);
                                }
                            }
                        }
                    }
                }
            }
        }

        if any {
            return vec![Peer::Any];
        }
        pods.sort_by(|a, b| peer_key(a).cmp(&peer_key(b)));
        cidrs.sort_by(|a, b| peer_key(a).cmp(&peer_key(b)));
        pods.extend(cidrs);
        pods
    }

    fn evaluate_direction(
        &self,
        direction: Direction,
        src: &PodRef,
        dst: &PodRef,
        traffic: Traffic,
    ) -> DirectionOutcome {
        let selected = match direction {
            Direction::Ingress => dst,
            Direction::Egress => src,
        };
        let peer = match direction {
            Direction::Ingress => src,
            Direction::Egress => dst,
        };
        let mut selecting_policies = 0;
        let mut witness = None;

        for policy in &self.policies {
            let rules = match direction {
                Direction::Ingress => policy.ingress.as_deref(),
                Direction::Egress => policy.egress.as_deref(),
            };
            let Some(rules) = rules else {
                continue;
            };
            if policy.namespace != selected.namespace
                || !selector_matches(&policy.pod_selector, &selected.labels)
            {
                continue;
            }
            selecting_policies += 1;
            if witness.is_none()
                && rules
                    .iter()
                    .any(|rule| rule.matches(policy, peer, dst, traffic, &self.namespaces))
            {
                witness = Some((policy.namespace.clone(), policy.name.clone()));
            }
        }

        if selecting_policies == 0 {
            DirectionOutcome::DefaultAllow
        } else if let Some((namespace, name)) = witness {
            DirectionOutcome::AllowedByPolicy { namespace, name }
        } else {
            DirectionOutcome::Isolated { selecting_policies }
        }
    }

    fn direction_posture(
        &self,
        pod: &PodRef,
        direction: Direction,
        policy_name_limit: usize,
    ) -> DirectionPosture {
        let mut selecting_policies = 0usize;
        let mut policies = Vec::with_capacity(policy_name_limit.min(8));
        for policy in &self.policies {
            let applies_to_direction = match direction {
                Direction::Ingress => policy.ingress.is_some(),
                Direction::Egress => policy.egress.is_some(),
            };
            if !applies_to_direction
                || policy.namespace != pod.namespace
                || !selector_matches(&policy.pod_selector, &pod.labels)
            {
                continue;
            }
            selecting_policies += 1;
            if policies.len() < policy_name_limit {
                policies.push(format!("{}/{}", policy.namespace, policy.name));
            }
        }
        DirectionPosture {
            direction,
            isolated: selecting_policies > 0,
            selecting_policies,
            policies_truncated: selecting_policies > policies.len(),
            policies,
        }
    }

    fn evaluate_ingress_peer(&self, dst: &PodRef, src: &PodRef) -> DirectionOutcome {
        let mut selecting_policies = 0;
        let mut witness = None;
        for policy in &self.policies {
            let Some(rules) = policy.ingress.as_deref() else {
                continue;
            };
            if policy.namespace != dst.namespace
                || !selector_matches(&policy.pod_selector, &dst.labels)
            {
                continue;
            }
            selecting_policies += 1;
            if witness.is_none()
                && rules.iter().any(|rule| {
                    rule.ports.admits_any(dst)
                        && rule.peers.matches_peer(policy, src, &self.namespaces)
                })
            {
                witness = Some((policy.namespace.clone(), policy.name.clone()));
            }
        }
        if selecting_policies == 0 {
            DirectionOutcome::DefaultAllow
        } else if let Some((namespace, name)) = witness {
            DirectionOutcome::AllowedByPolicy { namespace, name }
        } else {
            DirectionOutcome::Isolated { selecting_policies }
        }
    }

    fn push_incomplete_reason(&self, reasons: &mut Vec<VerdictReason>) {
        match self.completeness {
            Completeness::Complete => {}
            Completeness::Truncated {
                evaluated_policies,
                total_policies,
            } => reasons.push(VerdictReason::PolicySetTruncated {
                evaluated_policies,
                total_policies,
            }),
            Completeness::IncompleteInventory {
                policies,
                pods,
                namespaces,
            } => reasons.push(VerdictReason::InventoryIncomplete {
                policies,
                pods,
                namespaces,
            }),
        }
    }

    fn mark_inventory_incomplete(&mut self, status: InventoryStatus) {
        self.completeness = Completeness::IncompleteInventory {
            policies: status.policies.incomplete,
            pods: status.pods.incomplete,
            namespaces: status.namespaces.incomplete,
        };
        self.truncated = true;
    }
}

impl DirectionOutcome {
    fn allowed(&self) -> bool {
        !matches!(self, Self::Isolated { .. })
    }

    fn proves_allow(&self) -> bool {
        matches!(self, Self::AllowedByPolicy { .. })
    }

    fn into_reason(self, direction: Direction) -> VerdictReason {
        match self {
            Self::DefaultAllow => VerdictReason::DefaultAllow { direction },
            Self::AllowedByPolicy { namespace, name } => VerdictReason::AllowedByPolicy {
                direction,
                namespace,
                name,
            },
            Self::Isolated { selecting_policies } => VerdictReason::Isolated {
                direction,
                selecting_policies,
            },
        }
    }
}

impl Rule {
    fn matches(
        &self,
        policy: &CompiledPolicy,
        peer: &PodRef,
        destination: &PodRef,
        traffic: Traffic,
        namespaces: &HashMap<String, BTreeMap<String, String>>,
    ) -> bool {
        self.peers.matches_peer(policy, peer, namespaces)
            && self.ports.matches_port(destination, traffic)
    }
}

impl MatchSet<PeerMatch> {
    fn matches_peer(
        &self,
        policy: &CompiledPolicy,
        peer: &PodRef,
        namespaces: &HashMap<String, BTreeMap<String, String>>,
    ) -> bool {
        match self {
            Self::Any => true,
            Self::Listed(peers) => peers
                .iter()
                .any(|candidate| peer_allows(policy, candidate, peer, namespaces)),
        }
    }
}

impl MatchSet<PortMatch> {
    fn matches_port(&self, destination: &PodRef, traffic: Traffic) -> bool {
        match self {
            Self::Any => true,
            Self::Listed(ports) => ports.iter().any(|port| port.matches(destination, traffic)),
        }
    }

    fn admits_any(&self, destination: &PodRef) -> bool {
        match self {
            Self::Any => true,
            Self::Listed(ports) => ports.iter().any(|port| port.admits_any(destination)),
        }
    }
}

impl PortMatch {
    fn matches(&self, destination: &PodRef, traffic: Traffic) -> bool {
        match self {
            Self::Number {
                protocol,
                first,
                last,
            } => traffic.protocol == *protocol && (*first..=*last).contains(&traffic.port),
            Self::Named { protocol, name } => {
                traffic.protocol == *protocol
                    && destination.ports.iter().any(|port| {
                        port.name == *name
                            && port.protocol == *protocol
                            && port.port == traffic.port
                    })
            }
            Self::Protocol { protocol } => traffic.protocol == *protocol,
        }
    }

    fn admits_any(&self, destination: &PodRef) -> bool {
        match self {
            Self::Number { .. } | Self::Protocol { .. } => true,
            Self::Named { protocol, name } => destination
                .ports
                .iter()
                .any(|port| port.name == *name && port.protocol == *protocol),
        }
    }
}

fn compile(policy: &NetworkPolicy) -> Option<CompiledPolicy> {
    let namespace = policy.metadata.namespace.as_deref()?.to_string();
    let spec = policy.spec.as_ref()?;
    let (ingress, egress) = policy_types(spec.policy_types.as_deref(), spec.egress.is_some());
    Some(CompiledPolicy {
        name: policy.metadata.name.clone().unwrap_or_default(),
        namespace,
        pod_selector: spec.pod_selector.clone().unwrap_or_default(),
        ingress: ingress.then(|| {
            spec.ingress
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(compile_ingress_rule)
                .collect()
        }),
        egress: egress.then(|| {
            spec.egress
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(compile_egress_rule)
                .collect()
        }),
    })
}

/// Kubernetes defaults every policy to ingress and adds egress only when the
/// egress field is present. An explicit nonempty list replaces that default.
fn policy_types(policy_types: Option<&[String]>, has_egress: bool) -> (bool, bool) {
    match policy_types {
        Some(types) if !types.is_empty() => (
            types.iter().any(|value| value == "Ingress"),
            types.iter().any(|value| value == "Egress"),
        ),
        _ => (true, has_egress),
    }
}

fn compile_ingress_rule(rule: &NetworkPolicyIngressRule) -> Rule {
    Rule {
        peers: compile_peers(rule.from.as_deref()),
        ports: compile_ports(rule.ports.as_deref()),
    }
}

fn compile_egress_rule(rule: &NetworkPolicyEgressRule) -> Rule {
    Rule {
        peers: compile_peers(rule.to.as_deref()),
        ports: compile_ports(rule.ports.as_deref()),
    }
}

fn compile_peers(peers: Option<&[NetworkPolicyPeer]>) -> MatchSet<PeerMatch> {
    match peers {
        None | Some([]) => MatchSet::Any,
        Some(peers) => MatchSet::Listed(peers.iter().filter_map(compile_peer).collect()),
    }
}

fn compile_ports(ports: Option<&[NetworkPolicyPort]>) -> MatchSet<PortMatch> {
    match ports {
        None | Some([]) => MatchSet::Any,
        Some(ports) => MatchSet::Listed(ports.iter().filter_map(compile_port).collect()),
    }
}

fn compile_peer(peer: &NetworkPolicyPeer) -> Option<PeerMatch> {
    match (
        peer.ip_block.as_ref(),
        peer.namespace_selector.as_ref(),
        peer.pod_selector.as_ref(),
    ) {
        (Some(block), None, None) => compile_cidr(block),
        (None, namespace_selector, pod_selector)
            if namespace_selector.is_some() || pod_selector.is_some() =>
        {
            Some(PeerMatch::Select {
                namespace_selector: namespace_selector.cloned(),
                pod_selector: pod_selector.cloned(),
            })
        }
        _ => None,
    }
}

fn compile_cidr(block: &k8s_openapi::api::networking::v1::IPBlock) -> Option<PeerMatch> {
    let network = IpNetwork::parse(&block.cidr)?;
    let except = block.except.clone().unwrap_or_default();
    let exclusions: Option<Vec<_>> = except
        .iter()
        .map(|value| {
            let exclusion = IpNetwork::parse(value)?;
            network.contains_network(exclusion).then_some(exclusion)
        })
        .collect();
    Some(PeerMatch::Cidr(CidrMatch {
        cidr: block.cidr.clone(),
        except,
        network,
        exclusions: exclusions?,
    }))
}

fn compile_port(port: &NetworkPolicyPort) -> Option<PortMatch> {
    let protocol = Protocol::from_api(port.protocol.as_deref())?;
    match (&port.port, port.end_port) {
        (None, None) => Some(PortMatch::Protocol { protocol }),
        (Some(IntOrString::Int(first)), end) => {
            let first = u16::try_from(*first).ok().filter(|value| *value != 0)?;
            let last = match end {
                Some(last) => u16::try_from(last).ok().filter(|value| *value >= first)?,
                None => first,
            };
            Some(PortMatch::Number {
                protocol,
                first,
                last,
            })
        }
        (Some(IntOrString::String(name)), None) if !name.is_empty() => Some(PortMatch::Named {
            protocol,
            name: name.clone(),
        }),
        _ => None,
    }
}

fn peer_allows(
    policy: &CompiledPolicy,
    peer: &PeerMatch,
    candidate: &PodRef,
    namespaces: &HashMap<String, BTreeMap<String, String>>,
) -> bool {
    match peer {
        PeerMatch::Cidr(block) => candidate.ips.iter().any(|ip| {
            block.network.contains(*ip)
                && !block.exclusions.iter().any(|except| except.contains(*ip))
        }),
        PeerMatch::Select {
            namespace_selector,
            pod_selector,
        } => {
            let namespace_matches = match namespace_selector {
                Some(selector) => namespaces
                    .get(&candidate.namespace)
                    .is_some_and(|labels| selector_matches(selector, labels)),
                None => candidate.namespace == policy.namespace,
            };
            namespace_matches
                && pod_selector
                    .as_ref()
                    .is_none_or(|selector| selector_matches(selector, &candidate.labels))
        }
    }
}

impl IpNetwork {
    fn parse(value: &str) -> Option<Self> {
        let (address, prefix) = value.split_once('/')?;
        let address: IpAddr = address.parse().ok()?;
        let prefix: u8 = prefix.parse().ok()?;
        match address {
            IpAddr::V4(address) if prefix <= 32 => {
                let mask = prefix_mask_v4(prefix);
                Some(Self::V4 {
                    network: u32::from(address) & mask,
                    prefix,
                })
            }
            IpAddr::V6(address) if prefix <= 128 => {
                let mask = prefix_mask_v6(prefix);
                Some(Self::V6 {
                    network: u128::from(address) & mask,
                    prefix,
                })
            }
            _ => None,
        }
    }

    fn contains(self, address: IpAddr) -> bool {
        match (self, address) {
            (Self::V4 { network, prefix }, IpAddr::V4(address)) => {
                u32::from(address) & prefix_mask_v4(prefix) == network
            }
            (Self::V6 { network, prefix }, IpAddr::V6(address)) => {
                u128::from(address) & prefix_mask_v6(prefix) == network
            }
            _ => false,
        }
    }

    fn contains_network(self, other: Self) -> bool {
        match (self, other) {
            (
                Self::V4 { network, prefix },
                Self::V4 {
                    network: other,
                    prefix: other_prefix,
                },
            ) => other_prefix >= prefix && other & prefix_mask_v4(prefix) == network,
            (
                Self::V6 { network, prefix },
                Self::V6 {
                    network: other,
                    prefix: other_prefix,
                },
            ) => other_prefix >= prefix && other & prefix_mask_v6(prefix) == network,
            _ => false,
        }
    }
}

fn prefix_mask_v4(prefix: u8) -> u32 {
    u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0)
}

fn prefix_mask_v6(prefix: u8) -> u128 {
    u128::MAX.checked_shl(u32::from(128 - prefix)).unwrap_or(0)
}

fn same_pod(a: &PodRef, b: &PodRef) -> bool {
    a.namespace == b.namespace && a.name == b.name
}

fn push_pod(out: &mut Vec<Peer>, namespace: &str, name: &str) {
    let peer = Peer::Pod {
        namespace: namespace.to_string(),
        name: name.to_string(),
    };
    if !out.contains(&peer) {
        out.push(peer);
    }
}

fn peer_key(peer: &Peer) -> (u8, &str, &str, &[String]) {
    match peer {
        Peer::Any => (0, "", "", &[]),
        Peer::Pod { namespace, name } => (1, namespace, name, &[]),
        Peer::Cidr { cidr, except } => (2, cidr, "", except),
    }
}

/// Unknown selector operators fail closed so malformed input cannot create an
/// allow which the API server itself would reject.
fn selector_matches(selector: &LabelSelector, labels: &BTreeMap<String, String>) -> bool {
    if let Some(match_labels) = &selector.match_labels {
        for (key, value) in match_labels {
            if labels.get(key) != Some(value) {
                return false;
            }
        }
    }
    for expression in selector.match_expressions.as_deref().unwrap_or_default() {
        let value = labels.get(&expression.key);
        let values = expression.values.as_deref().unwrap_or_default();
        let matched = match expression.operator.as_str() {
            "In" => value.is_some_and(|value| values.iter().any(|item| item == value)),
            "NotIn" => value.is_none_or(|value| values.iter().all(|item| item != value)),
            "Exists" => value.is_some(),
            "DoesNotExist" => value.is_none(),
            _ => false,
        };
        if !matched {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "netpol_test.rs"]
mod tests;
