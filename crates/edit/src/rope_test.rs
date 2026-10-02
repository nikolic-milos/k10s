//! The rope held against a `String` model, including random edits, so a
//! divergence is found rather than reasoned about. Chunk boundaries are where
//! the bugs live: a cluster split across two leaves is still one cluster.

use super::*;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};

fn sample(lines: usize) -> String {
    let mut text = String::new();
    for index in 0..lines {
        text.push_str(&format!(
            "line {index}: metadata, labels, annotations, café 🦀\n"
        ));
    }
    text
}

#[test]
fn an_empty_rope_has_one_empty_line() {
    let rope = Rope::default();
    assert_eq!(rope.len(), 0);
    assert_eq!(rope.len_lines(), 1);
    assert_eq!(rope.line(0), "");
    assert_eq!(rope.max_point(), Point::new(0, 0));
    assert_eq!(rope.to_string(), "");
}

#[test]
fn construction_round_trips_across_chunk_boundaries() {
    let text = sample(500);
    assert!(text.len() > MAX_CHUNK * 4, "the fixture must span chunks");
    let rope = Rope::from(text.as_str());
    assert_eq!(rope.to_string(), text);
    assert_eq!(rope.len(), text.len());
    assert_eq!(rope.len_lines(), 501);
}

#[test]
fn replace_matches_the_string_model_at_seams() {
    let text = sample(200);
    let insert = "REPLACED ✓";
    let seams = [
        0,
        1,
        TARGET_CHUNK - 1,
        TARGET_CHUNK,
        TARGET_CHUNK + 1,
        text.len() / 2,
        text.len() - 1,
        text.len(),
    ];
    for &start in &seams {
        for &end in &seams {
            if start > end {
                continue;
            }
            let mut model = text.clone();
            let start = nearest_boundary(&model, start);
            let end = nearest_boundary(&model, end).max(start);
            model.replace_range(start..end, insert);
            let mut rope = Rope::from(text.as_str());
            rope.replace(start..end, insert);
            assert_eq!(rope.to_string(), model, "splice {start}..{end}");
        }
    }
}

#[test]
#[should_panic(expected = "rope edits must be a range inside the rope")]
fn a_reversed_edit_range_is_refused_by_name() {
    // Not only under `debug_assertions`, and not as the char-boundary assert
    // a wrapped length reaches several frames deeper.
    let mut rope = Rope::from("alpha\nbeta\n");
    rope.replace(std::ops::Range { start: 6, end: 2 }, "x");
}

#[test]
#[should_panic(expected = "rope edits must be a range inside the rope")]
fn an_edit_range_past_the_end_is_refused_by_name() {
    let mut rope = Rope::from("alpha\nbeta\n");
    rope.replace(6..600, "x");
}

