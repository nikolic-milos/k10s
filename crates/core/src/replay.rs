use std::sync::Arc;

use crate::ingest::{Capability, DesyncReason, IngestEvent, Intake, Op, Payload, ResourceEvent};
use crate::model::{KindId, State, ToolId};

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

    pub fn replay_into(&self, intake: &mut Intake) {
        for e in &self.events {
            intake.push(e.clone());
        }
    }

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

pub fn resync_after_expired() -> RecordedStream {
    let mut s = initial_sync();
    s.push(IngestEvent::Desync {
        kind: KindId::POD,
        reason: DesyncReason::Expired,
    });
    s.push(instance("pod-1", "prod", "wl-api", State::OK, Op::Added));
    s.push(instance("pod-2", "prod", "wl-api", State::OK, Op::Added));
    s.push(IngestEvent::Synced { kind: KindId::POD });
    s
}

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
        owner("wl-vmi", "vms", "web-vm", vmi, Op::Deleted),
        IngestEvent::Capability {
            kind: vmi,
            verdict: Capability::Absent,
        },
    ])
}

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

pub fn bookmarked_reconnect() -> RecordedStream {
    let mut s = initial_sync();
    let mut later = instance("pod-1", "prod", "wl-api", State::OK, Op::Modified);
    if let IngestEvent::Resource(r) = &mut later {
        r.resource_version = 4_096;
    }
    s.push(later);
    let mut gone = instance("pod-2", "prod", "wl-api", State::OK, Op::Deleted);
    if let IngestEvent::Resource(r) = &mut gone {
        r.resource_version = 4_097;
    }
    s.push(gone);
    s
}

pub fn unknown_kind_and_reason() -> RecordedStream {
    let widget = KindId(9_200);
    let unnameable = crate::model::ReasonId(9_300);
    RecordedStream::record([
        scope("ns-edge", "edge", Op::Added),
        IngestEvent::Capability {
            kind: widget,
            verdict: Capability::Watchable,
        },
        owner("wl-widget", "edge", "widget", widget, Op::Added),
        instance(
            "pod-widget",
            "edge",
            "wl-widget",
            State {
                severity: crate::model::Severity::Err,
                reason: unnameable,
            },
            Op::Added,
        ),
        IngestEvent::Synced { kind: widget },
        IngestEvent::Synced { kind: KindId::POD },
    ])
}

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
        let (_, reason) = find_desync(&out)[0];
        assert!(!reason.is_recoverable(), "403 must not look retryable");
        assert!(res_count(&out) >= 3);
    }

    #[test]
    fn a_crd_flows_through_without_a_compiled_in_kind() {
        let mut i = Intake::new();
        let stream = crd_added_and_removed_midstream();
        let out = stream.drain_through(&mut i);
        let vmi = KindId(9_100);
        assert!(!vmi.is_builtin(), "the fixture must use an unknown kind");

        assert!(
            !out.iter().any(|e| matches!(
                e,
                IngestEvent::Resource(r) if r.uid.as_ref() == "wl-vmi"
            )),
            "an object added and removed in one tick should not surface"
        );
        assert_eq!(i.stats().elided, 1);

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
        let IngestEvent::Resource(r) = &out[0] else {
            panic!("expected a resource event")
        };
        let Payload::Instance { state } = r.payload else {
            panic!("expected an instance payload")
        };
        assert_eq!(state, State::of(crate::model::ReasonId::UNKNOWN));
    }

    #[test]
    fn a_bookmarked_reconnect_relists_nothing() {
        let stream = bookmarked_reconnect();
        assert!(
            find_desync(&stream.events).is_empty(),
            "a resumed watch has nothing to declare desynced"
        );

        let mut adds: Vec<&str> = stream
            .resources()
            .filter(|r| r.op == Op::Added)
            .map(|r| &*r.uid)
            .collect();
        let before = adds.len();
        adds.sort_unstable();
        adds.dedup();
        assert_eq!(before, adds.len(), "no object may be added twice");

        let versions: Vec<u64> = stream
            .resources()
            .filter(|r| r.op != Op::Added)
            .map(|r| r.resource_version)
            .collect();
        assert_eq!(versions, vec![4_096, 4_097]);

        let mut i = Intake::new();
        let out = stream.drain_through(&mut i);
        assert_eq!(res_count(&out), 3, "one namespace, one owner, one pod");
        assert!(
            out.iter().any(|e| matches!(
                e,
                IngestEvent::Resource(r) if r.uid.as_ref() == "pod-1" && r.resource_version == 4_096
            )),
            "the surviving event must be the later one"
        );
    }

    #[test]
    fn an_unknown_kind_and_reason_keep_the_severity_they_arrived_with() {
        let mut i = Intake::new();
        let out = unknown_kind_and_reason().drain_through(&mut i);
        assert_eq!(res_count(&out), 3);

        let instance = out
            .iter()
            .find_map(|e| match e {
                IngestEvent::Resource(r) if matches!(r.payload, Payload::Instance { .. }) => {
                    Some(r)
                }
                _ => None,
            })
            .expect("the instance arrived");
        let Payload::Instance { state } = instance.payload else {
            panic!("expected an instance payload")
        };
        assert_eq!(state.severity, crate::model::Severity::Err);
        assert!(
            state.reason.0 >= crate::model::BUILTIN_REASON_COUNT,
            "the fixture has to use a reason past the compiled-in table"
        );
        assert_eq!(
            crate::model::reason_severity(state.reason),
            crate::model::Severity::Unknown,
            "the static table knows nothing about it, which is why State::of would be wrong"
        );

        let owner = out
            .iter()
            .find_map(|e| match e {
                IngestEvent::Resource(r) if matches!(r.payload, Payload::Owner { .. }) => Some(r),
                _ => None,
            })
            .expect("the owner arrived");
        assert!(!owner.kind.is_builtin());
        assert_eq!(crate::model::kind_short(owner.kind), "?");
    }

    #[test]
    fn replaying_the_same_stream_twice_gives_the_same_result() {
        let stream = resync_after_expired();
        let mut a = Intake::new();
        let mut b = Intake::new();
        assert_eq!(stream.drain_through(&mut a), stream.drain_through(&mut b));
    }
}
