//! Node and capacity view: allocatable versus requested versus used.
//!
//! Nodes are listed whole (they are few and carry no secrets), requests are
//! summed from each node's non-terminated pods using the scheduler's
//! effective-request rule (init containers sequence, sidecars accumulate),
//! and usage comes from `metrics.k8s.io` only when the cluster serves it --
//! absent metrics make the usage columns invisible, not broken, and install
//! nothing. The "PDB blocked" column joins `PodDisruptionBudget`s to each
//! node's pods -- how many pods sit under a budget with zero disruptions
//! allowed -- and follows the same rule: if the budgets are unreadable,
//! absent, or too many to list completely, the column disappears rather
//! than under-count. Every fetch is bounded (`limit` plus a `truncated`
//! flag) and a node whose pods cannot be listed shows `?` cells rather
//! than a guess.

use std::collections::{BTreeMap, HashMap};

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Container, Node, Pod, PodSpec};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::Client;
use kube::api::{Api, ListParams, Request};
use serde::Deserialize;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::{Fetched, classify};

const NODE_LIMIT: u32 = 500;
const PODS_PER_NODE_LIMIT: u32 = 1_500;
const CONCURRENT_NODE_SCANS: usize = 8;
const PDB_LIMIT: u32 = 1_000;

pub(crate) async fn fetch_node_table(client: &Client) -> Fetched<TablePage> {
    let api: Api<Node> = Api::all(client.clone());
    let nodes = match api.list(&ListParams::default().limit(NODE_LIMIT)).await {
        Ok(list) => list,
        Err(error) => return classify("nodes", &error),
    };
    let truncated = nodes
        .metadata
        .continue_
        .as_deref()
        .is_some_and(|token| !token.is_empty());
    let usage = fetch_usage(client).await;
    let budgets = fetch_blocked_budgets(client).await;

    let scanned: Vec<(Node, Result<Load, kube::Error>)> =
        futures::stream::iter(nodes.items.into_iter().map(|node| {
            let client = client.clone();
            let budgets = budgets.as_deref();
            async move {
                let name = node.metadata.name.clone().unwrap_or_default();
                let load = load_on(&client, &name, budgets).await;
                (node, load)
            }
        }))
        .buffered(CONCURRENT_NODE_SCANS)
        .collect()
        .await;

    let mut columns = vec![
        column("Name"),
        column("Status"),
        column("Roles"),
        column("Version"),
        column("Pods"),
        column("CPU req"),
        column("Memory req"),
    ];
    if usage.is_some() {
        columns.push(column("CPU use"));
        columns.push(column("Memory use"));
    }
    if budgets.is_some() {
        columns.push(column("PDB blocked"));
    }
    columns.push(column("Taints"));

    let rows = scanned
        .into_iter()
        .map(|(node, load)| row(node, load, usage.as_ref(), budgets.is_some()))
        .collect();
    Fetched::Ok(TablePage {
        columns,
        rows,
        truncated,
        // The rows are computed per node, so a continue token could not
        // resume this table; more nodes than the limit means "narrow it".
        continue_token: None,
    })
}

fn column(name: &str) -> TableColumn {
    TableColumn {
        name: name.to_string(),
        wide: false,
    }
}

struct Load {
    pods: usize,
    cpu_millis: i64,
    mem_bytes: i64,
    pdb_blocked: usize,
}

async fn load_on(
    client: &Client,
    node: &str,
    budgets: Option<&[BlockedBudget]>,
) -> Result<Load, kube::Error> {
    let api: Api<Pod> = Api::all(client.clone());
    let params = ListParams::default()
        .fields(&format!(
            "spec.nodeName={node},status.phase!=Succeeded,status.phase!=Failed"
        ))
        .limit(PODS_PER_NODE_LIMIT);
    let pods = api.list(&params).await?;
    let mut load = Load {
        pods: pods.items.len(),
        cpu_millis: 0,
        mem_bytes: 0,
        pdb_blocked: 0,
    };
    let empty = BTreeMap::new();
    for pod in &pods.items {
        if let Some(spec) = &pod.spec {
            load.cpu_millis += effective_request(spec, "cpu", parse_cpu_millis);
            load.mem_bytes += effective_request(spec, "memory", parse_bytes);
        }
        if let Some(budgets) = budgets {
            let namespace = pod.metadata.namespace.as_deref().unwrap_or_default();
            let labels = pod.metadata.labels.as_ref().unwrap_or(&empty);
            if budgets
                .iter()
                .any(|budget| budget.namespace == namespace && budget.selects(labels))
            {
                load.pdb_blocked += 1;
            }
        }
    }
    Ok(load)
}

