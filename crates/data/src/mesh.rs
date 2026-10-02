//! Istio / Linkerd CRs and Envoy admin, only if the mesh is already there.
//!
//! A 404 on the API group is absence, not failure: the cluster does not run
//! that mesh, and k10s does not install one and does not ship a CNI. Listing
//! goes through [`kube::api::Request`]. Envoy admin is spoken to at
//! `http://127.0.0.1:PORT` after the shell has forwarded to the sidecar
//! (15000). This module never opens that forward.
//!
//! [`DeclaredReach`] is policy (NetworkPolicy or a mesh CR). [`ObservedReach`]
//! is telemetry already sitting in Prometheus (Hubble or mesh metrics). Mixing
//! those is a correctness bug, not a cosmetic one. Observed edges are built
//! from series labels a caller already has; Hubble itself is never scraped.

use std::time::Duration;

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::read::Fetched;

pub const ISTIO_GROUP: &str = "networking.istio.io";
pub const LINKERD_GROUP: &str = "linkerd.io";
pub const ENVOY_ADMIN_PORT: u16 = 15000;
pub const MAX_MESH_BYTES: usize = 8 << 20;
pub const MAX_OBJECTS: usize = 2_000;

const PAGE_LIMIT: u32 = 200;
const CONSULT_DEADLINE: Duration = Duration::from_secs(4);
const ISTIO_VERSION: &str = "v1";
const LINKERD_VERSION: &str = "v1alpha2";

const ISTIO_KINDS: &[MeshKind] = &[
    MeshKind::VirtualService,
    MeshKind::DestinationRule,
    MeshKind::Gateway,
    MeshKind::Sidecar,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshKind {
    VirtualService,
    DestinationRule,
    Gateway,
    Sidecar,
    ServiceProfile,
}

impl MeshKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MeshKind::VirtualService => "VirtualService",
            MeshKind::DestinationRule => "DestinationRule",
            MeshKind::Gateway => "Gateway",
            MeshKind::Sidecar => "Sidecar",
            MeshKind::ServiceProfile => "ServiceProfile",
        }
    }

    pub fn group(self) -> &'static str {
        match self {
            MeshKind::ServiceProfile => LINKERD_GROUP,
            _ => ISTIO_GROUP,
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            MeshKind::VirtualService => "virtualservices",
            MeshKind::DestinationRule => "destinationrules",
            MeshKind::Gateway => "gateways",
            MeshKind::Sidecar => "sidecars",
            MeshKind::ServiceProfile => "serviceprofiles",
        }
    }
}

/// 404 on the group document is [`GroupState::Absent`]: served false.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GroupState {
    Served,
    #[default]
    Absent,
    Denied,
    Failed {
        why: String,
    },
}

impl GroupState {
    pub fn is_served(&self) -> bool {
        matches!(self, GroupState::Served)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshObject {
    pub kind: MeshKind,
    pub namespace: String,
    pub name: String,
    pub hosts: Vec<String>,
    pub destinations: Vec<String>,
    pub gateways: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MeshInventory {
    pub istio: GroupState,
    pub linkerd: GroupState,
    pub objects: Vec<MeshObject>,
    pub truncated: bool,
}

impl MeshInventory {
    pub fn present(&self) -> bool {
        self.istio.is_served() || self.linkerd.is_served()
    }
}

/// Policy that *allows* a path. NetworkPolicy or a mesh CR, never telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyReason {
    pub kind: String,
    pub group: String,
    pub namespace: String,
    pub name: String,
}

/// Telemetry that *saw* a path. Hubble or mesh metrics already in Prometheus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryExporter {
    Hubble,
    Istio,
    Linkerd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryReason {
    pub metric: String,
    pub exporter: TelemetryExporter,
}

/// Declared connectivity: can reach, per policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredReach {
    pub from: String,
    pub to: String,
    pub because: PolicyReason,
}

/// Observed connectivity: did reach, per telemetry. Never a policy stand-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedReach {
    pub from: String,
    pub to: String,
    pub because: TelemetryReason,
}

