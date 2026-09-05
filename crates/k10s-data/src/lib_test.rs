//! The seam between the data plane and the world: that a bounded sink
//! backpressures instead of dropping, that a scope is published parent-first
//! and retracted children-first, and that a report summarises without ever
//! naming an object.

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

fn settle(settled: &mut Settled, kind: KindId, listed: bool) {
    let mut store = Store::new(Vec::new());
    let mut desyncs = Vec::new();
    apply(
        Message::Settled { kind, listed },
        &mut store,
        settled,
        &mut desyncs,
        &IngestMetrics::default(),
    );
}

#[test]
fn a_kind_is_caught_up_only_when_every_one_of_its_streams_listed() {
    let mut settled: Settled = HashMap::new();
    settle(&mut settled, KindId::POD, true);
    assert!(
        !kind_synced(&settled, KindId::POD, 2),
        "one namespace of two has not answered yet"
    );

    settle(&mut settled, KindId::POD, false);
    assert!(
        !kind_synced(&settled, KindId::POD, 2),
        "a stream that settled without listing cannot be spoken for by the one that did"
    );

    let mut both: Settled = HashMap::new();
    settle(&mut both, KindId::POD, true);
    settle(&mut both, KindId::POD, true);
    assert!(kind_synced(&both, KindId::POD, 2));
    assert!(
        !kind_synced(&both, KindId::NAMESPACE, 1),
        "a kind no stream ever settled is not caught up"
    );
}

#[test]
fn deleting_what_the_store_never_held_is_not_a_change() {
    let mut store = Store::new(Vec::new());
    store.apply(object(KindId::NAMESPACE, Role::Scope, "ns-1", "prod"));
    let mut settled: Settled = HashMap::new();
    let mut desyncs = Vec::new();
    let metrics = IngestMetrics::default();

    let phantom = apply(
        Message::Delete {
            kind: KindId::POD,
            uid: "never-here".into(),
        },
        &mut store,
        &mut settled,
        &mut desyncs,
        &metrics,
    );
    assert!(
        phantom.is_none(),
        "a delete the store did not hold changes nothing and must not force a reconcile"
    );

    let real = apply(
        Message::Delete {
            kind: KindId::NAMESPACE,
            uid: "ns-1".into(),
        },
        &mut store,
        &mut settled,
        &mut desyncs,
        &metrics,
    );
    assert!(matches!(
        real,
        Some(Change {
            op: Op::Deleted,
            ..
        })
    ));
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

fn resource(group: &str, version: &str, kind: &str, plural: &str) -> kube::discovery::ApiResource {
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
