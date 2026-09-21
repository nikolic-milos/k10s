use super::*;
use k10s_core::{IngestEvent, KindId, Op, State, replay};
use k10s_world::{LayoutMode, PublishBench};

fn pod(uid: &str, namespace: &str, owner: &str, name: &str, op: Op) -> IngestEvent {
    let IngestEvent::Resource(mut event) = replay::instance(uid, namespace, owner, State::OK, op)
    else {
        unreachable!();
    };
    event.name = name.into();
    IngestEvent::Resource(event)
}

#[test]
fn alert_labels_join_both_namespace_and_pod_and_resolve_the_current_uid() {
    let events = [
        replay::scope("ns-prod", "prod", Op::Added),
        replay::scope("ns-staging", "staging", Op::Added),
        replay::owner("wl-prod", "prod", "api", KindId::DEPLOYMENT, Op::Added),
        replay::owner(
            "wl-staging",
            "staging",
            "api",
            KindId::DEPLOYMENT,
            Op::Added,
        ),
        pod("prod-pod", "prod", "wl-prod", "api-1", Op::Added),
        pod("staging-pod", "staging", "wl-staging", "api-1", Op::Added),
    ];
    let mut world = PublishBench::new(&events, LayoutMode::Spread);
    let before = world.snapshot();
    assert_eq!(
        pod_uid(&before, "prod", "api-1").as_deref(),
        Some("prod-pod")
    );
    assert_eq!(
        pod_uid(&before, "staging", "api-1").as_deref(),
        Some("staging-pod")
    );
    for (namespace, pod) in [
        ("", "api-1"),
        ("prod", ""),
        ("other", "api-1"),
        ("prod", "api"),
        ("prod", "api-1.*"),
    ] {
        assert_eq!(pod_uid(&before, namespace, pod), None);
    }
    world.apply_events(&[pod("prod-pod", "prod", "wl-prod", "api-1", Op::Deleted)]);
    world.run_publish();
    assert_eq!(
        pod_uid(&world.snapshot(), "prod", "api-1"),
        None,
        "a deleted pod is not a target"
    );
    world.apply_events(&[pod("replacement", "prod", "wl-prod", "api-1", Op::Added)]);
    world.run_publish();
    assert_eq!(
        pod_uid(&world.snapshot(), "prod", "api-1").as_deref(),
        Some("replacement")
    );
    assert_eq!(
        pod_uid(&before, "prod", "api-1").as_deref(),
        Some("prod-pod"),
        "the old snapshot stays isolated"
    );
}

fn selection() -> Selection {
    Selection {
        level: Level::Cell,
        kind: "pod",
        kind_id: KindId::POD,
        uid: "uid-pod".into(),
        name: "api.v2-7d4f".into(),
        namespace: Some("prod-eu".into()),
        owner: Some("api".into()),
    }
}

#[test]
fn a_pod_pick_keeps_exact_equality_and_never_expands_to_its_owner() {
    assert_eq!(
        matchers(&selection()).expect("a pod"),
        vec![
            AlertMatcher {
                name: "namespace".into(),
                value: "prod-eu".into(),
                is_regex: false,
                is_equal: true
            },
            AlertMatcher {
                name: "pod".into(),
                value: "api.v2-7d4f".into(),
                is_regex: false,
                is_equal: true
            },
        ]
    );
    let namespace = Selection {
        level: Level::Region,
        kind: "namespace",
        kind_id: KindId::NAMESPACE,
        name: "prod-eu".into(),
        namespace: None,
        owner: None,
        ..selection()
    };
    assert_eq!(
        matchers(&namespace).expect("a namespace"),
        vec![AlertMatcher {
            name: "namespace".into(),
            value: "prod-eu".into(),
            is_regex: false,
            is_equal: true
        },]
    );
    for level in [Level::Block, Level::Sat] {
        assert!(
            matchers(&Selection {
                level,
                ..selection()
            })
            .is_err(),
            "an owner must not silently widen a silence to its namespace"
        );
    }
    assert!(
        matchers(&Selection {
            namespace: None,
            ..selection()
        })
        .is_err()
    );
}

