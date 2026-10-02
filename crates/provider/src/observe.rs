#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertPodOutcome {
    Pod { namespace: String, pod: String },
    Absent,
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertmanagerEndpoint {
    pub namespace: String,
    pub service: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertmanagerOutcome {
    Ready(AlertmanagerEndpoint),
    Absent,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertMatcher {
    pub name: String,
    pub value: String,
    pub is_regex: bool,
    pub is_equal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilenceRequest {
    pub endpoint: AlertmanagerEndpoint,
    pub matchers: Vec<AlertMatcher>,
    pub window: std::ops::Range<std::time::SystemTime>,
    pub created_by: String,
    pub comment: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SilenceOutcome {
    Applied { id: String },
    NeedsConfirm { starts_at: String, ends_at: String },
    Denied { what: &'static str, why: String },
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPresence {
    Ready,
    /// The adapter is not on this cluster. The pane stays down.
    Missing,
    /// The tool is visible but this process cannot speak to it.
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObserveReach {
    pub prometheus: ToolPresence,
    pub loki: ToolPresence,
    pub traces: ToolPresence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryDialect {
    PromQL,
    LogQL,
    TraceQL,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrafanaPanelKind {
    Timeseries,
    Stat,
    Gauge,
    Table,
    Logs,
    Heatmap,
    Bar,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrafanaPanelRow {
    pub dashboard_uid: String,
    pub dashboard_title: String,
    pub panel_id: i64,
    pub title: String,
    pub kind: GrafanaPanelKind,
    pub expr: String,
    pub dialect: QueryDialect,
    pub transformed: bool,
    pub browser_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrafanaOutcome {
    Catalog {
        panels: Vec<GrafanaPanelRow>,
        truncated: bool,
    },
    Absent,
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromSeriesView {
    pub labels: String,
    pub points: Vec<(i64, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromOutcome {
    Series {
        series: Vec<PromSeriesView>,
        truncated: bool,
        dropped_series: usize,
    },
    Absent,
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LokiOutcome {
    Logs { lines: Vec<String>, truncated: bool },
    Absent,
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanView {
    pub id: String,
    pub parent: String,
    pub name: String,
    pub service: String,
    pub start_us: u64,
    pub duration_us: u64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceOutcome {
    Trace {
        trace_id: String,
        spans: Vec<SpanView>,
    },
    Absent,
    Denied(&'static str),
    Failed(String),
}
