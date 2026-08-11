//! Three-way line diff: what an apply would change, and who changed it first.
//!
//! §5.2's row asks for a three-way view -- live, last-applied, editor -- and the
//! third document is the whole point. A region only the buffer changed is the
//! user's own edit; a region only the cluster changed is drift an apply would
//! revert; a region both changed is a conflict. Without the base document those
//! three collapse into one indistinguishable "differs", which is why an object
//! carrying no `last-applied-configuration` is a labelled two-way diff rather
//! than a three-way one that guesses.
//!
//! The alignment underneath is deliberately not a minimal edit script. Lines
//! intern to `u32`, common prefix and suffix are trimmed, and what is left is
//! anchored on lines occurring exactly once on both sides -- patience
//! alignment -- with an exact Hirschberg LCS inside each gap the anchors leave.
//! Anchoring on unique lines is what stops a manifest's forty identical
//! `ports:` lines from pairing off by coincidence, and it confines the
//! quadratic part to the gaps. A gap wider than the budget is reported whole
//! and labelled [`Diff::coarse`]: never aligned wrongly, and never allowed to
//! take a frame.
//!
//! There is a fourth classification because three were not honest. A region the
//! base says *nothing* about, where the cluster and the buffer each hold
//! something, is literally a conflict -- and reading it as one implies a refusal
//! the server has not made. The ordinary reason a base declares nothing about a
//! field is that nobody declared it and the server defaulted it, so
//! [`Origin::Undeclared`] says that instead, and leaves the question of what an
//! apply does there to the dry run, which is the only thing that can answer it.
//!
//! A [`Row`] is twelve bytes and carries a byte range into its own side rather
//! than a copied line, so the diff of a megabyte is a vector rather than a
//! second copy of the document, and only the visible rows are ever composed
//! into runs. It held a line number too, until nothing was found to be reading
//! it: at the 196,608-row ceiling that field was 786 KB of answers to a
//! question no caller asks.
//!
//! A [`Hunk`] carries one thing its rows cannot supply: the span it covers on
//! the *buffer* side, positioned even when the buffer has no lines there. That
//! is what makes [`keep_theirs`] possible -- taking the cluster's side of one
//! hunk is an edit to the buffer, and text only the cluster has still has a
//! place in the buffer where it belongs. Summing row lengths afterwards would
//! have derived the same number until the two sides spelled a line ending
//! differently, and then it would have spliced at the wrong byte.

use std::collections::HashMap;
use std::ops::Range;

// The quadratic ceiling for one gap the anchors could not split, in logical DP
// cells. Hirschberg visits fewer than twice this many cells across its recursive
// passes, and the total budget below is charged by that conservative bound.
const MAX_GAP_CELLS: usize = 4 << 20;

// And the ceiling for the whole quadratic/anchoring part of the alignment, so a
// document made of a thousand unalignable gaps cannot cost a thousand times the
// single-gap bound. Common edge scans and row emission are linear instead, and
// are bounded by MAX_SIDE_LINES.
pub(crate) const MAX_TOTAL_CELLS: usize = 8 << 20;

// Byte ranges ride in `u32` and the manifest emitter caps a document at 2 MiB;
// a local file has no such cap, so the diff states its own rather than
// truncating one side of a comparison.
pub const MAX_SIDE_BYTES: usize = 8 << 20;

// A byte cap alone is not a work or memory cap: eight MiB of newlines is more
// than eight million rows. At this bound the largest possible conflict emits at
// most 196,608 rows, while the 2 MiB manifest emitter's normal dense fixture
// still fits. Anything denser is not reviewable in one synchronous frame.
pub const MAX_SIDE_LINES: usize = 1 << 16;

/// Which of the three documents a row's text lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The object as the cluster has it now.
    Live,
    /// The `last-applied-configuration`: what was declared last time.
    Base,
    /// The editor buffer: what would be declared now.
    Buffer,
}

/// Who changed a region, which is what decides whether applying it is an edit,
/// a revert, or a collision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// All three documents agree here, so an apply changes nothing.
    Common,
    /// The buffer alone moved away from the base: the user's own edit.
    Mine,
    /// The cluster alone moved away from the base, so applying the buffer
    /// reverts whatever moved it.
    Theirs,
    /// Cluster and buffer both moved away from the base, differently.
    Conflict,
    /// Cluster and buffer differ where the base declared nothing at all.
    ///
    /// Strictly this is a conflict -- neither side's text was ever declared --
    /// and labelling it one reads as a refusal the server has not made. The
    /// usual reason a last-applied document is silent about a field is that
    /// nobody declared it and the server defaulted it. Without `managedFields`
    /// no client can say what an apply does to a field it never declared, which
    /// is precisely why the dry run is the authoritative half of the review, so
    /// this label says what is true and defers the rest to it.
    Undeclared,
}

