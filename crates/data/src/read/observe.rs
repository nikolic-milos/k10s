//! Bounded observability queries and confirmed silence requests.

use super::{Fetched, Reader};
use crate::integrations::{alertmanager, grafana, loki, prom, traces};
use crate::reach::{Bound, ReachSettings, ToolKind, ToolReach};
use crate::{overlay, reach};
use kube::Client;
use std::time::{SystemTime, UNIX_EPOCH};

/// Dashboards we can run as queries, plus a system-browser base for panels
/// this process will not execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrafanaCatalog {
    pub dashboards: Vec<grafana::Dashboard>,
    pub extra_hits: Vec<grafana::SearchHit>,
    pub truncated: bool,
    pub browser_base: Option<String>,
    pub served: bool,
}

/// Whether an in-cluster tool answered a bind. [`Seen::Absent`] hides a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Bound,
    Unbound,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObserveTools {
    pub prometheus: Seen,
    pub loki: Seen,
    pub traces: Seen,
}

impl Reader {
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
            reply(load_grafana_catalog(&client).await);
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
            reply(query_prometheus_optional(&client, &expr, start, end, &step).await);
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
            reply(query_loki_optional(&client, &query).await);
        });
    }

    /// Trace id lookup against Tempo, then Jaeger. [`Fetched::Ok`]`None` is
    /// absence of both stores.
    pub fn lookup_trace(
        &self,
        trace_id: String,
        reply: impl FnOnce(Fetched<Option<traces::Trace>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(lookup_trace_optional(&client, &trace_id).await);
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

async fn load_grafana_catalog(client: &Client) -> Fetched<GrafanaCatalog> {
    let mut dashboards = Vec::new();
    let mut truncated = false;
    let mut provisioned_failure: Option<String> = None;
    match grafana::fetch_provisioned_from_configmaps(client).await {
        Fetched::Ok(provisioned) => {
            dashboards = provisioned.dashboards;
            truncated |= provisioned.truncated;
        }
        Fetched::Denied { what } => return Fetched::Denied { what },
        // Best effort: live Grafana can still fill the catalog. The failure
        // is kept so an otherwise empty answer reports it instead of
        // claiming Grafana is absent.
        Fetched::Failed { why, .. } => provisioned_failure = Some(why),
    }

    let reach = reach::bind(client, ToolKind::Grafana, &ReachSettings::default()).await;
    let mut extra_hits = Vec::new();
    let (browser_base, bound) = match &reach {
        ToolReach::Bound(bound) => (bound_browser_base(bound), true),
        ToolReach::Unbound(unbound) => (unbound.browser_url.clone(), false),
        ToolReach::Absent { .. } => (None, false),
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
                        match grafana::fetch_dashboard(client, &bound, &hit.uid).await {
                            Fetched::Ok(dashboard) => {
                                dashboards.push(dashboard);
                                fetched += 1;
                            }
                            Fetched::Denied { what } => return Fetched::Denied { what },
                            Fetched::Failed { what, why } => return Fetched::Failed { what, why },
                        }
                    } else {
                        extra_hits.push(hit);
                        truncated = true;
                    }
                }
            }
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { what, why } => return Fetched::Failed { what, why },
        }
    } else if let ToolReach::Unbound(unbound) = reach {
        if dashboards.is_empty() {
            return Fetched::Failed {
                what: "grafana",
                why: unbound_why(&unbound),
            };
        }
    }

    if !bound
        && dashboards.is_empty()
        && let Some(why) = provisioned_failure
    {
        return Fetched::Failed {
            what: "grafana",
            why,
        };
    }

    Fetched::Ok(GrafanaCatalog {
        served: bound || !dashboards.is_empty(),
        dashboards,
        extra_hits,
        truncated,
        browser_base,
    })
}

fn bound_browser_base(bound: &Bound) -> Option<String> {
    match &bound.transport {
        reach::Transport::Url { base } => Some(base.clone()),
        _ => bound.found.as_ref().map(|found| {
            format!(
                "http://{}.{}.svc:{}",
                found.name, found.namespace, found.port
            )
        }),
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
        ToolReach::Bound(_) => Seen::Bound,
        ToolReach::Unbound(_) => Seen::Unbound,
        ToolReach::Absent { .. } => Seen::Absent,
    }
}

async fn probe_observe_tools(client: &Client) -> ObserveTools {
    let settings = ReachSettings::default();
    let (prometheus, loki, tempo, jaeger) = tokio::join!(
        reach::bind(client, ToolKind::Prometheus, &settings),
        reach::bind(client, ToolKind::Loki, &settings),
        reach::bind(client, ToolKind::Tempo, &settings),
        reach::bind(client, ToolKind::Jaeger, &settings),
    );
    let traces = match (seen_of(&tempo), seen_of(&jaeger)) {
        (Seen::Bound, _) | (_, Seen::Bound) => Seen::Bound,
        (Seen::Unbound, _) | (_, Seen::Unbound) => Seen::Unbound,
        (Seen::Absent, Seen::Absent) => Seen::Absent,
    };
    ObserveTools {
        prometheus: seen_of(&prometheus),
        loki: seen_of(&loki),
        traces,
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
    expr: &str,
    start: f64,
    end: f64,
    step: &str,
) -> Fetched<Option<prom::QueryResult>> {
    match reach::bind(client, ToolKind::Prometheus, &ReachSettings::default()).await {
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
    query: &loki::RangeQuery,
) -> Fetched<Option<loki::Logs>> {
    match reach::bind(client, ToolKind::Loki, &ReachSettings::default()).await {
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

async fn lookup_trace_optional(client: &Client, trace_id: &str) -> Fetched<Option<traces::Trace>> {
    let settings = ReachSettings::default();
    // A Jaeger bind costs its own cluster-wide Service scan plus a probe, so
    // it only runs when Tempo did not already answer.
    let bound = match reach::bind(client, ToolKind::Tempo, &settings).await {
        ToolReach::Bound(bound) => bound,
        tempo => match (
            tempo,
            reach::bind(client, ToolKind::Jaeger, &settings).await,
        ) {
            (_, ToolReach::Bound(bound)) => bound,
            (ToolReach::Unbound(unbound), _) => {
                return Fetched::Failed {
                    what: "tempo",
                    why: unbound_why(&unbound),
                };
            }
            (_, ToolReach::Unbound(unbound)) => {
                return Fetched::Failed {
                    what: "jaeger",
                    why: unbound_why(&unbound),
                };
            }
            (_, ToolReach::Absent { .. }) => return Fetched::Ok(None),
        },
    };
    match traces::lookup(client, &bound, trace_id).await {
        Fetched::Ok(trace) => Fetched::Ok(Some(trace)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

fn unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}
