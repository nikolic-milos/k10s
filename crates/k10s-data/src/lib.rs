//! The Kubernetes data plane: reads, apply, and named day-2 writes.
//!
//! Everything here reaches the cluster through the API server with an
//! ordinary kubeconfig: no operator, agent, CRD, or in-cluster footprint of
//! any kind, ever. Watches recover from 410 by relisting and reaping, RBAC is
//! probed upfront into a tri-state capability verdict so denial degrades into
//! a label rather than an empty map, and Secrets are projected to metadata
//! structurally -- a secret value must never reach a snapshot, a log line, or
//! an error message. Error text shown to a person is redaction-filtered
//! because exec credential plugins leak their environment through `Debug`.
//! The `kube::Client` seam is a `tower` service, which is what lets
//! `tests/scripted_apiserver.rs` be the API server.

pub mod alertmanager;
pub mod apply;
pub mod argo;
pub mod assemble;
pub mod browse;
pub mod cilium;
pub mod cilium_control;
pub mod cnpg;
pub mod connect;
pub mod day2;
pub mod describe;
pub mod discover;
pub mod eso;
pub mod exec;
pub mod falco;
pub mod flux;
pub mod forward;
pub mod gateway;
pub mod grafana;
pub mod harbor;
pub mod helm;
pub mod helm_reveal;
pub mod ingress;
pub mod inspect;
pub mod kargo;
pub mod kyverno;
pub mod logs;
pub mod loki;
pub mod manifest;
#[cfg(test)]
mod manifest_test;
pub mod mapping;
pub mod mesh;
pub mod metrics;
pub mod netpol;
pub mod nodes;
pub mod oci;
pub mod openapi;
pub mod otel;
pub mod overlay;
pub mod policy;
mod projection;
pub mod prom;
pub mod proxies;
pub mod pss;
pub mod rbac;
pub mod rbac_index;
pub mod reach;
pub mod read;
mod served;
pub mod talos;
pub mod tetragon;
pub mod traces;
pub mod traefik;
pub mod vault;
pub mod velero;
pub mod watch;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use k10s_core::{Capability, Catalog, DesyncReason, IngestEvent, KindId, Op, Role};

use assemble::{AssembleStats, Store};
use connect::{ConnectError, Connector, Env};
use discover::{Discovered, WatchTarget};
use mapping::AttachKinds;
use projection::{Change, Projection};
use rbac::Access;
use watch::Message;

pub type EventSink = Sender<IngestEvent>;

pub const DEFAULT_EVENT_SINK_CAPACITY: usize = 8_192;

/// How long the first publish waits for attached listings once the geometry
/// has settled: three frames at 60 Hz.
const ATTACHMENT_GRACE: Duration = Duration::from_millis(50);

const INTERNAL_QUEUE: usize = DEFAULT_EVENT_SINK_CAPACITY;

#[derive(Debug, Clone)]
pub struct Options {
    pub context: Option<String>,
    /// One kubeconfig to use instead of whatever the environment points at. The
    /// launch screen can name a file `KUBECONFIG` and `~/.kube/config` do not,
    /// and a context picked out of that file has to be connected through it.
    pub kubeconfig: Option<std::path::PathBuf>,
    pub probe_namespaces: Vec<String>,
    pub sync_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            context: None,
            kubeconfig: None,
            probe_namespaces: Vec::new(),
            sync_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClusterReport {
    pub context: Option<String>,
    pub cluster_url: String,
    pub server_version: Option<String>,
    pub aggregated_discovery: bool,
    pub kinds_discovered: usize,
    pub kinds_watched: usize,
    pub streams: usize,
    pub namespaced_streams: usize,
    pub probe_requests: u32,
    pub probe_degraded: bool,
    pub kinds_unanswered: usize,
    pub probed_namespaces: Vec<String>,
    pub namespaces_unanswered: Vec<String>,
    pub objects_held: usize,
    pub assemble: AssembleStats,
    pub desyncs: Vec<(KindId, DesyncReason)>,
    pub unsettled: Vec<KindId>,
    /// Attached kinds whose listing had not finished when the geometry kinds
    /// had. The first publish went ahead without them; their streams keep
    /// running and each lands as one batch when its listing settles.
    pub deferred: Vec<KindId>,
    pub connect_ms: f64,
    pub discover_ms: f64,
    pub probe_ms: f64,
    pub list_ms: f64,
    pub assemble_ms: f64,
    pub total_ms: f64,
}

impl ClusterReport {
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

pub struct Sync {
    pub events: Vec<IngestEvent>,
    pub catalog: Catalog,
    pub report: ClusterReport,
    pub inspector: inspect::Inspector,
    pub reader: read::Reader,
}

#[derive(Debug, Default)]
pub struct IngestMetrics {
    pub applies: AtomicU64,
    pub deletes: AtomicU64,
    pub desyncs: AtomicU64,
    pub forwarded: AtomicU64,
}

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

#[derive(Debug)]
pub enum DataError {
    Connect(ConnectError),
    Discovery(String),
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

