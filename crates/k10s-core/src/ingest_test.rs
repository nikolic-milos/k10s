//! Coalescing and its bounds: a flapping object costs one event per tick, a
//! delete is never downgraded by a late modify, first-touch order survives
//! compaction, overflow asks for a resync instead of growing, and the one
//! verdict retrying cannot fix outlives the bound that buried it.

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
    let mut i = Intake::new();
    i.push(res("ghost", Op::Added, 1));
    i.push(res("ghost", Op::Deleted, 2));
    assert!(i.is_empty(), "elided object still pending");
    assert!(i.drain().is_empty());
    assert_eq!(i.stats().elided, 1);

    let mut i = Intake::new();
    i.push(res("real", Op::Deleted, 9));
    assert_eq!(uids(&i.drain()), vec![("real".into(), Op::Deleted, 9)]);
    assert_eq!(i.stats().elided, 0);
}

#[test]
fn an_object_that_flaps_before_it_dies_is_still_elided() {
    let mut i = Intake::new();
    i.push(res("ghost", Op::Added, 1));
    i.push(res("ghost", Op::Modified, 2));
    i.push(res("ghost", Op::Modified, 3));
    i.push(res("ghost", Op::Deleted, 4));
    assert!(
        i.is_empty(),
        "an object born and buried inside one tick was never published, so its \
         tombstone has nobody to tell"
    );
    assert!(i.drain().is_empty());
    assert_eq!(i.stats().elided, 1);
}

#[test]
fn an_elide_storm_does_not_ratchet_the_arena() {
    let mut i = Intake::with_capacity(8);
    i.push(res("anchor", Op::Added, 1));
    for rv in 0..250_000 {
        i.push(res("ghost-x", Op::Added, rv * 4));
        i.push(res("ghost-y", Op::Added, rv * 4 + 1));
        i.push(res("ghost-x", Op::Deleted, rv * 4 + 2));
        i.push(res("ghost-y", Op::Deleted, rv * 4 + 3));
    }
    assert_eq!(i.stats().elided, 500_000);
    assert_eq!(
        i.stats().dropped,
        0,
        "three live uids never exceed capacity"
    );
    assert!(
        i.pending.capacity() <= 2 * 8,
        "arena ratcheted past twice the live set, to {} slots of {} bytes",
        i.pending.capacity(),
        size_of::<Option<ResourceEvent>>()
    );
    assert_eq!(
        uids(&i.drain()),
        vec![("anchor".into(), Op::Added, 1)],
        "the storm's only survivor"
    );
}

#[test]
fn compacting_the_arena_keeps_first_touch_order() {
    let mut i = Intake::with_capacity(8);
    i.push(res("ghost", Op::Added, 1));
    i.push(res("first", Op::Added, 1));
    i.push(res("ghost", Op::Deleted, 2));
    i.push(res("last", Op::Added, 1));
    assert_eq!(i.pending.len(), 2, "the ghost's slot is gone, not vacant");
    i.push(res("first", Op::Modified, 7));
    assert_eq!(
        uids(&i.drain()),
        vec![
            ("first".into(), Op::Modified, 7),
            ("last".into(), Op::Added, 1)
        ]
    );
}

#[test]
fn an_elided_slot_never_moves_the_uids_around_it() {
    let mut i = Intake::new();
    for uid in ["a", "ghost-mid", "b", "ghost-tail"] {
        i.push(res(uid, Op::Added, 1));
    }
    i.push(res("ghost-mid", Op::Deleted, 2));
    i.push(res("ghost-tail", Op::Deleted, 2));
    i.push(res("c", Op::Added, 1));
    i.push(res("b", Op::Modified, 7));
    assert_eq!(i.stats().elided, 2);
    assert_eq!(
        uids(&i.drain()),
        vec![
            ("a".into(), Op::Added, 1),
            ("b".into(), Op::Modified, 7),
            ("c".into(), Op::Added, 1),
        ]
    );
}

#[test]
fn a_delete_after_a_readd_is_not_elided() {
    let mut i = Intake::new();
    i.push(res("pod-a", Op::Deleted, 1));
    i.push(res("pod-a", Op::Added, 2));
    i.push(res("pod-a", Op::Deleted, 3));
    assert_eq!(uids(&i.drain()), vec![("pod-a".into(), Op::Deleted, 3)]);
    assert_eq!(i.stats().elided, 0);
}

