//! Fixtures and assertions the world's test modules share.
//!
//! The oracles live here rather than beside one of the three suites because
//! they are what the others check themselves against: `assert_published_matches_full`
//! is the claim that an incremental publish equals a fresh materialize, and
//! `assert_only_ancestors_moved` is the layout-stability claim. A suite that
//! reimplemented either would be grading its own homework.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use k10s_clustergen::{GenConfig, Scenario, generate};
use k10s_core::{
    IngestEvent, NsNode, PodNode, ReasonId, Rect, SceneSnapshot, Severity, State, WorkloadNode,
};

use crate::Topology;
use crate::input::{self, ClusterInput};
use crate::materialize_snapshot;
use crate::{Aggregates, set_pod_state};

// The workloads a generated stream puts under each namespace, keyed by that
// namespace's uid and name together -- the uid is what an event's `parent`
// must carry, and the name is what its `namespace` field must say.
pub(crate) type OwnersByScope = std::collections::HashMap<(Arc<str>, Arc<str>), Vec<Arc<str>>>;

// Where every object in a snapshot is, keyed by the uid that names it rather
// than by the slot that holds it -- a slot is reused, and a reused slot
// compared against itself is two different objects being asked to be in the
// same place.
pub(crate) struct Placement {
    pub(crate) rect: std::collections::HashMap<Arc<str>, Rect>,
    pub(crate) parent: std::collections::HashMap<Arc<str>, Arc<str>>,
    // Which uids name workloads, so the stability oracle can tell "a card
    // rebuilt what is inside it" from "a card moved", which it may not.
    pub(crate) blocks: std::collections::HashSet<Arc<str>>,
}

pub(crate) fn placement(snap: &SceneSnapshot) -> Placement {
    // A tombstoned slot's id is the empty string, and an empty string is not
    // an object.
    fn live(uid: Option<&Arc<str>>) -> Option<Arc<str>> {
        uid.filter(|uid| !uid.is_empty()).cloned()
    }

    let mut rect = std::collections::HashMap::new();
    let mut parent = std::collections::HashMap::new();
    let mut blocks = std::collections::HashSet::new();
    for (index, node) in snap.regions.iter().enumerate() {
        if let Some(uid) = live(snap.ids.regions.get(index)) {
            rect.insert(uid, node.rect);
        }
    }
    for (index, node) in snap.blocks.iter().enumerate() {
        if let Some(uid) = live(snap.ids.blocks.get(index)) {
            rect.insert(uid.clone(), node.rect);
            blocks.insert(uid);
        }
    }
    for (index, node) in snap.cells.iter().enumerate() {
        if let Some(uid) = live(snap.ids.cells.get(index)) {
            rect.insert(uid, node.rect);
        }
    }
    for (index, node) in snap.sats.iter().enumerate() {
        if let Some(uid) = live(snap.ids.sats.get(index)) {
            rect.insert(uid, node.rect);
        }
    }
    for region in 0..snap.regions.len() {
        let Some(above) = live(snap.ids.regions.get(region)) else {
            continue;
        };
        snap.for_each_region_block(region, |block, _| {
            if let Some(uid) = live(snap.ids.blocks.get(block)) {
                parent.insert(uid, above.clone());
            }
        });
    }
    for block in 0..snap.blocks.len() {
        let Some(above) = live(snap.ids.blocks.get(block)) else {
            continue;
        };
        snap.for_each_block_cell(block, |cell, _| {
            if let Some(uid) = live(snap.ids.cells.get(cell)) {
                parent.insert(uid, above.clone());
            }
        });
        snap.for_each_block_sat(block, |sat, _| {
            if let Some(uid) = live(snap.ids.sats.get(sat)) {
                parent.insert(uid, above.clone());
            }
        });
    }
    Placement {
        rect,
        parent,
        blocks,
    }
}

