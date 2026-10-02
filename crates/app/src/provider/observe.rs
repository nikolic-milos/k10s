//! Translate observability queries and alert operations into provider outcomes.

use super::kubernetes::table_outcome;
use k10s_data::read::Fetched;

pub(super) fn omitted_notice_answer(uid: &str) -> Option<k10s_provider::AlertPodOutcome> {
    k10s_data::integrations::alertmanager::is_omitted_notice(uid).then(|| {
        k10s_provider::AlertPodOutcome::Failed(
            "this row counts alerts that are not shown; it names no pod".to_string(),
        )
    })
}

pub(super) fn alert_pod_outcome(
    fingerprint: &str,
    fetched: Fetched<Option<k10s_data::integrations::alertmanager::Alerts>>,
) -> k10s_provider::AlertPodOutcome {
    use k10s_provider::AlertPodOutcome;
    match fetched {
        Fetched::Ok(None) => AlertPodOutcome::Absent,
        Fetched::Ok(Some(alerts)) => {
            let Some(alert) = alerts
                .items
                .into_iter()
                .find(|alert| alert.fingerprint == fingerprint)
            else {
                return AlertPodOutcome::Failed(
                    "the selected alert is no longer in the listing; refresh the alerts"
                        .to_string(),
                );
            };
            if alert.namespace.is_empty() || alert.pod.is_empty() {
                return AlertPodOutcome::Failed(
                    "this alert needs both namespace and pod labels to locate an object"
                        .to_string(),
                );
            }
            AlertPodOutcome::Pod {
                namespace: alert.namespace,
                pod: alert.pod,
            }
        }
        Fetched::Denied { what } => AlertPodOutcome::Denied(what),
        Fetched::Failed { why, .. } => AlertPodOutcome::Failed(why),
    }
}

pub(super) fn alertmanager_outcome(
    answer: k10s_data::reach::ToolReach,
) -> k10s_provider::AlertmanagerOutcome {
    use k10s_data::reach::{ToolReach, Transport};
    use k10s_provider::{AlertmanagerEndpoint, AlertmanagerOutcome};
    match answer {
        ToolReach::Absent { .. } => AlertmanagerOutcome::Absent,
        ToolReach::Unbound(unbound) => AlertmanagerOutcome::Failed(unbound.why),
        ToolReach::Bound(bound) => match bound.transport {
            Transport::Proxy { namespace, service, port } => {
                AlertmanagerOutcome::Ready(AlertmanagerEndpoint { namespace, service, port })
            }
            Transport::NeedsForward { .. } => AlertmanagerOutcome::Failed(
                "Alertmanager cannot be reached through the API-server proxy; silence writes need that route".to_string(),
            ),
            Transport::Url { .. } => AlertmanagerOutcome::Failed(
                "Alertmanager silence writes are only implemented on the API-server proxy".to_string(),
            ),
        },
    }
}

pub(super) fn alertmanager_bound(
    endpoint: &k10s_provider::AlertmanagerEndpoint,
) -> k10s_data::reach::Bound {
    use k10s_data::reach::{Bound, ToolAuth, ToolKind, Transport};
    Bound {
        kind: ToolKind::Alertmanager,
        found: None,
        transport: Transport::Proxy {
            namespace: endpoint.namespace.clone(),
            service: endpoint.service.clone(),
            port: endpoint.port,
        },
        auth: ToolAuth::Anonymous,
    }
}

pub(super) fn silence_spec(
    request: &k10s_provider::SilenceRequest,
) -> Result<k10s_data::integrations::alertmanager::SilenceSpec, String> {
    use k10s_data::integrations::alertmanager::{Matcher, SilenceSpec};
    SilenceSpec::for_window(
        request
            .matchers
            .iter()
            .map(|matcher| Matcher {
                name: matcher.name.clone(),
                value: matcher.value.clone(),
                is_regex: matcher.is_regex,
                is_equal: matcher.is_equal,
            })
            .collect(),
        request.window.clone(),
        request.created_by.clone(),
        request.comment.clone(),
    )
}

pub(super) fn silence_outcome(
    answer: k10s_data::integrations::alertmanager::SilenceOutcome,
    window: std::ops::Range<String>,
) -> k10s_provider::SilenceOutcome {
    use k10s_data::integrations::alertmanager::SilenceOutcome as Plane;
    use k10s_provider::SilenceOutcome as Shell;
    match answer {
        Plane::Applied { id, .. } => Shell::Applied { id },
        Plane::NeedsConfirm { .. } => Shell::NeedsConfirm {
            starts_at: window.start,
            ends_at: window.end,
        },
        Plane::Denied { what, why } => Shell::Denied { what, why },
        Plane::Failed { why, .. } => Shell::Failed(why),
    }
}

