//! The Kubernetes data plane: read-only, and the only thing here that knows
//! Kubernetes exists.
//!
//! It produces [`k10s_core::IngestEvent`], the same contract the generator
//! implements and the world consumes, so a real cluster and a synthetic one are
//! interchangeable at the seam. Nothing above this crate imports `kube`.
//!
//! # Shape
//!
//! One blocking call, [`DataPlane::sync`], does the whole cold start and returns a
//! conforming initial sync; the watches it opened stay open and feed live events
//! into the sink for the incremental phase to consume.
//!
//! ```text
//! connect ── kubeconfig merge, context, exec/OIDC, in-cluster    connect.rs
//!   discover ── /api + /apis, preferred versions, CRDs interned  discover.rs
//!     probe ── SelfSubjectRulesReview + SelfSubjectAccessReview  rbac.rs
//!       watch ── one reflector per kind or permitted namespace   watch.rs
//!         map ── objects to Payload, containers to State         mapping.rs
//!           assemble ── a hierarchy, parents first, Added only   assemble.rs
//! ```
//!
//! # Where the work is
//!
//! kube-rs owns the protocol. The kubeconfig merge rules, exec-credential plugins,
//! OIDC refresh, in-cluster detection, discovery, and `resourceVersion` with
//! bookmarks and 410 recovery are all its. What this crate adds is the mapping into
//! our contract plus three things kube-rs deliberately leaves to a caller:
//! credential expiry on a cached client, an RBAC verdict as an input rather than an
//! error handler, and the refusal to retry a 403.
//!
//! # Secret hygiene
//!
//! `SceneSnapshot` is cloned, held by readers and lives across frames, so a secret
//! that enters it has unbounded lifetime. The invariant is therefore structural
//! rather than careful: a Secret is watched through the API server's
//! `PartialObjectMetadata` projection, so its values are never sent to this
//! process, and the only staging function a Secret reaches is given an
//! `ObjectMeta`. [`discover::fidelity_of`] is the single place that could break
//! this, and it has a test that says so. The crate also logs nothing.

pub mod assemble;
pub mod connect;
pub mod discover;
pub mod mapping;
pub mod rbac;
pub mod watch;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use k10s_core::{
    Capability, Catalog, DesyncReason, IngestEvent, KindId, Op, Payload, ResourceEvent, State,
};

use assemble::{AssembleStats, Index, Store};
use connect::{ConnectError, Connector, Env};
use discover::{Discovered, WatchTarget};
use mapping::{AttachKinds, Detail, Staged};
use rbac::Access;
use watch::Message;

/// Where a producer sends what it learns. Bounded queueing and coalescing are the
/// consumer's job, via `k10s_core::Intake`.
pub type EventSink = Sender<IngestEvent>;

/// How many messages may sit between the watch tasks and the collector.
///
/// Bounded on purpose. An unbounded queue dies on a resync storm; blocking a watch
/// task slows one stream, which is recoverable, and the collector is a few
/// microseconds per message.
const INTERNAL_QUEUE: usize = 8192;

/// What to connect to and how patient to be about it.
#[derive(Debug, Clone)]
pub struct Options {
    /// A context by name. `None` uses the kubeconfig's `current-context`.
    pub context: Option<String>,
    /// Namespaces to run a rules review in.
    ///
    /// Only matters on a cluster that denies cluster-wide list: with cluster-wide
    /// permission one access review answers for every namespace, and probing two
    /// hundred namespaces to learn the same thing would be two hundred requests.
    pub probe_namespaces: Vec<String>,
    /// How long to wait for every kind to finish its initial list.
    pub sync_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            context: None,
            probe_namespaces: Vec::new(),
            sync_timeout: Duration::from_secs(30),
        }
    }
}

/// What the cold start cost and found, for the report §6.7 asks for.
#[derive(Debug, Clone, Default)]
pub struct ClusterReport {
    pub context: Option<String>,
    pub cluster_url: String,
    pub server_version: Option<String>,
    /// Whether the two-request aggregated discovery document was available.
    pub aggregated_discovery: bool,
    pub kinds_discovered: usize,
    pub kinds_watched: usize,
    pub streams: usize,
    /// Streams scoped to one namespace rather than to the cluster, which is the
    /// shape a restricted service account produces. Counted per stream, so a kind
    /// read across three namespaces contributes three.
    pub namespaced_streams: usize,
    pub probe_requests: u32,
    /// The probe could not run, so capabilities gate nothing and a stream's own
    /// 403 is the verdict.
    pub probe_degraded: bool,
    /// Kinds whose cluster-wide access review never answered. They are attempted
    /// rather than gated, so a denial on one arrives as a stream error instead of
    /// a label.
    pub kinds_unanswered: usize,
    /// The namespaces a rules review answered for. A kind reported `Forbidden` was
    /// checked against these and nowhere else, so this is the set `--namespace`
    /// extends.
    pub probed_namespaces: Vec<String>,
    pub objects_held: usize,
    pub assemble: AssembleStats,
    pub desyncs: Vec<(KindId, DesyncReason)>,
    /// Kinds that never finished listing inside the timeout.
    pub unsettled: Vec<KindId>,
    pub connect_ms: f64,
    pub discover_ms: f64,
    pub probe_ms: f64,
    pub list_ms: f64,
    pub assemble_ms: f64,
    pub total_ms: f64,
}

