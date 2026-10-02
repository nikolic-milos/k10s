//! What the diff view claims, checked without a window.
//!
//! The two things worth testing here are the classification -- an edit, a
//! change in the read copy, or a collision -- and the preconditions a *press*
//! has to clear. The second is why [`crate::diff_gate::prepare`] is one pure
//! function: the two preconditions that ever failed were the two that lived
//! somewhere else, guarding one way in while another way round stayed open.

use k10s_edit::diff::{self, Origin, Verdict};

use crate::diff::*;
use crate::diff_gate::{
    ApplyGate, Armed, Flight, Identity, Keepable, Preparation, Ready, Review, Reviewed, Seen, Sent,
    Step, dry_run_left, gone_note, identity, kept_note, landed_note, may_still_land, prepare,
    raced_note, recreated_note, refuse_keep, reviewed, seen_after, stale_object_note,
};
use crate::editor::BufferStamp;
use k10s_provider::{ApplyOutcome, Conflicted, LiveCopy};

const LIVE: &str = concat!(
    "apiVersion: v1\n",
    "kind: Pod\n",
    "metadata:\n",
    "  name: web\n",
    "spec:\n",
    "  containers:\n",
    "    - image: nginx:1.27\n",
    "      name: web\n",
);

fn local(live: &str, base: Option<&str>, buffer: &str) -> DiffState {
    let mut state = DiffState::new();
    state.set_viewport(40);
    state.set(
        Mode::Local,
        live.to_string(),
        base.map(str::to_string),
        buffer.to_string(),
    );
    state
}

fn texts(state: &DiffState) -> Vec<String> {
    state.visible().map(|painted| painted.rendered()).collect()
}

#[test]
fn an_unchanged_buffer_folds_to_one_line_and_says_there_is_nothing_to_do() {
    let state = local(LIVE, None, LIVE);
    assert_eq!(state.diff().verdict(), Verdict::Agreed);
    assert_eq!(state.len(), 1, "every line folded into one");
    assert_eq!(texts(&state), vec!["  ... 8 unchanged"]);
    assert!(state.summary().contains("no differences"));
}

#[test]
fn unfolding_shows_every_line_of_the_live_document() {
    let mut state = local(LIVE, None, LIVE);
    state.toggle_folded();
    assert!(!state.folded());
    assert_eq!(state.len(), 8);
    assert_eq!(texts(&state)[0], " apiVersion: v1");
}

#[test]
fn a_change_carries_a_header_naming_who_made_it() {
    let buffer = LIVE.replace("nginx:1.27", "nginx:1.28");
    let state = local(LIVE, None, &buffer);
    let lines = texts(&state);
    assert!(
        lines.contains(&"  you changed this".to_string()),
        "{lines:?}"
    );
    assert!(lines.contains(&"-    - image: nginx:1.27".to_string()));
    assert!(lines.contains(&"+    - image: nginx:1.28".to_string()));
    assert!(state.summary().contains("+1 -1"));
    assert!(state.summary().contains("1 yours"));
}

// The status line's remarks are formatted when the comparison changes and
// stored. A fold is not such a change: it formats nothing, so the stored
// strings are the same allocations after it, and "folded" is the line's own
// borrowed piece.
#[test]
fn a_fold_formats_nothing_and_the_line_says_folded() {
    let buffer = LIVE.replace("nginx:1.27", "nginx:1.28");
    let mut state = local(LIVE, Some(LIVE), &buffer);
    let (summary, hints) = (state.summary().as_ptr(), state.hints().as_ptr());
    state.toggle_folded();
    assert_eq!(
        (state.summary().as_ptr(), state.hints().as_ptr()),
        (summary, hints)
    );
    assert!(!state.hints().contains("folded"), "{}", state.hints());
    let line = |folded| {
        status_line(Line {
            folded,
            ..line_at(&[], None)
        })
    };
    assert_eq!(line(true), "3 hunks  ·  folded");
    assert_eq!(line(false), "3 hunks");
}

#[test]
fn the_fold_note_requires_a_comparison_with_differences() {
    let mut state = local(LIVE, Some(LIVE), LIVE);
    assert!(state.folded());
    assert!(!state.folded_note());
    state.set(
        Mode::Local,
        LIVE.into(),
        Some(LIVE.into()),
        LIVE.replace("nginx:1.27", "nginx:1.28"),
    );
    assert!(state.folded_note());
    state.toggle_folded();
    assert!(!state.folded_note());
    assert!(!DiffState::new().folded_note());
}

// A state nobody has given a comparison says so, whichever way it was made.
#[test]
fn a_new_state_is_comparing() {
    assert_eq!(DiffState::new().summary(), "comparing...");
    assert_eq!(DiffState::default().summary(), "comparing...");
}

#[test]
fn a_two_way_comparison_says_it_has_no_base() {
    let buffer = LIVE.replace("nginx:1.27", "nginx:1.28");
    let state = local(LIVE, None, &buffer);
    assert!(
        state
            .hints()
            .contains("no last-applied-configuration, so this is two-way")
    );
}

#[test]
fn drift_in_the_copy_read_is_labelled_as_changed_since_the_last_apply() {
    let base = LIVE;
    let live = LIVE.replace("nginx:1.27", "nginx:1.29");
    let buffer = LIVE;
    let state = local(&live, Some(base), buffer);
    let lines = texts(&state);
    assert!(
        lines.contains(&"  changed since the last apply, in the copy this editor read".to_string()),
        "{lines:?}"
    );
    assert_eq!(
        state.summary(),
        "against the copy this editor read  ·  +1 -1  ·  1 changed since the last apply"
    );
    assert_eq!(
        state.hints(),
        "t takes a hunk from the copy this editor read"
    );
    assert!(!state.hints().contains("two-way"));
}

#[test]
fn a_conflict_shows_all_three_documents_in_order() {
    let base = "a\nvalue: one\nz\n";
    let live = "a\nvalue: two\nz\n";
    let buffer = "a\nvalue: three\nz\n";
    let state = local(live, Some(base), buffer);
    let lines = texts(&state);
    assert_eq!(
        lines,
        vec![
            " a",
            "  both changed this since the last apply",
            "-value: two",
            "|value: one",
            "+value: three",
            " z",
        ]
    );
    assert!(state.summary().contains("1 conflicting"));
}

