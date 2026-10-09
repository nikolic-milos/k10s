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

pub(super) fn observe_reach(
    fetched: Fetched<k10s_data::read::ObserveTools>,
) -> k10s_provider::ObserveReach {
    use k10s_provider::{ObserveReach, ToolPresence};
    let error = match fetched {
        Fetched::Ok(tools) => {
            return ObserveReach {
                prometheus: tool_presence(tools.prometheus),
                loki: tool_presence(tools.loki),
                traces: tool_presence(tools.traces),
                error: (!tools.errors.is_empty()).then(|| tools.errors.join("; ")),
            };
        }
        Fetched::Denied { what } => format!("{what}: access denied for this account"),
        Fetched::Failed { why, .. } => why,
    };
    ObserveReach {
        prometheus: ToolPresence::Blocked,
        loki: ToolPresence::Blocked,
        traces: ToolPresence::Blocked,
        error: Some(error),
    }
}

pub(super) fn loki_probe_outcome(
    fetched: k10s_data::read::Fetched<k10s_data::read::Seen>,
) -> Result<k10s_provider::ToolPresence, String> {
    match fetched {
        k10s_data::read::Fetched::Ok(seen) => Ok(tool_presence(seen)),
        k10s_data::read::Fetched::Denied { what } => {
            Err(format!("{what}: access denied for this account"))
        }
        k10s_data::read::Fetched::Failed { why, .. } => Err(why),
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
    k10s_provider::grafana_panel_path(uid, panel_id).map(|path| format!("{base}{path}"))
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
    browser_service: Option<&(String, String)>,
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
                browser_forward: browser_service.map(|(namespace, name)| {
                    k10s_provider::ForwardRequest {
                        namespace: namespace.clone(),
                        name: name.clone(),
                        service: true,
                    }
                }),
                datasource_uid: None,
                ref_id: String::new(),
                unit: panel.unit.clone(),
                reducer: panel.reducer.clone(),
                min_step: String::new(),
                min_interval: panel.min_interval.clone(),
                unsupported: panel.unsupported.clone(),
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
                browser_forward: browser_service.map(|(namespace, name)| {
                    k10s_provider::ForwardRequest {
                        namespace: namespace.clone(),
                        name: name.clone(),
                        service: true,
                    }
                }),
                datasource_uid: query.datasource.clone(),
                ref_id: query.ref_id.clone(),
                unit: panel.unit.clone(),
                reducer: panel.reducer.clone(),
                min_step: query.min_step.clone(),
                min_interval: panel.min_interval.clone(),
                unsupported: panel
                    .unsupported
                    .clone()
                    .or_else(|| query.unsupported.clone()),
            });
        }
    }
}

pub(super) fn grafana_outcome(
    fetched: Fetched<k10s_data::read::GrafanaCatalog>,
) -> k10s_provider::GrafanaOutcome {
    match fetched {
        Fetched::Ok(catalog) if !catalog.served && catalog.errors.is_empty() => {
            k10s_provider::GrafanaOutcome::Absent
        }
        Fetched::Ok(catalog) => {
            let mut panels = Vec::new();
            for dashboard in &catalog.dashboards {
                flatten_dashboard(
                    dashboard,
                    catalog.browser_base.as_deref(),
                    catalog.browser_service.as_ref(),
                    &mut panels,
                );
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
                    browser_forward: catalog.browser_service.as_ref().map(|(namespace, name)| {
                        k10s_provider::ForwardRequest {
                            namespace: namespace.clone(),
                            name: name.clone(),
                            service: true,
                        }
                    }),
                    datasource_uid: None,
                    ref_id: String::new(),
                    unit: String::new(),
                    reducer: String::new(),
                    min_step: String::new(),
                    min_interval: String::new(),
                    unsupported: Some("dashboard was not fetched at the catalog limit".to_string()),
                });
            }
            k10s_provider::GrafanaOutcome::Catalog {
                panels,
                truncated: catalog.truncated,
                notices: catalog.errors,
            }
        }
        Fetched::Denied { what } => k10s_provider::GrafanaOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::GrafanaOutcome::Failed(why),
    }
}

fn series_labels(labels: &[(String, String)]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(
        labels
            .iter()
            .map(|(name, value)| name.len() + value.len() + 4)
            .sum::<usize>()
            + 2,
    );
    text.push('{');
    for (index, (name, value)) in labels.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        write!(&mut text, "{name}={value:?}").expect("writing to a String");
    }
    text.push('}');
    text
}

