use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;
use std::sync::Arc;

use bevy_ecs::prelude::{Mut, World};
use k10s_core::layout::{
    CARD_HEADER, CARD_PAD, NS_GAP, NS_HEADER, NS_PAD, POD_GAP, POD_PITCH, POD_SIZE, SAT_RING_GAP,
    SAT_RING0_GAP, SAT_SIZE, WL_GAP, WL_HEADER, WL_PAD,
};
use k10s_core::{
    EdgeInst, IngestEvent, KindId, Op, Payload, Rect, ResourceEvent, Severity, State, ToolId,
};

use crate::{
    Aggregates, Dirty, DirtyPods, Pending, PodH, SnapshotPool, Topology, layout::LayoutMode,
    rollup_of,
};

const NO_SLOT: u32 = u32::MAX;
const DEAD_RECT: Rect = Rect {
    x: f32::MAX,
    y: f32::MAX,
    w: 0.0,
    h: 0.0,
};

#[derive(Default)]
pub(super) struct SlotMap {
    by_uid: HashMap<Arc<str>, u32>,
    uid_by_slot: Vec<Option<Arc<str>>>,
    free: BinaryHeap<Reverse<u32>>,
}

impl SlotMap {
    pub(super) fn insert(&mut self, uid: Arc<str>) -> (u32, bool) {
        if let Some(&slot) = self.by_uid.get(&uid) {
            return (slot, false);
        }
        let slot = match self.free.pop() {
            Some(Reverse(slot)) => {
                self.uid_by_slot[slot as usize] = Some(uid.clone());
                slot
            }
            None => {
                let slot = self.uid_by_slot.len() as u32;
                self.uid_by_slot.push(Some(uid.clone()));
                slot
            }
        };
        self.by_uid.insert(uid, slot);
        (slot, true)
    }

    pub(super) fn remove(&mut self, uid: &str) -> Option<u32> {
        let slot = self.by_uid.remove(uid)?;
        self.uid_by_slot[slot as usize] = None;
        self.free.push(Reverse(slot));
        Some(slot)
    }

    pub(super) fn get(&self, uid: &str) -> Option<u32> {
        self.by_uid.get(uid).copied()
    }

    pub(super) fn uid(&self, slot: u32) -> Option<&Arc<str>> {
        self.uid_by_slot.get(slot as usize)?.as_ref()
    }

    pub(super) fn is_active(&self, slot: usize) -> bool {
        self.uid_by_slot.get(slot).is_some_and(Option::is_some)
    }

    pub(super) fn slots(&self) -> usize {
        self.uid_by_slot.len()
    }

    pub(super) fn active(&self) -> usize {
        self.by_uid.len()
    }
}

pub(super) struct Adjacency {
    pub(super) ranges: Vec<Range<u32>>,
    pub(super) indices: Vec<u32>,
}

impl Adjacency {
    pub(super) fn build(parents: &[u32], children: &SlotMap, parent_slots: usize) -> Self {
        let mut counts = vec![0u32; parent_slots];
        for (child, &parent) in parents.iter().enumerate() {
            if children.is_active(child) && (parent as usize) < parent_slots {
                counts[parent as usize] += 1;
            }
        }

        let mut ranges = Vec::with_capacity(parent_slots);
        let mut end = 0u32;
        for count in counts {
            let start = end;
            end += count;
            ranges.push(start..end);
        }

        let mut indices = vec![0u32; end as usize];
        let mut cursors: Vec<u32> = ranges.iter().map(|range| range.start).collect();
        for (child, &parent) in parents.iter().enumerate() {
            if !children.is_active(child) || (parent as usize) >= parent_slots {
                continue;
            }
            let cursor = &mut cursors[parent as usize];
            indices[*cursor as usize] = child as u32;
            *cursor += 1;
        }
        Adjacency { ranges, indices }
    }

    pub(super) fn is_direct(&self) -> bool {
        self.indices
            .iter()
            .enumerate()
            .all(|(index, &child)| index == child as usize)
    }
}

pub(super) fn apply_events(world: &mut World, events: &[IngestEvent], mode: LayoutMode) -> bool {
    let structural = {
        let topology = world.resource::<Topology>();
        events.iter().any(|event| match event {
            IngestEvent::Resource(resource) => !topology.pod_state_only(resource),
            _ => false,
        })
    };
    if !structural {
        return world.resource_scope(|world, topology: Mut<Topology>| {
            world.resource_scope(|world, mut dirty: Mut<DirtyPods>| {
                let mut changed = false;
                for event in events {
                    let IngestEvent::Resource(resource) = event else {
                        continue;
                    };
                    let Payload::Instance { state } = resource.payload else {
                        continue;
                    };
                    let Some(slot) = topology.pod_slots.get(&resource.uid) else {
                        continue;
                    };
                    let entity = topology.pod_entities[slot as usize];
                    let Some(mut health) = world.get_mut::<PodH>(entity) else {
                        continue;
                    };
                    if health.0 == state {
                        continue;
                    }
                    health.0 = state;
                    dirty.0.push((slot, state));
                    changed = true;
                }
                changed
            })
        });
    }

    let mut topology = world
        .remove_resource::<Topology>()
        .expect("the world owns its topology while applying live events");
    let mut aggregates = world
        .remove_resource::<Aggregates>()
        .expect("the world owns its aggregates while applying live events");
    let mut dirt = BatchDirt::default();
    // Pod-state deltas queued for rollup() but not yet folded would be lost to
    // the incremental path, whose "old state" is what aggregates already hold.
    if !world.resource::<DirtyPods>().0.is_empty() {
        dirt.aggregates_full = true;
    }
    for event in events {
        let IngestEvent::Resource(resource) = event else {
            continue;
        };
        apply_resource(
            world,
            &mut topology,
            &mut aggregates,
            &mut dirt,
            resource,
            mode,
        );
    }
    rebuild_selective(&mut topology, &dirt);
    fit_after_departures(&mut topology, &dirt, mode);
    topology.spatial_revision += 1;
    if dirt.identity {
        topology.identity_revision += 1;
    }
    let aggregates = if dirt.aggregates_full {
        rebuild_aggregates(world, &topology)
    } else {
        aggregates
    };
    // A patch must win by construction: when the batch invalidated the
    // aggregates or counts wholesale, or touched a large share of the scene,
    // the full materialize is both simpler and cheaper.
    let total_slots = topology.ns_slots.slots()
        + topology.wl_slots.slots()
        + topology.pod_slots.slots()
        + topology.sat_slots.slots();
    let full = dirt.aggregates_full || dirt.counts_full || dirt.touched() * 2 > total_slots;
    world.insert_resource(topology);
    world.insert_resource(aggregates);
    world.resource_mut::<DirtyPods>().0.clear();
    {
        let mut pool = world.resource_mut::<SnapshotPool>();
        for pending in &mut pool.pending {
            if full {
                *pending = Pending::full();
            } else if !pending.all {
                let structural = &mut pending.structural;
                structural.active = true;
                structural.nss.extend_from_slice(&dirt.nss);
                structural.wls.extend_from_slice(&dirt.wls);
                structural.pods.extend_from_slice(&dirt.pods);
                structural.sats.extend_from_slice(&dirt.sats);
                structural.ranges_ns_wl |= dirt.ns_wl;
                structural.ranges_wl_pod |= dirt.wl_pod;
                structural.ranges_wl_sat |= dirt.wl_sat;
                structural.edges |= dirt.edges;
            }
        }
    }
    world.resource_mut::<Dirty>().0 = true;
    true
}