/// One rendered line: which document it came from, what class of change it
/// belongs to, and where its bytes are on its own side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub side: Side,
    pub origin: Origin,
    start: u32,
    end: u32,
}

impl Row {
    /// The row's bytes within its own side's text, line terminator excluded.
    pub fn bytes(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// A run of adjacent rows sharing one origin: what next-change navigation steps
/// over and what the header counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub origin: Origin,
    pub rows: Range<usize>,
    buffer_start: u32,
    buffer_end: u32,
}

impl Hunk {
    /// The bytes of the buffer this hunk covers -- empty, but *positioned*, when
    /// the buffer has no lines here. An edit that takes the cluster's side of a
    /// hunk the buffer is missing entirely has to know where the missing lines
    /// belong, and the answer is a line boundary rather than nothing.
    pub fn buffer(&self) -> Range<usize> {
        self.buffer_start as usize..self.buffer_end as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub mine: u32,
    pub theirs: u32,
    pub conflict: u32,
    /// Hunks the base declared nothing about. Counted apart from `conflict`
    /// because a summary that folds them together reports a refusal that is not
    /// coming.
    pub undeclared: u32,
    /// Buffer lines inside changed hunks: what the apply would add.
    pub added: u32,
    /// Live lines inside changed hunks: what the apply would replace.
    pub removed: u32,
}

/// The three documents, as text. `base` is absent on any object never applied
/// with a client that writes `last-applied-configuration` -- which includes
/// everything created by server-side apply -- and the diff says so rather than
/// inventing a base.
#[derive(Debug, Clone, Copy)]
pub struct Sides<'a> {
    pub base: Option<&'a str>,
    pub live: &'a str,
    pub buffer: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    pub rows: Vec<Row>,
    pub hunks: Vec<Hunk>,
    pub counts: Counts,
    /// No base document was given, so every difference reads as the user's own
    /// edit and no conflict can be detected. A view must label this.
    pub two_way: bool,
    /// A region diverged past the alignment budget and is reported whole rather
    /// than aligned line by line.
    pub coarse: bool,
    /// The three sides disagree about whether the document ends in a newline --
    /// invisible in a line-oriented diff, so it is reported instead.
    pub final_newline_differs: bool,
    /// Some(reason) when no diff was computed at all.
    pub refused: Option<&'static str>,
}

/// What a comparison concluded. This is deliberately not a boolean: a boolean
/// could not tell "the three documents agree" from "nothing was compared", and
/// the write path read the second as the first -- telling a user that an apply
/// of a document the diff had refused to review would change nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No comparison was made, and this is why. Every count in [`Diff`] is zero
    /// because nothing counted them, not because there was nothing to count.
    Refused(&'static str),
    /// Compared: the documents agree, so an apply changes nothing.
    Agreed,
    /// Compared: they differ.
    Differs,
}

impl Diff {
    pub fn verdict(&self) -> Verdict {
        if let Some(reason) = self.refused {
            return Verdict::Refused(reason);
        }
        if self.counts.mine + self.counts.theirs + self.counts.conflict + self.counts.undeclared > 0
        {
            return Verdict::Differs;
        }
        Verdict::Agreed
    }
}

pub fn three_way(sides: Sides<'_>) -> Diff {
    let two_way = sides.base.is_none();
    if sides.live.len() > MAX_SIDE_BYTES
        || sides.buffer.len() > MAX_SIDE_BYTES
        || sides.base.is_some_and(|base| base.len() > MAX_SIDE_BYTES)
    {
        return refused(
            two_way,
            "one side of this comparison is larger than the 8 MiB the diff aligns",
        );
    }

    let base_text = sides.base.unwrap_or(sides.live);
    let Some(base_lines) = split_lines(base_text) else {
        return refused(
            two_way,
            "one side of this comparison has more than 65,536 lines",
        );
    };
    // In a two-way comparison base is live. Share both line bounds and interned
    // IDs instead of allocating an identical second copy of each.
    let live_lines = if two_way {
        None
    } else {
        let Some(lines) = split_lines(sides.live) else {
            return refused(
                two_way,
                "one side of this comparison has more than 65,536 lines",
            );
        };
        Some(lines)
    };
    let Some(buffer_lines) = split_lines(sides.buffer) else {
        return refused(
            two_way,
            "one side of this comparison has more than 65,536 lines",
        );
    };
    let live_lines = live_lines.as_deref().unwrap_or(&base_lines);

    let mut interner: HashMap<&str, u32> = HashMap::new();
    let base_ids = intern(&mut interner, base_text, &base_lines);
    let live_ids = if two_way {
        None
    } else {
        Some(intern(&mut interner, sides.live, live_lines))
    };
    let live_ids = live_ids.as_deref().unwrap_or(&base_ids);
    let buffer_ids = intern(&mut interner, sides.buffer, &buffer_lines);

    let mut budget = MAX_TOTAL_CELLS;
    // With no base the base *is* live, so the alignment is the identity and
    // hashing it again would only cost time.
    let (base_to_live, mut coarse) = if two_way {
        (
            (0..base_ids.len() as u32).map(|at| (at, at)).collect(),
            false,
        )
    } else {
        align(&base_ids, live_ids, &mut budget)
    };
    let (base_to_buffer, buffer_coarse) = align(&base_ids, &buffer_ids, &mut budget);
    coarse |= buffer_coarse;

    let live_of = side_map(base_ids.len(), &base_to_live);
    let buffer_of = side_map(base_ids.len(), &base_to_buffer);

    let mut stable: Vec<(u32, u32, u32)> = Vec::new();
    for at in 0..base_ids.len() {
        if let (Some(live), Some(buffer)) = (live_of[at], buffer_of[at]) {
            stable.push((at as u32, live, buffer));
        }
    }

    let mut build = Build {
        rows: Vec::new(),
        hunks: Vec::new(),
        counts: Counts::default(),
        base: &base_lines,
        live: live_lines,
        buffer: &buffer_lines,
        buffer_bytes: sides.buffer.len() as u32,
    };

    let mut base_at = 0usize;
    let mut live_at = 0usize;
    let mut buffer_at = 0usize;
    let mut next = 0usize;
    loop {
        while let Some(&(at, live, buffer)) = stable.get(next) {
            if at as usize != base_at || live as usize != live_at || buffer as usize != buffer_at {
                break;
            }
            build.open(Origin::Common, buffer_at..buffer_at + 1);
            build.push(Side::Live, Origin::Common, live_at);
            base_at += 1;
            live_at += 1;
            buffer_at += 1;
            next += 1;
        }
        if base_at >= base_ids.len() && live_at >= live_ids.len() && buffer_at >= buffer_ids.len() {
            break;
        }
        let (base_end, live_end, buffer_end) = match stable.get(next) {
            Some(&(at, live, buffer)) => (at as usize, live as usize, buffer as usize),
            None => (base_ids.len(), live_ids.len(), buffer_ids.len()),
        };
        // Every region between two stable points has content on at least one
        // side, so this always advances; the guard is here because a loop that
        // depends on an alignment invariant should not be able to hang if the
        // invariant is ever broken.
        if base_end <= base_at && live_end <= live_at && buffer_end <= buffer_at {
            debug_assert!(false, "a region between two stable points must advance");
            return refused(
                two_way,
                "the comparison could not be aligned safely; no partial diff was shown",
            );
        }
        let origin = classify(
            &base_ids[base_at..base_end],
            &live_ids[live_at..live_end],
            &buffer_ids[buffer_at..buffer_end],
        );
        // A region both sides removed is agreement with nothing to show: the
        // live run is empty, so no row is pushed for it, and a hunk opened for
        // it would be the one empty hunk in an otherwise row-backed list.
        if origin != Origin::Common || live_at < live_end {
            build.open(origin, buffer_at..buffer_end);
        }
        match origin {
            Origin::Common => {
                for line in live_at..live_end {
                    build.push(Side::Live, Origin::Common, line);
                }
            }
            Origin::Conflict => {
                build.region(Side::Live, origin, live_at..live_end);
                build.region(Side::Base, origin, base_at..base_end);
                build.region(Side::Buffer, origin, buffer_at..buffer_end);
            }
            // The base is what is empty in an undeclared region, so there is no
            // base row to show and it renders like the two-sided ones.
            Origin::Mine | Origin::Theirs | Origin::Undeclared => {
                build.region(Side::Live, origin, live_at..live_end);
                build.region(Side::Buffer, origin, buffer_at..buffer_end);
            }
        }
        base_at = base_end;
        live_at = live_end;
        buffer_at = buffer_end;
    }

    Diff {
        rows: build.rows,
        hunks: build.hunks,
        counts: build.counts,
        two_way,
        coarse,
        final_newline_differs: newline_differs(&sides),
        refused: None,
    }
}

pub(crate) fn refused(two_way: bool, reason: &'static str) -> Diff {
    Diff {
        two_way,
        refused: Some(reason),
        ..Diff::default()
    }
}

struct Build<'a> {
    rows: Vec<Row>,
    hunks: Vec<Hunk>,
    counts: Counts,
    base: &'a [Line],
    live: &'a [Line],
    buffer: &'a [Line],
    buffer_bytes: u32,
}

