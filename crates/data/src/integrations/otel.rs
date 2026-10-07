//! OpenTelemetry Collector metadata from the CRs the operator already
//! publishes, plus a health GET on a bound collector.
//!
//! `OpenTelemetryCollector` lives on `opentelemetry.io`. A cluster that does
//! not serve the group answers 404 and the kind is invisible, not broken; a
//! 403 is Denied. Nothing is installed to find them.
//!
//! Collector config is never pulled into the inventory. `spec.config` is
//! YAML or a structured object and routinely holds exporter tokens. Revealing
//! it would need [`crate::reach::Scratch`] and an explicit click; that path
//! is out of scope here. The inventory types have nowhere to put those bytes,
//! and the request requires PartialObjectMetadataList without a full-object fallback.
//! Group discovery is capped at 64 KiB. Metadata pages share an eight-MiB
//! budget and sixteen LIST exchanges; each exchange has a four-second deadline.
//! Legal names retain their full identity. Only table display cells are clipped;
//! oversized names or namespaces count as unreadable objects.
//!
//! Health is a GET of a well-known extension path on a [`crate::reach::Bound`]
//! `OtelCollector`. zpages (55679) and the health_check extension (13133)
//! speak HTTP, and the reach fingerprint prefers those ports so a discovered
//! Service lands on one health can use. A bind that still landed on the
//! Prometheus metrics port or the OTLP receiver is a labelled Failed why,
//! not a fake healthy.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::reach::{Bound, ToolAuth, ToolKind, Transport, tool_get};
use crate::read::Fetched;
use crate::served::ListErr;

pub const MAX_COLLECTORS: usize = 2_000;
pub const MAX_FIELD_CHARS: usize = 200;
const MAX_NAME_BYTES: usize = 253;
const MAX_NAMESPACE_BYTES: usize = 63;

pub const HEALTH_PORT: u16 = 13133;
pub const ZPAGES_PORT: u16 = 55679;
pub const METRICS_PORT: u16 = 8888;
pub const OTLP_HTTP_PORT: u16 = 4318;

const PAGE_LIMIT: u32 = 200;
const MAX_GROUP_BYTES: usize = 64 << 10;
const MAX_LIST_REQUESTS: usize = 16;
const GROUP: &str = "opentelemetry.io";
const PLURAL: &str = "opentelemetrycollectors";
const FALLBACK_VERSION: &str = "v1beta1";
const WHAT: &str = "otel collectors";
const HEALTH_WHAT: &str = "otel-collector";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collector {
    pub name: String,
    pub namespace: String,
    /// Metadata only; inventory does not read configuration or status.
    pub mode: String,
    pub replicas: Option<i32>,
    /// Not fetched; metadata does not include readiness.
    pub ready: String,
    pub image: String,
}

