//! The simulation world: ingest in, immutable scene snapshots out.
//!
//! A bevy_ecs world drains a bounded `Intake` once per tick, lays out scopes
//! and owners deterministically (same seed, same scene), and publishes
//! `Arc<SceneSnapshot>` through an `ArcSwap` from a fixed-depth pool.
//! Isolation is the invariant to protect: extraction never mutates through a
//! shared `Arc`, so a reader keeps a coherent scene for as long as it holds
//! one, whatever the pool depth. Layout non-overlap, containment, and
//! determinism are tested properties, not intentions.

pub mod input;
pub mod layout;
mod topology;

// The suites live beside the code rather than inside it: 1,690 lines of tests
// in this file made the implementation hard to find, and  modules
// compile into no binary a benchmark or the app ever links, so splitting them
// costs nothing at runtime. Splitting the *implementation* the same way did
// cost something -- see benchmarks/README.md -- which is why it is still here.
#[cfg(test)]
mod publish_test;
#[cfg(test)]
mod spawn_test;
#[cfg(test)]
mod stability_test;
#[cfg(test)]
mod test_support;

use std::ops::Range;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::input::ClusterInput;
use bevy_ecs::prelude::*;
use crossbeam_channel::Receiver;
use k10s_core::{
    EdgeInst, IngestEvent, Intake, KindId, NsExt, NsNode, PodExt, PodNode, ReasonId, Rect, SatExt,
    SatNode, SceneIds, SceneSnapshot, Severity, SharedScene, State, ToolId, Totals, WlExt,
    WorkloadNode, WorldCtrl,
};
use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

pub use layout::LayoutMode;

const TICK_HZ: f32 = 20.0;

#[derive(Clone, Copy)]
struct PodDelta {
    // Aggregates owns the canonical state and is updated immediately. Carry
    // both ends of every transition so rollup can update derived counts later,
    // including several transitions of the same slot in one tick.
    slot: u32,
    old: State,
    new: State,
}

#[derive(Resource, Default)]
struct DirtyPods(Vec<PodDelta>);

fn set_pod_state(world: &mut World, idx: u32, new: State) {
    world.resource_scope(|world, mut dirty: Mut<DirtyPods>| {
        let mut aggregates = world.resource_mut::<Aggregates>();
        let current = &mut aggregates.pod_state[idx as usize];
        let old = *current;
        if old != new {
            *current = new;
            dirty.0.push(PodDelta {
                slot: idx,
                old,
                new,
            });
        }
    });
}

fn update_pod_states(world: &mut World, indices: &[u32], mut f: impl FnMut(State) -> State) {
    world.resource_scope(|world, mut dirty: Mut<DirtyPods>| {
        let mut aggregates = world.resource_mut::<Aggregates>();
        for &idx in indices {
            let current = &mut aggregates.pod_state[idx as usize];
            let old = *current;
            let new = f(old);
            if new != old {
                *current = new;
                dirty.0.push(PodDelta {
                    slot: idx,
                    old,
                    new,
                });
            }
        }
    });
}

#[derive(Resource)]
struct Topology {
    spatial_revision: u64,
    identity_revision: u64,
    ns_slots: topology::SlotMap,
    ns_labels: Vec<Arc<str>>,
    ns_rects: Vec<Rect>,
    ns_wl_range: Vec<Range<u32>>,
    region_blocks: Vec<u32>,
    ns_pod_count: Vec<u32>,
    wl_slots: topology::SlotMap,
    wl_labels: Vec<Arc<str>>,
    wl_rects: Vec<Rect>,
    wl_card_rects: Vec<Rect>,
    wl_kinds: Vec<KindId>,
    wl_tools: Vec<ToolId>,
    wl_ns: Vec<u32>,
    wl_depends_on: Vec<Vec<Arc<str>>>,
    wl_pod_range: Vec<Range<u32>>,
    block_cells: Vec<u32>,
    wl_sat_range: Vec<Range<u32>>,
    block_sats: Vec<u32>,
    pod_slots: topology::SlotMap,
    pod_labels: Vec<Arc<str>>,
    pod_rects: Vec<Rect>,
    pod_wl: Vec<u32>,
    sat_slots: topology::SlotMap,
    sat_labels: Vec<Arc<str>>,
    sat_details: Vec<Arc<str>>,
    sat_kinds: Vec<KindId>,
    sat_rects: Vec<Rect>,
    sat_wl: Vec<u32>,
    edges: Vec<EdgeInst>,
    ns_edge_range: Vec<Range<u32>>,
    cross_edge_range: Range<u32>,
    bounds: Rect,
    // The header band this layout mode reserves above a card's pod grid. Held
    // here so both publish paths can carry it onto the snapshot: the painter
    // cannot infer it, and guessing it from a card's height draws the header
    // over the first row of pods in whichever mode was not guessed for.
    card_header: f32,
}

#[derive(Resource, Debug, Clone, Default, PartialEq)]
struct Aggregates {
    pod_state: Vec<State>,
    wl_rollup: Vec<Severity>,
    ns_rollup: Vec<Severity>,
    ns_unhealthy: Vec<f32>,
    wl_sev_counts: Vec<[u32; 4]>,
    ns_sev_counts: Vec<[u32; 4]>,
    ns_unhealthy_count: Vec<u32>,
}

/// Take one pod out of a derived count.
///
/// Every decrement of a severity bucket or an unhealthy tally is paired with an
/// increment that already happened, so reaching zero here means a structural
/// path and a state path disagreed about which bucket a pod was in. A bare
/// `-= 1` wraps to `u32::MAX` in release and poisons every rollup that reads
/// the bucket afterwards; this fails the suites loudly and keeps a shipped
/// world merely wrong by one pod instead of by four billion.
#[inline(always)]
fn release_one(count: &mut u32) {
    debug_assert!(*count > 0, "a derived count lost a pod it never held");
    *count = count.saturating_sub(1);
}

