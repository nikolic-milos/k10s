use crate::diff::*;
use rand::{RngExt, SeedableRng};

fn text_of(side: &str, rows: &[Row], want: Side) -> Vec<String> {
    rows.iter()
        .filter(|row| row.side == want)
        .map(|row| side[row.bytes()].to_string())
        .collect()
}

fn two_way(live: &str, buffer: &str) -> Diff {
    three_way(Sides {
        base: None,
        live,
        buffer,
    })
}

// An independently written reference: the textbook quadratic LCS, used only
// to state what the alignment is allowed to be. The alignment under test is
// patience-anchored and so is not required to reach this length -- but it
// may never exceed it, and on inputs with no repeated lines the two agree
// exactly, which is what makes the agreement worth asserting.
fn reference_lcs(left: &[u32], right: &[u32]) -> usize {
    let mut table = vec![vec![0usize; right.len() + 1]; left.len() + 1];
    for (row, l) in left.iter().enumerate() {
        for (column, r) in right.iter().enumerate() {
            table[row + 1][column + 1] = if l == r {
                table[row][column] + 1
            } else {
                table[row][column + 1].max(table[row + 1][column])
            };
        }
    }
    table[left.len()][right.len()]
}

fn aligned(left: &[u32], right: &[u32]) -> (Vec<(u32, u32)>, bool) {
    let mut budget = MAX_TOTAL_CELLS;
    align(left, right, &mut budget)
}

fn assert_valid(left: &[u32], right: &[u32], matched: &[(u32, u32)]) {
    let mut last: Option<(u32, u32)> = None;
    for pair in matched {
        assert_eq!(
            left[pair.0 as usize], right[pair.1 as usize],
            "a matched pair must be the same line: {pair:?}"
        );
        if let Some(previous) = last {
            assert!(
                pair.0 > previous.0 && pair.1 > previous.1,
                "matches must ascend in both coordinates: {previous:?} then {pair:?}"
            );
        }
        last = Some(*pair);
    }
    assert!(
        matched.len() <= reference_lcs(left, right),
        "an alignment cannot match more lines than the longest common subsequence has"
    );
}

#[test]
fn splitting_lines_keeps_the_last_one_and_invents_none() {
    assert_eq!(split_lines("").unwrap().len(), 0);
    assert_eq!(split_lines("a\nb\n").unwrap().len(), 2);
    assert_eq!(split_lines("a\nb").unwrap().len(), 2);
    assert_eq!(split_lines("\n").unwrap().len(), 1);
    assert_eq!(split_lines("\n\n").unwrap().len(), 2);
    let text = "alpha\nbeta";
    let lines = split_lines(text).unwrap();
    assert_eq!(&text[lines[0].range()], "alpha");
    assert_eq!(&text[lines[1].range()], "beta");
}

#[test]
fn crlf_is_a_terminator_not_invisible_row_content() {
    let text = "alpha\r\nemoji: 👩🏽‍💻\r\n";
    let lines = split_lines(text).unwrap();
    assert_eq!(&text[lines[0].range()], "alpha");
    assert_eq!(&text[lines[1].range()], "emoji: 👩🏽‍💻");

    let diff = two_way(text, "alpha\nemoji: 👩🏽‍💻\n");
    assert_eq!(
        diff.verdict(),
        Verdict::Agreed,
        "line-ending spelling is not YAML content"
    );
    assert!(!diff.final_newline_differs);
}

#[test]
fn an_identical_pair_matches_every_line() {
    let ids: Vec<u32> = (0..64).collect();
    let (matched, coarse) = aligned(&ids, &ids);
    assert!(!coarse);
    assert_eq!(matched.len(), 64);
    assert_valid(&ids, &ids, &matched);
}

#[test]
fn alignment_is_exact_when_no_line_repeats() {
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0x5eed);
    for _ in 0..400 {
        let left: Vec<u32> = (0..rng.random_range(0..40u32)).collect();
        // A subsequence of a permutation of distinct lines: every line is
        // unique on both sides, so patience anchoring is the exact answer.
        let mut right: Vec<u32> = left
            .iter()
            .copied()
            .filter(|_| rng.random_bool(0.7))
            .collect();
        for _ in 0..rng.random_range(0..6u32) {
            let at = rng.random_range(0..=right.len());
            right.insert(at, rng.random_range(1000..2000u32));
        }
        let (matched, coarse) = aligned(&left, &right);
        assert!(!coarse);
        assert_valid(&left, &right, &matched);
        assert_eq!(
            matched.len(),
            reference_lcs(&left, &right),
            "distinct lines leave nothing for anchoring to get wrong: {left:?} {right:?}"
        );
    }
}

