//! Editing with many cursors, and the grapheme cluster as the unit that moves,
//! deletes and counts as one character. Undo restores selections with the
//! text, and a coalesced typing run breaks the moment the cursor moves.

use super::*;

fn buffer(text: &str) -> Buffer {
    Buffer::new(text)
}

#[test]
fn typing_at_two_cursors_lands_both_and_coalesces_one_undo_step() {
    let mut buffer = buffer("aa\nbb\n");
    buffer.set_selections(vec![Selection::caret(0), Selection::caret(3)], 0);
    buffer.insert("x");
    buffer.insert("y");
    assert_eq!(buffer.text(), "xyaa\nxybb\n");
    assert_eq!(
        buffer.selections(),
        &[Selection::caret(2), Selection::caret(7)]
    );
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "aa\nbb\n", "typing coalesced into one step");
    assert!(buffer.redo());
    assert_eq!(buffer.text(), "xyaa\nxybb\n");
}

#[test]
fn moving_the_cursor_breaks_the_typing_coalescence() {
    let mut buffer = buffer("");
    buffer.insert("a");
    buffer.move_cursors(Motion::Left, false);
    buffer.move_cursors(Motion::Right, false);
    buffer.insert("b");
    assert_eq!(buffer.text(), "ab");
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "a", "movement started a new undo group");
}

#[test]
fn a_replacement_selection_types_over_its_range() {
    let mut buffer = buffer("hello world");
    buffer.set_selections(vec![Selection::range(0, 5)], 0);
    buffer.insert("goodbye");
    assert_eq!(buffer.text(), "goodbye world");
    assert_eq!(buffer.selections(), &[Selection::caret(7)]);
}

#[test]
fn splices_report_tree_sitter_coordinates_in_application_order() {
    let mut buffer = buffer("aa\nbb\n");
    buffer.set_selections(vec![Selection::caret(0), Selection::caret(3)], 0);
    let splices = buffer.insert("x");
    assert_eq!(splices.len(), 2);
    assert_eq!(splices[0].start, 3, "the later edit applies first");
    assert_eq!(splices[0].start_point, Point::new(1, 0));
    assert_eq!(splices[0].new_end, 4);
    assert_eq!(splices[1].start, 0);
    assert_eq!(splices[1].new_end_point, Point::new(0, 1));
}

#[test]
fn newline_copies_indentation_and_deepens_after_a_colon() {
    let mut buffer = buffer("spec:");
    buffer.set_selections(vec![Selection::caret(5)], 0);
    buffer.newline();
    assert_eq!(buffer.text(), "spec:\n  ");
    buffer.insert("containers:");
    buffer.newline();
    assert_eq!(buffer.text(), "spec:\n  containers:\n    ");
}

#[test]
fn backspace_in_leading_whitespace_releases_one_indent_stop() {
    let mut buffer = buffer("    name: x");
    buffer.set_selections(vec![Selection::caret(4)], 0);
    buffer.backspace();
    assert_eq!(buffer.text(), "  name: x");
    buffer.backspace();
    assert_eq!(buffer.text(), "name: x");
    buffer.backspace();
    assert_eq!(buffer.text(), "name: x", "nothing left of column zero");
}

#[test]
fn indent_and_outdent_cover_every_selected_row_once() {
    let mut buffer = buffer("a:\nb:\nc:\n");
    buffer.set_selections(vec![Selection::range(0, 6)], 0);
    buffer.indent();
    assert_eq!(buffer.text(), "  a:\n  b:\nc:\n");
    buffer.outdent();
    assert_eq!(buffer.text(), "a:\nb:\nc:\n");
    buffer.outdent();
    assert_eq!(buffer.text(), "a:\nb:\nc:\n", "outdent stops at the margin");
}

#[test]
fn vertical_motion_keeps_the_goal_column_across_short_lines() {
    let mut buffer = buffer("longer line\nab\nanother long line\n");
    buffer.set_selections(vec![Selection::caret(8)], 0);
    buffer.move_cursors(Motion::Down, false);
    assert_eq!(buffer.primary_selection().head, 14, "clamped to short line");
    buffer.move_cursors(Motion::Down, false);
    let point = buffer.rope().byte_to_point(buffer.primary_selection().head);
    assert_eq!(point, Point::new(2, 8), "the goal column is remembered");
}

