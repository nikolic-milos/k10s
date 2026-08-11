use std::collections::VecDeque;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use k10s_clustergen::{GenConfig, Scenario, generate};
use k10s_core::{
    IngestEvent, KindId, Level, Op, ReasonId, SceneSnapshot, Severity, State, ToolId, replay,
};

use crate::PublishBench;
use crate::build_world;
use crate::layout::LayoutMode;
use crate::test_support::*;
use crate::topology;
use crate::{Aggregates, set_pod_state};
use crate::{PublishStats, SNAPSHOT_POOL_DEPTH, Topology};
use crate::{SnapshotPool, materialize_nodes, materialize_snapshot};

#[test]
fn parallel_node_materialization_is_exactly_the_sequential_algorithm() {
    let spec = input_of(91, 12_000, Scenario::Platform);
    let scene = k10s_core::new_shared_scene();
    let (world, _) = build_world(&spec, scene, LayoutMode::Spread);
    let topology = world.resource::<Topology>();
    let aggregates = world.resource::<Aggregates>();
    let mut sequential = SceneSnapshot::default();
    let mut parallel = SceneSnapshot::default();

    materialize_nodes(&mut sequential, &topology, &aggregates, false);
    materialize_nodes(&mut parallel, &topology, &aggregates, true);

    assert_eq!(parallel.regions, sequential.regions);
    assert_eq!(parallel.blocks, sequential.blocks);
    assert_eq!(parallel.cells, sequential.cells);
    assert_eq!(parallel.sats, sequential.sats);
}

#[test]
fn initial_snapshot_published_and_rollups_react() {
    let spec = input_of(1, 3000, Scenario::Platform);
    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = build_world(&spec, scene.clone(), LayoutMode::Spread);

    schedule.run(&mut world);
    let snap = scene.load();
    assert_eq!(snap.rev, 1);
    assert_eq!(snap.totals.cells as usize, snap.cells.len());
    assert!(snap.totals.cells > 0);

    schedule.run(&mut world);
    assert_eq!(scene.load().rev, 1);

    set_pod_state(&mut world, 0, st(Severity::Err));
    schedule.run(&mut world);
    let snap = scene.load();
    assert_eq!(snap.rev, 2);
    assert_eq!(snap.cells[0].ext.state.severity, Severity::Err);
    let pod_rect = snap.cells[0].rect;
    let owner = &snap.blocks[world.resource::<Topology>().pod_wl[0] as usize];
    assert_eq!(owner.ext.rollup, Severity::Err);
    assert!(owner.rect.intersects(&pod_rect));
}

#[test]
fn incremental_publish_matches_full_materialize() {
    let spec = input_of(3, 5_000, Scenario::Platform);
    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = build_world(&spec, scene.clone(), LayoutMode::Spread);
    schedule.run(&mut world);

    let flip = |world: &mut World, pod: usize, h: Severity| {
        set_pod_state(world, pod as u32, st(h));
    };

    flip(&mut world, 0, Severity::Err);
    schedule.run(&mut world);
    flip(&mut world, 1, Severity::Warn);
    schedule.run(&mut world);
    flip(&mut world, 2, Severity::Unknown);
    schedule.run(&mut world);

    let snap = scene.load_full();
    assert_eq!(snap.rev, 4);
    let full = {
        let topo = world.resource::<Topology>();
        let agg = world.resource::<Aggregates>();
        materialize_snapshot(topo, agg, 4)
    };
    assert_eq!(snap.cells.len(), full.cells.len());
    for (a, b) in snap.cells.iter().zip(full.cells.iter()) {
        assert_eq!(a.ext.state, b.ext.state);
    }
    for (a, b) in snap.blocks.iter().zip(full.blocks.iter()) {
        assert_eq!(a.ext.rollup, b.ext.rollup);
    }
    for (a, b) in snap.regions.iter().zip(full.regions.iter()) {
        assert_eq!(a.ext.unhealthy_frac, b.ext.unhealthy_frac);
    }
    assert_eq!(snap.region_edges.len(), snap.regions.len());
    assert_eq!(snap.cells[0].ext.state.severity, Severity::Err);
    assert_eq!(snap.cells[1].ext.state.severity, Severity::Warn);
    assert_eq!(snap.cells[2].ext.state.severity, Severity::Unknown);
}