#[test]
fn alignment_stays_valid_when_lines_repeat_heavily() {
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0xd1ff);
    for _ in 0..400 {
        let alphabet = rng.random_range(1..4u32);
        let left: Vec<u32> = (0..rng.random_range(0..40u32))
            .map(|_| rng.random_range(0..alphabet))
            .collect();
        let right: Vec<u32> = (0..rng.random_range(0..40u32))
            .map(|_| rng.random_range(0..alphabet))
            .collect();
        let (matched, coarse) = aligned(&left, &right);
        assert!(!coarse, "a forty-line gap fits the budget");
        assert_valid(&left, &right, &matched);
    }
}

#[test]
fn hirschberg_is_exact_in_both_asymmetric_orientations() {
    for left_len in 0..=7usize {
        for right_len in 0..=7usize {
            let combinations = 1usize << (left_len + right_len);
            for bits in 0..combinations {
                let left: Vec<u32> = (0..left_len).map(|at| ((bits >> at) & 1) as u32).collect();
                let right: Vec<u32> = (0..right_len)
                    .map(|at| ((bits >> (left_len + at)) & 1) as u32)
                    .collect();
                let mut matched = Vec::new();
                hirschberg(&left, &right, 0, 0, &mut matched);
                assert_valid(&left, &right, &matched);
                assert_eq!(matched.len(), reference_lcs(&left, &right));
            }
        }
    }
}

#[test]
fn a_repeating_gap_with_no_anchor_still_reaches_the_exact_answer() {
    // Two lines, alternating: nothing is unique, so the whole span falls
    // through anchoring into the Hirschberg path.
    let left: Vec<u32> = (0..40).map(|at| at % 2).collect();
    let right: Vec<u32> = (0..30).map(|at| (at + 1) % 2).collect();
    let (matched, coarse) = aligned(&left, &right);
    assert!(!coarse);
    assert_valid(&left, &right, &matched);
    assert_eq!(matched.len(), reference_lcs(&left, &right));
}

#[test]
fn an_unalignable_span_past_the_budget_is_coarse_rather_than_slow() {
    // No line is unique and no prefix or suffix is shared, so the span
    // reaches the quadratic path with more cells than the ceiling allows.
    let side = 4096usize;
    let left: Vec<u32> = (0..side).map(|at| (at % 2) as u32).collect();
    let right: Vec<u32> = (0..side).map(|at| ((at + 1) % 2) as u32).collect();
    let mut budget = MAX_TOTAL_CELLS;
    let (matched, coarse) = align(&left, &right, &mut budget);
    assert!(coarse, "{} cells is past the ceiling", side * side);
    assert!(matched.is_empty(), "a coarse span matches nothing");
}

#[test]
fn identical_documents_produce_one_common_hunk_and_no_change() {
    let text = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n";
    let diff = two_way(text, text);
    assert_eq!(diff.verdict(), Verdict::Agreed);
    assert_eq!(diff.hunks.len(), 1);
    assert_eq!(diff.hunks[0].origin, Origin::Common);
    assert_eq!(diff.rows.len(), 4);
    assert!(diff.two_way);
    assert!(!diff.coarse);
    assert!(!diff.final_newline_differs);
    assert_eq!(diff.refused, None);
}

#[test]
fn a_two_way_diff_calls_every_difference_the_users_own() {
    let live = "kind: Pod\nreplicas: 1\nname: web\n";
    let buffer = "kind: Pod\nreplicas: 3\nname: web\n";
    let diff = two_way(live, buffer);
    assert!(diff.two_way);
    assert_eq!(diff.counts.mine, 1);
    assert_eq!(diff.counts.theirs, 0);
    assert_eq!(diff.counts.conflict, 0);
    assert_eq!(diff.counts.added, 1);
    assert_eq!(diff.counts.removed, 1);
    assert_eq!(
        text_of(live, &diff.rows, Side::Live),
        vec!["kind: Pod", "replicas: 1", "name: web"]
    );
    assert_eq!(
        text_of(buffer, &diff.rows, Side::Buffer),
        vec!["replicas: 3"]
    );
}