#[test]
fn word_motion_hops_identifiers_not_bytes() {
    let mut buffer = buffer("image: nginx:1.27-alpine");
    buffer.set_selections(vec![Selection::caret(0)], 0);
    buffer.move_cursors(Motion::WordRight, false);
    assert_eq!(buffer.primary_selection().head, 5);
    buffer.move_cursors(Motion::WordRight, false);
    assert_eq!(buffer.primary_selection().head, 12);
    buffer.move_cursors(Motion::WordLeft, false);
    assert_eq!(buffer.primary_selection().head, 7);
}

#[test]
fn home_toggles_between_first_glyph_and_column_zero() {
    let mut buffer = buffer("  name: value");
    buffer.set_selections(vec![Selection::caret(9)], 0);
    buffer.move_cursors(Motion::Home, false);
    assert_eq!(buffer.primary_selection().head, 2);
    buffer.move_cursors(Motion::Home, false);
    assert_eq!(buffer.primary_selection().head, 0);
    buffer.move_cursors(Motion::Home, false);
    assert_eq!(buffer.primary_selection().head, 2);
}

#[test]
fn select_next_occurrence_grows_a_cursor_per_match_and_wraps() {
    let mut buffer = buffer("name: a\nname: b\nname: c\n");
    buffer.set_selections(vec![Selection::caret(1)], 0);
    buffer.select_next_occurrence();
    assert_eq!(buffer.primary_selection(), Selection::range(0, 4));
    buffer.select_next_occurrence();
    buffer.select_next_occurrence();
    assert_eq!(buffer.selections().len(), 3);
    buffer.select_next_occurrence();
    assert_eq!(buffer.selections().len(), 3, "every match is already held");
    buffer.insert("app");
    assert_eq!(buffer.text(), "app: a\napp: b\napp: c\n");
}

#[test]
fn overlapping_selections_merge_and_keep_a_primary() {
    let mut buffer = buffer("abcdefgh");
    buffer.set_selections(
        vec![
            Selection::range(0, 4),
            Selection::range(2, 6),
            Selection::caret(7),
        ],
        1,
    );
    assert_eq!(
        buffer.selections(),
        &[Selection::range(0, 6), Selection::caret(7)]
    );
    assert_eq!(buffer.primary_selection(), Selection::range(0, 6));
}

#[test]
fn delete_lines_removes_whole_rows_without_double_counting() {
    let mut buffer = buffer("one\ntwo\nthree\n");
    buffer.set_selections(vec![Selection::caret(1), Selection::caret(5)], 0);
    buffer.delete_lines();
    assert_eq!(buffer.text(), "three\n");
}

#[test]
fn undo_restores_selections_with_the_text() {
    let mut buffer = buffer("hello");
    buffer.set_selections(vec![Selection::range(0, 5)], 0);
    buffer.insert("bye");
    assert_eq!(buffer.text(), "bye");
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "hello");
    assert_eq!(buffer.selections(), &[Selection::range(0, 5)]);
    assert!(!buffer.selections()[0].is_caret());
}

#[test]
fn add_cursor_below_lands_on_the_goal_column() {
    let mut buffer = buffer("alpha\nbe\ngamma\n");
    buffer.set_selections(vec![Selection::caret(4)], 0);
    buffer.add_cursor_vertically(true);
    assert_eq!(buffer.selections().len(), 2);
    assert_eq!(buffer.selections()[1].head, 8, "clamped to the short line");
    buffer.add_cursor_vertically(true);
    let last = buffer.selections()[2];
    assert_eq!(
        buffer.rope().byte_to_point(last.head),
        Point::new(2, 4),
        "the goal column survives the short line"
    );
}

#[test]
fn multibyte_text_never_splits_a_glyph() {
    let mut buffer = buffer("a🦀b");
    buffer.set_selections(vec![Selection::caret(1)], 0);
    buffer.move_cursors(Motion::Right, false);
    assert_eq!(buffer.primary_selection().head, 5);
    buffer.backspace();
    assert_eq!(buffer.text(), "ab");
}

#[test]
fn indenting_a_selected_line_keeps_it_selected() {
    // One edit and one selection, so counting them cannot tell this from
    // typing over the selection -- and collapsing here loses the block the
    // user is still indenting.
    let mut buffer = buffer("a:\nb:\n");
    buffer.set_selections(vec![Selection::range(0, 2)], 0);
    buffer.indent();
    assert_eq!(buffer.text(), "  a:\nb:\n");
    let primary = buffer.primary_selection();
    assert!(
        !primary.is_caret(),
        "the selection survives the indent: {primary:?}"
    );
    assert_eq!(
        buffer
            .rope()
            .slice_to_string(primary.start()..primary.end()),
        "a:"
    );
    buffer.outdent();
    assert_eq!(buffer.text(), "a:\nb:\n");
    assert!(!buffer.primary_selection().is_caret(), "and the outdent");
}