// Which derived structures a live batch invalidated, and which slots it
// touched. Everything defaults to clean, and each upsert or removal marks
// exactly what it changed, so a batch pays to rebuild the relations it
// invalidated and the snapshots patch only the nodes that moved.
#[derive(Default)]
struct BatchDirt {
    identity: bool,
    // Something left a parent, so a parent may now be bigger than its contents.
    // Separate from `wl_pod` because arrivals never need the fitting pass and
    // adding one object is the path this engine is measured on.
    shrank: bool,
    ns_wl: bool,
    wl_pod: bool,
    wl_sat: bool,
    edges: bool,
    counts_full: bool,
    aggregates_full: bool,
    nss: Vec<u32>,
    wls: Vec<u32>,
    pods: Vec<u32>,
    sats: Vec<u32>,
}

impl BatchDirt {
    fn touched(&self) -> usize {
        self.nss.len() + self.wls.len() + self.pods.len() + self.sats.len()
    }
}

fn rebuild_selective(topology: &mut Topology, dirt: &BatchDirt) {
    if dirt.ns_wl {
        let region_blocks = Adjacency::build(
            &topology.wl_ns,
            &topology.wl_slots,
            topology.ns_slots.slots(),
        );
        let direct = region_blocks.is_direct();
        topology.ns_wl_range = region_blocks.ranges;
        topology.region_blocks = if direct {
            Vec::new()
        } else {
            region_blocks.indices
        };
    }
    if dirt.wl_pod {
        let block_cells = Adjacency::build(
            &topology.pod_wl,
            &topology.pod_slots,
            topology.wl_slots.slots(),
        );
        let direct = block_cells.is_direct();
        topology.wl_pod_range = block_cells.ranges;
        topology.block_cells = if direct {
            Vec::new()
        } else {
            block_cells.indices
        };
    }
    if dirt.wl_sat {
        let block_sats = Adjacency::build(
            &topology.sat_wl,
            &topology.sat_slots,
            topology.wl_slots.slots(),
        );
        let direct = block_sats.is_direct();
        topology.wl_sat_range = block_sats.ranges;
        topology.block_sats = if direct {
            Vec::new()
        } else {
            block_sats.indices
        };
    }
    if dirt.counts_full {
        topology.ns_pod_count.fill(0);
        for pod in 0..topology.pod_slots.slots() {
            if !topology.pod_slots.is_active(pod) {
                continue;
            }
            let workload = topology.pod_wl[pod] as usize;
            if !topology.wl_slots.is_active(workload) {
                continue;
            }
            let namespace = topology.wl_ns[workload] as usize;
            if topology.ns_slots.is_active(namespace) {
                topology.ns_pod_count[namespace] += 1;
            }
        }
    }
    if dirt.edges {
        rebuild_edges(topology);
    }
}

impl Topology {
    fn pod_state_only(&self, resource: &ResourceEvent) -> bool {
        if resource.op != Op::Modified {
            return false;
        }
        let Payload::Instance { .. } = resource.payload else {
            return false;
        };
        let Some(pod) = self.pod_slots.get(&resource.uid) else {
            return false;
        };
        let Some(parent) = resource
            .parent
            .as_deref()
            .and_then(|uid| self.wl_slots.get(uid))
        else {
            return false;
        };
        self.pod_wl[pod as usize] == parent
            && self.pod_labels[pod as usize].as_ref() == resource.name.as_ref()
    }
}

fn apply_resource(
    world: &mut World,
    topology: &mut Topology,
    aggregates: &mut Aggregates,
    dirt: &mut BatchDirt,
    resource: &ResourceEvent,
    mode: LayoutMode,
) {
    if resource.op == Op::Deleted {
        remove_resource(topology, aggregates, dirt, resource);
        return;
    }
    match &resource.payload {
        Payload::Scope => upsert_namespace(topology, aggregates, dirt, resource),
        Payload::Owner {
            kind,
            tool,
            depends_on,
        } => upsert_workload(
            topology, aggregates, dirt, resource, *kind, *tool, depends_on, mode,
        ),
        Payload::Instance { state } => {
            upsert_pod(world, topology, aggregates, dirt, resource, *state, mode)
        }
        Payload::Attached { kind, detail } if mode.emits_attachments() => {
            upsert_satellite(topology, dirt, resource, *kind, detail)
        }
        Payload::Attached { .. } => {}
    }
}