#[test]
fn the_three_classifications_come_out_of_who_moved() {
    let base = "a\nmine\nb\ntheirs\nc\nboth\nz\n";
    let live = "a\nmine\nb\ntheirs-changed\nc\nboth-cluster\nz\n";
    let buffer = "a\nmine-changed\nb\ntheirs\nc\nboth-user\nz\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer,
    });
    assert!(!diff.two_way);
    assert_eq!(diff.counts.mine, 1, "only the buffer moved line two");
    assert_eq!(diff.counts.theirs, 1, "only the cluster moved line three");
    assert_eq!(diff.counts.conflict, 1, "both moved line four");
    let conflict: Vec<Side> = diff
        .rows
        .iter()
        .filter(|row| row.origin == Origin::Conflict)
        .map(|row| row.side)
        .collect();
    assert_eq!(
        conflict,
        vec![Side::Live, Side::Base, Side::Buffer],
        "a conflict shows all three, in that order"
    );
}

// Two changes on alternating sides with no base line surviving in both
// between them cannot be told apart: a stable point needs agreement on both
// sides at once, and there is none. Reporting one conflict over the whole
// span is what diff3 does and is the honest answer -- claiming "yours" and
// "theirs" separately would assert an alignment nothing established.
#[test]
fn adjacent_changes_with_nothing_stable_between_them_are_one_conflict() {
    let base = "a\nmine\ntheirs\nz\n";
    let live = "a\nmine\ntheirs-changed\nz\n";
    let buffer = "a\nmine-changed\ntheirs\nz\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer,
    });
    assert_eq!(diff.counts.conflict, 1);
    assert_eq!(diff.counts.mine, 0);
    assert_eq!(diff.counts.theirs, 0);
    assert_eq!(
        text_of(base, &diff.rows, Side::Base),
        vec!["mine", "theirs"],
        "the conflict carries the whole span of base it covers"
    );
}

// Both sides hold something the last apply never declared. It is a conflict
// by the letter of the classification and reads as a refusal the server has
// not made: the base is usually silent because the value was defaulted, not
// because anybody claimed it.
#[test]
fn a_region_the_base_never_declared_is_not_called_a_conflict() {
    let base = "kind: Pod\nz: 1\n";
    let live = "kind: Pod\nimagePullPolicy: Always\nz: 1\n";
    let buffer = "kind: Pod\nimagePullPolicy: IfNotPresent\nz: 1\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer,
    });
    assert_eq!(diff.counts.undeclared, 1);
    assert_eq!(diff.counts.conflict, 0, "no refusal is being promised");
    assert_eq!(diff.counts.mine, 0);
    assert_eq!(diff.counts.theirs, 0);
    assert_eq!(diff.verdict(), Verdict::Differs);
    let origins: Vec<Origin> = diff.hunks.iter().map(|hunk| hunk.origin).collect();
    assert_eq!(
        origins,
        vec![Origin::Common, Origin::Undeclared, Origin::Common]
    );
    assert_eq!(
        text_of(base, &diff.rows, Side::Base),
        Vec::<String>::new(),
        "there is no base row to show: the base is what is empty"
    );

    // And the shape a reviewer found: the buffer deletes the defaulted line
    // instead of retyping it, so only the cluster holds text here. That used
    // to be `Theirs` -- "the cluster changed this; applying reverts it" --
    // which is the same false promise one arm over: an apply does not revert
    // a field this client never declared.
    let deleted = "kind: Pod\nz: 1\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer: deleted,
    });
    assert_eq!(diff.counts.undeclared, 1);
    assert_eq!(
        diff.counts.theirs, 0,
        "nothing here is drift an apply would revert"
    );
    assert_eq!(
        diff.hunks
            .iter()
            .map(|hunk| hunk.origin)
            .collect::<Vec<Origin>>(),
        vec![Origin::Common, Origin::Undeclared, Origin::Common]
    );

    // A base that declared it and a cluster that moved it *is* drift.
    let declared_and_moved = "kind: Pod\nimagePullPolicy: Never\nz: 1\n";
    let diff = three_way(Sides {
        base: Some(declared_and_moved),
        live,
        buffer: declared_and_moved,
    });
    assert_eq!(diff.counts.theirs, 1);
    assert_eq!(diff.counts.undeclared, 0);

    // And a base that *did* declare something is still a conflict.
    let declared = "kind: Pod\nimagePullPolicy: Never\nz: 1\n";
    let diff = three_way(Sides {
        base: Some(declared),
        live,
        buffer,
    });
    assert_eq!(diff.counts.conflict, 1);
    assert_eq!(diff.counts.undeclared, 0);
}

