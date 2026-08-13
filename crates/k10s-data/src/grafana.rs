//! Grafana dashboard JSON to a query list, fetched from a bound Grafana or
//! from provisioning ConfigMaps. No gpui.
//!
//! A dashboard is a title, a uid, and the panels we can run. Each panel keeps
//! the title Grafana gave it and the PromQL/LogQL/TraceQL its targets already
//! carry. Transformations, variables, and plugin panel types are not executed:
//! those panels are [`PanelKind::Unsupported`] so a UI can deep-link them
//! rather than fake Grafana's engine. Nested row panels are walked; the row
//! itself is not a query.
//!
//! Fetch is Grafana's search and dashboard API through [`crate::reach::tool_get`],
//! or labelled ConfigMaps when Grafana itself is not bound. Secrets are not
//! read: the sidecar can watch those too, and this path refuses that. Parse is
//! shared. The bytes are attacker-shaped. Caps refuse a dashboard rather than
//! truncating one: a missing panel is visible, a silent drop is not.

use k8s_openapi::api::core::v1::ConfigMap;
use kube::Client;
use kube::api::{Api, ListParams};
use serde::Deserialize;
use serde_json::Value;

use crate::reach::{Bound, tool_get};
use crate::read::{Fetched, classify};

pub const MAX_DASHBOARD_BYTES: usize = 8 << 20;
pub const MAX_PANELS: usize = 512;
pub const MAX_QUERIES_PER_PANEL: usize = 16;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_EXPR_CHARS: usize = 8_192;
/// Provisioned ConfigMaps we will parse. Reaching it is stated, not hidden.
pub const MAX_PROVISIONED: usize = 256;

const PAGE_LIMIT: u32 = 200;
const MAX_CONFIGMAPS: usize = 2_000;
/// kube-prometheus-stack's sidecar default, and the boolean spelling some
/// charts use instead.
const DASHBOARD_SELECTOR: &str = "grafana_dashboard in (1,true)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryDialect {
    PromQL,
    LogQL,
    TraceQL,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKind {
    Timeseries,
    Stat,
    Gauge,
    Table,
    Logs,
    Heatmap,
    Bar,
    /// Grafana's engine, a plugin, or a transformation we will not run.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelQuery {
    pub ref_id: String,
    pub expr: String,
    pub dialect: QueryDialect,
    pub datasource: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panel {
    pub id: i64,
    pub title: String,
    pub kind: PanelKind,
    pub queries: Vec<PanelQuery>,
    /// Grafana ran a transformation on this panel: we keep the queries so a
    /// person can still see the raw series, and the UI must say the picture
    /// is not Grafana's.
    pub transformed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dashboard {
    pub uid: String,
    pub title: String,
    pub panels: Vec<Panel>,
    /// Panels past [`MAX_PANELS`] were not walked.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardError {
    TooLarge { bytes: usize },
    NotJson(String),
    NotADashboard,
}

impl std::fmt::Display for DashboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DashboardError::TooLarge { bytes } => write!(
                f,
                "dashboard JSON is {bytes} bytes; the cap is {MAX_DASHBOARD_BYTES}"
            ),
            DashboardError::NotJson(why) => write!(f, "dashboard JSON did not parse: {why}"),
            DashboardError::NotADashboard => {
                write!(f, "JSON is not a Grafana dashboard (no title, no panels)")
            }
        }
    }
}

/// Parse dashboard JSON from Grafana's API or a provisioning ConfigMap.
///
/// Accepts the API envelope `{ "dashboard": { ... } }` and a bare dashboard
/// object. A ConfigMap that stores the JSON as a string is the fetch side's
/// job: this function wants the document, not the ConfigMap.
pub fn parse_dashboard(bytes: &[u8]) -> Result<Dashboard, DashboardError> {
    if bytes.len() > MAX_DASHBOARD_BYTES {
        return Err(DashboardError::TooLarge { bytes: bytes.len() });
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| DashboardError::NotJson(error.to_string()))?;
    let dashboard = match value.get("dashboard") {
        Some(inner) => inner,
        None => &value,
    };
    let title = clip(
        dashboard.get("title").and_then(Value::as_str).unwrap_or(""),
        MAX_TITLE_CHARS,
    );
    let uid = dashboard
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(panels) = dashboard.get("panels").and_then(Value::as_array) else {
        if title.is_empty() && uid.is_empty() {
            return Err(DashboardError::NotADashboard);
        }
        return Ok(Dashboard {
            uid,
            title,
            panels: Vec::new(),
            truncated: false,
        });
    };

    let mut out = Vec::new();
    let mut truncated = false;
    walk_panels(panels, &mut out, &mut truncated);
    Ok(Dashboard {
        uid,
        title,
        panels: out,
        truncated,
    })
}