fn rollup_of(counts: &[u32; 4]) -> Severity {
    if counts[3] > 0 {
        Severity::Err
    } else if counts[2] > 0 {
        Severity::Warn
    } else if counts[1] > 0 {
        Severity::Unknown
    } else {
        Severity::Ok
    }
}

#[derive(Resource)]
struct SceneOut(SharedScene);

#[derive(Resource, Default)]
struct Dirty(bool);

#[derive(Resource, Default)]
struct Rev(u64);

#[derive(Default)]
struct Pending {
    all: bool,
    pods: Vec<u32>,
    wls: Vec<u32>,
    nss: Vec<u32>,
    structural: Structural,
}

// A structural batch's footprint on one pooled snapshot: which slots need
// their node rewritten from the topology, and which derived vectors changed
// wholesale. It accumulates across batches until that buffer publishes.
#[derive(Default)]
struct Structural {
    active: bool,
    nss: Vec<u32>,
    wls: Vec<u32>,
    pods: Vec<u32>,
    sats: Vec<u32>,
    ranges_ns_wl: bool,
    ranges_wl_pod: bool,
    ranges_wl_sat: bool,
    edges: bool,
}

impl Structural {
    fn clear(&mut self) {
        self.active = false;
        self.nss.clear();
        self.wls.clear();
        self.pods.clear();
        self.sats.clear();
        self.ranges_ns_wl = false;
        self.ranges_wl_pod = false;
        self.ranges_wl_sat = false;
        self.edges = false;
    }
}

impl Pending {
    fn full() -> Self {
        Pending {
            all: true,
            ..Pending::default()
        }
    }

    fn clear(&mut self) {
        self.all = false;
        self.pods.clear();
        self.wls.clear();
        self.nss.clear();
        self.structural.clear();
    }
}

pub const SNAPSHOT_POOL_DEPTH: usize = 3;

#[derive(Resource)]
struct SnapshotPool {
    bufs: [Arc<SceneSnapshot>; SNAPSHOT_POOL_DEPTH],
    pending: [Pending; SNAPSHOT_POOL_DEPTH],
    spatial_revisions: [u64; SNAPSHOT_POOL_DEPTH],
    identity_revisions: [u64; SNAPSHOT_POOL_DEPTH],
    next: usize,
}

impl SnapshotPool {
    fn new() -> Self {
        SnapshotPool {
            bufs: std::array::from_fn(|_| Arc::new(SceneSnapshot::default())),
            pending: std::array::from_fn(|_| Pending::full()),
            spatial_revisions: [0; SNAPSHOT_POOL_DEPTH],
            identity_revisions: [0; SNAPSHOT_POOL_DEPTH],
            next: 0,
        }
    }
}

#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublishStats {
    pub publishes: u64,
    pub full_materializes: u64,
    pub structural_patches: u64,
    pub deep_clones: u64,
}

#[derive(Resource, Default)]
struct RollupScratch {
    wl_stamp: Vec<bool>,
    ns_stamp: Vec<bool>,
    wl_list: Vec<u32>,
    ns_list: Vec<u32>,
}

#[inline(never)]
fn rollup(
    mut dirty_pods: ResMut<DirtyPods>,
    topo: Res<Topology>,
    mut agg: ResMut<Aggregates>,
    mut scratch: ResMut<RollupScratch>,
    mut pool: ResMut<SnapshotPool>,
    mut dirty: ResMut<Dirty>,
) {
    if dirty_pods.0.is_empty() {
        return;
    }
    let agg = &mut *agg;
    let scratch = &mut *scratch;
    scratch.wl_stamp.resize(topo.wl_labels.len(), false);
    scratch.ns_stamp.resize(topo.ns_labels.len(), false);
    debug_assert!(scratch.wl_list.is_empty() && scratch.ns_list.is_empty());

    let mut changed = false;
    for &PodDelta { slot, old, new } in &dirty_pods.0 {
        let i = slot as usize;
        changed = true;
        for p in &mut pool.pending {
            if !p.all {
                p.pods.push(slot);
            }
        }
        if old.severity == new.severity {
            continue;
        }
        let wl = topo.pod_wl[i];
        let ns = topo.wl_ns[wl as usize] as usize;
        let sev = &mut agg.wl_sev_counts[wl as usize];
        release_one(&mut sev[old.severity.rank() as usize]);
        sev[new.severity.rank() as usize] += 1;
        let nsev = &mut agg.ns_sev_counts[ns];
        release_one(&mut nsev[old.severity.rank() as usize]);
        nsev[new.severity.rank() as usize] += 1;
        if old.severity.is_unhealthy() != new.severity.is_unhealthy() {
            if new.severity.is_unhealthy() {
                agg.ns_unhealthy_count[ns] += 1;
            } else {
                release_one(&mut agg.ns_unhealthy_count[ns]);
            }
        }
        if !std::mem::replace(&mut scratch.wl_stamp[wl as usize], true) {
            scratch.wl_list.push(wl);
        }
    }
    dirty_pods.0.clear();
    if changed {
        dirty.0 = true;
    }
    if scratch.wl_list.is_empty() {
        return;
    }

    for &wl in &scratch.wl_list {
        agg.wl_rollup[wl as usize] = rollup_of(&agg.wl_sev_counts[wl as usize]);
        let ns = topo.wl_ns[wl as usize];
        if !std::mem::replace(&mut scratch.ns_stamp[ns as usize], true) {
            scratch.ns_list.push(ns);
        }
    }
    for &ns in &scratch.ns_list {
        let total = topo.ns_pod_count[ns as usize].max(1) as f32;
        agg.ns_unhealthy[ns as usize] = agg.ns_unhealthy_count[ns as usize] as f32 / total;
        agg.ns_rollup[ns as usize] = rollup_of(&agg.ns_sev_counts[ns as usize]);
    }

    for p in &mut pool.pending {
        if !p.all {
            p.wls.extend_from_slice(&scratch.wl_list);
            p.nss.extend_from_slice(&scratch.ns_list);
        }
    }
    for &wl in &scratch.wl_list {
        scratch.wl_stamp[wl as usize] = false;
    }
    for &ns in &scratch.ns_list {
        scratch.ns_stamp[ns as usize] = false;
    }
    scratch.wl_list.clear();
    scratch.ns_list.clear();
}