#[test]
fn a_reason_only_change_publishes_instead_of_piling_up() {
    let spec = platform(21, 2_000);
    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = build_world(&spec, scene.clone(), LayoutMode::Spread);
    schedule.run(&mut world);
    assert_eq!(scene.load().rev, 1);

    let warn = [ReasonId::PENDING, ReasonId::NOT_READY];
    set_pod_state(&mut world, 0, State::of(warn[0]));
    schedule.run(&mut world);
    let base = scene.load().rev;

    for round in 0..8u64 {
        let want = State::of(warn[(round as usize + 1) % 2]);
        set_pod_state(&mut world, 0, want);
        schedule.run(&mut world);

        let snap = scene.load_full();
        assert_eq!(
            snap.rev,
            base + round + 1,
            "a reason-only change must publish"
        );
        assert_eq!(snap.cells[0].ext.state, want);
        assert_published_matches_full(&world, &snap);
        for (slot, p) in world.resource::<SnapshotPool>().pending.iter().enumerate() {
            assert!(
                p.pods.len() <= SNAPSHOT_POOL_DEPTH,
                "pool slot {slot} holds {} pending pods after {} reason-only ticks",
                p.pods.len(),
                round + 1
            );
        }
    }
}

#[test]
fn held_buffer_is_never_mutated_under_reader() {
    let spec = input_of(4, 2_000, Scenario::Platform);
    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = build_world(&spec, scene.clone(), LayoutMode::Spread);
    schedule.run(&mut world);

    let held = scene.load_full();
    let held_health: Vec<Severity> = held.cells.iter().map(|c| c.ext.state.severity).collect();

    let flip = |world: &mut World, pod: usize, h: Severity| {
        set_pod_state(world, pod as u32, st(h));
    };
    let target = held_health
        .iter()
        .position(|&h| h != Severity::Err)
        .expect("some pod not already Err");
    flip(&mut world, target, Severity::Err);
    schedule.run(&mut world);
    flip(&mut world, target, Severity::Warn);
    schedule.run(&mut world);

    assert_eq!(held.rev, 1, "reader's snapshot changed under it");
    for (cell, &h) in held.cells.iter().zip(&held_health) {
        assert_eq!(
            cell.ext.state.severity, h,
            "reader's snapshot changed under it"
        );
    }
    let fresh = scene.load_full();
    assert_eq!(fresh.rev, 3);
    assert_eq!(fresh.cells[target].ext.state.severity, Severity::Warn);
}

#[test]
fn a_reader_lapped_twice_costs_no_deep_clone() {
    const LAPPED_PUBLISHES_ABSORBED: usize = 2;
    const { assert!(SNAPSHOT_POOL_DEPTH > LAPPED_PUBLISHES_ABSORBED) };

    let spec = platform(9, 3_000);
    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = build_world(&spec, scene.clone(), LayoutMode::Spread);
    schedule.run(&mut world);

    let held = scene.load_full();
    for pod in 0..LAPPED_PUBLISHES_ABSORBED {
        flip_to_other(&mut world, pod);
        schedule.run(&mut world);
    }

    let stats = *world.resource::<PublishStats>();
    assert_eq!(stats.publishes as usize, LAPPED_PUBLISHES_ABSORBED + 1);
    assert_eq!(
        stats.deep_clones, 0,
        "one reader lapped {LAPPED_PUBLISHES_ABSORBED} times forced {} deep clones at pool \
         depth {SNAPSHOT_POOL_DEPTH}",
        stats.deep_clones
    );
    assert_eq!(held.rev, 1);
    assert_eq!(scene.load().rev as usize, LAPPED_PUBLISHES_ABSORBED + 1);
    assert_published_matches_full(&world, &scene.load_full());
}

