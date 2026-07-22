pub mod layout;

use std::ops::Range;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;
use crossbeam_channel::Receiver;
use k10s_clustergen::ClusterSpec;
use k10s_core::{
    EdgeInst, Health, NsExt, NsNode, PodExt, PodNode, Rect, SatExt, SatKind, SatNode,
    SceneSnapshot, SharedScene, Tool, Totals, WlExt, WorkloadKind, WorkloadNode, WorldCtrl,
};
use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

pub use layout::LayoutMode;

const TICK_HZ: f32 = 20.0;

#[derive(Component)]
struct PodH(Health);

#[derive(Resource, Default)]
struct DirtyPods(Vec<(u32, Health)>);

fn set_pod_health(world: &mut World, idx: u32, new: Health) {
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

fn update_pod_healths(world: &mut World, indices: &[u32], mut f: impl FnMut(Health) -> Health) {
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
    wl_kinds: Vec<WorkloadKind>,
    wl_tools: Vec<Tool>,
    wl_ns: Vec<u32>,
    wl_pod_range: Vec<Range<u32>>,
    wl_sat_range: Vec<Range<u32>>,
    pod_labels: Vec<Arc<str>>,
    pod_rects: Vec<Rect>,
    pod_wl: Vec<u32>,
    pod_entities: Vec<Entity>,
    sat_labels: Vec<Arc<str>>,
    sat_details: Vec<Arc<str>>,
    sat_kinds: Vec<SatKind>,
    sat_rects: Vec<Rect>,
    edges: Vec<EdgeInst>,
    ns_edge_range: Vec<Range<u32>>,
    bounds: Rect,
}

#[derive(Resource)]
struct Aggregates {
    pod_health: Vec<Health>,
    wl_health: Vec<Health>,
    ns_unhealthy: Vec<f32>,
    wl_sev_counts: Vec<[u32; 4]>,
    ns_unhealthy_count: Vec<u32>,
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

#[derive(Resource)]
struct SnapshotPool {
    bufs: [Arc<SceneSnapshot>; 2],
    pending: [Pending; 2],
    next: usize,
}

impl SnapshotPool {
    fn new() -> Self {
        SnapshotPool {
            bufs: [
                Arc::new(SceneSnapshot::default()),
                Arc::new(SceneSnapshot::default()),
            ],
            pending: [Pending::full(), Pending::full()],
            next: 0,
        }
    }
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
        let old = agg.pod_health[i];
        if old != new {
            agg.pod_health[i] = new;
            for p in &mut pool.pending {
                if !p.all {
                    p.pods.push(iu);
                }
            }
            let wl = topo.pod_wl[i];
            let sev = &mut agg.wl_sev_counts[wl as usize];
            sev[old.severity() as usize] -= 1;
            sev[new.severity() as usize] += 1;
            if old.is_unhealthy() != new.is_unhealthy() {
                let ns = topo.wl_ns[wl as usize] as usize;
                if new.is_unhealthy() {
                    agg.ns_unhealthy_count[ns] += 1;
                } else {
                    agg.ns_unhealthy_count[ns] -= 1;
                }
            }
            if !std::mem::replace(&mut scratch.wl_stamp[wl as usize], true) {
                scratch.wl_list.push(wl);
            }
        }
    }
    dirty_pods.0.clear();
    if scratch.wl_list.is_empty() {
        return;
    }

    for &wl in &scratch.wl_list {
        let counts = &agg.wl_sev_counts[wl as usize];
        agg.wl_health[wl as usize] = if counts[3] > 0 {
            Health::Err
        } else if counts[2] > 0 {
            Health::Warn
        } else if counts[1] > 0 {
            Health::Unknown
        } else {
            Health::Ok
        };
        let ns = topo.wl_ns[wl as usize];
        if !std::mem::replace(&mut scratch.ns_stamp[ns as usize], true) {
            scratch.ns_list.push(ns);
        }
    }
    for &ns in &scratch.ns_list {
        let range = &topo.ns_pod_range[ns as usize];
        let total = (range.end - range.start).max(1) as f32;
        agg.ns_unhealthy[ns as usize] = agg.ns_unhealthy_count[ns as usize] as f32 / total;
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
            .map(
                |((((&rect, label), wl_range), pod_range), &unhealthy_frac)| NsNode {
                    rect,
                    label: label.clone(),
                    weight: pod_range.end - pod_range.start,
                    children: wl_range.clone(),
                    ext: NsExt { unhealthy_frac },
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
            .zip(&agg.wl_health)
            .map(
                |(
                    (((((((&rect, &inner), label), pod_range), sat_range), &kind), &tool), &ns),
                    &health,
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
                            health,
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
            .zip(&agg.pod_health)
            .map(|((&rect, label), &health)| PodNode {
                rect,
                label: label.clone(),
                ext: PodExt { health },
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

    snap.cross_edges = topo.edges.len() as u32..topo.edges.len() as u32;
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
    out: Res<SceneOut>,
) {
    if !dirty.0 && rev.0 > 0 {
        return;
    }
    dirty.0 = false;
    rev.0 += 1;

    let SnapshotPool {
        bufs,
        pending,
        next,
    } = &mut *pool;
    let (buf, pending) = (&mut bufs[*next], &mut pending[*next]);
    if pending.all {
        match Arc::get_mut(buf) {
            Some(snap) => materialize_into(snap, &topo, &agg, rev.0),
            None => *buf = Arc::new(materialize_snapshot(&topo, &agg, rev.0)),
        }
    } else {
        let snap = Arc::make_mut(buf);
        for &i in &pending.pods {
            snap.cells[i as usize].ext.health = agg.pod_health[i as usize];
        }
        for &i in &pending.wls {
            snap.blocks[i as usize].ext.health = agg.wl_health[i as usize];
        }
        for &i in &pending.nss {
            snap.regions[i as usize].ext.unhealthy_frac = agg.ns_unhealthy[i as usize];
        }
        snap.rev = rev.0;
    }
    pending.clear();
    out.0.store(buf.clone());
    *next = 1 - *next;
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
        update_pod_healths(&mut self.world, &indices, |cur| match cur {
            Health::Ok => Health::Warn,
            Health::Warn => Health::Err,
            Health::Err => Health::Unknown,
            Health::Unknown => Health::Ok,
        });
    }

    pub fn run_publish(&mut self) {
        self.schedule.run(&mut self.world);
    }

    pub fn snapshot(&self) -> Arc<SceneSnapshot> {
        self.world.resource::<SceneOut>().0.load_full()
    }
}

fn build_world(spec: &ClusterSpec, scene: SharedScene, mode: LayoutMode) -> (World, Schedule) {
    let lay = layout::layout(spec, mode);
    let with_sats = !lay.sat_rects.is_empty();

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
    let mut pod_health = Vec::new();
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
                pod_health.push(pod.health);
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
                edges.push(EdgeInst {
                    a: wl_idx,
                    b: wl_start + dep,
                });
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

    let wl_health: Vec<Health> = wl_pod_range
        .iter()
        .map(|r| {
            (r.start as usize..r.end as usize)
                .map(|i| pod_health[i])
                .max_by_key(|h| h.severity())
                .unwrap_or(Health::Ok)
        })
        .collect();
    let wl_sev_counts: Vec<[u32; 4]> = wl_pod_range
        .iter()
        .map(|r| {
            let mut counts = [0u32; 4];
            for i in r.start as usize..r.end as usize {
                counts[pod_health[i].severity() as usize] += 1;
            }
            counts
        })
        .collect();
    let ns_unhealthy_count: Vec<u32> = ns_pod_range
        .iter()
        .map(|r| {
            (r.start as usize..r.end as usize)
                .filter(|&i| pod_health[i].is_unhealthy())
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
        .spawn_batch(pod_health.iter().map(|&h| (PodH(h),)).collect::<Vec<_>>())
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
        bounds: lay.bounds,
    });
    world.insert_resource(Aggregates {
        pod_health,
        wl_health,
        ns_unhealthy,
        wl_sev_counts,
        ns_unhealthy_count,
    });
    world.insert_resource(SceneOut(scene));
    world.insert_resource(Dirty(false));
    world.insert_resource(Rev(0));
    world.insert_resource(SnapshotPool::new());
    world.insert_resource(RollupScratch::default());
    world.insert_resource(DirtyPods::default());

    let mut schedule = Schedule::default();
    schedule.add_systems((rollup, extract).chain());
    (world, schedule)
}

fn weighted_health(rng: &mut ChaCha8Rng) -> Health {
    match rng.random_range(0..100u32) {
        0..90 => Health::Ok,
        90..94 => Health::Warn,
        94..98 => Health::Err,
        _ => Health::Unknown,
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
                            let new = weighted_health(&mut rng);
                            set_pod_health(&mut world, i as u32, new);
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
    use super::*;
    use k10s_clustergen::{GenConfig, Scenario, generate};

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

        set_pod_health(&mut world, 0, Health::Err);
        schedule.run(&mut world);
        let snap = scene.load();
        assert_eq!(snap.rev, 2);
        assert_eq!(snap.cells[0].ext.health, Health::Err);
        let pod_rect = snap.cells[0].rect;
        let owner = &snap.blocks[world.resource::<Topology>().pod_wl[0] as usize];
        assert_eq!(owner.ext.health, Health::Err);
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

        let flip = |world: &mut World, pod: usize, h: Health| {
            set_pod_health(world, pod as u32, h);
        };

        flip(&mut world, 0, Health::Err);
        schedule.run(&mut world);
        flip(&mut world, 1, Health::Warn);
        schedule.run(&mut world);
        flip(&mut world, 2, Health::Unknown);
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
            assert_eq!(a.ext.health, b.ext.health);
        }
        for (a, b) in snap.blocks.iter().zip(full.blocks.iter()) {
            assert_eq!(a.ext.health, b.ext.health);
        }
        for (a, b) in snap.regions.iter().zip(full.regions.iter()) {
            assert_eq!(a.ext.unhealthy_frac, b.ext.unhealthy_frac);
        }
        assert_eq!(snap.region_edges.len(), snap.regions.len());
        assert_eq!(snap.cells[0].ext.health, Health::Err);
        assert_eq!(snap.cells[1].ext.health, Health::Warn);
        assert_eq!(snap.cells[2].ext.health, Health::Unknown);
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
        let held_health: Vec<Health> = held.cells.iter().map(|c| c.ext.health).collect();

        let flip = |world: &mut World, pod: usize, h: Health| {
            set_pod_health(world, pod as u32, h);
        };
        let target = held_health
            .iter()
            .position(|&h| h != Health::Err)
            .expect("some pod not already Err");
        flip(&mut world, target, Health::Err);
        schedule.run(&mut world);
        flip(&mut world, target, Health::Warn);
        schedule.run(&mut world);

        assert_eq!(held.rev, 1, "reader's snapshot changed under it");
        for (cell, &h) in held.cells.iter().zip(&held_health) {
            assert_eq!(cell.ext.health, h, "reader's snapshot changed under it");
        }
        let fresh = scene.load_full();
        assert_eq!(fresh.rev, 3);
        assert_eq!(fresh.cells[target].ext.health, Health::Warn);
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