fn materialize_regions(out: &mut Vec<NsNode>, topo: &Topology, agg: &Aggregates) {
    out.clear();
    out.extend(
        topo.ns_rects
            .iter()
            .zip(&topo.ns_labels)
            .zip(&topo.ns_wl_range)
            .zip(&topo.ns_pod_count)
            .zip(&agg.ns_unhealthy)
            .zip(&agg.ns_rollup)
            .map(
                |(((((&rect, label), wl_range), &pod_count), &unhealthy_frac), &rollup)| NsNode {
                    rect,
                    label: label.clone(),
                    weight: pod_count,
                    children: wl_range.clone(),
                    ext: NsExt {
                        unhealthy_frac,
                        rollup,
                    },
                },
            ),
    );
}

fn materialize_blocks(out: &mut Vec<WorkloadNode>, topo: &Topology, agg: &Aggregates) {
    out.clear();
    out.extend(
        topo.wl_rects
            .iter()
            .zip(&topo.wl_card_rects)
            .zip(&topo.wl_labels)
            .zip(&topo.wl_pod_range)
            .zip(&topo.wl_sat_range)
            .zip(&topo.wl_kinds)
            .zip(&topo.wl_tools)
            .zip(&topo.wl_ns)
            .zip(&agg.wl_rollup)
            .map(
                |(
                    (((((((&rect, &inner), label), pod_range), sat_range), &kind), &tool), &ns),
                    &rollup,
                )| WorkloadNode {
                    rect,
                    inner,
                    label: label.clone(),
                    children: pod_range.clone(),
                    sats: sat_range.clone(),
                    ext: WlExt {
                        kind,
                        tool,
                        rollup,
                        ns,
                    },
                },
            ),
    );
}

fn materialize_cells(out: &mut Vec<PodNode>, topo: &Topology, agg: &Aggregates) {
    out.clear();
    out.extend(
        topo.pod_rects
            .iter()
            .zip(&topo.pod_labels)
            .zip(&agg.pod_state)
            .map(|((&rect, label), &state)| PodNode {
                rect,
                label: label.clone(),
                ext: PodExt { state },
            }),
    );
}

fn materialize_sats(out: &mut Vec<SatNode>, topo: &Topology) {
    out.clear();
    out.extend(
        topo.sat_rects
            .iter()
            .zip(&topo.sat_labels)
            .zip(&topo.sat_kinds)
            .zip(&topo.sat_details)
            .map(|(((&rect, label), &kind), detail)| SatNode {
                rect,
                label: label.clone(),
                ext: SatExt {
                    kind,
                    detail: detail.clone(),
                },
            }),
    );
}

// Below this measured crossover, creating scoped workers costs more than the
// disjoint copies save. Above it, three coarse lanes avoid allocator-heavy
// fine-grained parallelism while preserving deterministic output.
const PARALLEL_NODE_MATERIALIZE_MIN: usize = 250_000;

fn materialize_nodes(snap: &mut SceneSnapshot, topo: &Topology, agg: &Aggregates, parallel: bool) {
    let scene = &mut snap.scene;
    let (regions, blocks, cells, sats) = (
        &mut scene.regions,
        &mut scene.blocks,
        &mut scene.cells,
        &mut scene.sats,
    );
    if !parallel {
        materialize_regions(regions, topo, agg);
        materialize_blocks(blocks, topo, agg);
        materialize_cells(cells, topo, agg);
        materialize_sats(sats, topo);
        return;
    }

    std::thread::scope(|scope| {
        let hierarchy = scope.spawn(|| {
            materialize_regions(regions, topo, agg);
            materialize_blocks(blocks, topo, agg);
        });
        let satellites = scope.spawn(|| materialize_sats(sats, topo));
        materialize_cells(cells, topo, agg);
        hierarchy
            .join()
            .expect("hierarchy materialization worker panicked");
        satellites
            .join()
            .expect("satellite materialization worker panicked");
    });
}

fn should_parallelize_node_materialization(topo: &Topology) -> bool {
    let nodes =
        topo.ns_rects.len() + topo.wl_rects.len() + topo.pod_rects.len() + topo.sat_rects.len();
    nodes >= PARALLEL_NODE_MATERIALIZE_MIN
        && std::thread::available_parallelism().is_ok_and(|parallelism| parallelism.get() >= 3)
}

fn materialize_into(
    snap: &mut SceneSnapshot,
    topo: &Topology,
    agg: &Aggregates,
    rev: u64,
    rebuild_spatial_index: bool,
    rebuild_ids: bool,
) {
    snap.rev = rev;
    snap.bounds = topo.bounds;
    snap.card_header = topo.card_header;
    snap.totals = Totals {
        regions: topo.ns_slots.active() as u32,
        blocks: topo.wl_slots.active() as u32,
        cells: topo.pod_slots.active() as u32,
        sats: topo.sat_slots.active() as u32,
        edges: topo.edges.len() as u32,
    };

    materialize_nodes(
        snap,
        topo,
        agg,
        should_parallelize_node_materialization(topo),
    );

    // Identity only moves when a slot table does, and the revision says so.
    // SlotIds shares topology's flat immutable base, so a million-slot publish
    // clones four handles instead of reference-counting a million strings; a
    // later structural patch copies only its touched 1,024-entry page.
    if rebuild_ids {
        snap.ids = Arc::new(SceneIds {
            regions: topo.ns_slots.ids().clone(),
            blocks: topo.wl_slots.ids().clone(),
            cells: topo.pod_slots.ids().clone(),
            sats: topo.sat_slots.ids().clone(),
        });
    }

    snap.region_blocks.clear();
    snap.region_blocks.extend_from_slice(&topo.region_blocks);
    snap.block_cells.clear();
    snap.block_cells.extend_from_slice(&topo.block_cells);
    snap.block_sats.clear();
    snap.block_sats.extend_from_slice(&topo.block_sats);
    if rebuild_spatial_index {
        snap.rebuild_spatial_index();
    }

    snap.edges.clear();
    snap.edges.extend_from_slice(&topo.edges);
    snap.region_edges.clear();
    snap.region_edges.extend(topo.ns_edge_range.iter().cloned());

    snap.cross_edges = topo.cross_edge_range.clone();
    snap.rebuild_edge_indexes();
}

