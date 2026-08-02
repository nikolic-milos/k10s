//! Node and capacity view: allocatable versus requested versus used.
//!
//! Nodes are listed whole (they are few and carry no secrets), requests are
//! summed from each node's non-terminated pods using the scheduler's
//! effective-request rule (init containers sequence, sidecars accumulate),
//! and usage comes from `metrics.k8s.io` only when the cluster serves it --
//! absent metrics make the usage columns invisible, not broken, and install
//! nothing. Every fetch is bounded (`limit` plus a `truncated` flag) and a
//! node whose pods cannot be listed shows `?` cells rather than a guess.

use std::collections::HashMap;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Container, Node, Pod, PodSpec};
use kube::Client;
use kube::api::{Api, ListParams, Request};
use serde::Deserialize;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::{Fetched, classify};

const NODE_LIMIT: u32 = 500;
const PODS_PER_NODE_LIMIT: u32 = 1_500;
const CONCURRENT_NODE_SCANS: usize = 8;

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

    let scanned: Vec<(Node, Result<Load, kube::Error>)> =
        futures::stream::iter(nodes.items.into_iter().map(|node| {
            let client = client.clone();
            async move {
                let name = node.metadata.name.clone().unwrap_or_default();
                let load = load_on(&client, &name).await;
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
    columns.push(column("Taints"));

    let rows = scanned
        .into_iter()
        .map(|(node, load)| row(node, load, usage.as_ref()))
        .collect();
    Fetched::Ok(TablePage {
        columns,
        rows,
        truncated,
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
}

async fn load_on(client: &Client, node: &str) -> Result<Load, kube::Error> {
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
    };
    for pod in &pods.items {
        if let Some(spec) = &pod.spec {
            load.cpu_millis += effective_request(spec, "cpu", parse_cpu_millis);
            load.mem_bytes += effective_request(spec, "memory", parse_bytes);
        }
    }
    Ok(load)
}

fn row(node: Node, load: Result<Load, kube::Error>, usage: Option<&Usage>) -> TableRow {
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
    fn a_used_over_allocatable_cell_carries_the_percentage() {
        assert_eq!(counted(1500, Some(4000), fmt_cpu), "1500m/4 (38%)");
        assert_eq!(counted(12, Some(110), |n| n.to_string()), "12/110 (11%)");
        assert_eq!(counted(1500, None, fmt_cpu), "1500m");
        assert_eq!(counted_opt(None, Some(4000), fmt_cpu), "?");
    }
}
