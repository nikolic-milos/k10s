use gpui::SharedString;

use super::*;

fn armed(subject: &str) -> Latch<u32> {
    let mut latch = Latch::Idle;
    latch.arm(
        subject.to_string(),
        7,
        "scale to 0 in context lab  (enter again to confirm)".into(),
        "scale to 0 in context lab: sent, waiting for the server".into(),
    );
    latch
}

fn sent_line() -> SharedString {
    "scale to 0 in context lab: sent, waiting for the server".into()
}

// The defect: ten presses during a held write sent ten writes. One arm is one
// write, however many presses follow it.
#[test]
fn a_burst_of_presses_after_one_arm_sends_exactly_one_write() {
    let mut latch = armed("deploy/api/scale to 0");
    assert_eq!(latch.press("deploy/api/scale to 0"), Press::Go(7));
    for _ in 0..9 {
        assert_eq!(latch.press("deploy/api/scale to 0"), Press::Refused);
    }
    assert_eq!(
        latch,
        Latch::Sent {
            subject: "deploy/api/scale to 0".to_string(),
            line: sent_line(),
        }
    );
}

#[test]
fn a_press_on_another_subject_asks_and_leaves_nothing_armed() {
    let mut latch = armed("deploy/api/scale to 0");
    assert_eq!(latch.press("deploy/api/restart"), Press::Ask);
    assert_eq!(latch, Latch::Idle);
    assert_eq!(latch.press("deploy/api/scale to 0"), Press::Ask);
}

#[test]
fn moving_off_the_row_never_releases_a_write_that_is_out() {
    let mut latch = armed("node/n1/drain");
    assert_eq!(latch.press("node/n1/drain"), Press::Go(7));
    latch.disarm();
    assert!(matches!(latch, Latch::Sent { .. }), "{latch:?}");
    latch.arm("node/n1/drain".to_string(), 8, "a".into(), "b".into());
    assert!(
        matches!(latch, Latch::Sent { .. }),
        "an ask answering while the write is out arms nothing: {latch:?}"
    );
    assert_eq!(latch.press("node/n2/cordon"), Press::Refused);
    assert!(matches!(latch, Latch::Sent { .. }), "{latch:?}");
}

#[test]
fn an_answer_on_its_own_row_holds_until_the_person_moves_off_it() {
    let mut latch = armed("node/n1/drain");
    assert_eq!(latch.press("node/n1/drain"), Press::Go(7));
    latch.settle(Some("node/n1/drain"));
    assert_eq!(
        latch,
        Latch::Answered {
            subject: "node/n1/drain".to_string()
        }
    );
    assert_eq!(latch.press("node/n1/drain"), Press::Refused);
    latch.arm("node/n1/drain".to_string(), 8, "a".into(), "b".into());
    assert_eq!(
        latch.press("node/n1/drain"),
        Press::Refused,
        "an arm after the answer is not taken"
    );
    latch.disarm();
    assert_eq!(latch, Latch::Idle);
    assert_eq!(latch.press("node/n1/drain"), Press::Ask);
}

#[test]
fn an_answer_landing_after_the_person_moved_leaves_nothing_held() {
    let mut latch = armed("node/n1/drain");
    assert_eq!(latch.press("node/n1/drain"), Press::Go(7));
    latch.settle(Some("node/n1/cordon"));
    assert_eq!(latch, Latch::Idle);
    let mut latch = armed("node/n1/drain");
    assert_eq!(latch.press("node/n1/drain"), Press::Go(7));
    latch.settle(None);
    assert_eq!(latch, Latch::Idle);
}

#[test]
fn a_press_on_another_subject_after_an_answer_asks() {
    let mut latch = armed("node/n1/drain");
    assert_eq!(latch.press("node/n1/drain"), Press::Go(7));
    latch.settle(Some("node/n1/drain"));
    assert_eq!(latch.press("node/n1/cordon"), Press::Ask);
    assert_eq!(latch, Latch::Idle);
}

#[test]
fn a_disarmed_arm_asks_again() {
    let mut latch = armed("deploy/api/scale to 0");
    latch.disarm();
    assert_eq!(latch, Latch::Idle);
    assert_eq!(latch.press("deploy/api/scale to 0"), Press::Ask);
}

fn sent(subject: &str) -> Latch<u32> {
    Latch::Sent {
        subject: subject.to_string(),
        line: sent_line(),
    }
}

fn answered(subject: &str) -> Latch<u32> {
    Latch::Answered {
        subject: subject.to_string(),
    }
}