#[test]
fn the_dry_run_mode_labels_the_change_as_the_servers_own_answer() {
    let mut state = DiffState::new();
    state.set_viewport(40);
    let would_be = LIVE.replace("nginx:1.27", "nginx:1.28");
    state.set(Mode::DryRun, LIVE.to_string(), None, would_be);
    let lines = texts(&state);
    assert!(
        lines.contains(&"  the apply would change this".to_string()),
        "{lines:?}"
    );
    assert!(state.summary().contains("against the server's dry run"));
    assert!(!state.hints().contains("two-way"));
}

#[test]
fn a_long_unchanged_run_between_two_changes_keeps_context_on_both_sides() {
    let mut live = String::from("head: 1\n");
    for at in 0..40 {
        live.push_str(&format!("filler-{at}: x\n"));
    }
    live.push_str("tail: 1\n");
    let buffer = live
        .replace("head: 1", "head: 2")
        .replace("tail: 1", "tail: 2");
    let state = local(&live, None, &buffer);
    let lines = texts(&state);
    assert!(
        lines.iter().any(|line| line.starts_with("  ... ")),
        "the middle folds: {lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with(" filler"))
            .count(),
        CONTEXT_LINES * 2,
        "three lines of context each side: {lines:?}"
    );
}

#[test]
fn navigation_steps_between_changes_and_stops_at_the_ends() {
    let mut live = String::new();
    for at in 0..30 {
        live.push_str(&format!("line-{at}\n"));
    }
    let buffer = live
        .replace("line-5\n", "line-five\n")
        .replace("line-20\n", "line-twenty\n");
    let mut state = local(&live, None, &buffer);
    state.set_viewport(4);
    assert_eq!(state.top(), 0);
    assert!(state.next_change(), "the first change is ahead");
    let first = state.top();
    assert!(state.next_change(), "so is the second");
    let second = state.top();
    assert!(second > first);
    assert!(!state.next_change(), "and nothing after it");
    assert!(state.prev_change());
    assert_eq!(state.top(), first);
    assert!(!state.prev_change());
}

#[test]
fn scrolling_clamps_at_both_ends() {
    let mut state = local(LIVE, None, LIVE);
    state.toggle_folded();
    state.set_viewport(3);
    state.scroll_by(-5);
    assert_eq!(state.top(), 0);
    state.scroll_by(500);
    assert_eq!(state.top(), state.len() - 1);
    state.home();
    assert_eq!(state.top(), 0);
    state.page_by(1);
    assert_eq!(state.top(), 2);
    state.end();
    assert_eq!(state.top(), state.len() - 1);
}

#[test]
fn an_oversized_side_is_reported_rather_than_diffed() {
    let big = "x".repeat(k10s_edit::diff::MAX_SIDE_BYTES + 1);
    let state = local(&big, None, "a\n");
    assert!(
        state.summary().contains("larger than the 8 MiB"),
        "{}",
        state.summary()
    );
    assert!(state.is_empty());
}

// What five independent reviewers found in the flag this replaced: a reply
// that no longer speaks for the current comparison must still hand back the
// wire, or nothing can ever be applied again.
#[test]
fn the_wire_is_released_by_its_holder_even_when_its_answer_is_no_longer_wanted() {
    let mut flight = Flight::default();
    let first = flight.take(false).expect("the wire is free");
    assert!(flight.busy());
    assert_eq!(flight.take(false), None, "one request at a time");

    // A superseded reply releasing by its own ticket is exactly the case the
    // flag got wrong.
    assert!(!flight.release(first), "nothing was queued behind it");
    assert!(!flight.busy(), "the wire is free again");

    let second = flight.take(false).expect("so a second apply can go out");
    assert_ne!(second, first, "tickets are not reused");
    flight.release(first);
    assert!(
        flight.busy(),
        "and a stale ticket cannot release someone else's request"
    );
    flight.release(second);
    assert!(!flight.busy());
}

// A real apply that does not land cleanly (a transport error, a dropped reply,
// a refusal) used to leave its review standing, so two more presses resent the
// same bytes with nothing asked of the server again. Taking the wire for a
// real apply spends the review, whatever answer follows.
#[test]
fn a_real_apply_spends_its_review_when_it_takes_the_wire() {
    let stamp = BufferStamp::of(1, 4);
    let review = Review {
        stamp,
        force: false,
    };
    let mut flight = Flight::default();
    flight.record_review(review);
    let ticket = flight.take(false).expect("the wire is free");
    assert_eq!(flight.reviewed(), None);
    flight.release(ticket);
    let mut at = ready(&[], Verdict::Differs);
    at.dry_run = flight.reviewed();
    assert_eq!(
        prepare(Armed::Apply, at),
        Err("the server has not been asked what this would store; ctrl-alt-r asks it".to_string()),
        "the next press asks for a new review"
    );

    flight.record_review(review);
    let ticket = flight.take(true).expect("free again");
    assert_eq!(flight.reviewed(), Some(review), "a dry run spends nothing");
    assert_eq!(flight.take(false), None, "one request at a time");
    assert_eq!(
        flight.reviewed(),
        Some(review),
        "and an apply refused the wire spends nothing either"
    );
    flight.release(ticket);
    at.dry_run = flight.reviewed();
    assert_eq!(prepare(Armed::Apply, at), Ok(Preparation::Confirm));
}

// A dry run asked for while the wire is held used to be dropped, under a
// message calling the holder "an apply" when it was another dry run: the
// tab then sat in the local comparison waiting for an answer nobody had
// asked for. A dry run changes nothing, so it is owed rather than lost.
#[test]
fn a_comparison_asked_for_while_the_wire_is_held_is_owed_and_the_holder_is_named() {
    let mut flight = Flight::default();
    let ticket = flight.take(true).expect("the wire is free");
    assert_eq!(flight.holder(), "a dry run");
    assert_eq!(flight.take(true), None);
    flight.owe_a_comparison();
    assert!(
        flight.release(ticket),
        "the comparison asked for meanwhile is owed once the wire frees"
    );
    assert!(!flight.busy());

    let ticket = flight.take(false).expect("free again");
    assert_eq!(flight.holder(), "an apply", "and an apply says so");
    assert!(
        !flight.release(ticket),
        "the owed comparison is owed exactly once"
    );
}

