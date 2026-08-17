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

use crate::alertmanager;
use crate::apply::{self, ApplyOutcome, ApplyRequest};
use crate::argo;
use crate::browse::{self, TablePage};
use crate::cilium;
use crate::cilium_control;
use crate::cnpg;
use crate::day2;
use crate::describe::{self, DescribeRequest, Described};
use crate::discover::KindTarget;
use crate::eso;
use crate::exec::{ExecEvent, ExecRequest, ExecSession, ExecTransport, KubeExecTransport};
use crate::falco;
use crate::flux;
use crate::forward::{self, ForwardRegistry, ForwardRequest, ForwardRow, KubeForwarder};
use crate::gateway;
use crate::grafana;
use crate::harbor;
use crate::helm;
use crate::helm_reveal;
use crate::ingress;
use crate::kargo;
use crate::kyverno;
use crate::logs::{self, LogChunk, LogRequest, LogStop};
use crate::loki;
use crate::manifest;
use crate::mesh;
use crate::metrics::{self, UsageOutcome, UsageRequest, UsageStop};
use crate::netpol;
use crate::nodes;
use crate::openapi;
use crate::otel;
use crate::overlay;
use crate::policy;
use crate::prom;
use crate::proxies;
use crate::reach::{self, Bound, ReachSettings, ToolKind, ToolReach};
use crate::tetragon;
use crate::traces;
use crate::traefik;
use crate::vault;
use crate::velero;

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

#[derive(Clone)]
pub struct Reader {
    client: Client,
    handle: tokio::runtime::Handle,
    targets: Arc<[KindTarget]>,
    verdicts: Arc<HashMap<KindId, Capability>>,
    caps: Arc<HashMap<KindId, day2::Caps>>,
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
        caps: &[(KindId, day2::Caps)],
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
            caps: Arc::new(caps.iter().copied().collect()),
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

    // Server-side apply. Dry run and apply are the same request with one query
    // parameter between them, which is why they are one method: a caller cannot
    // reach the apply without having been able to reach the dry run. Day-2
    // clicks are a different method; they are not documents.
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