// Every state under every operation: the state each leaves and what a press
// answers. `a` is the latch's subject and `b` another.
#[test]
fn every_transition_of_the_latch() {
    let pressed = |mut latch: Latch<u32>, subject: &str| {
        let press = latch.press(subject);
        (press, latch)
    };
    let after = |mut latch: Latch<u32>, op: fn(&mut Latch<u32>)| {
        op(&mut latch);
        latch
    };
    let armed8 = || {
        let mut latch = Latch::Idle;
        latch.arm("a".to_string(), 8, "armed".into(), "sent".into());
        latch
    };

    assert_eq!(pressed(Latch::Idle, "a"), (Press::Ask, Latch::Idle));
    assert_eq!(pressed(armed("a"), "a"), (Press::Go(7), sent("a")));
    assert_eq!(pressed(sent("a"), "a"), (Press::Refused, sent("a")));
    assert_eq!(pressed(answered("a"), "a"), (Press::Refused, answered("a")));

    assert_eq!(pressed(Latch::Idle, "b"), (Press::Ask, Latch::Idle));
    assert_eq!(pressed(armed("a"), "b"), (Press::Ask, Latch::Idle));
    assert_eq!(pressed(sent("a"), "b"), (Press::Refused, sent("a")));
    assert_eq!(pressed(answered("a"), "b"), (Press::Ask, Latch::Idle));

    let arm = |latch: &mut Latch<u32>| latch.arm("a".to_string(), 8, "armed".into(), "sent".into());
    assert_eq!(after(Latch::Idle, arm), armed8());
    assert_eq!(after(armed("a"), arm), armed("a"));
    assert_eq!(after(sent("a"), arm), sent("a"));
    assert_eq!(after(answered("a"), arm), answered("a"));

    let resume = |latch: &mut Latch<u32>| latch.resume("a".to_string(), sent_line());
    assert_eq!(after(Latch::Idle, resume), sent("a"));
    assert_eq!(after(armed("a"), resume), armed("a"));
    assert_eq!(after(sent("b"), resume), sent("b"));
    assert_eq!(after(answered("a"), resume), answered("a"));

    let disarm = |latch: &mut Latch<u32>| latch.disarm();
    assert_eq!(after(Latch::Idle, disarm), Latch::Idle);
    assert_eq!(after(armed("a"), disarm), Latch::Idle);
    assert_eq!(after(sent("a"), disarm), sent("a"));
    assert_eq!(after(answered("a"), disarm), Latch::Idle);

    let on_a = |latch: &mut Latch<u32>| latch.settle(Some("a"));
    assert_eq!(after(Latch::Idle, on_a), Latch::Idle);
    assert_eq!(after(armed("a"), on_a), armed("a"));
    assert_eq!(after(sent("a"), on_a), answered("a"));
    assert_eq!(after(answered("a"), on_a), answered("a"));

    let on_b = |latch: &mut Latch<u32>| latch.settle(Some("b"));
    assert_eq!(after(Latch::Idle, on_b), Latch::Idle);
    assert_eq!(after(armed("a"), on_b), armed("a"));
    assert_eq!(after(sent("a"), on_b), Latch::Idle);
    assert_eq!(after(answered("a"), on_b), answered("a"));

    let on_none = |latch: &mut Latch<u32>| latch.settle(None);
    assert_eq!(after(Latch::Idle, on_none), Latch::Idle);
    assert_eq!(after(armed("a"), on_none), armed("a"));
    assert_eq!(after(sent("a"), on_none), Latch::Idle);
    assert_eq!(after(answered("a"), on_none), answered("a"));
}

#[test]
fn a_write_resumes_only_into_an_idle_view() {
    let mut latch: Latch<u32> = Latch::Idle;
    latch.resume("prod/web/3".to_string(), "applying web revision 3".into());
    assert_eq!(latch.press("prod/web/3"), Press::Refused);
    let mut latch = armed("prod/web/3");
    latch.resume("prod/web/3".to_string(), "applying web revision 3".into());
    assert!(matches!(latch, Latch::Armed { .. }), "{latch:?}");
}

#[test]
fn the_hint_is_the_way_to_ask_again() {
    assert_eq!(ASK_AGAIN, "(move off this row and back to ask again)");
}

// Every state with and without a message and a label. Armed and Sent show
// their line with the message as a note under it; Answered shows the answer
// and the hint; Idle shows the message; the label always shows.
#[test]
fn the_slot_shows_each_state_and_never_hides_a_message_or_a_label() {
    let status: SharedString = "has no previous revision to diff".into();
    let label: SharedString = "deployments: access denied for this account".into();
    let armed_line: SharedString = "scale to 0 in context lab  (enter again to confirm)".into();
    let slot = |line: Option<&SharedString>,
                hint: Option<&'static str>,
                note: Option<&SharedString>,
                label: Option<&SharedString>| Slot {
        line: line.cloned(),
        hint,
        note: note.cloned(),
        label: label.cloned(),
    };
    for status in [None, Some(&status)] {
        for label in [None, Some(&label)] {
            assert_eq!(
                Slot::of(&Latch::<u32>::Idle, status, label),
                slot(status, None, None, label)
            );
            assert_eq!(
                Slot::of(&armed("a"), status, label),
                slot(Some(&armed_line), None, status, label)
            );
            assert_eq!(
                Slot::of(&sent("a"), status, label),
                slot(Some(&sent_line()), None, status, label)
            );
            assert_eq!(
                Slot::of(&answered("a"), status, label),
                slot(status, Some(ASK_AGAIN), None, label)
            );
        }
    }
}