fn walk_panels(panels: &[Value], out: &mut Vec<Panel>, truncated: &mut bool) {
    for panel in panels {
        if out.len() >= MAX_PANELS {
            *truncated = true;
            return;
        }
        let kind_name = panel.get("type").and_then(Value::as_str).unwrap_or("");
        if kind_name == "row" {
            if let Some(nested) = panel.get("panels").and_then(Value::as_array) {
                walk_panels(nested, out, truncated);
            }
            continue;
        }
        out.push(read_panel(panel, out.len() as i64 + 1));
    }
}

fn read_panel(panel: &Value, fallback_id: i64) -> Panel {
    let id = panel
        .get("id")
        .and_then(Value::as_i64)
        .unwrap_or(fallback_id);
    let title = clip(
        panel.get("title").and_then(Value::as_str).unwrap_or(""),
        MAX_TITLE_CHARS,
    );
    let kind = panel_kind(panel.get("type").and_then(Value::as_str).unwrap_or(""));
    let transformed = panel
        .get("transformations")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    let datasource = datasource_name(panel.get("datasource"));
    let mut queries = Vec::new();
    if let Some(targets) = panel.get("targets").and_then(Value::as_array) {
        for target in targets.iter().take(MAX_QUERIES_PER_PANEL) {
            if let Some(query) = read_query(target, datasource.as_deref()) {
                queries.push(query);
            }
        }
    }
    Panel {
        id,
        title,
        kind,
        queries,
        transformed,
    }
}

fn read_query(target: &Value, panel_datasource: Option<&str>) -> Option<PanelQuery> {
    let expr = target
        .get("expr")
        .and_then(Value::as_str)
        .or_else(|| target.get("query").and_then(Value::as_str))
        .unwrap_or("")
        .trim();
    if expr.is_empty() {
        return None;
    }
    let datasource =
        datasource_name(target.get("datasource")).or_else(|| panel_datasource.map(str::to_string));
    let dialect = dialect_of(
        datasource.as_deref(),
        target.get("queryType").and_then(Value::as_str),
    );
    Some(PanelQuery {
        ref_id: target
            .get("refId")
            .and_then(Value::as_str)
            .unwrap_or("A")
            .to_string(),
        expr: clip(expr, MAX_EXPR_CHARS),
        dialect,
        datasource,
    })
}

fn panel_kind(type_name: &str) -> PanelKind {
    match type_name {
        "timeseries" | "graph" | "xychart" => PanelKind::Timeseries,
        "stat" | "singlestat" => PanelKind::Stat,
        "gauge" | "bargauge" => PanelKind::Gauge,
        "table" | "table-old" => PanelKind::Table,
        "logs" => PanelKind::Logs,
        "heatmap" | "histogram" => PanelKind::Heatmap,
        "barchart" | "piechart" => PanelKind::Bar,
        _ => PanelKind::Unsupported,
    }
}

fn dialect_of(datasource: Option<&str>, query_type: Option<&str>) -> QueryDialect {
    if query_type.is_some_and(|t| t.eq_ignore_ascii_case("traceql")) {
        return QueryDialect::TraceQL;
    }
    let Some(name) = datasource else {
        return QueryDialect::PromQL;
    };
    let lower = name.to_ascii_lowercase();
    if lower.contains("loki") || lower.contains("logql") {
        QueryDialect::LogQL
    } else if lower.contains("tempo") || lower.contains("jaeger") || lower.contains("trace") {
        QueryDialect::TraceQL
    } else if lower.contains("prometheus")
        || lower.contains("mimir")
        || lower.contains("thanos")
        || lower.contains("prom")
    {
        QueryDialect::PromQL
    } else {
        QueryDialect::Unknown
    }
}

fn datasource_name(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        let text = text.trim();
        return (!text.is_empty() && text != "-- Grafana --").then(|| text.to_string());
    }
    let uid = value.get("uid").and_then(Value::as_str).unwrap_or("");
    let type_name = value.get("type").and_then(Value::as_str).unwrap_or("");
    if !uid.is_empty() && uid != "grafana" {
        return Some(uid.to_string());
    }
    if !type_name.is_empty() {
        return Some(type_name.to_string());
    }
    None
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

/// A Grafana search hit: enough to fetch the dashboard by uid.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SearchHit {
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, alias = "folderTitle")]
    pub folder_title: String,
    #[serde(default, rename = "type")]
    pub kind: String,
}