// One pod's severity moves between (workload, namespace) buckets. `from` and
// `to` of None mean the pod is entering or leaving the world; the rollups of
// every touched bucket are refreshed immediately, which per event is O(1).
fn shift_pod_severity(
    topology: &Topology,
    aggregates: &mut Aggregates,
    from: Option<(u32, State)>,
    to: Option<(u32, State)>,
) {
    let mut touched = [None::<u32>; 2];
    if let Some((workload, state)) = from {
        let namespace = topology.wl_ns[workload as usize] as usize;
        let rank = state.severity.rank() as usize;
        aggregates.wl_sev_counts[workload as usize][rank] -= 1;
        aggregates.ns_sev_counts[namespace][rank] -= 1;
        if state.severity.is_unhealthy() {
            aggregates.ns_unhealthy_count[namespace] -= 1;
        }
        touched[0] = Some(workload);
    }
    if let Some((workload, state)) = to {
        let namespace = topology.wl_ns[workload as usize] as usize;
        let rank = state.severity.rank() as usize;
        aggregates.wl_sev_counts[workload as usize][rank] += 1;
        aggregates.ns_sev_counts[namespace][rank] += 1;
        if state.severity.is_unhealthy() {
            aggregates.ns_unhealthy_count[namespace] += 1;
        }
        touched[1] = Some(workload);
    }
    for workload in touched.into_iter().flatten() {
        let namespace = topology.wl_ns[workload as usize] as usize;
        aggregates.wl_rollup[workload as usize] =
            rollup_of(&aggregates.wl_sev_counts[workload as usize]);
        aggregates.ns_rollup[namespace] = rollup_of(&aggregates.ns_sev_counts[namespace]);
        let total = topology.ns_pod_count[namespace].max(1) as f32;
        aggregates.ns_unhealthy[namespace] =
            aggregates.ns_unhealthy_count[namespace] as f32 / total;
    }
}

fn upsert_namespace(
    topology: &mut Topology,
    aggregates: &mut Aggregates,
    dirt: &mut BatchDirt,
    resource: &ResourceEvent,
) {
    let (slot, inserted) = topology.ns_slots.insert(resource.uid.clone());
    let index = slot as usize;
    ensure_namespace(topology, index + 1);
    topology.ns_labels[index] = resource.name.clone();
    dirt.nss.push(slot);
    if !inserted {
        return;
    }
    dirt.identity = true;
    dirt.ns_wl = true;
    ensure_namespace_aggregates(aggregates, index + 1);
    aggregates.ns_sev_counts[index] = [0; 4];
    aggregates.ns_unhealthy_count[index] = 0;
    aggregates.ns_unhealthy[index] = 0.0;
    aggregates.ns_rollup[index] = Severity::Ok;

    let width = NS_PAD * 2.0 + POD_SIZE + CARD_PAD * 2.0;
    let height = NS_PAD * 2.0 + NS_HEADER + POD_SIZE + CARD_HEADER + CARD_PAD * 2.0;
    let x = if topology.ns_slots.active() == 1 {
        0.0
    } else {
        topology.bounds.max_x() + NS_GAP
    };
    topology.ns_rects[index] = Rect::new(x, 0.0, width, height);
    grow_world(topology, topology.ns_rects[index]);
}

#[expect(clippy::too_many_arguments)]
fn upsert_workload(
    topology: &mut Topology,
    aggregates: &mut Aggregates,
    dirt: &mut BatchDirt,
    resource: &ResourceEvent,
    kind: KindId,
    tool: ToolId,
    depends_on: &[Arc<str>],
    mode: LayoutMode,
) {
    let Some(namespace) = resource
        .parent
        .as_deref()
        .and_then(|uid| topology.ns_slots.get(uid))
    else {
        return;
    };
    let slots_before = topology.wl_slots.slots();
    let (slot, inserted) = topology.wl_slots.insert(resource.uid.clone());
    let index = slot as usize;
    ensure_workload(topology, index + 1);
    let moved = !inserted && topology.wl_ns[index] != namespace;
    dirt.wls.push(slot);
    if inserted || moved {
        dirt.nss.push(namespace);
    }
    if inserted {
        dirt.identity = true;
        dirt.ns_wl = true;
        dirt.edges = true;
        if topology.wl_slots.slots() > slots_before {
            // The pod and satellite adjacencies key their ranges by workload
            // slot, so a grown slot table must regrow them even though no
            // child moved.
            dirt.wl_pod = true;
            dirt.wl_sat = true;
        }
        ensure_workload_aggregates(aggregates, index + 1);
        aggregates.wl_sev_counts[index] = [0; 4];
        aggregates.wl_rollup[index] = Severity::Ok;
    }
    if moved {
        // A workload changing namespace would need its pods' severity buckets
        // transferred wholesale; it cannot happen on a real cluster, so it
        // pays the full rebuild instead of carrying transfer code.
        dirt.ns_wl = true;
        dirt.edges = true;
        dirt.counts_full = true;
        dirt.aggregates_full = true;
    }
    if !inserted && !moved && topology.wl_depends_on[index].as_slice() != depends_on {
        dirt.edges = true;
    }
    topology.wl_labels[index] = resource.name.clone();
    topology.wl_kinds[index] = kind;
    topology.wl_tools[index] = tool;
    topology.wl_ns[index] = namespace;
    topology.wl_depends_on[index].clear();
    topology.wl_depends_on[index].extend_from_slice(depends_on);
    if inserted || moved {
        place_workload(topology, slot, namespace, mode);
    }
}