impl ClusterReport {
    /// A one-line summary for a startup log. Names no object and no credential.
    pub fn summary(&self) -> String {
        let ctx = self.context.as_deref().unwrap_or("in-cluster");
        let version = self.server_version.as_deref().unwrap_or("unknown");
        format!(
            "{ctx} at {} (server {version}), {} kinds discovered, {} watched over {} streams, \
             {} objects, {} namespaces / {} owners / {} instances / {} attachments in {:.0} ms",
            self.cluster_url,
            self.kinds_discovered,
            self.kinds_watched,
            self.streams,
            self.objects_held,
            self.assemble.scopes,
            self.assemble.owners,
            self.assemble.instances,
            self.assemble.attachments,
            self.total_ms,
        )
    }
}

/// A conforming initial sync, plus the catalog its ids were interned into.
pub struct Sync {
    /// Ordered so `k10s_world::input::fold` accepts it: parents first, every event
    /// `Added`, no orphans.
    pub events: Vec<IngestEvent>,
    /// A snapshot. The live forwarder keeps its own and may intern reasons this one
    /// has never seen; those render as `Unknown`, which is what they are.
    pub catalog: Catalog,
    pub report: ClusterReport,
}

/// Counters for the ingest budget §6.7 defines here and enforces from D.
#[derive(Debug, Default)]
pub struct IngestMetrics {
    pub applies: AtomicU64,
    pub deletes: AtomicU64,
    pub desyncs: AtomicU64,
    /// Live events handed to the sink after the initial sync. There is no drop
    /// counter next to it on purpose: the internal queue is bounded and applies
    /// backpressure to one stream, so nothing here drops anything.
    pub forwarded: AtomicU64,
}

/// A flat read of [`IngestMetrics`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub applies: u64,
    pub deletes: u64,
    pub desyncs: u64,
    pub forwarded: u64,
}

impl IngestMetrics {
    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            applies: self.applies.load(Ordering::Relaxed),
            deletes: self.deletes.load(Ordering::Relaxed),
            desyncs: self.desyncs.load(Ordering::Relaxed),
            forwarded: self.forwarded.load(Ordering::Relaxed),
        }
    }
}

/// What can go wrong before there is anything to show.
#[derive(Debug)]
pub enum DataError {
    Connect(ConnectError),
    /// Discovery failed, which means we cannot name anything, so there is no
    /// degraded mode to fall back to.
    Discovery(String),
    /// The cluster serves none of the kinds we know how to draw.
    NothingWatchable,
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::Connect(e) => write!(f, "{e}"),
            DataError::Discovery(why) => {
                write!(f, "cannot discover what the cluster serves: {why}")
            }
            DataError::NothingWatchable => write!(
                f,
                "the cluster serves none of the kinds k10s can draw, or none of them are readable"
            ),
        }
    }
}

impl std::error::Error for DataError {}

impl From<ConnectError> for DataError {
    fn from(e: ConnectError) -> Self {
        DataError::Connect(e)
    }
}

pub struct DataPlane {
    runtime: tokio::runtime::Runtime,
    events: EventSink,
    metrics: Arc<IngestMetrics>,
}

impl DataPlane {
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// The sink watchers publish into.
    pub fn events(&self) -> &EventSink {
        &self.events
    }

    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Connects, discovers, probes, lists, and returns a conforming initial sync.
    ///
    /// Blocking, because the caller is the app's startup path and the whole point
    /// is that the render thread never waits on I/O: this runs before a window
    /// exists. The watches stay open afterwards and live events go to the sink.
    pub fn sync(&self, options: &Options) -> Result<Sync, DataError> {
        let metrics = self.metrics.clone();
        let sink = self.events.clone();
        self.runtime
            .block_on(async move { cold_start(options, sink, metrics).await })
    }

    /// Lists the contexts a kubeconfig declares, without connecting to any of them.
    pub fn contexts(&self) -> Result<Vec<String>, DataError> {
        Ok(Connector::load(&Env::from_process())?.contexts())
    }
}

pub fn spawn(events: EventSink) -> std::io::Result<DataPlane> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("k10s-data")
        .enable_all()
        .build()?;
    Ok(DataPlane {
        runtime,
        events,
        metrics: Arc::new(IngestMetrics::default()),
    })
}