/// What the group list answered.
///
/// A 404 on the group is [`KindSet::NotServed`]: invisible, not broken. A 403
/// is [`KindSet::Denied`]. Those are different states on purpose; collapsing
/// them would tell someone the operator is absent when the account was refused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KindSet {
    Served {
        items: Vec<Collector>,
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

    pub fn items(&self) -> &[Collector] {
        match self {
            KindSet::Served { items, .. } => items,
            KindSet::NotServed | KindSet::Denied => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub collectors: KindSet,
    pub services: Vec<CollectorService>,
    pub services_truncated: bool,
}

impl Inventory {
    pub fn served(&self) -> bool {
        self.collectors.served() || !self.services.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorService {
    pub name: String,
    pub namespace: String,
    pub port: u16,
    pub health: String,
    pub source: String,
}

/// The extension answered. The path is kept so a UI can say which one spoke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub path: String,
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
    kind: String,
    #[serde(default)]
    metadata: WireListMeta,
    #[serde(deserialize_with = "list_items")]
    items: Vec<WireObject>,
}

fn list_items<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<WireObject>, D::Error> {
    Ok(Option::<Vec<WireObject>>::deserialize(deserializer)?.unwrap_or_default())
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
}

#[derive(Deserialize, Default)]
struct WireMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
}

pub(crate) enum GroupAnswer {
    Served(Vec<String>),
    NotServed,
    Denied,
    Failed(String),
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_FIELD_CHARS) {
        Some((at, _)) => {
            let mut out = text[..at].to_string();
            out.push('\u{2026}');
            out
        }
        None => text.to_string(),
    }
}

fn from_wire(wire: WireObject) -> Option<Collector> {
    if wire.metadata.name.is_empty()
        || wire.metadata.name.len() > MAX_NAME_BYTES
        || wire.metadata.namespace.len() > MAX_NAMESPACE_BYTES
    {
        return None;
    }
    Some(Collector {
        name: wire.metadata.name,
        namespace: wire.metadata.namespace,
        mode: "Metadata only".into(),
        replicas: None,
        ready: "Not fetched".into(),
        image: String::new(),
    })
}

/// Reduce metadata; configuration and status are never required for inventory.
pub fn parse_collector(value: &Value) -> Option<Collector> {
    let wire: WireObject = serde_json::from_value(value.clone()).ok()?;
    from_wire(wire)
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
    if !out.iter().any(|have| have == FALLBACK_VERSION) {
        out.push(FALLBACK_VERSION.to_string());
    }
    out
}

fn collection_url(version: &str, namespace: Option<&str>) -> String {
    let mut path = format!("/apis/{GROUP}/{version}");
    if let Some(namespace) = namespace {
        path.push_str("/namespaces/");
        path.push_str(namespace);
    }
    path.push('/');
    path.push_str(PLURAL);
    path
}

async fn probe_group(client: &Client) -> GroupAnswer {
    let request = match http::Request::get(format!("/apis/{GROUP}"))
        .header(http::header::ACCEPT, "application/json")
        .body(Vec::new())
    {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    let (status, outcome) =
        crate::reach::bounded_request_with_status(client, WHAT, request, MAX_GROUP_BYTES).await;
    match outcome {
        Fetched::Ok(bytes) => {
            let group: WireGroup = match serde_json::from_slice(&bytes) {
                Ok(group) => group,
                Err(error) => {
                    return GroupAnswer::Failed(format!(
                        "{GROUP} discovery returned an unsupported response: {error}"
                    ));
                }
            };
            let versions = order_versions(&group.preferred.version, {
                group
                    .versions
                    .into_iter()
                    .map(|item| item.version)
                    .collect()
            });
            if versions.iter().any(|version| {
                version.bytes().any(|byte| {
                    !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && byte != b'-'
                })
            }) {
                return GroupAnswer::Failed(format!(
                    "{GROUP} discovery returned an unsupported API version"
                ));
            }
            GroupAnswer::Served(versions)
        }
        Fetched::Denied { .. } => GroupAnswer::Denied,
        Fetched::Failed { .. } if status == Some(http::StatusCode::NOT_FOUND) => {
            GroupAnswer::NotServed
        }
        Fetched::Failed { why, .. } => GroupAnswer::Failed(why),
    }
}

#[derive(Default)]
struct ListBudget {
    requests: usize,
    bytes: usize,
}

async fn list_at_version(
    client: &Client,
    version: &str,
    namespace: Option<&str>,
    budget: &mut ListBudget,
) -> Result<KindSet, ListErr> {
    let path = collection_url(version, namespace);
    let mut items = Vec::new();
    let mut unreadable = 0usize;
    let mut token: Option<String> = None;
    let mut truncated = false;
    let mut pages = 0usize;
    loop {
        if budget.requests == MAX_LIST_REQUESTS {
            return Err(ListErr::Failed(format!(
                "{WHAT} exceed {MAX_LIST_REQUESTS} LIST exchanges; no partial inventory is used"
            )));
        }
        if budget.bytes == crate::reach::MAX_BODY_BYTES {
            return Err(ListErr::Failed(format!(
                "{WHAT} exceed 8 MiB; no partial inventory is used"
            )));
        }
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let mut request = match Request::new(path.clone()).list(&params) {
            Ok(request) => request,
            Err(error) => return Err(ListErr::Failed(error.to_string())),
        };
        request.headers_mut().insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static(
                "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1",
            ),
        );
        budget.requests += 1;
        let (status, outcome) = crate::reach::bounded_request_with_status(
            client,
            WHAT,
            request,
            crate::reach::MAX_BODY_BYTES - budget.bytes,
        )
        .await;
        let bytes = match outcome {
            Fetched::Ok(bytes) => bytes,
            Fetched::Denied { .. } => return Err(ListErr::Denied),
            Fetched::Failed { .. } if pages == 0 && status == Some(http::StatusCode::NOT_FOUND) => {
                return Err(ListErr::NotFound);
            }
            Fetched::Failed { why, .. } => return Err(ListErr::Failed(why)),
        };
        budget.bytes += bytes.len();
        pages += 1;
        let page: WireList = serde_json::from_slice(&bytes).map_err(|error| {
            ListErr::Failed(format!("{WHAT} returned an unsupported response: {error}"))
        })?;
        if page.kind != "PartialObjectMetadataList" {
            return Err(ListErr::Failed(format!(
                "{WHAT} did not return PartialObjectMetadataList; no full-object inventory is used"
            )));
        }
        for wire in page.items {
            if items.len() + unreadable == MAX_COLLECTORS {
                truncated = true;
                break;
            }
            match from_wire(wire) {
                Some(collector) => items.push(collector),
                None => unreadable += 1,
            }
        }
        if page.metadata.cont.is_empty() {
            break;
        }
        if items.len() + unreadable == MAX_COLLECTORS || truncated {
            truncated = true;
            break;
        }
        if token.as_deref() == Some(page.metadata.cont.as_str()) {
            return Err(ListErr::Failed(format!(
                "{WHAT} repeated a continuation token; no partial inventory is used"
            )));
        }
        token = Some(page.metadata.cont);
    }
    Ok(KindSet::Served {
        items,
        truncated,
        unreadable,
    })
}

/// List `OpenTelemetryCollector` CRs. A missing group is invisible; a
/// forbidden one is Denied.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let versions = match probe_group(client).await {
        GroupAnswer::NotServed => {
            return Fetched::Ok(Inventory {
                collectors: KindSet::NotServed,
                ..Inventory::default()
            });
        }
        GroupAnswer::Denied => {
            return Fetched::Ok(Inventory {
                collectors: KindSet::Denied,
                ..Inventory::default()
            });
        }
        GroupAnswer::Failed(why) => {
            return Fetched::Failed { what: WHAT, why };
        }
        GroupAnswer::Served(versions) => versions,
    };
    let mut budget = ListBudget::default();
    for version in versions {
        match list_at_version(client, &version, namespace, &mut budget).await {
            Ok(set) => {
                return Fetched::Ok(Inventory {
                    collectors: set,
                    ..Inventory::default()
                });
            }
            Err(ListErr::NotFound) => continue,
            Err(ListErr::Denied) => {
                return Fetched::Ok(Inventory {
                    collectors: KindSet::Denied,
                    ..Inventory::default()
                });
            }
            Err(ListErr::Failed(why)) => {
                return Fetched::Failed { what: WHAT, why };
            }
        }
    }
    Fetched::Ok(Inventory {
        collectors: KindSet::NotServed,
        ..Inventory::default()
    })
}

