pub mod layout;

use std::ops::Range;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;
use crossbeam_channel::Receiver;
use k10s_clustergen::ClusterSpec;
use k10s_core::{
    EdgeInst, KindId, NsExt, NsNode, PodExt, PodNode, ReasonId, Rect, SatExt, SatNode,
    SceneSnapshot, Severity, SharedScene, State, ToolId, Totals, WlExt, WorkloadNode, WorldCtrl,
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
    ns_labels: Vec<Arc<str>>,
    ns_rects: Vec<Rect>,
    ns_wl_range: Vec<Range<u32>>,
    ns_pod_range: Vec<Range<u32>>,
    wl_labels: Vec<Arc<str>>,
    wl_rects: Vec<Rect>,
    wl_card_rects: Vec<Rect>,
    wl_kinds: Vec<KindId>,
    wl_tools: Vec<ToolId>,
    wl_ns: Vec<u32>,
    wl_pod_range: Vec<Range<u32>>,
    wl_sat_range: Vec<Range<u32>>,
    pod_labels: Vec<Arc<str>>,
    pod_rects: Vec<Rect>,
    pod_wl: Vec<u32>,
    pod_entities: Vec<Entity>,
    sat_labels: Vec<Arc<str>>,
    sat_details: Vec<Arc<str>>,
    sat_kinds: Vec<KindId>,
    sat_rects: Vec<Rect>,
    edges: Vec<EdgeInst>,
    ns_edge_range: Vec<Range<u32>>,
    /// Tail of `edges` holding links whose ends live in different regions, so the
    /// culler cannot reach them through any single region's range.
    cross_edge_range: Range<u32>,
    bounds: Rect,
}

#[derive(Resource)]
struct Aggregates {
    pod_state: Vec<State>,
    wl_rollup: Vec<Severity>,
    ns_rollup: Vec<Severity>,
    ns_unhealthy: Vec<f32>,
    wl_sev_counts: Vec<[u32; 4]>,
    /// Per-severity histograms, kept per scope for the same reason they are kept
    /// per owner: a max cannot be lowered incrementally, but a histogram can, so
    /// a pod leaving Err drops the rollup without rescanning the scope.
    ns_sev_counts: Vec<[u32; 4]>,
    ns_unhealthy_count: Vec<u32>,
}

/// The highest severity present in a per-severity histogram.
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
    }
}

pub const SNAPSHOT_POOL_DEPTH: usize = 3;

#[derive(Resource)]
struct SnapshotPool {
    bufs: [Arc<SceneSnapshot>; SNAPSHOT_POOL_DEPTH],
    pending: [Pending; SNAPSHOT_POOL_DEPTH],
    next: usize,
}

impl SnapshotPool {
    fn new() -> Self {
        SnapshotPool {
            bufs: std::array::from_fn(|_| Arc::new(SceneSnapshot::default())),
            pending: std::array::from_fn(|_| Pending::full()),
            next: 0,
        }
    }
}