    pub fn events(&self) -> &EventSink {
        &self.events
    }

    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn sync(&self, options: &Options) -> Result<Sync, DataError> {
        let metrics = self.metrics.clone();
        let sink = self.events.clone();
        self.runtime
            .block_on(async move { cold_start(options, sink, metrics).await })
    }

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
    let mut connector = match &options.kubeconfig {
        Some(path) => Connector::from_file(path)?,
        None => Connector::load(&Env::from_process())?,
    };
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
    report.namespaces_unanswered = access.unanswered_namespaces().map(str::to_string).collect();

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
    drop(tx);

    let at_list = Instant::now();
    let mut store = Store::new(pass_through.clone());
    let mut settled: Settled = HashMap::new();
    let deadline = Instant::now() + options.sync_timeout;
    // The first frame is geometry: scopes, owners, instances. Attachments
    // decorate it. Gating the first publish on every attached listing as well
    // makes the slowest ConfigMap or Secret list the first frame, and on a
    // cluster with thousands of either that is the frame. So the gate waits
    // for the geometry kinds, or the deadline; attached kinds still listing
    // are deferred, and forward_live folds each in as one batch, with one
    // reconcile, when its listing settles.
    let first_frame: HashSet<KindId> = watch_set
        .iter()
        .filter(|want| want.target.role != Role::Attached)
        .map(|want| want.target.id)
        .collect();
    loop {
        if expected
            .iter()
            .all(|(kind, n)| !first_frame.contains(kind) || streams_settled(&settled, *kind) >= *n)
        {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let message = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(m)) => m,
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
    // Attached listings that finish within a few frames of the geometry ride
    // the first publish; on a quick server that is all of them, and the live
    // channel stays quiet exactly as before. Deferral is for the ones that do
    // not: a first frame that waits longer for its decoration than for its
    // geometry is the situation it exists for.
    let grace = Instant::now() + ATTACHMENT_GRACE;
    loop {
        if expected
            .iter()
            .all(|(kind, n)| streams_settled(&settled, *kind) >= *n)
        {
            break;
        }
        let remaining = grace
            .min(deadline)
            .saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let message = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(m)) => m,
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
        .filter(|(kind, n)| first_frame.contains(kind) && streams_settled(&settled, **kind) < **n)
        .map(|(kind, _)| *kind)
        .collect();
    report.unsettled.sort_by_key(|k| k.0);
    let deferred: HashSet<KindId> = expected
        .iter()
        .filter(|(kind, n)| !first_frame.contains(kind) && !kind_synced(&settled, **kind, **n))
        .map(|(kind, _)| *kind)
        .collect();
    report.deferred = deferred.iter().copied().collect();
    report.deferred.sort_by_key(|k| k.0);
    report.objects_held = store.len();

    let at_assemble = Instant::now();
    let assembled = assemble::assemble(&store, &mut catalog);
    report.assemble_ms = ms(at_assemble);
    report.assemble = assembled.stats;
    let projection = Projection::from_assembled(&assembled);

    let mut events = assembled.events;
    let verdict_list = verdicts(&discovered, &watch_set, &access);
    let caps_list = day2_caps(&discovered, &access);
    for (kind, verdict) in &verdict_list {
        events.push(IngestEvent::Capability {
            kind: *kind,
            verdict: *verdict,
        });
    }
    for (kind, reason) in &report.desyncs {
        events.push(IngestEvent::Desync {
            kind: *kind,
            reason: *reason,
        });
    }
    for (kind, n) in &expected {
        if kind_synced(&settled, *kind, *n) {
            events.push(IngestEvent::Synced { kind: *kind });
        }
    }
    report.total_ms = ms(started);

    let snapshot = catalog.clone();
    let inspector = inspect::Inspector::new(client.clone());
    let reader = read::Reader::new(
        client.clone(),
        discovered.targets.clone(),
        &verdict_list,
        &caps_list,
    );
    tokio::spawn(forward_live(
        rx,
        store,
        catalog,
        projection,
        sink,
        metrics,
        Deferred {
            settled,
            expected,
            kinds: deferred,
            deadline,
        },
    ));

    Ok(Sync {
        events,
        catalog: snapshot,
        report,
        inspector,
        reader,
    })
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn attach_kinds(discovered: &Discovered) -> AttachKinds {
    let id =
        |kind: &str, fallback: KindId| discovered.find("", kind).map(|t| t.id).unwrap_or(fallback);
    AttachKinds {
        config_map: id("ConfigMap", KindId::CONFIG_MAP),
        secret: id("Secret", KindId::SECRET),
        volume: id("PersistentVolumeClaim", KindId::VOLUME),
    }
}

type Settled = HashMap<KindId, (usize, bool)>;

fn streams_settled(settled: &Settled, kind: KindId) -> usize {
    settled.get(&kind).map(|(n, _)| *n).unwrap_or(0)
}

fn kind_synced(settled: &Settled, kind: KindId, streams: usize) -> bool {
    settled
        .get(&kind)
        .is_some_and(|(count, listed)| *count >= streams && *listed)
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
            let before = store.replace(*staged).map(Box::new);
            let op = if before.is_some() {
                Op::Modified
            } else {
                Op::Added
            };
            Some(Change { op, uid, before })
        }
        Message::Delete { uid, .. } => {
            metrics.deletes.fetch_add(1, Ordering::Relaxed);
            let before = Box::new(store.remove(&uid)?);
            Some(Change {
                op: Op::Deleted,
                uid,
                before: Some(before),
            })
        }
        Message::Settled { kind, listed } => {
            let entry = settled.entry(kind).or_insert((0, true));
            entry.0 += 1;
            entry.1 &= listed;
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

/// What the first publish left listing: attached kinds whose streams had not
/// settled when the geometry had. Their objects are held until the kind
/// settles and then applied as one batch with one reconcile, because the
/// projection re-assembles on every structural change and a listing of ten
/// thousand ConfigMaps must not cost ten thousand assemblies. The deadline is
/// the sync timeout the gate itself honoured: past it, whatever is held is
/// applied as it stands, which is exactly what a timed-out sync used to
/// publish.
struct Deferred {
    settled: Settled,
    expected: HashMap<KindId, usize>,
    kinds: HashSet<KindId>,
    deadline: Instant,
}

async fn forward_live(
    mut rx: tokio::sync::mpsc::Receiver<Message>,
    mut store: Store,
    mut catalog: Catalog,
    mut projection: Projection,
    sink: EventSink,
    metrics: Arc<IngestMetrics>,
    mut deferred: Deferred,
) {
    let mut desyncs = Vec::new();
    let mut held: HashMap<KindId, Vec<Message>> = HashMap::new();
    loop {
        let message = if deferred.kinds.is_empty() {
            rx.recv().await
        } else {
            match tokio::time::timeout_at(deferred.deadline.into(), rx.recv()).await {
                Ok(message) => message,
                Err(_) => {
                    let kinds: Vec<KindId> = deferred.kinds.drain().collect();
                    let events = release(
                        &kinds,
                        &mut held,
                        &mut store,
                        &mut catalog,
                        &mut projection,
                        &mut deferred,
                        &mut desyncs,
                        &metrics,
                    );
                    for event in events {
                        if !send_live(&sink, event).await {
                            return;
                        }
                        metrics.forwarded.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
            }
        };
        let Some(message) = message else {
            return;
        };
        let kind = message.kind();
        if deferred.kinds.contains(&kind)
            && matches!(message, Message::Apply { .. } | Message::Delete { .. })
        {
            held.entry(kind).or_default().push(message);
            continue;
        }
        let forward = match &message {
            Message::Desync { kind, reason } => Some(IngestEvent::Desync {
                kind: *kind,
                reason: *reason,
            }),
            _ => None,
        };
        let changed = apply(
            message,
            &mut store,
            &mut deferred.settled,
            &mut desyncs,
            &metrics,
        );
        let mut events = match forward {
            Some(event) => vec![event],
            None => changed
                .as_ref()
                .map(|change| projection.project(&store, &mut catalog, change))
                .unwrap_or_default(),
        };
        let streams = deferred.expected.get(&kind).copied().unwrap_or(0);
        if deferred.kinds.contains(&kind) && kind_synced(&deferred.settled, kind, streams) {
            deferred.kinds.remove(&kind);
            events.extend(release(
                &[kind],
                &mut held,
                &mut store,
                &mut catalog,
                &mut projection,
                &mut deferred,
                &mut desyncs,
                &metrics,
            ));
        }
        for event in events {
            if !send_live(&sink, event).await {
                return;
            }
            metrics.forwarded.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Apply what the named deferred kinds held, in arrival order, then reconcile
/// once. A kind whose listing completed says so with Synced; one released by
/// the deadline does not, because it is incomplete and Synced would be a lie.
#[allow(clippy::too_many_arguments)]
fn release(
    kinds: &[KindId],
    held: &mut HashMap<KindId, Vec<Message>>,
    store: &mut Store,
    catalog: &mut Catalog,
    projection: &mut Projection,
    deferred: &mut Deferred,
    desyncs: &mut Vec<(KindId, DesyncReason)>,
    metrics: &IngestMetrics,
) -> Vec<IngestEvent> {
    let mut any = false;
    for kind in kinds {
        for message in held.remove(kind).unwrap_or_default() {
            apply(message, store, &mut deferred.settled, desyncs, metrics);
            any = true;
        }
    }
    let mut events = if any {
        projection.reconcile(store, catalog)
    } else {
        Vec::new()
    };
    for kind in kinds {
        let streams = deferred.expected.get(kind).copied().unwrap_or(0);
        if kind_synced(&deferred.settled, *kind, streams) {
            events.push(IngestEvent::Synced { kind: *kind });
        }
    }
    events
}

async fn send_live(sink: &EventSink, event: IngestEvent) -> bool {
    match sink.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Disconnected(_)) => false,
        Err(TrySendError::Full(event)) => {
            let sink = sink.clone();
            matches!(
                tokio::task::spawn_blocking(move || sink.send(event)).await,
                Ok(Ok(()))
            )
        }
    }
}

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
            continue;
        }
        let probed: Vec<&rbac::RuleSet> = access
            .namespaces()
            .filter_map(|ns| access.rules(ns))
            .collect();
        if probed.is_empty() {
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

fn day2_caps(discovered: &Discovered, access: &Access) -> Vec<(KindId, day2::Caps)> {
    let mut out: Vec<(KindId, day2::Caps)> = discovered
        .targets
        .iter()
        .map(|target| (target.id, access.day2_caps(target)))
        .collect();
    out.sort_by_key(|(kind, _)| kind.0);
    out
}

#[cfg(test)]
#[path = "lib_test.rs"]
mod tests;
