//! Bounded observability queries and confirmed silence requests.

use super::{Fetched, Reader};
use crate::integrations::{alertmanager, grafana, loki, prom, traces};
use crate::reach::{Bound, ReachSettings, ToolKind, ToolReach};
use crate::{overlay, reach};
use kube::Client;
use std::time::{SystemTime, UNIX_EPOCH};

mod sources;
pub(super) use sources::Sources;

/// Dashboards we can run as queries, plus a system-browser base for panels
/// this process will not execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrafanaCatalog {
    pub dashboards: Vec<grafana::Dashboard>,
    pub extra_hits: Vec<grafana::SearchHit>,
    pub truncated: bool,
    pub browser_base: Option<String>,
    pub browser_service: Option<(String, String)>,
    pub served: bool,
    pub errors: Vec<String>,
}

/// Whether an in-cluster tool answered a bind. [`Seen::Absent`] hides a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Bound,
    Unbound,
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveTools {
    pub prometheus: Seen,
    pub loki: Seen,
    pub traces: Seen,
    pub errors: Vec<String>,
}

impl ObserveTools {
    fn blocked(why: String) -> Self {
        Self {
            prometheus: Seen::Unbound,
            loki: Seen::Unbound,
            traces: Seen::Unbound,
            errors: vec![why],
        }
    }
}