pub(super) fn prom_outcome(
    fetched: Fetched<Option<k10s_data::integrations::prom::QueryResult>>,
) -> k10s_provider::PromOutcome {
    use k10s_provider::{FetchTiming, PromOutcome, PromSeriesView};
    match fetched {
        Fetched::Ok(None) => PromOutcome::Absent,
        Fetched::Ok(Some(result)) => {
            let started = std::time::Instant::now();
            let metrics = k10s_data::telemetry::request_metrics().unwrap_or_default();
            let series = result
                .series
                .into_iter()
                .map(|series| PromSeriesView {
                    labels: series_labels(&series.labels),
                    label_set: series.labels,
                    points: series.points,
                    gaps: series.gaps,
                })
                .collect();
            let projection_ns = started.elapsed().as_nanos() as u64;
            let total_ns = k10s_data::telemetry::request_metrics()
                .map_or(metrics.total_ns, |metrics| metrics.total_ns);
            PromOutcome::Series {
                series,
                truncated: result.truncated,
                dropped_series: result.dropped_series,
                warnings: result.warnings,
                infos: result.infos,
                source: result.source,
                step_ms: result.step_ms,
                expr: result.expr,
                timing: FetchTiming {
                    bytes: result.timing.bytes,
                    network_ns: result.timing.network_ns,
                    decode_ns: result.timing.decode_ns,
                    projection_ns,
                    queue_ns: metrics.queue_ns,
                    total_ns,
                    requests: metrics.requests,
                    total_bytes: metrics.bytes,
                    binding_network_ns: metrics.network_ns.saturating_sub(result.timing.network_ns),
                },
            }
        }
        Fetched::Denied { what } => PromOutcome::Denied(what),
        Fetched::Failed { why, .. } => PromOutcome::Failed(why),
    }
}

pub(super) fn loki_outcome(
    fetched: Fetched<Option<k10s_data::integrations::loki::Logs>>,
) -> k10s_provider::LokiOutcome {
    use k10s_provider::{LogEntry, LokiOutcome};
    match fetched {
        Fetched::Ok(None) => LokiOutcome::Absent,
        Fetched::Ok(Some(logs)) => {
            let mut entries = Vec::new();
            for stream in logs.streams {
                let labels: std::sync::Arc<[(String, String)]> = stream.labels.into();
                for line in stream.lines {
                    let trace_id = line
                        .metadata
                        .iter()
                        .chain(labels.iter())
                        .find(|(key, _)| key == "trace_id")
                        .map(|(_, value)| value.as_str())
                        .filter(|value| {
                            matches!(value.len(), 16 | 32)
                                && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                                && value.bytes().any(|byte| byte != b'0')
                        })
                        .map(str::to_ascii_lowercase);
                    entries.push(LogEntry {
                        timestamp_ns: line.ts_ns,
                        line: line.line,
                        labels: labels.clone(),
                        metadata: line.metadata,
                        trace_id,
                    });
                }
            }
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.timestamp_ns));
            LokiOutcome::Logs {
                entries,
                truncated: logs.truncated,
                dropped_streams: logs.dropped_streams,
                dropped_lines: logs.dropped_lines,
                clipped_lines: logs.clipped_lines,
                limit_reached: logs.limit_reached,
                source: logs.source,
            }
        }
        Fetched::Denied { what } => LokiOutcome::Denied(what),
        Fetched::Failed { why, .. } => LokiOutcome::Failed(why),
    }
}

pub(super) fn trace_outcome(
    fetched: Fetched<Option<k10s_data::integrations::traces::Trace>>,
) -> k10s_provider::TraceOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_provider::TraceOutcome::Absent,
        Fetched::Ok(Some(trace)) if trace.spans.is_empty() && trace.trace_id.is_empty() => {
            k10s_provider::TraceOutcome::Empty {
                source: trace.source,
            }
        }
        Fetched::Ok(Some(trace)) => k10s_provider::TraceOutcome::Trace {
            trace_id: trace.trace_id,
            source: trace.source,
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
                    namespace: span.namespace,
                    pod: span.pod,
                    pod_uid: span.pod_uid,
                    node: span.node,
                })
                .collect(),
        },
        Fetched::Denied { what } => k10s_provider::TraceOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TraceOutcome::Failed(why),
    }
}