/// The header band a layout mode reserves above a card's pod grid.
fn card_header_of(mode: LayoutMode) -> f32 {
    match mode {
        LayoutMode::Spread => k10s_core::layout::CARD_HEADER,
        LayoutMode::Dense => k10s_core::layout::WL_HEADER,
    }
}

fn materialize_snapshot(topo: &Topology, agg: &Aggregates, rev: u64) -> SceneSnapshot {
    let mut snap = SceneSnapshot::default();
    materialize_into(&mut snap, topo, agg, rev, true, true);
    snap
}

fn ns_node(topo: &Topology, agg: &Aggregates, i: usize) -> NsNode {
    NsNode {
        rect: topo.ns_rects[i],
        label: topo.ns_labels[i].clone(),
        weight: topo.ns_pod_count[i],
        children: topo.ns_wl_range[i].clone(),
        ext: NsExt {
            unhealthy_frac: agg.ns_unhealthy[i],
            rollup: agg.ns_rollup[i],
        },
    }
}

fn wl_node(topo: &Topology, agg: &Aggregates, i: usize) -> WorkloadNode {
    WorkloadNode {
        rect: topo.wl_rects[i],
        inner: topo.wl_card_rects[i],
        label: topo.wl_labels[i].clone(),
        children: topo.wl_pod_range[i].clone(),
        sats: topo.wl_sat_range[i].clone(),
        ext: WlExt {
            kind: topo.wl_kinds[i],
            tool: topo.wl_tools[i],
            rollup: agg.wl_rollup[i],
            ns: topo.wl_ns[i],
        },
    }
}

fn pod_node(topo: &Topology, agg: &Aggregates, i: usize) -> PodNode {
    PodNode {
        rect: topo.pod_rects[i],
        label: topo.pod_labels[i].clone(),
        ext: PodExt {
            state: agg.pod_state[i],
        },
    }
}

fn sat_node(topo: &Topology, i: usize) -> SatNode {
    SatNode {
        rect: topo.sat_rects[i],
        label: topo.sat_labels[i].clone(),
        ext: SatExt {
            kind: topo.sat_kinds[i],
            detail: topo.sat_details[i].clone(),
        },
    }
}

// The structural sibling of the pods/wls/nss patch loops: the snapshot
// mirrors the topology slot arrays one to one, so a structural batch patches
// the slots it touched and the derived vectors it invalidated instead of
// rebuilding fifty thousand nodes to move one pod. Isolation is untouched --
// this runs on a uniquely owned buffer exactly like the state patch.
fn patch_structural_into(
    snap: &mut SceneSnapshot,
    topo: &Topology,
    agg: &Aggregates,
    structural: &Structural,
) {
    snap.bounds = topo.bounds;
    snap.card_header = topo.card_header;
    snap.totals = Totals {
        regions: topo.ns_slots.active() as u32,
        blocks: topo.wl_slots.active() as u32,
        cells: topo.pod_slots.active() as u32,
        sats: topo.sat_slots.active() as u32,
        edges: topo.edges.len() as u32,
    };

    let tombstone: Arc<str> = Arc::from("");
    let uid_of = |slots: &topology::SlotMap, slot: usize| {
        slots
            .uid(slot as u32)
            .cloned()
            .unwrap_or_else(|| tombstone.clone())
    };
    // Deref hides the field split from the borrow checker; name both halves.
    let SceneSnapshot { scene, ids } = snap;
    let ids = Arc::make_mut(ids);
    for i in scene.regions.len()..topo.ns_rects.len() {
        scene.regions.push(ns_node(topo, agg, i));
        ids.regions.push(uid_of(&topo.ns_slots, i));
    }
    for &i in &structural.nss {
        scene.regions[i as usize] = ns_node(topo, agg, i as usize);
        ids.regions[i as usize] = uid_of(&topo.ns_slots, i as usize);
    }
    for i in scene.blocks.len()..topo.wl_rects.len() {
        scene.blocks.push(wl_node(topo, agg, i));
        ids.blocks.push(uid_of(&topo.wl_slots, i));
    }
    for &i in &structural.wls {
        scene.blocks[i as usize] = wl_node(topo, agg, i as usize);
        ids.blocks[i as usize] = uid_of(&topo.wl_slots, i as usize);
    }
    for i in scene.cells.len()..topo.pod_rects.len() {
        scene.cells.push(pod_node(topo, agg, i));
        ids.cells.push(uid_of(&topo.pod_slots, i));
    }
    for &i in &structural.pods {
        scene.cells[i as usize] = pod_node(topo, agg, i as usize);
        ids.cells[i as usize] = uid_of(&topo.pod_slots, i as usize);
    }
    for i in scene.sats.len()..topo.sat_rects.len() {
        scene.sats.push(sat_node(topo, i));
        ids.sats.push(uid_of(&topo.sat_slots, i));
    }
    for &i in &structural.sats {
        scene.sats[i as usize] = sat_node(topo, i as usize);
        ids.sats[i as usize] = uid_of(&topo.sat_slots, i as usize);
    }

    // A rebuilt adjacency renumbers every parent's range, so the ranges are
    // refreshed level-wide; that is a plain field write per parent, with no
    // label traffic behind it.
    if structural.ranges_ns_wl {
        for (node, range) in scene.regions.iter_mut().zip(&topo.ns_wl_range) {
            node.children = range.clone();
        }
        scene.region_blocks.clear();
        scene.region_blocks.extend_from_slice(&topo.region_blocks);
    }
    if structural.ranges_wl_pod {
        for (node, range) in scene.blocks.iter_mut().zip(&topo.wl_pod_range) {
            node.children = range.clone();
        }
        scene.block_cells.clear();
        scene.block_cells.extend_from_slice(&topo.block_cells);
    }
    if structural.ranges_wl_sat {
        for (node, range) in scene.blocks.iter_mut().zip(&topo.wl_sat_range) {
            node.sats = range.clone();
        }
        scene.block_sats.clear();
        scene.block_sats.extend_from_slice(&topo.block_sats);
    }

    if structural.edges {
        scene.edges.clear();
        scene.edges.extend_from_slice(&topo.edges);
        scene.region_edges.clear();
        scene
            .region_edges
            .extend(topo.ns_edge_range.iter().cloned());
        scene.cross_edges = topo.cross_edge_range.clone();
        scene.rebuild_edge_indexes();
    }

    scene.rebuild_spatial_index();
}

