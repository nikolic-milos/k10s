//! A deliberately small persistent rope: the text under the editor.
//!
//! Chunked UTF-8 in a balanced tree of `Arc` nodes, so cloning a rope is one
//! refcount bump and every edit is a path copy -- the undo stack holds whole
//! snapshots at the cost of the spine. Node summaries carry bytes, newlines,
//! and the length of the trailing line, which is exactly what byte<->point
//! conversion and line addressing need and nothing more. Offsets are bytes
//! everywhere and `Point` columns are byte columns, matching tree-sitter.
//! Chunks never split inside a char, so a char never spans two leaves.
//! Grapheme clusters may straddle leaves, so cursor-facing boundaries come
//! from a segmentation cursor fed the rope's chunks rather than from a
//! materialized string.

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete, UnicodeSegmentation as _};

pub const TARGET_CHUNK: usize = 2048;
const MAX_CHUNK: usize = TARGET_CHUNK * 2;
const MAX_CHILDREN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

impl Point {
    pub fn new(row: usize, column: usize) -> Point {
        Point { row, column }
    }

    fn advanced_by(self, summary: Summary) -> Point {
        if summary.newlines > 0 {
            Point {
                row: self.row + summary.newlines,
                column: summary.last_line_len,
            }
        } else {
            Point {
                row: self.row,
                column: self.column + summary.bytes,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Summary {
    bytes: usize,
    newlines: usize,
    last_line_len: usize,
}

impl Summary {
    fn of(text: &str) -> Summary {
        let bytes = text.len();
        let mut newlines = 0;
        let mut last_newline = None;
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                newlines += 1;
                last_newline = Some(index);
            }
        }
        let last_line_len = match last_newline {
            Some(index) => bytes - index - 1,
            None => bytes,
        };
        Summary {
            bytes,
            newlines,
            last_line_len,
        }
    }

    fn add(self, other: Summary) -> Summary {
        Summary {
            bytes: self.bytes + other.bytes,
            newlines: self.newlines + other.newlines,
            last_line_len: if other.newlines > 0 {
                other.last_line_len
            } else {
                self.last_line_len + other.bytes
            },
        }
    }
}

#[derive(Debug)]
enum Node {
    Leaf {
        summary: Summary,
        text: String,
    },
    Internal {
        height: u8,
        summary: Summary,
        children: Vec<Arc<Node>>,
    },
}

impl Node {
    fn summary(&self) -> Summary {
        match self {
            Node::Leaf { summary, .. } | Node::Internal { summary, .. } => *summary,
        }
    }

    fn height(&self) -> u8 {
        match self {
            Node::Leaf { .. } => 0,
            Node::Internal { height, .. } => *height,
        }
    }
}

fn leaf(text: String) -> Arc<Node> {
    Arc::new(Node::Leaf {
        summary: Summary::of(&text),
        text,
    })
}

fn internal(children: Vec<Arc<Node>>) -> Arc<Node> {
    debug_assert!(!children.is_empty() && children.len() <= MAX_CHILDREN);
    let height = children[0].height() + 1;
    debug_assert!(children.iter().all(|child| child.height() + 1 == height));
    let summary = children
        .iter()
        .fold(Summary::default(), |acc, child| acc.add(child.summary()));
    Arc::new(Node::Internal {
        height,
        summary,
        children,
    })
}

fn level_of(mut nodes: Vec<Arc<Node>>) -> Arc<Node> {
    debug_assert!(!nodes.is_empty());
    while nodes.len() > 1 {
        nodes = nodes
            .chunks(MAX_CHILDREN)
            .map(|group| internal(group.to_vec()))
            .collect();
    }
    nodes.pop().expect("level_of never receives zero nodes")
}

fn chunk_leaves(text: &str) -> Vec<Arc<Node>> {
    let mut leaves = Vec::with_capacity(text.len() / TARGET_CHUNK + 1);
    let mut rest = text;
    while rest.len() > MAX_CHUNK {
        let mut split = TARGET_CHUNK;
        while !rest.is_char_boundary(split) {
            split += 1;
        }
        let (head, tail) = rest.split_at(split);
        leaves.push(leaf(head.to_string()));
        rest = tail;
    }
    if !rest.is_empty() || leaves.is_empty() {
        leaves.push(leaf(rest.to_string()));
    }
    leaves
}

fn concat(a: Arc<Node>, b: Arc<Node>) -> Arc<Node> {
    if a.summary().bytes == 0 {
        return b;
    }
    if b.summary().bytes == 0 {
        return a;
    }
    let height_a = a.height();
    let height_b = b.height();
    if height_a == height_b {
        if let (Node::Leaf { text: text_a, .. }, Node::Leaf { text: text_b, .. }) =
            (a.as_ref(), b.as_ref())
            && text_a.len() + text_b.len() <= MAX_CHUNK
            && (text_a.len() < TARGET_CHUNK / 2 || text_b.len() < TARGET_CHUNK / 2)
        {
            let mut merged = String::with_capacity(text_a.len() + text_b.len());
            merged.push_str(text_a);
            merged.push_str(text_b);
            return leaf(merged);
        }
        if let (
            Node::Internal {
                children: children_a,
                ..
            },
            Node::Internal {
                children: children_b,
                ..
            },
        ) = (a.as_ref(), b.as_ref())
            && children_a.len() + children_b.len() <= MAX_CHILDREN
        {
            let mut children = Vec::with_capacity(children_a.len() + children_b.len());
            children.extend(children_a.iter().cloned());
            children.extend(children_b.iter().cloned());
            return internal(children);
        }
        return internal(vec![a, b]);
    }
    if height_a > height_b {
        let Node::Internal { children, .. } = a.as_ref() else {
            unreachable!("a leaf has height zero, the minimum");
        };
        let last = children.len() - 1;
        let merged = concat(children[last].clone(), b);
        let mut kids: Vec<Arc<Node>> = children[..last].to_vec();
        if merged.height() == height_a {
            let Node::Internal {
                children: merged_children,
                ..
            } = merged.as_ref()
            else {
                unreachable!("an over-tall concat result is always internal");
            };
            kids.extend(merged_children.iter().cloned());
        } else {
            kids.push(merged);
        }
        rebuild_level(kids)
    } else {
        let Node::Internal { children, .. } = b.as_ref() else {
            unreachable!("a leaf has height zero, the minimum");
        };
        let merged = concat(a, children[0].clone());
        let mut kids: Vec<Arc<Node>> = Vec::with_capacity(children.len() + 1);
        if merged.height() == height_b {
            let Node::Internal {
                children: merged_children,
                ..
            } = merged.as_ref()
            else {
                unreachable!("an over-tall concat result is always internal");
            };
            kids.extend(merged_children.iter().cloned());
        } else {
            kids.push(merged);
        }
        kids.extend(children[1..].iter().cloned());
        rebuild_level(kids)
    }
}

fn rebuild_level(kids: Vec<Arc<Node>>) -> Arc<Node> {
    if kids.len() <= MAX_CHILDREN {
        internal(kids)
    } else {
        let mid = kids.len() / 2;
        let right = internal(kids[mid..].to_vec());
        let left = internal(kids[..mid].to_vec());
        internal(vec![left, right])
    }
}

fn split(node: &Arc<Node>, at: usize) -> (Arc<Node>, Arc<Node>) {
    debug_assert!(at <= node.summary().bytes);
    match node.as_ref() {
        Node::Leaf { text, .. } => {
            assert!(
                text.is_char_boundary(at),
                "rope edits must land on char boundaries"
            );
            (leaf(text[..at].to_string()), leaf(text[at..].to_string()))
        }
        Node::Internal { children, .. } => {
            let mut remaining = at;
            let mut left = leaf(String::new());
            let mut index = 0;
            for (child_index, child) in children.iter().enumerate() {
                let bytes = child.summary().bytes;
                if remaining <= bytes {
                    index = child_index;
                    break;
                }
                remaining -= bytes;
                index = child_index;
            }
            let (child_left, child_right) = split(&children[index], remaining);
            for child in &children[..index] {
                left = concat(left, child.clone());
            }
            left = concat(left, child_left);
            let mut right = child_right;
            for child in &children[index + 1..] {
                right = concat(right, child.clone());
            }
            (left, right)
        }
    }
}

#[derive(Clone)]
pub struct Rope {
    root: Arc<Node>,
}

impl Default for Rope {
    fn default() -> Rope {
        Rope {
            root: leaf(String::new()),
        }
    }
}

impl From<&str> for Rope {
    fn from(text: &str) -> Rope {
        Rope {
            root: level_of(chunk_leaves(text)),
        }
    }
}

impl Rope {
    pub fn len(&self) -> usize {
        self.root.summary().bytes
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len_lines(&self) -> usize {
        self.root.summary().newlines + 1
    }

    pub fn max_point(&self) -> Point {
        let summary = self.root.summary();
        Point {
            row: summary.newlines,
            column: summary.last_line_len,
        }
    }

    pub fn replace(&mut self, range: Range<usize>, text: &str) {
        // Stated here rather than only under `debug_assertions`, because the
        // release failure for a reversed range is a wrapped length that lands
        // as a char-boundary assert several frames deep in `split`, naming the
        // wrong precondition.
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "rope edits must be a range inside the rope"
        );
        let (left, rest) = split(&self.root, range.start);
        let (_, right) = split(&rest, range.end - range.start);
        let middle = level_of(chunk_leaves(text));
        self.root = concat(concat(left, middle), right);
    }

    pub fn insert(&mut self, offset: usize, text: &str) {
        self.replace(offset..offset, text);
    }

    pub fn delete(&mut self, range: Range<usize>) {
        self.replace(range, "");
    }

    pub fn byte_to_point(&self, offset: usize) -> Point {
        debug_assert!(offset <= self.len());
        let mut node = &self.root;
        let mut point = Point::default();
        let mut remaining = offset.min(self.len());
        loop {
            match node.as_ref() {
                Node::Leaf { text, .. } => {
                    let mut newlines = 0;
                    let mut last_newline = None;
                    for (index, byte) in text.as_bytes()[..remaining].iter().enumerate() {
                        if *byte == b'\n' {
                            newlines += 1;
                            last_newline = Some(index);
                        }
                    }
                    return match last_newline {
                        Some(index) => Point {
                            row: point.row + newlines,
                            column: remaining - index - 1,
                        },
                        None => Point {
                            row: point.row,
                            column: point.column + remaining,
                        },
                    };
                }
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let summary = child.summary();
                        if remaining <= summary.bytes {
                            next = Some(child);
                            break;
                        }
                        remaining -= summary.bytes;
                        point = point.advanced_by(summary);
                    }
                    node = next.expect("remaining is bounded by the subtree byte count");
                }
            }
        }
    }