impl Reader {
    pub fn observe_probe(
        &self,
        reply: impl FnOnce(Fetched<ObserveTools>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let sources = self.observe_sources.clone();
        self.telemetry.submit(
            &self.handle,
            async move { Fetched::Ok(sources.probe(&client).await) },
            reply,
        )
    }

    pub fn observe_schedule_counts(&self) -> crate::telemetry::ScheduleCounts {
        self.telemetry.counts()
    }

    pub fn resolve_observe_scope(
        &self,
        request: crate::describe::DescribeRequest,
        reply: impl FnOnce(Fetched<crate::telemetry::Scope>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let targets = self.targets.clone();
        self.telemetry.submit(
            &self.handle,
            async move { crate::telemetry::resolve_scope(&client, &targets, &request).await },
            reply,
        )
    }

    pub fn observe_prometheus(
        &self,
        expr: String,
        start: f64,
        end: f64,
        step: String,
        reply: impl FnOnce(Fetched<Option<prom::QueryResult>>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let sources = self.observe_sources.clone();
        self.telemetry.submit(
            &self.handle,
            async move {
                let outcome =
                    query_prometheus_optional(&client, Some(&sources), &expr, start, end, &step)
                        .await;
                sources.complete(outcome).await
            },
            reply,
        )
    }

    pub fn observe_loki(
        &self,
        query: loki::RangeQuery,
        reply: impl FnOnce(Fetched<Option<loki::Logs>>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let sources = self.observe_sources.clone();
        self.telemetry.submit(
            &self.handle,
            async move {
                let outcome = query_loki_optional(&client, Some(&sources), &query).await;
                sources.complete(outcome).await
            },
            reply,
        )
    }

    pub fn observe_trace(
        &self,
        trace_id: String,
        reply: impl FnOnce(Fetched<Option<traces::Trace>>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let sources = self.observe_sources.clone();
        self.telemetry.submit(
            &self.handle,
            async move {
                let outcome = lookup_trace_optional(&client, Some(&sources), &trace_id).await;
                sources.complete(outcome).await
            },
            reply,
        )
    }

    pub fn observe_grafana(
        &self,
        reply: impl FnOnce(Fetched<GrafanaCatalog>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let sources = self.observe_sources.clone();
        self.telemetry.submit(
            &self.handle,
            async move {
                let outcome = load_grafana_catalog(&client, Some(&sources)).await;
                sources.complete(outcome).await
            },
            reply,
        )
    }

    pub fn observe_panel(
        &self,
        target: grafana::PanelQuery,
        variables: grafana::Variables,
        reply: impl FnOnce(Fetched<Option<prom::QueryResult>>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let sources = self.observe_sources.clone();
        self.telemetry.submit(
            &self.handle,
            async move {
                let outcome = match sources.bind(&client, ToolKind::Grafana).await {
                    ToolReach::Absent { .. } => Fetched::Ok(None),
                    ToolReach::Unbound(unbound) => Fetched::Failed {
                        what: "grafana",
                        why: unbound.why,
                    },
                    ToolReach::Bound(bound) => {
                        optional(grafana::query_target(&client, &bound, &target, &variables).await)
                    }
                };
                sources.complete(outcome).await
            },
            reply,
        )
    }

    /// [`Fetched::Ok`]`None` is absence: the pane stays hidden.
    pub fn fetch_alertmanager_alerts(
        &self,
        reply: impl FnOnce(Fetched<Option<alertmanager::Alerts>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(fetch_alertmanager_optional(&client).await);
        });
    }

    /// Write to the Alertmanager endpoint that was reviewed. A new discovery
    /// here could redirect the confirmation to a different service.
    pub fn create_silence(
        &self,
        bound: reach::Bound,
        spec: alertmanager::SilenceSpec,
        confirm: bool,
        reply: impl FnOnce(alertmanager::SilenceOutcome) + Send + 'static,
    ) {
        let client = self.interactive.clone();
        self.handle.spawn(async move {
            reply(alertmanager::create_silence(&client, &bound, &spec, confirm).await);
        });
    }

    /// Find and bind an in-cluster tool. First paint must not wait on this.
    pub fn bind_tool(
        &self,
        kind: ToolKind,
        settings: ReachSettings,
        reply: impl FnOnce(ToolReach) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(reach::bind(&client, kind, &settings).await);
        });
    }

    pub fn tool_get(
        &self,
        bound: Bound,
        rest: String,
        reply: impl FnOnce(Fetched<Vec<u8>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(reach::tool_get(&client, &bound, &rest).await);
        });
    }

    /// Run one already-named PromQL range query through a previously bound
    /// endpoint. Binding and querying remain separate so tool discovery never
    /// delays the scene's first publish or first paint.
    pub fn query_prometheus_range(
        &self,
        bound: Bound,
        expr: String,
        start: f64,
        end: f64,
        step: String,
        reply: impl FnOnce(Fetched<prom::QueryResult>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(prom::query_range(&client, &bound, &expr, start, end, &step).await);
        });
    }

    /// Grafana dashboards as extracted queries. Provisioned ConfigMaps also
    /// work without a Grafana bind. A bound API's failure is reported even
    /// when provisioned dashboards exist, so a partial read cannot look complete.
    /// [`GrafanaCatalog::served`] is false only when Grafana is absent and
    /// nothing was provisioned.
    pub fn fetch_grafana_catalog(
        &self,
        reply: impl FnOnce(Fetched<GrafanaCatalog>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(load_grafana_catalog(&client, None).await);
        });
    }

    /// Bind Prometheus, Loki, and a trace store. First paint must not wait.
    pub fn probe_observe_tools(&self, reply: impl FnOnce(ObserveTools) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(probe_observe_tools(&client).await);
        });
    }

    /// Named PromQL through a freshly bound Prometheus. [`Fetched::Ok`]`None`
    /// is absence: the PromQL box stays hidden.
    pub fn query_prometheus(
        &self,
        expr: String,
        start: f64,
        end: f64,
        step: String,
        reply: impl FnOnce(Fetched<Option<prom::QueryResult>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(query_prometheus_optional(&client, None, &expr, start, end, &step).await);
        });
    }

    /// LogQL against a bound Loki. [`Fetched::Ok`]`None` hides the Loki pane.
    pub fn query_loki(
        &self,
        query: loki::RangeQuery,
        reply: impl FnOnce(Fetched<Option<loki::Logs>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(query_loki_optional(&client, None, &query).await);
        });
    }

    /// Trace lookup uses Jaeger only when Tempo is absent. Binding failures
    /// retain their own reason; [`Fetched::Ok`]`None` is absence of both stores.
    pub fn lookup_trace(
        &self,
        trace_id: String,
        reply: impl FnOnce(Fetched<Option<traces::Trace>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(lookup_trace_optional(&client, None, &trace_id).await);
        });
    }
}
pub(super) async fn bind_prometheus(
    client: &Client,
    settings: &ReachSettings,
) -> Fetched<reach::Bound> {
    match reach::bind(client, ToolKind::Prometheus, settings).await {
        ToolReach::Absent { .. } => Fetched::Failed {
            what: "prometheus",
            why: "Prometheus is not in this cluster".to_string(),
        },
        ToolReach::Unbound(unbound) => Fetched::Failed {
            what: "prometheus",
            why: unbound.why,
        },
        ToolReach::Bound(bound) => Fetched::Ok(bound),
    }
}