pub(super) fn optional_table_outcome(
    fetched: Fetched<Option<k10s_data::browse::TablePage>>,
) -> k10s_provider::TableOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_provider::TableOutcome::Absent,
        Fetched::Ok(Some(page)) => table_outcome(Fetched::Ok(page)),
        Fetched::Denied { what } => k10s_provider::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TableOutcome::Failed(why),
    }
}

pub(super) fn tool_presence(seen: k10s_data::read::Seen) -> k10s_provider::ToolPresence {
    match seen {
        k10s_data::read::Seen::Bound => k10s_provider::ToolPresence::Ready,
        k10s_data::read::Seen::Unbound => k10s_provider::ToolPresence::Blocked,
        k10s_data::read::Seen::Absent => k10s_provider::ToolPresence::Missing,
    }
}

/// The uid arrives from a fetched dashboard listing, so it is
/// attacker-shaped; percent-encoding here keeps the launcher's URL gate
/// sufficient. Real Grafana uids are alphanumeric with `-`/`_` and pass
/// through unchanged.
fn grafana_panel_url(base: Option<&str>, uid: &str, panel_id: i64) -> Option<String> {
    let base = base?.trim_end_matches('/');
    if uid.is_empty() {
        return Some(base.to_string());
    }
    let mut encoded = String::with_capacity(uid.len());
    for byte in uid.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => encoded.push(byte as char),
            _ => {
                encoded.push('%');
                encoded.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
                encoded.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap_or('0'));
            }
        }
    }
    Some(format!("{base}/d/{encoded}?viewPanel={panel_id}"))
}

fn panel_kind(
    kind: k10s_data::integrations::grafana::PanelKind,
) -> k10s_provider::GrafanaPanelKind {
    match kind {
        k10s_data::integrations::grafana::PanelKind::Timeseries => {
            k10s_provider::GrafanaPanelKind::Timeseries
        }
        k10s_data::integrations::grafana::PanelKind::Stat => k10s_provider::GrafanaPanelKind::Stat,
        k10s_data::integrations::grafana::PanelKind::Gauge => {
            k10s_provider::GrafanaPanelKind::Gauge
        }
        k10s_data::integrations::grafana::PanelKind::Table => {
            k10s_provider::GrafanaPanelKind::Table
        }
        k10s_data::integrations::grafana::PanelKind::Logs => k10s_provider::GrafanaPanelKind::Logs,
        k10s_data::integrations::grafana::PanelKind::Heatmap => {
            k10s_provider::GrafanaPanelKind::Heatmap
        }
        k10s_data::integrations::grafana::PanelKind::Bar => k10s_provider::GrafanaPanelKind::Bar,
        k10s_data::integrations::grafana::PanelKind::Unsupported => {
            k10s_provider::GrafanaPanelKind::Unsupported
        }
    }
}

fn query_dialect(
    dialect: k10s_data::integrations::grafana::QueryDialect,
) -> k10s_provider::QueryDialect {
    match dialect {
        k10s_data::integrations::grafana::QueryDialect::PromQL => {
            k10s_provider::QueryDialect::PromQL
        }
        k10s_data::integrations::grafana::QueryDialect::LogQL => k10s_provider::QueryDialect::LogQL,
        k10s_data::integrations::grafana::QueryDialect::TraceQL => {
            k10s_provider::QueryDialect::TraceQL
        }
        k10s_data::integrations::grafana::QueryDialect::Unknown => {
            k10s_provider::QueryDialect::Unknown
        }
    }
}

