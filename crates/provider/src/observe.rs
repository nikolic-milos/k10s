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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveReach {
    pub prometheus: ToolPresence,
    pub loki: ToolPresence,
    pub traces: ToolPresence,
    pub error: Option<String>,
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
    pub browser_forward: Option<crate::ForwardRequest>,
    pub datasource_uid: Option<String>,
    pub ref_id: String,
    pub unit: String,
    pub reducer: String,
    pub min_step: String,
    pub min_interval: String,
    pub unsupported: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrafanaOutcome {
    Catalog {
        panels: Vec<GrafanaPanelRow>,
        truncated: bool,
        notices: Vec<String>,
    },
    Absent,
    Denied(&'static str),
    Failed(String),
}

/// A dashboard UID occupies one path segment in both browser routes.
pub fn grafana_panel_path(uid: &str, panel_id: i64) -> Option<String> {
    use std::fmt::Write as _;
    if matches!(uid, "" | "." | "..") {
        return None;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut path = String::with_capacity(uid.len() + 24);
    path.push_str("/d/");
    for byte in uid.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => path.push(byte as char),
            _ => {
                path.push('%');
                path.push(HEX[(byte >> 4) as usize] as char);
                path.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    write!(path, "?viewPanel={panel_id}").expect("formatting into a String");
    Some(path)
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromSeriesView {
    pub labels: String,
    pub label_set: Vec<(String, String)>,
    pub points: Vec<(i64, f64)>,
    pub gaps: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromOutcome {
    Series {
        series: Vec<PromSeriesView>,
        truncated: bool,
        dropped_series: usize,
        warnings: Vec<String>,
        infos: Vec<String>,
        source: String,
        step_ms: u64,
        expr: String,
        timing: super::FetchTiming,
    },
    Absent,
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LokiOutcome {
    Logs {
        entries: Vec<LogEntry>,
        truncated: bool,
        dropped_streams: usize,
        dropped_lines: usize,
        clipped_lines: usize,
        limit_reached: bool,
        source: String,
    },
    Absent,
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp_ns: u64,
    pub line: String,
    pub labels: std::sync::Arc<[(String, String)]>,
    pub metadata: Vec<(String, String)>,
    pub trace_id: Option<String>,
}

impl LogEntry {
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.metadata
            .iter()
            .chain(self.labels.iter())
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
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
    pub namespace: String,
    pub pod: String,
    pub pod_uid: String,
    pub node: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceOutcome {
    Empty,
    Trace {
        trace_id: String,
        spans: Vec<SpanView>,
    },
    Absent,
    Denied(&'static str),
    Failed(String),
}
