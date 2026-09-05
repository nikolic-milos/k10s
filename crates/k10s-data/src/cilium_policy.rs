//! Declared Cilium connectivity, compiled from CiliumNetworkPolicy and
//! CiliumClusterwideNetworkPolicy.
//!
//! The verdict is what those CRs allow. It is not a Hubble flow, not a
//! Prometheus series, and not a packet. Mixing this result with observed
//! traffic is a correctness bug, not a cosmetic one.
//!
//! L7 HTTP on `toPorts.rules.http` is still declared: the policy named a
//! method and path. Hubble never saw it here. Go-regex path matching is not
//! evaluated (no regex crate in the lock); a named path matches exactly or
//! as a prefix.
//!
//! L3 selector kinds this module does not evaluate (FQDN, service, node,
//! group, and requires based) fail closed: such a rule never proves an allow,
//! and an isolation that hinges on one is Indeterminate, not Deny. A
//! `toPorts` entry the compiler cannot hold fails closed the same way. A
//! direction with `spec.enableDefaultDeny` false contributes allows without
//! isolating.

use std::{collections::BTreeMap, net::IpAddr};

use serde::Deserialize;
use serde_json::Value;

/// The cap keeps a large policy dump from turning a verdict into an unbounded walk.
pub const MAX_POLICIES: usize = 2_000;

const NS_LABEL: &str = "io.kubernetes.pod.namespace";

/// Cilium's namespace-label keys on identities and selectors.
const NS_LABELS_PREFIX: &str = "io.cilium.k8s.namespace.labels.";

/// Reserved numeric identities Cilium assigns. A normal workload identity is
/// never one of these.
const ID_HOST: i64 = 1;
const ID_WORLD: i64 = 2;
const ID_UNMANAGED: i64 = 3;
const ID_INIT: i64 = 5;
const ID_REMOTE_NODE: i64 = 6;
const ID_KUBE_APISERVER: i64 = 7;
const ID_INGRESS: i64 = 8;
const ID_WORLD_IPV4: i64 = 9;
const ID_WORLD_IPV6: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
    Sctp,
    /// Cilium's omitted `toPorts.ports.protocol`: every protocol.
    Any,
}

impl Protocol {
    fn from_api(value: Option<&str>) -> Option<Self> {
        match value
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("ANY")
        {
            "TCP" | "tcp" => Some(Self::Tcp),
            "UDP" | "udp" => Some(Self::Udp),
            "SCTP" | "sctp" => Some(Self::Sctp),
            "ANY" | "any" | "ANY_PROTO" => Some(Self::Any),
            _ => None,
        }
    }

    fn admits(self, traffic: Protocol) -> bool {
        matches!(self, Self::Any) || self == traffic
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedPort {
    pub name: String,
    pub port: u16,
    pub protocol: Protocol,
}

/// HTTP a policy declares on `toPorts.rules.http`. Not a Hubble L7 decode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeclaredL7 {
    pub method: String,
    pub path: String,
}

/// The destination protocol, port, and optional declared L7 under test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traffic {
    pub protocol: Protocol,
    pub port: u16,
    pub l7: Option<DeclaredL7>,
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
    IncompleteInventory,
}

/// Entities as Cilium spells them on `fromEntities` / `toEntities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entity {
    World,
    Host,
    RemoteNode,
    Cluster,
    Init,
    Ingress,
    Unmanaged,
    KubeApiserver,
    /// `all`: every identity, including world.
    All,
    Health,
}

