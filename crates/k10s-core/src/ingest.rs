use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::model::{KindId, State, ToolId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Scope,
    Owner {
        kind: KindId,
        tool: ToolId,
        depends_on: Vec<Arc<str>>,
    },
    Instance {
        state: State,
    },
    Attached {
        kind: KindId,
        detail: Arc<str>,
    },
}

impl Payload {
    pub fn kind(&self) -> Option<KindId> {
        match self {
            Payload::Scope => Some(KindId::NAMESPACE),
            Payload::Owner { kind, .. } => Some(*kind),
            Payload::Instance { .. } => Some(KindId::POD),
            Payload::Attached { kind, .. } => Some(*kind),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceEvent {
    pub kind: KindId,
    pub uid: Arc<str>,
    pub namespace: Arc<str>,
    pub name: Arc<str>,
    pub resource_version: u64,
    pub parent: Option<Arc<str>>,
    pub op: Op,
    pub payload: Payload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesyncReason {
    Expired,
    Forbidden,
    Closed,
    Malformed,
    Overflow,
}

impl DesyncReason {
    pub fn is_recoverable(self) -> bool {
        !matches!(self, DesyncReason::Forbidden)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Watchable,
    Forbidden,
    Absent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IngestEvent {
    Resource(ResourceEvent),
    Synced { kind: KindId },
    Desync { kind: KindId, reason: DesyncReason },
    Capability { kind: KindId, verdict: Capability },
}

pub const DEFAULT_INTAKE_CAPACITY: usize = 262_144;

pub const CONTROL_CAPACITY: usize = 4_096;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntakeStats {
    pub accepted: u64,
    pub coalesced: u64,
    pub superseded: u64,
    pub elided: u64,
    pub dropped: u64,
    pub desyncs: u64,
}

#[derive(Debug)]
pub struct Intake {
    pending: Vec<Option<ResourceEvent>>,
    first_added: Vec<bool>,
    by_uid: HashMap<Arc<str>, usize>,
    control: Vec<IngestEvent>,
    overflowed: HashSet<KindId>,
    unrecoverable: HashMap<KindId, DesyncReason>,
    capacity: usize,
    stats: IntakeStats,
}

impl Default for Intake {
    fn default() -> Self {
        Intake::with_capacity(DEFAULT_INTAKE_CAPACITY)
    }
}

impl Intake {
    pub fn new() -> Self {
        Intake::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Intake {
            pending: Vec::new(),
            first_added: Vec::new(),
            by_uid: HashMap::new(),
            control: Vec::new(),
            overflowed: HashSet::new(),
            unrecoverable: HashMap::new(),
            capacity,
            stats: IntakeStats::default(),
        }
    }

    pub fn stats(&self) -> IntakeStats {
        self.stats
    }

    pub fn is_empty(&self) -> bool {
        self.by_uid.is_empty()
            && self.control.is_empty()
            && self.overflowed.is_empty()
            && self.unrecoverable.is_empty()
    }

    pub fn push(&mut self, event: IngestEvent) {
        match event {
            IngestEvent::Resource(ev) => self.push_resource(ev),
            IngestEvent::Synced { kind }
            | IngestEvent::Desync { kind, .. }
            | IngestEvent::Capability { kind, .. } => self.push_control(kind, event),
        }
    }

    fn push_control(&mut self, kind: KindId, event: IngestEvent) {
        if self.control.len() >= CONTROL_CAPACITY {
            self.stats.dropped += 1;
            if let IngestEvent::Desync { reason, .. } = event
                && !reason.is_recoverable()
            {
                self.retain_unrecoverable(kind, reason);
            }
            self.signal_overflow(kind);
            return;
        }
        if let IngestEvent::Desync { .. } = event {
            self.stats.desyncs += 1;
        }
        self.control.push(event);
    }

    /// Keep a desync retrying cannot fix even when the control buffer is full.
    ///
    /// Overflow replaces dropped control events with one recoverable `Overflow`
    /// desync per kind. A `Forbidden` verdict answered that way would tell the
    /// world to retry a permission it will never be granted, so the verdict is
    /// held aside -- bounded and deduplicated like the overflow set -- and
    /// drained after it.
    fn retain_unrecoverable(&mut self, kind: KindId, reason: DesyncReason) {
        if self.unrecoverable.len() < CONTROL_CAPACITY
            && self.unrecoverable.insert(kind, reason).is_none()
        {
            self.stats.desyncs += 1;
        }
    }

    fn signal_overflow(&mut self, kind: KindId) {
        if self.overflowed.len() < CONTROL_CAPACITY && self.overflowed.insert(kind) {
            self.stats.desyncs += 1;
        }
    }

    fn push_resource(&mut self, ev: ResourceEvent) {
        if let Some(&slot) = self.by_uid.get(&ev.uid) {
            let prev = self.pending[slot]
                .as_ref()
                .expect("an indexed slot always holds an event");
            if ev.op == Op::Deleted && self.first_added[slot] {
                self.by_uid.remove(&ev.uid);
                if slot + 1 == self.pending.len() {
                    self.pending.pop();
                    self.first_added.pop();
                } else {
                    self.pending[slot] = None;
                }
                self.stats.elided += 1;
                let holes = self.pending.len() - self.by_uid.len();
                if holes >= self.by_uid.len() {
                    self.compact();
                }
                return;
            }
            if prev.op == Op::Deleted && ev.op != Op::Added {
                self.stats.superseded += 1;
                return;
            }
            self.pending[slot] = Some(ev);
            self.stats.coalesced += 1;
            return;
        }

        if self.by_uid.len() >= self.capacity {
            self.stats.dropped += 1;
            self.signal_overflow(ev.kind);
            return;
        }

        let slot = self.pending.len();
        self.by_uid.insert(ev.uid.clone(), slot);
        self.first_added.push(ev.op == Op::Added);
        self.pending.push(Some(ev));
        self.stats.accepted += 1;
    }

    fn compact(&mut self) {
        let mut live = 0;
        for slot in 0..self.pending.len() {
            if self.pending[slot].is_none() {
                continue;
            }
            if slot != live {
                self.pending.swap(slot, live);
                self.first_added.swap(slot, live);
            }
            let uid = &self.pending[live]
                .as_ref()
                .expect("the slot just took a live event")
                .uid;
            *self.by_uid.get_mut(uid).expect("a live event is indexed") = live;
            live += 1;
        }
        self.pending.truncate(live);
        self.first_added.truncate(live);
    }

    pub fn drain_into(&mut self, out: &mut Vec<IngestEvent>) {
        out.reserve(
            self.pending.len()
                + self.control.len()
                + self.overflowed.len()
                + self.unrecoverable.len(),
        );
        for ev in self.pending.drain(..).flatten() {
            out.push(IngestEvent::Resource(ev));
        }
        out.append(&mut self.control);
        let mut overflowed: Vec<KindId> = self.overflowed.drain().collect();
        overflowed.sort_unstable_by_key(|kind| kind.0);
        out.extend(overflowed.into_iter().map(|kind| IngestEvent::Desync {
            kind,
            reason: DesyncReason::Overflow,
        }));
        let mut unrecoverable: Vec<(KindId, DesyncReason)> = self.unrecoverable.drain().collect();
        unrecoverable.sort_unstable_by_key(|(kind, _)| kind.0);
        out.extend(
            unrecoverable
                .into_iter()
                .map(|(kind, reason)| IngestEvent::Desync { kind, reason }),
        );
        self.first_added.clear();
        self.by_uid.clear();
    }

    pub fn drain(&mut self) -> Vec<IngestEvent> {
        let mut out = Vec::new();
        self.drain_into(&mut out);
        out
    }
}

#[cfg(test)]
#[path = "ingest_test.rs"]
mod tests;