impl Build<'_> {
    // Open the hunk the rows about to be pushed belong to, or extend the one
    // already open when it shares their origin. Hunks are built here rather than
    // grouped out of the finished rows afterwards because this is the only place
    // that knows the buffer coordinate: a region with no buffer rows in it has
    // no trace of the buffer left in its rows, and its place in the buffer is
    // what an edit taking the cluster's side of it needs.
    fn open(&mut self, origin: Origin, buffer_lines: Range<usize>) {
        let span = self.buffer_span(buffer_lines);
        match self.hunks.last_mut() {
            Some(hunk) if hunk.origin == origin => hunk.buffer_end = hunk.buffer_end.max(span.end),
            _ => self.hunks.push(Hunk {
                origin,
                rows: self.rows.len()..self.rows.len(),
                buffer_start: span.start,
                buffer_end: span.end,
            }),
        }
    }

    // Byte bounds for a run of buffer lines. An empty run still has a place: the
    // start of the line that follows it, which is a line boundary, or the end of
    // the document when nothing follows.
    fn buffer_span(&self, lines: Range<usize>) -> Range<u32> {
        match self.buffer.get(lines.clone()) {
            Some([first, .., last]) => first.start..last.end,
            Some([only]) => only.start..only.end,
            // Empty, or -- for a run that starts past the last line -- absent.
            _ => match self.buffer.get(lines.start) {
                Some(next) => next.start..next.start,
                None => self.buffer_bytes..self.buffer_bytes,
            },
        }
    }

    fn push(&mut self, side: Side, origin: Origin, line: usize) {
        let lines = match side {
            Side::Base => self.base,
            Side::Live => self.live,
            Side::Buffer => self.buffer,
        };
        let bounds = lines[line];
        self.rows.push(Row {
            side,
            origin,
            start: bounds.start,
            end: bounds.end,
        });
        match self.hunks.last_mut() {
            Some(hunk) => {
                debug_assert_eq!(
                    hunk.origin, origin,
                    "every row belongs to the hunk opened for it"
                );
                hunk.rows.end = self.rows.len();
            }
            None => debug_assert!(false, "a row is pushed into an opened hunk"),
        }
    }

    fn region(&mut self, side: Side, origin: Origin, lines: Range<usize>) {
        match side {
            Side::Live => self.counts.removed += lines.len() as u32,
            Side::Buffer => self.counts.added += lines.len() as u32,
            Side::Base => {}
        }
        if side == Side::Live {
            match origin {
                Origin::Mine => self.counts.mine += 1,
                Origin::Theirs => self.counts.theirs += 1,
                Origin::Conflict => self.counts.conflict += 1,
                Origin::Undeclared => self.counts.undeclared += 1,
                Origin::Common => {}
            }
        }
        for line in lines {
            self.push(side, origin, line);
        }
    }
}