fn upsert_pod(
    world: &mut World,
    topology: &mut Topology,
    aggregates: &mut Aggregates,
    dirt: &mut BatchDirt,
    resource: &ResourceEvent,
    state: State,
    mode: LayoutMode,
) {
    let Some(workload) = resource
        .parent
        .as_deref()
        .and_then(|uid| topology.wl_slots.get(uid))
    else {
        return;
    };
    let (slot, inserted) = topology.pod_slots.insert(resource.uid.clone());
    let index = slot as usize;
    ensure_pod(world, topology, index + 1);
    ensure_pod_aggregates(aggregates, index + 1);
    let moved = !inserted && topology.pod_wl[index] != workload;
    let previous = (!inserted).then(|| (topology.pod_wl[index], aggregates.pod_state[index]));
    topology.pod_labels[index] = resource.name.clone();
    topology.pod_wl[index] = workload;
    dirt.pods.push(slot);
    dirt.wls.push(workload);
    dirt.nss.push(topology.wl_ns[workload as usize]);
    if let Some((old_workload, _)) = previous
        && moved
        && topology.wl_slots.is_active(old_workload as usize)
    {
        dirt.wls.push(old_workload);
        dirt.nss.push(topology.wl_ns[old_workload as usize]);
        // A move is a departure from where it was, so the card it left may now
        // be bigger than what remains in it.
        dirt.shrank = true;
    }
    if inserted || moved {
        dirt.wl_pod = true;
        topology.pod_rects[index] = place_pod(topology, slot, workload, mode);
    }
    if inserted {
        dirt.identity = true;
    }
    match previous {
        None => {
            topology.ns_pod_count[topology.wl_ns[workload as usize] as usize] += 1;
            shift_pod_severity(topology, aggregates, None, Some((workload, state)));
        }
        Some((old_workload, old_state)) => {
            if moved {
                let old_ns = topology.wl_ns[old_workload as usize] as usize;
                topology.ns_pod_count[old_ns] -= 1;
                topology.ns_pod_count[topology.wl_ns[workload as usize] as usize] += 1;
            }
            if moved || old_state != state {
                shift_pod_severity(
                    topology,
                    aggregates,
                    Some((old_workload, old_state)),
                    Some((workload, state)),
                );
            }
        }
    }
    aggregates.pod_state[index] = state;
    let entity = topology.pod_entities[index];
    if let Some(mut health) = world.get_mut::<PodH>(entity) {
        health.0 = state;
    }
}

fn upsert_satellite(
    topology: &mut Topology,
    dirt: &mut BatchDirt,
    resource: &ResourceEvent,
    kind: KindId,
    detail: &Arc<str>,
) {
    let Some(workload) = resource
        .parent
        .as_deref()
        .and_then(|uid| topology.wl_slots.get(uid))
    else {
        return;
    };
    let (slot, inserted) = topology.sat_slots.insert(resource.uid.clone());
    let index = slot as usize;
    ensure_satellite(topology, index + 1);
    let moved = !inserted && topology.sat_wl[index] != workload;
    topology.sat_labels[index] = resource.name.clone();
    topology.sat_kinds[index] = kind;
    topology.sat_details[index] = detail.clone();
    topology.sat_wl[index] = workload;
    dirt.sats.push(slot);
    if inserted {
        dirt.identity = true;
    }
    if inserted || moved {
        dirt.wl_sat = true;
        dirt.wls.push(workload);
        dirt.nss.push(topology.wl_ns[workload as usize]);
        topology.sat_rects[index] = place_satellite(topology, slot, workload);
    }
}

fn remove_resource(
    topology: &mut Topology,
    aggregates: &mut Aggregates,
    dirt: &mut BatchDirt,
    resource: &ResourceEvent,
) {
    match resource.payload {
        Payload::Scope => {
            let Some(namespace) = topology.ns_slots.get(&resource.uid) else {
                return;
            };
            let workloads: Vec<Arc<str>> = (0..topology.wl_slots.slots())
                .filter(|&index| {
                    topology.wl_slots.is_active(index) && topology.wl_ns[index] == namespace
                })
                .filter_map(|index| topology.wl_slots.uid(index as u32).cloned())
                .collect();
            for uid in workloads {
                remove_workload(topology, aggregates, dirt, &uid);
            }
            if let Some(slot) = topology.ns_slots.remove(&resource.uid) {
                dirt.identity = true;
                dirt.ns_wl = true;
                dirt.nss.push(slot);
                let index = slot as usize;
                topology.ns_labels[index] = Arc::from("");
                topology.ns_rects[index] = DEAD_RECT;
                topology.ns_pod_count[index] = 0;
                aggregates.ns_sev_counts[index] = [0; 4];
                aggregates.ns_unhealthy_count[index] = 0;
                aggregates.ns_unhealthy[index] = 0.0;
                aggregates.ns_rollup[index] = Severity::Ok;
            }
        }
        Payload::Owner { .. } => remove_workload(topology, aggregates, dirt, &resource.uid),
        Payload::Instance { .. } => remove_pod(topology, aggregates, dirt, &resource.uid),
        Payload::Attached { .. } => remove_satellite(topology, dirt, &resource.uid),
    }
}