#[test]
fn publish_under_a_lapped_reader_stays_correct() {
    let spec = platform(11, 4_000);
    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = build_world(&spec, scene.clone(), LayoutMode::Spread);
    schedule.run(&mut world);

    let pinned = scene.load_full();
    let pinned_health: Vec<Severity> = pinned.cells.iter().map(|c| c.ext.state.severity).collect();
    let mut recent: VecDeque<Arc<SceneSnapshot>> = VecDeque::new();
    recent.push_back(scene.load_full());

    let pods = world.resource::<Topology>().pod_labels.len();
    for round in 0..SNAPSHOT_POOL_DEPTH * 4 {
        flip_to_other(&mut world, (round * 37) % pods);
        flip_to_other(&mut world, (round * 101 + 5) % pods);
        schedule.run(&mut world);

        let published = scene.load_full();
        assert_eq!(published.rev as usize, round + 2);
        assert_published_matches_full(&world, &published);
        recent.push_back(published);
        if recent.len() > SNAPSHOT_POOL_DEPTH {
            recent.pop_front();
        }
    }

    let stats = *world.resource::<PublishStats>();
    assert!(
        stats.deep_clones > 0,
        "lapped reader never forced the clone path: {stats:?}"
    );
    assert_eq!(stats.full_materializes as usize, SNAPSHOT_POOL_DEPTH);
    assert_eq!(pinned.rev, 1);
    for (i, (cell, &h)) in pinned.cells.iter().zip(&pinned_health).enumerate() {
        assert_eq!(
            cell.ext.state.severity, h,
            "pinned snapshot cell {i} changed"
        );
    }
}

#[test]
fn rollup_arithmetic_survives_adversarial_dirty_streams() {
    let spec = platform(13, 3_000);
    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = build_world(&spec, scene.clone(), LayoutMode::Spread);
    schedule.run(&mut world);
    assert_rollup_arithmetic(&world);

    let wl = world
        .resource::<Topology>()
        .wl_pod_range
        .iter()
        .position(|r| r.end - r.start >= 4)
        .expect("a workload with four or more pods");
    let pods: Vec<u32> = world.resource::<Topology>().wl_pod_range[wl]
        .clone()
        .collect();
    let originals: Vec<State> = pods
        .iter()
        .map(|&i| world.resource::<Aggregates>().pod_state[i as usize])
        .collect();

    for h in [
        Severity::Err,
        Severity::Warn,
        Severity::Ok,
        Severity::Unknown,
        Severity::Err,
    ] {
        set_pod_state(&mut world, pods[0], st(h));
    }
    set_pod_state(&mut world, pods[0], originals[0]);
    set_pod_state(&mut world, pods[1], st(Severity::Err));
    set_pod_state(&mut world, pods[1], st(Severity::Warn));
    set_pod_state(&mut world, pods[2], st(Severity::Unknown));
    schedule.run(&mut world);

    assert_rollup_arithmetic(&world);
    assert_eq!(
        world.resource::<Aggregates>().pod_state[pods[0] as usize],
        originals[0]
    );
    assert_eq!(
        world.resource::<Aggregates>().pod_state[pods[1] as usize],
        st(Severity::Warn)
    );
    assert_published_matches_full(&world, &scene.load_full());

    for round in 0..4u32 {
        for (slot, &pod) in pods.iter().enumerate() {
            for h in [
                Severity::Err,
                Severity::Ok,
                Severity::Warn,
                Severity::Unknown,
            ] {
                set_pod_state(&mut world, pod, st(h));
            }
            set_pod_state(&mut world, pod, originals[slot]);
            if (slot as u32).is_multiple_of(round + 1) {
                set_pod_state(&mut world, pod, st(Severity::Err));
            }
        }
        schedule.run(&mut world);
        assert_rollup_arithmetic(&world);
        assert_published_matches_full(&world, &scene.load_full());
    }

    for (slot, &pod) in pods.iter().enumerate() {
        set_pod_state(&mut world, pod, originals[slot]);
    }
    schedule.run(&mut world);
    assert_rollup_arithmetic(&world);
    for (slot, &pod) in pods.iter().enumerate() {
        assert_eq!(
            world.resource::<Aggregates>().pod_state[pod as usize],
            originals[slot]
        );
    }
}