// The fold rebuilds the row list, so a cell index means something different
// afterwards; what has to survive is the document row being read.
#[test]
fn folding_keeps_what_was_on_screen_rather_than_jumping_to_the_top() {
    let mut live = String::new();
    for at in 0..60 {
        live.push_str(&format!("line-{at}\n"));
    }
    let buffer = live.replace("line-40\n", "line-forty\n");
    let mut state = local(&live, None, &buffer);
    state.set_viewport(6);
    state.toggle_folded();
    assert!(
        !state.folded(),
        "unfolded, every line of the document is a row"
    );

    // Reading the change, forty lines down: what the fold must not do is
    // take the reader back to the top of a sixty-line document.
    assert!(state.next_change());
    let unfolded_top = state.top();
    assert!(unfolded_top > 30, "the change is well down the document");
    let on_screen: Vec<String> = state
        .visible()
        .map(|painted| painted.text.to_string())
        .collect();
    assert!(
        on_screen.iter().any(|line| line == "line-forty"),
        "the change is on screen: {on_screen:?}"
    );

    state.toggle_folded();
    assert!(state.folded());
    let after: Vec<String> = state
        .visible()
        .map(|painted| painted.text.to_string())
        .collect();
    assert!(
        after.iter().any(|line| line == "line-forty"),
        "and it still is after folding: {after:?}"
    );
    assert!(
        after.iter().any(|line| line == "line-40"),
        "with the line it replaced: {after:?}"
    );
}

#[test]
fn folding_near_the_end_stays_near_the_end() {
    let mut live = String::new();
    for at in 0..60 {
        live.push_str(&format!("line-{at}\n"));
    }
    let buffer = live.replace("line-2\n", "line-two\n");
    let mut state = local(&live, None, &buffer);
    state.set_viewport(4);
    state.toggle_folded();
    state.end();
    assert!(state.top() > 50, "reading the tail of the document");
    state.toggle_folded();
    assert_eq!(
        state.top(),
        state.len() - 1,
        "the fold swallowed everything after the anchor, so the end is where \
         the reader stays"
    );
}

// The data plane caps the causes it carries and says so. A review that
// states a cut list as a total asks for consent to take fewer fields than
// the press would actually take.
#[test]
fn a_cut_conflict_list_authorises_a_force_as_a_floor_not_a_total() {
    assert_eq!(taken(1, false), "1 field");
    assert_eq!(taken(3, false), "3 fields");
    assert_eq!(taken(32, true), "at least 32 fields");
    assert_eq!(taken(1, true), "at least 1 field");
}

