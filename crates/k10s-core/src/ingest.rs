//! The ingestion contract: an event stream, not a snapshot type.
//!
//! Everything that can tell k10s what is in a cluster speaks this. The generator
//! implements it, a recorded stream implements it, and the kube data plane will
//! implement it, which is what stops the world from taking any one producer's
//! type as its input contract.
//!
//! Three things beyond the obvious add/modify/delete, each because leaving them
//! out produces a specific wrong UI:
//!
//! - [`IngestEvent::Synced`] separates "this kind holds nothing" from "this kind
//!   has not loaded yet". Without it an empty list is indistinguishable from a
//!   pending one, and the app looks like it works while showing nothing.
//! - [`IngestEvent::Desync`] says a stream broke and why, so a 410 becomes a
//!   resync and a 403 becomes a labelled, disabled affordance rather than both
//!   silently becoming an empty list.
//! - [`IngestEvent::Capability`] carries an RBAC verdict as a first-class input,
//!   because on a restricted cluster "forbidden" is the normal answer and must
//!   not read as "absent".
//!
//! A snapshot is a replay of [`Op::Added`], so a producer that only knows how to
//! describe a whole cluster is still a producer.

use std::collections::HashMap;
use std::sync::Arc;

use crate::model::{KindId, State, ToolId};

/// What happened to one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Added,
    Modified,
    Deleted,
}

/// The scene-shaped part of an event.
///
/// Deliberately not the raw API object: the payload carries what the map needs
/// and nothing else, so a watch on a 40 KB Pod does not drag 40 KB through the
/// intake. Richer detail is fetched on demand by whatever panel wants it.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    /// A namespace today, a cluster once clusters are super-regions.
    Scope,
    /// Something that owns instances: Deployment, StatefulSet, a CRD.
    Owner {
        kind: KindId,
        tool: ToolId,
        /// Uids of other owners this one depends on.
        ///
        /// A relation, not a resource, because a real cluster has no edge object:
        /// these come from Service selectors and owner references. Carrying uids
        /// rather than indices is what lets a dependency cross a namespace, which
        /// the generator's namespace-local `deps` never could.
        depends_on: Vec<Arc<str>>,
    },
    /// A single instance, which is where a reason genuinely exists.
    Instance { state: State },
    /// Attached to an owner rather than owning anything: PVC, Service, Secret.
    Attached { kind: KindId, detail: Arc<str> },
}

impl Payload {
    /// The kind this payload describes, for the levels that carry one.
    pub fn kind(&self) -> Option<KindId> {
        match self {
            Payload::Scope => Some(KindId::NAMESPACE),
            Payload::Owner { kind, .. } => Some(*kind),
            Payload::Instance { .. } => Some(KindId::POD),
            Payload::Attached { kind, .. } => Some(*kind),
        }
    }
}

/// One object changing.
///
/// `kind` is already a [`KindId`], interned by the producer, because the producer
/// is the only side that holds a catalog and because a string GVK below this
/// boundary would put string comparison on the path to the paint loop.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceEvent {
    pub kind: KindId,
    /// Stable cluster-assigned identity. The coalescing key, and the only field
    /// that may not change over an object's life.
    pub uid: Arc<str>,
    /// Empty for cluster-scoped objects.
    pub namespace: Arc<str>,
    pub name: Arc<str>,
    /// Parsed `resourceVersion`. Zero means the producer had none, which a
    /// generated or recorded stream is entitled to.
    pub resource_version: u64,
    /// The owning object's uid: an instance's owner, an owner's scope. `None` for
    /// a scope, which owns itself.
    pub parent: Option<Arc<str>>,
    pub op: Op,
    pub payload: Payload,
}

/// Why a stream stopped being trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesyncReason {
    /// HTTP 410: the resourceVersion is too old. Recoverable by relisting.
    Expired,
    /// HTTP 403. Not recoverable by retrying, and must surface as a labelled
    /// affordance rather than an empty list.
    Forbidden,
    /// The connection dropped without an error we can attribute.
    Closed,
    /// An event we could not decode. Recoverable, but worth counting: a stream
    /// that produces these steadily is a bug somewhere.
    Malformed,
    /// Intake could not hold the pending set. Recoverable only by relisting,
    /// which is the point: dropping to a resync beats growing without bound or
    /// blocking the watch until it expires.
    Overflow,
}