    /// Argo Applications already published on the cluster. Absence is
    /// [`argo::Inventory::served`] = false, not an error.
    pub fn fetch_argo(&self, reply: impl FnOnce(Fetched<argo::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(argo::fetch_inventory(&client, &targets, None).await);
        });
    }

    /// Flux CRs already published on the cluster. Absence is
    /// [`flux::Inventory::served`] = false, not an error.
    pub fn fetch_flux(&self, reply: impl FnOnce(Fetched<flux::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(flux::fetch(&client, None).await);
        });
    }

    /// CloudNativePG CRs already published on the cluster. Absence is
    /// [`cnpg::Inventory::served`] = false, not an error.
    pub fn fetch_cnpg(&self, reply: impl FnOnce(Fetched<cnpg::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(cnpg::fetch(&client, None).await);
        });
    }

    /// Velero CRs already published on the cluster. Absence is
    /// [`velero::Inventory::served`] = false, not an error.
    pub fn fetch_velero(&self, reply: impl FnOnce(Fetched<velero::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(velero::fetch(&client, None).await);
        });
    }

    pub fn fetch_cilium(&self, reply: impl FnOnce(Fetched<cilium::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(cilium::fetch(&client, None).await);
        });
    }

    pub fn fetch_cilium_control(
        &self,
        reply: impl FnOnce(Fetched<cilium_control::Inventory>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(cilium_control::fetch(&client, None).await);
        });
    }

    pub fn fetch_falco(&self, reply: impl FnOnce(Fetched<falco::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(falco::fetch(&client, None).await);
        });
    }

    pub fn fetch_tetragon(
        &self,
        reply: impl FnOnce(Fetched<tetragon::Inventory>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(tetragon::fetch(&client, None).await);
        });
    }

    pub fn fetch_traefik(&self, reply: impl FnOnce(Fetched<traefik::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(traefik::fetch(&client, None).await);
        });
    }

    pub fn fetch_gateway(&self, reply: impl FnOnce(Fetched<gateway::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(gateway::fetch(&client, None).await);
        });
    }

    pub fn fetch_ingress(&self, reply: impl FnOnce(Fetched<ingress::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(ingress::fetch(&client, None).await);
        });
    }

    pub fn fetch_proxies(&self, reply: impl FnOnce(Fetched<proxies::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(proxies::fetch(&client, None).await);
        });
    }

    pub fn fetch_kyverno(&self, reply: impl FnOnce(Fetched<kyverno::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(kyverno::fetch(&client, None).await);
        });
    }

    pub fn fetch_eso(&self, reply: impl FnOnce(Fetched<eso::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(eso::fetch(&client, None).await);
        });
    }

    pub fn fetch_vault(&self, reply: impl FnOnce(Fetched<vault::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(vault::fetch(&client, None).await);
        });
    }

    pub fn fetch_kargo(&self, reply: impl FnOnce(Fetched<kargo::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(kargo::fetch(&client, None).await);
        });
    }

    pub fn fetch_otel(&self, reply: impl FnOnce(Fetched<otel::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(otel::fetch(&client, None).await);
        });
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

    /// Every ecosystem family, fetched concurrently and reduced to its own
    /// table. A family whose answer is [`Fetched::Ok`]`(None)` is not on
    /// this cluster and stays hidden; Denied and Failed stay labelled per
    /// family so one broken adapter cannot hide the other fifteen.
    pub fn fetch_ecosystem(&self, reply: impl FnOnce(Vec<EcosystemFamily>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(ecosystem_families(&client).await);
        });
    }

    /// Scale, rollout, delete, evict, cordon, drain, debug. Caps are filled
    /// here from the probe so a caller cannot skip the gate; confirm still
    /// lives on the request, so the first press never touches the wire.
    pub fn day2(
        &self,
        kind: KindId,
        mut call: day2::Day2Call,
        reply: impl FnOnce(day2::Day2Outcome) + Send + 'static,
    ) {
        let Some(target) = self.target(kind) else {
            reply(day2::Day2Outcome::Failed {
                why: "this kind is not served by the connected cluster".to_string(),
            });
            return;
        };
        call.set_caps(self.caps_for(kind));
        if let day2::Day2Call::Rollout(request) = &mut call
            && let day2::RolloutAction::Restart { restarted_at } = &mut request.action
            && restarted_at.is_empty()
        {
            let elapsed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            *restarted_at = flux::rfc3339(elapsed.as_secs(), elapsed.subsec_nanos());
        }
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(day2::run(&client, &target, &call).await);
        });
    }

    fn caps_for(&self, kind: KindId) -> day2::Caps {
        self.caps.get(&kind).copied().unwrap_or_default()
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

    /// Grafana dashboards as extracted queries. Provisioned ConfigMaps do not
    /// wait on a Grafana bind. [`GrafanaCatalog::served`] is false only when
    /// Grafana is absent and nothing was provisioned.
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

    pub fn fetch_policy_reports(
        &self,
        reply: impl FnOnce(Fetched<policy::Inventory>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(policy::fetch_reports(&client).await);
        });
    }

    pub fn fetch_harbor(&self, reply: impl FnOnce(Fetched<harbor::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            let reach = reach::bind(&client, ToolKind::Harbor, &ReachSettings::default()).await;
            reply(harbor::fetch(&client, &reach).await);
        });
    }

    pub fn fetch_mesh_declared(&self, reply: impl FnOnce(mesh::MeshInventory) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(mesh::inventory(&client).await);
        });
    }

    /// One stored revision after an explicit reveal. The answer is
    /// [`helm_reveal::RevealedRevision`]: not `Clone`, and not an inventory.
    pub fn reveal_helm_revision(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: impl FnOnce(Fetched<helm_reveal::RevealedRevision>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(
                helm_reveal::reveal_revision(
                    &client,
                    &targets,
                    namespace.as_deref(),
                    &name,
                    revision,
                )
                .await,
            );
        });
    }

    /// Diff of two revisions as an owned string. Values use
    /// [`helm_reveal::diff_values`]; manifests are compared here and never
    /// stored on a release row.
    pub fn diff_helm_revisions(
        &self,
        namespace: Option<String>,
        name: String,
        from: u32,
        to: u32,
        reply: impl FnOnce(Fetched<String>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(
                diff_helm_revisions(&client, &targets, namespace.as_deref(), &name, from, to).await,
            );
        });
    }

    /// Server-side apply of a stored revision. The report's note is always
    /// [`helm_reveal::NOT_HELM_ROLLBACK`].
    pub fn rollback_helm_revision(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: impl FnOnce(Fetched<helm_reveal::RollbackReport>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(
                rollback_helm_revision(&client, &targets, namespace.as_deref(), &name, revision)
                    .await,
            );
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

async fn bind_prometheus(client: &Client, settings: &ReachSettings) -> Fetched<reach::Bound> {
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

async fn query_bound(
    client: &Client,
    bound: &reach::Bound,
    expr: &str,
) -> Fetched<prom::QueryResult> {
    let end = unix_secs();
    let start = end - overlay::RANGE_SECS;
    prom::query_range(client, bound, expr, start, end, overlay::STEP).await
}

async fn query_prometheus(
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
                            Fetched::Failed { .. } => extra_hits.push(hit),
                        }
                    } else {
                        extra_hits.push(hit);
                        truncated = true;
                    }
                }
            }
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { .. } => {}
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

