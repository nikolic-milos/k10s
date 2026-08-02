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

use std::ops::Range;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::input::ClusterInput;
use bevy_ecs::prelude::*;
use crossbeam_channel::Receiver;
use k10s_core::{
    EdgeInst, IngestEvent, Intake, KindId, NsExt, NsNode, PodExt, PodNode, ReasonId, Rect, SatExt,
    SatNode, SceneSnapshot, Severity, SharedScene, State, ToolId, Totals, WlExt, WorkloadNode,
    WorldCtrl,
};
use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

pub use layout::LayoutMode;

const TICK_HZ: f32 = 20.0;

#[derive(Component)]
struct PodH(State);

#[derive(Resource, Default)]
struct DirtyPods(Vec<(u32, State)>);

fn set_pod_state(world: &mut World, idx: u32, new: State) {
    let e = world.resource::<Topology>().pod_entities[idx as usize];
    let changed = match world.get_mut::<PodH>(e) {
        Some(mut h) if h.0 != new => {
            h.0 = new;
            true
        }
        _ => false,
    };
    if changed {
        world.resource_mut::<DirtyPods>().0.push((idx, new));
    }
}

fn update_pod_states(world: &mut World, indices: &[u32], mut f: impl FnMut(State) -> State) {
    let entities: Vec<Entity> = {
        let topo = world.resource::<Topology>();
        indices
            .iter()
            .map(|&i| topo.pod_entities[i as usize])
            .collect()
    };
    world.resource_scope(|world, mut dirty: Mut<DirtyPods>| {
        for (&idx, e) in indices.iter().zip(entities) {
            if let Some(mut h) = world.get_mut::<PodH>(e) {
                let new = f(h.0);
                if new != h.0 {
                    h.0 = new;
                    dirty.0.push((idx, new));
                }
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
    pod_entities: Vec<Entity>,
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
}

#[derive(Resource, Debug, Clone, PartialEq)]
struct Aggregates {
    pod_state: Vec<State>,
    wl_rollup: Vec<Severity>,
    ns_rollup: Vec<Severity>,
    ns_unhealthy: Vec<f32>,
    wl_sev_counts: Vec<[u32; 4]>,
    ns_sev_counts: Vec<[u32; 4]>,
    ns_unhealthy_count: Vec<u32>,
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
    for &(iu, new) in &dirty_pods.0 {
        let i = iu as usize;
        let old = agg.pod_state[i];
        if old == new {
            continue;
        }
        agg.pod_state[i] = new;
        changed = true;
        for p in &mut pool.pending {
            if !p.all {
                p.pods.push(iu);
            }
        }
        if old.severity == new.severity {
            continue;
        }
        let wl = topo.pod_wl[i];
        let ns = topo.wl_ns[wl as usize] as usize;
        let sev = &mut agg.wl_sev_counts[wl as usize];
        sev[old.severity.rank() as usize] -= 1;
        sev[new.severity.rank() as usize] += 1;
        let nsev = &mut agg.ns_sev_counts[ns];
        nsev[old.severity.rank() as usize] -= 1;
        nsev[new.severity.rank() as usize] += 1;
        if old.severity.is_unhealthy() != new.severity.is_unhealthy() {
            if new.severity.is_unhealthy() {
                agg.ns_unhealthy_count[ns] += 1;
            } else {
                agg.ns_unhealthy_count[ns] -= 1;
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
    snap.totals = Totals {
        regions: topo.ns_slots.active() as u32,
        blocks: topo.wl_slots.active() as u32,
        cells: topo.pod_slots.active() as u32,
        sats: topo.sat_slots.active() as u32,
        edges: topo.edges.len() as u32,
    };

    snap.regions.clear();
    snap.regions.extend(
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

    snap.blocks.clear();
    snap.blocks.extend(
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

    snap.cells.clear();
    snap.cells.extend(
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

    snap.sats.clear();
    snap.sats.extend(
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

    // Identity only moves when a slot table does, and the revision says so;
    // skipping the rebuild spares two atomic operations per object on the
    // repeated-materialize paths. The ids are Arc-shared so a snapshot clone
    // costs one reference bump; make_mut pays copy-on-write only when a
    // reader still holds the previous identity.
    if rebuild_ids {
        let tombstone: Arc<str> = Arc::from("");
        let uid_of = |slots: &topology::SlotMap, slot: usize| {
            slots
                .uid(slot as u32)
                .cloned()
                .unwrap_or_else(|| tombstone.clone())
        };
        let ids = Arc::make_mut(&mut snap.ids);
        ids.regions.clear();
        ids.regions
            .extend((0..topo.ns_slots.slots()).map(|slot| uid_of(&topo.ns_slots, slot)));
        ids.blocks.clear();
        ids.blocks
            .extend((0..topo.wl_slots.slots()).map(|slot| uid_of(&topo.wl_slots, slot)));
        ids.cells.clear();
        ids.cells
            .extend((0..topo.pod_slots.slots()).map(|slot| uid_of(&topo.pod_slots, slot)));
        ids.sats.clear();
        ids.sats
            .extend((0..topo.sat_slots.slots()).map(|slot| uid_of(&topo.sat_slots, slot)));
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
            let n = topo.pod_entities.len();
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
    build_world(&input, scene, mode)
}

fn build_world(spec: &ClusterInput, scene: SharedScene, mode: LayoutMode) -> (World, Schedule) {
    let lay = layout::layout(spec, mode);
    let owner_index = spec.owner_indices();
    let with_sats = mode.emits_attachments();

    let mut ns_slots = topology::SlotMap::default();
    let mut ns_labels = Vec::new();
    let mut ns_wl_range = Vec::new();
    let mut ns_pod_range = Vec::new();
    let mut wl_slots = topology::SlotMap::default();
    let mut wl_labels = Vec::new();
    let mut wl_kinds = Vec::new();
    let mut wl_tools = Vec::new();
    let mut wl_ns = Vec::new();
    let mut wl_depends_on = Vec::new();
    let mut wl_pod_range = Vec::new();
    let mut wl_sat_range = Vec::new();
    let mut pod_labels = Vec::new();
    let mut pod_slots = topology::SlotMap::default();
    let mut pod_wl = Vec::new();
    let mut pod_state = Vec::new();
    let mut sat_labels = Vec::new();
    let mut sat_slots = topology::SlotMap::default();
    let mut sat_details = Vec::new();
    let mut sat_kinds = Vec::new();
    let mut sat_wl = Vec::new();
    let mut edges = Vec::new();
    let mut ns_edge_range = Vec::with_capacity(spec.namespaces.len());
    let mut cross_pending: Vec<(u32, u32)> = Vec::new();

    for (ni, ns) in spec.namespaces.iter().enumerate() {
        let (ns_slot, inserted) = ns_slots.insert(ns.uid.clone());
        debug_assert!(inserted && ns_slot as usize == ni);
        let wl_start = wl_labels.len() as u32;
        let ns_pod_start = pod_labels.len() as u32;
        let edge_start = edges.len() as u32;
        for wl in &ns.workloads {
            let wl_idx = wl_labels.len() as u32;
            let (wl_slot, inserted) = wl_slots.insert(wl.uid.clone());
            debug_assert!(inserted && wl_slot == wl_idx);
            let pod_start = pod_labels.len() as u32;
            for pod in &wl.pods {
                let (pod_slot, inserted) = pod_slots.insert(pod.uid.clone());
                debug_assert!(inserted && pod_slot as usize == pod_labels.len());
                pod_labels.push(pod.name.clone());
                pod_wl.push(wl_idx);
                pod_state.push(pod.state);
            }
            let sat_start = sat_labels.len() as u32;
            if with_sats {
                for sat in &wl.sats {
                    let (sat_slot, inserted) = sat_slots.insert(sat.uid.clone());
                    debug_assert!(inserted && sat_slot as usize == sat_labels.len());
                    sat_labels.push(sat.name.clone());
                    sat_details.push(sat.detail.clone());
                    sat_kinds.push(sat.kind);
                    sat_wl.push(wl_idx);
                }
            }
            for target in &wl.depends_on {
                let Some(&to) = owner_index.get(target) else {
                    continue;
                };
                if to >= wl_start && to < wl_start + ns.workloads.len() as u32 {
                    edges.push(EdgeInst::blocks(wl_idx, to));
                } else {
                    cross_pending.push((wl_idx, to));
                }
            }
            wl_labels.push(wl.name.clone());
            wl_kinds.push(wl.kind);
            wl_tools.push(wl.tool);
            wl_ns.push(ni as u32);
            wl_depends_on.push(wl.depends_on.clone());
            wl_pod_range.push(pod_start..pod_labels.len() as u32);
            wl_sat_range.push(sat_start..sat_labels.len() as u32);
        }
        ns_labels.push(ns.name.clone());
        ns_wl_range.push(wl_start..wl_labels.len() as u32);
        ns_pod_range.push(ns_pod_start..pod_labels.len() as u32);
        ns_edge_range.push(edge_start..edges.len() as u32);
    }
    debug_assert_eq!(sat_labels.len(), lay.sat_rects.len());

    let cross_start = edges.len() as u32;
    for (a, b) in cross_pending {
        edges.push(EdgeInst::blocks(a, b));
    }
    let cross_edge_range = cross_start..edges.len() as u32;

    let sev_counts = |r: &Range<u32>| {
        let mut counts = [0u32; 4];
        for i in r.start as usize..r.end as usize {
            counts[pod_state[i].severity.rank() as usize] += 1;
        }
        counts
    };
    let wl_sev_counts: Vec<[u32; 4]> = wl_pod_range.iter().map(sev_counts).collect();
    let wl_rollup: Vec<Severity> = wl_sev_counts.iter().map(rollup_of).collect();
    let ns_sev_counts: Vec<[u32; 4]> = ns_pod_range.iter().map(sev_counts).collect();
    let ns_rollup: Vec<Severity> = ns_sev_counts.iter().map(rollup_of).collect();
    let ns_unhealthy_count: Vec<u32> = ns_pod_range
        .iter()
        .map(|r| {
            (r.start as usize..r.end as usize)
                .filter(|&i| pod_state[i].severity.is_unhealthy())
                .count() as u32
        })
        .collect();
    let ns_unhealthy: Vec<f32> = ns_pod_range
        .iter()
        .zip(&ns_unhealthy_count)
        .map(|(r, &count)| count as f32 / (r.end - r.start).max(1) as f32)
        .collect();

    let mut world = World::new();
    let pod_entities: Vec<Entity> = world
        .spawn_batch(pod_state.iter().map(|&h| (PodH(h),)).collect::<Vec<_>>())
        .collect();

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
        pod_entities,
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

fn weighted_state(rng: &mut ChaCha8Rng) -> State {
    match rng.random_range(0..100u32) {
        0..90 => State::of(ReasonId::RUNNING),
        90..94 => State::of(ReasonId::NOT_READY),
        94..98 => State::of(ReasonId::CRASH_LOOP_BACK_OFF),
        _ => State::of(ReasonId::UNKNOWN),
    }
}

pub fn spawn_world(
    events: Vec<IngestEvent>,
    live: Receiver<IngestEvent>,
    scene: SharedScene,
    ctrl: Receiver<WorldCtrl>,
    seed: u64,
    churn_rate: f32,
    mode: LayoutMode,
    on_publish: impl FnMut() + Send + 'static,
) -> JoinHandle<()> {
    spawn_world_boxed(
        events,
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
    events: Vec<IngestEvent>,
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
            let (mut world, mut schedule) = build_world_from_stream(&events, scene, mode);
            drop(events);
            let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0xC0FFEE);
            let tick = Duration::from_secs_f32(1.0 / TICK_HZ);
            let mut churn_on = true;
            let mut carry = 0.0f32;
            let mut published_rev = 0u64;
            let mut intake = Intake::new();
            let mut batch = Vec::new();

            loop {
                let start = Instant::now();
                for msg in ctrl.try_iter() {
                    match msg {
                        WorldCtrl::SetChurn(on) => churn_on = on,
                        WorldCtrl::Shutdown => return,
                    }
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
                    let n = world.resource::<Topology>().pod_entities.len();
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
                    std::thread::sleep(rest);
                }
            }
        })
        .expect("spawn k10s-world thread")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use k10s_clustergen::{GenConfig, Scenario, generate};
    use k10s_core::{Level, Op, replay};

    fn region_named<'a>(scene: &'a SceneSnapshot, name: &str) -> (usize, &'a NsNode) {
        scene
            .regions
            .iter()
            .enumerate()
            .find(|(_, node)| node.label.as_ref() == name)
            .expect("the named region is present")
    }

    fn workload_named<'a>(scene: &'a SceneSnapshot, name: &str) -> (usize, &'a WorkloadNode) {
        scene
            .blocks
            .iter()
            .enumerate()
            .find(|(_, node)| node.label.as_ref() == name)
            .expect("the named workload is present")
    }

    fn pod_named<'a>(scene: &'a SceneSnapshot, name: &str) -> (usize, &'a PodNode) {
        scene
            .cells
            .iter()
            .enumerate()
            .find(|(_, node)| node.label.as_ref() == name)
            .expect("the named pod is present")
    }

    fn platform(seed: u64, target_objects: u32) -> ClusterInput {
        input_of(seed, target_objects, Scenario::Platform)
    }

    fn input_of(seed: u64, target_objects: u32, scenario: Scenario) -> ClusterInput {
        input::fold(&stream_of(seed, target_objects, scenario)).0
    }

    fn stream_of(seed: u64, target_objects: u32, scenario: Scenario) -> Vec<IngestEvent> {
        let spec = generate(&GenConfig {
            seed,
            target_objects,
            scenario,
        });
        k10s_clustergen::stream::snapshot(&spec, true)
    }

    fn st(sev: Severity) -> State {
        match sev {
            Severity::Ok => State::of(ReasonId::RUNNING),
            Severity::Unknown => State::of(ReasonId::UNKNOWN),
            Severity::Warn => State::of(ReasonId::NOT_READY),
            Severity::Err => State::of(ReasonId::CRASH_LOOP_BACK_OFF),
        }
    }

    fn flip_to_other(world: &mut World, pod: usize) {
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
    fn assert_published_matches_full(world: &World, snap: &SceneSnapshot) {
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

    fn assert_rollup_arithmetic(world: &World) {
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
        let pinned_health: Vec<Severity> =
            pinned.cells.iter().map(|c| c.ext.state.severity).collect();
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
        let owner_index = spec.owner_indices();
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
            bench.apply_events(batch);
            bench.run_publish();
            topology::verify_derived_state(&mut bench.world);
            assert_published_matches_full(&bench.world, &bench.snapshot());
            let snapshot = bench.snapshot();
            assert!(
                snapshot.rev > index as u64,
                "each structural batch must publish"
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
        let events =
            k10s_clustergen::stream::snapshot(&spec, LayoutMode::Spread.emits_attachments());
        let parent = events
            .iter()
            .find_map(|event| match event {
                IngestEvent::Resource(r)
                    if matches!(r.payload, k10s_core::Payload::Owner { .. }) =>
                {
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
            bench.apply_events(&batch);
            bench.run_publish();
            assert_published_matches_full(&bench.world, &bench.snapshot());
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
    fn live_topology_uses_stable_slots_without_moving_existing_nodes() {
        let initial = replay::initial_sync();
        let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let before = bench.snapshot();
        let (prod_slot, prod) = region_named(&before, "prod");
        let (api_slot, api) = workload_named(&before, "api");
        let (pod_slot, pod) = pod_named(&before, "pod-1");
        let (prod_rect, api_rect, pod_rect) = (prod.rect, api.inner, pod.rect);
        drop(before);

        let added = [
            replay::scope("ns-canary", "canary", Op::Added),
            replay::owner("wl-canary", "canary", "edge", KindId::DEPLOYMENT, Op::Added),
            replay::instance("pod-canary", "canary", "wl-canary", State::OK, Op::Added),
        ];
        bench.apply_events(&added);
        bench.run_publish();
        let grown = bench.snapshot();
        assert_eq!(grown.totals.regions, 2);
        assert_eq!(grown.totals.blocks, 2);
        assert_eq!(grown.totals.cells, 3);
        assert_eq!(region_named(&grown, "prod").0, prod_slot);
        assert_eq!(workload_named(&grown, "api").0, api_slot);
        assert_eq!(pod_named(&grown, "pod-1").0, pod_slot);
        assert_eq!(grown.regions[prod_slot].rect, prod_rect);
        assert_eq!(grown.blocks[api_slot].inner, api_rect);
        assert_eq!(grown.cells[pod_slot].rect, pod_rect);

        let (canary_slot, _) = region_named(&grown, "canary");
        let (edge_slot, _) = workload_named(&grown, "edge");
        let (canary_pod_slot, _) = pod_named(&grown, "pod-canary");
        assert_eq!(
            grown.region_block_indices(canary_slot).collect::<Vec<_>>(),
            [edge_slot]
        );
        assert_eq!(
            grown.block_cell_indices(edge_slot).collect::<Vec<_>>(),
            [canary_pod_slot]
        );
        drop(grown);

        let deleted = [
            replay::instance("pod-canary", "canary", "wl-canary", State::OK, Op::Deleted),
            replay::owner(
                "wl-canary",
                "canary",
                "edge",
                KindId::DEPLOYMENT,
                Op::Deleted,
            ),
            replay::scope("ns-canary", "canary", Op::Deleted),
        ];
        bench.apply_events(&deleted);
        bench.run_publish();
        let shrunk = bench.snapshot();
        assert_eq!(shrunk.totals.regions, 1);
        assert_eq!(shrunk.totals.blocks, 1);
        assert_eq!(shrunk.totals.cells, 2);
        assert_eq!(shrunk.regions[prod_slot].rect, prod_rect);
        assert_eq!(shrunk.blocks[api_slot].inner, api_rect);
        assert_eq!(shrunk.cells[pod_slot].rect, pod_rect);
        assert!(
            shrunk
                .regions
                .iter()
                .all(|node| node.label.as_ref() != "canary")
        );
        drop(shrunk);

        bench.apply_events(&added);
        bench.run_publish();
        let readded = bench.snapshot();
        assert_eq!(region_named(&readded, "canary").0, canary_slot);
        assert_eq!(workload_named(&readded, "edge").0, edge_slot);
        assert_eq!(pod_named(&readded, "pod-canary").0, canary_pod_slot);
    }

    #[test]
    fn an_initial_stream_replays_changes_before_its_first_snapshot() {
        let mut events = replay::initial_sync().events;
        events.push(replay::instance(
            "pod-1",
            "prod",
            "wl-api",
            State::of(ReasonId::CRASH_LOOP_BACK_OFF),
            Op::Modified,
        ));
        events.push(replay::instance(
            "pod-2",
            "prod",
            "wl-api",
            State::OK,
            Op::Deleted,
        ));

        let bench = PublishBench::new(&events, LayoutMode::Spread);
        let snapshot = bench.snapshot();
        assert_eq!(snapshot.totals.cells, 1);
        assert_eq!(
            pod_named(&snapshot, "pod-1").1.ext.state.severity,
            Severity::Err
        );
        assert!(
            snapshot
                .cells
                .iter()
                .all(|pod| pod.label.as_ref() != "pod-2")
        );
    }

    #[test]
    fn live_pod_health_keeps_the_incremental_publish_fast_path() {
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

    #[test]
    fn adding_a_pod_grows_its_card_without_moving_occupied_slots() {
        let initial = replay::initial_sync();
        let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let before = bench.snapshot();
        let (api_slot, api) = workload_named(&before, "api");
        let first = pod_named(&before, "pod-1").1.rect;
        let second = pod_named(&before, "pod-2").1.rect;
        let card = api.inner;
        drop(before);

        bench.apply_events(&[replay::instance(
            "pod-3",
            "prod",
            "wl-api",
            State::OK,
            Op::Added,
        )]);
        bench.run_publish();
        let after = bench.snapshot();
        assert_eq!(pod_named(&after, "pod-1").1.rect, first);
        assert_eq!(pod_named(&after, "pod-2").1.rect, second);
        assert!(after.blocks[api_slot].inner.h >= card.h);
        assert_eq!(after.totals.cells, 3);
    }

    #[test]
    fn publish_hook_fires_per_snapshot_not_per_tick() {
        let scene = k10s_core::new_shared_scene();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
        let world = spawn_world(
            stream_of(2, 500, Scenario::Platform),
            crossbeam_channel::never(),
            scene.clone(),
            ctrl_rx,
            2,
            0.0,
            LayoutMode::Spread,
            move || {
                let _ = wake_tx.send(());
            },
        );

        wake_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("initial publish must fire the hook");
        assert_eq!(scene.load().rev, 1);
        assert!(
            wake_rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "no rev bump -> no wake"
        );

        ctrl_tx.send(WorldCtrl::Shutdown).unwrap();
        world.join().unwrap();
    }
}