#[inline(never)]
fn extract(
    topo: Res<Topology>,
    agg: Res<Aggregates>,
    mut dirty: ResMut<Dirty>,
    mut rev: ResMut<Rev>,
    mut pool: ResMut<SnapshotPool>,
    mut stats: ResMut<PublishStats>,
    out: Res<SceneOut>,
) {
    if !dirty.0 && rev.0 > 0 {
        return;
    }
    dirty.0 = false;
    rev.0 += 1;
    stats.publishes += 1;

    let SnapshotPool {
        bufs,
        pending,
        spatial_revisions,
        identity_revisions,
        next,
    } = &mut *pool;
    let (buf, pending, spatial_revision, identity_revision) = (
        &mut bufs[*next],
        &mut pending[*next],
        &mut spatial_revisions[*next],
        &mut identity_revisions[*next],
    );
    if pending.all {
        stats.full_materializes += 1;
        let rebuild_spatial_index = *spatial_revision != topo.spatial_revision;
        let rebuild_ids = *identity_revision != topo.identity_revision;
        match Arc::get_mut(buf) {
            Some(snap) => {
                materialize_into(snap, &topo, &agg, rev.0, rebuild_spatial_index, rebuild_ids)
            }
            None => *buf = Arc::new(materialize_snapshot(&topo, &agg, rev.0)),
        }
        *spatial_revision = topo.spatial_revision;
        *identity_revision = topo.identity_revision;
    } else {
        if Arc::get_mut(buf).is_none() {
            stats.deep_clones += 1;
        }
        let snap = Arc::make_mut(buf);
        if pending.structural.active {
            stats.structural_patches += 1;
            patch_structural_into(snap, &topo, &agg, &pending.structural);
            *spatial_revision = topo.spatial_revision;
            *identity_revision = topo.identity_revision;
        }
        for &i in &pending.pods {
            snap.cells[i as usize].ext.state = agg.pod_state[i as usize];
        }
        for &i in &pending.wls {
            snap.blocks[i as usize].ext.rollup = agg.wl_rollup[i as usize];
        }
        for &i in &pending.nss {
            let ext = &mut snap.regions[i as usize].ext;
            ext.unhealthy_frac = agg.ns_unhealthy[i as usize];
            ext.rollup = agg.ns_rollup[i as usize];
        }
        snap.rev = rev.0;
    }
    pending.clear();
    out.0.store(buf.clone());
    *next = (*next + 1) % SNAPSHOT_POOL_DEPTH;
}

pub struct ExtractBench {
    world: World,
    schedule: Schedule,
}

impl ExtractBench {
    pub fn new(events: &[IngestEvent], mode: LayoutMode) -> Self {
        let scene = k10s_core::new_shared_scene();
        let (mut world, mut schedule) = build_world_from_stream(events, scene, mode);
        schedule.run(&mut world);
        Self { world, schedule }
    }

    #[inline(never)]
    pub fn run_extract(&mut self) {
        {
            let mut pool = self.world.resource_mut::<SnapshotPool>();
            let next = pool.next;
            pool.pending[next] = Pending::full();
        }
        self.world.resource_mut::<Dirty>().0 = true;
        self.schedule.run(&mut self.world);
    }

    pub fn snapshot(&self) -> Arc<SceneSnapshot> {
        self.world.resource::<SceneOut>().0.load_full()
    }

    pub fn stats(&self) -> PublishStats {
        *self.world.resource::<PublishStats>()
    }
}

pub struct PublishBench {
    world: World,
    schedule: Schedule,
    mode: LayoutMode,
}

impl PublishBench {
    pub fn new(events: &[IngestEvent], mode: LayoutMode) -> Self {
        let scene = k10s_core::new_shared_scene();
        let (mut world, mut schedule) = build_world_from_stream(events, scene, mode);

        schedule.run(&mut world);
        world.resource_mut::<Dirty>().0 = true;
        schedule.run(&mut world);
        Self {
            world,
            schedule,
            mode,
        }
    }

    pub fn flip_pods(&mut self, k: usize) {
        let indices: Vec<u32> = {
            let topo = self.world.resource::<Topology>();
            let n = topo.pod_slots.slots();
            let stride = (n / k.max(1)).max(1);
            (0..k.min(n)).map(|j| ((j * stride) % n) as u32).collect()
        };
        update_pod_states(&mut self.world, &indices, |cur| match cur.severity {
            Severity::Ok => State::of(ReasonId::NOT_READY),
            Severity::Warn => State::of(ReasonId::CRASH_LOOP_BACK_OFF),
            Severity::Err => State::of(ReasonId::UNKNOWN),
            Severity::Unknown => State::of(ReasonId::RUNNING),
        });
    }