#[test]
fn scene_ranges_partition_every_array() {
    let spec = platform(17, 8_000);
    for mode in [LayoutMode::Spread, LayoutMode::Dense] {
        let scene = k10s_core::new_shared_scene();
        let (mut world, mut schedule) = build_world(&spec, scene.clone(), mode);
        schedule.run(&mut world);
        let snap = scene.load_full();

        assert_eq!(snap.totals.regions as usize, snap.regions.len(), "{mode:?}");
        assert_eq!(snap.totals.blocks as usize, snap.blocks.len(), "{mode:?}");
        assert_eq!(snap.totals.cells as usize, snap.cells.len(), "{mode:?}");
        assert_eq!(snap.totals.sats as usize, snap.sats.len(), "{mode:?}");
        assert_eq!(snap.totals.edges as usize, snap.edges.len(), "{mode:?}");
        assert!(snap.totals.cells > 0 && snap.totals.blocks > 0);
        assert_eq!(
            snap.sats.is_empty(),
            !mode.emits_attachments(),
            "{mode:?} snapshot disagrees with emits_attachments"
        );

        let mut next_block = 0u32;
        for (i, region) in snap.regions.iter().enumerate() {
            assert_eq!(
                region.children.start, next_block,
                "{mode:?} region {i} children not contiguous"
            );
            assert!(region.children.end >= region.children.start);
            next_block = region.children.end;
        }
        assert_eq!(next_block as usize, snap.blocks.len(), "{mode:?}");

        let mut next_cell = 0u32;
        let mut next_sat = 0u32;
        for (i, block) in snap.blocks.iter().enumerate() {
            assert_eq!(
                block.children.start, next_cell,
                "{mode:?} block {i} children not contiguous"
            );
            assert!(block.children.end >= block.children.start);
            next_cell = block.children.end;
            assert_eq!(
                block.sats.start, next_sat,
                "{mode:?} block {i} sats not contiguous"
            );
            assert!(block.sats.end >= block.sats.start);
            next_sat = block.sats.end;
            assert!(
                snap.regions[block.ext.ns as usize]
                    .children
                    .contains(&(i as u32)),
                "{mode:?} block {i} claims region {} which does not own it",
                block.ext.ns
            );
        }
        assert_eq!(next_cell as usize, snap.cells.len(), "{mode:?}");
        assert_eq!(next_sat as usize, snap.sats.len(), "{mode:?}");

        assert_eq!(snap.region_edges.len(), snap.regions.len(), "{mode:?}");
        let mut next_edge = 0u32;
        for (i, range) in snap.region_edges.iter().enumerate() {
            assert_eq!(
                range.start, next_edge,
                "{mode:?} region {i} edges not contiguous"
            );
            assert!(range.end >= range.start);
            next_edge = range.end;
        }
        assert_eq!(
            next_edge, snap.cross_edges.start,
            "{mode:?} region ranges must end where the cross tail begins"
        );
        assert_eq!(
            snap.cross_edges.end as usize,
            snap.edges.len(),
            "{mode:?} cross tail must run to the end of edges"
        );
        assert!(snap.cross_edges.start <= snap.cross_edges.end, "{mode:?}");
        assert!(
            snap.cross_edges.end as usize <= snap.edges.len(),
            "{mode:?} cross_edges {:?} outside edges of {}",
            snap.cross_edges,
            snap.edges.len()
        );

        for (i, region) in snap.regions.iter().enumerate() {
            let cells: u32 = region
                .children
                .clone()
                .map(|b| {
                    let block = &snap.blocks[b as usize];
                    block.children.end - block.children.start
                })
                .sum();
            assert_eq!(region.weight, cells, "{mode:?} region {i} weight");
        }
        for edge in &snap.edges {
            for end in [edge.a, edge.b] {
                let limit = match end.level() {
                    Level::Region => snap.regions.len(),
                    Level::Block => snap.blocks.len(),
                    Level::Cell => snap.cells.len(),
                    Level::Sat => snap.sats.len(),
                };
                assert!(
                    (end.index() as usize) < limit,
                    "{mode:?} edge endpoint {end:?} outside its {:?} array of {limit}",
                    end.level()
                );
            }
        }
    }
}