// A region both sides moved to the same place is a region an apply leaves
// alone, whatever the base said: the buffer already holds what the cluster
// holds. Reporting it as a change would be a change nobody can act on.
fn classify(base: &[u32], live: &[u32], buffer: &[u32]) -> Origin {
    let live_changed = live != base;
    let buffer_changed = buffer != base;
    match (live_changed, buffer_changed) {
        (false, false) => Origin::Common,
        // The base and the cluster agree there is nothing here and the buffer
        // added something: an edit, whatever the base is.
        (false, true) => Origin::Mine,
        (true, true) if live == buffer => Origin::Common,
        // A base with nothing here did not lose an argument: the ordinary reason
        // it is silent is that nobody declared the field and the server filled
        // it in. Calling that a conflict promises a refusal the server has not
        // made -- and the dry run, not this alignment, is what can say otherwise.
        //
        // This covers *both* remaining shapes, which is the correction a reviewer
        // found: the buffer holding different text, and the buffer holding none.
        // The second one used to be `Theirs`, whose label says "the cluster
        // changed this; applying reverts it" -- and an apply does not revert a
        // field this client never declared, so that was the same false promise
        // one arm over.
        (true, _) if base.is_empty() => Origin::Undeclared,
        (true, false) => Origin::Theirs,
        (true, true) => Origin::Conflict,
    }
}