// What "the map does not reshuffle" means, stated so a machine can check it.
//
// "Nothing already placed moves" is not true, and a test asserting it would
// be asserting a bug: a pod arriving on a card grows the card, and a card
// that grows can grow the namespace holding it. Growth upward is the layout
// working. What a person notices within ten seconds of a rolling update --
// and what §6.7's stability invariant is about -- is that the growth *stops
// there*: a pod arriving in one namespace must not move a workload in
// another, or a sibling pod on the same card, or anything under a namespace
// the batch never mentioned.
//
// So the invariant is that a change moves its own ancestors and nothing else,
// and the ancestors are excused by name rather than by a tolerance. The chain
// is walked in both snapshots, because a pod that changed parent has one
// ancestry before the batch and a different one after, and both of those
// cards legitimately resize.
//
// `rebuilt` names the cards -- by uid, and they are checked to be cards -- whose
// interiors the batch is entitled to rebuild: `repack_pod_grid` repacks a pod
// grid and re-orbits the satellites around it when a card's contents have gone
// far enough out of shape. It is a per-call argument and normally empty rather
// than a blanket licence, because an oracle that excuses a thing can no longer
// prove anything about it, and this one is the §6.7 invariant at twelve
// thousand objects. Naming a card here excuses its children and nothing else:
// the card itself still may not move, nor a sibling workload, nor anything
// under a card the batch never mentioned.
pub(crate) fn assert_only_ancestors_moved(
    before: &Placement,
    after: &Placement,
    touched: &[Arc<str>],
    rebuilt: &[&str],
    label: &str,
) {
    let mut excused: std::collections::HashSet<Arc<str>> = std::collections::HashSet::new();
    let mut walk: Vec<Arc<str>> = touched.to_vec();
    while let Some(uid) = walk.pop() {
        if !excused.insert(uid.clone()) {
            continue;
        }
        walk.extend(before.parent.get(&uid).cloned());
        walk.extend(after.parent.get(&uid).cloned());
    }
    for card in rebuilt {
        assert!(
            before.blocks.iter().any(|uid| uid.as_ref() == *card)
                || after.blocks.iter().any(|uid| uid.as_ref() == *card),
            "{label}: {card} is excused as a rebuilt card but is not a workload"
        );
    }
    let inside: Vec<Arc<str>> = before
        .parent
        .iter()
        .chain(after.parent.iter())
        .filter(|(_, parent)| rebuilt.contains(&parent.as_ref()))
        .map(|(uid, _)| uid.clone())
        .collect();
    excused.extend(inside);

    let mut held = 0usize;
    for (uid, was) in &before.rect {
        if excused.contains(uid) {
            continue;
        }
        // Absent afterwards is a deletion, and a deletion the batch did not
        // name is a different failure with its own tests. This one is about
        // what is still there.
        let Some(now) = after.rect.get(uid) else {
            continue;
        };
        assert_eq!(
            was, now,
            "{label}: {uid} moved, and nothing the batch touched is under it"
        );
        held += 1;
    }
    assert!(
        held > 0,
        "{label}: every object was excused as an ancestor, so this proves nothing"
    );
}

