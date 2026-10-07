//! UI-independent requests, labelled outcomes and cluster service contracts.
//!
//! This crate depends only on the shared model. Providers own transport and
//! scheduling; views own presentation. A reply may run synchronously or on a
//! worker thread, and the caller bridges it onto its own executor. No method
//! waits for I/O on the calling thread.
//!
//! [`ClusterProvider`] covers both reads and confirmed writes. Request identity,
//! confirmation and uncertain write outcomes stay explicit in their types.
//! [`ProviderSlot`] follows the selected connection; a document reviewing a write
//! can retain [`ProviderSlot::current`] until that request has been answered.
//! [`LaunchProvider`] resolves kubeconfigs and opens connections off the UI thread.

use std::cell::RefCell;
use std::rc::Rc;

use k10s_core::{KindId, OverlayKind};

pub type Reply<T> = Box<dyn FnOnce(T) + Send>;

mod detail;
pub use detail::{Detail, EventRow};

mod overlay;
pub use overlay::{OverlayOutcome, OverlayStamp, PodPostureView, PostureOutcome};

mod table;
pub use table::{EcosystemEntry, KindRow, TableColumn, TableOutcome, TablePage, TableRow};

mod observe;
mod telemetry;
pub use observe::{
    AlertMatcher, AlertPodOutcome, AlertmanagerEndpoint, AlertmanagerOutcome, GrafanaOutcome,
    GrafanaPanelKind, GrafanaPanelRow, LogEntry, LokiOutcome, ObserveReach, PromOutcome,
    PromSeriesView, QueryDialect, SilenceOutcome, SilenceRequest, SpanView, ToolPresence,
    TraceOutcome, grafana_panel_path,
};
pub use telemetry::{
    FetchTiming, LokiRequest, ObserveScope, PodIdentity, PromRequest, QueryWindow, RequestStop,
    ScopeOutcome, exact_pattern, quote,
};

mod helm;
pub use helm::{HelmReveal, HelmRevealOutcome, HelmRollbackOutcome};

mod kubernetes;
pub use kubernetes::{
    ApplyOutcome, ApplyRequest, Conflicted, ContainersOutcome, Day2Op, Day2Outcome, Day2Request,
    DescribeRequest, DocOutcome, LiveCopy, ManifestOutcome, SchemaCatalogOutcome, SchemaSource,
    SchemaTextOutcome,
};

mod logs;
pub use logs::{LogChunk, LogRequest, LogStop, WorkloadLogRequest};

mod usage;
pub use usage::{
    Bytes, Millicores, UsageOutcome, UsageRequest, UsageSample, UsageSource, UsageTarget,
};

mod forward;
pub use forward::{ForwardOutcome, ForwardRequest, ForwardRow, ForwardState};

mod exec;
pub use exec::{ExecEvent, ExecRequest, ExecSession, NullExecSession};

mod launch;
pub use launch::{
    ConfigSource, ConnectOutcome, ConnectRequest, Connection, ContextRow, DemoOutcome,
    LaunchProvider, NullLaunchProvider, ProviderFactory, ScanOutcome, ScanRequest,
};