    pub fn run_publish(&mut self) {
        self.schedule.run(&mut self.world);
    }

    pub fn apply_events(&mut self, events: &[IngestEvent]) {
        topology::apply_events(&mut self.world, events, self.mode);
    }

    pub fn snapshot(&self) -> Arc<SceneSnapshot> {
        self.world.resource::<SceneOut>().0.load_full()
    }

    pub fn pod_count(&self) -> usize {
        self.world.resource::<Topology>().pod_slots.active()
    }

    pub fn stats(&self) -> PublishStats {
        *self.world.resource::<PublishStats>()
    }
}

pub fn build_world_from_stream(
    events: &[IngestEvent],
    scene: SharedScene,
    mode: LayoutMode,
) -> (World, Schedule) {
    let (input, fold_stats) = input::fold(events);
    debug_assert_eq!(
        fold_stats.orphaned, 0,
        "a conforming stream leaves nothing unplaced"
    );
    build_world_owned(input, scene, mode)
}

#[cfg(test)]
fn build_world(spec: &ClusterInput, scene: SharedScene, mode: LayoutMode) -> (World, Schedule) {
    build_world_owned(spec.clone(), scene, mode)
}

fn build_world_owned(
    spec: ClusterInput,
    scene: SharedScene,
    mode: LayoutMode,
) -> (World, Schedule) {
    let lay = layout::layout(&spec, mode);
    build_world_with_layout(spec, lay, scene, mode)
}

fn build_world_with_layout(
    spec: ClusterInput,
    lay: layout::LayoutOut,
    scene: SharedScene,
    mode: LayoutMode,
) -> (World, Schedule) {
    let with_sats = mode.emits_attachments();
    let namespaces = spec.namespaces.len();
    let workloads = spec.total_workloads as usize;
    let pods = spec.total_pods as usize;
    let sats = if with_sats {
        spec.total_sats as usize
    } else {
        0
    };
    let edge_capacity = spec.total_edges as usize;

    let mut ns_slots = topology::SlotMap::with_capacity(namespaces);
    let mut ns_labels = Vec::with_capacity(namespaces);
    let mut ns_wl_range = Vec::with_capacity(namespaces);
    let mut ns_pod_range = Vec::with_capacity(namespaces);
    let mut wl_slots = topology::SlotMap::with_capacity(workloads);
    let mut wl_labels = Vec::with_capacity(workloads);
    let mut wl_kinds = Vec::with_capacity(workloads);
    let mut wl_tools = Vec::with_capacity(workloads);
    let mut wl_ns = Vec::with_capacity(workloads);
    let mut wl_depends_on = Vec::with_capacity(workloads);
    let mut wl_pod_range = Vec::with_capacity(workloads);
    let mut wl_sat_range = Vec::with_capacity(workloads);
    let mut pod_labels = Vec::with_capacity(pods);
    let mut pod_slots = topology::SlotMap::with_capacity(pods);
    let mut pod_wl = Vec::with_capacity(pods);
    let mut pod_state = Vec::with_capacity(pods);
    let mut wl_sev_counts = Vec::with_capacity(workloads);
    let mut wl_rollup = Vec::with_capacity(workloads);
    let mut ns_sev_counts = Vec::with_capacity(namespaces);
    let mut ns_rollup = Vec::with_capacity(namespaces);
    let mut ns_unhealthy_count = Vec::with_capacity(namespaces);
    let mut sat_labels = Vec::with_capacity(sats);
    let mut sat_slots = topology::SlotMap::with_capacity(sats);
    let mut sat_details = Vec::with_capacity(sats);
    let mut sat_kinds = Vec::with_capacity(sats);
    let mut sat_wl = Vec::with_capacity(sats);
    let mut edges = Vec::with_capacity(edge_capacity);
    let mut ns_edge_range = Vec::with_capacity(namespaces);
    let mut cross_pending: Vec<(u32, u32)> = Vec::with_capacity(edge_capacity);

    for (ni, ns) in spec.namespaces.into_iter().enumerate() {
        let (ns_slot, inserted) = ns_slots.insert(ns.uid);
        debug_assert!(inserted && ns_slot as usize == ni);
        let wl_start = wl_labels.len() as u32;
        let ns_pod_start = pod_labels.len() as u32;
        let mut ns_counts = [0u32; 4];
        for wl in ns.workloads {
            let wl_idx = wl_labels.len() as u32;
            let (wl_slot, inserted) = wl_slots.insert(wl.uid);
            debug_assert!(inserted && wl_slot == wl_idx);
            let pod_start = pod_labels.len() as u32;
            let mut wl_counts = [0u32; 4];
            for pod in wl.pods {
                let (pod_slot, inserted) = pod_slots.insert(pod.uid);
                debug_assert!(inserted && pod_slot as usize == pod_labels.len());
                let severity = pod.state.severity.rank() as usize;
                wl_counts[severity] += 1;
                ns_counts[severity] += 1;
                pod_labels.push(pod.name);
                pod_wl.push(wl_idx);
                pod_state.push(pod.state);
            }
            wl_rollup.push(rollup_of(&wl_counts));
            wl_sev_counts.push(wl_counts);
            let sat_start = sat_labels.len() as u32;
            if with_sats {
                for sat in wl.sats {
                    let (sat_slot, inserted) = sat_slots.insert(sat.uid);
                    debug_assert!(inserted && sat_slot as usize == sat_labels.len());
                    sat_labels.push(sat.name);
                    sat_details.push(sat.detail);
                    sat_kinds.push(sat.kind);
                    sat_wl.push(wl_idx);
                }
            }
            wl_labels.push(wl.name);
            wl_kinds.push(wl.kind);
            wl_tools.push(wl.tool);
            wl_ns.push(ni as u32);
            wl_depends_on.push(wl.depends_on);
            wl_pod_range.push(pod_start..pod_labels.len() as u32);
            wl_sat_range.push(sat_start..sat_labels.len() as u32);
        }
        ns_labels.push(ns.name);
        ns_wl_range.push(wl_start..wl_labels.len() as u32);
        ns_pod_range.push(ns_pod_start..pod_labels.len() as u32);
        ns_rollup.push(rollup_of(&ns_counts));
        ns_unhealthy_count.push(
            ns_counts[Severity::Warn.rank() as usize] + ns_counts[Severity::Err.rank() as usize],
        );
        ns_sev_counts.push(ns_counts);
    }
    debug_assert_eq!(sat_labels.len(), lay.sat_rects.len());

    // Resolve only after the permanent slot map is complete. Building a second
    // workload UID map before flattening cloned and hashed every workload just
    // to discard that index here. Namespace/workload order is already stable,
    // so local edge order remains byte-for-byte identical and cross edges still
    // occupy the tail.
    for (namespace, workloads) in ns_wl_range.iter().enumerate() {
        let edge_start = edges.len() as u32;
        for source in workloads.clone() {
            for target in &wl_depends_on[source as usize] {
                let Some(to) = wl_slots.get(target) else {
                    continue;
                };
                if wl_ns[to as usize] as usize == namespace {
                    edges.push(EdgeInst::blocks(source, to));
                } else {
                    cross_pending.push((source, to));
                }
            }
        }
        ns_edge_range.push(edge_start..edges.len() as u32);
    }
    let cross_start = edges.len() as u32;
    for (a, b) in cross_pending {
        edges.push(EdgeInst::blocks(a, b));
    }
    let cross_edge_range = cross_start..edges.len() as u32;

    let ns_unhealthy: Vec<f32> = ns_pod_range
        .iter()
        .zip(&ns_unhealthy_count)
        .map(|(r, &count)| count as f32 / (r.end - r.start).max(1) as f32)
        .collect();

    // Initial construction stays on contiguous Vec storage. Seal once all
    // slots exist so immutable snapshots can share that storage and later live
    // identity changes fall onto bounded copy-on-write pages.
    ns_slots.seal_ids();
    wl_slots.seal_ids();
    pod_slots.seal_ids();
    sat_slots.seal_ids();

    let mut world = World::new();
    world.insert_resource(Topology {
        spatial_revision: 1,
        identity_revision: 1,
        ns_slots,
        ns_labels,
        ns_rects: lay.ns_rects,
        ns_wl_range,
        region_blocks: Vec::new(),
        ns_pod_count: ns_pod_range
            .iter()
            .map(|range| range.end - range.start)
            .collect(),
        wl_slots,
        wl_labels,
        wl_rects: lay.wl_rects,
        wl_card_rects: lay.card_rects,
        wl_kinds,
        wl_tools,
        wl_ns,
        wl_depends_on,
        wl_pod_range,
        block_cells: Vec::new(),
        wl_sat_range,
        block_sats: Vec::new(),
        pod_slots,
        pod_labels,
        pod_rects: lay.pod_rects,
        pod_wl,
        sat_slots,
        sat_labels,
        sat_details,
        sat_kinds,
        sat_rects: lay.sat_rects,
        sat_wl,
        edges,
        ns_edge_range,
        cross_edge_range,
        bounds: lay.bounds,
        card_header: card_header_of(mode),
    });
    world.insert_resource(Aggregates {
        pod_state,
        wl_rollup,
        ns_rollup,
        ns_unhealthy,
        wl_sev_counts,
        ns_sev_counts,
        ns_unhealthy_count,
    });
    world.insert_resource(SceneOut(scene));
    world.insert_resource(Dirty(false));
    world.insert_resource(Rev(0));
    world.insert_resource(SnapshotPool::new());
    world.insert_resource(PublishStats::default());
    world.insert_resource(RollupScratch::default());
    world.insert_resource(DirtyPods::default());

    let mut schedule = Schedule::default();
    schedule.add_systems((rollup, extract).chain());
    (world, schedule)
}