/// Metadata plus individually identified extension endpoints. Process health
/// does not claim that receivers, exporters or end-to-end ingestion work.
pub async fn fetch_inventory(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let mut inventory = match fetch(client, namespace).await {
        Fetched::Ok(inventory) => inventory,
        Fetched::Denied { what } => return Fetched::Denied { what },
        Fetched::Failed { what, why } => return Fetched::Failed { what, why },
    };
    let services = match crate::reach::find_services(client, ToolKind::OtelCollector).await {
        Fetched::Ok(services) => services,
        Fetched::Denied { what } => return Fetched::Denied { what },
        Fetched::Failed { what, why } => return Fetched::Failed { what, why },
    };
    for (index, found) in services
        .into_iter()
        .filter(|found| namespace.is_none_or(|namespace| found.namespace == namespace))
        .enumerate()
    {
        if index == 16 {
            inventory.services_truncated = true;
            break;
        }
        let bound = Bound {
            kind: ToolKind::OtelCollector,
            transport: Transport::Proxy {
                namespace: found.namespace.clone(),
                service: found.name.clone(),
                port: found.port,
            },
            found: Some(found.clone()),
            auth: ToolAuth::Anonymous,
        };
        let outcome = match health(client, &bound).await {
            Fetched::Ok(health) => format!("Process extension answered {}", health.path),
            Fetched::Denied { what } => format!("{what}: access denied for this account"),
            Fetched::Failed { why, .. } => why,
        };
        inventory.services.push(CollectorService {
            name: found.name,
            namespace: found.namespace,
            port: found.port,
            health: outcome,
            source: bound.source(),
        });
    }
    Fetched::Ok(inventory)
}