// Every precondition in one place, so that adding a way in cannot add a way
// round. The baseline is a press that must go through; each case below
// changes exactly one thing about it.
fn ready<'a>(blocked: &'a [&'static str], verdict: Verdict) -> Ready<'a> {
    let stamp = BufferStamp::of(1, 4);
    Ready {
        patchable: true,
        blocked,
        verdict,
        reviewed: stamp,
        editor: Some(stamp),
        dry_run: Some(Review { stamp, force: true }),
        conflicts: 1,
        in_flight: None,
        raced: false,
    }
}

#[test]
fn a_press_that_clears_every_precondition_is_the_only_one_that_writes() {
    assert_eq!(
        prepare(Armed::Apply, ready(&[], Verdict::Differs)),
        Ok(Preparation::Confirm)
    );
    assert_eq!(
        prepare(Armed::Force, ready(&[], Verdict::Differs)),
        Ok(Preparation::Confirm)
    );
    assert_eq!(
        prepare(Armed::Apply, ready(&[], Verdict::Agreed)),
        Ok(Preparation::Confirm),
        "an apply that changes nothing is still an apply the user may make"
    );
}

// A document over the diff's ceiling paints no rows and counts nothing, so
// the review the press is confirming does not exist. It used to reach the
// cluster under a footer saying applying would change nothing.
#[test]
fn a_comparison_the_diff_refused_to_make_is_not_a_comparison_that_agreed() {
    let refused = Verdict::Refused("one side of this comparison has more than 65,536 lines");
    assert_eq!(
        prepare(Armed::Apply, ready(&[], refused)),
        Err("nothing here has been reviewed, so there is nothing to apply".to_string()),
    );
    assert_eq!(
        prepare(Armed::Force, ready(&[], refused)),
        Err("nothing here has been reviewed, so there is nothing to apply".to_string()),
    );
}

// The bytes of a blocked payload are the document unpruned -- every field
// the server owns still in it. Nothing read `blocked`, so they went out.
#[test]
fn a_payload_the_pruner_refused_never_becomes_a_request() {
    // The baseline press goes through, so the one thing that changed here
    // is the one thing that stopped it. The reasons themselves stand on
    // their own piece of the status line rather than being repeated.
    assert_eq!(
        prepare(Armed::Apply, ready(&[], Verdict::Differs)),
        Ok(Preparation::Confirm)
    );
    assert_eq!(
        prepare(
            Armed::Apply,
            ready(
                &["a cluster apply needs exactly one YAML document"],
                Verdict::Differs,
            ),
        ),
        Err("this document cannot be applied, so nothing was sent".to_string()),
    );
}

// `Buffer::new` restarts versions at zero, so a reload and the same number
// of keystrokes puts a different document on the same number. The review
// was of the first one.
#[test]
fn a_buffer_replaced_since_the_review_is_refused_even_at_the_same_version() {
    let mut at = ready(&[], Verdict::Differs);
    at.reviewed = BufferStamp::of(1, 3);
    at.editor = Some(BufferStamp::of(2, 3));
    at.dry_run = Some(Review {
        stamp: BufferStamp::of(1, 3),
        force: false,
    });
    let why = prepare(Armed::Apply, at).expect_err("refused");
    assert!(
        why.contains("the buffer changed after this comparison"),
        "{why}"
    );

    at.editor = None;
    let why = prepare(Armed::Apply, at).expect_err("refused");
    assert!(
        why.contains("the editor this diff came from is gone"),
        "{why}"
    );
}

// The dry run is the diff's right-hand side. Opening against the loaded copy
// alone and pressing ctrl-s twice used to reach a write whose payload the
// server had never seen -- defaulting, admission and merge all unasked.
#[test]
fn an_apply_the_server_has_never_seen_is_refused_until_it_has() {
    let mut at = ready(&[], Verdict::Differs);
    at.dry_run = None;
    let why = prepare(Armed::Apply, at).expect_err("refused");
    assert!(why.contains("ctrl-alt-r"), "{why}");

    at.dry_run = Some(Review {
        stamp: BufferStamp::of(1, 3),
        force: false,
    });
    assert_eq!(prepare(Armed::Force, at), Ok(Preparation::PreviewForce));
}

// This rule lived inside the ForceApply key handler, which is one of the
// two ways in; the palette dispatches the same action.
#[test]
fn a_force_needs_a_conflict_that_named_the_fields() {
    let mut at = ready(&[], Verdict::Differs);
    at.conflicts = 0;
    assert_eq!(
        prepare(Armed::Force, at),
        Err("nothing is owned elsewhere, so there is nothing to force".to_string())
    );
    assert_eq!(
        prepare(Armed::Apply, at),
        Ok(Preparation::Confirm),
        "which says nothing about a plain apply"
    );
}

#[test]
fn the_first_conflict_needs_a_forced_preview_before_confirming_a_write() {
    let mut at = ready(&[], Verdict::Differs);
    for prior in [
        None,
        Some(Review {
            stamp: at.reviewed,
            force: false,
        }),
        Some(Review {
            stamp: BufferStamp::of(1, 3),
            force: true,
        }),
    ] {
        at.dry_run = prior;
        assert_eq!(prepare(Armed::Force, at), Ok(Preparation::PreviewForce));
    }

    at.dry_run = Some(Review {
        stamp: at.reviewed,
        force: true,
    });
    assert_eq!(prepare(Armed::Force, at), Ok(Preparation::Confirm));
    let mut gate = ApplyGate::default();
    assert_eq!(gate.step(Armed::Force, true), Step::Ask);
    assert_eq!(gate.step(Armed::Force, true), Step::Go);
}

#[test]
fn a_forced_preview_still_checks_the_buffer_payload_conflict_and_wire() {
    let at = Ready {
        dry_run: None,
        ..ready(&[], Verdict::Differs)
    };
    for refused in [
        Ready {
            patchable: false,
            ..at
        },
        Ready {
            blocked: &["multiple documents"],
            ..at
        },
        Ready {
            verdict: Verdict::Refused("the comparison is too large"),
            ..at
        },
        Ready { editor: None, ..at },
        Ready {
            editor: Some(BufferStamp::of(2, 4)),
            ..at
        },
        Ready {
            editor: Some(BufferStamp::of(1, 5)),
            ..at
        },
        Ready { conflicts: 0, ..at },
        Ready {
            in_flight: Some("a dry run"),
            ..at
        },
        Ready {
            in_flight: Some("an apply"),
            ..at
        },
    ] {
        assert!(prepare(Armed::Force, refused).is_err(), "{refused:?}");
    }
}

#[test]
fn a_forced_preview_names_the_transfer_even_when_the_documents_agree() {
    for verdict in [Verdict::Differs, Verdict::Agreed] {
        assert_eq!(
            reviewed(verdict, true, Seen::Same),
            Reviewed::Recorded(
                "forced dry run only; ctrl-shift-s asks to take the conflicting fields"
            )
        );
    }
    assert!(matches!(
        reviewed(Verdict::Refused("too many lines"), true, Seen::Same),
        Reviewed::Refused(_)
    ));
}

// A comparison of two objects authorises nothing, whatever it concluded, and
// the next press says why in the race's own words rather than claiming the
// server was never asked.
#[test]
fn a_raced_dry_run_records_no_review_and_the_press_names_the_race() {
    for verdict in [Verdict::Differs, Verdict::Agreed] {
        for force in [false, true] {
            assert_eq!(reviewed(verdict, force, Seen::Raced), Reviewed::Raced);
        }
    }
    for seen in [Seen::Same, Seen::Replaced, Seen::Unknown, Seen::Gone] {
        assert!(
            matches!(
                reviewed(Verdict::Differs, false, seen),
                Reviewed::Recorded(_)
            ),
            "{seen:?} records"
        );
    }
    let mut at = ready(&[], Verdict::Differs);
    at.dry_run = None;
    at.raced = true;
    assert_eq!(
        prepare(Armed::Apply, at),
        Err("this comparison mixes two objects, so nothing was sent".to_string())
    );
    let mut at = ready(&[], Verdict::Differs);
    at.dry_run = None;
    assert_eq!(
        prepare(Armed::Apply, at),
        Err("the server has not been asked what this would store; ctrl-alt-r asks it".to_string())
    );
}

// After a creation lands, "the object this was opened from is gone; applying
// creates it" would still stand beside the answer. A real apply that landed, or
// may have, leaves the note nothing to say; a dry run or a refusal leaves it.
#[test]
fn a_real_apply_that_may_have_written_leaves_the_seen_note_nothing_to_say() {
    let landed = ApplyOutcome::Applied { uid: None };
    let unconfirmed = ApplyOutcome::Unconfirmed("no answer within 35 s".to_string());
    let refused = ApplyOutcome::Failed("Request entity too large: limit is 3145728".to_string());
    assert_eq!(seen_after(Seen::Gone, false, Some(&landed)), Seen::Unknown);
    assert_eq!(
        seen_after(Seen::Gone, false, Some(&unconfirmed)),
        Seen::Unknown
    );
    assert_eq!(
        seen_after(Seen::Replaced, false, None),
        Seen::Unknown,
        "a dropped reply"
    );
    assert_eq!(seen_after(Seen::Gone, false, Some(&refused)), Seen::Gone);
    assert_eq!(
        seen_after(Seen::Replaced, true, Some(&landed)),
        Seen::Replaced,
        "a dry run writes nothing"
    );
}

// Once a real apply went unconfirmed, the line says it may still land for the
// life of the view: no later answer says anything about that write.
#[test]
fn an_unconfirmed_apply_stands_for_the_life_of_the_view() {
    let unconfirmed = ApplyOutcome::Unconfirmed("no answer within 35 s".to_string());
    let landed = ApplyOutcome::Applied { uid: None };
    let refused = ApplyOutcome::Failed("Request entity too large: limit is 3145728".to_string());
    assert!(may_still_land(false, false, Some(&unconfirmed)));
    assert!(may_still_land(false, false, None), "a dropped reply");
    assert!(
        !may_still_land(false, true, None),
        "a dry run writes nothing"
    );
    assert!(!may_still_land(false, true, Some(&unconfirmed)));
    assert!(!may_still_land(false, false, Some(&landed)));
    assert!(
        may_still_land(true, true, None),
        "a later dry run, the commonest later answer, clears nothing"
    );
    for later in [&landed, &refused] {
        assert!(
            may_still_land(true, false, Some(later)),
            "{later:?} clears nothing"
        );
    }
}

#[test]
fn an_unpatchable_kind_and_a_held_wire_each_say_which_they_are() {
    let mut at = ready(&[], Verdict::Differs);
    at.patchable = false;
    assert_eq!(
        prepare(Armed::Apply, at),
        Err("the server serves this kind without a patch verb".to_string())
    );

    let mut at = ready(&[], Verdict::Differs);
    at.in_flight = Some("a dry run");
    assert_eq!(
        prepare(Armed::Apply, at),
        Err("a dry run is already in flight".to_string()),
        "and it is named for what it is, not for what it is not"
    );
}

// A dry run's answer describes one comparison and is worthless against
// another. A real apply's answer describes a write that has happened, and
// discarding it left a webhook denial nowhere on screen.
#[test]
fn a_recompute_discards_a_dry_run_and_never_a_write() {
    let sent = |dry_run, force| Sent {
        generation: 7,
        stamp: BufferStamp::of(1, 4),
        dry_run,
        force,
        note: String::new(),
        uid: Some("uid-1".to_string()),
    };
    for force in [false, true] {
        assert!(sent(true, force).still_speaks(7));
        assert!(
            !sent(true, force).still_speaks(8),
            "the comparison moved on"
        );
        assert!(sent(false, force).still_speaks(8), "the write did not");
    }
}

#[test]
fn a_dry_run_the_diff_could_not_compare_authorises_nothing() {
    assert_eq!(
        reviewed(Verdict::Differs, false, Seen::Same),
        Reviewed::Recorded("ctrl-s applies this")
    );
    assert_eq!(
        reviewed(Verdict::Agreed, false, Seen::Same),
        Reviewed::Recorded("the cluster already holds this; applying changes nothing")
    );
    let Reviewed::Refused(note) = reviewed(
        Verdict::Refused("one side of this comparison has more than 65,536 lines"),
        false,
        Seen::Same,
    ) else {
        panic!("a refusal is not a review");
    };
    assert!(note.contains("65,536 lines"), "{note}");
    assert!(
        !note.contains("applying changes nothing"),
        "the sentence that made an unreviewed apply look like a no-op: {note}"
    );
}

// Both halves of what the prune did. `kept` was written by the pruner and
// read by nobody, so an apply carrying `metadata.resourceVersion` -- an
// optimistic-lock precondition the user never asked for -- confirmed itself
// with an unqualified "applied".
#[test]
fn the_note_names_what_was_sent_as_well_as_what_was_not() {
    let mut payload = k10s_edit::Payload::default();
    assert_eq!(prune_note(&payload), "");

    payload.pruned = vec!["metadata.uid".to_string()];
    payload.kept = vec!["metadata.resourceVersion".to_string()];
    let note = prune_note(&payload);
    assert!(note.contains("were not sent (metadata.uid)"), "{note}");
    assert!(
        note.contains("went with it (metadata.resourceVersion)"),
        "the field still in the bytes is named too: {note}"
    );
}

// The remedy a message offers has to be one that can clear the state. `r`
// re-derives the comparison from the editor's cached text, which is the
// document the server has just called out of date.
#[test]
fn a_stale_answer_does_not_offer_a_key_that_cannot_help() {
    let note = stale_note("Operation cannot be fulfilled: the object has been modified");
    assert!(note.starts_with("Operation cannot be fulfilled"), "{note}");
    assert!(
        note.contains("r only re-compares the same text"),
        "the key that looks like the remedy is named as not being one: {note}"
    );
    assert!(note.contains("open the object again"), "{note}");
}

// A field neither document declared. Reading it as a conflict promises a
// refusal the server has not made, over a value the server itself supplied.
#[test]
fn a_field_the_last_apply_never_declared_is_labelled_undeclared_not_conflicting() {
    let base = "kind: Pod\nname: web\n";
    let live = "kind: Pod\nimagePullPolicy: Always\nname: web\n";
    let buffer = "kind: Pod\nimagePullPolicy: IfNotPresent\nname: web\n";
    let state = local(live, Some(base), buffer);
    let lines = texts(&state);
    assert!(
        lines.contains(
            &"  the last apply declared nothing here; the dry run says what applying does"
                .to_string()
        ),
        "{lines:?}"
    );
    assert!(
        state.summary().contains("1 undeclared"),
        "{}",
        state.summary()
    );
    assert!(
        !state.summary().contains("conflicting"),
        "and no refusal is implied: {}",
        state.summary()
    );
    assert_eq!(state.diff().verdict(), Verdict::Differs);
}

// Where the reader is, which is what an action acts on. `n` leaves the top on
// a hunk's own header, and the header belongs to the hunk it heads.
#[test]
fn the_hunk_under_the_reader_is_the_one_the_header_heads() {
    let live = "a\nb\nc\nd\ne\n";
    let buffer = "a\nB\nc\nd\nE\n";
    let mut state = local(live, None, buffer);
    state.set_viewport(4);
    assert_eq!(
        state.origin_of(state.hunk_at_top().expect("a hunk is on screen")),
        Some(Origin::Common),
        "the reader starts in the unchanged run"
    );
    assert!(state.next_change());
    let first = state.hunk_at_top().expect("on a change now");
    assert_eq!(state.origin_of(first), Some(Origin::Mine));
    assert!(state.next_change());
    let second = state.hunk_at_top().expect("and the next one");
    assert_eq!(state.origin_of(second), Some(Origin::Mine));
    assert_ne!(first, second, "a different hunk, not the same one twice");
}

// The acting half of the classification: naming drift since the last apply
// is half an answer while the only way to keep it is to retype it.
#[test]
fn taking_a_hunk_edits_the_buffer_into_the_copy_this_editor_read() {
    let base = "replicas: 1\nimage: nginx\n";
    let live = "replicas: 5\nimage: nginx\n";
    let buffer = "replicas: 1\nimage: nginx\n";
    let state = local(live, Some(base), buffer);
    // The first hunk *is* the change, so the reader starts on it: there is
    // nothing ahead for next-change to step to.
    let hunk = state.hunk_at_top().expect("the reader is on it");
    assert_eq!(state.origin_of(hunk), Some(Origin::Theirs));
    let keep = state.keep(hunk).expect("it can be kept");
    let mut text = buffer.to_string();
    text.replace_range(keep.range.clone(), &keep.text);
    assert_eq!(text, live);
    assert_eq!(
        kept_note(Origin::Theirs, &keep),
        "kept the change in the copy this editor read: 1 line in place of 1; ctrl-alt-r asks the \
         server about the result"
    );
}

// The load-bearing refusal. In the dry-run comparison the right-hand document
// is the *server's* answer, not the editor's buffer, so a hunk's ranges point
// into a document the editor does not have: an edit built from them would
// splice bytes at offsets that mean nothing where they land.
#[test]
fn a_hunk_of_the_servers_own_answer_is_never_spliced_into_the_buffer() {
    let stamp = BufferStamp::of(1, 4);
    let at = Keepable {
        mode: Some(Mode::DryRun),
        origin: Some(Origin::Mine),
        reviewed: stamp,
        editor: Some(stamp),
    };
    assert_eq!(
        refuse_keep(at).as_deref(),
        Some(
            "this compares the server's own answer; ctrl-alt-d compares with the copy this \
             editor read, whose hunks can be kept"
        ),
        "and names the way back"
    );

    let local = Keepable {
        mode: Some(Mode::Local),
        ..at
    };
    assert_eq!(refuse_keep(local), None, "the local one is the editor's");
}

#[test]
fn keeping_a_hunk_needs_the_buffer_the_comparison_was_made_of() {
    let at = Keepable {
        mode: Some(Mode::Local),
        origin: Some(Origin::Theirs),
        reviewed: BufferStamp::of(1, 4),
        editor: Some(BufferStamp::of(1, 5)),
    };
    let why = refuse_keep(at).expect("refused");
    assert!(
        why.contains("the buffer changed after this comparison"),
        "{why}"
    );

    let gone = Keepable { editor: None, ..at };
    let why = refuse_keep(gone).expect("refused");
    assert!(why.contains("nothing to edit"), "{why}");

    // A reload restarts versions at zero, so the same number is a different
    // document -- the rule an apply follows, and here it is sharper: a range
    // from another document splices at a meaningless offset.
    let replaced = Keepable {
        editor: Some(BufferStamp::of(2, 4)),
        ..at
    };
    assert!(refuse_keep(replaced).is_some());
}

#[test]
fn there_is_nothing_to_keep_where_the_documents_agree() {
    let stamp = BufferStamp::of(1, 4);
    let at = Keepable {
        mode: Some(Mode::Local),
        origin: Some(Origin::Common),
        reviewed: stamp,
        editor: Some(stamp),
    };
    let nothing =
        Some("nothing here differs from the copy this editor read; n moves to the next change");
    assert_eq!(refuse_keep(at).as_deref(), nothing);
    assert_eq!(
        refuse_keep(Keepable { origin: None, ..at }).as_deref(),
        nothing
    );

    for origin in [
        Origin::Mine,
        Origin::Theirs,
        Origin::Conflict,
        Origin::Undeclared,
    ] {
        assert_eq!(
            refuse_keep(Keepable {
                origin: Some(origin),
                ..at
            }),
            None,
            "{origin:?} has a side in the copy read to take"
        );
    }
}

#[test]
fn what_was_kept_is_named_by_the_classification_it_came_from() {
    let keep = |taken, dropped| diff::Keep {
        range: 0..0,
        text: String::new(),
        taken,
        dropped,
    };
    assert!(
        kept_note(Origin::Theirs, &keep(1, 0))
            .starts_with("kept the change in the copy this editor read: added 1 line")
    );
    assert!(
        kept_note(Origin::Mine, &keep(0, 2))
            .starts_with("put back the text of the copy this editor read: dropped 2 lines")
    );
    assert_eq!(
        kept_note(Origin::Conflict, &keep(3, 1)),
        "took the side of the copy this editor read: 3 lines in place of 1; ctrl-alt-r asks the \
         server about the result"
    );
    assert_eq!(
        kept_note(Origin::Undeclared, &keep(1, 1)),
        "took the value in the copy this editor read: 1 line in place of 1; ctrl-alt-r asks the \
         server about the result",
        "the dry run that authorised an apply is void, and the sentence says which key asks again"
    );
}

// A server-side apply creates what is absent, so a press can land on a
// different object than the one that was read. The uid says which, and a
// missing uid says nothing at all.
#[test]
fn an_answer_about_a_different_object_is_only_claimed_when_both_uids_are_known() {
    assert_eq!(identity(Some("a"), Some("a")), Identity::Same);
    assert_eq!(identity(Some("a"), Some("b")), Identity::Different);
    assert_eq!(identity(None, Some("b")), Identity::Unknown);
    assert_eq!(identity(Some("a"), None), Identity::Unknown);
    assert_eq!(identity(None, None), Identity::Unknown);

    assert!(recreated_note().contains("did not update the object this was opened from"));
    // Which is only said when both are known, and is said about the uid the
    // request went out with -- not about whatever the view points at when the
    // answer lands. A real apply's reply always speaks, so a recompute during
    // a slow apply would otherwise report a recreation that did not happen.
    assert_eq!(landed_note(Some("a"), Some("b")), recreated_note());
    assert_eq!(landed_note(Some("a"), Some("a")), "");
    assert_eq!(landed_note(None, Some("b")), "");
    assert_eq!(landed_note(Some("a"), None), "");
    assert_eq!(
        stale_object_note(),
        "the object this was opened from was replaced; this compares with the one the server holds \
         now, and applying lands on it"
    );
    assert_eq!(
        gone_note(),
        "the object this was opened from is gone; applying creates it"
    );
}

// The left side of a dry-run review is the dry run's own read of the object,
// never the copy the editor loaded, and that read is what says whether the
// object is the one the buffer was opened from.
#[test]
fn a_dry_run_is_compared_with_its_own_read_of_the_object() {
    let read = |uid: &str| LiveCopy::Read {
        yaml: "kind: ConfigMap\ndata:\n  a: landed\n".to_string(),
        uid: Some(uid.to_string()),
    };
    assert_eq!(
        dry_run_left(Some("uid-1"), read("uid-1"), Some("uid-1")),
        (
            "kind: ConfigMap\ndata:\n  a: landed\n".to_string(),
            Seen::Same
        )
    );
    // Replaced since the editor read it: the comparison is against the one
    // there now, and says so.
    assert_eq!(
        dry_run_left(Some("uid-1"), read("uid-2"), Some("uid-2")).1,
        Seen::Replaced
    );
    // Replaced between the dry run's read and its answer: two objects.
    assert_eq!(
        dry_run_left(Some("uid-1"), read("uid-1"), Some("uid-3")).1,
        Seen::Raced
    );
    // A read or an answer without a uid claims nothing: never a race and never
    // a replacement on a missing field.
    let unnamed = LiveCopy::Read {
        yaml: String::new(),
        uid: None,
    };
    assert_eq!(
        dry_run_left(Some("uid-1"), unnamed, Some("uid-1")).1,
        Seen::Unknown
    );
    assert_eq!(
        dry_run_left(Some("uid-1"), read("uid-1"), None).1,
        Seen::Same
    );
    assert_eq!(
        dry_run_left(None, read("uid-1"), Some("uid-1")).1,
        Seen::Unknown
    );
    // Gone: an empty left side, so the review reads as a creation, whatever
    // uid the answer would give the new object.
    assert_eq!(
        dry_run_left(Some("uid-1"), LiveCopy::Absent, Some("uid-new")),
        (String::new(), Seen::Gone)
    );
    assert_eq!(
        raced_note(),
        "the object was replaced between this review's read and its dry run; ctrl-alt-r compares \
         again"
    );
}

// The rule that cost a bug in the editor: one bit cannot tell two questions
// apart, so a press meant for the force answers the apply.
#[test]
fn the_gate_arms_under_its_own_name_and_a_different_press_re_asks() {
    let mut gate = ApplyGate::default();
    assert_eq!(gate.step(Armed::Apply, true), Step::Ask);
    assert_eq!(
        gate.step(Armed::Force, true),
        Step::Ask,
        "a different question"
    );
    assert_eq!(gate.step(Armed::Force, true), Step::Go);
    assert_eq!(
        gate.step(Armed::Force, true),
        Step::Ask,
        "firing disarms, so the next force asks again"
    );
    gate.disarm();
    assert_eq!(gate.step(Armed::Apply, true), Step::Ask);
    gate.disarm();
    assert_eq!(
        gate.step(Armed::Apply, true),
        Step::Ask,
        "a recompute makes the next press the first one again"
    );
}

#[test]
fn an_applied_note_says_whether_the_buffer_followed_the_write() {
    assert_eq!(
        applied_note(" (forced)", "", Reload::Done),
        "applied as fieldManager k10s (forced)"
    );
    let moved = applied_note("", "; landed on another object", Reload::Moved);
    assert!(
        moved.ends_with(
            "the buffer is not the one that was applied any more, so it was left as it is"
        ),
        "an edit made during the flight must be said to have survived: {moved}"
    );
    assert!(moved.contains("; landed on another object"));
    assert!(
        applied_note("", "", Reload::Gone)
            .ends_with("the editor it came from is closed, so nothing was reloaded"),
        "a closed editor did not have its buffer move"
    );
}

#[test]
fn a_scrolled_prompt_is_revealed_without_spending_the_confirmation() {
    for wanted in [Armed::Apply, Armed::Force] {
        let mut gate = ApplyGate::default();
        assert_eq!(gate.step(wanted, false), Step::Ask);
        assert_eq!(gate.armed(), Some(wanted));
        assert_eq!(gate.step(wanted, false), Step::Reveal);
        assert_eq!(gate.step(wanted, false), Step::Reveal);
        assert_eq!(gate.armed(), Some(wanted), "revealing keeps the question");
        assert_eq!(gate.step(wanted, true), Step::Go);
        assert_eq!(gate.armed(), None, "only a visible confirmation spends it");
        assert_eq!(gate.step(wanted, true), Step::Ask);
    }
    let mut gate = ApplyGate::default();
    assert_eq!(gate.step(Armed::Apply, true), Step::Ask);
    assert_eq!(gate.step(Armed::Force, false), Step::Ask);
    assert_eq!(
        gate.armed(),
        Some(Armed::Force),
        "a different question re-arms"
    );
}

// A reply that never arrived used to read "the request ended without an
// answer" for a real apply too, which says nothing about the write that may
// already have been served.
#[test]
fn a_real_apply_without_an_answer_says_it_may_have_landed() {
    assert_eq!(dropped_note(true), "the dry run ended without an answer");
    assert_eq!(
        dropped_note(false),
        "the apply ended without an answer; it may have landed; ctrl-alt-r compares the buffer \
         with the object as it is now"
    );
}

// The reason comes last: a lost connection's is kube's whole error chain, and
// at the lab's window width a line that led with it was cut before "may have
// landed", reading as a failure.
#[test]
fn an_unconfirmed_apply_gives_its_reason_and_the_key_that_shows_what_is_there() {
    assert_eq!(
        unconfirmed_note("no answer within 35 s"),
        "the apply may have landed; ctrl-alt-r compares the buffer with the object as it is now; \
         no answer within 35 s"
    );
}

#[test]
fn a_dry_run_whose_answer_cannot_be_shown_has_nothing_to_review() {
    assert_eq!(
        unrendered_note("answer too deep"),
        "the server accepted this dry run, but its answer cannot be shown here, so there is \
         nothing to review: answer too deep"
    );
}

#[test]
fn a_rejection_names_its_causes_bounded_or_falls_back_to_the_message() {
    assert_eq!(
        rejected_note("invalid", Vec::new()),
        "the server refused the document: invalid",
        "an empty cause list is still a refusal, carried by the message"
    );

    let named = rejected_note(
        "invalid",
        vec!["a: bad".to_string(), "b: worse".to_string()],
    );
    assert_eq!(named, "the server refused the document: a: bad; b: worse");

    let many: Vec<String> = (0..20).map(|n| format!("field-{n}: bad")).collect();
    let bounded = rejected_note("invalid", many);
    assert!(
        bounded.contains("field-11") && !bounded.contains("field-12"),
        "causes are bounded at MAX_SHOWN_CAUSES: {bounded}"
    );
}

fn line_at<'a>(conflict: &'a [Conflicted], armed: Option<Armed>) -> Line<'a> {
    Line {
        summary: "3 hunks",
        hints: "",
        blocked: &[],
        kept: &[],
        armed,
        conflict,
        conflict_truncated: false,
        seen: Seen::Same,
        unknown_write: false,
        status: None,
        context: Some("prod-eu"),
        folded: false,
    }
}