/// Phase timings for construction of the first immutable world snapshot.
///
/// This profiles the same functions as `spawn_world`, but leaves generation and
/// rendering out so regressions can be assigned to one owner before changing
/// production code.
#[derive(Clone, Copy, Debug)]
pub struct WorldBuildProfile {
    pub fold: Duration,
    pub layout: Duration,
    pub assemble: Duration,
    pub publish: Duration,
    pub total: Duration,
}

#[inline(never)]
pub fn profile_world_build(
    events: &[IngestEvent],
    mode: LayoutMode,
) -> (WorldBuildProfile, Arc<SceneSnapshot>) {
    let started = Instant::now();

    let phase = Instant::now();
    let (input, fold_stats) = input::fold(events);
    let fold = phase.elapsed();
    debug_assert_eq!(
        fold_stats.orphaned, 0,
        "a conforming stream leaves nothing unplaced"
    );

    profile_prepared_world_build_from(input, mode, fold, started)
}

#[inline(never)]
pub fn profile_prepared_world_build(
    input: &ClusterInput,
    mode: LayoutMode,
) -> (WorldBuildProfile, Arc<SceneSnapshot>) {
    let owned = input.clone();
    profile_prepared_world_build_from(owned, mode, Duration::ZERO, Instant::now())
}

fn profile_prepared_world_build_from(
    input: ClusterInput,
    mode: LayoutMode,
    fold: Duration,
    started: Instant,
) -> (WorldBuildProfile, Arc<SceneSnapshot>) {
    let phase = Instant::now();
    let layout = layout::layout(&input, mode);
    let layout_elapsed = phase.elapsed();

    let phase = Instant::now();
    let scene = k10s_core::new_shared_scene();
    let (mut world, mut schedule) = build_world_with_layout(input, layout, scene, mode);
    let assemble = phase.elapsed();

    let phase = Instant::now();
    schedule.run(&mut world);
    let publish = phase.elapsed();
    let snapshot = world.resource::<SceneOut>().0.load_full();

    (
        WorldBuildProfile {
            fold,
            layout: layout_elapsed,
            assemble,
            publish,
            total: started.elapsed(),
        },
        snapshot,
    )
}