/// The buffer edit that makes one hunk read the way the cluster reads it: which
/// bytes of the buffer to replace, and with what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keep {
    pub range: Range<usize>,
    pub text: String,
    /// Lines taken from the cluster's document, and lines of the buffer they
    /// replace, so a view can say what it did rather than that something
    /// happened.
    pub taken: usize,
    pub dropped: usize,
}

/// The edit that keeps the cluster's side of one hunk, or None when there is
/// nothing to keep: no such hunk, or one the three documents agree about.
///
/// This is the acting half of the three-way classification. Naming drift an
/// apply would revert is only half an answer if the only way to keep it is to
/// retype it, and the ranges to do it with are already here -- the hunk's rows
/// on the cluster's side, and the hunk's own span on the buffer's.
///
/// The replacement is composed line by line from the cluster's rows rather than
/// sliced whole out of the live document, so a line ending spelled differently
/// on the two sides stays on its own side instead of being pasted into the
/// buffer. Where the buffer has no lines at all the span is empty but placed at
/// a line boundary, and the inserted text carries the terminator that boundary
/// implies; where the cluster has none, the buffer's terminator goes with the
/// text it terminated, or the deletion would leave the blank line behind.
pub fn keep_theirs(diff: &Diff, hunk: usize, live: &str, buffer: &str) -> Option<Keep> {
    let hunk = diff.hunks.get(hunk)?;
    if hunk.origin == Origin::Common {
        return None;
    }
    let rows = diff.rows.get(hunk.rows.clone())?;
    let taken: Vec<&str> = rows
        .iter()
        .filter(|row| row.side == Side::Live)
        .map(|row| live.get(row.bytes()).unwrap_or(""))
        .collect();
    let dropped = rows.iter().filter(|row| row.side == Side::Buffer).count();
    let mut range = hunk.buffer();
    let ending = line_ending(buffer, range.start);
    let mut text = taken.join(ending);
    if taken.is_empty() {
        range.end += terminator(buffer, range.end);
    // Whether the buffer has lines here, which is *not* the same question as
    // whether its span is empty: a blank line's bytes are empty too, so a run of
    // one blank line spans zero bytes at a real position. Discriminating on the
    // span appended a terminator the blank line already had, and left it behind.
    } else if dropped == 0 {
        // Appending to a document whose last line has no terminator needs one
        // first, or the two lines arrive spliced into one.
        if range.start == buffer.len() && !buffer.is_empty() && !buffer.ends_with('\n') {
            text.insert_str(0, ending);
        }
        text.push_str(ending);
    }
    Some(Keep {
        range,
        text,
        taken: taken.len(),
        dropped,
    })
}

// How the line before `at` was terminated, so an edit spells its own line
// endings the way the document around it does rather than the way the cluster's
// emitter does.
fn line_ending(text: &str, at: usize) -> &'static str {
    match text.get(..at) {
        Some(before) if before.ends_with("\r\n") => "\r\n",
        _ => "\n",
    }
}

// The one line terminator at `at`, in bytes, if that is what is there.
pub(crate) fn terminator(text: &str, at: usize) -> usize {
    match text.get(at..) {
        Some(rest) if rest.starts_with("\r\n") => 2,
        Some(rest) => usize::from(rest.starts_with('\n')),
        None => 0,
    }
}