// A budget that currently blocks eviction: `status.disruptionsAllowed == 0`.
// Only these are held; a budget with headroom cannot block anything.
struct BlockedBudget {
    namespace: String,
    selector: Option<LabelSelector>,
}

impl BlockedBudget {
    fn selects(&self, labels: &BTreeMap<String, String>) -> bool {
        selector_matches(self.selector.as_ref(), labels)
    }
}

// None means the column must not exist: the kind is absent, unreadable, or
// the list was too large to be complete -- an under-count would be a wrong
// answer wearing a plausible one's clothes.
async fn fetch_blocked_budgets(client: &Client) -> Option<Vec<BlockedBudget>> {
    let api: Api<PodDisruptionBudget> = Api::all(client.clone());
    let list = api
        .list(&ListParams::default().limit(PDB_LIMIT))
        .await
        .ok()?;
    if list
        .metadata
        .continue_
        .as_deref()
        .is_some_and(|token| !token.is_empty())
    {
        return None;
    }
    Some(
        list.items
            .into_iter()
            .filter(|pdb| {
                pdb.status
                    .as_ref()
                    .is_some_and(|status| status.disruptions_allowed == 0)
            })
            .map(|pdb| BlockedBudget {
                namespace: pdb.metadata.namespace.unwrap_or_default(),
                selector: pdb.spec.and_then(|spec| spec.selector),
            })
            .collect(),
    )
}