fn remove_workload(
    topology: &mut Topology,
    aggregates: &mut Aggregates,
    dirt: &mut BatchDirt,
    uid: &str,
) {
    let Some(workload) = topology.wl_slots.get(uid) else {
        return;
    };
    let pods: Vec<Arc<str>> = (0..topology.pod_slots.slots())
        .filter(|&index| topology.pod_slots.is_active(index) && topology.pod_wl[index] == workload)
        .filter_map(|index| topology.pod_slots.uid(index as u32).cloned())
        .collect();
    for uid in pods {
        remove_pod(topology, aggregates, dirt, &uid);
    }
    let satellites: Vec<Arc<str>> = (0..topology.sat_slots.slots())
        .filter(|&index| topology.sat_slots.is_active(index) && topology.sat_wl[index] == workload)
        .filter_map(|index| topology.sat_slots.uid(index as u32).cloned())
        .collect();
    for uid in satellites {
        remove_satellite(topology, dirt, &uid);
    }
    if let Some(slot) = topology.wl_slots.remove(uid) {
        dirt.identity = true;
        dirt.ns_wl = true;
        dirt.edges = true;
        dirt.wls.push(slot);
        let index = slot as usize;
        if topology.ns_slots.is_active(topology.wl_ns[index] as usize) {
            dirt.nss.push(topology.wl_ns[index]);
        }
        debug_assert_eq!(
            aggregates.wl_sev_counts[index], [0; 4],
            "a workload's pods must all be removed before the workload"
        );
        topology.wl_labels[index] = Arc::from("");
        topology.wl_rects[index] = DEAD_RECT;
        topology.wl_card_rects[index] = DEAD_RECT;
        topology.wl_ns[index] = NO_SLOT;
        topology.wl_depends_on[index].clear();
        aggregates.wl_sev_counts[index] = [0; 4];
        aggregates.wl_rollup[index] = Severity::Ok;
    }
}

fn remove_pod(
    topology: &mut Topology,
    aggregates: &mut Aggregates,
    dirt: &mut BatchDirt,
    uid: &str,
) {
    if let Some(slot) = topology.pod_slots.remove(uid) {
        dirt.identity = true;
        dirt.wl_pod = true;
        dirt.shrank = true;
        dirt.pods.push(slot);
        let index = slot as usize;
        let workload = topology.pod_wl[index];
        let state = aggregates.pod_state[index];
        if topology.wl_slots.is_active(workload as usize) {
            let namespace = topology.wl_ns[workload as usize] as usize;
            topology.ns_pod_count[namespace] -= 1;
            shift_pod_severity(topology, aggregates, Some((workload, state)), None);
            dirt.wls.push(workload);
            dirt.nss.push(namespace as u32);
        }
        aggregates.pod_state[index] = State::OK;
        topology.pod_labels[index] = Arc::from("");
        topology.pod_rects[index] = DEAD_RECT;
        topology.pod_wl[index] = NO_SLOT;
    }
}

fn remove_satellite(topology: &mut Topology, dirt: &mut BatchDirt, uid: &str) {
    if let Some(slot) = topology.sat_slots.remove(uid) {
        dirt.identity = true;
        dirt.wl_sat = true;
        dirt.shrank = true;
        dirt.sats.push(slot);
        let index = slot as usize;
        topology.sat_labels[index] = Arc::from("");
        topology.sat_details[index] = Arc::from("");
        topology.sat_rects[index] = DEAD_RECT;
        topology.sat_wl[index] = NO_SLOT;
    }
}

fn ensure_namespace_aggregates(aggregates: &mut Aggregates, len: usize) {
    if aggregates.ns_sev_counts.len() < len {
        aggregates.ns_sev_counts.resize(len, [0; 4]);
        aggregates.ns_unhealthy_count.resize(len, 0);
        aggregates.ns_unhealthy.resize(len, 0.0);
        aggregates.ns_rollup.resize(len, Severity::Ok);
    }
}

fn ensure_workload_aggregates(aggregates: &mut Aggregates, len: usize) {
    if aggregates.wl_sev_counts.len() < len {
        aggregates.wl_sev_counts.resize(len, [0; 4]);
        aggregates.wl_rollup.resize(len, Severity::Ok);
    }
}

fn ensure_pod_aggregates(aggregates: &mut Aggregates, len: usize) {
    if aggregates.pod_state.len() < len {
        aggregates.pod_state.resize(len, State::OK);
    }
}

fn rebuild_edges(topology: &mut Topology) {
    let mut local = vec![Vec::new(); topology.ns_slots.slots()];
    let mut cross = Vec::new();
    for source in 0..topology.wl_slots.slots() {
        if !topology.wl_slots.is_active(source) {
            continue;
        }
        let source_namespace = topology.wl_ns[source] as usize;
        for target_uid in &topology.wl_depends_on[source] {
            let Some(target) = topology.wl_slots.get(target_uid) else {
                continue;
            };
            let edge = EdgeInst::blocks(source as u32, target);
            if topology.wl_ns[target as usize] as usize == source_namespace {
                local[source_namespace].push(edge);
            } else {
                cross.push(edge);
            }
        }
    }

    topology.edges.clear();
    topology.ns_edge_range.clear();
    for edges in local {
        let start = topology.edges.len() as u32;
        topology.edges.extend(edges);
        topology
            .ns_edge_range
            .push(start..topology.edges.len() as u32);
    }
    let start = topology.edges.len() as u32;
    topology.edges.extend(cross);
    topology.cross_edge_range = start..topology.edges.len() as u32;
}

fn rebuild_aggregates(world: &World, topology: &Topology) -> Aggregates {
    let mut pod_state = vec![State::OK; topology.pod_slots.slots()];
    let mut wl_sev_counts = vec![[0u32; 4]; topology.wl_slots.slots()];
    let mut ns_sev_counts = vec![[0u32; 4]; topology.ns_slots.slots()];
    let mut ns_unhealthy_count = vec![0u32; topology.ns_slots.slots()];
    for pod in 0..topology.pod_slots.slots() {
        if !topology.pod_slots.is_active(pod) {
            continue;
        }
        let state = world
            .get::<PodH>(topology.pod_entities[pod])
            .map(|health| health.0)
            .unwrap_or(State::OK);
        pod_state[pod] = state;
        let workload = topology.pod_wl[pod] as usize;
        let namespace = topology.wl_ns[workload] as usize;
        wl_sev_counts[workload][state.severity.rank() as usize] += 1;
        ns_sev_counts[namespace][state.severity.rank() as usize] += 1;
        if state.severity.is_unhealthy() {
            ns_unhealthy_count[namespace] += 1;
        }
    }
    let wl_rollup = wl_sev_counts.iter().map(rollup_of).collect();
    let ns_rollup = ns_sev_counts.iter().map(rollup_of).collect();
    let ns_unhealthy = topology
        .ns_pod_count
        .iter()
        .zip(&ns_unhealthy_count)
        .map(|(&total, &unhealthy)| unhealthy as f32 / total.max(1) as f32)
        .collect();
    Aggregates {
        pod_state,
        wl_rollup,
        ns_rollup,
        ns_unhealthy,
        wl_sev_counts,
        ns_sev_counts,
        ns_unhealthy_count,
    }
}