async fn cold_start(
    options: &Options,
    sink: EventSink,
    metrics: Arc<IngestMetrics>,
) -> Result<Sync, DataError> {
    let started = Instant::now();
    let mut connector = Connector::load(&Env::from_process())?;
    let connection = connector.connect(options.context.as_deref()).await?;
    let connect_ms = ms(started);

    let mut sync = sync_from(
        connection.client,
        &connection.default_namespace,
        options,
        sink,
        metrics,
    )
    .await?;
    sync.report.context = connection.context;
    sync.report.cluster_url = connection.cluster_url;
    sync.report.connect_ms = connect_ms;
    sync.report.total_ms = ms(started);
    Ok(sync)
}

/// Everything after the connection, given a client.
///
/// Split out because `kube::Client` is a `tower` service, so a test can *be* the
/// API server: discovery, the RBAC probe, the metadata projection on the wire and a
/// full initial sync are all driveable with no cluster and no sockets. That is the
/// difference between "carefully written" and "checked".
pub async fn sync_from(
    client: kube::Client,
    default_namespace: &str,
    options: &Options,
    sink: EventSink,
    metrics: Arc<IngestMetrics>,
) -> Result<Sync, DataError> {
    let started = Instant::now();
    let mut report = ClusterReport::default();

    let mut catalog = Catalog::new();
    // Pre-intern every reason the mapping can name, so the ids the live forwarder
    // hands out later agree with the snapshot the app holds. Only a reason nobody
    // has a severity for can be interned late, and those are `Unknown` anyway.
    for reason in mapping::known_reasons() {
        catalog.intern_reason(reason);
    }

    let at_discover = Instant::now();
    let discovered = discover::discover(&client, &mut catalog)
        .await
        .map_err(|e| DataError::Discovery(connect::describe(&e as &dyn std::error::Error)))?;
    report.discover_ms = ms(at_discover);
    report.server_version = discovered.server_version.clone();
    report.aggregated_discovery = discovered.aggregated;
    report.kinds_discovered = discovered.targets.len();

    let watch_set = discover::watch_set(&discovered);
    if watch_set.is_empty() {
        return Err(DataError::NothingWatchable);
    }

    let probe_namespaces = if options.probe_namespaces.is_empty() {
        vec![default_namespace.to_string()]
    } else {
        options.probe_namespaces.clone()
    };
    let at_probe = Instant::now();
    let access = rbac::probe(&client, &watch_set, &probe_namespaces).await;
    report.probe_ms = ms(at_probe);
    report.probe_requests = access.requests;
    report.probe_degraded = access.degraded;
    report.kinds_unanswered = access.unanswered();
    report.probed_namespaces = access.namespaces().map(str::to_string).collect();

    let attach = attach_kinds(&discovered);
    let pass_through: Vec<KindId> = watch_set
        .iter()
        .filter(|w| w.pass_through)
        .map(|w| w.target.id)
        .collect();

    let (tx, mut rx) = tokio::sync::mpsc::channel(INTERNAL_QUEUE);
    let mut expected: HashMap<KindId, usize> = HashMap::new();
    for want in &watch_set {
        let scope = access.scope_for(&want.target);
        // Counted from the requests actually planned, not from the scope: a
        // cluster-scoped kind collapses a per-namespace grant back to one request,
        // and reporting otherwise would overstate how restricted we are.
        report.namespaced_streams += watch::stream_scopes(want, &scope)
            .iter()
            .filter(|ns| ns.is_some())
            .count();
        let streams = watch::streams_for(&client, want, &scope, attach);
        if streams.is_empty() {
            continue;
        }
        report.kinds_watched += 1;
        report.streams += streams.len();
        expected.insert(want.target.id, streams.len());
        for stream in streams {
            let kind = want.target.id;
            let tx = tx.clone();
            tokio::spawn(watch::drive(kind, stream, tx));
        }
    }
    // The collector's own handle is dropped so the channel closes when every task
    // has ended, which is how a fully-forbidden cluster terminates rather than
    // waiting out the timeout.
    drop(tx);

    let at_list = Instant::now();
    let mut store = Store::new(pass_through.clone());
    // Per kind: how many of its streams have settled, and whether any of them
    // actually listed. Both are needed, because a stream that gave up settles too.
    let mut settled: Settled = HashMap::new();
    let deadline = Instant::now() + options.sync_timeout;
    loop {
        if expected
            .iter()
            .all(|(kind, n)| streams_settled(&settled, *kind) >= *n)
        {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let message = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(m)) => m,
            // Every task ended, or the timeout expired: assemble what arrived.
            Ok(None) | Err(_) => break,
        };
        apply(
            message,
            &mut store,
            &mut settled,
            &mut report.desyncs,
            &metrics,
        );
    }
    report.list_ms = ms(at_list);
    report.unsettled = expected
        .iter()
        .filter(|(kind, n)| streams_settled(&settled, **kind) < **n)
        .map(|(kind, _)| *kind)
        .collect();
    report.unsettled.sort_by_key(|k| k.0);
    report.objects_held = store.len();

    let at_assemble = Instant::now();
    let assembled = assemble::assemble(&store, &mut catalog);
    report.assemble_ms = ms(at_assemble);
    report.assemble = assembled.stats;

    let mut events = assembled.events;
    for (kind, verdict) in verdicts(&discovered, &watch_set, &access) {
        events.push(IngestEvent::Capability { kind, verdict });
    }
    for (kind, reason) in &report.desyncs {
        events.push(IngestEvent::Desync {
            kind: *kind,
            reason: *reason,
        });
    }
    // Only a kind that actually listed may claim `Synced`: it is what lets the UI
    // tell "this kind holds nothing" from "this kind never loaded", and claiming
    // it for a denied kind is the exact lie the contract exists to prevent.
    for (kind, n) in &expected {
        let all_settled = streams_settled(&settled, *kind) >= *n;
        // `listed` is the load-bearing half: a stream that was denied settles as
        // well, and letting that claim Synced would tell the UI a forbidden kind is
        // genuinely empty, which is the exact lie the contract exists to prevent.
        let listed = settled
            .get(kind)
            .map(|(_, listed)| *listed)
            .unwrap_or(false);
        if all_settled && listed && !report.unsettled.contains(kind) {
            events.push(IngestEvent::Synced { kind: *kind });
        }
    }
    report.total_ms = ms(started);

    let snapshot = catalog.clone();
    // The live phase: the same message handling, now emitting into the sink. The
    // world cannot fold deltas yet, so these are counted and delivered rather than
    // applied; that is phase D's job, and the events it needs are already flowing.
    tokio::spawn(forward_live(
        rx,
        store,
        catalog,
        assembled.index,
        sink,
        metrics,
    ));

    Ok(Sync {
        events,
        catalog: snapshot,
        report,
    })
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// The attachment kind ids, taken from discovery rather than assumed, so a cluster
/// that somehow serves them at another version still resolves references.
fn attach_kinds(discovered: &Discovered) -> AttachKinds {
    let id =
        |kind: &str, fallback: KindId| discovered.find("", kind).map(|t| t.id).unwrap_or(fallback);
    AttachKinds {
        config_map: id("ConfigMap", KindId::CONFIG_MAP),
        secret: id("Secret", KindId::SECRET),
        volume: id("PersistentVolumeClaim", KindId::VOLUME),
    }
}