#[test]
fn a_change_the_buffer_already_agrees_with_is_not_a_change() {
    let base = "a\nold\nz\n";
    let live = "a\nnew\nz\n";
    let buffer = "a\nnew\nz\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer,
    });
    assert_eq!(
        diff.verdict(),
        Verdict::Agreed,
        "the buffer holds what the cluster holds, so an apply changes nothing"
    );
}

#[test]
fn a_theirs_hunk_is_what_an_apply_would_revert() {
    let base = "replicas: 1\nimage: nginx\n";
    let live = "replicas: 5\nimage: nginx\n";
    let buffer = "replicas: 1\nimage: nginx\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer,
    });
    assert_eq!(diff.counts.theirs, 1);
    assert_eq!(diff.counts.mine, 0);
    assert_eq!(text_of(live, &diff.rows, Side::Live)[0], "replicas: 5");
    assert_eq!(text_of(buffer, &diff.rows, Side::Buffer)[0], "replicas: 1");
}

// Both sides deleted what the base declared. That is agreement, and the one
// region in the alignment with no row on any side a reader can be shown:
// building hunks eagerly rather than grouping finished rows put an empty
// hunk in the list here, which every consumer counts as a change.
#[test]
fn a_region_both_sides_removed_is_agreement_with_no_hunk_of_its_own() {
    let base = "a\ngone\nz\n";
    let live = "a\nz\n";
    let buffer = "a\nz\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer,
    });
    assert_eq!(diff.verdict(), Verdict::Agreed);
    assert_eq!(diff.rows.len(), 2);
    let origins: Vec<Origin> = diff.hunks.iter().map(|hunk| hunk.origin).collect();
    assert_eq!(origins, vec![Origin::Common]);
    for hunk in &diff.hunks {
        assert!(!hunk.rows.is_empty(), "a hunk is never empty");
    }
}

#[test]
fn insertions_and_deletions_carry_only_the_side_that_has_them() {
    let live = "a\nb\nc\n";
    let buffer = "a\nb\nb2\nc\n";
    let diff = two_way(live, buffer);
    assert_eq!(diff.counts.added, 1);
    assert_eq!(diff.counts.removed, 0);
    let inserted: Vec<String> = diff
        .rows
        .iter()
        .filter(|row| row.origin == Origin::Mine)
        .map(|row| buffer[row.bytes()].to_string())
        .collect();
    assert_eq!(inserted, vec!["b2"]);

    let diff = two_way(buffer, live);
    assert_eq!(diff.counts.added, 0);
    assert_eq!(diff.counts.removed, 1);
}

#[test]
fn every_line_of_both_sides_appears_in_order() {
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0xfeed);
    for _ in 0..300 {
        let mut live = String::new();
        for _ in 0..rng.random_range(0..30u32) {
            live.push_str(&format!("line-{}\n", rng.random_range(0..12u32)));
        }
        let mut buffer = String::new();
        for _ in 0..rng.random_range(0..30u32) {
            buffer.push_str(&format!("line-{}\n", rng.random_range(0..12u32)));
        }
        let diff = two_way(&live, &buffer);
        let live_rows = text_of(&live, &diff.rows, Side::Live);
        let buffer_side = text_of(&buffer, &diff.rows, Side::Buffer);
        let mut whole_buffer: Vec<String> = Vec::new();
        for row in &diff.rows {
            match row.side {
                Side::Live if row.origin == Origin::Common => {
                    whole_buffer.push(live[row.bytes()].to_string());
                }
                Side::Buffer => whole_buffer.push(buffer[row.bytes()].to_string()),
                _ => {}
            }
        }
        assert_eq!(
            live_rows,
            live.lines().map(str::to_string).collect::<Vec<String>>(),
            "the live side renders whole and in order"
        );
        assert_eq!(
            whole_buffer,
            buffer.lines().map(str::to_string).collect::<Vec<String>>(),
            "common rows plus buffer rows reconstruct the buffer"
        );
        assert!(buffer_side.len() <= buffer.lines().count());
    }
}

