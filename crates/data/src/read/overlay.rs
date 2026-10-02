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

    /// Overlay inventories, assembled off the paint path. Binding a tool never
    /// delays the scene's first publish.
    pub fn fetch_overlay(
        &self,
        kind: overlay::Kind,
        settings: ReachSettings,
        reply: impl FnOnce(Fetched<overlay::Frame>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let netpol = self.netpol.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(load_overlay(&client, &targets, &netpol, kind, settings).await);
        });
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
    let named = grafana_named_promql(client, &settings).await;
    let expr = named.as_deref().unwrap_or(overlay::CPU_EXPR);
    match query_prometheus(client, &settings, expr).await {
        Fetched::Ok(result) => {
            let mut frame = overlay::from_prometheus(&result);
            if frame.note.is_none() {
                frame.note = Some(if named.is_some() {
                    "PromQL named by Grafana".to_string()
                } else {
                    "cadvisor CPU; Grafana has not named a PromQL".to_string()
                });
            }
            Fetched::Ok(frame)
        }
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { why, .. } => Fetched::Ok(overlay::Frame {
            stamps: Vec::new(),
            truncated: false,
            note: Some(why),
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
    let mut denied = None;
    let mut failed = None;
    let mut ok = false;
    for (fetched, fallback) in [
        (istio, "istio_requests_total"),
        (hubble, "hubble_flows_processed_total"),
        (linkerd, "response_total"),
    ] {
        match fetched {
            Fetched::Ok(result) => {
                ok = true;
                labels.extend(mesh_series(&result, fallback));
            }
            Fetched::Denied { what } => denied = Some(what),
            Fetched::Failed { why, .. } => failed = Some(why),
        }
    }
    if !ok {
        if let Some(what) = denied {
            return Fetched::Denied { what };
        }
        return Fetched::Ok(overlay::Frame {
            stamps: Vec::new(),
            truncated: false,
            note: failed,
        });
    }
    Fetched::Ok(overlay::from_mesh_observed(&mesh::observed_from_series(
        &labels,
    )))
}

/// Provisioned ConfigMaps first: they are already on the API server, so the
/// overlay does not wait on a Grafana bind. Live search is the fallback when
/// those dashboards name nothing joinable.
async fn grafana_named_promql(client: &Client, settings: &ReachSettings) -> Option<String> {
    if let Fetched::Ok(provisioned) = grafana::fetch_provisioned_from_configmaps(client).await
        && let Some(expr) = grafana::name_overlay_promql(&provisioned.dashboards)
    {
        return Some(expr.to_string());
    }

    let ToolReach::Bound(bound) = reach::bind(client, ToolKind::Grafana, settings).await else {
        return None;
    };
    let Fetched::Ok(hits) = grafana::fetch_search(client, &bound, &[]).await else {
        return None;
    };
    let mut dashboards = Vec::new();
    for hit in hits.into_iter().take(grafana::MAX_NAMED_FROM_SEARCH) {
        if hit.uid.is_empty() {
            continue;
        }
        let Fetched::Ok(dash) = grafana::fetch_dashboard(client, &bound, &hit.uid).await else {
            continue;
        };
        dashboards.push(dash);
        if let Some(expr) = grafana::name_overlay_promql(&dashboards) {
            return Some(expr.to_string());
        }
    }
    None
}

fn mesh_series(result: &prom::QueryResult, fallback: &str) -> Vec<mesh::SeriesLabels> {
    result
        .series
        .iter()
        .map(|series| mesh::SeriesLabels {
            name: series
                .labels
                .iter()
                .find(|(key, _)| key == "__name__")
                .map(|(_, value)| value.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| fallback.to_string()),
            labels: series.labels.clone(),
        })
        .collect()
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