fn flatten_dashboard(
    dashboard: &k10s_data::integrations::grafana::Dashboard,
    browser_base: Option<&str>,
    into: &mut Vec<k10s_provider::GrafanaPanelRow>,
) {
    for panel in &dashboard.panels {
        if panel.queries.is_empty() {
            into.push(k10s_provider::GrafanaPanelRow {
                dashboard_uid: dashboard.uid.clone(),
                dashboard_title: dashboard.title.clone(),
                panel_id: panel.id,
                title: panel.title.clone(),
                kind: panel_kind(panel.kind),
                expr: String::new(),
                dialect: k10s_provider::QueryDialect::Unknown,
                transformed: panel.transformed,
                browser_url: grafana_panel_url(browser_base, &dashboard.uid, panel.id),
            });
            continue;
        }
        for query in &panel.queries {
            into.push(k10s_provider::GrafanaPanelRow {
                dashboard_uid: dashboard.uid.clone(),
                dashboard_title: dashboard.title.clone(),
                panel_id: panel.id,
                title: if panel.title.is_empty() {
                    query.ref_id.clone()
                } else {
                    format!("{} {}", panel.title, query.ref_id)
                },
                kind: panel_kind(panel.kind),
                expr: query.expr.clone(),
                dialect: query_dialect(query.dialect),
                transformed: panel.transformed,
                browser_url: grafana_panel_url(browser_base, &dashboard.uid, panel.id),
            });
        }
    }
}

pub(super) fn grafana_outcome(
    fetched: Fetched<k10s_data::read::GrafanaCatalog>,
) -> k10s_provider::GrafanaOutcome {
    match fetched {
        Fetched::Ok(catalog) if !catalog.served => k10s_provider::GrafanaOutcome::Absent,
        Fetched::Ok(catalog) => {
            let mut panels = Vec::new();
            for dashboard in &catalog.dashboards {
                flatten_dashboard(dashboard, catalog.browser_base.as_deref(), &mut panels);
            }
            for hit in catalog.extra_hits {
                panels.push(k10s_provider::GrafanaPanelRow {
                    dashboard_uid: hit.uid.clone(),
                    dashboard_title: hit.title.clone(),
                    panel_id: 0,
                    title: hit.title,
                    kind: k10s_provider::GrafanaPanelKind::Unsupported,
                    expr: String::new(),
                    dialect: k10s_provider::QueryDialect::Unknown,
                    transformed: false,
                    browser_url: grafana_panel_url(catalog.browser_base.as_deref(), &hit.uid, 0),
                });
            }
            k10s_provider::GrafanaOutcome::Catalog {
                panels,
                truncated: catalog.truncated,
            }
        }
        Fetched::Denied { what } => k10s_provider::GrafanaOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::GrafanaOutcome::Failed(why),
    }
}

fn series_labels(labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return "{}".to_string();
    }
    let inner = labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{inner}}}")
}

pub(super) fn prom_outcome(
    fetched: Fetched<Option<k10s_data::integrations::prom::QueryResult>>,
) -> k10s_provider::PromOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_provider::PromOutcome::Absent,
        Fetched::Ok(Some(result)) => k10s_provider::PromOutcome::Series {
            series: result
                .series
                .into_iter()
                .map(|series| k10s_provider::PromSeriesView {
                    labels: series_labels(&series.labels),
                    points: series.points,
                })
                .collect(),
            truncated: result.truncated,
            dropped_series: result.dropped_series,
        },
        Fetched::Denied { what } => k10s_provider::PromOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::PromOutcome::Failed(why),
    }
}

pub(super) fn loki_outcome(
    fetched: Fetched<Option<k10s_data::integrations::loki::Logs>>,
) -> k10s_provider::LokiOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_provider::LokiOutcome::Absent,
        Fetched::Ok(Some(logs)) => {
            let mut lines = Vec::new();
            for stream in &logs.streams {
                let labels = series_labels(&stream.labels);
                for line in &stream.lines {
                    lines.push(format!("{} {labels} {}", line.ts_ns, line.line));
                }
            }
            k10s_provider::LokiOutcome::Logs {
                lines,
                truncated: logs.truncated,
            }
        }
        Fetched::Denied { what } => k10s_provider::LokiOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::LokiOutcome::Failed(why),
    }
}

pub(super) fn trace_outcome(
    fetched: Fetched<Option<k10s_data::integrations::traces::Trace>>,
) -> k10s_provider::TraceOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_provider::TraceOutcome::Absent,
        Fetched::Ok(Some(trace)) => k10s_provider::TraceOutcome::Trace {
            trace_id: trace.trace_id,
            spans: trace
                .spans
                .into_iter()
                .map(|span| k10s_provider::SpanView {
                    id: span.id,
                    parent: span.parent,
                    name: span.name,
                    service: span.service,
                    start_us: span.start_us,
                    duration_us: span.duration_us,
                    status: span.status,
                })
                .collect(),
        },
        Fetched::Denied { what } => k10s_provider::TraceOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TraceOutcome::Failed(why),
    }
}