#[test]
fn a_three_way_diff_reconstructs_all_three_documents() {
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0x3a11);
    for _ in 0..300 {
        let document = |rng: &mut rand_chacha::ChaCha8Rng| {
            let mut text = String::new();
            for _ in 0..rng.random_range(0..24u32) {
                text.push_str(&format!("k{}: v\n", rng.random_range(0..10u32)));
            }
            text
        };
        let base = document(&mut rng);
        let live = document(&mut rng);
        let buffer = document(&mut rng);
        let diff = three_way(Sides {
            base: Some(&base),
            live: &live,
            buffer: &buffer,
        });
        let mut whole_live: Vec<String> = Vec::new();
        let mut whole_buffer: Vec<String> = Vec::new();
        for row in &diff.rows {
            match row.side {
                Side::Live => {
                    whole_live.push(live[row.bytes()].to_string());
                    if row.origin == Origin::Common {
                        whole_buffer.push(live[row.bytes()].to_string());
                    }
                }
                Side::Buffer => whole_buffer.push(buffer[row.bytes()].to_string()),
                Side::Base => {}
            }
        }
        assert_eq!(
            whole_live,
            live.lines().map(str::to_string).collect::<Vec<String>>()
        );
        assert_eq!(
            whole_buffer,
            buffer.lines().map(str::to_string).collect::<Vec<String>>()
        );
        for hunk in &diff.hunks {
            assert!(!hunk.rows.is_empty(), "a hunk is never empty");
            assert!(hunk.rows.end <= diff.rows.len());
        }
    }
}

// Next-change navigation is the shell's, in cell coordinates a fold can
// move; what this layer owes it is an ordered hunk list with the common
// runs still labelled, so a second implementation here would be a second
// opinion nobody consults.
#[test]
fn hunks_alternate_between_common_runs_and_changes_in_document_order() {
    let diff = two_way("a\nb\nc\nd\ne\n", "a\nB\nc\nd\nE\n");
    let origins: Vec<Origin> = diff.hunks.iter().map(|hunk| hunk.origin).collect();
    assert_eq!(
        origins,
        vec![Origin::Common, Origin::Mine, Origin::Common, Origin::Mine]
    );
    let mut at = 0;
    for hunk in &diff.hunks {
        assert_eq!(hunk.rows.start, at, "the hunks tile the rows in order");
        at = hunk.rows.end;
    }
    assert_eq!(at, diff.rows.len());
}

#[test]
fn an_empty_side_is_a_whole_document_of_one_kind() {
    let diff = two_way("", "a\nb\n");
    assert_eq!(diff.counts.added, 2);
    assert_eq!(diff.counts.removed, 0);
    assert_eq!(diff.counts.mine, 1);

    let diff = two_way("a\nb\n", "");
    assert_eq!(diff.counts.added, 0);
    assert_eq!(diff.counts.removed, 2);

    let diff = two_way("", "");
    assert!(diff.rows.is_empty());
    assert!(diff.hunks.is_empty());
    assert_eq!(diff.verdict(), Verdict::Agreed);
}

#[test]
fn a_missing_final_newline_is_reported_rather_than_invisible() {
    let diff = two_way("a\nb\n", "a\nb");
    assert_eq!(
        diff.verdict(),
        Verdict::Agreed,
        "a line-oriented diff sees the same two lines"
    );
    assert!(diff.final_newline_differs);

    let diff = two_way("a\nb\n", "a\nb\n");
    assert!(!diff.final_newline_differs);

    let diff = two_way("", "a\n");
    assert!(
        !diff.final_newline_differs,
        "an empty document has no last byte to disagree about"
    );
}

#[test]
fn an_oversized_side_is_refused_rather_than_truncated() {
    let big = "x".repeat(MAX_SIDE_BYTES + 1);
    let diff = two_way(&big, "a\n");
    assert_eq!(
        diff.refused,
        Some("one side of this comparison is larger than the 8 MiB the diff aligns")
    );
    assert!(diff.rows.is_empty());
    assert!(diff.two_way, "refusal preserves the comparison's mode");
}