/// Labels from a Prometheus series already in hand. This module does not scrape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesLabels {
    pub name: String,
    pub labels: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvoySummary {
    pub listeners: usize,
    pub clusters: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeshError {
    TooLarge { bytes: usize },
    NotJson(String),
    NotADump,
}

impl std::fmt::Display for MeshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeshError::TooLarge { bytes } => {
                write!(f, "mesh JSON is {bytes} bytes; the cap is {MAX_MESH_BYTES}")
            }
            MeshError::NotJson(why) => write!(f, "mesh JSON did not parse: {why}"),
            MeshError::NotADump => write!(f, "JSON is not an Envoy config_dump"),
        }
    }
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

/// Probe both groups and list the CRs they already serve.
pub async fn inventory(client: &Client) -> MeshInventory {
    let mut objects = Vec::new();
    let mut truncated = false;

    let (istio, istio_versions) = probe_group(client, ISTIO_GROUP, ISTIO_VERSION).await;
    if istio.is_served() {
        for kind in ISTIO_KINDS {
            let (items, more) = list_kind(client, ISTIO_GROUP, &istio_versions, *kind).await;
            truncated |= more;
            push_objects(&mut objects, items, &mut truncated);
            if objects.len() >= MAX_OBJECTS {
                break;
            }
        }
    }

    let (linkerd, linkerd_versions) = probe_group(client, LINKERD_GROUP, LINKERD_VERSION).await;
    if linkerd.is_served() && objects.len() < MAX_OBJECTS {
        let (items, more) = list_kind(
            client,
            LINKERD_GROUP,
            &linkerd_versions,
            MeshKind::ServiceProfile,
        )
        .await;
        truncated |= more;
        push_objects(&mut objects, items, &mut truncated);
    }

    MeshInventory {
        istio,
        linkerd,
        objects,
        truncated,
    }
}

fn push_objects(into: &mut Vec<MeshObject>, items: Vec<MeshObject>, truncated: &mut bool) {
    let room = MAX_OBJECTS.saturating_sub(into.len());
    if items.len() > room {
        *truncated = true;
        into.extend(items.into_iter().take(room));
    } else {
        into.extend(items);
    }
}

async fn probe_group(client: &Client, group: &str, fallback: &str) -> (GroupState, Vec<String>) {
    let path = format!("/apis/{group}");
    let request = match http::Request::get(&path).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => {
            return (
                GroupState::Failed {
                    why: error.to_string(),
                },
                vec![fallback.to_string()],
            );
        }
    };
    match tokio::time::timeout(CONSULT_DEADLINE, client.request::<Value>(request)).await {
        Err(_) => (
            GroupState::Failed {
                why: format!("{group} did not answer within 4 seconds"),
            },
            vec![fallback.to_string()],
        ),
        Ok(Ok(doc)) => (GroupState::Served, versions_of(&doc, fallback)),
        Ok(Err(error)) => (after_group(&error), vec![fallback.to_string()]),
    }
}