#[test]
fn the_armed_prompt_is_derived_so_no_status_can_overwrite_it() {
    let quiet = status_line(line_at(&[], None));
    assert_eq!(quiet, "3 hunks");

    let armed = status_line(Line {
        status: Some("connected"),
        ..line_at(&[], Some(Armed::Apply))
    });
    assert!(
        armed.contains("ctrl-s again to apply this to context prod-eu"),
        "the prompt stands for as long as the latch does, and names where it lands: {armed}"
    );
    assert!(
        armed.ends_with("connected"),
        "a one-shot status rides along rather than replacing it: {armed}"
    );
}

#[test]
fn the_conflict_list_counts_its_overflow_and_names_the_servers_remainder() {
    let conflict: Vec<Conflicted> = (0..14)
        .map(|n| Conflicted {
            field: format!(".spec.field{n}"),
            manager: "hpa".to_string(),
        })
        .collect();

    let listed = status_line(line_at(&conflict, None));
    assert!(
        listed.contains(", and 2 more"),
        "12 are shown and the arithmetic must say what is not: {listed}"
    );

    let truncated = status_line(Line {
        conflict_truncated: true,
        ..line_at(&conflict, None)
    });
    assert!(
        truncated.contains(", and 2 more") && truncated.contains("further fields the server named"),
        "the truncation sentence is in addition to the overflow count: {truncated}"
    );
}