// A refusal zeroes every count, so anything reading the counts alone reads
// it as agreement -- which is how the write path came to tell a user that
// applying a document it had never compared would change nothing.
#[test]
fn a_refusal_is_not_agreement() {
    let big = "x".repeat(MAX_SIDE_BYTES + 1);
    let diff = two_way(&big, "a\n");
    assert_eq!(diff.counts, Counts::default(), "nothing counted anything");
    assert_eq!(
        diff.verdict(),
        Verdict::Refused("one side of this comparison is larger than the 8 MiB the diff aligns")
    );

    assert_eq!(two_way("a\n", "a\n").verdict(), Verdict::Agreed);
    assert_eq!(two_way("a\n", "b\n").verdict(), Verdict::Differs);
}

// The size the module doc states, which is the whole reason a row is a byte
// range into its own side rather than a copied line.
#[test]
fn a_row_is_twelve_bytes() {
    assert_eq!(std::mem::size_of::<Row>(), 12);
}

#[test]
fn a_newline_dense_side_is_refused_before_it_can_expand_into_millions_of_rows() {
    let dense = "\n".repeat(MAX_SIDE_LINES + 1);
    let diff = two_way(&dense, &dense);
    assert_eq!(
        diff.refused,
        Some("one side of this comparison has more than 65,536 lines")
    );
    assert!(diff.rows.is_empty());
}

#[test]
fn a_one_line_change_in_a_large_document_stays_a_one_line_hunk() {
    let mut live = String::new();
    for at in 0..20_000 {
        live.push_str(&format!("  - name: worker-{at}\n"));
    }
    let buffer = live.replacen("worker-9999", "worker-changed", 1);
    let diff = two_way(&live, &buffer);
    assert_eq!(diff.counts.mine, 1);
    assert_eq!(diff.counts.added, 1);
    assert_eq!(diff.counts.removed, 1);
    assert!(!diff.coarse);
    assert_eq!(diff.rows.len(), 20_001);
}

fn kept(diff: &Diff, hunk: usize, live: &str, buffer: &str) -> String {
    let keep = keep_theirs(diff, hunk, live, buffer).expect("this hunk has a side to keep");
    let mut text = buffer.to_string();
    text.replace_range(keep.range, &keep.text);
    text
}

// Every hunk, taken from the last one backwards so that no edit moves the
// range of one not yet applied. Taking every side the cluster has is the
// whole cluster: the strongest statement available about the ranges being
// right, since one byte out anywhere shows up as a document that is not the
// live one.
fn kept_everywhere(diff: &Diff, live: &str, buffer: &str) -> String {
    let mut text = buffer.to_string();
    for at in (0..diff.hunks.len()).rev() {
        if let Some(keep) = keep_theirs(diff, at, live, buffer) {
            text.replace_range(keep.range, &keep.text);
        }
    }
    text
}

#[test]
fn keeping_the_clusters_side_of_a_drift_hunk_puts_its_lines_in_the_buffer() {
    let base = "replicas: 1\nimage: nginx\n";
    let live = "replicas: 5\nimage: nginx\n";
    let buffer = "replicas: 1\nimage: nginx\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer,
    });
    let drift = diff
        .hunks
        .iter()
        .position(|hunk| hunk.origin == Origin::Theirs)
        .expect("the cluster moved a line");
    let keep = keep_theirs(&diff, drift, live, buffer).expect("it can be kept");
    assert_eq!(keep.taken, 1);
    assert_eq!(keep.dropped, 1);
    assert_eq!(kept(&diff, drift, live, buffer), live);
}

#[test]
fn a_hunk_the_three_documents_agree_about_has_no_side_to_keep() {
    let text = "a\nb\n";
    let diff = two_way(text, text);
    assert_eq!(diff.hunks[0].origin, Origin::Common);
    assert_eq!(keep_theirs(&diff, 0, text, text), None);
    assert_eq!(keep_theirs(&diff, 99, text, text), None, "no such hunk");

    let big = "x".repeat(MAX_SIDE_BYTES + 1);
    let refused = two_way(&big, "a\n");
    assert!(refused.hunks.is_empty());
    assert_eq!(
        keep_theirs(&refused, 0, &big, "a\n"),
        None,
        "a comparison that was never made has nothing to act on"
    );
}