// An empty document has no opinion about its own last byte, so it never
// disagrees with one that does.
fn newline_differs(sides: &Sides<'_>) -> bool {
    let mut ends: Vec<bool> = Vec::with_capacity(3);
    for text in [Some(sides.live), Some(sides.buffer), sides.base]
        .into_iter()
        .flatten()
    {
        if !text.is_empty() {
            ends.push(text.ends_with('\n'));
        }
    }
    ends.windows(2).any(|pair| pair[0] != pair[1])
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Line {
    start: u32,
    end: u32,
}

impl Line {
    pub(crate) fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

// Line terminators are excluded from the range but do terminate the line, so a
// document ending in a newline has no phantom final line and one that does not
// still ends with a real one.
pub(crate) fn split_lines(text: &str) -> Option<Vec<Line>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (at, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            if lines.len() == MAX_SIDE_LINES {
                return None;
            }
            // CRLF is one line terminator. Keeping its CR in the row makes an
            // LF/CRLF comparison paint an invisible one-character change on
            // every line and contradicts Row::bytes' terminator-free contract.
            let end = at - usize::from(at > start && text.as_bytes()[at - 1] == b'\r');
            lines.push(Line {
                start: start as u32,
                end: end as u32,
            });
            start = at + 1;
        }
    }
    if start < text.len() {
        if lines.len() == MAX_SIDE_LINES {
            return None;
        }
        lines.push(Line {
            start: start as u32,
            end: text.len() as u32,
        });
    }
    Some(lines)
}

fn intern<'a>(ids: &mut HashMap<&'a str, u32>, text: &'a str, lines: &[Line]) -> Vec<u32> {
    lines
        .iter()
        .map(|line| {
            let next = ids.len() as u32;
            *ids.entry(&text[line.range()]).or_insert(next)
        })
        .collect()
}

fn side_map(len: usize, matched: &[(u32, u32)]) -> Vec<Option<u32>> {
    let mut map = vec![None; len];
    for (left, right) in matched {
        map[*left as usize] = Some(*right);
    }
    map
}

// Matched line pairs, ascending in both coordinates. The bool says a region ran
// out of budget and is left unmatched on purpose, which the caller reports
// rather than hides.
pub(crate) fn align(left: &[u32], right: &[u32], budget: &mut usize) -> (Vec<(u32, u32)>, bool) {
    let mut matched = Vec::new();
    let mut coarse = false;
    let mut stack = vec![(0usize, left.len(), 0usize, right.len())];
    while let Some((mut left_start, mut left_end, mut right_start, mut right_end)) = stack.pop() {
        while left_start < left_end
            && right_start < right_end
            && left[left_start] == right[right_start]
        {
            matched.push((left_start as u32, right_start as u32));
            left_start += 1;
            right_start += 1;
        }
        while left_start < left_end
            && right_start < right_end
            && left[left_end - 1] == right[right_end - 1]
        {
            left_end -= 1;
            right_end -= 1;
            matched.push((left_end as u32, right_end as u32));
        }
        if left_start == left_end || right_start == right_end {
            continue;
        }
        let span = (left_end - left_start) + (right_end - right_start);
        if span > *budget {
            coarse = true;
            continue;
        }
        *budget -= span;
        let spine = anchors(&left[left_start..left_end], &right[right_start..right_end]);
        if spine.is_empty() {
            let cells = (left_end - left_start).saturating_mul(right_end - right_start);
            let cost = cells.saturating_mul(2);
            if cells > MAX_GAP_CELLS || cost > *budget {
                coarse = true;
                continue;
            }
            *budget -= cost;
            let left_gap = &left[left_start..left_end];
            let right_gap = &right[right_start..right_end];
            if right_gap.len() <= left_gap.len() {
                hirschberg(
                    left_gap,
                    right_gap,
                    left_start as u32,
                    right_start as u32,
                    &mut matched,
                );
            } else {
                // Hirschberg's scratch rows are sized by its second argument.
                // Put the shorter side there and transpose the pairs back.
                let mut transposed = Vec::new();
                hirschberg(
                    right_gap,
                    left_gap,
                    right_start as u32,
                    left_start as u32,
                    &mut transposed,
                );
                matched.extend(transposed.into_iter().map(|(right, left)| (left, right)));
            }
            continue;
        }
        let mut left_at = left_start;
        let mut right_at = right_start;
        for (anchor_left, anchor_right) in &spine {
            let on_left = left_start + *anchor_left as usize;
            let on_right = right_start + *anchor_right as usize;
            if on_left > left_at && on_right > right_at {
                stack.push((left_at, on_left, right_at, on_right));
            }
            matched.push((on_left as u32, on_right as u32));
            left_at = on_left + 1;
            right_at = on_right + 1;
        }
        if left_at < left_end && right_at < right_end {
            stack.push((left_at, left_end, right_at, right_end));
        }
    }
    matched.sort_unstable();
    (matched, coarse)
}