pub fn parse_search(bytes: &[u8]) -> Result<Vec<SearchHit>, DashboardError> {
    if bytes.len() > MAX_DASHBOARD_BYTES {
        return Err(DashboardError::TooLarge { bytes: bytes.len() });
    }
    serde_json::from_slice(bytes).map_err(|error| DashboardError::NotJson(error.to_string()))
}

/// Keep hits whose folder title is in the allowlist. An empty allowlist keeps
/// everything: a Grafana with four hundred dashboards is why the allowlist
/// exists, not a reason to show none of them by default.
pub fn filter_folders(hits: Vec<SearchHit>, allowlist: &[String]) -> Vec<SearchHit> {
    if allowlist.is_empty() {
        return hits
            .into_iter()
            .filter(|hit| hit.kind != "dash-folder")
            .collect();
    }
    hits.into_iter()
        .filter(|hit| {
            hit.kind != "dash-folder"
                && allowlist.iter().any(|want| {
                    hit.folder_title.eq_ignore_ascii_case(want)
                        || hit.title.eq_ignore_ascii_case(want)
                })
        })
        .collect()
}

/// Search Grafana for dashboards, then keep the allowlisted folders.
pub async fn fetch_search(
    client: &Client,
    bound: &Bound,
    folder_allowlist: &[String],
) -> Fetched<Vec<SearchHit>> {
    match into_parsed(
        tool_get(client, bound, "api/search?type=dash-db").await,
        parse_search,
    ) {
        Fetched::Ok(hits) => Fetched::Ok(filter_folders(hits, folder_allowlist)),
        other => other,
    }
}

/// One dashboard by uid, through the same bind as search.
pub async fn fetch_dashboard(client: &Client, bound: &Bound, uid: &str) -> Fetched<Dashboard> {
    into_parsed(
        tool_get(client, bound, &format!("api/dashboards/uid/{uid}")).await,
        parse_dashboard,
    )
}

/// Dashboards Grafana's sidecar already stored as ConfigMaps.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Provisioned {
    pub dashboards: Vec<Dashboard>,
    /// ConfigMaps or keys past [`MAX_PROVISIONED`] were not walked.
    pub truncated: bool,
}

/// List ConfigMaps labelled `grafana_dashboard=1` (or `true`) and parse each
/// `data` value as dashboard JSON. No Grafana API. `binaryData` is ignored.
/// Secrets are never listed: that is the sidecar's other watch, and not ours.
pub async fn fetch_provisioned_from_configmaps(client: &Client) -> Fetched<Provisioned> {
    let api: Api<ConfigMap> = Api::all(client.clone());
    let mut dashboards = Vec::new();
    let mut truncated = false;
    let mut scanned = 0usize;
    let mut token: Option<String> = None;
    loop {
        let mut params = ListParams::default()
            .limit(PAGE_LIMIT)
            .labels(DASHBOARD_SELECTOR);
        if let Some(token) = token.as_deref() {
            params = params.continue_token(token);
        }
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(error) => return classify("grafana", &error),
        };
        for cm in page.items {
            scanned += 1;
            if scanned > MAX_CONFIGMAPS || dashboards.len() >= MAX_PROVISIONED {
                truncated = true;
                break;
            }
            collect_from_configmap(&cm, &mut dashboards, &mut truncated);
            if truncated {
                break;
            }
        }
        token = page.metadata.continue_.filter(|s| !s.is_empty());
        if token.is_none() || truncated || scanned > MAX_CONFIGMAPS {
            break;
        }
    }
    Fetched::Ok(Provisioned {
        dashboards,
        truncated,
    })
}

fn collect_from_configmap(cm: &ConfigMap, dashboards: &mut Vec<Dashboard>, truncated: &mut bool) {
    let Some(data) = &cm.data else {
        return;
    };
    for value in data.values() {
        if dashboards.len() >= MAX_PROVISIONED {
            *truncated = true;
            return;
        }
        if let Ok(dashboard) = parse_dashboard(value.as_bytes()) {
            dashboards.push(dashboard);
        }
    }
}

fn into_parsed<T>(
    fetched: Fetched<Vec<u8>>,
    parse: impl FnOnce(&[u8]) -> Result<T, DashboardError>,
) -> Fetched<T> {
    match fetched {
        Fetched::Ok(bytes) => match parse(&bytes) {
            Ok(value) => Fetched::Ok(value),
            Err(error) => Fetched::Failed {
                what: "grafana",
                why: error.to_string(),
            },
        },
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

#[cfg(test)]
#[path = "grafana_test.rs"]
mod tests;