// policy/v1 semantics: a nil selector selects no pods, an empty one selects
// every pod in the namespace. An operator we do not know does not match --
// Kubernetes defines exactly these four.
fn selector_matches(selector: Option<&LabelSelector>, labels: &BTreeMap<String, String>) -> bool {
    let Some(selector) = selector else {
        return false;
    };
    for (key, value) in selector.match_labels.as_ref().unwrap_or(&BTreeMap::new()) {
        if labels.get(key) != Some(value) {
            return false;
        }
    }
    for expression in selector.match_expressions.as_deref().unwrap_or_default() {
        let value = labels.get(&expression.key);
        let values = expression.values.as_deref().unwrap_or_default();
        let matched = match expression.operator.as_str() {
            "In" => value.is_some_and(|v| values.iter().any(|x| x == v)),
            "NotIn" => value.is_none_or(|v| !values.iter().any(|x| x == v)),
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

fn row(
    node: Node,
    load: Result<Load, kube::Error>,
    usage: Option<&Usage>,
    show_pdb: bool,
) -> TableRow {
    let name = node.metadata.name.clone().unwrap_or_default();
    let uid = node.metadata.uid.clone().unwrap_or_default();
    let status = node.status.as_ref();
    let allocatable = status.and_then(|s| s.allocatable.as_ref());
    let quantity = |key: &str| {
        allocatable
            .and_then(|map| map.get(key))
            .map(|q| q.0.as_str())
    };
    let cpu_alloc = quantity("cpu").and_then(parse_cpu_millis);
    let mem_alloc = quantity("memory").and_then(parse_bytes);
    let pods_alloc = quantity("pods").and_then(|q| q.parse::<i64>().ok());

    let mut cells = vec![
        name.clone(),
        status_text(&node),
        roles_text(&node),
        status
            .and_then(|s| s.node_info.as_ref())
            .map(|info| info.kubelet_version.clone())
            .unwrap_or_default(),
    ];
    match &load {
        Ok(load) => {
            cells.push(counted(load.pods as i64, pods_alloc, |n| n.to_string()));
            cells.push(counted(load.cpu_millis, cpu_alloc, fmt_cpu));
            cells.push(counted(load.mem_bytes, mem_alloc, fmt_bytes));
        }
        Err(_) => {
            cells.push("?".to_string());
            cells.push("?".to_string());
            cells.push("?".to_string());
        }
    }
    if let Some(usage) = usage {
        let used = usage.get(&name);
        cells.push(counted_opt(
            used.and_then(|u| parse_cpu_millis(&u.cpu)),
            cpu_alloc,
            fmt_cpu,
        ));
        cells.push(counted_opt(
            used.and_then(|u| parse_bytes(&u.memory)),
            mem_alloc,
            fmt_bytes,
        ));
    }
    if show_pdb {
        cells.push(match &load {
            Ok(load) => load.pdb_blocked.to_string(),
            Err(_) => "?".to_string(),
        });
    }
    cells.push(
        node.spec
            .as_ref()
            .and_then(|s| s.taints.as_ref())
            .map(|t| t.len())
            .unwrap_or(0)
            .to_string(),
    );
    TableRow {
        cells,
        name,
        namespace: None,
        uid,
    }
}

fn counted(value: i64, allocatable: Option<i64>, fmt: impl Fn(i64) -> String) -> String {
    match allocatable {
        Some(alloc) if alloc > 0 => {
            format!(
                "{}/{} ({}%)",
                fmt(value),
                fmt(alloc),
                (value * 100 + alloc / 2) / alloc
            )
        }
        _ => fmt(value),
    }
}

fn counted_opt(
    value: Option<i64>,
    allocatable: Option<i64>,
    fmt: impl Fn(i64) -> String,
) -> String {
    match value {
        Some(value) => counted(value, allocatable, fmt),
        None => "?".to_string(),
    }
}

fn status_text(node: &Node) -> String {
    let mut parts: Vec<String> = Vec::new();
    let conditions = node
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref())
        .unwrap_or_default();
    let ready = conditions.iter().find(|c| c.type_ == "Ready");
    parts.push(match ready.map(|c| c.status.as_str()) {
        Some("True") => "Ready".to_string(),
        Some("False") => "NotReady".to_string(),
        _ => "Unknown".to_string(),
    });
    for condition in conditions {
        if condition.type_ != "Ready" && condition.status == "True" {
            parts.push(condition.type_.clone());
        }
    }
    if node
        .spec
        .as_ref()
        .and_then(|s| s.unschedulable)
        .unwrap_or(false)
    {
        parts.push("SchedulingDisabled".to_string());
    }
    parts.join(",")
}

fn roles_text(node: &Node) -> String {
    const PREFIX: &str = "node-role.kubernetes.io/";
    let mut roles: Vec<&str> = node
        .metadata
        .labels
        .as_ref()
        .map(|labels| {
            labels
                .keys()
                .filter_map(|key| key.strip_prefix(PREFIX))
                .filter(|role| !role.is_empty())
                .collect()
        })
        .unwrap_or_default();
    roles.sort_unstable();
    if roles.is_empty() {
        "<none>".to_string()
    } else {
        roles.join(",")
    }
}

// The scheduler's effective request: init containers run in sequence (the
// largest one is the floor), restartable init containers -- sidecars -- keep
// running and accumulate on top of the app containers.
fn effective_request(spec: &PodSpec, key: &str, parse: fn(&str) -> Option<i64>) -> i64 {
    let request = |container: &Container| {
        container
            .resources
            .as_ref()
            .and_then(|r| r.requests.as_ref())
            .and_then(|requests| requests.get(key))
            .and_then(|quantity| parse(&quantity.0))
            .unwrap_or(0)
    };
    let regular: i64 = spec.containers.iter().map(&request).sum();
    let mut sidecars = 0;
    let mut max_init = 0;
    for container in spec.init_containers.as_deref().unwrap_or_default() {
        if container.restart_policy.as_deref() == Some("Always") {
            sidecars += request(container);
        } else {
            max_init = max_init.max(request(container));
        }
    }
    sidecars + regular.max(max_init)
}

type Usage = HashMap<String, WireUsage>;

#[derive(Deserialize)]
struct WireMetricsList {
    #[serde(default)]
    items: Vec<WireNodeMetrics>,
}

#[derive(Deserialize)]
struct WireNodeMetrics {
    #[serde(default)]
    metadata: WireName,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize, Default)]
struct WireName {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
pub(crate) struct WireUsage {
    #[serde(default)]
    cpu: String,
    #[serde(default)]
    memory: String,
}

async fn fetch_usage(client: &Client) -> Option<Usage> {
    let request = Request::new("/apis/metrics.k8s.io/v1beta1/nodes")
        .list(&ListParams::default())
        .ok()?;
    let list: WireMetricsList = client.request(request).await.ok()?;
    Some(
        list.items
            .into_iter()
            .map(|item| (item.metadata.name, item.usage))
            .collect(),
    )
}

// Kubernetes quantities: a decimal (exponents allowed) with an optional
// binary (Ki..Ei), decimal (k..E), or sub-unit (n, u, m) suffix. Parsed
// through f64 -- display precision, not arithmetic precision.
fn parse_quantity(text: &str) -> Option<f64> {
    const SUFFIXES: [(&str, f64); 15] = [
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", 1024.0 * 1024.0 * 1024.0),
        ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("Pi", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("Ei", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ];
    let text = text.trim();
    for (suffix, multiplier) in SUFFIXES {
        if let Some(number) = text.strip_suffix(suffix) {
            // A bare exponent like "12e3" must not lose its "E" to the exa
            // suffix: the remainder has to still parse as a number.
            if let Ok(value) = number.parse::<f64>() {
                return Some(value * multiplier);
            }
        }
    }
    text.parse::<f64>().ok()
}

pub(crate) fn parse_cpu_millis(text: &str) -> Option<i64> {
    parse_quantity(text).map(|value| (value * 1000.0).round() as i64)
}

pub(crate) fn parse_bytes(text: &str) -> Option<i64> {
    parse_quantity(text).map(|value| value.round() as i64)
}

fn fmt_cpu(millis: i64) -> String {
    if millis % 1000 == 0 {
        (millis / 1000).to_string()
    } else {
        format!("{millis}m")
    }
}

fn fmt_bytes(bytes: i64) -> String {
    const KI: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KI * KI * KI {
        format!("{:.1}Gi", b / (KI * KI * KI))
    } else if b >= KI * KI {
        format!("{:.0}Mi", b / (KI * KI))
    } else if b >= KI {
        format!("{:.0}Ki", b / KI)
    } else {
        bytes.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

    #[test]
    fn quantities_parse_across_cpu_and_memory_grammars() {
        assert_eq!(parse_cpu_millis("2"), Some(2000));
        assert_eq!(parse_cpu_millis("1500m"), Some(1500));
        assert_eq!(parse_cpu_millis("0.5"), Some(500));
        assert_eq!(parse_cpu_millis("156340764n"), Some(156));
        assert_eq!(parse_cpu_millis("250u"), Some(0));
        assert_eq!(parse_bytes("128974848"), Some(128974848));
        assert_eq!(parse_bytes("64Mi"), Some(64 * 1024 * 1024));
        assert_eq!(parse_bytes("16Gi"), Some(16 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("1234Ki"), Some(1234 * 1024));
        assert_eq!(parse_bytes("5G"), Some(5_000_000_000));
        assert_eq!(parse_bytes("12e3"), Some(12_000), "exponent, not exa");
        assert_eq!(parse_bytes("129e6"), Some(129_000_000));
        assert_eq!(parse_bytes(""), None);
        assert_eq!(parse_bytes("Gi"), None);
        assert_eq!(parse_bytes("banana"), None);
    }

    #[test]
    fn formatting_round_trips_the_common_shapes() {
        assert_eq!(fmt_cpu(2000), "2");
        assert_eq!(fmt_cpu(1500), "1500m");
        assert_eq!(fmt_bytes(16 * 1024 * 1024 * 1024), "16.0Gi");
        assert_eq!(fmt_bytes(64 * 1024 * 1024), "64Mi");
        assert_eq!(fmt_bytes(512), "512");
    }

    fn container(name: &str, cpu: &str, restart_policy: Option<&str>) -> Container {
        Container {
            name: name.to_string(),
            restart_policy: restart_policy.map(str::to_string),
            resources: Some(ResourceRequirements {
                requests: Some(
                    [("cpu".to_string(), Quantity(cpu.to_string()))]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_effective_request_takes_the_init_floor_and_accumulates_sidecars() {
        let spec = PodSpec {
            containers: vec![
                container("app", "500m", None),
                container("proxy", "250m", None),
            ],
            init_containers: Some(vec![
                container("migrate", "2", None),
                container("sidecar-log", "100m", Some("Always")),
            ]),
            ..Default::default()
        };
        assert_eq!(
            effective_request(&spec, "cpu", parse_cpu_millis),
            100 + 2000,
            "the big init container is the floor, plus the running sidecar"
        );

        let steady = PodSpec {
            containers: vec![container("app", "500m", None)],
            init_containers: Some(vec![container("sidecar-log", "100m", Some("Always"))]),
            ..Default::default()
        };
        assert_eq!(effective_request(&steady, "cpu", parse_cpu_millis), 600);

        let missing = PodSpec {
            containers: vec![Container {
                name: "bare".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(effective_request(&missing, "cpu", parse_cpu_millis), 0);
    }

    #[test]
    fn node_status_reads_ready_pressure_and_cordon_state() {
        use k8s_openapi::api::core::v1::{NodeCondition, NodeSpec, NodeStatus};
        let node = |ready: &str, pressure: bool, unschedulable: bool| Node {
            status: Some(NodeStatus {
                conditions: Some(vec![
                    NodeCondition {
                        type_: "Ready".to_string(),
                        status: ready.to_string(),
                        ..Default::default()
                    },
                    NodeCondition {
                        type_: "MemoryPressure".to_string(),
                        status: if pressure { "True" } else { "False" }.to_string(),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            spec: Some(NodeSpec {
                unschedulable: Some(unschedulable),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(status_text(&node("True", false, false)), "Ready");
        assert_eq!(
            status_text(&node("False", true, true)),
            "NotReady,MemoryPressure,SchedulingDisabled"
        );
        assert_eq!(status_text(&Node::default()), "Unknown");
    }

    #[test]
    fn roles_come_from_the_role_labels_sorted_or_are_absent() {
        let mut node = Node::default();
        node.metadata.labels = Some(
            [
                ("node-role.kubernetes.io/worker".to_string(), String::new()),
                (
                    "node-role.kubernetes.io/control-plane".to_string(),
                    String::new(),
                ),
                ("kubernetes.io/hostname".to_string(), "n1".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(roles_text(&node), "control-plane,worker");
        assert_eq!(roles_text(&Node::default()), "<none>");
    }

    #[test]
    fn pdb_selectors_follow_policy_v1_semantics_including_the_nil_empty_split() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;

        let labels = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let api = labels(&[("app", "api"), ("tier", "web")]);

        assert!(
            !selector_matches(None, &api),
            "a nil selector selects no pods"
        );
        assert!(
            selector_matches(Some(&LabelSelector::default()), &api),
            "an empty selector selects every pod in the namespace"
        );

        let by_label = LabelSelector {
            match_labels: Some(labels(&[("app", "api")])),
            ..Default::default()
        };
        assert!(selector_matches(Some(&by_label), &api));
        assert!(!selector_matches(
            Some(&by_label),
            &labels(&[("app", "web")])
        ));
        assert!(!selector_matches(Some(&by_label), &BTreeMap::new()));

        let expression = |key: &str, operator: &str, values: &[&str]| LabelSelector {
            match_expressions: Some(vec![LabelSelectorRequirement {
                key: key.to_string(),
                operator: operator.to_string(),
                values: (!values.is_empty())
                    .then(|| values.iter().map(|v| v.to_string()).collect()),
            }]),
            ..Default::default()
        };
        assert!(selector_matches(
            Some(&expression("app", "In", &["api", "job"])),
            &api
        ));
        assert!(!selector_matches(
            Some(&expression("app", "In", &["job"])),
            &api
        ));
        assert!(!selector_matches(
            Some(&expression("app", "NotIn", &["api"])),
            &api
        ));
        assert!(
            selector_matches(
                Some(&expression("app", "NotIn", &["api"])),
                &BTreeMap::new()
            ),
            "NotIn matches a pod without the key at all"
        );
        assert!(selector_matches(
            Some(&expression("tier", "Exists", &[])),
            &api
        ));
        assert!(!selector_matches(
            Some(&expression("gone", "Exists", &[])),
            &api
        ));
        assert!(selector_matches(
            Some(&expression("gone", "DoesNotExist", &[])),
            &api
        ));
        assert!(
            !selector_matches(Some(&expression("app", "Wildcard", &["*"])), &api),
            "an unknown operator never matches, so the count cannot be wrong upward"
        );
    }

    #[test]
    fn a_used_over_allocatable_cell_carries_the_percentage() {
        assert_eq!(counted(1500, Some(4000), fmt_cpu), "1500m/4 (38%)");
        assert_eq!(counted(12, Some(110), |n| n.to_string()), "12/110 (11%)");
        assert_eq!(counted(1500, None, fmt_cpu), "1500m");
        assert_eq!(counted_opt(None, Some(4000), fmt_cpu), "?");
    }
}