/// How many of a kind's streams have settled, and whether any of them listed.
type Settled = HashMap<KindId, (usize, bool)>;

fn streams_settled(settled: &Settled, kind: KindId) -> usize {
    settled.get(&kind).map(|(n, _)| *n).unwrap_or(0)
}

/// One object changing, as the store now sees it.
struct Change {
    op: Op,
    uid: Arc<str>,
    /// The object as it was, for a delete: the store no longer holds it, and
    /// guessing the kind of what went away would put a Deployment on the map as a
    /// pod.
    removed: Option<Box<Staged>>,
}

fn apply(
    message: Message,
    store: &mut Store,
    settled: &mut Settled,
    desyncs: &mut Vec<(KindId, DesyncReason)>,
    metrics: &IngestMetrics,
) -> Option<Change> {
    match message {
        Message::Apply { staged, .. } => {
            metrics.applies.fetch_add(1, Ordering::Relaxed);
            let uid = staged.uid.clone();
            let op = if store.get(&uid).is_some() {
                Op::Modified
            } else {
                Op::Added
            };
            store.apply(*staged);
            Some(Change {
                op,
                uid,
                removed: None,
            })
        }
        Message::Delete { uid, .. } => {
            metrics.deletes.fetch_add(1, Ordering::Relaxed);
            let removed = store.remove(&uid).map(Box::new);
            Some(Change {
                op: Op::Deleted,
                uid,
                removed,
            })
        }
        Message::Settled { kind, listed } => {
            let entry = settled.entry(kind).or_insert((0, false));
            entry.0 += 1;
            entry.1 |= listed;
            None
        }
        Message::Desync { kind, reason } => {
            metrics.desyncs.fetch_add(1, Ordering::Relaxed);
            if !desyncs.contains(&(kind, reason)) {
                desyncs.push((kind, reason));
            }
            None
        }
    }
}

/// Forwards live events after the initial sync.
///
/// Parents come from the index the assembly built, so an object that changes is
/// reported under the owner it was placed beneath. A genuinely new object whose
/// parent the index has never seen is held in the store and skipped rather than
/// emitted as an orphan: the world's fold asserts on orphans, and inventing a
/// parent would be worse than waiting for the phase that can place it.
async fn forward_live(
    mut rx: tokio::sync::mpsc::Receiver<Message>,
    mut store: Store,
    mut catalog: Catalog,
    index: Index,
    sink: EventSink,
    metrics: Arc<IngestMetrics>,
) {
    let mut settled: Settled = HashMap::new();
    let mut desyncs = Vec::new();
    while let Some(message) = rx.recv().await {
        // A desync is passed straight through: it describes the stream, not an
        // object, so there is nothing to look up.
        let forward = match &message {
            Message::Desync { kind, reason } => Some(IngestEvent::Desync {
                kind: *kind,
                reason: *reason,
            }),
            _ => None,
        };
        let changed = apply(message, &mut store, &mut settled, &mut desyncs, &metrics);
        let event = match forward {
            Some(e) => Some(e),
            None => changed.and_then(|c| live_event(&store, &index, &mut catalog, &c)),
        };
        let Some(event) = event else { continue };
        if sink.send(event).is_err() {
            // The consumer is gone, which happens on shutdown.
            return;
        }
        metrics.forwarded.fetch_add(1, Ordering::Relaxed);
    }
}