fn nearest_boundary(text: &str, mut offset: usize) -> usize {
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

#[test]
fn points_and_offsets_agree_with_a_scan() {
    let text = sample(64);
    let rope = Rope::from(text.as_str());
    let mut expected = Point::new(0, 0);
    for (offset, byte) in text.bytes().enumerate() {
        if text.is_char_boundary(offset) {
            assert_eq!(rope.byte_to_point(offset), expected, "offset {offset}");
            assert_eq!(rope.point_to_byte(expected), offset, "offset {offset}");
        }
        if byte == b'\n' {
            expected.row += 1;
            expected.column = 0;
        } else {
            expected.column += 1;
        }
    }
    assert_eq!(rope.byte_to_point(text.len()), expected);
}

#[test]
fn line_access_covers_first_middle_and_last() {
    let rope = Rope::from("alpha\nbeta\n\ngamma");
    assert_eq!(rope.len_lines(), 4);
    assert_eq!(rope.line(0), "alpha");
    assert_eq!(rope.line(1), "beta");
    assert_eq!(rope.line(2), "");
    assert_eq!(rope.line(3), "gamma");
    let trailing = Rope::from("alpha\n");
    assert_eq!(trailing.len_lines(), 2);
    assert_eq!(trailing.line(1), "");
}

#[test]
fn chunk_slices_reassemble_any_subrange() {
    let text = sample(300);
    let rope = Rope::from(text.as_str());
    let mut rng = SmallRng::seed_from_u64(7);
    for _ in 0..200 {
        let start = nearest_boundary(&text, rng.random_range(0..=text.len()));
        let end = nearest_boundary(&text, rng.random_range(start..=text.len()));
        assert_eq!(rope.slice_to_string(start..end), text[start..end]);
    }
}

#[test]
fn char_stepping_respects_multibyte_glyphs() {
    let rope = Rope::from("a🦀é\n");
    assert_eq!(rope.next_char_offset(0), 1);
    assert_eq!(rope.next_char_offset(1), 5);
    assert_eq!(rope.next_char_offset(5), 7);
    assert_eq!(rope.prev_char_offset(7), 5);
    assert_eq!(rope.prev_char_offset(5), 1);
    assert_eq!(rope.prev_char_offset(1), 0);
    assert_eq!(rope.char_at(1), Some('🦀'));
    assert!(!rope.is_char_boundary(2));
    assert_eq!(rope.snap_to_char_boundary(2), 1);
}

#[test]
fn random_edits_never_diverge_from_the_string_model() {
    let mut rng = SmallRng::seed_from_u64(k10s_seed());
    let mut model = sample(400);
    let mut rope = Rope::from(model.as_str());
    let inserts = ["", "x", "two\nlines", "🦀🦀🦀", &"y".repeat(3000)];
    for step in 0..2000 {
        let start = nearest_boundary(&model, rng.random_range(0..=model.len()));
        let end = nearest_boundary(
            &model,
            rng.random_range(start..=model.len().min(start + 200)),
        )
        .max(start);
        let insert = inserts[rng.random_range(0..inserts.len())];
        model.replace_range(start..end, insert);
        rope.replace(start..end, insert);
        assert_eq!(rope.len(), model.len(), "step {step}");
        if step % 250 == 0 {
            assert_eq!(rope.to_string(), model, "step {step}");
        }
    }
    assert_eq!(rope.to_string(), model);
    assert_eq!(
        rope.len_lines(),
        model.bytes().filter(|byte| *byte == b'\n').count() + 1
    );
    assert!(
        rope.depth() <= 12,
        "depth {} exploded after 2000 edits",
        rope.depth()
    );
    let row = rope.len_lines() / 2;
    let start = rope.line_start(row);
    let expected: String = model[start..]
        .split('\n')
        .next()
        .unwrap_or_default()
        .to_string();
    assert_eq!(rope.line(row), expected);
}

fn k10s_seed() -> u64 {
    0x6b31_3073
}

#[test]
fn grapheme_steps_move_over_clusters_not_scalars() {
    // A combining accent, a ZWJ family, a flag, and a CRLF pair: four
    // things the eye reads as one character and four multi-scalar
    // clusters.
    let text = "e\u{301}x\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}y\u{1F1E9}\u{1F1EA}z\r\nw";
    let rope = Rope::from(text);
    let mut offsets = vec![0usize];
    let mut at = 0;
    while at < rope.len() {
        at = rope.next_grapheme_offset(at);
        offsets.push(at);
    }
    let expected: Vec<usize> = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect();
    assert_eq!(offsets, expected, "forward steps land on every cluster");
    let mut backwards = vec![rope.len()];
    let mut at = rope.len();
    while at > 0 {
        at = rope.prev_grapheme_offset(at);
        backwards.push(at);
    }
    backwards.reverse();
    assert_eq!(backwards, expected, "and so do backward steps");
}

#[test]
fn a_cluster_split_across_two_leaves_is_still_one_cluster() {
    // Chunks never split a char but happily split a cluster, so the
    // boundary search has to cross leaves. The letter has to be the last
    // byte of a leaf for that to be what is tested: a cluster that happens
    // to sit inside one leaf proves only that the easy path works.
    let mut text = "a".repeat(TARGET_CHUNK - 1);
    let seam = text.len();
    text.push('e');
    text.push('\u{301}');
    text.push('b');
    text.push_str(&"a".repeat(TARGET_CHUNK * 2));
    let rope = Rope::from(text.as_str());
    assert!(rope.depth() > 0, "the fixture spans several leaves");
    assert_eq!(
        rope.chunk_bytes_from(seam).len(),
        1,
        "the letter ends a leaf"
    );
    assert_eq!(rope.char_at(seam), Some('e'));
    assert_eq!(
        rope.next_grapheme_offset(seam),
        seam + 3,
        "the accent rides with its letter across the leaf boundary"
    );
    assert_eq!(rope.prev_grapheme_offset(seam + 3), seam);
    assert_eq!(rope.snap_to_grapheme_boundary(seam + 1), seam);
}

#[test]
fn a_flag_split_across_two_leaves_still_needs_its_pre_context() {
    // Regional indicators pair up, so the segmenter has to be told what
    // precedes the chunk it is looking at -- and here what precedes it is a
    // different leaf.
    let mut text = "a".repeat(TARGET_CHUNK - 4);
    let seam = text.len();
    text.push('\u{1F1E9}');
    text.push('\u{1F1EA}');
    text.push_str(&"a".repeat(TARGET_CHUNK * 2));
    let rope = Rope::from(text.as_str());
    assert_eq!(
        rope.chunk_bytes_from(seam).len(),
        4,
        "the first indicator ends a leaf"
    );
    assert_eq!(
        rope.next_grapheme_offset(seam),
        seam + 8,
        "both halves of the flag move as one"
    );
    assert_eq!(rope.prev_grapheme_offset(seam + 8), seam);
}

#[test]
fn grapheme_columns_count_what_the_eye_counts() {
    let rope = Rope::from("e\u{301}\u{1F1E9}\u{1F1EA}x\nplain\n");
    let line_end = rope.line_len(0);
    assert_eq!(rope.grapheme_column(line_end), 3);
    assert_eq!(rope.offset_at_grapheme_column(0, 2), line_end - 1);
    assert_eq!(
        rope.offset_at_grapheme_column(1, 99),
        rope.line_start(1) + 5,
        "a goal column past the end clamps to the line end"
    );
}

#[test]
fn snapshots_share_structure_and_stay_isolated() {
    let text = sample(300);
    let mut rope = Rope::from(text.as_str());
    let snapshot = rope.clone();
    rope.replace(10..20, "MUTATED");
    assert_eq!(snapshot.to_string(), text);
    assert_ne!(rope.to_string(), text);
}