#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublishStats {
    pub publishes: u64,
    pub full_materializes: u64,
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

    for &(iu, new) in &dirty_pods.0 {
        let i = iu as usize;
        let old = agg.pod_state[i];
        if old == new {
            continue;
        }
        agg.pod_state[i] = new;
        for p in &mut pool.pending {
            if !p.all {
                p.pods.push(iu);
            }
        }
        if old.severity == new.severity {
            // The reason moved but the severity did not, so the pod repaints and
            // every rollup above it is provably unchanged. Skipping the histogram
            // work here is what keeps a reason-only update O(1).
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
        let range = &topo.ns_pod_range[ns as usize];
        let total = (range.end - range.start).max(1) as f32;
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
    dirty.0 = true;
}

fn materialize_into(snap: &mut SceneSnapshot, topo: &Topology, agg: &Aggregates, rev: u64) {
    snap.rev = rev;
    snap.bounds = topo.bounds;
    snap.totals = Totals {
        regions: topo.ns_labels.len() as u32,
        blocks: topo.wl_labels.len() as u32,
        cells: topo.pod_labels.len() as u32,
        sats: topo.sat_labels.len() as u32,
        edges: topo.edges.len() as u32,
    };

    snap.regions.clear();
    snap.regions.extend(
        topo.ns_rects
            .iter()
            .zip(&topo.ns_labels)
            .zip(&topo.ns_wl_range)
            .zip(&topo.ns_pod_range)
            .zip(&agg.ns_unhealthy)
            .zip(&agg.ns_rollup)
            .map(
                |(((((&rect, label), wl_range), pod_range), &unhealthy_frac), &rollup)| NsNode {
                    rect,
                    label: label.clone(),
                    weight: pod_range.end - pod_range.start,
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
                )| {
                    WorkloadNode {
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
                    }
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

    snap.edges.clear();
    snap.edges.extend_from_slice(&topo.edges);
    snap.region_edges.clear();
    snap.region_edges.extend(topo.ns_edge_range.iter().cloned());

    snap.cross_edges = topo.cross_edge_range.clone();
}

fn materialize_snapshot(topo: &Topology, agg: &Aggregates, rev: u64) -> SceneSnapshot {
    let mut snap = SceneSnapshot::default();
    materialize_into(&mut snap, topo, agg, rev);
    snap
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
        next,
    } = &mut *pool;
    let (buf, pending) = (&mut bufs[*next], &mut pending[*next]);
    if pending.all {
        stats.full_materializes += 1;
        match Arc::get_mut(buf) {
            Some(snap) => materialize_into(snap, &topo, &agg, rev.0),
            None => *buf = Arc::new(materialize_snapshot(&topo, &agg, rev.0)),
        }
    } else {
        if Arc::get_mut(buf).is_none() {
            stats.deep_clones += 1;
        }
        let snap = Arc::make_mut(buf);
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
    pub fn new(spec: ClusterSpec, mode: LayoutMode) -> Self {
        let scene = k10s_core::new_shared_scene();
        let (mut world, mut schedule) = build_world(&spec, scene, mode);
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
}

impl PublishBench {
    pub fn new(spec: ClusterSpec, mode: LayoutMode) -> Self {
        let scene = k10s_core::new_shared_scene();
        let (mut world, mut schedule) = build_world(&spec, scene, mode);

        schedule.run(&mut world);
        world.resource_mut::<Dirty>().0 = true;
        schedule.run(&mut world);
        Self { world, schedule }
    }

    pub fn flip_pods(&mut self, k: usize) {
        let indices: Vec<u32> = {
            let topo = self.world.resource::<Topology>();
            let n = topo.pod_entities.len();
            let stride = (n / k.max(1)).max(1);
            (0..k.min(n)).map(|j| ((j * stride) % n) as u32).collect()
        };
        // Rotates through one reason per severity, so churn still moves severity
        // on every step (the idle invariant depends on churn actually changing
        // something) while exercising the reason channel the model just gained.
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

    pub fn snapshot(&self) -> Arc<SceneSnapshot> {
        self.world.resource::<SceneOut>().0.load_full()
    }

    pub fn pod_count(&self) -> usize {
        self.world.resource::<Topology>().pod_entities.len()
    }

    pub fn stats(&self) -> PublishStats {
        *self.world.resource::<PublishStats>()
    }
}

fn build_world(spec: &ClusterSpec, scene: SharedScene, mode: LayoutMode) -> (World, Schedule) {
    let lay = layout::layout(spec, mode);
    let with_sats = mode.emits_attachments();

    let mut ns_labels = Vec::new();
    let mut ns_wl_range = Vec::new();
    let mut ns_pod_range = Vec::new();
    let mut wl_labels = Vec::new();
    let mut wl_kinds = Vec::new();
    let mut wl_tools = Vec::new();
    let mut wl_ns = Vec::new();
    let mut wl_pod_range = Vec::new();
    let mut wl_sat_range = Vec::new();
    let mut pod_labels = Vec::new();
    let mut pod_wl = Vec::new();
    let mut pod_state = Vec::new();
    let mut sat_labels = Vec::new();
    let mut sat_details = Vec::new();
    let mut sat_kinds = Vec::new();
    let mut edges = Vec::new();
    let mut ns_edge_range = Vec::with_capacity(spec.namespaces.len());

    for (ni, ns) in spec.namespaces.iter().enumerate() {
        let wl_start = wl_labels.len() as u32;
        let ns_pod_start = pod_labels.len() as u32;
        let edge_start = edges.len() as u32;
        for wl in &ns.workloads {
            let wl_idx = wl_labels.len() as u32;
            let pod_start = pod_labels.len() as u32;
            for pod in &wl.pods {
                pod_labels.push(Arc::<str>::from(pod.name.as_str()));
                pod_wl.push(wl_idx);
                pod_state.push(pod.state);
            }
            let sat_start = sat_labels.len() as u32;
            if with_sats {
                for sat in &wl.sats {
                    sat_labels.push(Arc::<str>::from(sat.name.as_str()));
                    sat_details.push(Arc::<str>::from(sat.detail.as_str()));
                    sat_kinds.push(sat.kind);
                }
            }
            for &dep in &wl.deps {
                edges.push(EdgeInst::blocks(wl_idx, wl_start + dep));
            }
            wl_labels.push(Arc::<str>::from(wl.name.as_str()));
            wl_kinds.push(wl.kind);
            wl_tools.push(wl.tool);
            wl_ns.push(ni as u32);
            wl_pod_range.push(pod_start..pod_labels.len() as u32);
            wl_sat_range.push(sat_start..sat_labels.len() as u32);
        }
        ns_labels.push(Arc::<str>::from(ns.name.as_str()));
        ns_wl_range.push(wl_start..wl_labels.len() as u32);
        ns_pod_range.push(ns_pod_start..pod_labels.len() as u32);
        ns_edge_range.push(edge_start..edges.len() as u32);
    }
    debug_assert_eq!(sat_labels.len(), lay.sat_rects.len());

    // Appended after every namespace range is closed, so the per-region ranges
    // stay contiguous and these form a tail the culler visits once per frame
    // rather than per region.
    let cross_start = edges.len() as u32;
    let wl_total = wl_labels.len() as u32;
    for &(a, b) in &spec.cross_deps {
        if a < wl_total && b < wl_total {
            edges.push(EdgeInst::blocks(a, b));
        }
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
    // Scopes histogram their own pods rather than folding the owner rollups,
    // because a rollup has already lost the counts needed to decrement it later.
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
        ns_labels,
        ns_rects: lay.ns_rects,
        ns_wl_range,
        ns_pod_range,
        wl_labels,
        wl_rects: lay.wl_rects,
        wl_card_rects: lay.card_rects,
        wl_kinds,
        wl_tools,
        wl_ns,
        wl_pod_range,
        wl_sat_range,
        pod_labels,
        pod_rects: lay.pod_rects,
        pod_wl,
        pod_entities,
        sat_labels,
        sat_details,
        sat_kinds,
        sat_rects: lay.sat_rects,
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

/// One draw against the same thresholds as before, so churn behaviour is
/// unchanged; the reason now travels with the severity.
fn weighted_state(rng: &mut ChaCha8Rng) -> State {
    match rng.random_range(0..100u32) {
        0..90 => State::of(ReasonId::RUNNING),
        90..94 => State::of(ReasonId::NOT_READY),
        94..98 => State::of(ReasonId::CRASH_LOOP_BACK_OFF),
        _ => State::of(ReasonId::UNKNOWN),
    }
}

pub fn spawn_world(
    spec: ClusterSpec,
    scene: SharedScene,
    ctrl: Receiver<WorldCtrl>,
    seed: u64,
    churn_rate: f32,
    mode: LayoutMode,
    on_publish: impl Fn() + Send + 'static,
) -> JoinHandle<()> {
    spawn_world_boxed(
        spec,
        scene,
        ctrl,
        seed,
        churn_rate,
        mode,
        Box::new(on_publish),
    )
}

fn spawn_world_boxed(
    spec: ClusterSpec,
    scene: SharedScene,
    ctrl: Receiver<WorldCtrl>,
    seed: u64,
    churn_rate: f32,
    mode: LayoutMode,
    on_publish: Box<dyn Fn() + Send + 'static>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("k10s-world".into())
        .spawn(move || {
            let (mut world, mut schedule) = build_world(&spec, scene, mode);
            drop(spec);
            let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0xC0FFEE);
            let tick = Duration::from_secs_f32(1.0 / TICK_HZ);
            let mut churn_on = true;
            let mut carry = 0.0f32;
            let mut published_rev = 0u64;

            loop {
                let start = Instant::now();
                for msg in ctrl.try_iter() {
                    match msg {
                        WorldCtrl::SetChurn(on) => churn_on = on,
                        WorldCtrl::Shutdown => return,
                    }
                }

                if churn_on {
                    carry += churn_rate / TICK_HZ;
                    let flips = carry as usize;
                    carry -= flips as f32;
                    let n = world.resource::<Topology>().pod_entities.len();
                    if n > 0 {
                        for _ in 0..flips {
                            let i = rng.random_range(0..n);
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
    use k10s_core::Level;

    fn platform(seed: u64, target_objects: u32) -> ClusterSpec {
        generate(&GenConfig {
            seed,
            target_objects,
            scenario: Scenario::Platform,
        })
    }

    /// Tests reason about severities; the model stores a reason alongside one.
    /// A single representative reason per severity keeps these cases readable and
    /// still exercises the reason channel end to end.
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

    fn assert_published_matches_full(world: &World, snap: &SceneSnapshot) {
        let topo = world.resource::<Topology>();
        let agg = world.resource::<Aggregates>();
        let full = materialize_snapshot(topo, agg, snap.rev);
        assert_eq!(snap.cells.len(), full.cells.len());
        assert_eq!(snap.blocks.len(), full.blocks.len());
        assert_eq!(snap.regions.len(), full.regions.len());
        for (i, (a, b)) in snap.cells.iter().zip(&full.cells).enumerate() {
            assert_eq!(a.ext.state, b.ext.state, "cell {i} at rev {}", snap.rev);
            assert_eq!(a.rect, b.rect, "cell {i} rect at rev {}", snap.rev);
        }
        for (i, (a, b)) in snap.blocks.iter().zip(&full.blocks).enumerate() {
            assert_eq!(a.ext.rollup, b.ext.rollup, "block {i} at rev {}", snap.rev);
            assert_eq!(a.children, b.children, "block {i} children");
        }
        for (i, (a, b)) in snap.regions.iter().zip(&full.regions).enumerate() {
            assert_eq!(
                a.ext.unhealthy_frac, b.ext.unhealthy_frac,
                "region {i} at rev {}",
                snap.rev
            );
            assert_eq!(
                a.ext.rollup, b.ext.rollup,
                "region {i} rollup at rev {}",
                snap.rev
            );
        }
        assert_eq!(snap.totals.cells, full.totals.cells);
        assert_eq!(snap.bounds, full.bounds);
    }

    fn assert_rollup_arithmetic(world: &World) {
        let topo = world.resource::<Topology>();
        let agg = world.resource::<Aggregates>();
        for (wl, range) in topo.wl_pod_range.iter().enumerate() {
            let cells = (range.end - range.start) as usize;
            let counts = agg.wl_sev_counts[wl];
            assert_eq!(
                counts.iter().sum::<u32>() as usize,
                cells,
                "workload {wl} severity counts {counts:?} do not sum to {cells} cells"
            );
            let mut expect = [0u32; 4];
            for i in range.start as usize..range.end as usize {
                expect[agg.pod_state[i].severity.rank() as usize] += 1;
            }
            assert_eq!(counts, expect, "workload {wl} severity counts drifted");
            let worst = (range.start as usize..range.end as usize)
                .map(|i| agg.pod_state[i].severity)
                .max()
                .unwrap_or(Severity::Ok);
            assert_eq!(agg.wl_rollup[wl], worst, "workload {wl} rollup drifted");
        }
        for (ns, range) in topo.ns_pod_range.iter().enumerate() {
            let unhealthy = (range.start as usize..range.end as usize)
                .filter(|&i| agg.pod_state[i].severity.is_unhealthy())
                .count() as u32;
            assert_eq!(
                agg.ns_unhealthy_count[ns], unhealthy,
                "namespace {ns} unhealthy count drifted"
            );
            let total = (range.end - range.start).max(1) as f32;
            assert_eq!(
                agg.ns_unhealthy[ns],
                unhealthy as f32 / total,
                "namespace {ns} unhealthy fraction drifted"
            );
        }
    }

    #[test]
    fn initial_snapshot_published_and_rollups_react() {
        let spec = generate(&GenConfig {
            seed: 1,
            target_objects: 3000,
            scenario: Scenario::Platform,
        });
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
        let spec = generate(&GenConfig {
            seed: 3,
            target_objects: 5_000,
            scenario: Scenario::Platform,
        });
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
    fn held_buffer_is_never_mutated_under_reader() {
        let spec = generate(&GenConfig {
            seed: 4,
            target_objects: 2_000,
            scenario: Scenario::Platform,
        });
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
            // The region ranges no longer cover the whole array: they run to where
            // the cross-region tail begins, and the two together partition it
            // exactly, with nothing shared and nothing unreachable.
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
                // Endpoints are tagged now, so an in-range check has to know which
                // array each end indexes rather than assuming both are blocks.
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
        // cross_edges used to be written as len..len, so the culler's cross-region
        // scan was dead code and no cross-namespace topology could exist.
        let spec = platform(55, 20_000);
        assert!(
            !spec.cross_deps.is_empty(),
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

            // Every per-region range must end where the cross tail begins, so no
            // edge is both region-owned and cross, and none is unreachable.
            for (i, r) in snap.region_edges.iter().enumerate() {
                assert!(
                    r.end <= cross.start,
                    "{mode:?}: region {i} range {r:?} overlaps the cross tail at {}",
                    cross.start
                );
            }

            // The defining property: both ends of a cross edge sit in different
            // regions, which is exactly what no single region's range can cover.
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

            // And the culler reaches them: with the whole world visible and no
            // budget to bind, it must draw at least the cross links.
            let drawn = k10s_atlas::walk_edges(&snap, &snap.bounds, usize::MAX, |_, _| {});
            assert!(
                drawn >= cross.len(),
                "{mode:?}: culler drew {drawn} edges, fewer than the {} cross links",
                cross.len()
            );
        }
    }

    #[test]
    fn publish_hook_fires_per_snapshot_not_per_tick() {
        let spec = generate(&GenConfig {
            seed: 2,
            target_objects: 500,
            scenario: Scenario::Platform,
        });
        let scene = k10s_core::new_shared_scene();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
        let world = spawn_world(
            spec,
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