/// Builds one live event, or nothing if it cannot be placed.
///
/// An object whose parent the index has never seen is held in the store and skipped
/// rather than emitted as an orphan: the world's fold asserts on orphans, and
/// inventing a parent would be worse than waiting for the phase that can place it.
/// A pass-through owner is held to the same rule about itself: it is emitted only
/// where the sync drew a card for it.
fn live_event(
    store: &Store,
    index: &Index,
    catalog: &mut Catalog,
    change: &Change,
) -> Option<IngestEvent> {
    // For a delete the store no longer holds the object, so the message's own copy
    // is the only description of what went away.
    let staged: &Staged = match &change.removed {
        Some(removed) => removed,
        None => store.get(&change.uid)?,
    };
    let uid = &*change.uid;
    let op = change.op;
    let (parent, payload) = match &staged.detail {
        Detail::Scope => (None, Payload::Scope),
        Detail::Owner { tool } => {
            // A pass-through kind is watched to resolve ownership, so it has a card
            // only where assembly promoted one for having nothing above it. Emitting
            // the rest would stand a ReplicaSet next to its own Deployment the first
            // time a rolling update touched it, which is the doubling pass-through
            // exists to prevent. One promoted after the sync waits in the store,
            // like every other object the index cannot place.
            if store.is_pass_through(staged.kind) && !index.emitted_owner(uid) {
                return None;
            }
            (
                Some(index.scope_uid(&staged.namespace)?.clone()),
                Payload::Owner {
                    kind: staged.kind,
                    tool: *tool,
                    depends_on: live_depends_on(index, staged),
                },
            )
        }
        Detail::Instance { reason, .. } => (
            Some(index.parent_of(uid)?.clone()),
            Payload::Instance {
                state: State {
                    severity: reason.severity,
                    reason: catalog.intern_reason(&reason.display),
                },
            },
        ),
        Detail::Attached { detail, .. } => (
            Some(
                index
                    .attachment_owner(staged.kind, &staged.namespace, &staged.name)?
                    .clone(),
            ),
            Payload::Attached {
                kind: staged.kind,
                detail: detail.clone(),
            },
        ),
    };
    Some(IngestEvent::Resource(ResourceEvent {
        kind: staged.kind,
        uid: staged.uid.clone(),
        namespace: staged.namespace.clone(),
        name: staged.name.clone(),
        resource_version: staged.resource_version,
        parent,
        op,
        payload,
    }))
}

/// What one live owner depends on, by the rule the initial sync used: its
/// controlling reference, and only where that names an owner with a card.
///
/// Not the whole truth, and the gap is one-sided. An owner the live phase itself
/// emitted is not in the index, so an edge onto it is missing here rather than
/// pointing at a workload the world does not have — the same reason that owner's
/// instances are not placed either, and the same phase closes both. What follows for
/// a phase-D apply: it may add every edge this names, and must not read a short list
/// as the owner having lost the rest.
fn live_depends_on(index: &Index, staged: &Staged) -> Vec<Arc<str>> {
    staged
        .controller
        .as_ref()
        .filter(|c| index.emitted_owner(&c.uid))
        .map(|c| vec![c.uid.clone()])
        .unwrap_or_default()
}

