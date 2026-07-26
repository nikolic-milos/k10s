//! Recorded ingest streams, and the scenarios a data plane has to survive.
//!
//! The point is to make the hard cases testable without a cluster. Contract tests
//! over recorded streams are what let the kube layer be checked against initial
//! sync, a 410 resync, partial permissions, a CRD appearing mid-stream and
//! malformed events, on a machine with no kubeconfig.
//!
//! Storage is in memory for now. Persisting these as fixtures is a protobuf job
//! and deliberately not done here.

use std::sync::Arc;

use crate::ingest::{Capability, DesyncReason, IngestEvent, Intake, Op, Payload, ResourceEvent};
use crate::model::{KindId, State, ToolId};

/// A captured stream, replayable as many times as a test likes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordedStream {
    pub events: Vec<IngestEvent>,
}

impl RecordedStream {
    pub fn new() -> Self {
        RecordedStream::default()
    }

    pub fn record(events: impl IntoIterator<Item = IngestEvent>) -> Self {
        RecordedStream {
            events: events.into_iter().collect(),
        }
    }

    pub fn push(&mut self, event: IngestEvent) -> &mut Self {
        self.events.push(event);
        self
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Feeds the whole stream through an intake, as a producer would.
    pub fn replay_into(&self, intake: &mut Intake) {
        for e in &self.events {
            intake.push(e.clone());
        }
    }

    /// Replays and drains in one step, which is what a single-tick test wants.
    pub fn drain_through(&self, intake: &mut Intake) -> Vec<IngestEvent> {
        self.replay_into(intake);
        intake.drain()
    }

    pub fn resources(&self) -> impl Iterator<Item = &ResourceEvent> {
        self.events.iter().filter_map(|e| match e {
            IngestEvent::Resource(r) => Some(r),
            _ => None,
        })
    }
}

/// Builders for a scope, an owner and an instance, so scenarios stay readable.
pub fn scope(uid: &str, name: &str, op: Op) -> IngestEvent {
    IngestEvent::Resource(ResourceEvent {
        kind: KindId::NAMESPACE,
        uid: uid.into(),
        namespace: Arc::from(""),
        name: name.into(),
        resource_version: 0,
        parent: None,
        op,
        payload: Payload::Scope,
    })
}

pub fn owner(uid: &str, ns: &str, name: &str, kind: KindId, op: Op) -> IngestEvent {
    IngestEvent::Resource(ResourceEvent {
        kind,
        uid: uid.into(),
        namespace: ns.into(),
        name: name.into(),
        resource_version: 0,
        parent: Some(format!("ns-{ns}").into()),
        op,
        payload: Payload::Owner {
            kind,
            tool: ToolId::NONE,
            depends_on: Vec::new(),
        },
    })
}

pub fn instance(uid: &str, ns: &str, parent: &str, state: State, op: Op) -> IngestEvent {
    IngestEvent::Resource(ResourceEvent {
        kind: KindId::POD,
        uid: uid.into(),
        namespace: ns.into(),
        name: uid.into(),
        resource_version: 0,
        parent: Some(parent.into()),
        op,
        payload: Payload::Instance { state },
    })
}

/// A small complete initial sync: one scope, one owner, two instances, then the
/// `Synced` that makes absence meaningful.
pub fn initial_sync() -> RecordedStream {
    RecordedStream::record([
        scope("ns-prod", "prod", Op::Added),
        owner("wl-api", "prod", "api", KindId::DEPLOYMENT, Op::Added),
        instance("pod-1", "prod", "wl-api", State::OK, Op::Added),
        instance("pod-2", "prod", "wl-api", State::OK, Op::Added),
        IngestEvent::Capability {
            kind: KindId::POD,
            verdict: Capability::Watchable,
        },
        IngestEvent::Synced {
            kind: KindId::NAMESPACE,
        },
        IngestEvent::Synced { kind: KindId::POD },
    ])
}

/// A watch that expires and relists. The relist repeats objects as `Added`, which
/// is exactly why coalescing has to be idempotent.
pub fn resync_after_expired() -> RecordedStream {
    let mut s = initial_sync();
    s.push(IngestEvent::Desync {
        kind: KindId::POD,
        reason: DesyncReason::Expired,
    });
    // The relist: same uids, arriving again as Added.
    s.push(instance("pod-1", "prod", "wl-api", State::OK, Op::Added));
    s.push(instance("pod-2", "prod", "wl-api", State::OK, Op::Added));
    // And one that vanished while we were disconnected: absent from the relist,
    // which is the case only Synced makes detectable.
    s.push(IngestEvent::Synced { kind: KindId::POD });
    s
}

/// A cluster where we may read pods but not secrets. The forbidden kind must be
/// reported, not silently empty.
pub fn partial_permissions() -> RecordedStream {
    RecordedStream::record([
        scope("ns-prod", "prod", Op::Added),
        owner("wl-api", "prod", "api", KindId::DEPLOYMENT, Op::Added),
        instance("pod-1", "prod", "wl-api", State::OK, Op::Added),
        IngestEvent::Capability {
            kind: KindId::POD,
            verdict: Capability::Watchable,
        },
        IngestEvent::Capability {
            kind: KindId::SECRET,
            verdict: Capability::Forbidden,
        },
        IngestEvent::Desync {
            kind: KindId::SECRET,
            reason: DesyncReason::Forbidden,
        },
        IngestEvent::Synced { kind: KindId::POD },
    ])
}

/// A CRD kind nobody compiled in, appearing and then being removed while we watch.
pub fn crd_added_and_removed_midstream() -> RecordedStream {
    let vmi = KindId(9_100);
    RecordedStream::record([
        scope("ns-vms", "vms", Op::Added),
        IngestEvent::Capability {
            kind: vmi,
            verdict: Capability::Watchable,
        },
        owner("wl-vmi", "vms", "web-vm", vmi, Op::Added),
        instance("pod-vmi", "vms", "wl-vmi", State::OK, Op::Added),
        IngestEvent::Synced { kind: vmi },
        // The CRD is uninstalled: the kind stops being served entirely.
        owner("wl-vmi", "vms", "web-vm", vmi, Op::Deleted),
        IngestEvent::Capability {
            kind: vmi,
            verdict: Capability::Absent,
        },
    ])
}

/// An undecodable event, then recovery. A steady trickle of these is a bug, but
/// one must not take the stream down.
pub fn malformed_then_recovered() -> RecordedStream {
    let mut s = RecordedStream::record([
        scope("ns-prod", "prod", Op::Added),
        IngestEvent::Desync {
            kind: KindId::POD,
            reason: DesyncReason::Malformed,
        },
    ]);
    s.push(owner(
        "wl-api",
        "prod",
        "api",
        KindId::DEPLOYMENT,
        Op::Added,
    ));
    s.push(instance("pod-1", "prod", "wl-api", State::OK, Op::Added));
    s.push(IngestEvent::Synced { kind: KindId::POD });
    s
}

/// One pod churning through many states inside a single tick.
pub fn churn(updates: usize) -> RecordedStream {
    use crate::model::ReasonId;
    let cycle = [
        ReasonId::RUNNING,
        ReasonId::NOT_READY,
        ReasonId::CRASH_LOOP_BACK_OFF,
        ReasonId::UNKNOWN,
    ];
    let mut s = RecordedStream::record([instance("pod-1", "prod", "wl-api", State::OK, Op::Added)]);
    for i in 0..updates {
        s.push(instance(
            "pod-1",
            "prod",
            "wl-api",
            State::of(cycle[i % cycle.len()]),
            Op::Modified,
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res_count(events: &[IngestEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, IngestEvent::Resource(_)))
            .count()
    }

    fn find_desync(events: &[IngestEvent]) -> Vec<(KindId, DesyncReason)> {
        events
            .iter()
            .filter_map(|e| match e {
                IngestEvent::Desync { kind, reason } => Some((*kind, *reason)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn initial_sync_delivers_every_object_and_then_declares_completion() {
        let mut i = Intake::new();
        let out = initial_sync().drain_through(&mut i);
        assert_eq!(res_count(&out), 4, "scope, owner, two instances");
        let synced: Vec<KindId> = out
            .iter()
            .filter_map(|e| match e {
                IngestEvent::Synced { kind } => Some(*kind),
                _ => None,
            })
            .collect();
        assert_eq!(synced, vec![KindId::NAMESPACE, KindId::POD]);
    }

    #[test]
    fn a_relist_after_410_does_not_duplicate_objects() {
        // The property that matters: coalescing by uid makes a resync idempotent,
        // so a flapping watch cannot inflate the scene.
        let mut i = Intake::new();
        let out = resync_after_expired().drain_through(&mut i);
        assert_eq!(res_count(&out), 4, "relisted pods must not double up");
        assert_eq!(
            find_desync(&out),
            vec![(KindId::POD, DesyncReason::Expired)]
        );
        assert!(DesyncReason::Expired.is_recoverable());
    }

    #[test]
    fn a_forbidden_kind_is_reported_rather_than_empty() {
        let mut i = Intake::new();
        let out = partial_permissions().drain_through(&mut i);
        let caps: Vec<(KindId, Capability)> = out
            .iter()
            .filter_map(|e| match e {
                IngestEvent::Capability { kind, verdict } => Some((*kind, *verdict)),
                _ => None,
            })
            .collect();
        assert!(caps.contains(&(KindId::SECRET, Capability::Forbidden)));
        assert!(caps.contains(&(KindId::POD, Capability::Watchable)));
        // And it must not be retried into the ground.
        let (_, reason) = find_desync(&out)[0];
        assert!(!reason.is_recoverable(), "403 must not look retryable");
        // Pods still arrived: one denied kind cannot take the rest down.
        assert!(res_count(&out) >= 3);
    }

    #[test]
    fn a_crd_flows_through_without_a_compiled_in_kind() {
        let mut i = Intake::new();
        let stream = crd_added_and_removed_midstream();
        let out = stream.drain_through(&mut i);
        let vmi = KindId(9_100);
        assert!(!vmi.is_builtin(), "the fixture must use an unknown kind");

        // Added then Deleted inside one tick elides, which is correct: nothing
        // downstream ever saw the VMI.
        assert!(
            !out.iter().any(|e| matches!(
                e,
                IngestEvent::Resource(r) if r.uid.as_ref() == "wl-vmi"
            )),
            "an object added and removed in one tick should not surface"
        );
        assert_eq!(i.stats().elided, 1);

        // The capability transition still reaches the consumer, so the UI can go
        // from showing the kind to hiding it rather than showing it as broken.
        let caps: Vec<Capability> = out
            .iter()
            .filter_map(|e| match e {
                IngestEvent::Capability { kind, verdict } if *kind == vmi => Some(*verdict),
                _ => None,
            })
            .collect();
        assert_eq!(caps, vec![Capability::Watchable, Capability::Absent]);
    }

    #[test]
    fn a_crd_deleted_in_a_later_tick_does_surface() {
        // The counterpart to the elision above: across ticks, the delete is real.
        let vmi = KindId(9_100);
        let mut i = Intake::new();
        RecordedStream::record([owner("wl-vmi", "vms", "web-vm", vmi, Op::Added)])
            .replay_into(&mut i);
        assert_eq!(res_count(&i.drain()), 1);

        RecordedStream::record([owner("wl-vmi", "vms", "web-vm", vmi, Op::Deleted)])
            .replay_into(&mut i);
        let out = i.drain();
        assert_eq!(res_count(&out), 1);
        assert!(matches!(
            &out[0],
            IngestEvent::Resource(r) if r.op == Op::Deleted
        ));
    }

    #[test]
    fn a_malformed_event_does_not_take_the_stream_down() {
        let mut i = Intake::new();
        let out = malformed_then_recovered().drain_through(&mut i);
        assert_eq!(
            find_desync(&out),
            vec![(KindId::POD, DesyncReason::Malformed)]
        );
        assert_eq!(
            res_count(&out),
            3,
            "objects after the bad event still arrive"
        );
    }

    #[test]
    fn churn_collapses_to_one_event_per_object_per_tick() {
        let mut i = Intake::new();
        let out = churn(200).drain_through(&mut i);
        assert_eq!(res_count(&out), 1, "200 updates, one publish");
        assert_eq!(i.stats().coalesced, 200);
        // And the surviving event carries the last state, not the first.
        let IngestEvent::Resource(r) = &out[0] else {
            panic!("expected a resource event")
        };
        let Payload::Instance { state } = r.payload else {
            panic!("expected an instance payload")
        };
        assert_eq!(state, State::of(crate::model::ReasonId::UNKNOWN));
    }

    #[test]
    fn replaying_the_same_stream_twice_gives_the_same_result() {
        let stream = resync_after_expired();
        let mut a = Intake::new();
        let mut b = Intake::new();
        assert_eq!(stream.drain_through(&mut a), stream.drain_through(&mut b));
    }
}
