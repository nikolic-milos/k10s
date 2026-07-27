use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;
use std::sync::Arc;

use bevy_ecs::prelude::{Mut, World};
use k10s_core::layout::{
    CARD_HEADER, CARD_PAD, NS_GAP, NS_HEADER, NS_PAD, POD_GAP, POD_PITCH, POD_SIZE, SAT_RING_GAP,
    SAT_RING0_GAP, SAT_SIZE, WL_GAP, WL_HEADER, WL_PAD,
};
use k10s_core::{EdgeInst, IngestEvent, KindId, Op, Payload, Rect, ResourceEvent, State, ToolId};

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
    for event in events {
        let IngestEvent::Resource(resource) = event else {
            continue;
        };
        apply_resource(world, &mut topology, resource, mode);
    }
    rebuild_structure(&mut topology);
    topology.spatial_revision += 1;
    let aggregates = rebuild_aggregates(world, &topology);
    world.insert_resource(topology);
    world.insert_resource(aggregates);
    world.resource_mut::<DirtyPods>().0.clear();
    {
        let mut pool = world.resource_mut::<SnapshotPool>();
        for pending in &mut pool.pending {
            *pending = Pending::full();
        }
    }
    world.resource_mut::<Dirty>().0 = true;
    true
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
    resource: &ResourceEvent,
    mode: LayoutMode,
) {
    if resource.op == Op::Deleted {
        remove_resource(topology, resource);
        return;
    }
    match &resource.payload {
        Payload::Scope => upsert_namespace(topology, resource),
        Payload::Owner {
            kind,
            tool,
            depends_on,
        } => upsert_workload(topology, resource, *kind, *tool, depends_on, mode),
        Payload::Instance { state } => upsert_pod(world, topology, resource, *state, mode),
        Payload::Attached { kind, detail } if mode.emits_attachments() => {
            upsert_satellite(topology, resource, *kind, detail)
        }
        Payload::Attached { .. } => {}
    }
}

fn upsert_namespace(topology: &mut Topology, resource: &ResourceEvent) {
    let (slot, inserted) = topology.ns_slots.insert(resource.uid.clone());
    let index = slot as usize;
    ensure_namespace(topology, index + 1);
    topology.ns_labels[index] = resource.name.clone();
    if !inserted {
        return;
    }

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

fn upsert_workload(
    topology: &mut Topology,
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
    let (slot, inserted) = topology.wl_slots.insert(resource.uid.clone());
    let index = slot as usize;
    ensure_workload(topology, index + 1);
    let moved = !inserted && topology.wl_ns[index] != namespace;
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
    let moved = !inserted && topology.pod_wl[index] != workload;
    topology.pod_labels[index] = resource.name.clone();
    topology.pod_wl[index] = workload;
    if inserted || moved {
        topology.pod_rects[index] = place_pod(topology, slot, workload, mode);
    }
    let entity = topology.pod_entities[index];
    if let Some(mut health) = world.get_mut::<PodH>(entity) {
        health.0 = state;
    }
}

fn upsert_satellite(
    topology: &mut Topology,
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
    if inserted || moved {
        topology.sat_rects[index] = place_satellite(topology, slot, workload);
    }
}

fn remove_resource(topology: &mut Topology, resource: &ResourceEvent) {
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
                remove_workload(topology, &uid);
            }
            if let Some(slot) = topology.ns_slots.remove(&resource.uid) {
                let index = slot as usize;
                topology.ns_labels[index] = Arc::from("");
                topology.ns_rects[index] = DEAD_RECT;
                topology.ns_pod_count[index] = 0;
            }
        }
        Payload::Owner { .. } => remove_workload(topology, &resource.uid),
        Payload::Instance { .. } => remove_pod(topology, &resource.uid),
        Payload::Attached { .. } => remove_satellite(topology, &resource.uid),
    }
}

fn remove_workload(topology: &mut Topology, uid: &str) {
    let Some(workload) = topology.wl_slots.get(uid) else {
        return;
    };
    let pods: Vec<Arc<str>> = (0..topology.pod_slots.slots())
        .filter(|&index| topology.pod_slots.is_active(index) && topology.pod_wl[index] == workload)
        .filter_map(|index| topology.pod_slots.uid(index as u32).cloned())
        .collect();
    for uid in pods {
        remove_pod(topology, &uid);
    }
    let satellites: Vec<Arc<str>> = (0..topology.sat_slots.slots())
        .filter(|&index| topology.sat_slots.is_active(index) && topology.sat_wl[index] == workload)
        .filter_map(|index| topology.sat_slots.uid(index as u32).cloned())
        .collect();
    for uid in satellites {
        remove_satellite(topology, &uid);
    }
    if let Some(slot) = topology.wl_slots.remove(uid) {
        let index = slot as usize;
        topology.wl_labels[index] = Arc::from("");
        topology.wl_rects[index] = DEAD_RECT;
        topology.wl_card_rects[index] = DEAD_RECT;
        topology.wl_ns[index] = NO_SLOT;
        topology.wl_depends_on[index].clear();
    }
}

fn remove_pod(topology: &mut Topology, uid: &str) {
    if let Some(slot) = topology.pod_slots.remove(uid) {
        let index = slot as usize;
        topology.pod_labels[index] = Arc::from("");
        topology.pod_rects[index] = DEAD_RECT;
        topology.pod_wl[index] = NO_SLOT;
    }
}

fn remove_satellite(topology: &mut Topology, uid: &str) {
    if let Some(slot) = topology.sat_slots.remove(uid) {
        let index = slot as usize;
        topology.sat_labels[index] = Arc::from("");
        topology.sat_details[index] = Arc::from("");
        topology.sat_rects[index] = DEAD_RECT;
        topology.sat_wl[index] = NO_SLOT;
    }
}

fn rebuild_structure(topology: &mut Topology) {
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

    rebuild_edges(topology);
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

fn ensure_namespace(topology: &mut Topology, len: usize) {
    topology.ns_labels.resize_with(len, || Arc::from(""));
    topology.ns_rects.resize(len, DEAD_RECT);
    topology.ns_wl_range.resize(len, 0..0);
    topology.ns_pod_count.resize(len, 0);
}

fn ensure_workload(topology: &mut Topology, len: usize) {
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

fn ensure_pod(world: &mut World, topology: &mut Topology, len: usize) {
    topology.pod_labels.resize_with(len, || Arc::from(""));
    topology.pod_rects.resize(len, DEAD_RECT);
    topology.pod_wl.resize(len, NO_SLOT);
    while topology.pod_entities.len() < len {
        topology
            .pod_entities
            .push(world.spawn((PodH(State::OK),)).id());
    }
}

fn ensure_satellite(topology: &mut Topology, len: usize) {
    topology.sat_labels.resize_with(len, || Arc::from(""));
    topology.sat_details.resize_with(len, || Arc::from(""));
    topology.sat_kinds.resize(len, KindId::SERVICE);
    topology.sat_rects.resize(len, DEAD_RECT);
    topology.sat_wl.resize(len, NO_SLOT);
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
        let occupied = (0..topology.pod_slots.slots()).any(|index| {
            topology.pod_slots.is_active(index)
                && index != slot as usize
                && topology.pod_wl[index] == workload
                && topology.pod_rects[index] == candidate
        });
        if !occupied {
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
        let occupied = (0..topology.sat_slots.slots()).any(|index| {
            topology.sat_slots.is_active(index)
                && index != slot as usize
                && topology.sat_wl[index] == workload
                && topology.sat_rects[index].intersects(&candidate)
        });
        if !occupied {
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