// The buffer has no lines in this hunk at all, so the edit is an insertion
// and the only thing that says where is the hunk's own span.
#[test]
fn keeping_lines_the_buffer_does_not_have_inserts_them_where_they_belong() {
    let live = "a\nb\nc\n";
    let buffer = "a\nc\n";
    let diff = two_way(live, buffer);
    let hunk = diff
        .hunks
        .iter()
        .position(|hunk| hunk.origin != Origin::Common)
        .expect("a line is missing from the buffer");
    let keep = keep_theirs(&diff, hunk, live, buffer).expect("it can be kept");
    assert_eq!(keep.taken, 1);
    assert_eq!(keep.dropped, 0);
    assert!(keep.range.is_empty(), "nothing is replaced, only inserted");
    assert_eq!(keep.text, "b\n");
    assert_eq!(kept(&diff, hunk, live, buffer), live);
}

#[test]
fn keeping_a_side_the_cluster_does_not_have_takes_the_line_terminator_too() {
    let live = "a\nc\n";
    let buffer = "a\nb\nc\n";
    let diff = two_way(live, buffer);
    let hunk = diff
        .hunks
        .iter()
        .position(|hunk| hunk.origin != Origin::Common)
        .expect("the buffer has a line the cluster does not");
    let keep = keep_theirs(&diff, hunk, live, buffer).expect("it can be kept");
    assert_eq!(keep.taken, 0);
    assert_eq!(keep.dropped, 1);
    assert_eq!(keep.text, "");
    assert_eq!(
        kept(&diff, hunk, live, buffer),
        live,
        "the blank line the text sat on goes with it"
    );
}

// A blank line's bytes are empty, so a run of one spans zero bytes at a real
// position -- the same encoding as "the buffer has no lines here". Reading the
// span instead of asking whether the hunk has buffer rows appended a
// terminator the blank line already had and left the blank line behind.
#[test]
fn keeping_a_side_over_a_blank_buffer_line_replaces_it_rather_than_pushing_it_down() {
    for (live, buffer) in [
        ("a\nX\nb\n", "a\n\nb\n"),
        ("X\na\n", "\na\n"),
        ("a\nX\n", "a\n\n"),
        ("a\nX\nY\nb\n", "a\n\n\nb\n"),
    ] {
        let diff = two_way(live, buffer);
        assert_eq!(
            kept_everywhere(&diff, live, buffer),
            live,
            "live {live:?} buffer {buffer:?}"
        );
    }
}

#[test]
fn an_insertion_at_the_end_of_a_document_with_no_final_newline_gets_one_first() {
    let live = "a\nb\n";
    let buffer = "a";
    let diff = two_way(live, buffer);
    let hunk = diff
        .hunks
        .iter()
        .position(|hunk| hunk.origin != Origin::Common)
        .expect("the buffer is missing a line");
    assert_eq!(kept(&diff, hunk, live, buffer), "a\nb\n");

    // And an empty buffer has no dangling line to terminate.
    let diff = two_way(live, "");
    assert_eq!(kept(&diff, 0, live, ""), live);
}

// Line-ending spelling is not content and the diff never reports it, so an
// edit must not smuggle one side's spelling into the other's document.
#[test]
fn an_edit_spells_its_line_endings_the_way_the_buffer_around_it_does() {
    let live = "a\nb\nc\n";
    let buffer = "a\r\nc\r\n";
    let diff = two_way(live, buffer);
    let hunk = diff
        .hunks
        .iter()
        .position(|hunk| hunk.origin != Origin::Common)
        .expect("a line is missing from the buffer");
    assert_eq!(
        keep_theirs(&diff, hunk, live, buffer).unwrap().text,
        "b\r\n"
    );
    assert_eq!(kept(&diff, hunk, live, buffer), "a\r\nb\r\nc\r\n");

    // And a CRLF line that goes takes both of its bytes with it.
    let live = "a\r\n";
    let buffer = "a\r\nb\r\n";
    let diff = two_way(live, buffer);
    let hunk = diff
        .hunks
        .iter()
        .position(|hunk| hunk.origin != Origin::Common)
        .expect("the buffer has a line the cluster does not");
    assert_eq!(kept(&diff, hunk, live, buffer), "a\r\n");
}

