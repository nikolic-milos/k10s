//! Off-thread overlay assembly and network policy inspection.

use super::observe::{bind_prometheus, query_bound, query_prometheus};
use super::{Fetched, Reader};
use crate::discover::KindTarget;
use crate::integrations::{argo, flux, grafana, mesh, policy, prom};
use crate::reach::{ReachSettings, ToolKind, ToolReach};
use crate::{netpol, overlay, reach};
use kube::Client;
use std::sync::Mutex;
use std::time::{Duration, Instant};

impl Reader {
    /// Fetch one bounded cluster-wide input for repeated policy inspection.
    pub fn fetch_network_policy_inventory(
        &self,
        reply: impl FnOnce(Fetched<netpol::Inventory>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(netpol::fetch(&client).await);
        });
    }

    /// Overlay inventories share bounded telemetry admission off the paint path.
    /// One deadline covers the queue and the complete inventory. Dropping the
    /// guard cancels queued or in-flight work.
    pub fn fetch_overlay(
        &self,
        kind: overlay::Kind,
        settings: ReachSettings,
        reply: impl FnOnce(Fetched<overlay::Frame>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let netpol = self.netpol.clone();
        let targets = self.targets.clone();
        let deadline = tokio::time::Instant::now() + crate::connect::INTERACTIVE_DEADLINE;
        self.telemetry.submit_until(
            &self.handle,
            deadline,
            async move { load_overlay(&client, &targets, &netpol, kind, settings).await },
            reply,
        )
    }

    /// Isolation and named ports for one pod. Not a traffic verdict.
    pub fn fetch_pod_posture(
        &self,
        namespace: String,
        name: String,
        reply: impl FnOnce(Fetched<netpol::PodInspection>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let netpol = self.netpol.clone();
        self.handle.spawn(async move {
            reply(load_pod_inspection(&client, &netpol, &namespace, &name).await);
        });
    }
}
const NETPOL_TTL: Duration = Duration::from_secs(30);

async fn load_overlay(
    client: &Client,
    targets: &[KindTarget],
    cache: &Mutex<Option<(Instant, netpol::Inventory)>>,
    kind: overlay::Kind,
    settings: ReachSettings,
) -> Fetched<overlay::Frame> {
    match kind {
        overlay::Kind::Sync => load_sync(client, targets).await,
        overlay::Kind::Metrics => load_metrics(client, settings).await,
        overlay::Kind::Policy => load_policy(client).await,
        overlay::Kind::Netpol => load_netpol(client, cache).await,
        overlay::Kind::MeshDeclared => {
            Fetched::Ok(overlay::from_mesh_declared(&mesh::inventory(client).await))
        }
        overlay::Kind::MeshObserved => load_mesh_observed(client, settings).await,
    }
}

async fn load_sync(client: &Client, targets: &[KindTarget]) -> Fetched<overlay::Frame> {
    match argo::fetch_inventory(client, targets, None).await {
        Fetched::Ok(inventory) if inventory.served => Fetched::Ok(overlay::from_argo(&inventory)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
        Fetched::Ok(_) => match flux::fetch(client, None).await {
            Fetched::Ok(inventory) => Fetched::Ok(overlay::from_flux(&inventory)),
            Fetched::Denied { what } => Fetched::Denied { what },
            Fetched::Failed { what, why } => Fetched::Failed { what, why },
        },
    }
}

async fn load_policy(client: &Client) -> Fetched<overlay::Frame> {
    match policy::fetch_reports(client).await {
        Fetched::Ok(inventory) => Fetched::Ok(overlay::from_policy_reports(&inventory)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

async fn load_netpol(
    client: &Client,
    cache: &Mutex<Option<(Instant, netpol::Inventory)>>,
) -> Fetched<overlay::Frame> {
    match cached_netpol(client, cache).await {
        Fetched::Ok(inventory) => Fetched::Ok(overlay::from_netpol(&inventory)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

async fn load_metrics(client: &Client, settings: ReachSettings) -> Fetched<overlay::Frame> {
    let (named, notices) = grafana_named_promql(client, &settings).await;
    let expr = named.as_deref().unwrap_or(overlay::CPU_EXPR);
    match query_prometheus(client, &settings, expr).await {
        Fetched::Ok(result) => {
            let mut frame = overlay::from_prometheus(&result);
            let origin = if named.is_some() {
                "PromQL named by Grafana"
            } else {
                "cadvisor CPU; Grafana has not named a PromQL"
            };
            if let Some(note) = frame.note.as_mut() {
                note.push_str("; ");
                note.push_str(origin);
            } else {
                frame.note = Some(origin.to_string());
            }
            frame.note = metrics_note(frame.note, notices);
            Fetched::Ok(frame)
        }
        Fetched::Denied { what } if notices.text.is_empty() => Fetched::Denied { what },
        Fetched::Denied { what } => Fetched::Failed {
            what,
            why: metrics_note(
                Some(format!("{what}: access denied for this account")),
                notices,
            )
            .expect("the denial supplies a note"),
        },
        Fetched::Failed { why, .. } => Fetched::Ok(overlay::Frame {
            stamps: Vec::new(),
            truncated: false,
            note: metrics_note(Some(why), notices),
        }),
    }
}

async fn load_mesh_observed(client: &Client, settings: ReachSettings) -> Fetched<overlay::Frame> {
    let bound = match bind_prometheus(client, &settings).await {
        Fetched::Ok(bound) => bound,
        Fetched::Denied { what } => return Fetched::Denied { what },
        Fetched::Failed { why, .. } => {
            return Fetched::Ok(overlay::Frame {
                stamps: Vec::new(),
                truncated: false,
                note: Some(why),
            });
        }
    };

    let (istio, hubble, linkerd) = tokio::join!(
        query_bound(client, &bound, overlay::MESH_EXPR),
        query_bound(client, &bound, overlay::HUBBLE_EXPR),
        query_bound(client, &bound, overlay::LINKERD_EXPR),
    );

    let mut labels = Vec::new();
    let mut notices = MetricSourceNotes::default();
    let mut truncated = false;
    let mut failures = Vec::new();
    let mut available = false;
    for (fetched, source, fallback) in [
        (istio, "Istio query", "istio_requests_total"),
        (hubble, "Hubble query", "hubble_flows_processed_total"),
        (linkerd, "Linkerd query", "response_total"),
    ] {
        match fetched {
            Fetched::Ok(result) => {
                available = true;
                truncated |= result.truncated;
                overlay::append_prometheus_notices(&result, source, &mut notices.text);
                labels.extend(mesh_series(result.series, fallback));
            }
            Fetched::Denied { what } => {
                failures.push(format!("{source}: {what}: access denied for this account"));
            }
            Fetched::Failed { what, why } => {
                failures.push(format!("{source}: {what}: {why}"));
            }
        }
    }
    let mut frame = overlay::from_mesh_observed(&mesh::observed_from_series(&labels));
    if !failures.is_empty() {
        let state = if available {
            "incomplete"
        } else {
            "unavailable"
        };
        frame.note = Some(format!(
            "Mesh observations are {state}; {}",
            failures.join("; ")
        ));
    }
    frame.truncated |= truncated;
    frame.note = metrics_note(frame.note, notices);
    Fetched::Ok(frame)
}

/// Provisioned ConfigMaps first: they are already on the API server, so the
/// overlay does not wait on a Grafana bind. Live search is the fallback when
/// those dashboards name nothing joinable.
async fn grafana_named_promql(
    client: &Client,
    settings: &ReachSettings,
) -> (Option<String>, MetricSourceNotes) {
    let mut notices = MetricSourceNotes::default();
    match grafana::fetch_provisioned_from_configmaps(client).await {
        Fetched::Ok(provisioned) => {
            if provisioned.truncated {
                notices.push(&["Provisioned ConfigMap dashboard selection is incomplete"]);
            }
            for why in &provisioned.errors {
                notices.push(&["provisioned dashboards: ", why]);
            }
            if let Some(expr) = grafana::name_overlay_promql(&provisioned.dashboards) {
                return (Some(expr.to_string()), notices);
            }
        }
        Fetched::Denied { what } => notices.push(&[
            "provisioned dashboards: ",
            what,
            ": access denied for this account",
        ]),
        Fetched::Failed { why, .. } => {
            notices.push(&["provisioned dashboards: ", &why]);
        }
    }

    let bound = match reach::bind(client, ToolKind::Grafana, settings).await {
        ToolReach::Bound(bound) => bound,
        ToolReach::Absent { .. } => return (None, notices),
        ToolReach::Unbound(unbound) => {
            notices.push(&["Grafana binding: ", &unbound.why]);
            return (None, notices);
        }
    };
    let mut page = match grafana::fetch_search(client, &bound, &[]).await {
        Fetched::Ok(page) => page,
        Fetched::Denied { what } => {
            notices.push(&["Grafana search: ", what, ": access denied for this account"]);
            return (None, notices);
        }
        Fetched::Failed { why, .. } => {
            notices.push(&["Grafana search: ", &why]);
            return (None, notices);
        }
    };
    let mut invalid_uid = false;
    page.hits.retain(|hit| {
        let valid = grafana::valid_dashboard_uid(&hit.uid);
        invalid_uid |= !valid;
        valid
    });
    if invalid_uid {
        notices.push(&[grafana::INVALID_DASHBOARD_UID]);
    }
    let more_hits = page.hits.len() > grafana::MAX_NAMED_FROM_SEARCH;
    for hit in page.hits.into_iter().take(grafana::MAX_NAMED_FROM_SEARCH) {
        let dashboard = match grafana::fetch_dashboard(client, &bound, &hit.uid).await {
            Fetched::Ok(dash) => dash,
            Fetched::Denied { what } => {
                notices.push(&[
                    "Grafana dashboard ",
                    &hit.uid,
                    ": ",
                    what,
                    ": access denied for this account",
                ]);
                continue;
            }
            Fetched::Failed { why, .. } => {
                notices.push(&["Grafana dashboard ", &hit.uid, ": ", &why]);
                continue;
            }
        };
        if let Some(expr) = grafana::name_overlay_promql(std::slice::from_ref(&dashboard)) {
            return (Some(expr.to_string()), notices);
        }
    }
    if more_hits {
        notices.prioritize("Grafana dashboard search-hit selection is incomplete");
    }
    if page.page_full {
        notices.prioritize(grafana::SEARCH_PAGE_FULL_NOTICE);
    }
    (None, notices)
}

#[derive(Default)]
struct MetricSourceNotes {
    // The adapters cap dashboard keys, fetched dashboards and response bodies.
    text: String,
}

impl MetricSourceNotes {
    fn push(&mut self, parts: &[&str]) {
        if !self.text.is_empty() {
            self.text.push_str("; ");
        }
        for part in parts {
            self.text.push_str(part);
        }
    }

    fn prioritize(&mut self, notice: &str) {
        if !self.text.is_empty() {
            self.text.insert_str(0, "; ");
        }
        self.text.insert_str(0, notice);
    }
}

fn metrics_note(note: Option<String>, notices: MetricSourceNotes) -> Option<String> {
    if notices.text.is_empty() {
        return note;
    }
    let Some(mut note) = note else {
        return Some(notices.text);
    };
    if !note.is_empty() {
        note.push_str("; ");
    }
    note.push_str(&notices.text);
    Some(note)
}

fn mesh_series(
    series: Vec<prom::Series>,
    fallback: &str,
) -> impl Iterator<Item = mesh::SeriesLabels> + '_ {
    series.into_iter().map(move |series| mesh::SeriesLabels {
        name: series
            .labels
            .iter()
            .find(|(key, _)| key == "__name__")
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_string()),
        labels: series.labels,
    })
}

async fn load_pod_inspection(
    client: &Client,
    cache: &Mutex<Option<(Instant, netpol::Inventory)>>,
    namespace: &str,
    name: &str,
) -> Fetched<netpol::PodInspection> {
    match cached_netpol(client, cache).await {
        Fetched::Ok(inventory) => Fetched::Ok(netpol::PodInspection::from_inventory(
            &inventory, namespace, name,
        )),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

async fn cached_netpol(
    client: &Client,
    cache: &Mutex<Option<(Instant, netpol::Inventory)>>,
) -> Fetched<netpol::Inventory> {
    if let Ok(guard) = cache.lock()
        && let Some((at, inventory)) = guard.as_ref()
        && at.elapsed() < NETPOL_TTL
    {
        return Fetched::Ok(inventory.clone());
    }
    let fetched = netpol::fetch(client).await;
    if let Fetched::Ok(inventory) = &fetched
        && let Ok(mut guard) = cache.lock()
    {
        *guard = Some((Instant::now(), inventory.clone()));
    }
    fetched
}