// Declare each method once so the slot cannot delegate a request to the wrong
// operation. Parameter names and doc attributes survive into both expansions.
macro_rules! cluster_provider {
    (
        $(
            $(#[$attr:meta])*
            fn $name:ident(&self $(, $arg:ident: $ty:ty)* $(,)?) $(-> $ret:ty)?;
        )*
    ) => {
        pub trait ClusterProvider {
            $(
                $(#[$attr])*
                fn $name(&self $(, $arg: $ty)*) $(-> $ret)?;
            )*
        }

        impl ClusterProvider for ProviderSlot {
            $(
                fn $name(&self $(, $arg: $ty)*) $(-> $ret)? {
                    self.current().$name($($arg),*)
                }
            )*
        }
    };
}

cluster_provider! {
    fn fetch_events(&self, namespace: &str, name: &str, reply: Reply<Detail>);
    fn fetch_log_tail(&self, namespace: &str, pod: &str, reply: Reply<Detail>);
    fn kinds(&self) -> Vec<KindRow>;
    /// `continue_token` is a token a previous page carried; None means the
    /// first page.
    fn fetch_table(&self, kind: KindId, continue_token: Option<String>, reply: Reply<TableOutcome>);
    fn fetch_node_table(&self, reply: Reply<TableOutcome>);
    fn fetch_describe(&self, request: &DescribeRequest, reply: Reply<DocOutcome>);
    /// Helm release labels as a metadata-only inventory. Stored Secret payloads
    /// are never fetched; fields absent from metadata remain unavailable.
    fn fetch_releases(&self, reply: Reply<TableOutcome>);
    /// Argo Applications. [`TableOutcome::Absent`] means the group is not
    /// served, so the view stays invisible rather than opening an empty pane.
    fn fetch_argo(&self, reply: Reply<TableOutcome>);
    /// Flux CRs. Absent is the same rule as Argo.
    fn fetch_flux(&self, reply: Reply<TableOutcome>);
    /// Scale, rollout, delete, evict, cordon, drain, debug. Caps are applied
    /// on the far side of the seam so a caller cannot skip the gate. The first
    /// call with confirm=false never writes (a drain's ask reads the node's
    /// pods).
    fn run_day2(&self, request: &Day2Request, reply: Reply<Day2Outcome>);
    fn fetch_manifest(&self, request: &DescribeRequest, reply: Reply<ManifestOutcome>);
    /// Server-side apply. Dry run and apply differ by one field of the
    /// request, so a caller cannot reach the second without being able to
    /// reach the first. Day-2 clicks are [`ClusterProvider::run_day2`]; they
    /// are not documents.
    fn apply(&self, request: &ApplyRequest, reply: Reply<ApplyOutcome>);
    fn fetch_schema_catalog(&self, reply: Reply<SchemaCatalogOutcome>);
    fn fetch_schema_document(&self, url: &str, reply: Reply<SchemaTextOutcome>);
    fn fetch_crd_schemas(&self, reply: Reply<SchemaTextOutcome>);
    fn fetch_containers(&self, namespace: &str, pod: &str, reply: Reply<ContainersOutcome>);
    fn follow_log(
        &self,
        request: &LogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop;
    fn follow_workload_logs(
        &self,
        request: &WorkloadLogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop;
    /// Live usage for a pod or workload, re-delivered on the provider's own
    /// cadence until the guard drops; a tick that repeats the last answer is
    /// not re-delivered, and Denied or Absent ends the poll by itself.
    fn poll_usage(
        &self,
        request: &UsageRequest,
        on_update: Box<dyn Fn(UsageOutcome) + Send + Sync>,
    ) -> LogStop;
    fn open_forward(&self, request: &ForwardRequest, reply: Reply<ForwardOutcome>);
    /// Local registry state: synchronous, no cluster round trip.
    fn list_forwards(&self) -> Vec<ForwardRow>;
    fn close_forward(&self, id: u64) -> bool;
    fn start_exec(
        &self,
        request: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession>;
    /// Overlay stamps assembled off the paint path. Empty Ready is a missing
    /// adapter, not a hole in the cluster.
    fn fetch_overlay(&self, kind: OverlayKind, reply: Reply<OverlayOutcome>);
    /// Isolation and named ports. Not an allow or deny: that needs a source,
    /// protocol, and destination port.
    fn fetch_pod_posture(&self, namespace: &str, name: &str, reply: Reply<PostureOutcome>);
    /// Grafana dashboards reduced to queries. [`GrafanaOutcome::Absent`] hides
    /// the Grafana pane.
    fn fetch_grafana(&self, reply: Reply<GrafanaOutcome>) -> RequestStop;
    fn probe_observe(&self, reply: Reply<ObserveReach>) -> RequestStop;
    fn resolve_observe_scope(&self, request: DescribeRequest, reply: Reply<ScopeOutcome>) -> RequestStop;
    fn query_panel(&self, panel: GrafanaPanelRow, scope: ObserveScope, window: QueryWindow, reply: Reply<PromOutcome>) -> RequestStop;
    fn query_promql(&self, request: PromRequest, reply: Reply<PromOutcome>) -> RequestStop;
    fn query_loki(&self, request: LokiRequest, reply: Reply<LokiOutcome>) -> RequestStop;
    fn lookup_trace(&self, trace_id: String, reply: Reply<TraceOutcome>) -> RequestStop;
    /// PolicyReport findings. Absent hides the pane.
    fn fetch_policy(&self, reply: Reply<TableOutcome>);
    /// Harbor projects. Absent hides the pane.
    fn fetch_harbor(&self, reply: Reply<TableOutcome>);
    /// Declared mesh inventory. Observed stays the overlay.
    fn fetch_mesh(&self, reply: Reply<TableOutcome>);
    /// Every ecosystem family the data plane can list, each reduced to its
    /// own table. Absent families stay hidden; one denied or failed family
    /// never hides the others.
    fn fetch_ecosystem(&self, reply: Reply<Vec<EcosystemEntry>>);
    /// Locate the pod behind a selected Alertmanager row. `uid` is the row's
    /// identity: an alert's fingerprint, whose alert is re-read so its
    /// namespace and pod labels name the join, or the identity of the row
    /// counting omitted alerts, which names no alert, so the lookup is
    /// answered without a request. Table cells never supply object identity.
    fn fetch_alert_pod(&self, uid: String, reply: Reply<AlertPodOutcome>);
    /// Discover the endpoint once so the silence review names its destination.
    fn bind_alertmanager(&self, reply: Reply<AlertmanagerOutcome>);
    fn create_silence(&self, request: &SilenceRequest, confirm: bool, reply: Reply<SilenceOutcome>);
    /// Stored payload operations report their metadata-only policy refusal
    /// before transport. The outcome API also serves disconnected providers.
    fn reveal_helm(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: Reply<HelmRevealOutcome>,
    );
    fn diff_helm(
        &self,
        namespace: Option<String>,
        name: String,
        from: u32,
        to: u32,
        reply: Reply<DocOutcome>,
    );
    fn rollback_helm(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: Reply<HelmRollbackOutcome>,
    );
}

pub struct NullProvider;

const NO_CLUSTER: &str = "no cluster connected; this view needs one";

impl ClusterProvider for NullProvider {
    fn fetch_events(&self, _: &str, _: &str, reply: Reply<Detail>) {
        reply(Detail::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_log_tail(&self, _: &str, _: &str, reply: Reply<Detail>) {
        reply(Detail::Failed(NO_CLUSTER.to_string()));
    }

    fn kinds(&self) -> Vec<KindRow> {
        Vec::new()
    }

    fn fetch_table(&self, _: KindId, _: Option<String>, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_node_table(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_describe(&self, _: &DescribeRequest, reply: Reply<DocOutcome>) {
        reply(DocOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_releases(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_argo(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_flux(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn run_day2(&self, _: &Day2Request, reply: Reply<Day2Outcome>) {
        reply(Day2Outcome::Failed {
            why: NO_CLUSTER.to_string(),
        });
    }

    fn fetch_manifest(&self, _: &DescribeRequest, reply: Reply<ManifestOutcome>) {
        reply(ManifestOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn apply(&self, _: &ApplyRequest, reply: Reply<ApplyOutcome>) {
        reply(ApplyOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_schema_catalog(&self, reply: Reply<SchemaCatalogOutcome>) {
        reply(SchemaCatalogOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_schema_document(&self, _: &str, reply: Reply<SchemaTextOutcome>) {
        reply(SchemaTextOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_crd_schemas(&self, reply: Reply<SchemaTextOutcome>) {
        reply(SchemaTextOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_containers(&self, _: &str, _: &str, reply: Reply<ContainersOutcome>) {
        reply(ContainersOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn follow_log(&self, _: &LogRequest, on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>) -> LogStop {
        on_chunk(LogChunk::Failed(NO_CLUSTER.to_string()));
        LogStop::noop()
    }

    fn follow_workload_logs(
        &self,
        _: &WorkloadLogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        on_chunk(LogChunk::Failed(NO_CLUSTER.to_string()));
        LogStop::noop()
    }

    fn poll_usage(
        &self,
        _: &UsageRequest,
        on_update: Box<dyn Fn(UsageOutcome) + Send + Sync>,
    ) -> LogStop {
        on_update(UsageOutcome::Failed(NO_CLUSTER.to_string()));
        LogStop::noop()
    }

    fn open_forward(&self, _: &ForwardRequest, reply: Reply<ForwardOutcome>) {
        reply(ForwardOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn list_forwards(&self) -> Vec<ForwardRow> {
        Vec::new()
    }

    fn close_forward(&self, _: u64) -> bool {
        false
    }

    fn start_exec(
        &self,
        _: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession> {
        on_event(ExecEvent::Failed(NO_CLUSTER.to_string()));
        Box::new(NullExecSession)
    }

    fn fetch_overlay(&self, _: OverlayKind, reply: Reply<OverlayOutcome>) {
        reply(OverlayOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_pod_posture(&self, _: &str, _: &str, reply: Reply<PostureOutcome>) {
        reply(PostureOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_grafana(&self, reply: Reply<GrafanaOutcome>) -> RequestStop {
        reply(GrafanaOutcome::Failed(NO_CLUSTER.to_string()));
        RequestStop::noop()
    }

    fn probe_observe(&self, reply: Reply<ObserveReach>) -> RequestStop {
        reply(ObserveReach {
            prometheus: ToolPresence::Missing,
            loki: ToolPresence::Missing,
            traces: ToolPresence::Missing,
            error: None,
        });
        RequestStop::noop()
    }

    fn resolve_observe_scope(&self, _: DescribeRequest, reply: Reply<ScopeOutcome>) -> RequestStop {
        reply(ScopeOutcome::Failed(NO_CLUSTER.to_string()));
        RequestStop::noop()
    }
    fn query_panel(
        &self,
        _: GrafanaPanelRow,
        _: ObserveScope,
        _: QueryWindow,
        reply: Reply<PromOutcome>,
    ) -> RequestStop {
        reply(PromOutcome::Failed(NO_CLUSTER.to_string()));
        RequestStop::noop()
    }

    fn query_promql(&self, _: PromRequest, reply: Reply<PromOutcome>) -> RequestStop {
        reply(PromOutcome::Failed(NO_CLUSTER.to_string()));
        RequestStop::noop()
    }

    fn query_loki(&self, _: LokiRequest, reply: Reply<LokiOutcome>) -> RequestStop {
        reply(LokiOutcome::Failed(NO_CLUSTER.to_string()));
        RequestStop::noop()
    }

    fn lookup_trace(&self, _: String, reply: Reply<TraceOutcome>) -> RequestStop {
        reply(TraceOutcome::Failed(NO_CLUSTER.to_string()));
        RequestStop::noop()
    }

    fn fetch_policy(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_harbor(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_mesh(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_ecosystem(&self, reply: Reply<Vec<EcosystemEntry>>) {
        reply(Vec::new());
    }

    fn fetch_alert_pod(&self, _: String, reply: Reply<AlertPodOutcome>) {
        reply(AlertPodOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn bind_alertmanager(&self, reply: Reply<AlertmanagerOutcome>) {
        reply(AlertmanagerOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn create_silence(&self, _: &SilenceRequest, _: bool, reply: Reply<SilenceOutcome>) {
        reply(SilenceOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn reveal_helm(&self, _: Option<String>, _: String, _: u32, reply: Reply<HelmRevealOutcome>) {
        reply(HelmRevealOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn diff_helm(&self, _: Option<String>, _: String, _: u32, _: u32, reply: Reply<DocOutcome>) {
        reply(DocOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn rollback_helm(
        &self,
        _: Option<String>,
        _: String,
        _: u32,
        reply: Reply<HelmRollbackOutcome>,
    ) {
        reply(HelmRollbackOutcome::Failed(NO_CLUSTER.to_string()));
    }
}

/// The one place the workspace's provider lives.
///
/// Every view clones this rather than the provider behind it, so adopting a
/// connection after the window is open re-points the whole workspace at once.
/// Without it a cluster chosen from the launch screen would reach the views
/// opened *after* the choice and nothing that was already on screen, and a
/// cluster replaced would leave open views holding a data plane that had since
/// been retired.
pub struct ProviderSlot(RefCell<Rc<dyn ClusterProvider>>);

impl ProviderSlot {
    pub fn new(provider: Rc<dyn ClusterProvider>) -> ProviderSlot {
        ProviderSlot(RefCell::new(provider))
    }

    pub fn empty() -> ProviderSlot {
        ProviderSlot::new(Rc::new(NullProvider))
    }

    pub fn set(&self, provider: Rc<dyn ClusterProvider>) {
        *self.0.borrow_mut() = provider;
    }

    /// Retain the current connection without holding the slot borrowed.
    /// Synchronous replies may replace the slot or issue another request.
    pub fn current(&self) -> Rc<dyn ClusterProvider> {
        self.0.borrow().clone()
    }
}

#[cfg(test)]
mod tests;