/// One ecosystem family reduced to its table. The id is the stable key a
/// shell surface joins presentation onto; nothing here names a pane.
#[derive(Debug, Clone, PartialEq)]
pub struct EcosystemFamily {
    pub id: &'static str,
    pub answer: Fetched<Option<TablePage>>,
}

async fn ecosystem_families(client: &Client) -> Vec<EcosystemFamily> {
    macro_rules! family {
        ($id:literal, $module:ident) => {
            async {
                EcosystemFamily {
                    id: $id,
                    answer: match $module::fetch(client, None).await {
                        Fetched::Ok(inventory) => Fetched::Ok($module::table_page(&inventory)),
                        Fetched::Denied { what } => Fetched::Denied { what },
                        Fetched::Failed { what, why } => Fetched::Failed { what, why },
                    },
                }
            }
        };
    }
    let alertmanager = async {
        EcosystemFamily {
            id: "alertmanager",
            answer: match fetch_alertmanager_optional(client).await {
                Fetched::Ok(alerts) => Fetched::Ok(alertmanager::table_page(alerts.as_ref())),
                Fetched::Denied { what } => Fetched::Denied { what },
                Fetched::Failed { what, why } => Fetched::Failed { what, why },
            },
        }
    };
    let families = tokio::join!(
        family!("cilium", cilium),
        family!("cilium-control", cilium_control),
        family!("tetragon", tetragon),
        family!("falco", falco),
        family!("traefik", traefik),
        family!("gateway", gateway),
        family!("ingress", ingress),
        family!("proxies", proxies),
        family!("kyverno", kyverno),
        family!("eso", eso),
        family!("vault", vault),
        family!("velero", velero),
        family!("cnpg", cnpg),
        family!("kargo", kargo),
        family!("otel", otel),
        alertmanager,
    );
    let (
        cilium,
        cilium_control,
        tetragon,
        falco,
        traefik,
        gateway,
        ingress,
        proxies,
        kyverno,
        eso,
        vault,
        velero,
        cnpg,
        kargo,
        otel,
        alertmanager,
    ) = families;
    vec![
        cilium,
        cilium_control,
        tetragon,
        falco,
        traefik,
        gateway,
        ingress,
        proxies,
        kyverno,
        eso,
        vault,
        velero,
        cnpg,
        kargo,
        otel,
        alertmanager,
    ]
}

async fn fetch_alertmanager_optional(client: &Client) -> Fetched<Option<alertmanager::Alerts>> {
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

async fn diff_helm_revisions(
    client: &Client,
    targets: &[crate::discover::KindTarget],
    namespace: Option<&str>,
    name: &str,
    from: u32,
    to: u32,
) -> Fetched<String> {
    let left = helm_reveal::reveal_revision(client, targets, namespace, name, from).await;
    let right = helm_reveal::reveal_revision(client, targets, namespace, name, to).await;
    match (left, right) {
        (Fetched::Ok(from_rev), Fetched::Ok(to_rev)) => {
            let values = helm_reveal::diff_values(&from_rev, &to_rev);
            let manifests = match (from_rev.manifest().as_str(), to_rev.manifest().as_str()) {
                (Ok(left), Ok(right)) if left == right => format!(
                    "the stored manifests of revision {} and revision {} are identical",
                    from_rev.revision, to_rev.revision
                ),
                (Ok(left), Ok(right)) => format!(
                    "the stored manifests of revision {} and revision {} differ ({} vs {} lines)",
                    from_rev.revision,
                    to_rev.revision,
                    left.lines().count(),
                    right.lines().count()
                ),
                _ => format!(
                    "the stored manifests of revision {} and revision {} differ and are not UTF-8",
                    from_rev.revision, to_rev.revision
                ),
            };
            Fetched::Ok(format!("{values}\n\n{manifests}\n"))
        }
        (Fetched::Denied { what }, _) | (_, Fetched::Denied { what }) => Fetched::Denied { what },
        (Fetched::Failed { what, why }, _) | (_, Fetched::Failed { what, why }) => {
            Fetched::Failed { what, why }
        }
    }
}

async fn rollback_helm_revision(
    client: &Client,
    targets: &[crate::discover::KindTarget],
    namespace: Option<&str>,
    name: &str,
    revision: u32,
) -> Fetched<helm_reveal::RollbackReport> {
    match helm_reveal::reveal_revision(client, targets, namespace, name, revision).await {
        Fetched::Ok(revealed) => {
            Fetched::Ok(helm_reveal::rollback(client, targets, &revealed).await)
        }
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