// Every ensure_* grows and never shrinks: a reused tombstone slot arrives
// with a length below the high-water mark, and a plain resize would truncate
// every live entry above it.
fn ensure_namespace(topology: &mut Topology, len: usize) {
    if topology.ns_labels.len() < len {
        topology.ns_labels.resize_with(len, || Arc::from(""));
        topology.ns_rects.resize(len, DEAD_RECT);
        topology.ns_wl_range.resize(len, 0..0);
        topology.ns_pod_count.resize(len, 0);
    }
}

fn ensure_workload(topology: &mut Topology, len: usize) {
    if topology.wl_labels.len() < len {
        topology.wl_labels.resize_with(len, || Arc::from(""));
        topology.wl_rects.resize(len, DEAD_RECT);
        topology.wl_card_rects.resize(len, DEAD_RECT);
        topology.wl_kinds.resize(len, KindId::DEPLOYMENT);
        topology.wl_tools.resize(len, ToolId::NONE);
        topology.wl_ns.resize(len, NO_SLOT);
        topology.wl_depends_on.resize_with(len, Vec::new);
        topology.wl_pod_range.resize(len, 0..0);
        topology.wl_sat_range.resize(len, 0..0);
    }
}

fn ensure_pod(world: &mut World, topology: &mut Topology, len: usize) {
    if topology.pod_labels.len() < len {
        topology.pod_labels.resize_with(len, || Arc::from(""));
        topology.pod_rects.resize(len, DEAD_RECT);
        topology.pod_wl.resize(len, NO_SLOT);
    }
    while topology.pod_entities.len() < len {
        topology
            .pod_entities
            .push(world.spawn((PodH(State::OK),)).id());
    }
}

fn ensure_satellite(topology: &mut Topology, len: usize) {
    if topology.sat_labels.len() < len {
        topology.sat_labels.resize_with(len, || Arc::from(""));
        topology.sat_details.resize_with(len, || Arc::from(""));
        topology.sat_kinds.resize(len, KindId::SERVICE);
        topology.sat_rects.resize(len, DEAD_RECT);
        topology.sat_wl.resize(len, NO_SLOT);
    }
}

fn place_workload(topology: &mut Topology, slot: u32, namespace: u32, mode: LayoutMode) {
    let region = topology.ns_rects[namespace as usize];
    let x = (0..topology.wl_slots.slots())
        .filter(|&index| {
            topology.wl_slots.is_active(index)
                && index != slot as usize
                && topology.wl_ns[index] == namespace
        })
        .map(|index| topology.wl_rects[index].max_x() + WL_GAP)
        .fold(region.x + NS_PAD, f32::max);
    let y = region.y + NS_HEADER + NS_PAD;
    let (pad, header) = match mode {
        LayoutMode::Spread => (CARD_PAD, CARD_HEADER),
        LayoutMode::Dense => (WL_PAD, WL_HEADER),
    };
    let card = Rect::new(x, y, POD_SIZE + pad * 2.0, POD_SIZE + pad * 2.0 + header);
    topology.wl_card_rects[slot as usize] = card;
    topology.wl_rects[slot as usize] = card;
    grow_namespace(topology, namespace, card);
}

fn place_pod(topology: &mut Topology, slot: u32, workload: u32, mode: LayoutMode) -> Rect {
    let card = topology.wl_card_rects[workload as usize];
    let (pad, header) = match mode {
        LayoutMode::Spread => (CARD_PAD, CARD_HEADER),
        LayoutMode::Dense => (WL_PAD, WL_HEADER),
    };
    let columns = (((card.w - pad * 2.0 + POD_GAP) / POD_PITCH).floor() as usize).max(1);
    // One pass over the pod slots collects this workload's occupied corners;
    // probing then costs the candidate count, not candidates x every pod in
    // the world. Positions compare exactly because every pod rect comes from
    // the same formula below.
    let occupied: std::collections::HashSet<(u32, u32)> = (0..topology.pod_slots.slots())
        .filter(|&index| {
            topology.pod_slots.is_active(index)
                && index != slot as usize
                && topology.pod_wl[index] == workload
        })
        .map(|index| {
            let rect = topology.pod_rects[index];
            (rect.x.to_bits(), rect.y.to_bits())
        })
        .collect();
    let mut position = 0usize;
    loop {
        let column = position % columns;
        let row = position / columns;
        let candidate = Rect::new(
            card.x + pad + column as f32 * POD_PITCH,
            card.y + header + pad + row as f32 * POD_PITCH,
            POD_SIZE,
            POD_SIZE,
        );
        if !occupied.contains(&(candidate.x.to_bits(), candidate.y.to_bits())) {
            grow_workload(topology, workload, candidate, pad);
            return candidate;
        }
        position += 1;
    }
}