    pub fn point_to_byte(&self, point: Point) -> usize {
        let row = point.row.min(self.len_lines() - 1);
        let start = self.line_start(row);
        let len = self.line_len(row);
        self.snap_to_char_boundary(start + point.column.min(len))
    }

    pub fn line_start(&self, row: usize) -> usize {
        debug_assert!(row < self.len_lines());
        if row == 0 {
            return 0;
        }
        let mut node = &self.root;
        let mut offset = 0;
        let mut remaining = row;
        loop {
            match node.as_ref() {
                Node::Leaf { text, .. } => {
                    for (index, byte) in text.bytes().enumerate() {
                        if byte == b'\n' {
                            remaining -= 1;
                            if remaining == 0 {
                                return offset + index + 1;
                            }
                        }
                    }
                    unreachable!("the row count was checked against the summary");
                }
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let summary = child.summary();
                        if remaining <= summary.newlines {
                            next = Some(child);
                            break;
                        }
                        remaining -= summary.newlines;
                        offset += summary.bytes;
                    }
                    node = next.expect("remaining is bounded by the subtree newline count");
                }
            }
        }
    }

    pub fn line_len(&self, row: usize) -> usize {
        let start = self.line_start(row);
        if row + 1 < self.len_lines() {
            self.line_start(row + 1) - 1 - start
        } else {
            self.len() - start
        }
    }

    pub fn line(&self, row: usize) -> String {
        let start = self.line_start(row);
        self.slice_to_string(start..start + self.line_len(row))
    }

    pub fn slice_to_string(&self, range: Range<usize>) -> String {
        let mut out = String::with_capacity(range.end - range.start);
        for chunk in self.chunks_in(range) {
            out.push_str(chunk);
        }
        out
    }

    pub fn chunks(&self) -> Chunks<'_> {
        self.chunks_in(0..self.len())
    }

    pub fn chunks_in(&self, range: Range<usize>) -> Chunks<'_> {
        debug_assert!(range.start <= range.end && range.end <= self.len());
        Chunks {
            stack: vec![(&self.root, 0)],
            range,
        }
    }

    fn chunk_at(&self, offset: usize) -> (&str, usize) {
        let mut node = &self.root;
        let mut start = 0;
        let mut remaining = offset.min(self.len());
        loop {
            match node.as_ref() {
                Node::Leaf { text, .. } => return (text, start),
                Node::Internal { children, .. } => {
                    let last = children.len() - 1;
                    let mut next = None;
                    for (index, child) in children.iter().enumerate() {
                        let bytes = child.summary().bytes;
                        if remaining < bytes || index == last {
                            next = Some(child);
                            break;
                        }
                        remaining -= bytes;
                        start += bytes;
                    }
                    node = next.expect("every internal node has children");
                }
            }
        }
    }

    pub fn chunk_bytes_from(&self, offset: usize) -> &[u8] {
        if offset >= self.len() {
            return b"";
        }
        let (text, start) = self.chunk_at(offset);
        &text.as_bytes()[offset - start..]
    }

    pub fn is_char_boundary(&self, offset: usize) -> bool {
        if offset == 0 || offset >= self.len() {
            return true;
        }
        let (text, start) = self.chunk_at(offset);
        text.is_char_boundary(offset - start)
    }

    pub fn snap_to_char_boundary(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.len());
        while offset > 0 && !self.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    pub fn char_at(&self, offset: usize) -> Option<char> {
        if offset >= self.len() {
            return None;
        }
        let (text, start) = self.chunk_at(offset);
        text[offset - start..].chars().next()
    }

    pub fn next_char_offset(&self, offset: usize) -> usize {
        match self.char_at(offset) {
            Some(ch) => offset + ch.len_utf8(),
            None => self.len(),
        }
    }

    pub fn prev_char_offset(&self, offset: usize) -> usize {
        if offset == 0 {
            return 0;
        }
        let (text, start) = self.chunk_at(offset - 1);
        let mut index = offset - 1 - start;
        while !text.is_char_boundary(index) {
            index -= 1;
        }
        start + index
    }

    // A cursor steps over what the eye sees as one character, which is a
    // grapheme cluster and not a scalar: an accented letter, a flag, a
    // family emoji, or a CRLF pair all move in one press and delete in one
    // press. Segmentation runs over the rope's own chunks, so a cluster that
    // straddles two leaves is still one cluster.
    pub fn next_grapheme_offset(&self, offset: usize) -> usize {
        self.grapheme_boundary(offset, true)
    }

    pub fn prev_grapheme_offset(&self, offset: usize) -> usize {
        self.grapheme_boundary(offset, false)
    }

    fn grapheme_boundary(&self, offset: usize, forward: bool) -> usize {
        let len = self.len();
        let offset = self.snap_to_char_boundary(offset);
        if len == 0 || (forward && offset >= len) || (!forward && offset == 0) {
            return if forward { len } else { 0 };
        }
        let mut cursor = GraphemeCursor::new(offset, len, true);
        loop {
            let probe = if forward {
                cursor.cur_cursor()
            } else {
                cursor.cur_cursor().saturating_sub(1)
            };
            let (chunk, start) = self.chunk_at(probe.min(len - 1));
            let step = if forward {
                cursor.next_boundary(chunk, start)
            } else {
                cursor.prev_boundary(chunk, start)
            };
            match step {
                Ok(Some(boundary)) => return boundary,
                Ok(None) => return if forward { len } else { 0 },
                Err(GraphemeIncomplete::PreContext(index)) => {
                    // The regional-indicator and emoji rules need to know what
                    // precedes the chunk; a bounded window is enough, and it is
                    // the only allocation on this path.
                    let window = self.snap_to_char_boundary(index.saturating_sub(TARGET_CHUNK));
                    let context = self.slice_to_string(window..index);
                    cursor.provide_context(&context, window);
                }
                Err(GraphemeIncomplete::NextChunk | GraphemeIncomplete::PrevChunk) => {}
                // InvalidOffset and InvalidState cannot arise from a boundary
                // we snapped ourselves, but a scalar step is still a legal
                // cursor position rather than a panic.
                Err(_) => {
                    return if forward {
                        self.next_char_offset(offset)
                    } else {
                        self.prev_char_offset(offset)
                    };
                }
            }
        }
    }

    // The nearest cluster boundary at or before an offset. Motions that count
    // something other than clusters -- a line length, a word class -- can land
    // inside one, and a caret inside a cluster is a caret the eye cannot place.
    pub fn snap_to_grapheme_boundary(&self, offset: usize) -> usize {
        let offset = self.snap_to_char_boundary(offset);
        if offset == 0 || offset >= self.len() {
            return offset;
        }
        let previous = self.prev_grapheme_offset(offset);
        if self.next_grapheme_offset(previous) <= offset {
            offset
        } else {
            previous
        }
    }

    // A run of one line's bytes, borrowed from the leaf that holds it whenever
    // it fits in one -- which is every line the editor draws -- and copied only
    // when it straddles leaves. Vertical motion asks twice per keypress.
    fn line_text(&self, start: usize, len: usize) -> Cow<'_, str> {
        let (chunk, chunk_start) = self.chunk_at(start);
        let from = start - chunk_start;
        match from.checked_add(len) {
            Some(to) if to <= chunk.len() => Cow::Borrowed(&chunk[from..to]),
            _ => Cow::Owned(self.slice_to_string(start..start + len)),
        }
    }

    // How far along its line an offset sits, counted in grapheme clusters:
    // the goal column vertical motion remembers.
    pub fn grapheme_column(&self, offset: usize) -> usize {
        let point = self.byte_to_point(offset);
        let start = self.line_start(point.row);
        self.line_text(start, point.column).graphemes(true).count()
    }

    pub fn offset_at_grapheme_column(&self, row: usize, column: usize) -> usize {
        let start = self.line_start(row);
        let line = self.line_text(start, self.line_len(row));
        let byte_column = line
            .grapheme_indices(true)
            .nth(column)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
        start + byte_column
    }

    #[doc(hidden)]
    pub fn depth(&self) -> usize {
        self.root.height() as usize
    }
}

impl fmt::Display for Rope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.chunks() {
            formatter.write_str(chunk)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Rope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Rope")
            .field("bytes", &self.len())
            .field("lines", &self.len_lines())
            .field("depth", &self.depth())
            .finish()
    }
}

pub struct Chunks<'a> {
    stack: Vec<(&'a Arc<Node>, usize)>,
    range: Range<usize>,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        while let Some((node, start)) = self.stack.pop() {
            let bytes = node.summary().bytes;
            if start >= self.range.end || start + bytes <= self.range.start {
                continue;
            }
            match node.as_ref() {
                Node::Leaf { text, .. } => {
                    let from = self.range.start.max(start) - start;
                    let to = self.range.end.min(start + bytes) - start;
                    if from < to {
                        return Some(&text[from..to]);
                    }
                }
                Node::Internal { children, .. } => {
                    let mut child_start = start + bytes;
                    for child in children.iter().rev() {
                        child_start -= child.summary().bytes;
                        self.stack.push((child, child_start));
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
#[path = "rope_test.rs"]
mod tests;