#[test]
fn cross_namespace_edges_land_in_the_cross_range() {
    let spec = platform(55, 20_000);
    let owner_index = crate::input::owner_indices(&spec);
    let ns_of_block: Vec<u32> = spec
        .namespaces
        .iter()
        .enumerate()
        .flat_map(|(ni, ns)| ns.workloads.iter().map(move |_| ni as u32))
        .collect();
    let crossing = spec
        .namespaces
        .iter()
        .enumerate()
        .flat_map(|(ni, ns)| ns.workloads.iter().map(move |wl| (ni as u32, wl)))
        .filter(|(ni, wl)| {
            wl.depends_on.iter().any(|t| {
                owner_index
                    .get(t)
                    .is_some_and(|&to| ns_of_block[to as usize] != *ni)
            })
        })
        .count();
    assert!(
        crossing > 0,
        "the generator produced no cross-namespace links"
    );
    for mode in [LayoutMode::Dense, LayoutMode::Spread] {
        let scene = k10s_core::new_shared_scene();
        let (mut world, mut schedule) = build_world(&spec, scene.clone(), mode);
        schedule.run(&mut world);
        let snap = scene.load_full();

        let cross = &snap.cross_edges;
        assert!(!cross.is_empty(), "{mode:?}: cross range still empty");
        assert_eq!(
            cross.end as usize,
            snap.edges.len(),
            "{mode:?}: cross links must be the tail of edges"
        );

        for (i, r) in snap.region_edges.iter().enumerate() {
            assert!(
                r.end <= cross.start,
                "{mode:?}: region {i} range {r:?} overlaps the cross tail at {}",
                cross.start
            );
        }

        let ns_of = |block: u32| snap.blocks[block as usize].ext.ns;
        for e in &snap.edges[cross.start as usize..cross.end as usize] {
            assert_eq!(e.a.level(), Level::Block, "{mode:?}");
            assert_eq!(e.b.level(), Level::Block, "{mode:?}");
            assert_ne!(
                ns_of(e.a.index()),
                ns_of(e.b.index()),
                "{mode:?}: cross edge {e:?} has both ends in one namespace"
            );
        }

        let drawn = k10s_atlas::walk_edges(&snap, &snap.bounds, usize::MAX, |_, _| {});
        assert!(
            drawn >= cross.len(),
            "{mode:?}: culler drew {drawn} edges, fewer than the {} cross links",
            cross.len()
        );
    }
}