fn place_satellite(topology: &mut Topology, slot: u32, workload: u32) -> Rect {
    let card = topology.wl_card_rects[workload as usize];
    let (center_x, center_y) = card.center();
    let base_radius = 0.5 * (card.w * card.w + card.h * card.h).sqrt() + SAT_RING0_GAP;
    let siblings: Vec<Rect> = (0..topology.sat_slots.slots())
        .filter(|&index| {
            topology.sat_slots.is_active(index)
                && index != slot as usize
                && topology.sat_wl[index] == workload
        })
        .map(|index| topology.sat_rects[index])
        .collect();
    let mut position = 0usize;
    loop {
        const RING_SLOTS: usize = 12;
        let ring = position / RING_SLOTS;
        let angle = (position % RING_SLOTS) as f32 * std::f32::consts::TAU / RING_SLOTS as f32;
        let radius = base_radius + ring as f32 * SAT_RING_GAP;
        let candidate = Rect::new(
            center_x + radius * angle.cos() - SAT_SIZE * 0.5,
            center_y + radius * angle.sin() - SAT_SIZE * 0.5,
            SAT_SIZE,
            SAT_SIZE,
        );
        if !siblings.iter().any(|rect| rect.intersects(&candidate)) {
            let workload_index = workload as usize;
            topology.wl_rects[workload_index] =
                rect_union(topology.wl_rects[workload_index], candidate);
            let namespace = topology.wl_ns[workload_index];
            grow_namespace(topology, namespace, topology.wl_rects[workload_index]);
            return candidate;
        }
        position += 1;
    }
}

// One parent's children, through whichever form the adjacency took.
//
// `Adjacency::build` drops the index vector when every parent's children are
// already contiguous and in order, so an empty `indices` is not an empty scene --
// it means the range *is* the answer. Reading the range without checking would
// silently visit nothing on exactly the scenes that are cheapest to lay out.
fn children(ranges: &[Range<u32>], indices: &[u32], parent: usize) -> Vec<usize> {
    let Some(range) = ranges.get(parent) else {
        return Vec::new();
    };
    let (start, end) = (range.start as usize, range.end as usize);
    if indices.is_empty() {
        return (start..end).collect();
    }
    let end = end.min(indices.len());
    if start >= end {
        return Vec::new();
    }
    indices[start..end].iter().map(|&at| at as usize).collect()
}

// Bring parents back down to what is actually in them.
//
// `grow_workload` and `grow_namespace` are the arriving half, and only growing is
// right there: a pod must be visible the frame it appears, and a card that grew
// to hold it has to stay grown while it is there. Nothing was ever the leaving
// half, so a Deployment scaled to eighty and back to eight kept the card it
// needed at eighty -- which is the first thing anyone notices on a real cluster,
// and was noticed on one.
//
// Three rules make this safe against the property the whole layout exists for:
//
//  - a parent never shrinks below the union of what it still holds, so a child
//    that kept its position can never be clipped by its parent shrinking. Keeping
//    positions is the point; moving them is the reshuffle;
//  - no sibling is repositioned. A shrunk card leaves a gap, and the gap stays
//    until something is placed in it. A layout that closed gaps would move things
//    nobody touched, which is exactly what must not happen;
//  - and it runs only when something departed. Arrivals are the measured path,
//    and they pay nothing for this.
//
// Cost is proportional to the change and not to the scene, because it runs after
// `rebuild_selective` and reads the adjacency that just rebuilt: a workload costs
// its own pods and satellites, a namespace its own workloads. Only the world
// bounds are O(namespaces), which is the term §6.1 already budgets at Z0.
fn fit_after_departures(topology: &mut Topology, dirt: &BatchDirt, mode: LayoutMode) {
    if !dirt.shrank {
        return;
    }
    let (pad, header) = match mode {
        LayoutMode::Spread => (CARD_PAD, CARD_HEADER),
        LayoutMode::Dense => (WL_PAD, WL_HEADER),
    };

    let mut workloads: Vec<u32> = dirt.wls.clone();
    workloads.sort_unstable();
    workloads.dedup();
    for workload in workloads {
        let index = workload as usize;
        if !topology.wl_slots.is_active(index) {
            continue;
        }
        let card = topology.wl_card_rects[index];
        // An empty card keeps the size a single pod would need, so a workload
        // that lost every pod does not collapse to a sliver and then jump back.
        let mut fitted = Rect::new(
            card.x,
            card.y,
            POD_SIZE + pad * 2.0,
            POD_SIZE + pad * 2.0 + header,
        );
        for pod in children(&topology.wl_pod_range, &topology.block_cells, index) {
            let rect = topology.pod_rects[pod];
            fitted.w = fitted.w.max(rect.max_x() - card.x + pad);
            fitted.h = fitted.h.max(rect.max_y() - card.y + pad);
        }
        topology.wl_card_rects[index] = fitted;
        // The halo is the card and whatever orbits it. Satellites were placed on
        // a ring sized by the card they had at the time and they keep those
        // positions, so the halo must still contain them.
        let mut halo = fitted;
        for sat in children(&topology.wl_sat_range, &topology.block_sats, index) {
            halo = rect_union(halo, topology.sat_rects[sat]);
        }
        topology.wl_rects[index] = halo;
    }

    let mut namespaces: Vec<u32> = dirt.nss.clone();
    namespaces.sort_unstable();
    namespaces.dedup();
    for namespace in namespaces {
        let index = namespace as usize;
        if !topology.ns_slots.is_active(index) {
            continue;
        }
        let region = topology.ns_rects[index];
        let mut fitted = Rect::new(
            region.x,
            region.y,
            NS_PAD * 2.0 + POD_SIZE + pad * 2.0,
            NS_PAD * 2.0 + NS_HEADER + POD_SIZE + header + pad * 2.0,
        );
        for workload in children(&topology.ns_wl_range, &topology.region_blocks, index) {
            let rect = topology.wl_rects[workload];
            fitted.w = fitted.w.max(rect.max_x() - region.x + NS_PAD);
            fitted.h = fitted.h.max(rect.max_y() - region.y + NS_PAD);
        }
        topology.ns_rects[index] = fitted;
    }

    // And the world, which the roadmap has carried as "bounds never shrink"
    // since before there was anything to shrink them for.
    let mut bounds = Rect::new(topology.bounds.x, topology.bounds.y, 0.0, 0.0);
    for index in 0..topology.ns_slots.slots() {
        if !topology.ns_slots.is_active(index) {
            continue;
        }
        let rect = topology.ns_rects[index];
        bounds.w = bounds.w.max(rect.max_x() - bounds.x);
        bounds.h = bounds.h.max(rect.max_y() - bounds.y);
    }
    topology.bounds = bounds;
}