pub(super) async fn query_bound(
    client: &Client,
    bound: &reach::Bound,
    expr: &str,
) -> Fetched<prom::QueryResult> {
    let end = unix_secs();
    let start = end - overlay::RANGE_SECS;
    prom::query_range(client, bound, expr, start, end, overlay::STEP).await
}

pub(super) async fn query_prometheus(
    client: &Client,
    settings: &ReachSettings,
    expr: &str,
) -> Fetched<prom::QueryResult> {
    match bind_prometheus(client, settings).await {
        Fetched::Ok(bound) => query_bound(client, &bound, expr).await,
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

async fn load_grafana_catalog(
    client: &Client,
    sources: Option<&Sources>,
) -> Fetched<GrafanaCatalog> {
    let mut dashboards = Vec::new();
    let mut truncated = false;
    let mut errors = Vec::new();
    match grafana::fetch_provisioned_from_configmaps(client).await {
        Fetched::Ok(provisioned) => {
            dashboards = provisioned.dashboards;
            truncated |= provisioned.truncated;
            errors.extend(
                provisioned
                    .errors
                    .into_iter()
                    .map(|why| format!("provisioned dashboards: {why}")),
            );
        }
        Fetched::Denied { what } => return Fetched::Denied { what },
        Fetched::Failed { why, .. } => errors.push(format!("provisioned dashboards: {why}")),
    }

    let reach = bind_observe_source(client, sources, ToolKind::Grafana).await;
    let mut extra_hits = Vec::new();
    let (browser_base, bound) = match &reach {
        ToolReach::Bound(bound) => (bound_browser_base(bound), true),
        ToolReach::Unbound(unbound) => (unbound.browser_url.clone(), false),
        ToolReach::Absent { .. } => (None, false),
    };

    let browser_service = match &reach {
        ToolReach::Bound(bound) => bound
            .found
            .as_ref()
            .map(|found| (found.namespace.clone(), found.name.clone())),
        _ => None,
    };
    if let ToolReach::Bound(bound) = reach {
        match grafana::fetch_search(client, &bound, &[]).await {
            Fetched::Ok(hits) => {
                let mut fetched = 0usize;
                for hit in hits {
                    if hit.uid.is_empty() {
                        continue;
                    }
                    if dashboards.iter().any(|dashboard| dashboard.uid == hit.uid) {
                        continue;
                    }
                    if fetched < grafana::MAX_NAMED_FROM_SEARCH {
                        fetched += 1;
                        match grafana::fetch_dashboard(client, &bound, &hit.uid).await {
                            Fetched::Ok(dashboard) => {
                                dashboards.push(dashboard);
                            }
                            Fetched::Denied { what } => return Fetched::Denied { what },
                            Fetched::Failed { why, .. } => {
                                errors.push(format!("Grafana dashboard {}: {why}", hit.uid))
                            }
                        }
                    } else {
                        extra_hits.push(hit);
                        truncated = true;
                    }
                }
            }
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { why, .. } => errors.push(format!("Grafana search: {why}")),
        }
    } else if let ToolReach::Unbound(unbound) = reach {
        errors.push(format!("Grafana binding: {}", unbound_why(&unbound)));
    }

    if !bound && dashboards.is_empty() && !errors.is_empty() {
        return Fetched::Failed {
            what: "grafana",
            why: errors.join("; "),
        };
    }

    Fetched::Ok(GrafanaCatalog {
        served: bound || !dashboards.is_empty(),
        dashboards,
        extra_hits,
        truncated,
        browser_base,
        browser_service,
        errors,
    })
}

fn bound_browser_base(bound: &Bound) -> Option<String> {
    match &bound.transport {
        reach::Transport::Url { base } => Some(base.clone()),
        _ => None,
    }
}

fn unbound_why(unbound: &reach::Unbound) -> String {
    let mut why = unbound.why.clone();
    if let Some(url) = &unbound.browser_url {
        why.push_str("; open ");
        why.push_str(url);
        why.push_str(" in the system browser");
    }
    why
}

fn seen_of(reach: &ToolReach) -> Seen {
    match reach {
        ToolReach::Bound(bound)
            if !matches!(bound.transport, reach::Transport::NeedsForward { .. }) =>
        {
            Seen::Bound
        }
        ToolReach::Bound(_) => Seen::Unbound,
        ToolReach::Unbound(_) => Seen::Unbound,
        ToolReach::Absent { .. } => Seen::Absent,
    }
}

async fn probe_observe_tools(client: &Client) -> ObserveTools {
    let settings = ReachSettings::default();
    let services = match reach::discover_services(client).await {
        Fetched::Ok(services) => services,
        Fetched::Denied { what } => {
            return ObserveTools::blocked(format!("{what}: access denied for this account"));
        }
        Fetched::Failed { why, .. } => return ObserveTools::blocked(why),
    };
    let prometheus = reach::bind_candidates(
        client,
        ToolKind::Prometheus,
        settings.for_kind(ToolKind::Prometheus),
        reach::service_candidates(ToolKind::Prometheus, &services),
    )
    .await;
    let loki = reach::bind_candidates(
        client,
        ToolKind::Loki,
        settings.for_kind(ToolKind::Loki),
        reach::service_candidates(ToolKind::Loki, &services),
    )
    .await;
    let tempo = reach::bind_candidates(
        client,
        ToolKind::Tempo,
        settings.for_kind(ToolKind::Tempo),
        reach::service_candidates(ToolKind::Tempo, &services),
    )
    .await;
    let trace = match tempo {
        ToolReach::Absent { .. } => {
            reach::bind_candidates(
                client,
                ToolKind::Jaeger,
                settings.for_kind(ToolKind::Jaeger),
                reach::service_candidates(ToolKind::Jaeger, &services),
            )
            .await
        }
        tempo => tempo,
    };
    observe_tools(prometheus, loki, trace)
}

fn observe_tools(prometheus: ToolReach, loki: ToolReach, trace: ToolReach) -> ObserveTools {
    ObserveTools {
        prometheus: seen_of(&prometheus),
        loki: seen_of(&loki),
        traces: seen_of(&trace),
        errors: [&prometheus, &loki, &trace]
            .into_iter()
            .filter_map(|reach| match reach {
                ToolReach::Unbound(unbound) => {
                    Some(format!("{}: {}", unbound.kind.as_str(), unbound.why))
                }
                ToolReach::Bound(bound)
                    if matches!(bound.transport, reach::Transport::NeedsForward { .. }) =>
                {
                    Some(format!(
                        "{} requires a service forward",
                        bound.kind.as_str()
                    ))
                }
                _ => None,
            })
            .collect(),
    }
}

pub(super) async fn fetch_alertmanager_optional(
    client: &Client,
) -> Fetched<Option<alertmanager::Alerts>> {
    match reach::bind(client, ToolKind::Alertmanager, &ReachSettings::default()).await {
        ToolReach::Absent { .. } => Fetched::Ok(None),
        ToolReach::Unbound(unbound) => Fetched::Failed {
            what: "alertmanager",
            why: unbound_why(&unbound),
        },
        ToolReach::Bound(bound) => match alertmanager::fetch_alerts(client, &bound).await {
            Fetched::Ok(alerts) => Fetched::Ok(Some(alerts)),
            Fetched::Denied { what } => Fetched::Denied { what },
            Fetched::Failed { what, why } => Fetched::Failed { what, why },
        },
    }
}

async fn query_prometheus_optional(
    client: &Client,
    sources: Option<&Sources>,
    expr: &str,
    start: f64,
    end: f64,
    step: &str,
) -> Fetched<Option<prom::QueryResult>> {
    match bind_observe_source(client, sources, ToolKind::Prometheus).await {
        ToolReach::Absent { .. } => Fetched::Ok(None),
        ToolReach::Unbound(unbound) => Fetched::Failed {
            what: "prometheus",
            why: unbound_why(&unbound),
        },
        ToolReach::Bound(bound) => {
            match prom::query_range(client, &bound, expr, start, end, step).await {
                Fetched::Ok(result) => Fetched::Ok(Some(result)),
                Fetched::Denied { what } => Fetched::Denied { what },
                Fetched::Failed { what, why } => Fetched::Failed { what, why },
            }
        }
    }
}

async fn query_loki_optional(
    client: &Client,
    sources: Option<&Sources>,
    query: &loki::RangeQuery,
) -> Fetched<Option<loki::Logs>> {
    match bind_observe_source(client, sources, ToolKind::Loki).await {
        ToolReach::Absent { .. } => Fetched::Ok(None),
        ToolReach::Unbound(unbound) => Fetched::Failed {
            what: "loki",
            why: unbound_why(&unbound),
        },
        ToolReach::Bound(bound) => match loki::query_range(client, &bound, query).await {
            Fetched::Ok(logs) => Fetched::Ok(Some(logs)),
            Fetched::Denied { what } => Fetched::Denied { what },
            Fetched::Failed { what, why } => Fetched::Failed { what, why },
        },
    }
}

async fn lookup_trace_optional(
    client: &Client,
    sources: Option<&Sources>,
    trace_id: &str,
) -> Fetched<Option<traces::Trace>> {
    let bound = match bind_observe_source(client, sources, ToolKind::Tempo).await {
        ToolReach::Bound(bound) => bound,
        ToolReach::Unbound(unbound) => {
            return Fetched::Failed {
                what: "tempo",
                why: unbound_why(&unbound),
            };
        }
        ToolReach::Absent { .. } => {
            match bind_observe_source(client, sources, ToolKind::Jaeger).await {
                ToolReach::Bound(bound) => bound,
                ToolReach::Unbound(unbound) => {
                    return Fetched::Failed {
                        what: "jaeger",
                        why: unbound_why(&unbound),
                    };
                }
                ToolReach::Absent { .. } => return Fetched::Ok(None),
            }
        }
    };
    match traces::lookup(client, &bound, trace_id).await {
        Fetched::Ok(mut trace) => {
            trace.source = format!("{} / {}", bound.kind.as_str(), bound.source());
            Fetched::Ok(Some(trace))
        }
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

async fn bind_observe_source(
    client: &Client,
    sources: Option<&Sources>,
    kind: ToolKind,
) -> ToolReach {
    match sources {
        Some(sources) => sources.bind(client, kind).await,
        None => reach::bind(client, kind, &ReachSettings::default()).await,
    }
}

fn unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

fn optional<T>(fetched: Fetched<T>) -> Fetched<Option<T>> {
    match fetched {
        Fetched::Ok(value) => Fetched::Ok(Some(value)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}