fn weighted_state(rng: &mut ChaCha8Rng) -> State {
    match rng.random_range(0..100u32) {
        0..90 => State::of(ReasonId::RUNNING),
        90..94 => State::of(ReasonId::NOT_READY),
        94..98 => State::of(ReasonId::CRASH_LOOP_BACK_OFF),
        _ => State::of(ReasonId::UNKNOWN),
    }
}

/// One complete scene at the world's ownership boundary.
///
/// Cluster snapshots arrive in their native event representation. Sources
/// that already own a hierarchy keep it, avoiding a flatten/fold round trip.
pub enum WorldSeed {
    Events(Vec<IngestEvent>),
    Prepared(ClusterInput),
}

impl From<Vec<IngestEvent>> for WorldSeed {
    fn from(events: Vec<IngestEvent>) -> Self {
        Self::Events(events)
    }
}

impl From<ClusterInput> for WorldSeed {
    fn from(scene: ClusterInput) -> Self {
        Self::Prepared(scene)
    }
}

fn build_seed(seed: WorldSeed, scene: SharedScene, mode: LayoutMode) -> (World, Schedule) {
    match seed {
        WorldSeed::Events(events) => build_world_from_stream(&events, scene, mode),
        WorldSeed::Prepared(prepared) => build_world_owned(prepared, scene, mode),
    }
}

pub fn spawn_world(
    seed_scene: impl Into<WorldSeed>,
    live: Receiver<IngestEvent>,
    scene: SharedScene,
    ctrl: Receiver<WorldCtrl>,
    seed: u64,
    churn_rate: f32,
    mode: LayoutMode,
    on_publish: impl FnMut() + Send + 'static,
) -> JoinHandle<()> {
    spawn_world_boxed(
        seed_scene.into(),
        live,
        scene,
        ctrl,
        seed,
        churn_rate,
        mode,
        Box::new(on_publish),
    )
}

fn spawn_world_boxed(
    seed_scene: WorldSeed,
    live: Receiver<IngestEvent>,
    scene: SharedScene,
    ctrl: Receiver<WorldCtrl>,
    seed: u64,
    churn_rate: f32,
    mode: LayoutMode,
    mut on_publish: Box<dyn FnMut() + Send + 'static>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("k10s-world".into())
        .spawn(move || {
            let (mut world, mut schedule) = build_seed(seed_scene, scene.clone(), mode);
            let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0xC0FFEE);
            let tick = Duration::from_secs_f32(1.0 / TICK_HZ);
            let mut churn_rate = churn_rate;
            let mut churn_on = true;
            let mut carry = 0.0f32;
            let mut published_rev = 0u64;
            let mut intake = Intake::new();
            let mut batch = Vec::new();
            let mut pending_ctrl = None;

            loop {
                let start = Instant::now();
                for msg in pending_ctrl.take().into_iter().chain(ctrl.try_iter()) {
                    let replacement = match msg {
                        WorldCtrl::SetChurn(on) => {
                            churn_on = on;
                            None
                        }
                        // The carry is dropped with the rate: keeping a
                        // fractional flip banked at 120/s and spending it after
                        // a real cluster arrives is exactly the transition
                        // nobody asked for.
                        WorldCtrl::SetChurnRate(rate) => {
                            churn_rate = rate.max(0.0);
                            carry = 0.0;
                            None
                        }
                        WorldCtrl::Rebuild(stream) => Some(WorldSeed::Events(stream)),
                        WorldCtrl::RebuildPrepared(prepared) => Some(WorldSeed::Prepared(prepared)),
                        WorldCtrl::Shutdown => return,
                    };
                    let Some(replacement) = replacement else {
                        continue;
                    };
                    // Everything still queued belongs to the scene being
                    // replaced. Whoever sends this has already stopped what was
                    // producing that -- it joins the forwarding thread first --
                    // so draining discards exactly the old scene's tail and
                    // nothing else. Control and events are separate channels;
                    // carrying either whole seed makes replacement one act.
                    for _ in live.try_iter() {}
                    intake = Intake::new();
                    let rebuilt = build_seed(replacement, scene.clone(), mode);
                    world = rebuilt.0;
                    schedule = rebuilt.1;
                    // A replacement is a new snapshot in the same process, not
                    // a new revision domain. Give its initially-dirty extract a
                    // floor above every scene already published. This also
                    // distinguishes a legitimately empty replacement from the
                    // empty shell without object-count or thread-order guesses.
                    world.resource_mut::<Rev>().0 = published_rev.max(1);
                    world.resource_mut::<Dirty>().0 = true;
                    carry = 0.0;
                }

                for event in live.try_iter() {
                    intake.push(event);
                }
                if !intake.is_empty() {
                    intake.drain_into(&mut batch);
                    topology::apply_events(&mut world, &batch, mode);
                    batch.clear();
                }

                if churn_on {
                    carry += churn_rate / TICK_HZ;
                    let flips = carry as usize;
                    carry -= flips as f32;
                    let n = world.resource::<Topology>().pod_slots.slots();
                    if n > 0 {
                        for _ in 0..flips {
                            let i = rng.random_range(0..n);
                            if !world.resource::<Topology>().pod_slots.is_active(i) {
                                continue;
                            }
                            let new = weighted_state(&mut rng);
                            set_pod_state(&mut world, i as u32, new);
                        }
                    }
                }

                schedule.run(&mut world);

                let rev = world.resource::<Rev>().0;
                if rev != published_rev {
                    published_rev = rev;
                    on_publish();
                }

                if let Some(rest) = tick.checked_sub(start.elapsed()) {
                    match ctrl.recv_timeout(rest) {
                        Ok(message) => pending_ctrl = Some(message),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        })
        .expect("spawn k10s-world thread")
}