/// The capability verdict for every kind worth reporting.
///
/// Watch-set kinds are probe-verified. Everything else the cluster serves gets the
/// rules-review answer when there is one, which costs no extra request and is what
/// makes an arbitrary CRD show as readable or denied rather than as nothing at all.
/// Kinds the watch set names but the cluster does not serve are reported
/// [`Capability::Absent`], because invisible is the correct rendering of absent.
pub fn verdicts(
    discovered: &Discovered,
    watch_set: &[WatchTarget],
    access: &Access,
) -> Vec<(KindId, Capability)> {
    let mut out: Vec<(KindId, Capability)> = Vec::new();
    let mut seen: Vec<KindId> = Vec::new();
    for want in watch_set {
        out.push((want.target.id, access.verdict(&want.target)));
        seen.push(want.target.id);
    }
    for target in &discovered.targets {
        if seen.contains(&target.id) {
            continue;
        }
        if !target.listable || !target.watchable {
            out.push((target.id, Capability::Absent));
            continue;
        }
        if !target.namespaced {
            // Cluster-scoped and outside the watch set: a rules review cannot
            // answer for it and an access review per kind would be a request per
            // kind, so it stays unprobed rather than guessed at.
            continue;
        }
        let probed: Vec<&rbac::RuleSet> = access
            .namespaces()
            .filter_map(|ns| access.rules(ns))
            .collect();
        if probed.is_empty() {
            // Nothing was asked about this kind, so nothing is claimed about it.
            continue;
        }
        let allowed = probed.iter().any(|rules| {
            rules.allows_reflection(target.group(), target.plural()) || rules.is_incomplete()
        });
        out.push((
            target.id,
            if allowed {
                Capability::Watchable
            } else {
                Capability::Forbidden
            },
        ));
    }
    out.sort_by_key(|(kind, _)| kind.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::{Controller, Reason};
    use k10s_core::{Intake, Role, Severity, ToolId, replay};

    #[test]
    fn the_sink_carries_contract_events_to_an_intake() {
        // The regression this guards: the sender used to be discarded, so nothing a
        // producer said could reach a consumer.
        let (tx, rx) = crossbeam_channel::unbounded();
        let plane = spawn(tx).expect("build the runtime");

        for event in replay::initial_sync().events {
            plane.events().send(event).expect("sink is live");
        }

        let mut intake = Intake::new();
        while let Ok(event) = rx.try_recv() {
            intake.push(event);
        }
        let drained = intake.drain();
        assert_eq!(
            drained
                .iter()
                .filter(|e| matches!(e, IngestEvent::Resource(_)))
                .count(),
            4
        );
        assert_eq!(plane.metrics(), MetricsSnapshot::default());
    }

    /// A pass-through kind that is not a built-in, so nothing below can pass on a
    /// compiled-in id.
    const RS: KindId = KindId(9_500);

    fn object(kind: KindId, role: Role, uid: &str, name: &str) -> Staged {
        let namespace = if role == Role::Scope { "" } else { "prod" };
        Staged {
            kind,
            role,
            uid: uid.into(),
            namespace: namespace.into(),
            name: name.into(),
            resource_version: 7,
            controller: None,
            detail: match role {
                Role::Scope => Detail::Scope,
                Role::Owner => Detail::Owner { tool: ToolId::NONE },
                _ => Detail::Instance {
                    reason: Reason {
                        severity: Severity::Ok,
                        display: "Running".into(),
                    },
                    labels: Vec::new(),
                    refs: Vec::new(),
                },
            },
        }
    }

    fn under(mut staged: Staged, controller: Controller) -> Staged {
        staged.controller = Some(controller);
        staged
    }

    fn ctrl(uid: &str, kind: &str, name: &str, api_version: &str) -> Controller {
        Controller {
            uid: uid.into(),
            kind: kind.into(),
            name: name.into(),
            api_version: api_version.into(),
        }
    }

    /// What a cold start over these objects would leave to the live phase.
    fn after_sync(objects: Vec<Staged>) -> (Store, Catalog, assemble::Assembled) {
        let mut store = Store::new(vec![RS]);
        for object in objects {
            store.apply(object);
        }
        let mut catalog = Catalog::new();
        let assembled = assemble::assemble(&store, &mut catalog);
        (store, catalog, assembled)
    }

    fn modified(uid: &str) -> Change {
        Change {
            op: Op::Modified,
            uid: uid.into(),
            removed: None,
        }
    }

    fn resource_event(event: Option<IngestEvent>) -> ResourceEvent {
        match event.expect("an event was built") {
            IngestEvent::Resource(r) => r,
            other => panic!("expected a resource event, got {other:?}"),
        }
    }

    #[test]
    fn a_live_replicaset_is_emitted_only_where_the_sync_gave_it_a_card() {
        // Both ReplicaSets change on a rolling update. Emitting the one under the
        // Deployment would stand a second card beside it and double the Deployment
        // on the map, which is what pass-through exists to prevent; suppressing the
        // hand-rolled one would freeze the only card its pods have.
        let (mut store, mut catalog, assembled) = after_sync(vec![
            object(KindId::NAMESPACE, Role::Scope, "ns-1", "prod"),
            object(KindId::DEPLOYMENT, Role::Owner, "dep-1", "api"),
            under(
                object(RS, Role::Owner, "rs-1", "api-abc"),
                ctrl("dep-1", "Deployment", "api", "apps/v1"),
            ),
            object(RS, Role::Owner, "rs-2", "hand-rolled"),
            under(
                object(KindId::POD, Role::Instance, "pod-1", "api-abc-1"),
                ctrl("rs-1", "ReplicaSet", "api-abc", "apps/v1"),
            ),
            under(
                object(KindId::POD, Role::Instance, "pod-2", "hand-rolled-1"),
                ctrl("rs-2", "ReplicaSet", "hand-rolled", "apps/v1"),
            ),
        ]);
        let index = &assembled.index;

        assert!(
            live_event(&store, index, &mut catalog, &modified("rs-1")).is_none(),
            "a ReplicaSet under a Deployment has no card to update"
        );
        // The promoted one keeps updating, under its namespace and as its own kind.
        let promoted = resource_event(live_event(&store, index, &mut catalog, &modified("rs-2")));
        assert_eq!(promoted.parent.as_deref(), Some("ns-1"));
        assert!(matches!(promoted.payload, Payload::Owner { kind, .. } if kind == RS));
        // The guard is about pass-through, not about owners.
        let dep = resource_event(live_event(&store, index, &mut catalog, &modified("dep-1")));
        assert_eq!(&*dep.uid, "dep-1");
        // And the pods on both sides are still placed, which is the whole point of
        // watching a ReplicaSet nobody draws.
        for (pod, parent) in [("pod-1", "dep-1"), ("pod-2", "rs-2")] {
            let event = resource_event(live_event(&store, index, &mut catalog, &modified(pod)));
            assert_eq!(event.parent.as_deref(), Some(parent), "{pod}");
        }

        // A delete is answered from the copy that went away rather than the store,
        // and a Deleted for a card the world never had is as wrong as an Added.
        let removed = store.remove("rs-1").map(Box::new);
        assert!(removed.is_some(), "the store held it");
        let vanished = Change {
            op: Op::Deleted,
            uid: "rs-1".into(),
            removed,
        };
        assert!(live_event(&store, index, &mut catalog, &vanished).is_none());
    }

    #[test]
    fn a_live_owner_repeats_the_dependency_the_sync_gave_it() {
        // One producer describing the same Job two ways is the defect: the sync
        // names its CronJob and the live payload used to say the Job depends on
        // nothing, so a phase-D apply that trusted it would erase the edge on the
        // Job's first status change.
        let (store, mut catalog, assembled) = after_sync(vec![
            object(KindId::NAMESPACE, Role::Scope, "ns-1", "prod"),
            object(KindId::CRON_JOB, Role::Owner, "cj-1", "nightly"),
            under(
                object(KindId::JOB, Role::Owner, "job-1", "nightly-123"),
                ctrl("cj-1", "CronJob", "nightly", "batch/v1"),
            ),
            // A Job under a controller nobody drew: an operator's CRD outside the
            // watch set, with no pod to discover it from.
            under(
                object(KindId::JOB, Role::Owner, "job-2", "adhoc"),
                ctrl("ro-1", "Rollout", "web", "argoproj.io/v1alpha1"),
            ),
        ]);
        let index = &assembled.index;
        let from_sync = |uid: &str| {
            assembled
                .events
                .iter()
                .find_map(|e| match e {
                    IngestEvent::Resource(r) if &*r.uid == uid => match &r.payload {
                        Payload::Owner { depends_on, .. } => Some(depends_on.clone()),
                        _ => None,
                    },
                    _ => None,
                })
                .expect("the sync emitted this owner")
        };
        let live = |uid: &str, catalog: &mut Catalog| {
            let event = live_event(&store, index, catalog, &modified(uid));
            match resource_event(event).payload {
                Payload::Owner { depends_on, .. } => depends_on,
                other => panic!("expected an owner payload, got {other:?}"),
            }
        };

        assert_eq!(live("job-1", &mut catalog), vec![Arc::<str>::from("cj-1")]);
        assert_eq!(
            live("job-1", &mut catalog),
            from_sync("job-1"),
            "one producer, one answer"
        );
        // An endpoint nothing drew is not named at all: a uid the world has no
        // workload for is a dangling edge, which is worse than a missing one.
        assert!(live("job-2", &mut catalog).is_empty());
        assert!(from_sync("job-2").is_empty());
    }

    fn resource(
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
    ) -> kube::discovery::ApiResource {
        kube::discovery::ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: if group.is_empty() {
                version.to_string()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.to_string(),
            plural: plural.to_string(),
        }
    }

    fn caps(scope: kube::discovery::Scope, ops: &[&str]) -> kube::discovery::ApiCapabilities {
        kube::discovery::ApiCapabilities {
            scope,
            subresources: Vec::new(),
            operations: ops.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn cluster() -> (Discovered, Catalog) {
        use kube::discovery::Scope;
        let mut catalog = Catalog::new();
        let items = [
            (
                resource("", "v1", "Namespace", "namespaces"),
                caps(Scope::Cluster, &["list", "watch"]),
            ),
            (
                resource("", "v1", "Pod", "pods"),
                caps(Scope::Namespaced, &["list", "watch"]),
            ),
            (
                resource("apps", "v1", "Deployment", "deployments"),
                caps(Scope::Namespaced, &["list", "watch"]),
            ),
            // Served but not watchable: absent, because it can never populate.
            (
                resource("", "v1", "ComponentStatus", "componentstatuses"),
                caps(Scope::Cluster, &["list"]),
            ),
            // A CRD outside the watch set, answered by the rules review.
            (
                resource("argoproj.io", "v1alpha1", "Application", "applications"),
                caps(Scope::Namespaced, &["list", "watch"]),
            ),
        ];
        let targets = items
            .iter()
            .map(|(r, c)| discover::intern(&mut catalog, r.clone(), c))
            .collect();
        (
            Discovered {
                targets,
                server_version: Some("v1.32.1".into()),
                aggregated: true,
            },
            catalog,
        )
    }

    #[test]
    fn an_unprobed_cluster_reports_the_watch_set_and_nothing_it_did_not_check() {
        let (discovered, _) = cluster();
        let watch_set = discover::watch_set(&discovered);
        let verdicts = verdicts(&discovered, &watch_set, &Access::unprobed());
        // Every watch-set kind is attempted, so every one is Watchable.
        for want in &watch_set {
            assert_eq!(
                verdicts
                    .iter()
                    .find(|(k, _)| *k == want.target.id)
                    .map(|(_, v)| *v),
                Some(Capability::Watchable),
                "{}",
                want.target.kind()
            );
        }
        // A served-but-unwatchable kind is absent whatever the probe said.
        let cs = discovered.find("", "ComponentStatus").unwrap();
        assert_eq!(
            verdicts.iter().find(|(k, _)| *k == cs.id).map(|(_, v)| *v),
            Some(Capability::Absent)
        );
        // And an unprobed CRD gets no verdict rather than an invented one.
        let app = discovered.find("argoproj.io", "Application").unwrap();
        assert!(verdicts.iter().all(|(k, _)| *k != app.id));
    }

    #[test]
    fn verdicts_are_sorted_and_never_duplicated() {
        let (discovered, _) = cluster();
        let watch_set = discover::watch_set(&discovered);
        let verdicts = verdicts(&discovered, &watch_set, &Access::unprobed());
        let mut kinds: Vec<u32> = verdicts.iter().map(|(k, _)| k.0).collect();
        let before = kinds.len();
        assert!(kinds.windows(2).all(|w| w[0] <= w[1]), "not sorted");
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), before, "a kind got two verdicts");
    }

    #[test]
    fn every_reason_the_mapping_can_name_is_interned_before_the_watches_start() {
        // Why it matters: the live forwarder holds its own catalog, so an id it
        // allocates after the snapshot is taken would not resolve in the app's.
        // Pre-interning the whole vocabulary bounds that to reasons nobody has a
        // severity for, which display as Unknown, which is what they are.
        let mut catalog = Catalog::new();
        for reason in mapping::known_reasons() {
            catalog.intern_reason(reason);
        }
        let kinds_before = catalog.kind_count();
        let mut live = catalog.clone();
        for reason in mapping::known_reasons() {
            let a = catalog.intern_reason(reason);
            let b = live.intern_reason(reason);
            assert_eq!(a, b, "{reason} interned differently in two catalogs");
            assert_eq!(catalog.reason_display(a), reason);
        }
        assert_eq!(catalog.kind_count(), kinds_before, "no kind was touched");
        // Built-in reasons keep their compiled-in ids, which the map's
        // presentation tables index by.
        assert_eq!(
            catalog.intern_reason("CrashLoopBackOff"),
            k10s_core::ReasonId::CRASH_LOOP_BACK_OFF
        );
    }

    #[test]
    fn a_report_summarises_without_naming_an_object() {
        let report = ClusterReport {
            context: Some("prod".into()),
            cluster_url: "https://prod.example:6443".into(),
            server_version: Some("v1.32.1".into()),
            kinds_discovered: 210,
            kinds_watched: 12,
            streams: 12,
            objects_held: 4210,
            total_ms: 812.4,
            ..Default::default()
        };
        let text = report.summary();
        assert!(text.contains("prod"));
        assert!(text.contains("v1.32.1"));
        assert!(text.contains("210"));
        assert!(text.contains("812"));

        // In-cluster has no context and must still read.
        let anon = ClusterReport {
            cluster_url: "https://10.0.0.1".into(),
            ..Default::default()
        };
        assert!(anon.summary().contains("in-cluster"));
    }

    #[test]
    fn attachment_kinds_come_from_discovery_and_fall_back_to_the_builtins() {
        let (discovered, _) = cluster();
        let kinds = attach_kinds(&discovered);
        // This fixture serves neither, so the built-in ids stand.
        assert_eq!(kinds.secret, KindId::SECRET);
        assert_eq!(kinds.config_map, KindId::CONFIG_MAP);
        assert_eq!(kinds.volume, KindId::VOLUME);
    }

    #[test]
    fn errors_say_what_they_mean() {
        assert!(
            DataError::NothingWatchable
                .to_string()
                .contains("none of the kinds")
        );
        assert!(
            DataError::Discovery("connection refused".into())
                .to_string()
                .contains("connection refused")
        );
        assert!(
            DataError::from(ConnectError::NoSource)
                .to_string()
                .contains("KUBECONFIG")
        );
    }
}