#[test]
fn typing_over_a_selection_still_collapses_to_one_caret() {
    let mut buffer = buffer("a:\nb:\n");
    buffer.set_selections(vec![Selection::range(0, 2)], 0);
    buffer.insert("x");
    assert_eq!(buffer.text(), "x\nb:\n");
    assert_eq!(buffer.primary_selection(), Selection::caret(1));
}

#[test]
fn delete_lines_leaves_the_row_a_selection_only_touched_at_column_zero() {
    // Selecting a whole line downwards ends the selection at the next
    // line's column zero; that next line has not been selected.
    let mut buffer = buffer("one\ntwo\nthree\n");
    buffer.set_selections(vec![Selection::range(0, 4)], 0);
    buffer.delete_lines();
    assert_eq!(buffer.text(), "two\nthree\n");
}

#[test]
fn an_arrow_key_collapses_a_selection_to_its_edge_without_passing_it() {
    let mut buffer = buffer("abcdefgh");
    buffer.set_selections(vec![Selection::range(2, 5)], 0);
    buffer.move_cursors(Motion::Right, false);
    assert_eq!(
        buffer.primary_selection(),
        Selection::caret(5),
        "right lands on the end of what was selected, not one past it"
    );
    buffer.set_selections(vec![Selection::range(5, 2)], 0);
    buffer.move_cursors(Motion::Left, false);
    assert_eq!(
        buffer.primary_selection(),
        Selection::caret(2),
        "and left on the start, whichever end the head was"
    );
    buffer.set_selections(vec![Selection::range(2, 5)], 0);
    buffer.move_cursors(Motion::Left, false);
    assert_eq!(buffer.primary_selection(), Selection::caret(2));
    buffer.set_selections(vec![Selection::range(5, 2)], 0);
    buffer.move_cursors(Motion::Right, false);
    assert_eq!(buffer.primary_selection(), Selection::caret(5));
}

#[test]
fn no_motion_parks_the_caret_inside_a_cluster() {
    // A line length counts bytes and a word class counts scalars, so both
    // can land between the halves of a CRLF or inside a combining sequence.
    let mut crlf = buffer("abc\r\ndef");
    crlf.set_selections(vec![Selection::caret(1)], 0);
    crlf.move_cursors(Motion::End, false);
    let head = crlf.primary_selection().head;
    assert_eq!(
        head,
        crlf.rope().snap_to_grapheme_boundary(head),
        "the end of a CRLF line is a cluster boundary"
    );
    assert_eq!(head, 3, "which is before the carriage return");

    let mut accented = buffer("e\u{301} tail");
    accented.set_selections(vec![Selection::caret(0)], 0);
    accented.move_cursors(Motion::WordRight, false);
    let head = accented.primary_selection().head;
    assert_eq!(head, accented.rope().snap_to_grapheme_boundary(head));
}

#[test]
fn a_cluster_moves_deletes_and_columns_as_one_character() {
    let mut buffer = buffer("x\u{1F469}\u{200D}\u{1F4BB}y\n");
    buffer.set_selections(vec![Selection::caret(1)], 0);
    buffer.move_cursors(Motion::Right, false);
    let after = buffer.primary_selection().head;
    assert_eq!(after, 1 + "\u{1F469}\u{200D}\u{1F4BB}".len());
    buffer.move_cursors(Motion::Left, false);
    assert_eq!(buffer.primary_selection().head, 1, "and back in one press");
    buffer.set_selections(vec![Selection::caret(after)], 0);
    buffer.backspace();
    assert_eq!(
        buffer.text(),
        "xy\n",
        "one press deletes the whole cluster, not one scalar"
    );
}

#[test]
fn a_goal_column_counts_clusters_not_scalars() {
    let mut buffer = buffer("\u{1F469}\u{200D}\u{1F4BB}ab\nwxyz\n");
    let first_line = buffer.rope().line_len(0);
    buffer.set_selections(vec![Selection::caret(first_line)], 0);
    buffer.move_cursors(Motion::Down, false);
    let point = buffer.rope().byte_to_point(buffer.primary_selection().head);
    assert_eq!(
        point,
        Point::new(1, 3),
        "three clusters across, not six scalars"
    );
}