impl DesyncReason {
    /// Whether relisting can fix this. `Forbidden` cannot, and retrying it in a
    /// loop is how a restricted cluster gets hammered.
    pub fn is_recoverable(self) -> bool {
        !matches!(self, DesyncReason::Forbidden)
    }
}

/// What we are allowed to do with a kind, probed up front rather than discovered
/// through a failed request mid-interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Listable and watchable.
    Watchable,
    /// Present, but we may not read it. The UI must say so.
    Forbidden,
    /// Not served by this cluster at all, which is invisible rather than broken.
    Absent,
}

/// Everything a producer can say.
#[derive(Debug, Clone, PartialEq)]
pub enum IngestEvent {
    Resource(ResourceEvent),
    /// This kind's initial list is complete: anything absent is genuinely absent.
    Synced {
        kind: KindId,
    },
    Desync {
        kind: KindId,
        reason: DesyncReason,
    },
    Capability {
        kind: KindId,
        verdict: Capability,
    },
}

/// How many distinct objects may sit in the pending set before intake gives up
/// and asks for a resync. Sized so a large cluster's initial sync fits, while a
/// runaway producer cannot exhaust memory.
pub const DEFAULT_INTAKE_CAPACITY: usize = 262_144;

/// Counters for what intake did, so a resync storm is visible rather than
/// inferred from a stall.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntakeStats {
    /// Resource events accepted.
    pub accepted: u64,
    /// Events that replaced an earlier pending event for the same object. This is
    /// the win: a pod flapping 50 times between ticks costs one publish.
    pub coalesced: u64,
    /// Objects added and deleted before anyone observed them, elided entirely.
    pub elided: u64,
    /// Events dropped because the pending set was full.
    pub dropped: u64,
    /// Kinds put into desync, including by overflow.
    pub desyncs: u64,
}

