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
//! A [`Row`] is twelve bytes and carries a byte range into its own side rather
//! than a copied line, so the diff of a megabyte is a vector rather than a
//! second copy of the document, and only the visible rows are ever composed
//! into runs. It held a line number too, until nothing was found to be reading
//! it: at the 196,608-row ceiling that field was 786 KB of answers to a
//! question no caller asks.

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
const MAX_TOTAL_CELLS: usize = 8 << 20;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub mine: u32,
    pub theirs: u32,
    pub conflict: u32,
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
        if self.counts.mine + self.counts.theirs + self.counts.conflict > 0 {
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
        counts: Counts::default(),
        base: &base_lines,
        live: live_lines,
        buffer: &buffer_lines,
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
            Origin::Mine | Origin::Theirs => {
                build.region(Side::Live, origin, live_at..live_end);
                build.region(Side::Buffer, origin, buffer_at..buffer_end);
            }
        }
        base_at = base_end;
        live_at = live_end;
        buffer_at = buffer_end;
    }

    let hunks = group(&build.rows);
    Diff {
        rows: build.rows,
        hunks,
        counts: build.counts,
        two_way,
        coarse,
        final_newline_differs: newline_differs(&sides),
        refused: None,
    }
}

fn refused(two_way: bool, reason: &'static str) -> Diff {
    Diff {
        two_way,
        refused: Some(reason),
        ..Diff::default()
    }
}

struct Build<'a> {
    rows: Vec<Row>,
    counts: Counts,
    base: &'a [Line],
    live: &'a [Line],
    buffer: &'a [Line],
}

impl Build<'_> {
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
        (false, true) => Origin::Mine,
        (true, false) => Origin::Theirs,
        (true, true) if live == buffer => Origin::Common,
        (true, true) => Origin::Conflict,
    }
}

fn group(rows: &[Row]) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for (at, row) in rows.iter().enumerate() {
        match hunks.last_mut() {
            Some(hunk) if hunk.origin == row.origin => hunk.rows.end = at + 1,
            _ => hunks.push(Hunk {
                origin: row.origin,
                rows: at..at + 1,
            }),
        }
    }
    hunks
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
struct Line {
    start: u32,
    end: u32,
}

impl Line {
    fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

// Line terminators are excluded from the range but do terminate the line, so a
// document ending in a newline has no phantom final line and one that does not
// still ends with a real one.
fn split_lines(text: &str) -> Option<Vec<Line>> {
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
fn align(left: &[u32], right: &[u32], budget: &mut usize) -> (Vec<(u32, u32)>, bool) {
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
fn hirschberg(
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

#[cfg(test)]
mod tests {
    use super::*;
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
                    let left: Vec<u32> =
                        (0..left_len).map(|at| ((bits >> at) & 1) as u32).collect();
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
            Verdict::Refused(
                "one side of this comparison is larger than the 8 MiB the diff aligns"
            )
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
}