#[test]
fn selective_rebuilds_match_a_full_rebuild_after_every_batch_shape() {
    use k10s_core::{Payload, ResourceEvent};
    let sat = |uid: &str, parent: &str, op: Op| {
        IngestEvent::Resource(ResourceEvent {
            kind: KindId::SERVICE,
            uid: uid.into(),
            namespace: "prod".into(),
            name: uid.into(),
            resource_version: 0,
            parent: Some(parent.into()),
            op,
            payload: Payload::Attached {
                kind: KindId::SERVICE,
                detail: Arc::from("80/TCP"),
            },
        })
    };
    let owner_with_deps = |uid: &str, name: &str, deps: &[&str], op: Op| {
        IngestEvent::Resource(ResourceEvent {
            kind: KindId::DEPLOYMENT,
            uid: uid.into(),
            namespace: "prod".into(),
            name: name.into(),
            resource_version: 0,
            parent: Some("ns-prod".into()),
            op,
            payload: Payload::Owner {
                kind: KindId::DEPLOYMENT,
                tool: ToolId::NONE,
                depends_on: deps.iter().map(|dep| Arc::from(*dep)).collect(),
            },
        })
    };
    let renamed_pod = |uid: &str, name: &str, state: State| {
        IngestEvent::Resource(ResourceEvent {
            kind: KindId::POD,
            uid: uid.into(),
            namespace: "prod".into(),
            name: name.into(),
            resource_version: 0,
            parent: Some("wl-api".into()),
            op: Op::Modified,
            payload: Payload::Instance { state },
        })
    };

    let batches: Vec<Vec<IngestEvent>> = vec![
        // The rolling-update hot path: pods only.
        vec![replay::instance(
            "pod-3",
            "prod",
            "wl-api",
            State::of(ReasonId::CRASH_LOOP_BACK_OFF),
            Op::Added,
        )],
        // A rename forces a pod state change through the structural path.
        vec![renamed_pod(
            "pod-3",
            "pod-3-renamed",
            State::of(ReasonId::NOT_READY),
        )],
        vec![replay::instance(
            "pod-2",
            "prod",
            "wl-api",
            State::OK,
            Op::Deleted,
        )],
        // A new workload with edges, then its pod, parent-first.
        vec![
            owner_with_deps("wl-edge", "edge", &["wl-api"], Op::Added),
            replay::instance("pod-e1", "prod", "wl-edge", State::OK, Op::Added),
        ],
        // Dependency change on an existing workload without a move.
        vec![owner_with_deps("wl-edge", "edge", &[], Op::Modified)],
        vec![sat("svc-api", "wl-api", Op::Added)],
        vec![sat("svc-api", "wl-api", Op::Deleted)],
        // A whole new namespace, then its content.
        vec![
            replay::scope("ns-canary", "canary", Op::Added),
            replay::owner("wl-canary", "canary", "canary", KindId::JOB, Op::Added),
            replay::instance(
                "pod-c1",
                "canary",
                "wl-canary",
                State::of(ReasonId::PENDING),
                Op::Added,
            ),
        ],
        // A workload delete cascades its pod before the slot clears.
        vec![replay::owner(
            "wl-canary",
            "canary",
            "canary",
            KindId::JOB,
            Op::Deleted,
        )],
        vec![replay::scope("ns-canary", "canary", Op::Deleted)],
        // Slot reuse after the tombstones above: the reused slot's
        // identity vector entry must change with it, which is the whole
        // reason the snapshot carries ids.
        vec![replay::instance(
            "pod-4",
            "prod",
            "wl-api",
            State::OK,
            Op::Added,
        )],
    ];

    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    topology::verify_derived_state(&mut bench.world);

    let held = bench.snapshot();
    let held_before = (*held).clone();

    for (index, batch) in batches.iter().enumerate() {
        let before = placement(&bench.snapshot());
        bench.apply_events(batch);
        bench.run_publish();
        topology::verify_derived_state(&mut bench.world);
        assert_published_matches_full(&bench.world, &bench.snapshot());
        let snapshot = bench.snapshot();
        assert!(
            snapshot.rev > index as u64,
            "each structural batch must publish"
        );
        // The two assertions above are equivalence oracles: they compare this
        // snapshot to a fresh materialize of the same state. Neither compares
        // it to the snapshot *before* the batch, which is the only comparison
        // that can see the map reshuffle.
        assert_only_ancestors_moved(
            &before,
            &placement(&snapshot),
            &touched_uids(batch),
            &[],
            &format!("batch {index}"),
        );
    }

    let stats = bench.stats();
    assert!(
        stats.structural_patches > 0,
        "the small batches must exercise the patch path: {stats:?}"
    );
    assert!(
        stats.full_materializes > 0,
        "batches touching most of a tiny scene must fall back to full: {stats:?}"
    );
    assert_eq!(
        held.regions, held_before.regions,
        "a held snapshot must never change under its reader"
    );
    assert_eq!(held.cells, held_before.cells);
    assert_eq!(held.totals, held_before.totals);
    assert_eq!(held.rev, held_before.rev);
}

#[test]
fn snapshot_ids_name_slots_and_survive_reuse() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);

    let snap = bench.snapshot();
    let slot = snap
        .cells
        .iter()
        .position(|cell| cell.label.as_ref() == "pod-1")
        .expect("pod-1 is in the initial sync");
    assert_eq!(
        snap.ids.cells[slot].as_ref(),
        "pod-1",
        "a slot's identity entry names the object living in it"
    );
    drop(snap);

    bench.apply_events(&[replay::instance(
        "pod-1",
        "prod",
        "wl-api",
        State::OK,
        Op::Deleted,
    )]);
    bench.run_publish();
    let snap = bench.snapshot();
    assert_eq!(
        snap.ids.cells[slot].as_ref(),
        "",
        "a tombstoned slot's identity must empty, not linger"
    );
    drop(snap);

    bench.apply_events(&[replay::instance(
        "pod-replacement",
        "prod",
        "wl-api",
        State::OK,
        Op::Added,
    )]);
    bench.run_publish();
    let snap = bench.snapshot();
    assert_eq!(
        snap.ids.cells[slot].as_ref(),
        "pod-replacement",
        "a reused slot must carry the new identity; a selection keyed by \
         uid sees the swap where one keyed by slot would silently follow it"
    );
}