/// Native list rows. `None` when the group answered 404, so a UI stays
/// invisible rather than opening an empty pane. A denied kind is a labelled
/// row, not absence, and objects that failed to decode are a labelled row,
/// not a silently shorter list.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served() {
        return None;
    }
    let columns = [
        "Name",
        "Namespace",
        "Kind",
        "Replicas / Port",
        "State",
        "Source",
    ]
    .iter()
    .map(|name| TableColumn {
        name: name.to_string(),
        wide: false,
    })
    .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    match &inventory.collectors {
        KindSet::NotServed => {}
        KindSet::Denied => {
            rows.push(TableRow {
                cells: vec![
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    "access denied for this account".to_string(),
                    String::new(),
                ],
                name: "OpenTelemetryCollector".to_string(),
                namespace: None,
                uid: "denied:OpenTelemetryCollector".to_string(),
            });
        }
        KindSet::Served {
            items,
            truncated: cap,
            unreadable,
        } => {
            truncated = *cap;
            for item in items {
                let uid = format!("{}/{}", item.namespace, item.name);
                rows.push(TableRow {
                    cells: vec![
                        clip(&item.name),
                        clip(&item.namespace),
                        item.mode.clone(),
                        item.replicas
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "Unknown".to_string()),
                        ready_label(&item.ready),
                        item.image.clone(),
                    ],
                    name: item.name.clone(),
                    namespace: Some(item.namespace.clone()),
                    uid,
                });
            }
            if *unreadable > 0 {
                rows.push(TableRow {
                    cells: vec![
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        format!(
                            "{} {} could not be decoded and {} not shown",
                            unreadable,
                            if *unreadable == 1 {
                                "collector"
                            } else {
                                "collectors"
                            },
                            if *unreadable == 1 { "is" } else { "are" },
                        ),
                        String::new(),
                    ],
                    name: "OpenTelemetryCollector".to_string(),
                    namespace: None,
                    uid: "unreadable:OpenTelemetryCollector".to_string(),
                });
            }
        }
    }
    rows.extend(inventory.services.iter().map(|service| TableRow {
        name: service.name.clone(),
        namespace: Some(service.namespace.clone()),
        uid: format!("service:{}/{}", service.namespace, service.name),
        cells: vec![
            clip(&service.name),
            clip(&service.namespace),
            "Service".into(),
            service.port.to_string(),
            service.health.clone(),
            service.source.clone(),
        ],
    }));
    Some(TablePage {
        columns,
        rows,
        truncated: truncated || inventory.services_truncated,
        continue_token: None,
    })
}

fn ready_label(status: &str) -> String {
    match status {
        "True" => "Ready".to_string(),
        "False" => "not ready".to_string(),
        "Unknown" => "unknown".to_string(),
        "" => "no Ready condition".to_string(),
        other => other.to_string(),
    }
}

/// GET a well-known health or zpages path on a bound collector.
///
/// Metrics (8888) and OTLP HTTP (4318) are not health signals: the outcome
/// is Failed with a why, and the wire is not touched.
pub async fn health(client: &Client, bound: &Bound) -> Fetched<Health> {
    if bound.kind != ToolKind::OtelCollector {
        return Fetched::Failed {
            what: HEALTH_WHAT,
            why: format!(
                "{} is not an OpenTelemetry Collector; bind OtelCollector",
                bound.kind.as_str()
            ),
        };
    }
    if matches!(bound.auth, ToolAuth::NamedToken(_))
        && matches!(bound.transport, Transport::Proxy { .. })
    {
        return Fetched::Failed {
            what: HEALTH_WHAT,
            why:
                "a named OpenTelemetry Collector token cannot ride the API-server proxy; it would \
                 share the kube client's Authorization header. Bind through a port-forward or a \
                 settings URL"
                    .to_string(),
        };
    }
    let path = match health_path(bound) {
        Ok(path) => path,
        Err(why) => {
            return Fetched::Failed {
                what: HEALTH_WHAT,
                why,
            };
        }
    };
    match tool_get(client, bound, path).await {
        Fetched::Ok(_) => Fetched::Ok(Health {
            path: path.to_string(),
        }),
        Fetched::Denied { .. } => Fetched::Denied { what: HEALTH_WHAT },
        Fetched::Failed { why, .. } => Fetched::Failed {
            what: HEALTH_WHAT,
            why,
        },
    }
}

pub(crate) fn health_path(bound: &Bound) -> Result<&'static str, String> {
    let (port, name) = port_of(bound);
    let name = name.unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    if port == HEALTH_PORT || lower.contains("health") {
        return Ok("");
    }
    if port == ZPAGES_PORT || lower.contains("zpages") {
        return Ok("debug/servicez");
    }
    if port == METRICS_PORT || lower == "metrics" {
        return Err(
            "this bind is the Prometheus metrics port, not the health_check or zpages extension; \
             it is not a health signal"
                .to_string(),
        );
    }
    if port == OTLP_HTTP_PORT || lower.contains("otlp") {
        return Err(
            "this bind is the OTLP receiver, not the health_check or zpages extension; it is not \
             a health signal"
                .to_string(),
        );
    }
    if port == 0 {
        return Err(
            "this bind does not name a health_check (13133) or zpages (55679) port; it is not a \
             health signal"
                .to_string(),
        );
    }
    Err(format!(
        "this bind's port {port} is not the health_check (13133) or zpages (55679) extension; it \
         is not a health signal"
    ))
}

fn port_of(bound: &Bound) -> (u16, Option<String>) {
    if let Some(found) = &bound.found {
        return (found.port, found.port_name.clone());
    }
    match &bound.transport {
        Transport::Proxy { port, .. } | Transport::NeedsForward { port, .. } => (*port, None),
        Transport::Url { base } => {
            let port = http::Uri::try_from(base.as_str())
                .ok()
                .and_then(|uri| uri.port_u16())
                .unwrap_or(0);
            (port, None)
        }
    }
}

#[cfg(test)]
#[path = "otel_test.rs"]
mod tests;