#[test]
fn a_dry_run_that_read_another_object_or_none_says_so_on_the_line() {
    let stale = status_line(Line {
        seen: Seen::Replaced,
        ..line_at(&[], None)
    });
    assert!(stale.contains(stale_object_note()), "{stale}");
    let gone = status_line(Line {
        seen: Seen::Gone,
        ..line_at(&[], None)
    });
    assert!(gone.contains(gone_note()), "{gone}");
    let raced = status_line(Line {
        seen: Seen::Raced,
        ..line_at(&[], None)
    });
    assert!(raced.contains(raced_note()), "{raced}");
    for quiet in [Seen::Same, Seen::Unknown] {
        let line = status_line(Line {
            seen: quiet,
            ..line_at(&[], None)
        });
        assert!(
            !line.contains("replaced") && !line.contains("gone"),
            "{quiet:?}: {line}"
        );
    }
}

// Derived, like the armed prompt, so the message a dry run or an owed
// comparison writes cannot erase it.
#[test]
fn an_apply_that_may_still_land_stays_on_the_line_whatever_the_status_says() {
    let line = status_line(Line {
        unknown_write: true,
        status: Some("ctrl-s applies this"),
        ..line_at(&[], None)
    });
    assert!(line.contains(standing_note()), "{line}");
    assert_eq!(
        standing_note(),
        "an apply from this view went unconfirmed and may still land"
    );
    assert!(line.contains("ctrl-s applies this"), "{line}");
    assert!(!status_line(line_at(&[], None)).contains("may still land"));
}

