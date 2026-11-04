//! Bounded observability queries and confirmed silence requests.

use super::{Fetched, Reader};
use crate::integrations::{alertmanager, grafana, loki, prom, traces};
use crate::reach::{Bound, ReachSettings, ToolKind, ToolReach};
use crate::{overlay, reach};
use k10s_core::ServiceIdentity;
use kube::Client;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

mod sources;
pub(super) use sources::Sources;

/// Dashboards we can run as queries, plus a system-browser base for panels
/// this process will not execute.
#[derive(Clone, PartialEq, Eq)]
pub struct GrafanaCatalog {
    pub dashboards: Vec<grafana::Dashboard>,
    /// Search results without a readable dashboard definition.
    pub extra_hits: Vec<grafana::CatalogHit>,
    pub truncated: bool,
    pub browser_base: Option<String>,
    /// Namespace, Service name and discovered Service port for a browser forward.
    pub browser_service: Option<(String, String, u16)>,
    /// The exact Grafana Service binding whose datasource UIDs these rows use.
    pub query_source: Option<Arc<ServiceIdentity>>,
    pub served: bool,
    pub errors: Vec<String>,
}

impl std::fmt::Debug for GrafanaCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrafanaCatalog")
            .field("dashboards", &self.dashboards)
            .field("extra_hits", &self.extra_hits)
            .field("truncated", &self.truncated)
            .field(
                "browser_base",
                &self.browser_base.as_ref().map(|_| "[redacted]"),
            )
            .field("browser_service", &self.browser_service)
            .field("query_source", &self.query_source)
            .field("served", &self.served)
            .field("errors", &self.errors)
            .finish()
    }
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

    pub fn observe_loki_probe(
        &self,
        reply: impl FnOnce(Fetched<Seen>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let sources = self.observe_sources.clone();
        self.telemetry.submit(
            &self.handle,
            async move {
                let reach = sources.bind(&client, ToolKind::Loki).await;
                match reach {
                    ToolReach::Unbound(unbound) => Fetched::Failed {
                        what: "Loki",
                        why: unbound.why,
                    },
                    ToolReach::Bound(bound)
                        if matches!(bound.transport, reach::Transport::NeedsForward { .. }) =>
                    {
                        Fetched::Failed {
                            what: "Loki",
                            why: "Loki requires a service forward".into(),
                        }
                    }
                    reach => Fetched::Ok(seen_of(&reach)),
                }
            },
            reply,
        )
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
                query_prometheus_optional(&client, Some(&sources), &expr, start, end, &step).await
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
            async move { query_loki_optional(&client, Some(&sources), &query).await },
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
            async move { lookup_trace_optional(&client, Some(&sources), &trace_id).await },
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
            async move { load_grafana_catalog(&client, Some(&sources)).await },
            reply,
        )
    }

    pub fn observe_panel(
        &self,
        source: Option<Arc<ServiceIdentity>>,
        target: grafana::PanelQuery,
        variables: grafana::Variables,
        reply: impl FnOnce(Fetched<Option<prom::QueryResult>>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let Some(source) = source else {
            reply(Fetched::Failed {
                what: "grafana panel",
                why: "this catalogue has no verified Grafana Service identity; refresh the catalogue or use a raw query".to_string(),
            });
            return crate::telemetry::RequestStop::noop();
        };
        let client = self.interactive.clone();
        let sources = self.observe_sources.clone();
        self.telemetry.submit(
            &self.handle,
            async move {
                let reach = match sources.bind_panel(&client, &source).await {
                    Ok(reach) => reach,
                    Err(why) => {
                        return Fetched::Failed {
                            what: "grafana panel",
                            why,
                        };
                    }
                };
                match reach {
                    ToolReach::Absent { .. } => Fetched::Failed {
                        what: "grafana panel",
                        why: "the selected Grafana Service is not served by this cluster; refresh the catalogue".to_string(),
                    },
                    ToolReach::Unbound(unbound) => Fetched::Failed {
                        what: "grafana",
                        why: unbound.why,
                    },
                    ToolReach::Bound(bound) => {
                        optional(grafana::query_target(&client, &bound, &target, &variables).await)
                    }
                }
            },
            reply,
        )
    }

    /// Compare a browser route with the last verified source without waiting or fetching.
    pub fn grafana_source_is_current(&self, source: &ServiceIdentity) -> bool {
        self.observe_sources.grafana_is_current(source)
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
    let mut denied = None;
    let mut source_succeeded = false;
    let mut dashboard_read_failed = false;
    match grafana::fetch_provisioned_from_configmaps(client).await {
        Fetched::Ok(provisioned) => {
            source_succeeded = true;
            dashboards = provisioned.dashboards;
            truncated |= provisioned.truncated;
            errors.extend(
                provisioned
                    .errors
                    .into_iter()
                    .map(|why| format!("provisioned dashboards: {why}")),
            );
        }
        Fetched::Denied { what } => {
            denied = Some(what);
            errors.push(format!(
                "provisioned dashboards: {what}: access denied for this account"
            ));
        }
        Fetched::Failed { why, .. } => errors.push(format!("provisioned dashboards: {why}")),
    }

    let uncached = Sources::default();
    let (reach, query_source) = sources.unwrap_or(&uncached).bind_catalog(client).await;
    let mut extra_hits: Vec<grafana::CatalogHit> = Vec::new();
    let (browser_base, bound) = match &reach {
        ToolReach::Bound(bound) => (bound_browser_base(bound), true),
        // Automatic Service hints must not bypass the pinned browser-forward route.
        ToolReach::Unbound(_) => (None, false),
        ToolReach::Absent { .. } => (None, false),
    };

    let browser_service = match &reach {
        ToolReach::Bound(bound) => bound
            .found
            .as_ref()
            .map(|found| (found.namespace.clone(), found.name.clone(), found.port)),
        _ => None,
    };
    if let ToolReach::Bound(bound) = reach {
        match grafana::fetch_search(client, &bound, &[]).await {
            Fetched::Ok(page) => {
                source_succeeded = true;
                denied = None;
                if page.page_full {
                    errors.insert(0, grafana::SEARCH_PAGE_FULL_NOTICE.to_string());
                }
                let mut attempted = Vec::new();
                let mut invalid_uid = false;
                for hit in page.hits {
                    if !grafana::valid_dashboard_uid(&hit.uid) {
                        if !invalid_uid {
                            errors.push(grafana::INVALID_DASHBOARD_UID.to_string());
                            invalid_uid = true;
                        }
                        continue;
                    }
                    if attempted.iter().any(|uid| uid == &hit.uid)
                        || extra_hits.iter().any(|seen| seen.search.uid == hit.uid)
                    {
                        continue;
                    }
                    if attempted.len() < grafana::MAX_NAMED_FROM_SEARCH {
                        attempted.push(hit.uid.clone());
                        match grafana::fetch_dashboard(client, &bound, &hit.uid).await {
                            Fetched::Ok(dashboard) => {
                                dashboards.retain(|file| {
                                    file.source != grafana::DashboardSource::ConfigMap
                                        || file.uid != hit.uid
                                });
                                dashboards.push(dashboard);
                            }
                            Fetched::Denied { .. } => {
                                dashboard_read_failed = true;
                                errors.push(format!(
                                    "Grafana dashboard {}: access denied for this account",
                                    hit.uid
                                ));
                                extra_hits.push(grafana::CatalogHit {
                                    search: hit,
                                    reason: grafana::CatalogHitReason::Denied,
                                });
                            }
                            Fetched::Failed { why, .. } => {
                                dashboard_read_failed = true;
                                errors.push(format!("Grafana dashboard {}: {why}", hit.uid));
                                extra_hits.push(grafana::CatalogHit {
                                    search: hit,
                                    reason: grafana::CatalogHitReason::Failed,
                                });
                            }
                        }
                    } else {
                        extra_hits.push(grafana::CatalogHit {
                            search: hit,
                            reason: grafana::CatalogHitReason::FetchLimit,
                        });
                        truncated = true;
                    }
                }
            }
            Fetched::Denied { .. } => {
                if denied.is_some() {
                    return Fetched::Denied {
                        what: "provisioned dashboard configmaps and Grafana search",
                    };
                }
                denied = Some("Grafana search");
                errors.push("Grafana search: access denied for this account".to_string());
            }
            Fetched::Failed { why, .. } => errors.push(format!("Grafana search: {why}")),
        }
    } else if let ToolReach::Unbound(unbound) = reach {
        errors.push(format!("Grafana binding: {}", unbound_why(&unbound)));
    }

    if dashboard_read_failed
        && dashboards.is_empty()
        && truncated
        && !extra_hits
            .iter()
            .any(|hit| hit.reason == grafana::CatalogHitReason::FetchLimit)
    {
        errors.insert(
            0,
            "Provisioned dashboard selection is incomplete".to_string(),
        );
    }
    if !source_succeeded && !errors.is_empty() {
        if errors.len() == 1
            && let Some(what) = denied
        {
            return Fetched::Denied { what };
        }
        return Fetched::Failed {
            what: "grafana",
            why: errors.join("; "),
        };
    }
    if dashboards.is_empty()
        && extra_hits.is_empty()
        && errors.len() == 1
        && let Some(what) = denied
    {
        return Fetched::Denied { what };
    }
    if !bound && dashboards.is_empty() && !errors.is_empty() {
        return Fetched::Failed {
            what: "grafana",
            why: errors.join("; "),
        };
    }

    for dashboard in &dashboards {
        if dashboard.truncated {
            truncated = true;
            let name = if !dashboard.uid.is_empty() {
                dashboard.uid.as_str()
            } else if !dashboard.title.is_empty() {
                dashboard.title.as_str()
            } else {
                "(unnamed)"
            };
            let source = match dashboard.source {
                grafana::DashboardSource::Grafana => "Grafana dashboard",
                grafana::DashboardSource::ConfigMap => "ConfigMap dashboard",
                grafana::DashboardSource::Unknown => "Dashboard",
            };
            errors.push(format!(
                "{source} {name}: panel or target records were not fully inspected after local limits"
            ));
        }
    }
    Fetched::Ok(GrafanaCatalog {
        served: bound || !dashboards.is_empty(),
        dashboards,
        extra_hits,
        truncated,
        browser_base,
        browser_service,
        query_source,
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
    if unbound.browser_url.is_some() {
        why.push_str("; open the tool in the system browser");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbound_diagnostics_do_not_include_the_browser_location() {
        let url = "https://example.test/private-path?token=private-query#private-fragment";
        let unbound = reach::Unbound {
            kind: ToolKind::Grafana,
            found: None,
            why: "Grafana needs a browser".into(),
            browser_url: Some(url.into()),
        };
        let diagnostic = unbound_why(&unbound);
        assert_eq!(
            diagnostic,
            "Grafana needs a browser; open the tool in the system browser"
        );
        for secret in ["private-path", "private-query", "private-fragment"] {
            assert!(!diagnostic.contains(secret));
        }
        assert_eq!(unbound.browser_url.as_deref(), Some(url));
    }
}