fn after_group(error: &kube::Error) -> GroupState {
    match error {
        kube::Error::Api(response) if response.code == 404 => GroupState::Absent,
        kube::Error::Api(response) if matches!(response.code, 401 | 403) => GroupState::Denied,
        other => GroupState::Failed {
            why: crate::connect::describe(other as &(dyn std::error::Error + 'static)),
        },
    }
}

fn versions_of(doc: &Value, fallback: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(preferred) = doc
        .pointer("/preferredVersion/version")
        .and_then(Value::as_str)
        && !preferred.is_empty()
    {
        out.push(preferred.to_string());
    }
    if let Some(versions) = doc.get("versions").and_then(Value::as_array) {
        for version in versions {
            let Some(name) = version.get("version").and_then(Value::as_str) else {
                continue;
            };
            if !name.is_empty() && !out.iter().any(|have| have == name) {
                out.push(name.to_string());
            }
        }
    }
    if out.is_empty() {
        out.push(fallback.to_string());
    }
    out
}

enum ListOnce {
    Ok(Vec<MeshObject>, bool),
    Missing,
    Stop,
}

async fn list_kind(
    client: &Client,
    group: &str,
    versions: &[String],
    kind: MeshKind,
) -> (Vec<MeshObject>, bool) {
    for version in versions {
        match list_once(client, group, version, kind).await {
            ListOnce::Ok(items, truncated) => return (items, truncated),
            ListOnce::Missing => continue,
            ListOnce::Stop => return (Vec::new(), false),
        }
    }
    (Vec::new(), false)
}

async fn list_once(client: &Client, group: &str, version: &str, kind: MeshKind) -> ListOnce {
    let path = format!("/apis/{group}/{version}/{}", kind.plural());
    let params = ListParams::default().limit(PAGE_LIMIT);
    let request = match Request::new(path).list(&params) {
        Ok(request) => request,
        Err(_) => return ListOnce::Stop,
    };
    match tokio::time::timeout(CONSULT_DEADLINE, client.request::<WireList>(request)).await {
        Err(_) => ListOnce::Stop,
        Ok(Ok(list)) => {
            let truncated = !list.metadata.cont.is_empty();
            let items = list
                .items
                .iter()
                .filter_map(|value| read_object(kind, value))
                .collect();
            ListOnce::Ok(items, truncated)
        }
        Ok(Err(kube::Error::Api(response))) if response.code == 404 => ListOnce::Missing,
        Ok(Err(_)) => ListOnce::Stop,
    }
}

fn read_object(kind: MeshKind, value: &Value) -> Option<MeshObject> {
    let meta = value.get("metadata")?;
    let name = meta.get("name").and_then(Value::as_str)?.to_string();
    if name.is_empty() {
        return None;
    }
    let namespace = meta
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let spec = value.get("spec").unwrap_or(&Value::Null);
    let (hosts, destinations, gateways) = spec_fields(kind, spec);
    Some(MeshObject {
        kind,
        namespace,
        name,
        hosts,
        destinations,
        gateways,
    })
}

fn spec_fields(kind: MeshKind, spec: &Value) -> (Vec<String>, Vec<String>, Vec<String>) {
    match kind {
        MeshKind::VirtualService => {
            let hosts = string_list(spec.get("hosts"));
            let gateways = string_list(spec.get("gateways"));
            let mut destinations = Vec::new();
            for section in ["http", "tcp", "tls"] {
                collect_destinations(spec.get(section), &mut destinations);
            }
            (hosts, destinations, gateways)
        }
        MeshKind::DestinationRule => {
            let host = spec.get("host").and_then(Value::as_str).unwrap_or("");
            let hosts = if host.is_empty() {
                Vec::new()
            } else {
                vec![host.to_string()]
            };
            (hosts, Vec::new(), Vec::new())
        }
        MeshKind::Gateway => {
            let mut hosts = Vec::new();
            if let Some(servers) = spec.get("servers").and_then(Value::as_array) {
                for server in servers {
                    hosts.extend(string_list(server.get("hosts")));
                }
            }
            (hosts, Vec::new(), Vec::new())
        }
        MeshKind::Sidecar => {
            let mut hosts = Vec::new();
            if let Some(egress) = spec.get("egress").and_then(Value::as_array) {
                for rule in egress {
                    hosts.extend(string_list(rule.get("hosts")));
                }
            }
            (hosts, Vec::new(), Vec::new())
        }
        MeshKind::ServiceProfile => (Vec::new(), Vec::new(), Vec::new()),
    }
}

fn collect_destinations(section: Option<&Value>, out: &mut Vec<String>) {
    let Some(routes) = section.and_then(Value::as_array) else {
        return;
    };
    for route in routes {
        let Some(destinations) = route.get("route").and_then(Value::as_array) else {
            continue;
        };
        for destination in destinations {
            let Some(host) = destination
                .pointer("/destination/host")
                .and_then(Value::as_str)
            else {
                continue;
            };
            if !host.is_empty() && !out.iter().any(|have| have == host) {
                out.push(host.to_string());
            }
        }
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a kind list from fixture JSON (or an already-fetched body).
pub fn parse_list(kind: MeshKind, bytes: &[u8]) -> Result<Vec<MeshObject>, MeshError> {
    if bytes.len() > MAX_MESH_BYTES {
        return Err(MeshError::TooLarge { bytes: bytes.len() });
    }
    let list: WireList =
        serde_json::from_slice(bytes).map_err(|error| MeshError::NotJson(error.to_string()))?;
    Ok(list
        .items
        .iter()
        .filter_map(|value| read_object(kind, value))
        .collect())
}

/// Edges a mesh CR or NetworkPolicy declares. Not observed traffic.
pub fn declared_from(objects: &[MeshObject]) -> Vec<DeclaredReach> {
    let mut out = Vec::new();
    for object in objects {
        let because = PolicyReason {
            kind: object.kind.as_str().to_string(),
            group: object.kind.group().to_string(),
            namespace: object.namespace.clone(),
            name: object.name.clone(),
        };
        match object.kind {
            MeshKind::VirtualService => {
                let from = object
                    .gateways
                    .first()
                    .cloned()
                    .unwrap_or_else(|| format!("{}/{}", object.namespace, object.name));
                let targets = if object.destinations.is_empty() {
                    &object.hosts
                } else {
                    &object.destinations
                };
                for to in targets {
                    out.push(DeclaredReach {
                        from: from.clone(),
                        to: to.clone(),
                        because: because.clone(),
                    });
                }
            }
            MeshKind::DestinationRule | MeshKind::Gateway | MeshKind::Sidecar => {
                let from = format!("{}/{}", object.namespace, object.name);
                for to in &object.hosts {
                    out.push(DeclaredReach {
                        from: from.clone(),
                        to: to.clone(),
                        because: because.clone(),
                    });
                }
            }
            MeshKind::ServiceProfile => {
                out.push(DeclaredReach {
                    from: "*".to_string(),
                    to: object.name.clone(),
                    because,
                });
            }
        }
    }
    out
}

/// A NetworkPolicy edge in the same contract as a mesh CR. Parsing NP objects
/// is [`crate::netpol`]; this only names the reason so the types stay one.
pub fn declared_from_policy(
    from: impl Into<String>,
    to: impl Into<String>,
    namespace: impl Into<String>,
    name: impl Into<String>,
) -> DeclaredReach {
    DeclaredReach {
        from: from.into(),
        to: to.into(),
        because: PolicyReason {
            kind: "NetworkPolicy".to_string(),
            group: "networking.k8s.io".to_string(),
            namespace: namespace.into(),
            name: name.into(),
        },
    }
}

/// Observed edges from Prometheus series labels already fetched. No Hubble scrape.
pub fn observed_from_series(series: &[SeriesLabels]) -> Vec<ObservedReach> {
    let mut out = Vec::new();
    for item in series {
        let Some(exporter) = exporter_of(&item.name) else {
            continue;
        };
        let Some(from) = endpoint(
            &item.labels,
            &[
                "source",
                "source_workload",
                "source_canonical_service",
                "source_app",
                "src",
                "client",
            ],
        ) else {
            continue;
        };
        let Some(to) = endpoint(
            &item.labels,
            &[
                "destination",
                "destination_workload",
                "destination_service",
                "destination_canonical_service",
                "destination_app",
                "dst",
                "authority",
            ],
        ) else {
            continue;
        };
        out.push(ObservedReach {
            from,
            to,
            because: TelemetryReason {
                metric: item.name.clone(),
                exporter,
            },
        });
    }
    out
}

fn exporter_of(name: &str) -> Option<TelemetryExporter> {
    if name.starts_with("hubble_") {
        Some(TelemetryExporter::Hubble)
    } else if name.starts_with("istio_requests") || name.starts_with("istio_tcp_") {
        Some(TelemetryExporter::Istio)
    } else if name == "route_response_total"
        || name == "response_total"
        || name.starts_with("linkerd_")
    {
        Some(TelemetryExporter::Linkerd)
    } else {
        None
    }
}

fn endpoint(labels: &[(String, String)], keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = labels
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Listeners and clusters from an Envoy admin `config_dump` body.
pub fn parse_config_dump(bytes: &[u8]) -> Result<EnvoySummary, MeshError> {
    if bytes.len() > MAX_MESH_BYTES {
        return Err(MeshError::TooLarge { bytes: bytes.len() });
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| MeshError::NotJson(error.to_string()))?;
    let Some(configs) = value.get("configs").and_then(Value::as_array) else {
        return Err(MeshError::NotADump);
    };
    let mut listeners = 0usize;
    let mut clusters = 0usize;
    for config in configs {
        let type_url = config.get("@type").and_then(Value::as_str).unwrap_or("");
        if type_url.contains("ListenersConfigDump") {
            listeners += array_len(config.get("static_listeners"));
            listeners += array_len(config.get("dynamic_listeners"));
        } else if type_url.contains("ClustersConfigDump") {
            clusters += array_len(config.get("static_clusters"));
            clusters += array_len(config.get("dynamic_active_clusters"));
            clusters += array_len(config.get("dynamic_warming_clusters"));
        }
    }
    Ok(EnvoySummary {
        listeners,
        clusters,
    })
}

fn array_len(value: Option<&Value>) -> usize {
    value.and_then(Value::as_array).map(Vec::len).unwrap_or(0)
}

/// GET a path on a forwarded sidecar admin. Loopback HTTP only; no forward is opened.
pub async fn envoy_get(base: &str, rest: &str) -> Fetched<Vec<u8>> {
    loopback_get(base, rest).await
}

/// `config_dump` at a forwarded admin base, summarised. Stats JSON is the same GET.
pub async fn envoy_admin(base: &str) -> Fetched<EnvoySummary> {
    match loopback_get(base, "config_dump").await {
        Fetched::Ok(bytes) => match parse_config_dump(&bytes) {
            Ok(summary) => Fetched::Ok(summary),
            Err(error) => Fetched::Failed {
                what: "envoy",
                why: error.to_string(),
            },
        },
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

fn parse_loopback(base: &str) -> Result<(String, u16), String> {
    let trimmed = base.trim().trim_end_matches('/');
    let uri: http::Uri = trimmed.parse().map_err(|_| format!("not a URL: {base}"))?;
    if uri.scheme_str() != Some("http") {
        return Err(
            "envoy admin is http://127.0.0.1 after a port-forward; https is not fetched".into(),
        );
    }
    if uri.host() != Some("127.0.0.1") {
        return Err(
            "envoy admin is only fetched at 127.0.0.1; this module does not open a forward".into(),
        );
    }
    let Some(port) = uri.port_u16() else {
        return Err("envoy admin URL must include a port, like http://127.0.0.1:15000".into());
    };
    Ok(("127.0.0.1".to_string(), port))
}

async fn loopback_get(base: &str, rest: &str) -> Fetched<Vec<u8>> {
    let (host, port) = match parse_loopback(base) {
        Ok(pair) => pair,
        Err(why) => {
            return Fetched::Failed { what: "envoy", why };
        }
    };
    let rest = rest.trim_start_matches('/');
    let path = if rest.is_empty() {
        "/".to_string()
    } else {
        format!("/{rest}")
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );

    let connect = tokio::time::timeout(
        CONSULT_DEADLINE,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await;
    let mut stream = match connect {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return Fetched::Failed {
                what: "envoy",
                why: error.to_string(),
            };
        }
        Err(_) => {
            return Fetched::Failed {
                what: "envoy",
                why: format!("{host}:{port} did not accept a connection within 4 seconds"),
            };
        }
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if let Err(error) = stream.write_all(request.as_bytes()).await {
        return Fetched::Failed {
            what: "envoy",
            why: error.to_string(),
        };
    }
    let mut buf = Vec::new();
    loop {
        if buf.len() > MAX_MESH_BYTES + 4096 {
            return Fetched::Failed {
                what: "envoy",
                why: format!("the answer exceeded {MAX_MESH_BYTES} bytes; it is hidden"),
            };
        }
        let mut chunk = [0u8; 8192];
        match tokio::time::timeout(CONSULT_DEADLINE, stream.read(&mut chunk)).await {
            Err(_) => {
                return Fetched::Failed {
                    what: "envoy",
                    why: "envoy admin stopped sending within 4 seconds".to_string(),
                };
            }
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(error)) => {
                return Fetched::Failed {
                    what: "envoy",
                    why: error.to_string(),
                };
            }
        }
    }
    split_http_body(&buf)
}

fn split_http_body(raw: &[u8]) -> Fetched<Vec<u8>> {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Fetched::Failed {
            what: "envoy",
            why: "envoy admin answered with no HTTP header terminator".to_string(),
        };
    };
    let headers = &raw[..split];
    let body = &raw[split + 4..];
    let status = headers
        .split(|&b| b == b'\n')
        .next()
        .and_then(|line| {
            let line = std::str::from_utf8(line).ok()?.trim();
            line.split_whitespace().nth(1)?.parse::<u16>().ok()
        })
        .unwrap_or(0);
    if matches!(status, 401 | 403) {
        return Fetched::Denied { what: "envoy" };
    }
    if !(200..300).contains(&status) {
        return Fetched::Failed {
            what: "envoy",
            why: format!("envoy admin answered {status}"),
        };
    }
    if body.len() > MAX_MESH_BYTES {
        return Fetched::Failed {
            what: "envoy",
            why: format!("the answer exceeded {MAX_MESH_BYTES} bytes; it is hidden"),
        };
    }
    Fetched::Ok(body.to_vec())
}

#[cfg(test)]
#[path = "mesh_test.rs"]
mod tests;
