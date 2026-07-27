pub mod assemble;
pub mod connect;
pub mod discover;
pub mod mapping;
mod projection;
pub mod rbac;
pub mod watch;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use k10s_core::{Capability, Catalog, DesyncReason, IngestEvent, KindId, Op};

use assemble::{AssembleStats, Store};
use connect::{ConnectError, Connector, Env};
use discover::{Discovered, WatchTarget};
use mapping::AttachKinds;
use projection::{Change, Projection};
use rbac::Access;
use watch::Message;

pub type EventSink = Sender<IngestEvent>;

pub const DEFAULT_EVENT_SINK_CAPACITY: usize = 8_192;

const INTERNAL_QUEUE: usize = DEFAULT_EVENT_SINK_CAPACITY;

#[derive(Debug, Clone)]
pub struct Options {
    pub context: Option<String>,
    pub probe_namespaces: Vec<String>,
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
    pub objects_held: usize,
    pub assemble: AssembleStats,
    pub desyncs: Vec<(KindId, DesyncReason)>,
    pub unsettled: Vec<KindId>,
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
    let projection = Projection::from_assembled(&assembled);

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
    for (kind, n) in &expected {
        let all_settled = streams_settled(&settled, *kind) >= *n;
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
    tokio::spawn(forward_live(rx, store, catalog, projection, sink, metrics));

    Ok(Sync {
        events,
        catalog: snapshot,
        report,
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
            let before = store.remove(&uid).map(Box::new);
            Some(Change {
                op: Op::Deleted,
                uid,
                before,
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

async fn forward_live(
    mut rx: tokio::sync::mpsc::Receiver<Message>,
    mut store: Store,
    mut catalog: Catalog,
    mut projection: Projection,
    sink: EventSink,
    metrics: Arc<IngestMetrics>,
) {
    let mut settled: Settled = HashMap::new();
    let mut desyncs = Vec::new();
    while let Some(message) = rx.recv().await {
        let forward = match &message {
            Message::Desync { kind, reason } => Some(IngestEvent::Desync {
                kind: *kind,
                reason: *reason,
            }),
            _ => None,
        };
        let changed = apply(message, &mut store, &mut settled, &mut desyncs, &metrics);
        let events = match forward {
            Some(event) => vec![event],
            None => changed
                .as_ref()
                .map(|change| projection.project(&store, &mut catalog, change))
                .unwrap_or_default(),
        };
        for event in events {
            if !send_live(&sink, event).await {
                return;
            }
            metrics.forwarded.fetch_add(1, Ordering::Relaxed);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::{Controller, Detail, Reason, Staged};
    use k10s_core::{Intake, Payload, ResourceEvent, Role, Severity, ToolId, replay};

    #[test]
    fn the_sink_carries_contract_events_to_an_intake() {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_bounded_sink_backpressures_without_dropping() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let first = IngestEvent::Synced { kind: KindId::POD };
        let second = IngestEvent::Synced {
            kind: KindId::NAMESPACE,
        };
        tx.send(first.clone()).expect("the sink is connected");

        let mut blocked = Box::pin(send_live(&tx, second.clone()));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut blocked)
                .await
                .is_err(),
            "a full sink must apply backpressure"
        );
        assert_eq!(rx.recv().expect("the first event remained queued"), first);
        assert!(blocked.await);
        assert_eq!(rx.recv().expect("the second event was forwarded"), second);
    }

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

    fn after_sync(objects: Vec<Staged>) -> (Store, Catalog, assemble::Assembled) {
        let mut store = Store::new(vec![RS]);
        for object in objects {
            store.apply(object);
        }
        let mut catalog = Catalog::new();
        let assembled = assemble::assemble(&store, &mut catalog);
        (store, catalog, assembled)
    }

    fn modified(store: &Store, uid: &str) -> Change {
        Change {
            op: Op::Modified,
            uid: uid.into(),
            before: store.get(uid).cloned().map(Box::new),
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
        let mut projection = Projection::from_assembled(&assembled);

        assert!(
            projection
                .project(&store, &mut catalog, &modified(&store, "rs-1"))
                .is_empty(),
            "a ReplicaSet under a Deployment has no card to update"
        );
        let promoted = resource_event(
            projection
                .project(&store, &mut catalog, &modified(&store, "rs-2"))
                .into_iter()
                .next(),
        );
        assert_eq!(promoted.parent.as_deref(), Some("ns-1"));
        assert!(matches!(promoted.payload, Payload::Owner { kind, .. } if kind == RS));
        let dep = resource_event(
            projection
                .project(&store, &mut catalog, &modified(&store, "dep-1"))
                .into_iter()
                .next(),
        );
        assert_eq!(&*dep.uid, "dep-1");
        for (pod, parent) in [("pod-1", "dep-1"), ("pod-2", "rs-2")] {
            let event = resource_event(
                projection
                    .project(&store, &mut catalog, &modified(&store, pod))
                    .into_iter()
                    .next(),
            );
            assert_eq!(event.parent.as_deref(), Some(parent), "{pod}");
        }

        let before = store.remove("rs-1").map(Box::new);
        assert!(before.is_some(), "the store held it");
        let vanished = Change {
            op: Op::Deleted,
            uid: "rs-1".into(),
            before,
        };
        let reparented = projection.project(&store, &mut catalog, &vanished);
        assert!(
            reparented.iter().any(|event| matches!(
                event,
                IngestEvent::Resource(resource)
                    if resource.uid.as_ref() == "rs-1" && resource.op == Op::Added
            )),
            "the still-running pod keeps a synthetic card for its now-unwatched controller"
        );
        assert!(
            reparented.iter().any(|event| matches!(
                event,
                IngestEvent::Resource(resource)
                    if resource.uid.as_ref() == "pod-1"
                        && resource.op == Op::Modified
                        && resource.parent.as_deref() == Some("rs-1")
            )),
            "the dependent pod follows the rebuilt index"
        );
    }

    #[test]
    fn a_live_namespace_makes_its_waiting_topology_visible_parent_first() {
        let (mut store, mut catalog, assembled) = after_sync(vec![
            object(KindId::DEPLOYMENT, Role::Owner, "dep-1", "api"),
            under(
                object(KindId::POD, Role::Instance, "pod-1", "api-1"),
                ctrl("dep-1", "Deployment", "api", "apps/v1"),
            ),
        ]);
        assert!(assembled.events.is_empty(), "the scope is not visible yet");
        let mut projection = Projection::from_assembled(&assembled);

        let namespace = object(KindId::NAMESPACE, Role::Scope, "ns-1", "prod");
        store.apply(namespace.clone());
        let events = projection.project(
            &store,
            &mut catalog,
            &Change {
                op: Op::Added,
                uid: namespace.uid,
                before: None,
            },
        );
        let resources: Vec<&ResourceEvent> = events
            .iter()
            .filter_map(|event| match event {
                IngestEvent::Resource(resource) => Some(resource),
                _ => None,
            })
            .collect();
        assert_eq!(
            resources
                .iter()
                .map(|resource| resource.uid.as_ref())
                .collect::<Vec<_>>(),
            ["ns-1", "dep-1", "pod-1"]
        );
        assert_eq!(resources[1].parent.as_deref(), Some("ns-1"));
        assert_eq!(resources[2].parent.as_deref(), Some("dep-1"));
        assert!(resources.iter().all(|resource| resource.op == Op::Added));
    }

    #[test]
    fn deleting_a_scope_retracts_children_before_their_parents() {
        let (mut store, mut catalog, assembled) = after_sync(vec![
            object(KindId::NAMESPACE, Role::Scope, "ns-1", "prod"),
            object(KindId::DEPLOYMENT, Role::Owner, "dep-1", "api"),
            under(
                object(KindId::POD, Role::Instance, "pod-1", "api-1"),
                ctrl("dep-1", "Deployment", "api", "apps/v1"),
            ),
        ]);
        let mut projection = Projection::from_assembled(&assembled);
        let before = store.remove("ns-1").map(Box::new);
        let events = projection.project(
            &store,
            &mut catalog,
            &Change {
                op: Op::Deleted,
                uid: "ns-1".into(),
                before,
            },
        );
        let deleted: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                IngestEvent::Resource(resource) if resource.op == Op::Deleted => {
                    Some(resource.uid.as_ref())
                }
                _ => None,
            })
            .collect();
        assert_eq!(deleted, ["pod-1", "dep-1", "ns-1"]);
    }

    #[test]
    fn a_live_owner_repeats_the_dependency_the_sync_gave_it() {
        let (store, mut catalog, assembled) = after_sync(vec![
            object(KindId::NAMESPACE, Role::Scope, "ns-1", "prod"),
            object(KindId::CRON_JOB, Role::Owner, "cj-1", "nightly"),
            under(
                object(KindId::JOB, Role::Owner, "job-1", "nightly-123"),
                ctrl("cj-1", "CronJob", "nightly", "batch/v1"),
            ),
            under(
                object(KindId::JOB, Role::Owner, "job-2", "adhoc"),
                ctrl("ro-1", "Rollout", "web", "argoproj.io/v1alpha1"),
            ),
        ]);
        let mut projection = Projection::from_assembled(&assembled);
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
        let live = |uid: &str, catalog: &mut Catalog, projection: &mut Projection| {
            let event = projection
                .project(&store, catalog, &modified(&store, uid))
                .into_iter()
                .next();
            match resource_event(event).payload {
                Payload::Owner { depends_on, .. } => depends_on,
                other => panic!("expected an owner payload, got {other:?}"),
            }
        };

        assert_eq!(
            live("job-1", &mut catalog, &mut projection),
            vec![Arc::<str>::from("cj-1")]
        );
        assert_eq!(
            live("job-1", &mut catalog, &mut projection),
            from_sync("job-1"),
            "one producer, one answer"
        );
        assert!(live("job-2", &mut catalog, &mut projection).is_empty());
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
            (
                resource("", "v1", "ComponentStatus", "componentstatuses"),
                caps(Scope::Cluster, &["list"]),
            ),
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
        let cs = discovered.find("", "ComponentStatus").unwrap();
        assert_eq!(
            verdicts.iter().find(|(k, _)| *k == cs.id).map(|(_, v)| *v),
            Some(Capability::Absent)
        );
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