pub(crate) fn touched_uids(batch: &[IngestEvent]) -> Vec<Arc<str>> {
    batch
        .iter()
        .filter_map(|event| match event {
            IngestEvent::Resource(resource) => Some(resource.uid.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn region_named<'a>(scene: &'a SceneSnapshot, name: &str) -> (usize, &'a NsNode) {
    scene
        .regions
        .iter()
        .enumerate()
        .find(|(_, node)| node.label.as_ref() == name)
        .expect("the named region is present")
}

pub(crate) fn workload_named<'a>(
    scene: &'a SceneSnapshot,
    name: &str,
) -> (usize, &'a WorkloadNode) {
    scene
        .blocks
        .iter()
        .enumerate()
        .find(|(_, node)| node.label.as_ref() == name)
        .expect("the named workload is present")
}

pub(crate) fn pod_named<'a>(scene: &'a SceneSnapshot, name: &str) -> (usize, &'a PodNode) {
    scene
        .cells
        .iter()
        .enumerate()
        .find(|(_, node)| node.label.as_ref() == name)
        .expect("the named pod is present")
}

pub(crate) fn platform(seed: u64, target_objects: u32) -> ClusterInput {
    input_of(seed, target_objects, Scenario::Platform)
}

pub(crate) fn input_of(seed: u64, target_objects: u32, scenario: Scenario) -> ClusterInput {
    input::fold(&stream_of(seed, target_objects, scenario)).0
}

pub(crate) fn stream_of(seed: u64, target_objects: u32, scenario: Scenario) -> Vec<IngestEvent> {
    let spec = generate(&GenConfig {
        seed,
        target_objects,
        scenario,
    });
    k10s_clustergen::stream::snapshot(&spec, true)
}

pub(crate) fn st(sev: Severity) -> State {
    match sev {
        Severity::Ok => State::of(ReasonId::RUNNING),
        Severity::Unknown => State::of(ReasonId::UNKNOWN),
        Severity::Warn => State::of(ReasonId::NOT_READY),
        Severity::Err => State::of(ReasonId::CRASH_LOOP_BACK_OFF),
    }
}

pub(crate) fn flip_to_other(world: &mut World, pod: usize) {
    let cur = world.resource::<Aggregates>().pod_state[pod];
    let new = if cur.severity == Severity::Err {
        State::of(ReasonId::NOT_READY)
    } else {
        State::of(ReasonId::CRASH_LOOP_BACK_OFF)
    };
    set_pod_state(world, pod as u32, new);
}

// A published snapshot -- state-patched, structurally patched, or fully
// materialized -- must be indistinguishable from a fresh materialize:
// node for node, range for range, and through the spatial index as the
// cull actually consumes it.
pub(crate) fn assert_published_matches_full(world: &World, snap: &SceneSnapshot) {
    let topo = world.resource::<Topology>();
    let agg = world.resource::<Aggregates>();
    let full = materialize_snapshot(topo, agg, snap.rev);
    assert_eq!(
        snap.regions, full.regions,
        "regions diverged at rev {}",
        snap.rev
    );
    assert_eq!(
        snap.blocks, full.blocks,
        "blocks diverged at rev {}",
        snap.rev
    );
    assert_eq!(snap.cells, full.cells, "cells diverged at rev {}", snap.rev);
    assert_eq!(snap.sats, full.sats, "sats diverged at rev {}", snap.rev);
    assert_eq!(
        snap.region_blocks, full.region_blocks,
        "region_blocks diverged"
    );
    assert_eq!(snap.block_cells, full.block_cells, "block_cells diverged");
    assert_eq!(snap.block_sats, full.block_sats, "block_sats diverged");
    assert_eq!(snap.edges, full.edges, "edges diverged");
    assert_eq!(
        snap.region_edges, full.region_edges,
        "region_edges diverged"
    );
    assert_eq!(snap.cross_edges, full.cross_edges, "cross_edges diverged");
    assert_eq!(snap.ids, full.ids, "identity vectors diverged");
    assert_eq!(snap.totals, full.totals, "totals diverged");
    assert_eq!(snap.bounds, full.bounds, "bounds diverged");
    assert_eq!(
        snap.card_header, full.card_header,
        "the reserved card header diverged"
    );

    let policy = k10s_atlas::testing::lod_policy();
    let mut fit = k10s_atlas::Camera::default();
    fit.fit(snap.bounds, 1600.0, 1000.0);
    let cameras = [fit.zoom, 0.12, 1.0, 4.5].map(|zoom| k10s_atlas::Camera {
        cx: snap.bounds.center().0,
        cy: snap.bounds.center().1,
        zoom,
    });
    for camera in cameras {
        let blend = k10s_atlas::StageBlend::settled(policy.stage_for_zoom(camera.zoom));
        let through_patched =
            k10s_atlas::cull(snap, &camera, &policy, blend, 1600.0, 1000.0, true, false);
        let through_full =
            k10s_atlas::cull(&full, &camera, &policy, blend, 1600.0, 1000.0, true, false);
        assert_eq!(
            through_patched, through_full,
            "the cull sees different scenes at zoom {}",
            camera.zoom
        );
    }
}

pub(crate) fn assert_rollup_arithmetic(world: &World) {
    let topo = world.resource::<Topology>();
    let agg = world.resource::<Aggregates>();
    for wl in 0..topo.wl_slots.slots() {
        let pods = (0..topo.pod_slots.slots())
            .filter(|&pod| topo.pod_slots.is_active(pod) && topo.pod_wl[pod] as usize == wl);
        let cells = pods.clone().count();
        let counts = agg.wl_sev_counts[wl];
        assert_eq!(
            counts.iter().sum::<u32>() as usize,
            cells,
            "workload {wl} severity counts {counts:?} do not sum to {cells} cells"
        );
        let mut expect = [0u32; 4];
        for pod in pods.clone() {
            expect[agg.pod_state[pod].severity.rank() as usize] += 1;
        }
        assert_eq!(counts, expect, "workload {wl} severity counts drifted");
        let worst = pods
            .map(|pod| agg.pod_state[pod].severity)
            .max()
            .unwrap_or(Severity::Ok);
        assert_eq!(agg.wl_rollup[wl], worst, "workload {wl} rollup drifted");
    }
    for ns in 0..topo.ns_slots.slots() {
        let pods = (0..topo.pod_slots.slots()).filter(|&pod| {
            let workload = topo.pod_wl[pod] as usize;
            topo.pod_slots.is_active(pod)
                && topo.wl_slots.is_active(workload)
                && topo.wl_ns[workload] as usize == ns
        });
        let unhealthy = pods
            .clone()
            .filter(|&pod| agg.pod_state[pod].severity.is_unhealthy())
            .count() as u32;
        assert_eq!(
            agg.ns_unhealthy_count[ns], unhealthy,
            "namespace {ns} unhealthy count drifted"
        );
        let total = topo.ns_pod_count[ns].max(1) as f32;
        assert_eq!(
            agg.ns_unhealthy[ns],
            unhealthy as f32 / total,
            "namespace {ns} unhealthy fraction drifted"
        );
    }
}