// A two-way comparison calls every difference the user's own, and keeping the
// cluster's side of one is how an edit is put back.
#[test]
fn keeping_the_clusters_side_of_the_users_own_edit_reverts_it() {
    let live = "replicas: 1\n";
    let buffer = "replicas: 9\n";
    let diff = two_way(live, buffer);
    assert_eq!(diff.hunks[0].origin, Origin::Mine);
    assert_eq!(kept(&diff, 0, live, buffer), live);
}

#[test]
fn keeping_the_clusters_side_of_an_undeclared_hunk_takes_the_defaulted_value() {
    let base = "kind: Pod\nz: 1\n";
    let live = "kind: Pod\nimagePullPolicy: Always\nz: 1\n";
    let buffer = "kind: Pod\nimagePullPolicy: IfNotPresent\nz: 1\n";
    let diff = three_way(Sides {
        base: Some(base),
        live,
        buffer,
    });
    let hunk = diff
        .hunks
        .iter()
        .position(|hunk| hunk.origin == Origin::Undeclared)
        .expect("neither side declared this");
    assert_eq!(kept(&diff, hunk, live, buffer), live);
}

// The hunks' buffer spans are ordered and disjoint, which is what makes
// applying several of them one at a time safe.
#[test]
fn hunk_spans_march_forward_through_the_buffer_without_overlapping() {
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0x5a11);
    for _ in 0..300 {
        let document = |rng: &mut rand_chacha::ChaCha8Rng| {
            let mut text = String::new();
            for _ in 0..rng.random_range(0..24u32) {
                match rng.random_range(0..10u32) {
                    // A blank line is a line whose bytes are empty, and its
                    // absence from these fixtures is why an aliased empty span
                    // went unnoticed.
                    0 => text.push('\n'),
                    key => text.push_str(&format!("k{key}: v\n")),
                }
            }
            text
        };
        let base = document(&mut rng);
        let live = document(&mut rng);
        let buffer = document(&mut rng);
        let diff = three_way(Sides {
            base: Some(&base),
            live: &live,
            buffer: &buffer,
        });
        let mut at = 0usize;
        for hunk in &diff.hunks {
            let span = hunk.buffer();
            assert!(span.start <= span.end, "a span is never inverted");
            assert!(
                span.start >= at,
                "spans ascend: {span:?} after {at} in {buffer:?}"
            );
            assert!(span.end <= buffer.len(), "a span stays inside the buffer");
            assert!(
                buffer.is_char_boundary(span.start) && buffer.is_char_boundary(span.end),
                "a span is spliceable"
            );
            at = span.end;
        }
    }
}

// One byte out anywhere in the spans and this is not the live document.
#[test]
fn keeping_every_hunk_makes_the_buffer_the_clusters_document() {
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0xbeef);
    for _ in 0..600 {
        let document = |rng: &mut rand_chacha::ChaCha8Rng| {
            let mut text = String::new();
            for _ in 0..rng.random_range(0..20u32) {
                match rng.random_range(0..8u32) {
                    0 => text.push('\n'),
                    key => text.push_str(&format!("k{key}: v\n")),
                }
            }
            text
        };
        let base = document(&mut rng);
        let live = document(&mut rng);
        let buffer = document(&mut rng);
        let diff = three_way(Sides {
            base: Some(&base),
            live: &live,
            buffer: &buffer,
        });
        assert_eq!(
            kept_everywhere(&diff, &live, &buffer),
            live,
            "base {base:?} live {live:?} buffer {buffer:?}"
        );
    }
}

#[test]
fn a_moved_block_aligns_on_its_unique_lines() {
    let live = "head\nalpha\nbeta\ngamma\ntail\n";
    let buffer = "head\ngamma\nalpha\nbeta\ntail\n";
    let diff = two_way(live, buffer);
    assert_eq!(diff.verdict(), Verdict::Differs);
    let common: Vec<String> = diff
        .rows
        .iter()
        .filter(|row| row.origin == Origin::Common)
        .map(|row| live[row.bytes()].to_string())
        .collect();
    assert!(
        common.contains(&"alpha".to_string()) && common.contains(&"beta".to_string()),
        "the block that did not move stays common: {common:?}"
    );
}
