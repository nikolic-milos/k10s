//! What the diff view claims, checked without a window.
//!
//! The two things worth testing here are the classification -- an edit, drift
//! the apply would revert, or a collision -- and the preconditions a *press*
//! has to clear. The second is why [`crate::diff_gate::refuse`] is one pure
//! function: the two preconditions that ever failed were the two that lived
//! somewhere else, guarding one way in while another way round stayed open.

use k10s_edit::diff::{self, Origin, Verdict};

use crate::diff::*;
use crate::diff_gate::{
    ApplyGate, Armed, Flight, Identity, Keepable, Ready, Sent, Step, identity, kept_note,
    landed_note, recreated_note, refuse, refuse_keep, reviewed, stale_object_note,
};
use crate::editor::BufferStamp;
use crate::provider::Conflicted;

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

#[test]
fn a_two_way_comparison_says_it_has_no_base() {
    let buffer = LIVE.replace("nginx:1.27", "nginx:1.28");
    let state = local(LIVE, None, &buffer);
    assert!(
        state
            .summary()
            .contains("no last-applied-configuration, so this is two-way")
    );
}

#[test]
fn drift_the_apply_would_revert_is_labelled_as_the_clusters_own() {
    let base = LIVE;
    let live = LIVE.replace("nginx:1.27", "nginx:1.29");
    let buffer = LIVE;
    let state = local(&live, Some(base), buffer);
    let lines = texts(&state);
    assert!(
        lines.contains(&"  the cluster changed this; applying reverts it".to_string()),
        "{lines:?}"
    );
    assert!(state.summary().contains("1 reverted by applying"));
    assert!(!state.summary().contains("two-way"));
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
    assert!(!state.summary().contains("two-way"));
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
        dry_run: Some(stamp),
        conflicts: 1,
        in_flight: None,
    }
}

#[test]
fn a_press_that_clears_every_precondition_is_the_only_one_that_writes() {
    assert_eq!(refuse(Armed::Apply, ready(&[], Verdict::Differs)), None);
    assert_eq!(refuse(Armed::Force, ready(&[], Verdict::Differs)), None);
    assert_eq!(
        refuse(Armed::Apply, ready(&[], Verdict::Agreed)),
        None,
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
        refuse(Armed::Apply, ready(&[], refused)),
        Some("nothing here has been reviewed, so there is nothing to apply".to_string()),
    );
    assert_eq!(
        refuse(Armed::Force, ready(&[], refused)),
        Some("nothing here has been reviewed, so there is nothing to apply".to_string()),
    );
}

