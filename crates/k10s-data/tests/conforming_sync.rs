use std::sync::Arc;

use k10s_core::{Catalog, IngestEvent, Intake, KindId, Op, Payload, Role, Severity, State, ToolId};
use k10s_data::assemble::{self, Store};
use k10s_data::mapping::{AttachKinds, AttachRef, Controller, Detail, Labels, Reason, Staged};
use k10s_world::input::{FoldStats, fold};

fn replica_set(catalog: &mut Catalog) -> KindId {
    catalog.intern_gvk_as("apps", "v1", "ReplicaSet", Role::Owner)
}

fn staged(kind: KindId, role: Role, uid: &str, ns: &str, name: &str, detail: Detail) -> Staged {
    Staged {
        kind,
        role,
        uid: uid.into(),
        namespace: ns.into(),
        name: name.into(),
        resource_version: 100,
        controller: None,
        detail,
    }
}

fn owned_by(mut s: Staged, uid: &str, kind: &str, name: &str, api_version: &str) -> Staged {
    s.controller = Some(Controller {
        uid: uid.into(),
        kind: kind.into(),
        name: name.into(),
        api_version: api_version.into(),
    });
    s
}

fn labels(pairs: &[(&str, &str)]) -> Labels {
    let mut out: Labels = pairs
        .iter()
        .map(|(k, v)| (Arc::from(*k), Arc::from(*v)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn instance(reason: &str, severity: Severity, labels: Labels, refs: Vec<AttachRef>) -> Detail {
    Detail::Instance {
        reason: Reason {
            severity,
            display: reason.into(),
        },
        labels,
        refs,
    }
}

fn attachment(detail: &str, selector: Labels) -> Detail {
    Detail::Attached {
        detail: detail.into(),
        selector,
    }
}

fn mounts(kind: KindId, name: &str) -> AttachRef {
    AttachRef {
        kind,
        name: name.into(),
    }
}

fn realistic_cluster(catalog: &mut Catalog) -> Store {
    let rs = replica_set(catalog);
    let mut store = Store::new(vec![rs]);

    for (uid, name) in [("ns-1", "prod"), ("ns-2", "staging")] {
        store.apply(staged(
            KindId::NAMESPACE,
            Role::Scope,
            uid,
            "",
            name,
            Detail::Scope,
        ));
    }

    store.apply(staged(
        KindId::DEPLOYMENT,
        Role::Owner,
        "dep-api",
        "prod",
        "api",
        Detail::Owner {
            tool: ToolId::NGINX,
        },
    ));
    store.apply(owned_by(
        staged(
            rs,
            Role::Owner,
            "rs-api",
            "prod",
            "api-7f9",
            Detail::Owner { tool: ToolId::NONE },
        ),
        "dep-api",
        "Deployment",
        "api",
        "apps/v1",
    ));
    for i in 0..3 {
        store.apply(owned_by(
            staged(
                KindId::POD,
                Role::Instance,
                &format!("pod-api-{i}"),
                "prod",
                &format!("api-7f9-{i}"),
                instance(
                    if i == 0 {
                        "CrashLoopBackOff"
                    } else {
                        "Running"
                    },
                    if i == 0 { Severity::Err } else { Severity::Ok },
                    labels(&[("app", "api"), ("tier", "web")]),
                    vec![
                        mounts(KindId::CONFIG_MAP, "api-config"),
                        mounts(KindId::SECRET, "api-tls"),
                        mounts(KindId::VOLUME, "api-data"),
                    ],
                ),
            ),
            "rs-api",
            "ReplicaSet",
            "api-7f9",
            "apps/v1",
        ));
    }
    store.apply(staged(
        KindId::SERVICE,
        Role::Attached,
        "svc-api",
        "prod",
        "api",
        attachment("ClusterIP 80", labels(&[("app", "api")])),
    ));
    store.apply(staged(
        KindId::CONFIG_MAP,
        Role::Attached,
        "cm-api",
        "prod",
        "api-config",
        attachment("", Vec::new()),
    ));
    store.apply(staged(
        KindId::SECRET,
        Role::Attached,
        "sec-api",
        "prod",
        "api-tls",
        attachment("", Vec::new()),
    ));
    store.apply(staged(
        KindId::VOLUME,
        Role::Attached,
        "pvc-api",
        "prod",
        "api-data",
        attachment("10Gi", Vec::new()),
    ));
    store.apply(staged(
        KindId::CONFIG_MAP,
        Role::Attached,
        "cm-orphan",
        "prod",
        "kube-root-ca.crt",
        attachment("", Vec::new()),
    ));

    store.apply(staged(
        KindId::CRON_JOB,
        Role::Owner,
        "cj-1",
        "prod",
        "nightly",
        Detail::Owner { tool: ToolId::NONE },
    ));
    store.apply(owned_by(
        staged(
            KindId::JOB,
            Role::Owner,
            "job-1",
            "prod",
            "nightly-29001",
            Detail::Owner { tool: ToolId::NONE },
        ),
        "cj-1",
        "CronJob",
        "nightly",
        "batch/v1",
    ));
    store.apply(owned_by(
        staged(
            KindId::POD,
            Role::Instance,
            "pod-job-1",
            "prod",
            "nightly-29001-x",
            instance("Succeeded", Severity::Ok, Vec::new(), Vec::new()),
        ),
        "job-1",
        "Job",
        "nightly-29001",
        "batch/v1",
    ));

    for i in 0..2 {
        store.apply(owned_by(
            staged(
                KindId::POD,
                Role::Instance,
                &format!("pod-vm-{i}"),
                "staging",
                &format!("vm-{i}"),
                instance("Running", Severity::Ok, Vec::new(), Vec::new()),
            ),
            "vmi-1",
            "VirtualMachineInstance",
            "web-vm",
            "kubevirt.io/v1",
        ));
    }

    store.apply(staged(
        KindId::POD,
        Role::Instance,
        "pod-debug",
        "staging",
        "debug",
        instance("Running", Severity::Ok, Vec::new(), Vec::new()),
    ));

    store.apply(staged(
        KindId::DEPLOYMENT,
        Role::Owner,
        "dep-hidden",
        "kube-system",
        "coredns",
        Detail::Owner { tool: ToolId::NONE },
    ));

    store
}

fn resources(events: &[IngestEvent]) -> Vec<&k10s_core::ResourceEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            IngestEvent::Resource(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[test]
fn an_assembled_sync_is_something_the_world_accepts() {
    let mut catalog = Catalog::new();
    let store = realistic_cluster(&mut catalog);
    let assembled = assemble::assemble(&store, &mut catalog);

    let (input, stats) = fold(&assembled.events);
    assert_eq!(
        stats,
        FoldStats::default(),
        "the world could not place everything: {stats:?}"
    );

    assert_eq!(input.namespaces.len(), 2, "kube-system was never seen");
    let prod = input
        .namespaces
        .iter()
        .find(|ns| &*ns.name == "prod")
        .expect("prod");
    let staging = input
        .namespaces
        .iter()
        .find(|ns| &*ns.name == "staging")
        .expect("staging");

    assert_eq!(prod.workloads.len(), 3);
    assert!(
        prod.workloads
            .iter()
            .all(|w| w.kind != replica_set(&mut Catalog::new()))
    );
    let api = prod
        .workloads
        .iter()
        .find(|w| &*w.name == "api")
        .expect("the deployment");
    assert_eq!(api.pods.len(), 3);
    assert_eq!(api.tool, ToolId::NGINX);
    assert_eq!(
        api.sats.len(),
        4,
        "service, config map, secret and claim all sit under the workload that uses them"
    );

    assert_eq!(staging.workloads.len(), 2);
    let vmi = staging
        .workloads
        .iter()
        .find(|w| &*w.name == "web-vm")
        .expect("the VMI card");
    assert_eq!(vmi.pods.len(), 2, "two pods, one card");
    assert!(!vmi.kind.is_builtin());
    let debug = staging
        .workloads
        .iter()
        .find(|w| &*w.name == "debug")
        .expect("the standalone pod card");
    assert_eq!(debug.pods.len(), 1);
    assert_eq!(debug.kind, KindId::POD);

    assert_eq!(input.total_edges, 1, "the Job depends on its CronJob");
    assert_eq!(
        assembled.stats.unattached, 1,
        "kube-root-ca.crt has no home"
    );
    assert_eq!(
        assembled.stats.unknown_namespace, 1,
        "the hidden deployment"
    );
}

#[test]
fn an_assembled_sync_builds_a_world_without_tripping_its_assertions() {
    let mut catalog = Catalog::new();
    let store = realistic_cluster(&mut catalog);
    let assembled = assemble::assemble(&store, &mut catalog);

    for mode in [
        k10s_world::LayoutMode::Spread,
        k10s_world::LayoutMode::Dense,
    ] {
        let scene = k10s_core::new_shared_scene();
        let (mut world, mut schedule) =
            k10s_world::build_world_from_stream(&assembled.events, scene.clone(), mode);
        schedule.run(&mut world);
        let snapshot = scene.load_full();
        assert_eq!(snapshot.regions.len(), 2, "{mode:?}");
        assert_eq!(
            snapshot.blocks.len(),
            5,
            "{mode:?}: three in prod, two in staging"
        );
        assert_eq!(snapshot.cells.len(), 7, "{mode:?}: every pod");
        assert_eq!(
            snapshot.sats.len(),
            if mode.emits_attachments() { 4 } else { 0 },
            "{mode:?}: dense layout places no attachments"
        );
        assert_eq!(snapshot.edges.len(), 1, "{mode:?}: the Job to CronJob edge");
    }
}

#[test]
fn the_severity_of_a_crashlooping_pod_reaches_the_scene() {
    let mut catalog = Catalog::new();
    let store = realistic_cluster(&mut catalog);
    let assembled = assemble::assemble(&store, &mut catalog);

    let crashing = resources(&assembled.events)
        .into_iter()
        .find(|r| r.uid.as_ref() == "pod-api-0")
        .expect("the crashing pod");
    let Payload::Instance { state } = crashing.payload else {
        panic!("expected an instance")
    };
    assert_eq!(state.severity, Severity::Err);
    assert_eq!(catalog.reason_display(state.reason), "CrashLoopBackOff");

    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = k10s_world::build_world_from_stream(
        &assembled.events,
        scene.clone(),
        k10s_world::LayoutMode::Spread,
    );
    schedule.run(&mut world);
    let snapshot = scene.load_full();

    let prod = snapshot
        .regions
        .iter()
        .find(|r| &*r.label == "prod")
        .expect("prod");
    assert_eq!(
        prod.ext.rollup,
        Severity::Err,
        "one crashlooping pod makes its namespace red"
    );
    let api = snapshot
        .blocks
        .iter()
        .find(|b| &*b.label == "api")
        .expect("api");
    assert_eq!(api.ext.rollup, Severity::Err);
    let staging = snapshot
        .regions
        .iter()
        .find(|r| &*r.label == "staging")
        .expect("staging");
    assert_eq!(staging.ext.rollup, Severity::Ok);
}

#[test]
fn an_assembled_sync_survives_an_intake_the_way_a_recorded_one_does() {
    let mut catalog = Catalog::new();
    let store = realistic_cluster(&mut catalog);
    let assembled = assemble::assemble(&store, &mut catalog);

    let mut intake = Intake::new();
    for event in &assembled.events {
        intake.push(event.clone());
    }
    let once = intake.drain();
    assert_eq!(resources(&once).len(), assembled.events.len());

    let mut intake = Intake::new();
    for _ in 0..2 {
        for event in &assembled.events {
            intake.push(event.clone());
        }
    }
    let twice = intake.drain();
    assert_eq!(
        resources(&twice).len(),
        resources(&once).len(),
        "a relist must not double the cluster"
    );
    assert!(resources(&once).iter().all(|r| r.op == Op::Added));
}

#[test]
fn a_secret_reaches_the_scene_as_a_name_and_nothing_else() {
    let mut catalog = Catalog::new();
    let store = realistic_cluster(&mut catalog);
    let assembled = assemble::assemble(&store, &mut catalog);

    let secret = resources(&assembled.events)
        .into_iter()
        .find(|r| r.kind == KindId::SECRET)
        .expect("the secret is on the map");
    let Payload::Attached { detail, .. } = &secret.payload else {
        panic!("expected an attachment")
    };
    assert!(detail.is_empty(), "a secret detail must carry nothing");
    assert_eq!(&*secret.name, "api-tls");

    for r in resources(&assembled.events) {
        if let Payload::Attached { detail, .. } = &r.payload {
            assert!(
                !detail.contains("BEGIN") && !detail.contains('='),
                "{}: {detail}",
                r.name
            );
        }
    }
}

#[test]
fn an_empty_cluster_is_an_empty_sync_rather_than_a_failure() {
    let mut catalog = Catalog::new();
    let store = Store::new(Vec::new());
    let assembled = assemble::assemble(&store, &mut catalog);
    assert!(assembled.events.is_empty());
    let (input, stats) = fold(&assembled.events);
    assert_eq!(stats, FoldStats::default());
    assert!(input.namespaces.is_empty());
}

#[test]
fn attachment_kinds_default_to_the_builtins_so_a_reference_resolves() {
    let kinds = AttachKinds::default();
    assert_eq!(kinds.config_map, KindId::CONFIG_MAP);
    assert_eq!(kinds.secret, KindId::SECRET);
    assert_eq!(kinds.volume, KindId::VOLUME);

    let mut catalog = Catalog::new();
    assert_eq!(catalog.intern_gvk("", "v1", "ConfigMap"), kinds.config_map);
    assert_eq!(catalog.intern_gvk("", "v1", "Secret"), kinds.secret);
    assert_eq!(
        catalog.intern_gvk("", "v1", "PersistentVolumeClaim"),
        kinds.volume
    );
}

#[test]
fn every_replay_scenario_still_folds_the_way_the_data_plane_output_does() {
    let (_, stats) = fold(&k10s_core::replay::initial_sync().events);
    assert_eq!(stats, FoldStats::default());

    let mut catalog = Catalog::new();
    let store = realistic_cluster(&mut catalog);
    let ours = assemble::assemble(&store, &mut catalog);
    let (_, stats) = fold(&ours.events);
    assert_eq!(stats, FoldStats::default());

    for events in [&k10s_core::replay::initial_sync().events, &ours.events] {
        let states: Vec<State> = resources(events)
            .into_iter()
            .filter_map(|r| match r.payload {
                Payload::Instance { state } => Some(state),
                _ => None,
            })
            .collect();
        assert!(!states.is_empty());
    }
}
