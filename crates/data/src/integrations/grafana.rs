//! Grafana dashboard JSON to a query list, fetched from a bound Grafana or
//! from provisioning ConfigMaps. No gpui.
//!
//! A dashboard is a title, a uid, and the panels we can run. Each panel keeps
//! the title Grafana gave it and the PromQL/LogQL/TraceQL its targets already
//! carry. The declared label and duration variables are expanded after datasource
//! verification. Transformations and unsupported panel semantics carry a reason
//! so a UI can deep-link them
//! rather than fake Grafana's engine. Nested row panels are walked; the row
//! itself is not a query.
//!
//! Fetch is Grafana's search and dashboard API through [`crate::reach::tool_get`],
//! or labelled ConfigMaps when Grafana itself is not bound. Secrets are not
//! read: the sidecar can watch those too, and this path refuses that. Parse is
//! shared. The bytes are attacker-shaped. Caps refuse a dashboard rather than
//! truncating one: a missing panel is visible, a silent drop is not.
//! Provisioned lists share an eight-MiB body budget and sixteen pages, with
//! four seconds per exchange. A failed later page keeps healthy dashboards
//! with a named error; denial remains an explicit denied result.

use k8s_openapi::api::core::v1::ConfigMap;
use kube::Client;
use kube::api::{ListParams, ObjectList, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::reach::{Bound, tool_get};
use crate::read::Fetched;

mod variables;
pub use variables::{Variables, duration_ms, duration_text, interpolate};

pub const MAX_DASHBOARD_BYTES: usize = 8 << 20;
pub const MAX_PANELS: usize = 512;
pub const MAX_QUERIES_PER_PANEL: usize = 16;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_EXPR_CHARS: usize = 8_192;
/// Provisioned ConfigMaps we will parse. Reaching it is stated, not hidden.
pub const MAX_PROVISIONED: usize = 256;
/// Grafana search can list hundreds of titles. Naming overlay PromQL needs
/// the panel expr, so we fetch this many dashboards and then stop.
pub const MAX_NAMED_FROM_SEARCH: usize = 16;

const PAGE_LIMIT: u32 = 200;
const MAX_CONFIGMAPS: usize = 2_000;
const MAX_PROVISIONED_PAGES: usize = 16;
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
    pub min_step: String,
    pub unsupported: Option<String>,
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
    pub unit: String,
    pub reducer: String,
    pub min_interval: String,
    pub unsupported: Option<String>,
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
    let datasource = panel.get("datasource");
    let mut queries = Vec::new();
    if let Some(targets) = panel.get("targets").and_then(Value::as_array) {
        for target in targets.iter().take(MAX_QUERIES_PER_PANEL) {
            if let Some(query) = read_query(target, datasource) {
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
        unit: panel
            .pointer("/fieldConfig/defaults/unit")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        reducer: panel
            .pointer("/options/reduceOptions/calcs/0")
            .and_then(Value::as_str)
            .unwrap_or("lastNotNull")
            .to_string(),
        min_interval: panel
            .get("interval")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        unsupported: panel_support_error(panel, kind, transformed),
    }
}

fn panel_support_error(panel: &Value, kind: PanelKind, transformed: bool) -> Option<String> {
    if transformed {
        let names = panel
            .get("transformations")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        return Some(format!("unsupported Grafana transformations: {names}"));
    }
    let type_name = panel
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !matches!(type_name, "timeseries" | "stat") {
        return Some(format!("unsupported Grafana panel type {type_name}"));
    }
    if panel
        .get("targets")
        .and_then(Value::as_array)
        .is_some_and(|targets| {
            targets
                .iter()
                .filter(|target| target.get("hide").and_then(Value::as_bool) != Some(true))
                .take(2)
                .count()
                > 1
        })
    {
        return Some(
            "unsupported Grafana multi-target panel; run an explicit raw query or open in Grafana"
                .to_string(),
        );
    }
    if panel
        .pointer("/fieldConfig/overrides")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
    {
        return Some("unsupported Grafana field overrides".to_string());
    }
    if panel
        .pointer("/fieldConfig/defaults/custom/stacking/mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode != "none")
    {
        return Some("unsupported Grafana stacked series".to_string());
    }
    if panel
        .pointer("/fieldConfig/defaults/custom/scaleDistribution/type")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode != "linear")
    {
        return Some("unsupported Grafana nonlinear axis".to_string());
    }
    if panel
        .pointer("/fieldConfig/defaults/custom/spanNulls")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Some("unsupported Grafana connected null values".to_string());
    }
    if kind == PanelKind::Stat {
        let options = panel.pointer("/options/reduceOptions");
        if options
            .and_then(|value| value.get("values"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Some("unsupported Grafana all-values stat".to_string());
        }
        let calcs = options
            .and_then(|value| value.get("calcs"))
            .and_then(Value::as_array);
        if calcs.is_some_and(|calcs| {
            calcs.len() != 1
                || !calcs[0].as_str().is_some_and(|calc| {
                    matches!(calc, "lastNotNull" | "min" | "max" | "mean" | "sum")
                })
        }) {
            return Some("unsupported Grafana stat reducer".to_string());
        }
    }
    None
}

fn read_query(target: &Value, panel_datasource: Option<&Value>) -> Option<PanelQuery> {
    if target.get("hide").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let expr = target
        .get("expr")
        .and_then(Value::as_str)
        .or_else(|| target.get("query").and_then(Value::as_str))
        .unwrap_or("")
        .trim();
    if expr.is_empty() {
        return None;
    }
    let datasource = [target.get("datasource"), panel_datasource]
        .into_iter()
        .flatten()
        .find_map(|value| datasource_name(Some(value)).map(|name| (name, value)));
    // The declared type determines the language; a UID identifies one instance.
    let dialect = dialect_of(
        datasource.as_ref().map(|(name, value)| {
            value
                .get("type")
                .and_then(Value::as_str)
                .filter(|kind| !kind.is_empty())
                .unwrap_or(name)
        }),
        target.get("queryType").and_then(Value::as_str),
    );
    Some(PanelQuery {
        ref_id: target
            .get("refId")
            .and_then(Value::as_str)
            .unwrap_or("A")
            .to_string(),
        expr: if expr.len() > super::prom::MAX_EXPR_BYTES {
            String::new()
        } else {
            expr.to_string()
        },
        dialect,
        datasource: datasource.map(|(name, _)| name),
        min_step: target
            .get("interval")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        unsupported: if expr.len() > super::prom::MAX_EXPR_BYTES {
            Some("panel expression exceeds 8 KiB".to_string())
        } else if target.get("instant").and_then(Value::as_bool) == Some(true) {
            Some("instant Grafana targets are unsupported in a range panel".to_string())
        } else if target
            .get("format")
            .and_then(Value::as_str)
            .is_some_and(|format| format != "time_series")
        {
            Some("unsupported Grafana target format".to_string())
        } else {
            None
        },
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

#[derive(Deserialize)]
struct Datasource {
    uid: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "jsonData")]
    settings: DatasourceSettings,
}
#[derive(Default, Deserialize)]
struct DatasourceSettings {
    #[serde(default, rename = "timeInterval")]
    scrape_interval: String,
}

/// Grafana's UID proxy keeps its configured backend and authentication together.
/// Datasource URLs and credential fields are never used to guess another Service.
pub async fn query_target(
    client: &Client,
    bound: &Bound,
    target: &PanelQuery,
    vars: &Variables,
) -> Fetched<super::prom::QueryResult> {
    let run = async {
        if let Some(why) = &target.unsupported {
            return Err(why.clone());
        }
        if target.dialect != QueryDialect::PromQL {
            return Err("the target is not a Prometheus datasource query".to_string());
        }
        let uid = target
            .datasource
            .as_deref()
            .ok_or("panel target has no explicit datasource UID")?;
        if uid.is_empty()
            || uid.len() > 128
            || !uid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(format!("unsupported datasource UID {uid}"));
        }
        let bytes = match tool_get(client, bound, &format!("api/datasources/uid/{uid}")).await {
            Fetched::Ok(bytes) => bytes,
            Fetched::Denied { what } => return Ok(Fetched::Denied { what }),
            Fetched::Failed { what, why } => return Ok(Fetched::Failed { what, why }),
        };
        let source: Datasource = serde_json::from_slice(&bytes)
            .map_err(|error| format!("datasource metadata is unsupported: {error}"))?;
        if source.uid != uid || source.kind != "prometheus" {
            return Err(format!(
                "datasource {uid} is {}, expected the same Prometheus UID",
                source.kind
            ));
        }
        let scrape = if source.settings.scrape_interval.is_empty() {
            15_000
        } else {
            duration_ms(&source.settings.scrape_interval)?
        };
        let (expr, step) = interpolate(target, vars, scrape)?;
        let path = format!("api/datasources/proxy/uid/{uid}/api/v1/query_range");
        let mut fetched = super::prom::query_range_path(
            client,
            bound,
            &path,
            &expr,
            vars.start_ms as f64 / 1000.0,
            vars.end_ms as f64 / 1000.0,
            &duration_text(step),
        )
        .await;
        if let Fetched::Ok(result) = &mut fetched {
            result.source = format!(
                "{} / datasource {uid} / target {}",
                bound.source(),
                target.ref_id
            );
            result.step_ms = step;
            result.expr = expr;
        }
        Ok(fetched)
    };
    match run.await {
        Ok(fetched) => fetched,
        Err(why) => Fetched::Failed {
            what: "grafana panel",
            why,
        },
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
        tool_get(client, bound, "api/search?type=dash-db&limit=100").await,
        parse_search,
    ) {
        Fetched::Ok(hits) => Fetched::Ok(filter_folders(hits, folder_allowlist)),
        other => other,
    }
}

/// One dashboard by uid, through the same bind as search, with its returned identity checked.
pub async fn fetch_dashboard(client: &Client, bound: &Bound, uid: &str) -> Fetched<Dashboard> {
    match into_parsed(
        tool_get(client, bound, &format!("api/dashboards/uid/{uid}")).await,
        parse_dashboard,
    ) {
        Fetched::Ok(dashboard) if dashboard.uid != uid => Fetched::Failed {
            what: "grafana",
            why: format!(
                "Grafana returned dashboard UID {:?} for requested UID {uid:?}",
                dashboard.uid
            ),
        },
        other => other,
    }
}

/// Dashboards Grafana's sidecar already stored as ConfigMaps.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Provisioned {
    pub dashboards: Vec<Dashboard>,
    /// ConfigMaps or keys past [`MAX_PROVISIONED`] were not walked.
    pub truncated: bool,
    pub errors: Vec<String>,
}

impl Provisioned {
    fn failed(mut self, why: String) -> Fetched<Self> {
        if self.dashboards.is_empty() {
            return Fetched::Failed {
                what: "grafana",
                why,
            };
        }
        self.errors.push(why);
        self.truncated = true;
        Fetched::Ok(self)
    }
}

/// List ConfigMaps labelled `grafana_dashboard=1` (or `true`) and parse each
/// `data` value as dashboard JSON. No Grafana API. `binaryData` is ignored.
/// Secrets are never listed: that is the sidecar's other watch, and not ours.
pub async fn fetch_provisioned_from_configmaps(client: &Client) -> Fetched<Provisioned> {
    let mut provisioned = Provisioned::default();
    let mut scanned = 0usize;
    let mut body_bytes = 0usize;
    let mut token: Option<String> = None;
    for _ in 0..MAX_PROVISIONED_PAGES {
        if body_bytes == crate::reach::MAX_BODY_BYTES {
            return provisioned.failed("provisioned dashboards exceed 8 MiB".to_string());
        }
        let mut params = ListParams::default()
            .limit(PAGE_LIMIT)
            .labels(DASHBOARD_SELECTOR);
        if let Some(token) = token.as_deref() {
            params = params.continue_token(token);
        }
        let request = Request::new("/api/v1/configmaps".to_string())
            .list(&params)
            .expect("fixed provisioning list parameters");
        let bytes = match crate::reach::bounded_request_with_status(
            client,
            "grafana",
            request,
            crate::reach::MAX_BODY_BYTES - body_bytes,
        )
        .await
        .1
        {
            Fetched::Ok(bytes) => bytes,
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { why, .. } => return provisioned.failed(why),
        };
        body_bytes += bytes.len();
        let page: ObjectList<ConfigMap> = match serde_json::from_slice(&bytes) {
            Ok(page) => page,
            Err(error) => {
                return provisioned.failed(format!(
                    "provisioned dashboards returned an unsupported response: {error}"
                ));
            }
        };
        for cm in page.items {
            scanned += 1;
            if scanned > MAX_CONFIGMAPS || provisioned.dashboards.len() >= MAX_PROVISIONED {
                provisioned.truncated = true;
                break;
            }
            collect_from_configmap(&cm, &mut provisioned.dashboards, &mut provisioned.truncated);
            if provisioned.truncated {
                break;
            }
        }
        let next = page.metadata.continue_.filter(|s| !s.is_empty());
        if next.is_none() || provisioned.truncated {
            return Fetched::Ok(provisioned);
        }
        if scanned == MAX_CONFIGMAPS || provisioned.dashboards.len() == MAX_PROVISIONED {
            provisioned.truncated = true;
            return Fetched::Ok(provisioned);
        }
        if next == token {
            return provisioned
                .failed("provisioned dashboards repeated a continuation token".to_string());
        }
        token = next;
    }
    provisioned.failed(format!(
        "provisioned dashboards exceed {MAX_PROVISIONED_PAGES} pages"
    ))
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

/// One PromQL a pod cell can join, copied from Grafana, not rewritten.
/// Prefers `sum by (namespace, pod)`. An expr that only names those labels
/// still qualifies. None means the caller keeps [`crate::overlay::CPU_EXPR`].
/// `$` is Grafana's engine (variables, `$__rate_interval`), not PromQL we
/// can send.
pub fn name_overlay_promql(dashboards: &[Dashboard]) -> Option<&str> {
    let mut best: Option<(&str, u8)> = None;
    for dashboard in dashboards {
        for panel in &dashboard.panels {
            for query in &panel.queries {
                let Some(score) = overlay_score(query) else {
                    continue;
                };
                match best {
                    Some((_, have)) if have >= score => {}
                    _ => best = Some((query.expr.as_str(), score)),
                }
                if score == 2 {
                    return Some(query.expr.as_str());
                }
            }
        }
    }
    best.map(|(expr, _)| expr)
}

fn overlay_score(query: &PanelQuery) -> Option<u8> {
    if query.dialect != QueryDialect::PromQL {
        return None;
    }
    if query.expr.contains('$') {
        return None;
    }
    if prefers_sum_by_pod_cell(&query.expr) {
        return Some(2);
    }
    if has_ident(&query.expr, "namespace") && has_ident(&query.expr, "pod") {
        return Some(1);
    }
    None
}

fn prefers_sum_by_pod_cell(expr: &str) -> bool {
    if !has_keyword(expr, "sum") {
        return false;
    }
    by_groupings(expr)
        .iter()
        .any(|grouping| has_ident(grouping, "namespace") && has_ident(grouping, "pod"))
}

fn by_groupings(expr: &str) -> Vec<&str> {
    let bytes = expr.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        if eq_ignore_ascii_case(&bytes[i..i + 2], b"by") && ident_bounded(bytes, i, 2) {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                if let Some(inner) = grouping_inner(expr, j) {
                    out.push(inner);
                    i = j + inner.len() + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn grouping_inner(expr: &str, open: usize) -> Option<&str> {
    let bytes = expr.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => quote = Some(c),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&expr[open + 1..i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn has_keyword(expr: &str, word: &str) -> bool {
    let bytes = expr.as_bytes();
    let needle = word.as_bytes();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if eq_ignore_ascii_case(&bytes[i..i + needle.len()], needle)
            && ident_bounded(bytes, i, needle.len())
        {
            return true;
        }
        i += 1;
    }
    false
}

fn has_ident(text: &str, ident: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = ident.as_bytes();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if bytes[i..].starts_with(needle) && ident_bounded(bytes, i, needle.len()) {
            return true;
        }
        i += 1;
    }
    false
}

fn ident_bounded(bytes: &[u8], start: usize, len: usize) -> bool {
    let before = start == 0 || !ident_char(bytes[start - 1]);
    let after = start + len == bytes.len() || !ident_char(bytes[start + len]);
    before && after
}

fn ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn eq_ignore_ascii_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
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