fn state() -> SilenceState {
    let mut state = SilenceState::new(
        AlertmanagerEndpoint {
            namespace: "monitoring".into(),
            service: "reviewed".into(),
            port: 9093,
        },
        matchers(&selection()).expect("matchers"),
    );
    assert!(state.comment.is_empty());
    state.edit("Investigating this pod.".into());
    state
}

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_789_182_000)
}
fn reviewed() -> SilenceOutcome {
    SilenceOutcome::NeedsConfirm {
        starts_at: "2026-09-12T03:00:00Z".into(),
        ends_at: "2026-09-12T04:00:00Z".into(),
    }
}

#[test]
fn confirmation_uses_the_reviewed_endpoint_matchers_reason_and_interval_once() {
    let mut state = state();
    state.edit("Investigating this pod.".into());
    let (preview, confirm) = state.submit(now()).expect("review");
    assert!(!confirm);
    assert_eq!(preview.endpoint.service, "reviewed");
    assert_eq!(preview.matchers, matchers(&selection()).expect("matchers"));
    assert_eq!(preview.window, now()..now() + Duration::from_secs(3600));
    assert!(
        state.submit(now()).is_none(),
        "Enter while reviewing cannot confirm unseen work"
    );
    state.edit("too late".into());
    state.adopt(reviewed());
    let (write, confirm) = state
        .submit(now() + Duration::from_secs(60))
        .expect("confirmation");
    assert!(confirm);
    assert_eq!(
        write, preview,
        "the wall clock and an in-flight edit cannot change the reviewed request"
    );
    assert!(
        state.submit(now()).is_none(),
        "a second Enter while writing cannot duplicate the POST"
    );
    state.adopt(SilenceOutcome::Applied {
        id: "silence-id".into(),
    });
    assert_eq!(state.status, "created silence silence-id");
    assert!(
        state.submit(now()).is_none(),
        "success never arms another POST"
    );
}

#[test]
fn editing_cancelling_or_expiring_the_review_requires_a_new_preview() {
    for change in ["edit", "cancel", "expire"] {
        let mut state = state();
        state.submit(now()).expect("review");
        state.adopt(reviewed());
        match change {
            "edit" => state.edit("a different reason".into()),
            "cancel" => state.cancel_review(),
            "expire" => assert!(state.submit(now() + Duration::from_secs(3600)).is_none()),
            _ => unreachable!(),
        }
        assert_eq!(state.phase, Phase::Editing);
        assert!(state.request.is_none());
        assert!(state.interval.is_none());
        let (_, confirm) = state
            .submit(now() + Duration::from_secs(3601))
            .expect("a new preview");
        assert!(!confirm);
    }
}

#[test]
fn denial_failure_and_a_missing_reason_cannot_become_a_confirmed_write() {
    let mut empty = state();
    empty.edit(" ".into());
    assert!(empty.submit(now()).is_none());
    assert_eq!(empty.status, "a silence needs a reason");
    for (outcome, sentence) in [
        (
            SilenceOutcome::Denied {
                what: "alertmanager",
                why: "access denied for this account".into(),
            },
            "alertmanager: access denied for this account",
        ),
        (
            SilenceOutcome::Failed("silence store is unavailable".into()),
            "silence store is unavailable",
        ),
    ] {
        let mut state = state();
        state.submit(now()).expect("review");
        state.adopt(outcome);
        assert_eq!(state.status, sentence);
        assert!(state.submit(now()).is_none());
    }
}

#[test]
fn a_reason_at_the_limit_is_reviewed_whole_and_an_overlong_edit_disarms_it() {
    let mut state = state();
    state.edit(String::new());
    let reason = "é".repeat(1024);
    state.append_reason(&reason);
    let (request, confirm) = state.submit(now()).expect("review the whole reason");
    assert!(!confirm);
    assert_eq!(request.comment, reason);
    state.adopt(reviewed());
    state.append_reason("x");
    assert_eq!(state.comment, reason);
    assert_eq!(state.phase, Phase::Editing);
    assert!(state.request.is_none());
    assert_eq!(
        state.status,
        "the reason exceeds 1024 characters; that input was not added"
    );
    state.edit(String::new());
    state.append_reason("  scheduled pod maintenance  ");
    let (request, confirm) = state.submit(now()).expect("review the replacement");
    assert!(!confirm);
    assert_eq!(request.comment, "scheduled pod maintenance");
    assert_eq!(state.comment, request.comment);
}