// The line runs in the order a person acts on it: the prompt, the payload's
// standing refusals of the press, then the answer, what stands about the
// object and its owners, and the comparison's own remarks last.
#[test]
fn the_status_line_puts_what_a_person_acts_on_before_the_hints() {
    let conflict = [Conflicted {
        field: ".data.a".to_string(),
        manager: "rival".to_string(),
    }];
    let line = status_line(Line {
        hints: "the final newline differs",
        seen: Seen::Replaced,
        unknown_write: true,
        status: Some("the last answer"),
        blocked: &["a blocked field"],
        kept: &["metadata.managedFields".to_string()],
        ..line_at(&conflict, Some(Armed::Apply))
    });
    let at = |piece: &str| {
        line.find(piece)
            .unwrap_or_else(|| panic!("{piece} in {line}"))
    };
    let order = [
        at("ctrl-s again"),
        at("3 hunks"),
        at("cannot be applied"),
        at("still carries the server's own"),
        at("the last answer"),
        at(stale_object_note()),
        at(standing_note()),
        at("owned elsewhere"),
        at("the final newline differs"),
    ];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{line}");
}

#[test]
fn the_armed_prompt_precedes_even_a_summary_taller_than_the_status_region() {
    let summary = "a long comparison summary\n".repeat(50);
    for armed in [Armed::Apply, Armed::Force] {
        let line = status_line(Line {
            summary: &summary,
            ..line_at(&[], Some(armed))
        });
        assert!(line.starts_with("ctrl-"), "{line}");
        assert!(line.find("again").unwrap() < line.find(&summary).unwrap());
    }
}