impl Entity {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "world" => Some(Self::World),
            "host" => Some(Self::Host),
            "remote-node" => Some(Self::RemoteNode),
            "cluster" => Some(Self::Cluster),
            "init" => Some(Self::Init),
            "ingress" => Some(Self::Ingress),
            "unmanaged" => Some(Self::Unmanaged),
            "kube-apiserver" => Some(Self::KubeApiserver),
            "all" => Some(Self::All),
            "health" => Some(Self::Health),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::World => "world",
            Self::Host => "host",
            Self::RemoteNode => "remote-node",
            Self::Cluster => "cluster",
            Self::Init => "init",
            Self::Ingress => "ingress",
            Self::Unmanaged => "unmanaged",
            Self::KubeApiserver => "kube-apiserver",
            Self::All => "all",
            Self::Health => "health",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LabelSelector {
    pub match_labels: BTreeMap<String, String>,
    pub match_expressions: Vec<LabelExpression>,
}

impl LabelSelector {
    pub fn is_empty(&self) -> bool {
        self.match_labels.is_empty() && self.match_expressions.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelExpression {
    pub key: String,
    pub operator: String,
    pub values: Vec<String>,
}

/// Two pod identities for a client-side verdict: labels plus a Cilium identity
/// id when the endpoint or CiliumIdentity named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointRef {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub labels: BTreeMap<String, String>,
    pub identity_id: Option<i64>,
    pub ips: Vec<IpAddr>,
    pub ports: Vec<NamedPort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictReason {
    SameEndpoint,
    DefaultAllow {
        direction: Direction,
    },
    AllowedByPolicy {
        direction: Direction,
        namespace: String,
        name: String,
        clusterwide: bool,
        /// The allowing rule named `toPorts.rules.http`. Still declared.
        declared_l7: bool,
    },
    Isolated {
        direction: Direction,
        selecting_policies: usize,
    },
    /// Every rule this module can evaluate failed closed, but a selecting
    /// policy carries one it cannot (an unmodelled L3 selector kind or an
    /// uncompilable `toPorts` entry) that may admit this peer. The isolation
    /// is not proven.
    IsolationUnproven {
        direction: Direction,
        selecting_policies: usize,
    },
    PolicySetTruncated {
        evaluated_policies: usize,
        total_policies: usize,
    },
    InventoryIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct Verdict {
    pub decision: Decision,
    pub completeness: Completeness,
    pub reasons: Vec<VerdictReason>,
}

impl Verdict {
    /// `None` keeps an incomplete answer distinct from deny.
    pub fn allowed(&self) -> Option<bool> {
        match self.decision {
            Decision::Allow => Some(true),
            Decision::Deny => Some(false),
            Decision::Indeterminate => None,
        }
    }
}

/// One CNP or CCNP document, reduced to the selectors a verdict walks.
///
/// `ingress` / `egress` use `None` for a missing field (this policy does not
/// isolate that direction) and `Some` for a present list, including empty
/// (default-deny, no allows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiliumPolicy {
    pub name: String,
    pub namespace: String,
    pub clusterwide: bool,
    pub endpoint_selector: LabelSelector,
    pub selects_nodes: bool,
    /// `spec.enableDefaultDeny`: a false direction contributes allows
    /// without isolating.
    pub default_deny_ingress: bool,
    pub default_deny_egress: bool,
    pub ingress: Option<Vec<PolicyRule>>,
    pub egress: Option<Vec<PolicyRule>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRule {
    pub endpoints: Option<Vec<LabelSelector>>,
    pub entities: Option<Vec<Entity>>,
    pub cidrs: Option<Vec<CidrRule>>,
    pub ports: Vec<PortRule>,
    pub http: Vec<DeclaredL7>,
    /// The rule names an L3 selector kind this module does not evaluate
    /// (FQDN, service, node, group, or requires based).
    pub unmodelled_l3: bool,
    /// A declared `toPorts` entry did not survive the wire decode.
    pub dropped_ports: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CidrRule {
    pub cidr: String,
    pub except: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRule {
    pub protocol: Protocol,
    pub port: Option<u16>,
    pub end_port: Option<u16>,
    pub name: Option<String>,
}

/// Compiled policy set. Declared connectivity only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    policies: Vec<CompiledPolicy>,
    completeness: Completeness,
    pub truncated: bool,
}

impl Default for Declared {
    fn default() -> Self {
        Declared {
            policies: Vec::new(),
            completeness: Completeness::Complete,
            truncated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledPolicy {
    name: String,
    namespace: String,
    clusterwide: bool,
    endpoint_selector: LabelSelector,
    default_deny_ingress: bool,
    default_deny_egress: bool,
    ingress: Option<Vec<CompiledRule>>,
    egress: Option<Vec<CompiledRule>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledRule {
    endpoints: Option<Vec<LabelSelector>>,
    entities: Option<Vec<Entity>>,
    cidrs: Option<Vec<CompiledCidr>>,
    ports: Vec<CompiledPort>,
    http: Vec<DeclaredL7>,
    unmodelled_l3: bool,
    dropped_ports: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledCidr {
    cidr: String,
    except: Vec<String>,
    network: IpNetwork,
    exclusions: Vec<IpNetwork>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompiledPort {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpNetwork {
    V4 { network: u32, prefix: u8 },
    V6 { network: u128, prefix: u8 },
}

enum DirectionOutcome {
    DefaultAllow,
    Allowed {
        namespace: String,
        name: String,
        clusterwide: bool,
        declared_l7: bool,
    },
    Isolated {
        selecting_policies: usize,
    },
    /// Isolated as far as this module can evaluate, but a selecting policy
    /// carries a rule it cannot; the isolation is not proven.
    Unproven {
        selecting_policies: usize,
    },
}

/// Parse one CNP or CCNP document. `spec` and `specs` both produce policies
/// that share the object's name.
pub fn parse_policy_document(value: &Value) -> Vec<CiliumPolicy> {
    let meta = value.get("metadata").unwrap_or(&Value::Null);
    let name = str_field(meta, "name");
    if name.is_empty() {
        return Vec::new();
    }
    let kind = str_field(value, "kind");
    let clusterwide = kind == "CiliumClusterwideNetworkPolicy";
    let namespace = if clusterwide {
        String::new()
    } else {
        str_field(meta, "namespace").to_string()
    };
    let mut out = Vec::new();
    if let Some(spec) = value.get("spec") {
        if let Some(policy) = rule_from_value(spec, name, &namespace, clusterwide) {
            out.push(policy);
        }
    }
    if let Some(specs) = value.get("specs").and_then(Value::as_array) {
        for spec in specs {
            if let Some(policy) = rule_from_value(spec, name, &namespace, clusterwide) {
                out.push(policy);
            }
        }
    }
    out
}

fn rule_from_value(
    spec: &Value,
    name: &str,
    namespace: &str,
    clusterwide: bool,
) -> Option<CiliumPolicy> {
    let wire: WireRule = serde_json::from_value(spec.clone()).ok()?;
    Some(CiliumPolicy {
        name: name.to_string(),
        namespace: namespace.to_string(),
        clusterwide,
        endpoint_selector: selector_from_wire(&wire.endpoint_selector),
        selects_nodes: wire.node_selector.is_some(),
        default_deny_ingress: wire.enable_default_deny.ingress.unwrap_or(true),
        default_deny_egress: wire.enable_default_deny.egress.unwrap_or(true),
        ingress: wire.ingress.map(|rules| {
            rules
                .into_iter()
                .map(|rule| compile_ingress_wire(&rule))
                .collect()
        }),
        egress: wire.egress.map(|rules| {
            rules
                .into_iter()
                .map(|rule| compile_egress_wire(&rule))
                .collect()
        }),
    })
}

/// Compile the supplied policies without walking past the cap.
///
/// Host-only rules (`nodeSelector` and no endpoint selector) are dropped:
/// this verdict is pod-to-pod, not a node-admin action.
pub fn declare(policies: &[CiliumPolicy]) -> Declared {
    let total = policies.len();
    let evaluated = total.min(MAX_POLICIES);
    let completeness = if total > MAX_POLICIES {
        Completeness::Truncated {
            evaluated_policies: evaluated,
            total_policies: total,
        }
    } else {
        Completeness::Complete
    };
    let compiled = policies
        .iter()
        .take(MAX_POLICIES)
        .filter_map(compile)
        .collect();
    Declared {
        policies: compiled,
        completeness,
        truncated: !matches!(completeness, Completeness::Complete),
    }
}

impl Declared {
    pub fn completeness(&self) -> Completeness {
        self.completeness
    }

    pub fn mark_incomplete(&mut self) {
        self.completeness = Completeness::IncompleteInventory;
        self.truncated = true;
    }

    /// Source egress and destination ingress for one destination port.
    ///
    /// This is declared connectivity, not packets Hubble saw.
    pub fn verdict(&self, src: &EndpointRef, dst: &EndpointRef, traffic: Traffic) -> Verdict {
        if same_endpoint(src, dst) {
            let mut reasons = vec![VerdictReason::SameEndpoint];
            self.push_incomplete_reason(&mut reasons);
            return Verdict {
                decision: Decision::Allow,
                completeness: self.completeness,
                reasons,
            };
        }

        let ingress = self.evaluate_direction(Direction::Ingress, src, dst, &traffic);
        let egress = self.evaluate_direction(Direction::Egress, src, dst, &traffic);
        let decision = match self.completeness {
            Completeness::Complete => {
                if ingress.allowed() && egress.allowed() {
                    Decision::Allow
                } else if ingress.isolates() || egress.isolates() {
                    Decision::Deny
                } else {
                    Decision::Indeterminate
                }
            }
            Completeness::Truncated { .. } | Completeness::IncompleteInventory => {
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

    fn evaluate_direction(
        &self,
        direction: Direction,
        src: &EndpointRef,
        dst: &EndpointRef,
        traffic: &Traffic,
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
        let mut unproven = false;

        for policy in &self.policies {
            let rules = match direction {
                Direction::Ingress => policy.ingress.as_deref(),
                Direction::Egress => policy.egress.as_deref(),
            };
            let Some(rules) = rules else {
                continue;
            };
            if !policy_selects(policy, selected) {
                continue;
            }
            let default_deny = match direction {
                Direction::Ingress => policy.default_deny_ingress,
                Direction::Egress => policy.default_deny_egress,
            };
            if default_deny {
                selecting_policies += 1;
            }
            if witness.is_none() {
                if let Some(rule) = rules
                    .iter()
                    .find(|rule| rule.matches(policy, peer, dst, traffic))
                {
                    witness = Some((
                        policy.namespace.clone(),
                        policy.name.clone(),
                        policy.clusterwide,
                        !rule.http.is_empty(),
                    ));
                }
            }
            unproven = unproven || rules.iter().any(|rule| rule.allow_unproven(policy, peer));
        }

        if selecting_policies == 0 {
            DirectionOutcome::DefaultAllow
        } else if let Some((namespace, name, clusterwide, declared_l7)) = witness {
            DirectionOutcome::Allowed {
                namespace,
                name,
                clusterwide,
                declared_l7,
            }
        } else if unproven {
            DirectionOutcome::Unproven { selecting_policies }
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
            Completeness::IncompleteInventory => reasons.push(VerdictReason::InventoryIncomplete),
        }
    }
}

impl DirectionOutcome {
    fn allowed(&self) -> bool {
        matches!(self, Self::DefaultAllow | Self::Allowed { .. })
    }

    fn isolates(&self) -> bool {
        matches!(self, Self::Isolated { .. })
    }

    fn proves_allow(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    fn into_reason(self, direction: Direction) -> VerdictReason {
        match self {
            Self::DefaultAllow => VerdictReason::DefaultAllow { direction },
            Self::Allowed {
                namespace,
                name,
                clusterwide,
                declared_l7,
            } => VerdictReason::AllowedByPolicy {
                direction,
                namespace,
                name,
                clusterwide,
                declared_l7,
            },
            Self::Isolated { selecting_policies } => VerdictReason::Isolated {
                direction,
                selecting_policies,
            },
            Self::Unproven { selecting_policies } => VerdictReason::IsolationUnproven {
                direction,
                selecting_policies,
            },
        }
    }
}

fn compile(policy: &CiliumPolicy) -> Option<CompiledPolicy> {
    if policy.name.is_empty() {
        return None;
    }
    if policy.selects_nodes && policy.endpoint_selector.is_empty() {
        return None;
    }
    Some(CompiledPolicy {
        name: policy.name.clone(),
        namespace: policy.namespace.clone(),
        clusterwide: policy.clusterwide,
        endpoint_selector: policy.endpoint_selector.clone(),
        default_deny_ingress: policy.default_deny_ingress,
        default_deny_egress: policy.default_deny_egress,
        ingress: policy
            .ingress
            .as_ref()
            .map(|rules| rules.iter().filter_map(compile_rule).collect()),
        egress: policy
            .egress
            .as_ref()
            .map(|rules| rules.iter().filter_map(compile_rule).collect()),
    })
}

fn compile_rule(rule: &PolicyRule) -> Option<CompiledRule> {
    let cidrs = rule
        .cidrs
        .as_ref()
        .map(|items| items.iter().filter_map(compile_cidr).collect());
    let ports: Vec<_> = rule.ports.iter().filter_map(compile_port).collect();
    let dropped_ports = rule.dropped_ports || ports.len() < rule.ports.len();
    Some(CompiledRule {
        endpoints: rule.endpoints.clone(),
        entities: rule.entities.clone(),
        cidrs,
        ports,
        http: rule.http.clone(),
        unmodelled_l3: rule.unmodelled_l3,
        dropped_ports,
    })
}

fn compile_cidr(rule: &CidrRule) -> Option<CompiledCidr> {
    let network = IpNetwork::parse(&rule.cidr)?;
    let exclusions: Option<Vec<_>> = rule
        .except
        .iter()
        .map(|value| {
            let exclusion = IpNetwork::parse(value)?;
            network.contains_network(exclusion).then_some(exclusion)
        })
        .collect();
    Some(CompiledCidr {
        cidr: rule.cidr.clone(),
        except: rule.except.clone(),
        network,
        exclusions: exclusions?,
    })
}

fn compile_port(port: &PortRule) -> Option<CompiledPort> {
    match (port.port, port.end_port, port.name.as_deref()) {
        (None, None, None) => Some(CompiledPort::Protocol {
            protocol: port.protocol,
        }),
        (Some(first), end, _) => {
            if first == 0 {
                return None;
            }
            let last = match end {
                Some(last) if last >= first => last,
                None => first,
                Some(_) => return None,
            };
            Some(CompiledPort::Number {
                protocol: port.protocol,
                first,
                last,
            })
        }
        (None, None, Some(name)) if !name.is_empty() => Some(CompiledPort::Named {
            protocol: port.protocol,
            name: name.to_string(),
        }),
        _ => None,
    }
}

fn policy_selects(policy: &CompiledPolicy, endpoint: &EndpointRef) -> bool {
    if !policy.clusterwide && policy.namespace != endpoint.namespace {
        return false;
    }
    selector_matches_endpoint(&policy.endpoint_selector, endpoint)
}

impl CompiledRule {
    fn matches(
        &self,
        policy: &CompiledPolicy,
        peer: &EndpointRef,
        destination: &EndpointRef,
        traffic: &Traffic,
    ) -> bool {
        self.l3_matches(policy, peer)
            && self.l4_matches(destination, traffic)
            && self.l7_matches(traffic)
    }

    /// Whether content this module fails closed on could have admitted the
    /// peer: an unmodelled L3 selector, or dropped ports on an L3 match.
    fn allow_unproven(&self, policy: &CompiledPolicy, peer: &EndpointRef) -> bool {
        if self.unmodelled_l3 {
            return true;
        }
        self.dropped_ports && self.l3_matches(policy, peer)
    }

    fn l3_specified(&self) -> bool {
        self.endpoints.is_some()
            || self.entities.is_some()
            || self.cidrs.is_some()
            || self.unmodelled_l3
    }

    fn l3_matches(&self, policy: &CompiledPolicy, peer: &EndpointRef) -> bool {
        if !self.l3_specified() {
            return true;
        }
        let endpoints = self.endpoints.as_ref().is_some_and(|selectors| {
            selectors.iter().any(|selector| {
                endpoint_selector_matches(selector, peer, &policy.namespace, policy.clusterwide)
            })
        });
        let entities = self
            .entities
            .as_ref()
            .is_some_and(|entities| entities.iter().any(|entity| entity_matches(*entity, peer)));
        let cidrs = self.cidrs.as_ref().is_some_and(|cidrs| {
            peer.ips.iter().any(|ip| {
                cidrs.iter().any(|block| {
                    block.network.contains(*ip)
                        && !block.exclusions.iter().any(|except| except.contains(*ip))
                })
            })
        });
        endpoints || entities || cidrs
    }

    fn l4_matches(&self, destination: &EndpointRef, traffic: &Traffic) -> bool {
        if self.ports.is_empty() {
            // Declared ports that did not compile never widen to every port.
            return !self.dropped_ports;
        }
        self.ports
            .iter()
            .any(|port| port.matches(destination, traffic))
    }

    fn l7_matches(&self, traffic: &Traffic) -> bool {
        if self.http.is_empty() {
            return true;
        }
        let Some(l7) = traffic.l7.as_ref() else {
            return true;
        };
        self.http.iter().any(|rule| http_matches(rule, l7))
    }
}

impl CompiledPort {
    fn matches(&self, destination: &EndpointRef, traffic: &Traffic) -> bool {
        match self {
            Self::Number {
                protocol,
                first,
                last,
            } => protocol.admits(traffic.protocol) && (*first..=*last).contains(&traffic.port),
            Self::Named { protocol, name } => {
                protocol.admits(traffic.protocol)
                    && destination.ports.iter().any(|port| {
                        port.name == *name
                            && protocol.admits(port.protocol)
                            && port.port == traffic.port
                    })
            }
            Self::Protocol { protocol } => protocol.admits(traffic.protocol),
        }
    }
}

fn http_matches(rule: &DeclaredL7, traffic: &DeclaredL7) -> bool {
    let method_ok =
        rule.method.is_empty() || rule.method.eq_ignore_ascii_case(traffic.method.trim());
    let path_ok = rule.path.is_empty() || path_matches(&rule.path, &traffic.path);
    method_ok && path_ok
}

fn path_matches(declared: &str, traffic: &str) -> bool {
    traffic == declared || traffic.starts_with(declared)
}

fn endpoint_selector_matches(
    selector: &LabelSelector,
    peer: &EndpointRef,
    policy_namespace: &str,
    clusterwide: bool,
) -> bool {
    if !clusterwide && !selector_mentions_namespace(selector) && peer.namespace != policy_namespace
    {
        return false;
    }
    selector_matches_endpoint(selector, peer)
}

fn selector_mentions_namespace(selector: &LabelSelector) -> bool {
    selector
        .match_labels
        .keys()
        .any(|key| mentions_namespace_key(key))
        || selector
            .match_expressions
            .iter()
            .any(|expr| mentions_namespace_key(&expr.key))
}

/// The exact pod-namespace label, which `label_get_on` may satisfy from
/// `EndpointRef::namespace`. The prefixed namespace-labels family is not
/// this: those values come from labels on the Namespace object, never from
/// the namespace name.
fn is_namespace_key(key: &str) -> bool {
    strip_k8s(key) == NS_LABEL
}

/// Any key that names the peer namespace, lifting the same-namespace gate.
fn mentions_namespace_key(key: &str) -> bool {
    let key = strip_k8s(key);
    key == NS_LABEL || key.starts_with(NS_LABELS_PREFIX)
}

fn entity_matches(entity: Entity, peer: &EndpointRef) -> bool {
    match entity {
        Entity::All => true,
        Entity::World => is_world(peer),
        Entity::Host => has_reserved(peer, ID_HOST, "reserved:host"),
        Entity::RemoteNode => has_reserved(peer, ID_REMOTE_NODE, "reserved:remote-node"),
        Entity::Init => has_reserved(peer, ID_INIT, "reserved:init"),
        Entity::Ingress => has_reserved(peer, ID_INGRESS, "reserved:ingress"),
        Entity::Unmanaged => has_reserved(peer, ID_UNMANAGED, "reserved:unmanaged"),
        Entity::KubeApiserver => has_reserved(peer, ID_KUBE_APISERVER, "reserved:kube-apiserver"),
        Entity::Health => has_reserved(peer, 4, "reserved:health"),
        Entity::Cluster => !is_world(peer),
    }
}

fn is_world(peer: &EndpointRef) -> bool {
    matches!(
        peer.identity_id,
        Some(ID_WORLD | ID_WORLD_IPV4 | ID_WORLD_IPV6)
    ) || has_label(peer, "reserved:world")
        || has_label(peer, "reserved:world-ipv4")
        || has_label(peer, "reserved:world-ipv6")
}

fn has_reserved(peer: &EndpointRef, id: i64, label: &str) -> bool {
    peer.identity_id == Some(id) || has_label(peer, label)
}

fn has_label(peer: &EndpointRef, key: &str) -> bool {
    label_get(&peer.labels, key).is_some()
}

/// Unknown operators fail closed so a malformed selector cannot create an allow.
pub fn selector_matches(selector: &LabelSelector, labels: &BTreeMap<String, String>) -> bool {
    selector_matches_with(selector, |key| label_get(labels, key))
}

fn selector_matches_endpoint(selector: &LabelSelector, endpoint: &EndpointRef) -> bool {
    selector_matches_with(selector, |key| label_get_on(endpoint, key))
}

fn selector_matches_with<'a>(
    selector: &LabelSelector,
    get: impl Fn(&str) -> Option<&'a str>,
) -> bool {
    for (key, value) in &selector.match_labels {
        if get(key) != Some(value.as_str()) {
            return false;
        }
    }
    for expression in &selector.match_expressions {
        let value = get(&expression.key);
        let matched = match expression.operator.as_str() {
            "In" => value.is_some_and(|value| expression.values.iter().any(|item| item == value)),
            "NotIn" => value.is_none_or(|value| expression.values.iter().all(|item| item != value)),
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

fn label_get<'a>(labels: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    if let Some(value) = labels.get(key) {
        return Some(value.as_str());
    }
    let stripped = strip_k8s(key);
    if stripped != key {
        if let Some(value) = labels.get(stripped) {
            return Some(value.as_str());
        }
    }
    let prefixed = format!("k8s:{stripped}");
    labels.get(&prefixed).map(String::as_str)
}

fn label_get_on<'a>(endpoint: &'a EndpointRef, key: &str) -> Option<&'a str> {
    if let Some(value) = label_get(&endpoint.labels, key) {
        return Some(value);
    }
    if is_namespace_key(key) && !endpoint.namespace.is_empty() {
        return Some(endpoint.namespace.as_str());
    }
    None
}

fn strip_k8s(key: &str) -> &str {
    key.strip_prefix("k8s:").unwrap_or(key)
}

fn same_endpoint(a: &EndpointRef, b: &EndpointRef) -> bool {
    if !a.uid.is_empty() && a.uid == b.uid {
        return true;
    }
    !a.namespace.is_empty() && a.namespace == b.namespace && !a.name.is_empty() && a.name == b.name
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

#[derive(Deserialize, Default)]
struct WireRule {
    #[serde(default, rename = "endpointSelector")]
    endpoint_selector: WireSelector,
    #[serde(default, rename = "nodeSelector")]
    node_selector: Option<WireSelector>,
    #[serde(default, rename = "enableDefaultDeny")]
    enable_default_deny: WireDefaultDeny,
    #[serde(default)]
    ingress: Option<Vec<WireFlow>>,
    #[serde(default)]
    egress: Option<Vec<WireFlow>>,
}

#[derive(Deserialize, Default)]
struct WireDefaultDeny {
    #[serde(default)]
    ingress: Option<bool>,
    #[serde(default)]
    egress: Option<bool>,
}

#[derive(Deserialize, Default, Clone)]
struct WireSelector {
    #[serde(default, rename = "matchLabels")]
    match_labels: BTreeMap<String, String>,
    #[serde(default, rename = "matchExpressions")]
    match_expressions: Vec<WireExpression>,
}

#[derive(Deserialize, Default, Clone)]
struct WireExpression {
    #[serde(default)]
    key: String,
    #[serde(default)]
    operator: String,
    #[serde(default)]
    values: Vec<String>,
}

#[derive(Deserialize, Default)]
struct WireFlow {
    #[serde(default, rename = "fromEndpoints")]
    from_endpoints: Option<Vec<WireSelector>>,
    #[serde(default, rename = "toEndpoints")]
    to_endpoints: Option<Vec<WireSelector>>,
    #[serde(default, rename = "fromEntities")]
    from_entities: Option<Vec<String>>,
    #[serde(default, rename = "toEntities")]
    to_entities: Option<Vec<String>>,
    #[serde(default, rename = "fromCIDRSet")]
    from_cidr_set: Option<Vec<WireCidr>>,
    #[serde(default, rename = "toCIDRSet")]
    to_cidr_set: Option<Vec<WireCidr>>,
    #[serde(default, rename = "fromCIDR")]
    from_cidr: Option<Vec<String>>,
    #[serde(default, rename = "toCIDR")]
    to_cidr: Option<Vec<String>>,
    #[serde(default, rename = "toPorts")]
    to_ports: Option<Vec<WireToPort>>,
    // L3 selector kinds this module does not evaluate. Their presence is
    // kept so the rule fails closed instead of matching every peer.
    #[serde(default, rename = "fromNodes")]
    from_nodes: Option<Value>,
    #[serde(default, rename = "fromRequires")]
    from_requires: Option<Value>,
    #[serde(default, rename = "toNodes")]
    to_nodes: Option<Value>,
    #[serde(default, rename = "toRequires")]
    to_requires: Option<Value>,
    #[serde(default, rename = "toServices")]
    to_services: Option<Value>,
    #[serde(default, rename = "toFQDNs")]
    to_fqdns: Option<Value>,
    #[serde(default, rename = "toGroups")]
    to_groups: Option<Value>,
}

#[derive(Deserialize, Default)]
struct WireCidr {
    #[serde(default)]
    cidr: String,
    #[serde(default)]
    except: Vec<String>,
}

#[derive(Deserialize, Default)]
struct WireToPort {
    #[serde(default)]
    ports: Vec<WirePort>,
    #[serde(default)]
    rules: WireL7,
}

#[derive(Deserialize, Default)]
struct WirePort {
    #[serde(default)]
    port: Value,
    #[serde(default, rename = "endPort")]
    end_port: Option<i64>,
    #[serde(default)]
    protocol: String,
}

#[derive(Deserialize, Default)]
struct WireL7 {
    #[serde(default)]
    http: Vec<WireHttp>,
}

#[derive(Deserialize, Default)]
struct WireHttp {
    #[serde(default)]
    method: String,
    #[serde(default)]
    path: String,
}

fn selector_from_wire(wire: &WireSelector) -> LabelSelector {
    LabelSelector {
        match_labels: wire.match_labels.clone(),
        match_expressions: wire
            .match_expressions
            .iter()
            .map(|expr| LabelExpression {
                key: expr.key.clone(),
                operator: expr.operator.clone(),
                values: expr.values.clone(),
            })
            .collect(),
    }
}

fn compile_ingress_wire(rule: &WireFlow) -> PolicyRule {
    flow_from_wire(
        rule.from_endpoints.as_ref(),
        rule.from_entities.as_ref(),
        rule.from_cidr_set.as_ref(),
        rule.from_cidr.as_ref(),
        rule.to_ports.as_ref(),
        rule.from_nodes.is_some() || rule.from_requires.is_some(),
    )
}

fn compile_egress_wire(rule: &WireFlow) -> PolicyRule {
    flow_from_wire(
        rule.to_endpoints.as_ref(),
        rule.to_entities.as_ref(),
        rule.to_cidr_set.as_ref(),
        rule.to_cidr.as_ref(),
        rule.to_ports.as_ref(),
        rule.to_nodes.is_some()
            || rule.to_requires.is_some()
            || rule.to_services.is_some()
            || rule.to_fqdns.is_some()
            || rule.to_groups.is_some(),
    )
}

fn flow_from_wire(
    endpoints: Option<&Vec<WireSelector>>,
    entities: Option<&Vec<String>>,
    cidr_set: Option<&Vec<WireCidr>>,
    cidr: Option<&Vec<String>>,
    to_ports: Option<&Vec<WireToPort>>,
    unmodelled_l3: bool,
) -> PolicyRule {
    let mut cidrs = None;
    if cidr_set.is_some() || cidr.is_some() {
        let mut items = Vec::new();
        if let Some(set) = cidr_set {
            for block in set {
                if block.cidr.is_empty() {
                    continue;
                }
                items.push(CidrRule {
                    cidr: block.cidr.clone(),
                    except: block.except.clone(),
                });
            }
        }
        if let Some(plain) = cidr {
            for value in plain {
                if value.is_empty() {
                    continue;
                }
                items.push(CidrRule {
                    cidr: value.clone(),
                    except: Vec::new(),
                });
            }
        }
        cidrs = Some(items);
    }
    let (ports, http, dropped_ports) = ports_from_wire(to_ports);
    PolicyRule {
        endpoints: endpoints.map(|items| items.iter().map(selector_from_wire).collect()),
        entities: entities.map(|items| {
            items
                .iter()
                .filter_map(|name| Entity::parse(name))
                .collect()
        }),
        cidrs,
        ports,
        http,
        unmodelled_l3,
        dropped_ports,
    }
}

fn ports_from_wire(to_ports: Option<&Vec<WireToPort>>) -> (Vec<PortRule>, Vec<DeclaredL7>, bool) {
    let Some(rules) = to_ports else {
        return (Vec::new(), Vec::new(), false);
    };
    let mut ports = Vec::new();
    let mut http = Vec::new();
    let mut dropped = false;
    for rule in rules {
        for http_rule in &rule.rules.http {
            http.push(DeclaredL7 {
                method: http_rule.method.clone(),
                path: http_rule.path.clone(),
            });
        }
        if rule.ports.is_empty() {
            continue;
        }
        for port in &rule.ports {
            let Some(protocol) = Protocol::from_api(Some(port.protocol.as_str())) else {
                dropped = true;
                continue;
            };
            let (number, name) = port_value(&port.port);
            // A malformed port is not an omitted one: it must not compile
            // into a protocol-wide entry.
            if number.is_none() && name.is_none() && !port.port.is_null() {
                dropped = true;
                continue;
            }
            ports.push(PortRule {
                protocol,
                port: number,
                end_port: port
                    .end_port
                    .and_then(|value| u16::try_from(value).ok())
                    .filter(|value| *value != 0),
                name,
            });
        }
    }
    (ports, http, dropped)
}

fn port_value(value: &Value) -> (Option<u16>, Option<String>) {
    if let Some(number) = value.as_i64().and_then(|n| u16::try_from(n).ok()) {
        return (Some(number).filter(|n| *n != 0), None);
    }
    if let Some(number) = value.as_u64().and_then(|n| u16::try_from(n).ok()) {
        return (Some(number).filter(|n| *n != 0), None);
    }
    let Some(text) = value.as_str().filter(|s| !s.is_empty()) else {
        return (None, None);
    };
    if let Ok(number) = text.parse::<u16>() {
        return (Some(number).filter(|n| *n != 0), None);
    }
    (None, Some(text.to_string()))
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
#[path = "cilium_policy_test.rs"]
mod tests;
