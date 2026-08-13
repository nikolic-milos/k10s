//! The read seam: one handle the UI pulls detail through.
//!
//! A [`Reader`] carries the client, the runtime handle, what discovery found,
//! and the probe's capability verdicts. Every method is fire-and-forget onto
//! the data plane's runtime with the answer handed to a caller-supplied
//! callback -- the render thread never blocks on the cluster -- and every
//! outcome is a labelled [`Fetched`]: an account the cluster refuses -- 401 or
//! 403, the same pair a watch calls forbidden -- arrives as `Denied`, never as
//! an empty panel or an error string a person has to diagnose. Errors that
//! reach text pass through the same redaction filter as everything else in
//! this crate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kube::Client;

use k10s_core::{Capability, KindId};

use crate::apply::{self, ApplyOutcome, ApplyRequest};
use crate::argo;
use crate::browse::{self, TablePage};
use crate::describe::{self, DescribeRequest, Described};
use crate::discover::KindTarget;
use crate::exec::{ExecEvent, ExecRequest, ExecSession, ExecTransport, KubeExecTransport};
use crate::flux;
use crate::forward::{self, ForwardRegistry, ForwardRequest, ForwardRow, KubeForwarder};
use crate::helm;
use crate::logs::{self, LogChunk, LogRequest, LogStop};
use crate::manifest;
use crate::mesh;
use crate::metrics::{self, UsageOutcome, UsageRequest, UsageStop};
use crate::netpol;
use crate::nodes;
use crate::openapi;
use crate::overlay;
use crate::policy;
use crate::prom;
use crate::reach::{self, Bound, ReachSettings, ToolKind, ToolReach};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched<T> {
    Ok(T),
    Denied { what: &'static str },
    Failed { what: &'static str, why: String },
}

pub(crate) fn classify<T>(what: &'static str, error: &kube::Error) -> Fetched<T> {
    if let kube::Error::Api(response) = error
        && matches!(response.code, 401 | 403)
    {
        return Fetched::Denied { what };
    }
    Fetched::Failed {
        what,
        why: crate::connect::describe(error as &(dyn std::error::Error + 'static)),
    }
}

// The same classification flattened to one displayable line, for states that
// carry text rather than a Fetched (a forward's Dead reason).
pub(crate) fn classify_text(what: &'static str, error: &kube::Error) -> String {
    match classify::<()>(what, error) {
        Fetched::Denied { what } => format!("{what}: access denied for this account"),
        Fetched::Failed { why, .. } => why,
        Fetched::Ok(()) => unreachable!("classify never returns Ok"),
    }
}

pub(crate) fn collection_path(target: &KindTarget, namespace: Option<&str>) -> String {
    let resource = &target.resource;
    let mut path = if resource.group.is_empty() {
        format!("/api/{}", resource.version)
    } else {
        format!("/apis/{}/{}", resource.group, resource.version)
    };
    if target.namespaced
        && let Some(namespace) = namespace
    {
        path.push_str("/namespaces/");
        path.push_str(namespace);
    }
    path.push('/');
    path.push_str(&resource.plural);
    path
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindRow {
    pub id: KindId,
    // "deployments.apps", "pods" -- the name a kubectl user types.
    pub display: String,
    pub kind: String,
    pub namespaced: bool,
    // None means the probe had no answer: attempted, not gated.
    pub verdict: Option<Capability>,
    // Whether the server serves a patch verb here at all, which is a different
    // question from whether this account may use it.
    pub patchable: bool,
}

#[derive(Clone)]
pub struct Reader {
    client: Client,
    handle: tokio::runtime::Handle,
    targets: Arc<[KindTarget]>,
    verdicts: Arc<HashMap<KindId, Capability>>,
    forwards: ForwardRegistry,
    netpol: Arc<Mutex<Option<(Instant, netpol::Inventory)>>>,
}

impl Reader {
    fn target(&self, kind: KindId) -> Option<KindTarget> {
        self.targets.iter().find(|t| t.id == kind).cloned()
    }

    pub(crate) fn new(
        client: Client,
        targets: Vec<KindTarget>,
        verdicts: &[(KindId, Capability)],
    ) -> Reader {
        let handle = tokio::runtime::Handle::current();
        Reader {
            forwards: ForwardRegistry::new(Arc::new(KubeForwarder::new(
                client.clone(),
                handle.clone(),
            ))),
            client,
            handle,
            targets: targets.into(),
            verdicts: Arc::new(verdicts.iter().copied().collect()),
            netpol: Arc::new(Mutex::new(None)),
        }
    }

    pub fn kinds(&self) -> Vec<KindRow> {
        let mut rows: Vec<KindRow> = self
            .targets
            .iter()
            .filter(|target| target.listable)
            .map(|target| KindRow {
                id: target.id,
                display: if target.group().is_empty() {
                    target.plural().to_string()
                } else {
                    format!("{}.{}", target.plural(), target.group())
                },
                kind: target.kind().to_string(),
                namespaced: target.namespaced,
                verdict: self.verdicts.get(&target.id).copied(),
                patchable: target.patchable,
            })
            .collect();
        rows.sort_by(|a, b| a.display.cmp(&b.display));
        rows
    }

    // `continue_token` asks for the page after the one that returned it;
    // None asks for the first page.
    pub fn fetch_table(
        &self,
        kind: KindId,
        continue_token: Option<String>,
        reply: impl FnOnce(Fetched<TablePage>) + Send + 'static,
    ) {
        let Some(target) = self.target(kind) else {
            reply(Fetched::Failed {
                what: "table",
                why: "this kind is not served by the connected cluster".to_string(),
            });
            return;
        };
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(browse::fetch_table(&client, &target, None, continue_token.as_deref()).await);
        });
    }

    pub fn fetch_describe(
        &self,
        request: DescribeRequest,
        reply: impl FnOnce(Fetched<Described>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(describe::fetch_describe(&client, &targets, &request).await);
        });
    }

    pub fn fetch_manifest(
        &self,
        request: DescribeRequest,
        reply: impl FnOnce(Fetched<manifest::Manifest>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(manifest::fetch_manifest(&client, &targets, &request).await);
        });
    }

    // The one mutating method in the crate. Dry run and apply are the same
    // request with one query parameter between them, which is why they are one
    // method: a caller cannot reach the apply without having been able to reach
    // the dry run.
    pub fn apply(&self, request: ApplyRequest, reply: impl FnOnce(ApplyOutcome) + Send + 'static) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(apply::apply(&client, &targets, &request).await);
        });
    }

    pub fn fetch_schema_catalog(
        &self,
        reply: impl FnOnce(Fetched<Vec<openapi::SchemaSource>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(openapi::fetch_catalog(&client).await);
        });
    }

    pub fn fetch_schema_document(
        &self,
        url: String,
        reply: impl FnOnce(Fetched<String>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(openapi::fetch_document(&client, &url).await);
        });
    }

    pub fn fetch_crd_schemas(&self, reply: impl FnOnce(Fetched<String>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(openapi::fetch_crds(&client).await);
        });
    }

    pub fn fetch_containers(
        &self,
        namespace: &str,
        pod: &str,
        reply: impl FnOnce(Fetched<Vec<String>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let namespace = namespace.to_string();
        let pod = pod.to_string();
        self.handle.spawn(async move {
            reply(logs::fetch_containers(&client, &namespace, &pod).await);
        });
    }

    // Helm's own release state, read out of the Secrets it writes: nothing
    // installed, nothing templated, and the narrowest list the server will serve
    // -- `type=helm.sh/release.v1` and `owner=helm` -- so no Secret that is not a
    // release ever crosses the wire.
    pub fn fetch_releases(
        &self,
        namespace: Option<String>,
        reply: impl FnOnce(Fetched<helm::Releases>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(helm::fetch_releases(&client, &targets, namespace.as_deref()).await);
        });
    }

    pub fn fetch_node_table(&self, reply: impl FnOnce(Fetched<TablePage>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(nodes::fetch_node_table(&client).await);
        });
    }

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

    pub fn follow_log(
        &self,
        request: LogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        logs::follow(&self.handle, self.client.clone(), request, on_chunk)
    }

    // The managed forward registry: list and close are synchronous local
    // state; opening goes through `open_forward` because the target has to
    // be resolved on the cluster first.
    pub fn forwards(&self) -> &ForwardRegistry {
        &self.forwards
    }

    // Resolution only -- what pod and which ports a request means -- with no
    // listener bound; this is the half a scripted API server can prove.
    pub fn resolve_forward(
        &self,
        request: ForwardRequest,
        reply: impl FnOnce(Fetched<forward::ForwardSpec>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(forward::resolve(&client, &request).await);
        });
    }

    // Resolve, then register: the registry's answer (collision, cap) comes
    // back as a labelled failure like everything else.
    pub fn open_forward(
        &self,
        request: ForwardRequest,
        reply: impl FnOnce(Fetched<ForwardRow>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let registry = self.forwards.clone();
        self.handle.spawn(async move {
            match forward::resolve(&client, &request).await {
                Fetched::Ok(spec) => reply(match registry.open(spec) {
                    Ok(row) => Fetched::Ok(row),
                    Err(error) => Fetched::Failed {
                        what: "port-forward",
                        why: error.to_string(),
                    },
                }),
                Fetched::Denied { what } => reply(Fetched::Denied { what }),
                Fetched::Failed { what, why } => reply(Fetched::Failed { what, why }),
            }
        });
    }

    // An interactive exec over the kube transport; the returned session
    // carries input and resize, and dropping it terminates the remote shell.
    pub fn start_exec(
        &self,
        request: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession> {
        KubeExecTransport::new(self.client.clone(), self.handle.clone()).start(request, on_event)
    }

    // Live usage for a pod or a workload, re-fetched on the request's own
    // cadence until the guard drops; every answer is a labelled
    // [`UsageOutcome`], and a tick that repeats the last one is not
    // re-delivered. Denied and Absent end the poll themselves.
    pub fn poll_usage(
        &self,
        request: UsageRequest,
        on_update: Box<dyn Fn(UsageOutcome) + Send + Sync>,
    ) -> UsageStop {
        metrics::poll(
            &self.handle,
            self.client.clone(),
            self.targets.clone(),
            request,
            on_update,
        )
    }

    // One merged follow over the pods the workload's selector matches; the
    // returned guard cancels every underlying follow.
    pub fn follow_workload_logs(
        &self,
        request: logs::WorkloadLogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        let Some(target) = self.target(request.kind) else {
            on_chunk(LogChunk::Failed {
                what: "workload logs",
                why: "this kind is not served by the connected cluster".to_string(),
            });
            return LogStop::noop();
        };
        logs::follow_workload(&self.handle, self.client.clone(), target, request, on_chunk)
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
        overlay::Kind::Policy => load_policy(client, cache).await,
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

async fn load_policy(
    client: &Client,
    cache: &Mutex<Option<(Instant, netpol::Inventory)>>,
) -> Fetched<overlay::Frame> {
    match policy::fetch_reports(client).await {
        Fetched::Ok(inventory) if inventory.served && !inventory.tints().is_empty() => {
            Fetched::Ok(overlay::from_policy_reports(&inventory))
        }
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
        Fetched::Ok(_) => match cached_netpol(client, cache).await {
            Fetched::Ok(inventory) => Fetched::Ok(overlay::from_netpol(&inventory)),
            Fetched::Denied { what } => Fetched::Denied { what },
            Fetched::Failed { what, why } => Fetched::Failed { what, why },
        },
    }
}

async fn load_metrics(client: &Client, settings: ReachSettings) -> Fetched<overlay::Frame> {
    match query_prometheus(client, settings, overlay::CPU_EXPR).await {
        Fetched::Ok(result) => Fetched::Ok(overlay::from_prometheus(&result)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { why, .. } => Fetched::Ok(overlay::Frame {
            stamps: Vec::new(),
            truncated: false,
            note: Some(why),
        }),
    }
}

async fn load_mesh_observed(client: &Client, settings: ReachSettings) -> Fetched<overlay::Frame> {
    match query_prometheus(client, settings, overlay::MESH_EXPR).await {
        Fetched::Ok(result) => {
            let labels: Vec<mesh::SeriesLabels> = result
                .series
                .iter()
                .map(|series| mesh::SeriesLabels {
                    name: series
                        .labels
                        .iter()
                        .find(|(key, _)| key == "__name__")
                        .map(|(_, value)| value.clone())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "istio_requests_total".to_string()),
                    labels: series.labels.clone(),
                })
                .collect();
            Fetched::Ok(overlay::from_mesh_observed(&mesh::observed_from_series(
                &labels,
            )))
        }
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { why, .. } => Fetched::Ok(overlay::Frame {
            stamps: Vec::new(),
            truncated: false,
            note: Some(why),
        }),
    }
}

async fn query_prometheus(
    client: &Client,
    settings: ReachSettings,
    expr: &str,
) -> Fetched<prom::QueryResult> {
    match reach::bind(client, ToolKind::Prometheus, &settings).await {
        ToolReach::Absent { .. } => Fetched::Failed {
            what: "prometheus",
            why: "Prometheus is not in this cluster".to_string(),
        },
        ToolReach::Unbound(unbound) => Fetched::Failed {
            what: "prometheus",
            why: unbound.why,
        },
        ToolReach::Bound(bound) => {
            let end = unix_secs();
            let start = end - overlay::RANGE_SECS;
            prom::query_range(client, &bound, expr, start, end, overlay::STEP).await
        }
    }
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

fn unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::discovery::{ApiCapabilities, ApiResource, Scope};

    fn target(
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        namespaced: bool,
    ) -> KindTarget {
        let mut catalog = k10s_core::Catalog::new();
        crate::discover::intern(
            &mut catalog,
            ApiResource {
                group: group.to_string(),
                version: version.to_string(),
                api_version: if group.is_empty() {
                    version.to_string()
                } else {
                    format!("{group}/{version}")
                },
                kind: kind.to_string(),
                plural: plural.to_string(),
            },
            &ApiCapabilities {
                scope: if namespaced {
                    Scope::Namespaced
                } else {
                    Scope::Cluster
                },
                subresources: Vec::new(),
                operations: vec!["get".into(), "list".into(), "watch".into()],
            },
        )
    }

    fn api_error(code: u16) -> kube::Error {
        kube::Error::Api(Box::new(kube::core::Status {
            code,
            reason: "Unauthorized".to_string(),
            message: "no".to_string(),
            ..Default::default()
        }))
    }

    #[test]
    fn an_account_the_cluster_refuses_is_denied_whichever_code_it_uses() {
        for code in [401, 403] {
            assert_eq!(
                classify::<()>("pods", &api_error(code)),
                Fetched::Denied { what: "pods" },
                "a watch stops on {code} as forbidden: a panel must say the same thing"
            );
        }
        assert!(matches!(
            classify::<()>("pods", &api_error(500)),
            Fetched::Failed { what: "pods", .. }
        ));
    }

    #[test]
    fn collection_paths_cover_core_group_and_namespace_scoping() {
        let pods = target("", "v1", "Pod", "pods", true);
        assert_eq!(
            collection_path(&pods, Some("prod")),
            "/api/v1/namespaces/prod/pods"
        );
        assert_eq!(collection_path(&pods, None), "/api/v1/pods");

        let deployments = target("apps", "v1", "Deployment", "deployments", true);
        assert_eq!(
            collection_path(&deployments, Some("prod")),
            "/apis/apps/v1/namespaces/prod/deployments"
        );

        let namespaces = target("", "v1", "Namespace", "namespaces", false);
        assert_eq!(
            collection_path(&namespaces, Some("prod")),
            "/api/v1/namespaces",
            "a cluster-scoped kind never nests under a namespace"
        );
    }
}