#[test]
fn a_delete_is_not_downgraded_by_a_late_modify() {
    let mut i = Intake::new();
    i.push(res("pod-a", Op::Modified, 1));
    i.push(res("pod-a", Op::Deleted, 2));
    i.push(res("pod-a", Op::Modified, 3));
    assert_eq!(uids(&i.drain()), vec![("pod-a".into(), Op::Deleted, 2)]);
}

#[test]
fn an_update_thrown_away_after_a_delete_is_not_a_coalesce() {
    let mut i = Intake::new();
    i.push(res("pod-a", Op::Modified, 1));
    i.push(res("pod-a", Op::Deleted, 2));
    i.push(res("pod-a", Op::Modified, 3));
    let s = i.stats();
    assert_eq!(s.coalesced, 1, "only the Deleted replaced anything");
    assert_eq!(s.superseded, 1);
}

#[test]
fn a_uid_reused_after_delete_is_added_again() {
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
fn a_control_storm_hits_the_control_bound_and_says_so() {
    let mut i = Intake::new();
    for _ in 0..CONTROL_CAPACITY {
        i.push(IngestEvent::Desync {
            kind: KindId::POD,
            reason: DesyncReason::Closed,
        });
        i.push(IngestEvent::Synced { kind: KindId::POD });
    }
    assert!(
        i.control.len() <= CONTROL_CAPACITY,
        "control grew to {}",
        i.control.len()
    );
    assert!(i.stats().dropped > 0, "the bound has to be counted");

    let out = i.drain();
    assert!(
        out.iter().any(|e| matches!(
            e,
            IngestEvent::Desync {
                reason: DesyncReason::Overflow,
                ..
            }
        )),
        "a bound nobody is told about is a stall"
    );
}

#[test]
fn a_forbidden_verdict_outlives_the_control_storm_that_buried_it() {
    let mut i = Intake::new();
    for _ in 0..CONTROL_CAPACITY {
        i.push(IngestEvent::Synced { kind: KindId::POD });
    }
    i.push(IngestEvent::Desync {
        kind: KindId::SECRET,
        reason: DesyncReason::Forbidden,
    });

    let out = i.drain();
    let desyncs: Vec<(usize, KindId, DesyncReason)> = out
        .iter()
        .enumerate()
        .filter_map(|(at, e)| match e {
            IngestEvent::Desync { kind, reason } => Some((at, *kind, *reason)),
            _ => None,
        })
        .collect();
    let forbidden = desyncs
        .iter()
        .find(|(_, kind, reason)| *kind == KindId::SECRET && *reason == DesyncReason::Forbidden)
        .expect("a 403 dropped by the control bound looks like a retryable stall");
    let overflow = desyncs
        .iter()
        .find(|(_, _, reason)| *reason == DesyncReason::Overflow)
        .expect("the bound still has to ask for a resync");
    assert!(
        overflow.0 < forbidden.0,
        "the last word on a kind must be the verdict retrying cannot fix"
    );
    assert!(i.is_empty());
}

#[test]
fn distinct_overflows_are_bounded_and_drain_deterministically() {
    let mut i = Intake::with_capacity(0);
    for n in (0..CONTROL_CAPACITY * 3).rev() {
        let mut event = match res("full", Op::Added, n as u64) {
            IngestEvent::Resource(event) => event,
            _ => unreachable!(),
        };
        event.kind = KindId(n as u32);
        i.push(IngestEvent::Resource(event));
    }
    assert_eq!(i.overflowed.len(), CONTROL_CAPACITY);
    assert!(i.control.is_empty());

    let drained = i.drain();
    assert_eq!(drained.len(), CONTROL_CAPACITY);
    let kinds: Vec<u32> = drained
        .into_iter()
        .map(|event| match event {
            IngestEvent::Desync {
                kind,
                reason: DesyncReason::Overflow,
            } => kind.0,
            other => panic!("unexpected event {other:?}"),
        })
        .collect();
    assert!(kinds.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(i.is_empty());
}

#[test]
fn an_object_already_pending_still_coalesces_when_full() {
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
