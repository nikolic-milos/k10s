//! Multi-cursor edits over the rope: the editor's only mutation path.
//!
//! A [`Buffer`] holds the rope, a sorted disjoint selection set, and an undo
//! stack of whole snapshots -- the rope is persistent, so a snapshot costs
//! one `Arc`. Every mutation is a transaction of pre-space edits applied
//! back-to-front, and each transaction reports [`Splice`]s in application
//! order so a syntax tree can be edited with coordinates that are valid at
//! the moment of each call. What the transaction does to the selection is
//! stated by its [`SelectionIntent`] rather than inferred from how many edits
//! it happened to produce: typing collapses to one caret per edit, while
//! indenting a block keeps the block selected. Consecutive typing coalesces
//! into one undo step until the cursor moves; undo restores selections along
//! with text. Cursor motion and single-character deletion step by grapheme
//! cluster, so an accented letter or a family emoji moves and deletes once.

use crate::rope::{Point, Rope};
use std::ops::Range;

const MAX_UNDO_DEPTH: usize = 512;
pub const INDENT: &str = "  ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    pub goal_column: Option<usize>,
}

impl Selection {
    pub fn caret(offset: usize) -> Selection {
        Selection {
            anchor: offset,
            head: offset,
            goal_column: None,
        }
    }

    pub fn range(anchor: usize, head: usize) -> Selection {
        Selection {
            anchor,
            head,
            goal_column: None,
        }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn is_caret(&self) -> bool {
        self.anchor == self.head
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditGroup {
    Typing,
    Deleting,
    Other,
}

// What a transaction means for the selection. Counting edits cannot tell the
// two apart -- indenting one selected line is one edit and one selection, and
// so is typing over that selection -- and guessing collapses a block the user
// is still working with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionIntent {
    // The edits are the cursors: each leaves a caret where its text ends.
    Collapse,
    // The edits are structural and the selection is the user's: anchors and
    // heads move through the splice and keep their extent.
    Preserve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Splice {
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
    pub start_point: Point,
    pub old_end_point: Point,
    pub new_end_point: Point,
}

#[derive(Debug, Clone)]
struct Snapshot {
    rope: Rope,
    selections: Vec<Selection>,
    primary: usize,
}

#[derive(Debug)]
pub struct Buffer {
    rope: Rope,
    selections: Vec<Selection>,
    primary: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_group: Option<EditGroup>,
    version: u64,
}

impl Buffer {
    pub fn new(text: &str) -> Buffer {
        Buffer {
            rope: Rope::from(text),
            selections: vec![Selection::caret(0)],
            primary: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_group: None,
            version: 0,
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn selections(&self) -> &[Selection] {
        &self.selections
    }

    pub fn primary_selection(&self) -> Selection {
        self.selections[self.primary]
    }

    pub fn set_selections(&mut self, selections: Vec<Selection>, primary: usize) {
        self.selections = selections;
        self.primary = primary;
        self.normalize_selections();
        self.last_group = None;
    }

    fn normalize_selections(&mut self) {
        for selection in &mut self.selections {
            selection.anchor = self.rope.snap_to_char_boundary(selection.anchor);
            selection.head = self.rope.snap_to_char_boundary(selection.head);
        }
        if self.selections.is_empty() {
            self.selections.push(Selection::caret(0));
            self.primary = 0;
            return;
        }
        let primary_before = self.selections[self.primary.min(self.selections.len() - 1)];
        let mut indexed: Vec<Selection> = std::mem::take(&mut self.selections);
        indexed.sort_by_key(|selection| (selection.start(), selection.end()));
        let mut merged: Vec<Selection> = Vec::with_capacity(indexed.len());
        for selection in indexed {
            match merged.last_mut() {
                Some(last)
                    if selection.start() < last.end()
                        || (selection.start() == last.end()
                            && selection.is_caret()
                            && last.is_caret()) =>
                {
                    let start = last.start();
                    let end = last.end().max(selection.end());
                    *last = Selection::range(start, end);
                }
                _ => merged.push(selection),
            }
        }
        self.primary = merged
            .iter()
            .position(|selection| {
                selection.start() <= primary_before.start()
                    && selection.end() >= primary_before.end()
            })
            .unwrap_or(merged.len() - 1);
        self.selections = merged;
    }

    fn push_undo(&mut self, group: EditGroup) {
        let coalesce = group != EditGroup::Other && self.last_group == Some(group);
        if !coalesce {
            self.undo.push(Snapshot {
                rope: self.rope.clone(),
                selections: self.selections.clone(),
                primary: self.primary,
            });
            if self.undo.len() > MAX_UNDO_DEPTH {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_group = Some(group);
    }

    pub fn edit(
        &mut self,
        mut edits: Vec<(Range<usize>, String)>,
        group: EditGroup,
        intent: SelectionIntent,
    ) -> Vec<Splice> {
        edits.sort_by_key(|(range, _)| range.start);
        debug_assert!(
            edits
                .windows(2)
                .all(|pair| pair[0].0.end <= pair[1].0.start),
            "transaction edits must be disjoint"
        );
        if edits.is_empty() {
            return Vec::new();
        }
        self.push_undo(group);
        let mut splices = Vec::with_capacity(edits.len());
        for (range, text) in edits.iter().rev() {
            let start_point = self.rope.byte_to_point(range.start);
            let old_end_point = self.rope.byte_to_point(range.end);
            self.rope.replace(range.clone(), text);
            splices.push(Splice {
                start: range.start,
                old_end: range.end,
                new_end: range.start + text.len(),
                start_point,
                old_end_point,
                new_end_point: extend_point(start_point, text),
            });
        }
        let carets: Vec<usize> = {
            let mut delta = 0usize;
            let mut shrink = 0usize;
            edits
                .iter()
                .map(|(range, text)| {
                    let caret = range.start + delta - shrink + text.len();
                    delta += text.len();
                    shrink += range.end - range.start;
                    caret
                })
                .collect()
        };
        // A collapse pairs one caret with one cursor, so it needs an edit per
        // cursor; a cursor whose edit was filtered out as empty (backspace at
        // column zero) has nothing to pair with and maps instead.
        if intent == SelectionIntent::Collapse && carets.len() == self.selections.len() {
            let primary = self.primary;
            self.selections = carets.into_iter().map(Selection::caret).collect();
            self.primary = primary.min(self.selections.len() - 1);
        } else {
            let map = |offset: usize| map_offset(offset, &edits);
            for selection in &mut self.selections {
                selection.anchor = map(selection.anchor);
                selection.head = map(selection.head);
                selection.goal_column = None;
            }
            self.normalize_selections();
        }
        self.version += 1;
        splices
    }

    pub fn insert(&mut self, text: &str) -> Vec<Splice> {
        let group = if text.chars().count() == 1 && text != "\n" {
            EditGroup::Typing
        } else {
            EditGroup::Other
        };
        let edits = self
            .selections
            .iter()
            .map(|selection| (selection.start()..selection.end(), text.to_string()))
            .collect();
        self.edit(edits, group, SelectionIntent::Collapse)
    }

    pub fn newline(&mut self) -> Vec<Splice> {
        let edits = self
            .selections
            .iter()
            .map(|selection| {
                let row = self.rope.byte_to_point(selection.start()).row;
                let line = self.rope.line(row);
                let head = &line[..(selection.start() - self.rope.line_start(row)).min(line.len())];
                let indent: String = line
                    .chars()
                    .take_while(|character| *character == ' ')
                    .collect();
                let deepen = head.trim_end().ends_with(':') || head.trim() == "-";
                let mut text = String::with_capacity(1 + indent.len() + INDENT.len());
                text.push('\n');
                text.push_str(&indent);
                if deepen {
                    text.push_str(INDENT);
                }
                (selection.start()..selection.end(), text)
            })
            .collect();
        self.edit(edits, EditGroup::Other, SelectionIntent::Collapse)
    }

    pub fn backspace(&mut self) -> Vec<Splice> {
        let edits = self
            .selections
            .iter()
            .map(|selection| {
                if !selection.is_caret() {
                    return (selection.start()..selection.end(), String::new());
                }
                let head = selection.head;
                let point = self.rope.byte_to_point(head);
                let line_start = self.rope.line_start(point.row);
                let before = self.rope.slice_to_string(line_start..head);
                if !before.is_empty() && before.chars().all(|character| character == ' ') {
                    let keep = (before.len() - 1) / INDENT.len() * INDENT.len();
                    return (line_start + keep..head, String::new());
                }
                (self.rope.prev_grapheme_offset(head)..head, String::new())
            })
            .filter(|(range, _)| !range.is_empty())
            .collect();
        self.edit(edits, EditGroup::Deleting, SelectionIntent::Collapse)
    }

    pub fn delete_forward(&mut self) -> Vec<Splice> {
        let edits = self
            .selections
            .iter()
            .map(|selection| {
                if selection.is_caret() {
                    (
                        selection.head..self.rope.next_grapheme_offset(selection.head),
                        String::new(),
                    )
                } else {
                    (selection.start()..selection.end(), String::new())
                }
            })
            .filter(|(range, _)| !range.is_empty())
            .collect();
        self.edit(edits, EditGroup::Deleting, SelectionIntent::Collapse)
    }

    pub fn delete_lines(&mut self) -> Vec<Splice> {
        let mut ranges: Vec<Range<usize>> = self
            .selections
            .iter()
            .map(|selection| {
                let (start_row, end_row) = self.row_span(selection);
                let start = self.rope.line_start(start_row);
                let end = if end_row + 1 < self.rope.len_lines() {
                    self.rope.line_start(end_row + 1)
                } else {
                    self.rope.len()
                };
                start..end
            })
            .collect();
        ranges.sort_by_key(|range| range.start);
        ranges.dedup_by(|next, prev| {
            if next.start <= prev.end {
                prev.end = prev.end.max(next.end);
                true
            } else {
                false
            }
        });
        let edits = ranges
            .into_iter()
            .map(|range| (range, String::new()))
            .collect();
        self.edit(edits, EditGroup::Other, SelectionIntent::Collapse)
    }

    pub fn indent(&mut self) -> Vec<Splice> {
        if self.selections.iter().all(Selection::is_caret) {
            let edits = self
                .selections
                .iter()
                .map(|selection| {
                    let column = self.rope.grapheme_column(selection.head);
                    let pad = INDENT.len() - column % INDENT.len();
                    (selection.head..selection.head, " ".repeat(pad))
                })
                .collect();
            return self.edit(edits, EditGroup::Other, SelectionIntent::Collapse);
        }
        let edits = self
            .selected_rows()
            .into_iter()
            .map(|row| {
                let start = self.rope.line_start(row);
                (start..start, INDENT.to_string())
            })
            .collect();
        self.edit(edits, EditGroup::Other, SelectionIntent::Preserve)
    }

    pub fn outdent(&mut self) -> Vec<Splice> {
        let edits: Vec<(Range<usize>, String)> = self
            .selected_rows()
            .into_iter()
            .filter_map(|row| {
                let start = self.rope.line_start(row);
                let line = self.rope.line(row);
                let strip = line
                    .chars()
                    .take(INDENT.len())
                    .take_while(|character| *character == ' ')
                    .count();
                (strip > 0).then(|| (start..start + strip, String::new()))
            })
            .collect();
        self.edit(edits, EditGroup::Other, SelectionIntent::Preserve)
    }

    // The rows a selection covers. A selection that ends exactly at the next
    // line's column zero has not touched that line -- it is where a downward
    // select-line lands -- so it does not count, and any operation that works
    // in whole lines must agree about that.
    fn row_span(&self, selection: &Selection) -> (usize, usize) {
        let start_row = self.rope.byte_to_point(selection.start()).row;
        let mut end_row = self.rope.byte_to_point(selection.end()).row;
        if end_row > start_row && self.rope.line_start(end_row) == selection.end() {
            end_row -= 1;
        }
        (start_row, end_row)
    }

    fn selected_rows(&self) -> Vec<usize> {
        let mut rows: Vec<usize> = self
            .selections
            .iter()
            .flat_map(|selection| {
                let (start_row, end_row) = self.row_span(selection);
                start_row..=end_row
            })
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    // The rows the cursors cover, for callers outside the buffer that edit in
    // whole lines (commenting) and must use the same rule.
    pub fn cursor_rows(&self) -> Vec<usize> {
        self.selected_rows()
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot {
            rope: self.rope.clone(),
            selections: self.selections.clone(),
            primary: self.primary,
        });
        self.restore(snapshot);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot {
            rope: self.rope.clone(),
            selections: self.selections.clone(),
            primary: self.primary,
        });
        self.restore(snapshot);
        true
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.rope = snapshot.rope;
        self.selections = snapshot.selections;
        self.primary = snapshot.primary;
        self.last_group = None;
        self.version += 1;
    }

    pub fn move_cursors(&mut self, motion: Motion, extend: bool) {
        let rope = self.rope.clone();
        for selection in &mut self.selections {
            // An arrow key with something selected collapses to the edge it
            // points at, and does not move past it. Reading that edge after the
            // motion has already advanced the head overshoots by one cluster,
            // because the head is one of the two edges being read.
            if !extend && !selection.is_caret() && matches!(motion, Motion::Left | Motion::Right) {
                let edge = if matches!(motion, Motion::Left) {
                    selection.start()
                } else {
                    selection.end()
                };
                *selection = Selection::caret(edge);
                continue;
            }
            let (target, goal) = apply_motion(&rope, selection, motion);
            // A line length and a word class both count something other than
            // clusters, so the landing offset is snapped: `End` on a line ending
            // in CRLF otherwise parks between the two.
            selection.head = rope.snap_to_grapheme_boundary(target);
            selection.goal_column = goal;
            if !extend {
                selection.anchor = selection.head;
            }
        }
        self.normalize_selections();
        self.last_group = None;
    }

    pub fn select_all(&mut self) {
        self.set_selections(vec![Selection::range(0, self.rope.len())], 0);
    }

    pub fn collapse_to_primary(&mut self) -> bool {
        if self.selections.len() > 1 {
            let primary = self.primary_selection();
            self.set_selections(vec![primary], 0);
            true
        } else if !self.selections[0].is_caret() {
            let head = self.selections[0].head;
            self.set_selections(vec![Selection::caret(head)], 0);
            true
        } else {
            false
        }
    }

    pub fn add_cursor_vertically(&mut self, below: bool) {
        let edge = if below {
            *self
                .selections
                .iter()
                .max_by_key(|selection| selection.head)
                .expect("a buffer always holds a selection")
        } else {
            *self
                .selections
                .iter()
                .min_by_key(|selection| selection.head)
                .expect("a buffer always holds a selection")
        };
        let point = self.rope.byte_to_point(edge.head);
        let goal = edge
            .goal_column
            .unwrap_or_else(|| self.rope.grapheme_column(edge.head));
        let target_row = if below {
            if point.row + 1 >= self.rope.len_lines() {
                return;
            }
            point.row + 1
        } else {
            if point.row == 0 {
                return;
            }
            point.row - 1
        };
        let offset = self.rope.offset_at_grapheme_column(target_row, goal);
        let mut selection = Selection::caret(offset);
        selection.goal_column = Some(goal);
        self.selections.push(selection);
        self.normalize_selections();
        self.last_group = None;
    }

    pub fn select_next_occurrence(&mut self) {
        let primary = self.primary_selection();
        if primary.is_caret() {
            let (start, end) = word_around(&self.rope, primary.head);
            if start < end {
                let index = self.primary;
                self.selections[index] = Selection::range(start, end);
                self.normalize_selections();
            }
            return;
        }
        let needle = self.rope.slice_to_string(primary.start()..primary.end());
        if needle.is_empty() {
            return;
        }
        let text = self.rope.to_string();
        let last_end = self
            .selections
            .iter()
            .map(Selection::end)
            .max()
            .unwrap_or(0);
        let found = find_from(&text, &needle, last_end)
            .or_else(|| find_from(&text, &needle, 0))
            .filter(|start| {
                !self
                    .selections
                    .iter()
                    .any(|selection| selection.start() == *start)
            });
        if let Some(start) = found {
            self.selections
                .push(Selection::range(start, start + needle.len()));
            self.primary = self.selections.len() - 1;
            self.normalize_selections();
            self.last_group = None;
        }
    }
}

fn find_from(text: &str, needle: &str, from: usize) -> Option<usize> {
    text.get(from..)
        .and_then(|tail| tail.find(needle))
        .map(|index| from + index)
}

fn extend_point(start: Point, text: &str) -> Point {
    let mut newlines = 0;
    let mut last_line_len = 0;
    for byte in text.bytes() {
        if byte == b'\n' {
            newlines += 1;
            last_line_len = 0;
        } else {
            last_line_len += 1;
        }
    }
    if newlines > 0 {
        Point::new(start.row + newlines, last_line_len)
    } else {
        Point::new(start.row, start.column + text.len())
    }
}

fn map_offset(offset: usize, edits: &[(Range<usize>, String)]) -> usize {
    let mut delta: isize = 0;
    for (range, text) in edits {
        if range.end <= offset {
            delta += text.len() as isize - range.len() as isize;
        } else if range.start < offset {
            return (range.start + text.len()).wrapping_add_signed(delta);
        } else {
            break;
        }
    }
    offset.wrapping_add_signed(delta)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    Home,
    End,
    PageUp(usize),
    PageDown(usize),
    DocStart,
    DocEnd,
}

fn apply_motion(rope: &Rope, selection: &Selection, motion: Motion) -> (usize, Option<usize>) {
    let head = selection.head;
    match motion {
        Motion::Left => (rope.prev_grapheme_offset(head), None),
        Motion::Right => (rope.next_grapheme_offset(head), None),
        Motion::Up | Motion::Down | Motion::PageUp(_) | Motion::PageDown(_) => {
            let point = rope.byte_to_point(head);
            let goal = selection
                .goal_column
                .unwrap_or_else(|| rope.grapheme_column(head));
            let rows = match motion {
                Motion::Up => -1isize,
                Motion::Down => 1,
                Motion::PageUp(page) => -(page as isize),
                Motion::PageDown(page) => page as isize,
                _ => unreachable!("only vertical motions reach this arm"),
            };
            let target_row = point.row.saturating_add_signed(rows);
            if rows > 0 && target_row >= rope.len_lines() {
                return (rope.len(), Some(goal));
            }
            let target_row = target_row.min(rope.len_lines() - 1);
            (rope.offset_at_grapheme_column(target_row, goal), Some(goal))
        }
        Motion::WordLeft => (word_boundary_left(rope, head), None),
        Motion::WordRight => (word_boundary_right(rope, head), None),
        Motion::Home => {
            let point = rope.byte_to_point(head);
            let start = rope.line_start(point.row);
            let line = rope.line(point.row);
            let first_glyph = start
                + line
                    .char_indices()
                    .find(|(_, character)| !character.is_whitespace())
                    .map(|(index, _)| index)
                    .unwrap_or(0);
            (
                if head == first_glyph {
                    start
                } else {
                    first_glyph
                },
                None,
            )
        }
        Motion::End => {
            let point = rope.byte_to_point(head);
            (rope.line_start(point.row) + rope.line_len(point.row), None)
        }
        Motion::DocStart => (0, None),
        Motion::DocEnd => (rope.len(), None),
    }
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn word_boundary_left(rope: &Rope, mut offset: usize) -> usize {
    while offset > 0 {
        let previous = rope.prev_char_offset(offset);
        let Some(character) = rope.char_at(previous) else {
            break;
        };
        if is_word_char(character) {
            break;
        }
        offset = previous;
    }
    while offset > 0 {
        let previous = rope.prev_char_offset(offset);
        let Some(character) = rope.char_at(previous) else {
            break;
        };
        if !is_word_char(character) {
            break;
        }
        offset = previous;
    }
    offset
}

fn word_boundary_right(rope: &Rope, mut offset: usize) -> usize {
    let len = rope.len();
    while offset < len {
        let Some(character) = rope.char_at(offset) else {
            break;
        };
        if is_word_char(character) {
            break;
        }
        offset = rope.next_char_offset(offset);
    }
    while offset < len {
        let Some(character) = rope.char_at(offset) else {
            break;
        };
        if !is_word_char(character) {
            break;
        }
        offset = rope.next_char_offset(offset);
    }
    offset
}

fn word_around(rope: &Rope, offset: usize) -> (usize, usize) {
    let mut start = offset;
    while start > 0 {
        let previous = rope.prev_char_offset(start);
        match rope.char_at(previous) {
            Some(character) if is_word_char(character) => start = previous,
            _ => break,
        }
    }
    let mut end = offset;
    while let Some(character) = rope.char_at(end) {
        if !is_word_char(character) {
            break;
        }
        end = rope.next_char_offset(end);
    }
    (start, end)
}

#[cfg(test)]
#[path = "buffer_test.rs"]
mod tests;