// The bytes of a blocked payload are the document unpruned -- every field
// the server owns still in it. Nothing read `blocked`, so they went out.
#[test]
fn a_payload_the_pruner_refused_never_becomes_a_request() {
    // The baseline press goes through, so the one thing that changed here
    // is the one thing that stopped it. The reasons themselves stand on
    // their own piece of the status line rather than being repeated.
    assert_eq!(refuse(Armed::Apply, ready(&[], Verdict::Differs)), None);
    assert_eq!(
        refuse(
            Armed::Apply,
            ready(
                &["a cluster apply needs exactly one YAML document"],
                Verdict::Differs,
            ),
        ),
        Some("this document cannot be applied, so nothing was sent".to_string()),
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
    at.dry_run = Some(BufferStamp::of(1, 3));
    let why = refuse(Armed::Apply, at).expect("refused");
    assert!(
        why.contains("the buffer changed after this comparison"),
        "{why}"
    );

    at.editor = None;
    let why = refuse(Armed::Apply, at).expect("refused");
    assert!(
        why.contains("the editor this diff came from is gone"),
        "{why}"
    );
}

// The dry run is the diff's right-hand side. Opening the view against live
// alone and pressing ctrl-s twice used to reach a write whose payload the
// server had never seen -- defaulting, admission and merge all unasked.
#[test]
fn an_apply_the_server_has_never_seen_is_refused_until_it_has() {
    let mut at = ready(&[], Verdict::Differs);
    at.dry_run = None;
    let why = refuse(Armed::Apply, at).expect("refused");
    assert!(why.contains("ctrl-alt-r"), "{why}");

    at.dry_run = Some(BufferStamp::of(1, 3));
    let why = refuse(Armed::Force, at).expect("a force is an apply too");
    assert!(why.contains("has not been asked"), "{why}");
}

// This rule lived inside the ForceApply key handler, which is one of the
// two ways in; the palette dispatches the same action.
#[test]
fn a_force_needs_a_conflict_that_named_the_fields() {
    let mut at = ready(&[], Verdict::Differs);
    at.conflicts = 0;
    assert_eq!(
        refuse(Armed::Force, at),
        Some("nothing is owned elsewhere, so there is nothing to force".to_string())
    );
    assert_eq!(
        refuse(Armed::Apply, at),
        None,
        "which says nothing about a plain apply"
    );
}

#[test]
fn an_unpatchable_kind_and_a_held_wire_each_say_which_they_are() {
    let mut at = ready(&[], Verdict::Differs);
    at.patchable = false;
    assert_eq!(
        refuse(Armed::Apply, at),
        Some("the server serves this kind without a patch verb".to_string())
    );

    let mut at = ready(&[], Verdict::Differs);
    at.in_flight = Some("a dry run");
    assert_eq!(
        refuse(Armed::Apply, at),
        Some("a dry run is already in flight".to_string()),
        "and it is named for what it is, not for what it is not"
    );
}

// A dry run's answer describes one comparison and is worthless against
// another. A real apply's answer describes a write that has happened, and
// discarding it left a webhook denial nowhere on screen.
#[test]
fn a_recompute_discards_a_dry_run_and_never_a_write() {
    let sent = |dry_run| Sent {
        generation: 7,
        stamp: BufferStamp::of(1, 4),
        dry_run,
        note: String::new(),
        uid: Some("uid-1".to_string()),
    };
    assert!(sent(true).still_speaks(7));
    assert!(!sent(true).still_speaks(8), "the comparison moved on");
    assert!(sent(false).still_speaks(8), "the write did not");
}

#[test]
fn a_dry_run_the_diff_could_not_compare_authorises_nothing() {
    assert_eq!(reviewed(Verdict::Differs), Ok("ctrl-s applies this"));
    assert_eq!(
        reviewed(Verdict::Agreed),
        Ok("the cluster already holds this; applying changes nothing")
    );
    let note = reviewed(Verdict::Refused(
        "one side of this comparison has more than 65,536 lines",
    ))
    .expect_err("a refusal is not a review");
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

// The acting half of the classification: naming drift an apply would revert
// is half an answer while the only way to keep it is to retype it.
#[test]
fn keeping_the_clusters_side_edits_the_buffer_into_the_clusters_document() {
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
        "kept the cluster's change: 1 line in place of 1; ctrl-alt-r asks the server about \
         the result"
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
    let why = refuse_keep(at).expect("refused");
    assert!(why.contains("the server's own answer"), "{why}");
    assert!(why.contains("ctrl-alt-d"), "and names the way back: {why}");

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
    let why = refuse_keep(at).expect("refused");
    assert!(why.contains("nothing here differs"), "{why}");
    assert!(
        why.contains("n moves to the next change"),
        "and names the key that moves: {why}"
    );
    assert!(refuse_keep(Keepable { origin: None, ..at }).is_some());

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
            "{origin:?} has a cluster side to keep"
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
            .starts_with("kept the cluster's change: added 1 line")
    );
    assert!(
        kept_note(Origin::Mine, &keep(0, 2))
            .starts_with("put the cluster's own text back: dropped 2 lines")
    );
    assert!(kept_note(Origin::Conflict, &keep(3, 1)).contains("3 lines in place of 1"));
    assert!(
        kept_note(Origin::Undeclared, &keep(1, 1)).contains("ctrl-alt-r"),
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
    assert!(
        stale_object_note().contains("open the object again"),
        "a dry run about another object leaves the comparison's left side stale"
    );
}

// The rule that cost a bug in the editor: one bit cannot tell two questions
// apart, so a press meant for the force answers the apply.
#[test]
fn the_gate_arms_under_its_own_name_and_a_different_press_re_asks() {
    let mut gate = ApplyGate::default();
    assert_eq!(gate.step(Armed::Apply), Step::Ask);
    assert_eq!(gate.step(Armed::Force), Step::Ask, "a different question");
    assert_eq!(gate.step(Armed::Force), Step::Go);
    assert_eq!(
        gate.step(Armed::Force),
        Step::Ask,
        "firing disarms, so the next force asks again"
    );
    gate.disarm();
    assert_eq!(gate.step(Armed::Apply), Step::Ask);
    gate.disarm();
    assert_eq!(
        gate.step(Armed::Apply),
        Step::Ask,
        "a recompute makes the next press the first one again"
    );
}

#[test]
fn an_applied_note_says_whether_the_buffer_followed_the_write() {
    assert_eq!(
        applied_note(" (forced)", "", true),
        "applied as fieldManager k10s (forced)"
    );
    let moved = applied_note("", "; landed on another object", false);
    assert!(
        moved.ends_with("the buffer moved meanwhile, so it was left as it is"),
        "an edit made during the flight must be said to have survived: {moved}"
    );
    assert!(moved.contains("; landed on another object"));
}

#[test]
fn an_unrendered_real_apply_never_reads_as_a_write_that_did_not_happen() {
    let dry = unrendered_note(true, "", "answer too deep");
    assert!(
        dry.contains("nothing to review"),
        "a dry run that cannot render has nothing to confirm: {dry}"
    );
    assert!(!dry.contains("applied as"), "{dry}");

    let real = unrendered_note(false, "", "answer too deep");
    assert!(
        real.starts_with("applied as fieldManager k10s"),
        "the object is already stored, and the sentence must say so first: {real}"
    );
    assert!(real.contains("answer too deep"));
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
        summary: "3 hunks".to_string(),
        blocked: &[],
        kept: &[],
        armed,
        conflict,
        conflict_truncated: false,
        identity: Identity::Same,
        status: None,
        context: Some("prod-eu"),
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
fn a_write_that_answered_about_another_object_marks_the_line_stale() {
    let stale = status_line(Line {
        identity: identity(Some("uid-read"), Some("uid-answered")),
        ..line_at(&[], None)
    });
    assert!(stale.contains(stale_object_note()), "{stale}");
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