// Lines occurring exactly once on each side, paired and reduced to a longest
// increasing subsequence: the patience spine. A line that repeats says nothing
// about which of its copies corresponds to which, so it is not an anchor.
fn anchors(left: &[u32], right: &[u32]) -> Vec<(u32, u32)> {
    #[derive(Default, Clone, Copy)]
    struct Sighting {
        on_left: u32,
        left_at: u32,
        on_right: u32,
        right_at: u32,
    }
    let mut seen: HashMap<u32, Sighting> = HashMap::new();
    for (at, line) in left.iter().enumerate() {
        let sighting = seen.entry(*line).or_default();
        sighting.on_left += 1;
        if sighting.on_left == 1 {
            sighting.left_at = at as u32;
        }
    }
    for (at, line) in right.iter().enumerate() {
        let sighting = seen.entry(*line).or_default();
        sighting.on_right += 1;
        if sighting.on_right == 1 {
            sighting.right_at = at as u32;
        }
    }
    let mut unique: Vec<(u32, u32)> = seen
        .values()
        .filter(|sighting| sighting.on_left == 1 && sighting.on_right == 1)
        .map(|sighting| (sighting.left_at, sighting.right_at))
        .collect();
    unique.sort_unstable();
    longest_increasing(&unique)
}

// Patience sorting over the right-hand positions. Every position is distinct --
// the pairs come from lines unique on both sides -- so the run is strictly
// increasing without a tie rule.
fn longest_increasing(pairs: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut tails: Vec<usize> = Vec::new();
    let mut before: Vec<usize> = vec![usize::MAX; pairs.len()];
    for (at, pair) in pairs.iter().enumerate() {
        let found = tails.partition_point(|candidate| pairs[*candidate].1 < pair.1);
        if found > 0 {
            before[at] = tails[found - 1];
        }
        if found == tails.len() {
            tails.push(at);
        } else {
            tails[found] = at;
        }
    }
    let mut chain = Vec::with_capacity(tails.len());
    let mut at = match tails.last() {
        Some(last) => *last,
        None => return chain,
    };
    loop {
        chain.push(pairs[at]);
        if before[at] == usize::MAX {
            break;
        }
        at = before[at];
    }
    chain.reverse();
    chain
}

// An exact longest common subsequence in space linear in the second side:
// split the left half in two, find the column where the forward and backward
// LCS lengths sum highest, and recurse on both halves. Recursion halves the
// left side, so the depth is logarithmic in it. The caller puts the shorter
// input second and transposes the result when necessary.
pub(crate) fn hirschberg(
    left: &[u32],
    right: &[u32],
    left_at: u32,
    right_at: u32,
    matched: &mut Vec<(u32, u32)>,
) {
    if left.is_empty() || right.is_empty() {
        return;
    }
    if left.len() == 1 {
        if let Some(at) = right.iter().position(|line| *line == left[0]) {
            matched.push((left_at, right_at + at as u32));
        }
        return;
    }
    let middle = left.len() / 2;
    let head = lcs_lengths(&left[..middle], right);
    let tail = lcs_lengths_reversed(&left[middle..], right);
    let split = (0..=right.len())
        .max_by_key(|at| head[*at] + tail[*at])
        .expect("the column range is never empty");
    hirschberg(&left[..middle], &right[..split], left_at, right_at, matched);
    hirschberg(
        &left[middle..],
        &right[split..],
        left_at + middle as u32,
        right_at + split as u32,
        matched,
    );
}

// `row[at]` is the length of the longest common subsequence of `left` and
// `right[..at]`.
fn lcs_lengths(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut previous = vec![0u32; right.len() + 1];
    let mut current = vec![0u32; right.len() + 1];
    for line in left {
        for at in 0..right.len() {
            current[at + 1] = if *line == right[at] {
                previous[at] + 1
            } else {
                current[at].max(previous[at + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous
}

// `row[at]` is the length of the longest common subsequence of `left` and
// `right[at..]`.
fn lcs_lengths_reversed(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut previous = vec![0u32; right.len() + 1];
    let mut current = vec![0u32; right.len() + 1];
    for line in left.iter().rev() {
        for at in (0..right.len()).rev() {
            current[at] = if *line == right[at] {
                previous[at + 1] + 1
            } else {
                current[at + 1].max(previous[at])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous
}