// A slot with anything in it is drawn: a denied listing's label alone must not
// leave an empty pane that reads as a quiet cluster.
#[test]
fn a_slot_is_empty_only_with_nothing_in_it() {
    let said: SharedString = "said".into();
    let empty = || Slot {
        line: None,
        hint: None,
        note: None,
        label: None,
    };
    assert!(empty().is_empty());
    for slot in [
        Slot {
            line: Some(said.clone()),
            ..empty()
        },
        Slot {
            hint: Some(ASK_AGAIN),
            ..empty()
        },
        Slot {
            note: Some(said.clone()),
            ..empty()
        },
        Slot {
            label: Some(said),
            ..empty()
        },
    ] {
        assert!(!slot.is_empty(), "{slot:?}");
    }
}

#[test]
fn status_text_keeps_the_listing_label_before_a_sent_write_and_new_notes() {
    let mut text = StatusText::default();
    let latch = sent("a");
    let mut label: SharedString = "deployments: access denied for this account".into();
    let mut note: SharedString = "has no previous revision to diff".into();
    assert_eq!(
        text.text(Slot::of(&latch, Some(&note), Some(&label)))
            .as_deref(),
        Some(
            "deployments: access denied for this account\nscale to 0 in context lab: sent, waiting for the server\nhas no previous revision to diff"
        )
    );
    note = "listing refreshed".into();
    assert_eq!(
        text.text(Slot::of(&latch, Some(&note), Some(&label)))
            .as_deref(),
        Some(
            "deployments: access denied for this account\nscale to 0 in context lab: sent, waiting for the server\nlisting refreshed"
        )
    );
    label = "deployments: listing is partial".into();
    assert_eq!(
        text.text(Slot::of(&latch, Some(&note), Some(&label)))
            .as_deref(),
        Some(
            "deployments: listing is partial\nscale to 0 in context lab: sent, waiting for the server\nlisting refreshed"
        )
    );
    assert_eq!(
        text.text(Slot::of(&latch, None, None)).as_deref(),
        Some(sent_line().as_ref())
    );
}

#[test]
fn status_text_keeps_the_answer_and_hint_until_the_latch_is_disarmed() {
    let mut text = StatusText::default();
    let mut latch = sent("a");
    text.text(Slot::of(&latch, None, None));
    latch.settle(Some("a"));
    let answer: SharedString = "Forbidden: this account cannot scale deployments.".into();
    let expected = format!("{answer}\n{ASK_AGAIN}");
    for _ in 0..2 {
        assert_eq!(
            text.text(Slot::of(&latch, Some(&answer), None)).as_deref(),
            Some(expected.as_str())
        );
    }
    assert_eq!(
        text.text(Slot::of(&latch, None, None)).as_deref(),
        Some(ASK_AGAIN)
    );
    latch.disarm();
    assert_eq!(
        text.text(Slot::of(&latch, Some(&answer), None)).as_deref(),
        Some(answer.as_ref())
    );
}

#[test]
fn status_text_clears_old_text_and_keeps_present_empty_lines() {
    let mut text = StatusText::default();
    let latch = Latch::<u32>::Idle;
    let answer: SharedString = "scaled".into();
    assert_eq!(
        text.text(Slot::of(&latch, Some(&answer), None)).as_deref(),
        Some("scaled")
    );
    assert_eq!(text.text(Slot::of(&latch, None, None)), None);
    assert_eq!(text.text(Slot::of(&latch, None, None)), None);
    let empty: SharedString = "".into();
    assert_eq!(
        text.text(Slot::of(&latch, Some(&empty), None)).as_deref(),
        Some("")
    );
    let label: SharedString = "deployments: listing is partial".into();
    assert_eq!(
        text.text(Slot::of(&latch, Some(&empty), Some(&label)))
            .as_deref(),
        Some("deployments: listing is partial\n")
    );
}

#[test]
fn a_listing_is_empty_only_with_no_rows_no_message_and_no_label() {
    let said: SharedString = "said".into();
    assert!(nothing_listed(0, None, None));
    assert!(!nothing_listed(1, None, None));
    assert!(!nothing_listed(0, Some(&said), None));
    assert!(!nothing_listed(0, None, Some(&said)));
}

#[test]
fn a_write_names_the_context_it_lands_in() {
    assert_eq!(write_target(None), "this cluster's in-cluster account");
    assert_eq!(write_target(Some("lab")), "context lab");
    assert_eq!(write_target(Some("")), "context \"\"");
}