/// Coalesces events by object uid and hands them over once per world tick.
///
/// Decoupling ingest rate from publish rate is structural here, not a tuning
/// knob: a producer may push as fast as it likes, and the world still does one
/// pass per tick over at most one event per changed object.
///
/// Order is preserved per object (last write wins) but not globally across
/// objects, which is inherent to coalescing. Control events are delivered after
/// the resource batch so a `Synced` cannot be observed before the events it
/// completes.
#[derive(Debug)]
pub struct Intake {
    /// Pending resource event per object, in first-touch order so a replay is
    /// deterministic.
    pending: Vec<Option<ResourceEvent>>,
    by_uid: HashMap<Arc<str>, usize>,
    control: Vec<IngestEvent>,
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
            by_uid: HashMap::new(),
            control: Vec::new(),
            capacity,
            stats: IntakeStats::default(),
        }
    }

    pub fn stats(&self) -> IntakeStats {
        self.stats
    }

    /// Whether anything is waiting. The world thread parks on this rather than
    /// ticking regardless.
    pub fn is_empty(&self) -> bool {
        self.by_uid.is_empty() && self.control.is_empty()
    }

    pub fn push(&mut self, event: IngestEvent) {
        match event {
            IngestEvent::Resource(ev) => self.push_resource(ev),
            other => {
                if let IngestEvent::Desync { .. } = other {
                    self.stats.desyncs += 1;
                }
                self.control.push(other);
            }
        }
    }

    fn push_resource(&mut self, ev: ResourceEvent) {
        if let Some(&slot) = self.by_uid.get(&ev.uid) {
            let prev = self.pending[slot]
                .as_ref()
                .expect("an indexed slot always holds an event");
            // Added then Deleted inside one tick: nothing downstream ever saw the
            // object, so there is nothing to tell it about.
            if prev.op == Op::Added && ev.op == Op::Deleted {
                self.pending[slot] = None;
                self.by_uid.remove(&ev.uid);
                self.stats.elided += 1;
                return;
            }
            // A Deleted must not be downgraded by a late Modified from the same
            // batch; ordering within a uid is last-write-wins otherwise.
            if prev.op == Op::Deleted && ev.op != Op::Added {
                self.stats.coalesced += 1;
                return;
            }
            self.pending[slot] = Some(ev);
            self.stats.coalesced += 1;
            return;
        }

        if self.by_uid.len() >= self.capacity {
            // Bounded, so a resync storm cannot exhaust memory. Blocking the
            // producer instead would just stall the watch until it expires, which
            // ends in the same resync with worse latency.
            self.stats.dropped += 1;
            let kind = ev.kind;
            if !self.control.iter().any(|c| {
                matches!(
                    c,
                    IngestEvent::Desync {
                        kind: k,
                        reason: DesyncReason::Overflow
                    } if *k == kind
                )
            }) {
                self.stats.desyncs += 1;
                self.control.push(IngestEvent::Desync {
                    kind,
                    reason: DesyncReason::Overflow,
                });
            }
            return;
        }

        let slot = self.pending.len();
        self.by_uid.insert(ev.uid.clone(), slot);
        self.pending.push(Some(ev));
        self.stats.accepted += 1;
    }

    /// Moves one tick's worth of events into `out`, resource events first.
    ///
    /// Appends rather than clearing, so a caller can accumulate across sources.
    pub fn drain_into(&mut self, out: &mut Vec<IngestEvent>) {
        out.reserve(self.pending.len() + self.control.len());
        // Elided objects leave holes, which is why this is a flatten rather than an
        // index walk.
        for ev in self.pending.drain(..).flatten() {
            out.push(IngestEvent::Resource(ev));
        }
        out.append(&mut self.control);
        self.by_uid.clear();
    }

    pub fn drain(&mut self) -> Vec<IngestEvent> {
        let mut out = Vec::new();
        self.drain_into(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReasonId;

    fn res(uid: &str, op: Op, rv: u64) -> IngestEvent {
        IngestEvent::Resource(ResourceEvent {
            kind: KindId::POD,
            uid: uid.into(),
            namespace: "default".into(),
            name: uid.into(),
            resource_version: rv,
            parent: Some("owner".into()),
            op,
            payload: Payload::Instance { state: State::OK },
        })
    }

    fn uids(events: &[IngestEvent]) -> Vec<(String, Op, u64)> {
        events
            .iter()
            .filter_map(|e| match e {
                IngestEvent::Resource(r) => Some((r.uid.to_string(), r.op, r.resource_version)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_flapping_object_costs_one_event_per_tick() {
        // The whole reason intake exists: ingest rate stops driving publish rate.
        let mut i = Intake::new();
        i.push(res("pod-a", Op::Added, 1));
        for rv in 2..=50 {
            i.push(res("pod-a", Op::Modified, rv));
        }
        let out = i.drain();
        assert_eq!(uids(&out), vec![("pod-a".into(), Op::Modified, 50)]);
        assert_eq!(i.stats().accepted, 1);
        assert_eq!(i.stats().coalesced, 49);
    }

    #[test]
    fn added_then_deleted_in_one_tick_is_elided() {
        // A pod that came and went between ticks was never observed downstream,
        // so publishing its arrival and departure would be pure waste.
        let mut i = Intake::new();
        i.push(res("ghost", Op::Added, 1));
        i.push(res("ghost", Op::Deleted, 2));
        assert!(i.is_empty(), "elided object still pending");
        assert!(i.drain().is_empty());
        assert_eq!(i.stats().elided, 1);

        // But a delete of something that existed before this tick must survive.
        let mut i = Intake::new();
        i.push(res("real", Op::Deleted, 9));
        assert_eq!(uids(&i.drain()), vec![("real".into(), Op::Deleted, 9)]);
        assert_eq!(i.stats().elided, 0);
    }

    #[test]
    fn a_delete_is_not_downgraded_by_a_late_modify() {
        // Producers can interleave; a Modified arriving after a Deleted for the
        // same uid must not resurrect it.
        let mut i = Intake::new();
        i.push(res("pod-a", Op::Modified, 1));
        i.push(res("pod-a", Op::Deleted, 2));
        i.push(res("pod-a", Op::Modified, 3));
        assert_eq!(uids(&i.drain()), vec![("pod-a".into(), Op::Deleted, 2)]);
    }

    #[test]
    fn a_uid_reused_after_delete_is_added_again() {
        // Deleted then Added is a genuinely new object on the same uid slot, which
        // must not be swallowed by the delete-wins rule above.
        let mut i = Intake::new();
        i.push(res("pod-a", Op::Modified, 1));
        i.push(res("pod-a", Op::Deleted, 2));
        i.push(res("pod-a", Op::Added, 3));
        assert_eq!(uids(&i.drain()), vec![("pod-a".into(), Op::Added, 3)]);
    }

    #[test]
    fn distinct_objects_keep_first_touch_order() {
        let mut i = Intake::new();
        for uid in ["c", "a", "b"] {
            i.push(res(uid, Op::Added, 1));
        }
        i.push(res("a", Op::Modified, 7));
        let got: Vec<String> = uids(&i.drain()).into_iter().map(|(u, _, _)| u).collect();
        assert_eq!(got, vec!["c", "a", "b"], "order must not depend on hashing");
    }

    #[test]
    fn control_events_arrive_after_the_batch_they_complete() {
        // Synced must never be observed before the events it declares complete,
        // or the UI concludes a populated kind is empty.
        let mut i = Intake::new();
        i.push(res("pod-a", Op::Added, 1));
        i.push(IngestEvent::Synced { kind: KindId::POD });
        i.push(res("pod-b", Op::Added, 1));
        let out = i.drain();
        let synced_at = out
            .iter()
            .position(|e| matches!(e, IngestEvent::Synced { .. }))
            .expect("synced present");
        assert_eq!(synced_at, out.len() - 1);
        assert_eq!(uids(&out).len(), 2);
    }

    #[test]
    fn overflow_asks_for_a_resync_instead_of_growing() {
        // Bounded on purpose: unbounded dies on a resync storm, and blocking the
        // producer just stalls the watch until it expires.
        let mut i = Intake::with_capacity(3);
        for n in 0..10 {
            i.push(res(&format!("pod-{n}"), Op::Added, 1));
        }
        let s = i.stats();
        assert_eq!(s.accepted, 3);
        assert_eq!(s.dropped, 7);
        assert_eq!(
            s.desyncs, 1,
            "one desync per kind, not one per dropped event"
        );

        let out = i.drain();
        assert_eq!(uids(&out).len(), 3);
        assert!(out.iter().any(|e| matches!(
            e,
            IngestEvent::Desync {
                reason: DesyncReason::Overflow,
                ..
            }
        )));
    }

    #[test]
    fn an_object_already_pending_still_coalesces_when_full() {
        // Capacity bounds distinct objects, not updates: a full intake must still
        // accept news about something it is already tracking.
        let mut i = Intake::with_capacity(2);
        i.push(res("a", Op::Added, 1));
        i.push(res("b", Op::Added, 1));
        i.push(res("a", Op::Modified, 5));
        assert_eq!(i.stats().dropped, 0);
        let got = uids(&i.drain());
        assert!(got.contains(&("a".into(), Op::Modified, 5)));
    }

    #[test]
    fn draining_resets_capacity_for_the_next_tick() {
        let mut i = Intake::with_capacity(2);
        i.push(res("a", Op::Added, 1));
        i.push(res("b", Op::Added, 1));
        assert_eq!(i.drain().len(), 2);
        assert!(i.is_empty());
        i.push(res("c", Op::Added, 1));
        i.push(res("d", Op::Added, 1));
        assert_eq!(
            i.stats().dropped,
            0,
            "capacity is per tick, not per lifetime"
        );
        assert_eq!(i.drain().len(), 2);
    }

    #[test]
    fn forbidden_is_the_one_desync_retrying_cannot_fix() {
        assert!(!DesyncReason::Forbidden.is_recoverable());
        for r in [
            DesyncReason::Expired,
            DesyncReason::Closed,
            DesyncReason::Malformed,
            DesyncReason::Overflow,
        ] {
            assert!(r.is_recoverable(), "{r:?}");
        }
    }

    #[test]
    fn payload_reports_the_kind_for_every_level() {
        assert_eq!(Payload::Scope.kind(), Some(KindId::NAMESPACE));
        assert_eq!(
            Payload::Instance {
                state: State::of(ReasonId::CRASH_LOOP_BACK_OFF)
            }
            .kind(),
            Some(KindId::POD)
        );
        assert_eq!(
            Payload::Owner {
                kind: KindId::STATEFUL_SET,
                tool: ToolId::POSTGRES,
                depends_on: Vec::new()
            }
            .kind(),
            Some(KindId::STATEFUL_SET)
        );
        assert_eq!(
            Payload::Attached {
                kind: KindId::VOLUME,
                detail: "8Gi".into()
            }
            .kind(),
            Some(KindId::VOLUME)
        );
        // A CRD flows through unchanged, which is the point of the open model.
        assert_eq!(
            Payload::Owner {
                kind: KindId(9_001),
                tool: ToolId::NONE,
                depends_on: Vec::new()
            }
            .kind(),
            Some(KindId(9_001))
        );
    }

    #[test]
    fn drain_into_appends_so_sources_can_be_merged() {
        let mut out = vec![IngestEvent::Synced {
            kind: KindId::NAMESPACE,
        }];
        let mut i = Intake::new();
        i.push(res("a", Op::Added, 1));
        i.drain_into(&mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], IngestEvent::Synced { .. }));
    }
}