fn grow_workload(topology: &mut Topology, workload: u32, child: Rect, pad: f32) {
    let index = workload as usize;
    let card = &mut topology.wl_card_rects[index];
    card.w = card.w.max(child.max_x() - card.x + pad);
    card.h = card.h.max(child.max_y() - card.y + pad);
    topology.wl_rects[index] = rect_union(topology.wl_rects[index], *card);
    let namespace = topology.wl_ns[index];
    grow_namespace(topology, namespace, topology.wl_rects[index]);
}

fn grow_namespace(topology: &mut Topology, namespace: u32, child: Rect) {
    let region = {
        let region = &mut topology.ns_rects[namespace as usize];
        region.w = region.w.max(child.max_x() - region.x + NS_PAD);
        region.h = region.h.max(child.max_y() - region.y + NS_PAD);
        *region
    };
    grow_world(topology, region);
}

fn grow_world(topology: &mut Topology, rect: Rect) {
    topology.bounds.w = topology.bounds.w.max(rect.max_x() - topology.bounds.x);
    topology.bounds.h = topology.bounds.h.max(rect.max_y() - topology.bounds.y);
}

fn rect_union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect::new(
        x,
        y,
        a.max_x().max(b.max_x()) - x,
        a.max_y().max(b.max_y()) - y,
    )
}

// The selective rebuild and the incremental aggregates are only safe if they
// are indistinguishable from recomputing everything. This recomputes
// everything and says so when they are not.
#[cfg(test)]
pub(super) fn verify_derived_state(world: &mut World) {
    let mut topology = world
        .remove_resource::<Topology>()
        .expect("topology present");

    fn child_lists(ranges: &[Range<u32>], indices: &[u32]) -> Vec<Vec<u32>> {
        ranges
            .iter()
            .map(|range| {
                if indices.is_empty() {
                    (range.start..range.end).collect()
                } else {
                    indices[range.start as usize..range.end as usize].to_vec()
                }
            })
            .collect()
    }
    let fresh = Adjacency::build(
        &topology.wl_ns,
        &topology.wl_slots,
        topology.ns_slots.slots(),
    );
    assert_eq!(
        child_lists(&topology.ns_wl_range, &topology.region_blocks),
        child_lists(&fresh.ranges, &fresh.indices),
        "the ns->wl adjacency drifted from a full rebuild"
    );
    let fresh = Adjacency::build(
        &topology.pod_wl,
        &topology.pod_slots,
        topology.wl_slots.slots(),
    );
    assert_eq!(
        child_lists(&topology.wl_pod_range, &topology.block_cells),
        child_lists(&fresh.ranges, &fresh.indices),
        "the wl->pod adjacency drifted from a full rebuild"
    );
    let fresh = Adjacency::build(
        &topology.sat_wl,
        &topology.sat_slots,
        topology.wl_slots.slots(),
    );
    assert_eq!(
        child_lists(&topology.wl_sat_range, &topology.block_sats),
        child_lists(&fresh.ranges, &fresh.indices),
        "the wl->sat adjacency drifted from a full rebuild"
    );

    let mut fresh_counts = vec![0u32; topology.ns_pod_count.len()];
    for pod in 0..topology.pod_slots.slots() {
        if !topology.pod_slots.is_active(pod) {
            continue;
        }
        let workload = topology.pod_wl[pod] as usize;
        if !topology.wl_slots.is_active(workload) {
            continue;
        }
        let namespace = topology.wl_ns[workload] as usize;
        if topology.ns_slots.is_active(namespace) {
            fresh_counts[namespace] += 1;
        }
    }
    assert_eq!(
        topology.ns_pod_count, fresh_counts,
        "ns_pod_count drifted from a full recount"
    );

    let stored_edges = topology.edges.clone();
    let stored_ranges = topology.ns_edge_range.clone();
    let stored_cross = topology.cross_edge_range.clone();
    rebuild_edges(&mut topology);
    assert_eq!(stored_edges, topology.edges, "edges drifted");
    assert_eq!(stored_ranges, topology.ns_edge_range, "edge ranges drifted");
    assert_eq!(
        stored_cross, topology.cross_edge_range,
        "cross range drifted"
    );

    let fresh_aggregates = rebuild_aggregates(world, &topology);
    assert_eq!(
        world.resource::<Aggregates>(),
        &fresh_aggregates,
        "aggregates drifted from a full rebuild"
    );
    world.insert_resource(topology);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_stable_and_reuse_the_lowest_tombstone() {
        let mut slots = SlotMap::default();
        assert_eq!(slots.insert("a".into()), (0, true));
        assert_eq!(slots.insert("b".into()), (1, true));
        assert_eq!(slots.insert("c".into()), (2, true));
        assert_eq!(slots.insert("b".into()), (1, false));
        assert_eq!(slots.remove("b"), Some(1));
        assert_eq!(slots.remove("a"), Some(0));
        assert_eq!(slots.insert("d".into()), (0, true));
        assert_eq!(slots.insert("e".into()), (1, true));
        assert_eq!(slots.uid(2).map(AsRef::as_ref), Some("c"));
    }

    #[test]
    fn adjacency_groups_stable_child_slots_by_parent() {
        let mut children = SlotMap::default();
        for uid in ["a", "b", "c", "d"] {
            children.insert(uid.into());
        }
        children.remove("b");
        let adjacency = Adjacency::build(&[1, 0, 1, 0], &children, 2);
        assert_eq!(adjacency.ranges, [0..1, 1..3]);
        assert_eq!(adjacency.indices, [3, 0, 2]);
        assert!(!adjacency.is_direct());
    }
}