#[test]
fn a_structural_patch_deep_clones_around_a_held_reader_at_scale() {
    let spec = generate(&GenConfig {
        seed: 55,
        target_objects: 12_000,
        scenario: Scenario::Platform,
    });
    let events = k10s_clustergen::stream::snapshot(&spec, LayoutMode::Spread.emits_attachments());
    let parent = events
        .iter()
        .find_map(|event| match event {
            IngestEvent::Resource(r) if matches!(r.payload, k10s_core::Payload::Owner { .. }) => {
                Some((r.uid.clone(), r.namespace.clone()))
            }
            _ => None,
        })
        .expect("the generated stream has an owner");
    let mut bench = PublishBench::new(&events, LayoutMode::Spread);
    // The pool is three deep and each buffer's first publish is a full
    // materialize by construction; warm the last one so the measured
    // rounds prove the steady state.
    let warmup = [replay::instance(
        "pod-live-warmup",
        &parent.1,
        &parent.0,
        State::OK,
        Op::Added,
    )];
    bench.apply_events(&warmup);
    bench.run_publish();
    let before = bench.stats();

    let held = bench.snapshot();
    let held_rev = held.rev;
    let held_cells = held.cells.len();

    for round in 0..4 {
        let uid = format!("pod-live-{round}");
        let batch = [replay::instance(
            &uid,
            &parent.1,
            &parent.0,
            State::of(ReasonId::NOT_READY),
            Op::Added,
        )];
        let placed = placement(&bench.snapshot());
        bench.apply_events(&batch);
        bench.run_publish();
        assert_published_matches_full(&bench.world, &bench.snapshot());
        // The same stability question the batch-shape test asks, asked at a
        // scale where the answer is not obvious by inspection: one pod
        // arriving in a twelve-thousand-object cluster moves its own card and
        // the namespace holding it, and leaves every other object in the
        // cluster exactly where it was.
        assert_only_ancestors_moved(
            &placed,
            &placement(&bench.snapshot()),
            &touched_uids(&batch),
            &[],
            &format!("round {round} at 12k"),
        );
    }
    topology::verify_derived_state(&mut bench.world);

    let stats = bench.stats();
    let delta_patches = stats.structural_patches - before.structural_patches;
    let delta_fulls = stats.full_materializes - before.full_materializes;
    assert_eq!(
        (delta_patches, delta_fulls),
        (4, 0),
        "a one-pod change at scale must patch, never fall back: {stats:?}"
    );
    assert!(
        stats.deep_clones > before.deep_clones,
        "the held reader's buffer must be cloned around, not mutated: {stats:?}"
    );
    assert_eq!(held.rev, held_rev, "the held snapshot must not move");
    assert_eq!(held.cells.len(), held_cells);
}

#[test]
fn canonical_pod_state_keeps_the_incremental_publish_fast_path() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    let before = bench.snapshot();
    let (slot, pod) = pod_named(&before, "pod-1");
    let rect = pod.rect;
    drop(before);

    let warm = replay::instance(
        "pod-1",
        "prod",
        "wl-api",
        State::of(ReasonId::NOT_READY),
        Op::Modified,
    );
    bench.apply_events(&[warm]);
    bench.run_publish();
    let before_stats = bench.stats();

    let modified = replay::instance(
        "pod-1",
        "prod",
        "wl-api",
        State::of(ReasonId::CRASH_LOOP_BACK_OFF),
        Op::Modified,
    );
    bench.apply_events(&[modified]);
    bench.run_publish();
    let after_stats = bench.stats();
    let after = bench.snapshot();
    assert_eq!(after.cells[slot].rect, rect);
    assert_eq!(after.cells[slot].ext.state.severity, Severity::Err);
    assert_eq!(
        after_stats.full_materializes, before_stats.full_materializes,
        "state-only changes must retain patch-in-place publication"
    );
}