// A comparison asked for while a real apply was out is not sent after it, and
// the apply's answer says so rather than being replaced a round trip later.
#[test]
fn an_owed_comparison_skipped_after_an_apply_is_named_after_its_answer() {
    assert_eq!(
        owed_not_sent("applied as fieldManager k10s".to_string(), false),
        "applied as fieldManager k10s; the comparison asked for while it was out was not sent; \
         ctrl-alt-r asks for it"
    );
    assert_eq!(
        owed_not_sent(unconfirmed_note("no answer within 35 s"), true),
        "the apply may have landed; ctrl-alt-r compares the buffer with the object as it is now; \
         no answer within 35 s; the comparison asked for while it was out was not sent"
    );
}

#[test]
fn row_colors_follow_the_classification_not_the_side_alone() {
    let family = k10s_theme::builtin_family();
    let theme = family
        .themes
        .first()
        .expect("the built-in family ships a theme");

    let color = |origin, side| {
        let painted = Painted {
            mark: ' ',
            origin,
            side,
            text: std::borrow::Cow::Borrowed(""),
        };
        row_color(theme, &painted)
    };

    assert_eq!(
        color(Origin::Common, Some(diff::Side::Live)),
        theme.shell.editor_foreground
    );
    assert_eq!(
        color(Origin::Mine, Some(diff::Side::Buffer)),
        theme.shell.success
    );
    assert_eq!(
        color(Origin::Mine, Some(diff::Side::Live)),
        theme.shell.error
    );
    // Undeclared is deliberately not an error colour: nothing was taken from
    // anyone and no refusal is coming.
    assert_eq!(
        color(Origin::Undeclared, Some(diff::Side::Live)),
        theme.shell.warning
    );
    assert_ne!(
        color(Origin::Undeclared, Some(diff::Side::Live)),
        theme.shell.error
    );
    assert_eq!(color(Origin::Conflict, None), theme.shell.text_accent);
}

#[test]
fn every_armed_write_names_the_context_it_would_land_in() {
    let apply = status_line(line_at(&[], Some(Armed::Apply)));
    assert!(
        apply.contains("context prod-eu"),
        "an apply says which cluster: {apply}"
    );

    let conflict = [Conflicted {
        field: ".spec.replicas".to_string(),
        manager: "hpa".to_string(),
    }];
    let force = status_line(line_at(&conflict, Some(Armed::Force)));
    assert!(
        force.contains("context prod-eu"),
        "so does a force, which is the more dangerous of the two: {force}"
    );

    let in_cluster = status_line(Line {
        context: None,
        ..line_at(&[], Some(Armed::Apply))
    });
    assert!(
        in_cluster.contains("in-cluster account"),
        "an unnamed context is an answer, not a blank: {in_cluster}"
    );
    assert!(
        !in_cluster.contains("to the cluster"),
        "and never the old sentence that named nothing: {in_cluster}"
    );
}
