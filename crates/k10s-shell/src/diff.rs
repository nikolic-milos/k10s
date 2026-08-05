//! The diff item: what an apply would change, reviewed before it happens.
//!
//! Two comparisons live in one view because they answer the same question at
//! different distances. The local one is three-way -- the object as the cluster
//! has it, the object as it was last declared, and the buffer -- and its value
//! is the classification: a hunk only the buffer changed is an edit, a hunk only
//! the cluster changed is drift the apply would revert, and a hunk both changed
//! is a collision. The dry-run one is two-way and authoritative: the API server
//! is handed the payload with `?dryRun=All`, and what comes back is what it
//! *would* store, defaulting and admission webhooks included, so the diff is
//! against the real outcome rather than a client's guess at merge semantics.
//!
//! Applying is a second deliberate press, and forcing is a third: [`ApplyGate`]
//! arms under the name of the thing being asked, so a press that answers a
//! different question re-asks instead of firing -- the same rule the editor's
//! destructive actions follow, and for the same reason. Any recompute disarms,
//! because the thing that was being confirmed is no longer what is on screen.
//!
//! Every precondition a *press* has to clear is in one pure function,
//! [`refuse`], and none of them is anywhere else. That is not tidiness: the two
//! that were not there were the two that failed. The force precondition lived
//! inside a key handler, so it guarded one of the ways in; the diff's own
//! refusal to compare was read by nobody, so an object too large to review
//! could be applied through a review that painted no rows under a footer saying
//! the apply would change nothing. The two conditions that belong to the
//! *bytes* rather than to the press -- a payload the pruner refused, and a wire
//! another request already holds -- are enforced where the bytes leave, in
//! `send`, because a dry run reaches that point without a press at all.
//!
//! Everything that decides is pure -- [`DiffState`], [`ApplyGate`], [`Flight`]
//! and [`refuse`] -- so the destructive rules are tested without a window. The
//! rows are built once per comparison and only the visible ones are ever turned
//! into text, so a diff of a megabyte costs a vector rather than a second copy
//! of the document.

use std::borrow::Cow;
use std::rc::Rc;

use gpui::{
    Context, FocusHandle, IntoElement, ParentElement, Render, Role, ScrollWheelEvent, SharedString,
    Styled, WeakEntity, Window, canvas, div, prelude::*, px, rgb,
};

use k10s_edit::diff::{self, Origin, Side, Verdict};
use k10s_theme::Theme;

use crate::editor::{BufferStamp, DiffSources, EditorView};
use crate::provider::{
    ApplyOutcome, ApplyRequest, Conflicted, DescribeRequest, ReadProvider, Reply,
};
use crate::ui::{CONTENT_PADDING, STATUS_BAR_HEIGHT, Viewport};

// How much unchanged text stays visible around a change when the folded view is
// on. Three lines is git's default and is enough to place a hunk in a manifest.
const CONTEXT_LINES: usize = 3;

// A conflict list is server data; the panel that renders it is bounded like
// every other buffer in the shell.
const MAX_SHOWN_CAUSES: usize = 12;

/// Which comparison is on screen. The same rows mean different things in each,
/// so the labelling is not cosmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Live, last-applied and the buffer, from the editor alone.
    Local,
    /// Live against what the server answered it would store.
    DryRun,
}

/// One rendered line: a line of one of the documents, a synthesised header
/// naming who changed the hunk that follows, or a fold standing in for
/// unchanged text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    Line(usize),
    Note(Origin),
    Folded(usize),
}

/// What a row looks like once resolved: the gutter mark, what class of change it
/// belongs to, and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Painted<'a> {
    pub mark: char,
    pub origin: Origin,
    pub side: Option<Side>,
    pub text: Cow<'a, str>,
}

impl Painted<'_> {
    /// The row exactly as it is painted, so nothing can hold a second opinion
    /// about where the gutter ends. A synthesised note or fold is not a line of
    /// any document, so its text sits one column past the ones that are.
    pub fn rendered(&self) -> String {
        if self.side.is_none() {
            format!("{} {}", self.mark, self.text)
        } else {
            format!("{}{}", self.mark, self.text)
        }
    }
}

#[derive(Debug, Default)]
pub struct DiffState {
    live: String,
    base: Option<String>,
    other: String,
    mode: Option<Mode>,
    diff: diff::Diff,
    cells: Vec<Cell>,
    top: usize,
    viewport: usize,
    folded: bool,
}

impl DiffState {
    pub fn new() -> DiffState {
        DiffState {
            viewport: 4,
            folded: true,
            ..DiffState::default()
        }
    }

    pub fn set(&mut self, mode: Mode, live: String, base: Option<String>, other: String) {
        self.live = live;
        self.base = base;
        self.other = other;
        self.mode = Some(mode);
        self.diff = diff::three_way(diff::Sides {
            base: self.base.as_deref(),
            live: &self.live,
            buffer: &self.other,
        });
        self.rebuild();
        self.top = 0;
    }

    pub fn mode(&self) -> Option<Mode> {
        self.mode
    }

    pub fn diff(&self) -> &diff::Diff {
        &self.diff
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn top(&self) -> usize {
        self.top
    }

    pub fn folded(&self) -> bool {
        self.folded
    }

    pub fn toggle_folded(&mut self) {
        // A cell index means something different on either side of a fold, so
        // what is held across the rebuild is the document row the viewport was
        // showing, not the row's position in a list that is about to change.
        let anchor = self.row_at_top();
        self.folded = !self.folded;
        self.rebuild();
        self.top = anchor
            .map(|row| self.cell_showing(row))
            .unwrap_or(0)
            .min(self.max_top());
    }

    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        self.top = self.top.min(self.max_top());
    }

    pub fn scroll_by(&mut self, delta: i64) {
        self.top = self
            .top
            .saturating_add_signed(delta as isize)
            .min(self.max_top());
    }

    pub fn page_by(&mut self, pages: i64) {
        let step = (self.viewport.saturating_sub(1).max(1)) as i64;
        self.scroll_by(pages.saturating_mul(step));
    }

    pub fn home(&mut self) {
        self.top = 0;
    }

    pub fn end(&mut self) {
        self.top = self.max_top();
    }

    /// Jump to the next hunk that is not `Common`; false when there is none
    /// ahead, so the caller can say so rather than paint an unchanged view.
    pub fn next_change(&mut self) -> bool {
        let found = self
            .cells
            .iter()
            .enumerate()
            .skip(self.top + 1)
            .find(|(_, cell)| matches!(cell, Cell::Note(_)))
            .map(|(at, _)| at);
        match found {
            Some(at) => {
                self.top = at.min(self.max_top());
                true
            }
            None => false,
        }
    }

    pub fn prev_change(&mut self) -> bool {
        let found = self
            .cells
            .iter()
            .enumerate()
            .take(self.top)
            .rfind(|(_, cell)| matches!(cell, Cell::Note(_)))
            .map(|(at, _)| at);
        match found {
            Some(at) => {
                self.top = at;
                true
            }
            None => false,
        }
    }

    pub fn visible(&self) -> impl Iterator<Item = Painted<'_>> {
        let end = (self.top + self.viewport).min(self.cells.len());
        self.cells[self.top.min(self.cells.len())..end]
            .iter()
            .map(|cell| self.paint(*cell))
    }

    fn paint(&self, cell: Cell) -> Painted<'_> {
        match cell {
            Cell::Note(origin) => Painted {
                mark: ' ',
                origin,
                side: None,
                text: Cow::Borrowed(self.note(origin)),
            },
            Cell::Folded(lines) => Painted {
                mark: ' ',
                origin: Origin::Common,
                side: None,
                text: Cow::Owned(format!("... {lines} unchanged")),
            },
            Cell::Line(at) => {
                let row = self.diff.rows[at];
                let text = match row.side {
                    Side::Live => &self.live[row.bytes()],
                    Side::Buffer => &self.other[row.bytes()],
                    // A base row exists only in a conflict, which needs a base
                    // to have been found; the fallback keeps a row that cannot
                    // happen from being a panic if it ever does.
                    Side::Base => self
                        .base
                        .as_deref()
                        .and_then(|base| base.get(row.bytes()))
                        .unwrap_or(""),
                };
                Painted {
                    mark: mark_of(row.side, row.origin),
                    origin: row.origin,
                    side: Some(row.side),
                    text: Cow::Borrowed(text),
                }
            }
        }
    }

    fn note(&self, origin: Origin) -> &'static str {
        match (self.mode, origin) {
            (Some(Mode::DryRun), _) => "the apply would change this",
            (_, Origin::Mine) => "you changed this",
            (_, Origin::Theirs) => "the cluster changed this; applying reverts it",
            (_, Origin::Conflict) => "both changed this since the last apply",
            // Not "both changed this": nobody declared it, so nothing was taken
            // from anyone and no refusal is coming. What an apply does to a field
            // it never declared is the dry run's question, not this alignment's.
            (_, Origin::Undeclared) => {
                "the last apply declared nothing here; the dry run says what applying does"
            }
            (_, Origin::Common) => "unchanged",
        }
    }

    /// Which hunk the viewport is reading, which is the one an action acts on.
    /// A note stands above its own hunk's rows, so landing on one -- where the
    /// next-change key leaves the reader -- answers with the hunk it heads
    /// rather than the one that ended before it.
    pub(crate) fn hunk_at_top(&self) -> Option<usize> {
        let row = self.row_at_top()?;
        self.diff
            .hunks
            .iter()
            .position(|hunk| hunk.rows.contains(&row))
    }

    pub(crate) fn origin_of(&self, hunk: usize) -> Option<Origin> {
        Some(self.diff.hunks.get(hunk)?.origin)
    }

    /// The buffer edit that keeps the cluster's side of one hunk. The two
    /// documents its ranges point into are this state's own, which is why the
    /// call lives here: in [`Mode::DryRun`] the right-hand document is the
    /// server's answer rather than the editor's buffer, so an edit derived from
    /// it would splice one document's ranges into another. [`refuse_keep`] is
    /// what keeps that press from arriving.
    pub(crate) fn keep(&self, hunk: usize) -> Option<diff::Keep> {
        debug_assert_eq!(
            self.mode,
            Some(Mode::Local),
            "only the local comparison's right-hand side is the editor's buffer"
        );
        diff::keep_theirs(&self.diff, hunk, &self.live, &self.other)
    }

    /// One line naming what the comparison found, for the footer.
    pub fn summary(&self) -> String {
        let Some(mode) = self.mode else {
            return "comparing...".to_string();
        };
        let verdict = self.diff.verdict();
        if let Verdict::Refused(reason) = verdict {
            return reason.to_string();
        }
        let counts = self.diff.counts;
        let mut pieces = Vec::new();
        pieces.push(match mode {
            Mode::Local => "against live".to_string(),
            Mode::DryRun => "against the server's dry run".to_string(),
        });
        if verdict == Verdict::Agreed {
            pieces.push("no differences".to_string());
        } else {
            pieces.push(format!("+{} -{}", counts.added, counts.removed));
            if counts.mine > 0 {
                pieces.push(format!("{} yours", counts.mine));
            }
            if counts.theirs > 0 {
                pieces.push(format!("{} reverted by applying", counts.theirs));
            }
            if counts.conflict > 0 {
                pieces.push(format!("{} conflicting", counts.conflict));
            }
            // Kept out of the conflict count on purpose: folded in, a summary
            // reports a refusal the server has not made over a field nobody
            // declared.
            if counts.undeclared > 0 {
                pieces.push(format!("{} undeclared", counts.undeclared));
            }
        }
        if mode == Mode::Local && self.diff.two_way {
            pieces.push("no last-applied-configuration, so this is two-way".to_string());
        }
        if self.diff.coarse {
            pieces.push("a region diverged too far to align line by line".to_string());
        }
        if self.diff.final_newline_differs {
            pieces.push("the final newline differs".to_string());
        }
        // One hint, on the line that already carries them, rather than one on
        // every hunk header: the action is in the registry, so the palette lists
        // it with its key like every other command.
        if mode == Mode::Local && verdict == Verdict::Differs {
            pieces.push("t keeps the cluster's side of a hunk".to_string());
        }
        if self.folded && verdict == Verdict::Differs {
            pieces.push("folded".to_string());
        }
        pieces.join("  ·  ")
    }

    fn max_top(&self) -> usize {
        self.cells.len().saturating_sub(1)
    }

    // The first document row the viewport is showing. A note or a fold stands in
    // for the rows after it rather than being one, so the nearest following line
    // is what answers for it.
    fn row_at_top(&self) -> Option<usize> {
        self.cells
            .get(self.top.min(self.cells.len())..)?
            .iter()
            .find_map(|cell| match cell {
                Cell::Line(at) => Some(*at),
                Cell::Note(_) | Cell::Folded(_) => None,
            })
    }

    // Where that row went. Folding can hide the exact row, so the first one at
    // or after it is the answer -- and when the fold swallowed everything after
    // it, the end is, because a reader near the end of a document belongs near
    // the end of it and not back at the top. The note above a hunk comes along
    // when there is one: a hunk read without its header is a hunk without its
    // reason.
    fn cell_showing(&self, row: usize) -> usize {
        let found = self
            .cells
            .iter()
            .position(|cell| matches!(cell, Cell::Line(at) if *at >= row))
            .unwrap_or_else(|| self.max_top());
        match found.checked_sub(1) {
            Some(before) if matches!(self.cells.get(before), Some(Cell::Note(_))) => before,
            _ => found,
        }
    }

    fn rebuild(&mut self) {
        self.cells = build(&self.diff, self.folded);
        self.top = self.top.min(self.max_top());
    }
}

// A hunk of unchanged text between two changes keeps the context nearest each of
// them and folds the middle; the run before the first change and the run after
// the last one only need the side facing a change.
fn build(diff: &diff::Diff, folded: bool) -> Vec<Cell> {
    let mut cells = Vec::new();
    let last = diff.hunks.len().saturating_sub(1);
    for (at, hunk) in diff.hunks.iter().enumerate() {
        if hunk.origin != Origin::Common {
            cells.push(Cell::Note(hunk.origin));
            cells.extend(hunk.rows.clone().map(Cell::Line));
            continue;
        }
        if !folded {
            cells.extend(hunk.rows.clone().map(Cell::Line));
            continue;
        }
        let len = hunk.rows.len();
        let leading = if at == 0 { 0 } else { CONTEXT_LINES.min(len) };
        let trailing = if at == last {
            0
        } else {
            CONTEXT_LINES.min(len - leading)
        };
        let hidden = len - leading - trailing;
        cells.extend(hunk.rows.clone().take(leading).map(Cell::Line));
        if hidden > 0 {
            cells.push(Cell::Folded(hidden));
        }
        cells.extend(hunk.rows.clone().skip(len - trailing).map(Cell::Line));
    }
    cells
}

fn mark_of(side: Side, origin: Origin) -> char {
    match (origin, side) {
        (Origin::Common, _) => ' ',
        (_, Side::Live) => '-',
        (_, Side::Buffer) => '+',
        // The base is neither removed nor added: it is what both sides moved
        // away from.
        (_, Side::Base) => '|',
    }
}

/// Which destructive question is armed. One bit could not tell an apply from a
/// force, and a press meant for one would answer the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Armed {
    Apply,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Ask,
    Go,
}

/// Which request owns the wire. A bare flag was not enough: the clear lived
/// after the guard that discards a reply belonging to a superseded comparison,
/// so a recompute during an apply stranded the flag and every later apply was
/// refused as "already in flight". A ticket separates the two questions -- who
/// holds the wire, and whose answer is still wanted -- and only the first one
/// decides when it is released.
#[derive(Debug, Default)]
pub(crate) struct Flight {
    holder: Option<u64>,
    // What the holder is. A dry run and an apply travel the same seam and only
    // one of them writes, so a message that calls a dry run "an apply" tells
    // the user a write is happening that is not.
    holds_apply: bool,
    // A comparison asked for while the wire was held. A dry run changes
    // nothing, so the honest answer to "compare this again" is to do it when
    // the wire frees rather than to drop the question and leave the tab under a
    // message about a request that has since finished. An apply is never
    // queued: a write has to be the press the user just made.
    owed: bool,
    issued: u64,
}

impl Flight {
    /// A ticket, or None when a request is already out.
    pub(crate) fn take(&mut self, dry_run: bool) -> Option<u64> {
        if self.holder.is_some() {
            return None;
        }
        self.issued += 1;
        self.holder = Some(self.issued);
        self.holds_apply = !dry_run;
        self.holder
    }

    /// Remember that a comparison was asked for while the wire was busy.
    pub(crate) fn owe_a_comparison(&mut self) {
        self.owed = true;
    }

    /// Release, but only by the request that holds it: a reply from a request
    /// that was already superseded must not hand the wire to nobody. True when
    /// a comparison was asked for meanwhile and is now owed.
    pub(crate) fn release(&mut self, ticket: u64) -> bool {
        if self.holder != Some(ticket) {
            return false;
        }
        self.holder = None;
        self.holds_apply = false;
        std::mem::take(&mut self.owed)
    }

    pub(crate) fn busy(&self) -> bool {
        self.holder.is_some()
    }

    /// What is on the wire, named the way a person would name it.
    pub(crate) fn holder(&self) -> &'static str {
        if self.holds_apply {
            return "an apply";
        }
        "a dry run"
    }
}

/// Everything a press has to be true about before a write leaves this view,
/// gathered so that [`refuse`] can be a pure function over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ready<'a> {
    /// Discovery says the server takes a patch for this kind at all.
    pub(crate) patchable: bool,
    /// Why the pruner would not build a payload, if it would not.
    pub(crate) blocked: &'a [&'static str],
    /// What the comparison on screen concluded.
    pub(crate) verdict: Verdict,
    /// The buffer this review was made of.
    pub(crate) reviewed: BufferStamp,
    /// The editor's buffer now, or None when the editor is gone.
    pub(crate) editor: Option<BufferStamp>,
    /// The buffer the server has answered a dry run for.
    pub(crate) dry_run: Option<BufferStamp>,
    /// How many fields a conflict named, which is the only thing a force may
    /// take.
    pub(crate) conflicts: usize,
    /// What holds the wire, if anything.
    pub(crate) in_flight: Option<&'static str>,
}

/// Why this press must not become a request, or None when it may. Every rule
/// that guards the wire is here and nowhere else: the two that were not here
/// were the two that let a write through -- a force whose precondition lived in
/// a key handler, and a comparison the diff had refused to make.
///
/// The sentences say what the press did, not what the status line already says
/// standing beside them: a refusal's own reason is the summary, and a blocked
/// payload's reasons are their own piece of that line.
pub(crate) fn refuse(wanted: Armed, at: Ready<'_>) -> Option<String> {
    if !at.patchable {
        return Some("the server serves this kind without a patch verb".to_string());
    }
    // The reasons themselves are already on the status line, standing rather
    // than one-shot, so this says what the press did and not what the line
    // beside it says.
    if !at.blocked.is_empty() {
        return Some("this document cannot be applied, so nothing was sent".to_string());
    }
    // A refusal is not agreement. Zero rows and zero counts mean the comparison
    // never happened, and applying what nobody compared is the thing the whole
    // view exists to prevent. The refusal's own sentence is the summary.
    if matches!(at.verdict, Verdict::Refused(_)) {
        return Some("nothing here has been reviewed, so there is nothing to apply".to_string());
    }
    match at.editor {
        None => {
            return Some("the editor this diff came from is gone; nothing to apply".to_string());
        }
        // The diff *is* the review. If the buffer moved since it was made, what
        // is on screen is not what would be sent, and the only honest answer is
        // to say so and re-compare -- never to send text nobody looked at.
        Some(stamp) if stamp != at.reviewed => {
            return Some(
                "the buffer changed after this comparison; r compares it again".to_string(),
            );
        }
        Some(_) => {}
    }
    // And the dry run *is* the diff's right-hand side. Opening the view against
    // live alone reaches a write whose payload the server never saw, defaulting
    // and admission included, which is a review of a guess.
    if at.dry_run != Some(at.reviewed) {
        return Some(
            "the server has not been asked what this would store; ctrl-alt-r asks it".to_string(),
        );
    }
    if wanted == Armed::Force && at.conflicts == 0 {
        return Some("nothing is owned elsewhere, so there is nothing to force".to_string());
    }
    if let Some(holder) = at.in_flight {
        return Some(format!("{holder} is already in flight"));
    }
    None
}

/// What a press that edits the buffer -- rather than the cluster -- has to be
/// true about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Keepable {
    /// Which comparison is on screen. This is the load-bearing one: in
    /// [`Mode::DryRun`] the right-hand document is what the *server* answered,
    /// not the editor's buffer, so the hunk's ranges point into a document the
    /// editor does not have. An edit built from them would splice bytes at
    /// offsets that mean nothing where they land.
    pub(crate) mode: Option<Mode>,
    /// The hunk the reader is on, if there is one.
    pub(crate) origin: Option<Origin>,
    pub(crate) reviewed: BufferStamp,
    pub(crate) editor: Option<BufferStamp>,
}

/// Why this hunk cannot be taken into the buffer, or None when it can.
///
/// Kept apart from [`refuse`] because nothing here reaches the cluster and none
/// of that function's rules apply: an unpatchable kind, a blocked payload and a
/// missing dry run all say nothing about whether a person may edit their own
/// text. What is shared is the rule that a review is of one buffer -- ranges
/// derived from a comparison of text the editor has since changed splice at the
/// wrong bytes, which in an editor is worse than a refusal.
pub(crate) fn refuse_keep(at: Keepable) -> Option<String> {
    match at.editor {
        None => {
            return Some("the editor this diff came from is gone; nothing to edit".to_string());
        }
        Some(stamp) if stamp != at.reviewed => {
            return Some(
                "the buffer changed after this comparison; r compares it again".to_string(),
            );
        }
        Some(_) => {}
    }
    if at.mode != Some(Mode::Local) {
        return Some(
            "this compares the server's own answer, not the cluster's; ctrl-alt-d compares \
             against live"
                .to_string(),
        );
    }
    match at.origin {
        None | Some(Origin::Common) => {
            Some("nothing here differs from the cluster; n moves to the next change".to_string())
        }
        Some(_) => None,
    }
}

/// What taking one hunk did, in the words of the classification it came from.
/// The dry run that authorised an apply is void afterwards -- the bytes it
/// answered for are not the bytes in the buffer any more -- so the sentence
/// names the key that asks again.
fn kept_note(origin: Origin, keep: &diff::Keep) -> String {
    let what = match origin {
        Origin::Theirs => "kept the cluster's change",
        Origin::Mine => "put the cluster's own text back",
        Origin::Conflict => "took the cluster's side of the conflict",
        Origin::Undeclared => "took the value the cluster holds",
        Origin::Common => "changed nothing",
    };
    let lines = match (keep.taken, keep.dropped) {
        (0, dropped) => format!("dropped {}", lines_of(dropped)),
        (taken, 0) => format!("added {}", lines_of(taken)),
        (taken, dropped) => format!("{} in place of {dropped}", lines_of(taken)),
    };
    format!("{what}: {lines}; ctrl-alt-r asks the server about the result")
}

fn lines_of(count: usize) -> String {
    if count == 1 {
        return "1 line".to_string();
    }
    format!("{count} lines")
}

/// The two-press latch in front of every write. Pure, so the rule is tested
/// without a window.
#[derive(Debug, Default)]
pub(crate) struct ApplyGate {
    armed: Option<Armed>,
}

impl ApplyGate {
    pub(crate) fn step(&mut self, wanted: Armed) -> Step {
        if self.armed == Some(wanted) {
            self.armed = None;
            return Step::Go;
        }
        self.armed = Some(wanted);
        Step::Ask
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = None;
    }

    pub(crate) fn armed(&self) -> Option<Armed> {
        self.armed
    }
}

// What a reply has to be settled against: the comparison the request was made
// from, rather than whatever is on screen when the answer lands. A recompute
// during a real apply must not let the reply describe a different document --
// or, worse, reload a different buffer over unsaved work.
#[derive(Debug, Clone)]
struct Sent {
    generation: u64,
    stamp: BufferStamp,
    dry_run: bool,
    // What the prune did to the bytes that went out, worded for after the fact.
    note: String,
    // Which object the live document was read from *when this went out*. A real
    // apply's reply always speaks, so reading the field live when the answer
    // lands compares the server's answer against whatever a recompute has since
    // pointed the view at -- and a recompute during a slow apply then reports a
    // recreation that did not happen, over a write that did. This is the field
    // this struct exists to be: settle against the request, not against the
    // screen.
    uid: Option<String>,
}

impl Sent {
    // Whose answer still reaches the user. A dry run speaks only for the
    // comparison it was asked about, so a recompute discards it. A real apply
    // *happened*, and what the server said about it is the user's whatever the
    // view is comparing now: the guard that discarded both turned an admission
    // webhook's refusal into an empty status bar next to a fresh local diff.
    fn still_speaks(&self, generation: u64) -> bool {
        !self.dry_run || self.generation == generation
    }
}

/// Whether an answer from the server is about the object the review was made of.
///
/// A server-side apply *creates* what is absent, so applying a document whose
/// object was deleted between the read and the press brings it back instead of
/// failing -- `kubectl apply`'s behaviour, and not what the person pressing the
/// key is thinking about. The uid is what distinguishes the two, and it costs no
/// second round trip: the answer to the apply carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Identity {
    /// Both uids are known and equal, so whatever else changed, this is the same
    /// object.
    Same,
    /// Both are known and differ: the object the server answered about is not the
    /// one this document was read from.
    Different,
    /// One side carried no uid. Silence, not a guess: an unfounded claim that an
    /// object was replaced sends someone looking for a deletion nobody made, and
    /// the missing-field case is exactly where that claim would come from.
    Unknown,
}

pub(crate) fn identity(read: Option<&str>, answered: Option<&str>) -> Identity {
    match (read, answered) {
        (Some(read), Some(answered)) if read == answered => Identity::Same,
        (Some(_), Some(_)) => Identity::Different,
        _ => Identity::Unknown,
    }
}

/// What a *write* that landed on a different object has to say for itself. Which
/// of the two mechanisms produced it -- the object was deleted and this apply
/// recreated it, or someone else replaced it first and this apply updated the
/// replacement -- is not knowable from here, and both share the sentence that
/// matters: the object on screen is gone and this went somewhere else.
fn landed_note(read: Option<&str>, answered: Option<&str>) -> &'static str {
    match identity(read, answered) {
        Identity::Different => recreated_note(),
        Identity::Same | Identity::Unknown => "",
    }
}

fn recreated_note() -> &'static str {
    "; it did not update the object this was opened from -- that one is gone, so the write landed \
     on a new object with a different uid"
}

/// And what a *dry run* about a different object says. Nothing has been written,
/// so the point is not what happened but that the left-hand side of the
/// comparison describes an object that no longer exists.
fn stale_object_note() -> &'static str {
    "the server answered about a different object than this was opened from -- that one is gone, \
     so the live side of this comparison is out of date; open the object again"
}

/// What a dry-run answer means once the comparison against it has been made:
/// the line to show, or -- when there is no comparison -- the line to show
/// *and* the fact that nothing was reviewed. Not two branches on a boolean,
/// because a comparison nobody could make is not a comparison that found
/// nothing, and reading it as one told the user that applying a document the
/// diff had refused to review would change nothing.
fn reviewed(verdict: Verdict) -> Result<&'static str, String> {
    match verdict {
        Verdict::Differs => Ok("ctrl-s applies this"),
        Verdict::Agreed => Ok("the cluster already holds this; applying changes nothing"),
        Verdict::Refused(reason) => Err(format!("the server answered, but {reason}")),
    }
}

pub struct DiffView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    // The buffer this diff is of. A diff outlives nothing: if the editor closed,
    // there is nothing to re-read and nothing to apply, and saying so beats
    // applying a document nobody can see any more.
    editor: WeakEntity<EditorView>,
    request: DescribeRequest,
    title: SharedString,
    state: DiffState,
    payload: k10s_edit::Payload,
    // Which document, and which revision of it, the payload was built from. An
    // apply sends what was reviewed or it sends nothing -- and a bare version
    // could not tell a document from its replacement, because a reload restarts
    // the count.
    stamp: BufferStamp,
    // The buffer the server has answered a dry run for. The dry run is the
    // right-hand side of the review, so an apply of anything else is an apply
    // of bytes the server has never seen. Cleared by every recompute.
    reviewed: Option<BufferStamp>,
    // Which object the live document was read from, and which object the server
    // last answered a dry run about. They differ when the object was deleted or
    // replaced since the read, which is the difference between an apply that
    // updates and one that creates. Both come from a server response; neither is
    // the uid of whatever was selected to open the editor.
    uid: Option<String>,
    answered: Option<String>,
    patchable: bool,
    status: Option<String>,
    gate: ApplyGate,
    conflict: Vec<Conflicted>,
    // The server named more conflicts than the bounded review carries. What is
    // held is then a floor, not a count, and every sentence that authorises a
    // force has to say so: the press would take fields this review never named.
    conflict_truncated: bool,
    flight: Flight,
    generation: u64,
    viewport: Viewport,
}

impl DiffView {
    pub fn new(
        provider: Rc<dyn ReadProvider>,
        editor: WeakEntity<EditorView>,
        sources: DiffSources,
        dry_run: bool,
        cx: &mut Context<Self>,
    ) -> DiffView {
        let mut view = DiffView {
            focus: cx.focus_handle(),
            provider,
            editor,
            request: sources.request.clone(),
            title: format!("{} (diff)", sources.title).into(),
            state: DiffState::new(),
            payload: k10s_edit::Payload::default(),
            stamp: sources.stamp,
            reviewed: None,
            uid: None,
            answered: None,
            patchable: false,
            status: None,
            gate: ApplyGate::default(),
            conflict: Vec::new(),
            conflict_truncated: false,
            flight: Flight::default(),
            generation: 0,
            viewport: Viewport::default(),
        };
        view.refresh(sources, dry_run, cx);
        view
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn request(&self) -> &DescribeRequest {
        &self.request
    }

    /// Re-compare, from the editor's current text. Anything armed is disarmed
    /// and any earlier dry-run answer is forgotten: what was being confirmed is
    /// not what is on screen any more.
    pub fn refresh(&mut self, sources: DiffSources, dry_run: bool, cx: &mut Context<Self>) {
        // Destructured without `..` on purpose: this is the one place a whole
        // comparison is replaced, and the field it silently did not replace was
        // the editor handle. A tab reused by the editor that replaced the one
        // it was opened from kept pointing at the dead entity and answered
        // every press with "the editor this diff came from is gone" while that
        // editor sat open in the next tab. Now a new field does not compile
        // until this function has said what to do with it.
        let DiffSources {
            request,
            title,
            stamp,
            editor,
            live,
            base,
            buffer,
            uid,
            payload,
            patchable,
        } = sources;
        self.generation += 1;
        self.gate.disarm();
        self.forget_conflicts();
        self.editor = editor;
        self.request = request;
        self.title = format!("{title} (diff)").into();
        self.payload = payload;
        self.stamp = stamp;
        self.reviewed = None;
        self.uid = uid;
        self.answered = None;
        self.patchable = patchable;
        self.state.set(Mode::Local, live, base, buffer);
        self.status = None;
        if dry_run {
            self.send(true, false, cx);
        }
        cx.notify();
    }

    fn refresh_from_editor(&mut self, dry_run: bool, cx: &mut Context<Self>) {
        let sources = self
            .editor
            .upgrade()
            .and_then(|editor| editor.read(cx).diff_sources());
        match sources {
            Some(sources) => self.refresh(sources, dry_run, cx),
            None => {
                self.status =
                    Some("the editor this diff came from is gone; nothing to compare".to_string());
                cx.notify();
            }
        }
    }

    // The state every write precondition is decided from. Assembled here and
    // judged in `refuse`, so that the rules are one pure function over data
    // rather than a sequence of early returns nobody can test.
    fn ready(&self, editor: Option<BufferStamp>) -> Ready<'_> {
        Ready {
            patchable: self.patchable,
            blocked: &self.payload.blocked,
            verdict: self.state.diff().verdict(),
            reviewed: self.stamp,
            editor,
            dry_run: self.reviewed,
            conflicts: self.conflict.len(),
            in_flight: self.flight.busy().then(|| self.flight.holder()),
        }
    }

    fn apply(&mut self, force: bool, cx: &mut Context<Self>) {
        let wanted = if force { Armed::Force } else { Armed::Apply };
        let editor = self
            .editor
            .upgrade()
            .map(|editor| editor.read(cx).buffer_stamp());
        if let Some(why) = refuse(wanted, self.ready(editor)) {
            self.gate.disarm();
            self.status = Some(why);
            cx.notify();
            return;
        }
        if self.gate.step(wanted) == Step::Ask {
            // The prompt itself comes from the latch in `status_line`, so there
            // is nothing to remember here and nothing that can go stale.
            cx.notify();
            return;
        }
        self.send(false, force, cx);
    }

    // Take the cluster's side of the hunk being read. The classification names
    // drift an apply would revert; until this existed the only way to keep it was
    // to retype it, and the ranges to do it with were already in the comparison.
    //
    // Nothing goes on the wire, so `refuse` is not the gate here and there is no
    // two-press latch: an edit to a buffer is undoable, and the editor's own undo
    // is what undoes it.
    fn keep_theirs(&mut self, cx: &mut Context<Self>) {
        let editor = self.editor.upgrade();
        let hunk = self.state.hunk_at_top();
        let at = Keepable {
            mode: self.state.mode(),
            origin: hunk.and_then(|hunk| self.state.origin_of(hunk)),
            reviewed: self.stamp,
            editor: editor.as_ref().map(|editor| editor.read(cx).buffer_stamp()),
        };
        if let Some(why) = refuse_keep(at) {
            self.status = Some(why);
            cx.notify();
            return;
        }
        // Past `refuse_keep` both of these are present: it refused a missing
        // editor and a hunk that is not there. Asking again rather than
        // unwrapping keeps that agreement from being load-bearing.
        let Some(((hunk, origin), editor)) = hunk.zip(at.origin).zip(editor) else {
            self.status = Some("there is nothing here to keep".to_string());
            cx.notify();
            return;
        };
        let Some(keep) = self.state.keep(hunk) else {
            self.status = Some("there is nothing here to keep".to_string());
            cx.notify();
            return;
        };
        let note = kept_note(origin, &keep);
        let took = editor.update(cx, |editor, cx| editor.keep_hunk(self.stamp, keep, cx));
        if !took {
            self.status =
                Some("the buffer changed after this comparison; r compares it again".to_string());
            cx.notify();
            return;
        }
        // The buffer is not the buffer the dry run answered for any more, so the
        // comparison is remade against live and the server is asked again on
        // purpose rather than automatically: this is an edit, and edits are what
        // the review comes after.
        self.refresh_from_editor(false, cx);
        self.status = Some(note);
        cx.notify();
    }

    fn send(&mut self, dry_run: bool, force: bool, cx: &mut Context<Self>) {
        // The one door the bytes come through. A payload the pruner refused
        // carries the document unpruned, and a dry run of *those* bytes is not
        // a review of the ones an apply would send -- it is the same mistake
        // one round earlier, with the server's blessing attached to it.
        let yaml = match self.payload.sendable() {
            Ok(yaml) => yaml.to_string(),
            Err(_) => {
                self.gate.disarm();
                self.status =
                    Some("this document cannot be applied, so nothing was sent".to_string());
                cx.notify();
                return;
            }
        };
        let Some(ticket) = self.flight.take(dry_run) else {
            if dry_run {
                self.flight.owe_a_comparison();
            }
            self.status = Some(format!("{} is already in flight", self.flight.holder()));
            cx.notify();
            return;
        };
        let request = ApplyRequest {
            kind: self.request.kind,
            namespace: self.request.namespace.clone(),
            name: self.request.name.clone(),
            yaml,
            dry_run,
            force,
        };
        let sent = Sent {
            generation: self.generation,
            stamp: self.stamp,
            dry_run,
            note: prune_note(&self.payload),
            uid: self.uid.clone(),
        };
        self.status = Some(if dry_run {
            "asking the server what it would store...".to_string()
        } else {
            "applying...".to_string()
        });
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply: Reply<ApplyOutcome> = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        self.provider.apply(&request, reply);
        cx.spawn(async move |this, cx| {
            let answer = rx.await;
            let _ = this.update(cx, |this, cx| {
                // The wire is released by whichever request holds it, on every
                // path out of the await: a reply that is dropped rather than
                // sent would otherwise strand the ticket, which is the exact
                // failure the ticket replaced a flag to fix.
                let owed = this.flight.release(ticket);
                let speaks = sent.still_speaks(this.generation);
                match answer {
                    Ok(outcome) if speaks => this.settle(outcome, &sent, cx),
                    Ok(_) => {}
                    Err(_) if speaks => {
                        this.status = Some("the request ended without an answer".to_string());
                    }
                    Err(_) => {}
                }
                if owed {
                    this.send(true, false, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn settle(&mut self, outcome: ApplyOutcome, sent: &Sent, cx: &mut Context<Self>) {
        match outcome {
            ApplyOutcome::Applied { yaml, dry_run, uid } if dry_run => {
                self.forget_conflicts();
                // Whose object the server answered about, before anything is said
                // about what it holds: `status_line` derives the warning from it
                // for as long as it stands, rather than printing it once into a
                // message the next action overwrites.
                self.answered = uid;
                let live = self.state.live.clone();
                self.state.set(Mode::DryRun, live, None, yaml);
                self.status = Some(match reviewed(self.state.diff().verdict()) {
                    Ok(note) => {
                        self.reviewed = Some(sent.stamp);
                        note.to_string()
                    }
                    // No review was made, so none authorises a press: the stamp
                    // stays unreviewed and `refuse` says why on the next one.
                    Err(note) => note,
                });
            }
            ApplyOutcome::Applied { uid, .. } => {
                self.forget_conflicts();
                // A write that landed on a different object than the one this
                // document was opened from. An apply creates what is absent, so
                // this is not an error -- it is `kubectl apply`'s behaviour -- but
                // it is the one outcome nobody presses ctrl-s expecting.
                let landed = landed_note(sent.uid.as_deref(), uid.as_deref());
                // The object the cluster now holds is not necessarily the bytes
                // that were sent: defaulting and admission both ran. Re-reading
                // is the only honest way to show what landed -- and it only
                // happens while the buffer is still the one that was applied, so
                // an edit made while the apply was in flight survives it.
                let reloaded = self
                    .editor
                    .upgrade()
                    .map(|editor| {
                        editor.update(cx, |editor, cx| editor.reload_if_applied(sent.stamp, cx))
                    })
                    .unwrap_or(false);
                self.status = Some(if reloaded {
                    format!("applied as fieldManager k10s{}{landed}", sent.note)
                } else {
                    format!(
                        "applied as fieldManager k10s{}{landed}; the buffer moved meanwhile, so \
                         it was left as it is",
                        sent.note
                    )
                });
            }
            // The server took it and answered; only rendering that answer
            // failed. On a real apply the object is already stored, so the one
            // thing this must not read as is a write that did not happen.
            ApplyOutcome::Unrendered { dry_run, why } if dry_run => {
                self.forget_conflicts();
                self.status = Some(format!(
                    "the server accepted this, but its answer cannot be shown here, so there is \
                     nothing to review: {why}"
                ));
            }
            ApplyOutcome::Unrendered { why, .. } => {
                self.forget_conflicts();
                self.status = Some(format!(
                    "applied as fieldManager k10s{}; the object the cluster now holds cannot be \
                     shown here, so the buffer was left as it is: {why}",
                    sent.note
                ));
            }
            ApplyOutcome::Conflict {
                message,
                causes,
                truncated,
            } => {
                self.conflict = causes;
                self.conflict_truncated = truncated;
                self.status = Some(format!(
                    "{message}; ctrl-shift-s takes {} from {}",
                    self.taken(),
                    managers(&self.conflict)
                ));
            }
            ApplyOutcome::Stale { message } => {
                self.forget_conflicts();
                self.status = Some(stale_note(&message));
            }
            ApplyOutcome::Rejected { message, causes } => {
                self.forget_conflicts();
                let named: Vec<String> = causes.into_iter().take(MAX_SHOWN_CAUSES).collect();
                self.status = Some(if named.is_empty() {
                    format!("the server refused the document: {message}")
                } else {
                    format!("the server refused the document: {}", named.join("; "))
                });
            }
            ApplyOutcome::Denied { what, why } => {
                self.forget_conflicts();
                self.status = Some(format!("{what} denied: {why}"));
            }
            ApplyOutcome::Failed(why) => {
                self.forget_conflicts();
                self.status = Some(why);
            }
        }
    }

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let rows = self.viewport.rows(
            STATUS_BAR_HEIGHT,
            CONTENT_PADDING * 2.0,
            k10s_theme::typography(cx).line_height(),
            400,
        );
        self.state.set_viewport(rows.max(4));
        cx.notify();
    }

    fn forget_conflicts(&mut self) {
        self.conflict.clear();
        self.conflict_truncated = false;
    }

    fn taken(&self) -> String {
        taken(self.conflict.len(), self.conflict_truncated)
    }

    fn status_line(&self) -> String {
        let mut pieces = vec![self.state.summary()];
        // The prune's own two answers, before the press rather than after it.
        // `kept` names server-owned fields still in the bytes -- a review that
        // says what was taken out and not what stayed describes a document that
        // is not the one being sent.
        if !self.payload.blocked.is_empty() {
            pieces.push(format!(
                "cannot be applied: {}",
                self.payload.blocked.join("; ")
            ));
        }
        if !self.payload.kept.is_empty() {
            pieces.push(format!(
                "still carries the server's own {}",
                self.payload.kept.join(", ")
            ));
        }
        // Derived, never remembered: a one-shot message can be overwritten by
        // the next action while the latch stays armed, and a press that fires a
        // write with no prompt on screen is the one surprise this view must not
        // have.
        if let Some(armed) = self.gate.armed() {
            pieces.push(match armed {
                Armed::Apply => "ctrl-s again to apply this to the cluster".to_string(),
                Armed::Force => format!(
                    "ctrl-shift-s again to take {} from their managers",
                    self.taken()
                ),
            });
        }
        if !self.conflict.is_empty() {
            let fields: Vec<String> = self
                .conflict
                .iter()
                .take(MAX_SHOWN_CAUSES)
                .map(|cause| format!("{} ({})", cause.field, cause.manager))
                .collect();
            let mut listed = fields.join(", ");
            if self.conflict.len() > MAX_SHOWN_CAUSES {
                listed.push_str(&format!(
                    ", and {} more",
                    self.conflict.len() - MAX_SHOWN_CAUSES
                ));
            }
            if self.conflict_truncated {
                listed.push_str(
                    ", and further fields the server named that this review does not carry",
                );
            }
            pieces.push(format!("owned elsewhere: {listed}"));
        }
        // Derived rather than remembered, for the same reason the armed prompt is:
        // it stands for as long as the comparison does, and a one-shot message
        // would be overwritten by the next thing that happened.
        if identity(self.uid.as_deref(), self.answered.as_deref()) == Identity::Different {
            pieces.push(stale_object_note().to_string());
        }
        if let Some(status) = &self.status {
            pieces.push(status.clone());
        }
        pieces.join("  ·  ")
    }
}

/// What a force would take, in the words the review can honestly use. The
/// server's cause list is bounded before it reaches here, so once it has been
/// cut the count held is a floor rather than a total: asking for consent to
/// take "32 fields" when the server named more would be asking for consent to
/// something narrower than the press actually does.
fn taken(count: usize, truncated: bool) -> String {
    let plural = if count == 1 { "" } else { "s" };
    if truncated {
        return format!("at least {count} field{plural}");
    }
    format!("{count} field{plural}")
}

/// What a 409 with no causes leaves the user. It used to offer `r`, and `r`
/// cannot clear this state: it recomputes the comparison from the text the
/// editor already holds, which is the very document the server has just called
/// out of date, so the next two presses answer `Stale` again for as long as
/// anyone keeps pressing. Forcing is not on offer either -- a causeless 409
/// names no field to take. Only a fresh read helps, and neither this view nor
/// the editor's context has a binding that asks for one.
fn stale_note(message: &str) -> String {
    format!(
        "{message}; forcing cannot help and r only re-compares the same text -- open the object \
         again to read its current state"
    )
}

fn managers(causes: &[Conflicted]) -> String {
    let mut names: Vec<&str> = causes.iter().map(|cause| cause.manager.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.join(", ")
}

/// What the prune did to the bytes that went out, both halves of it. The half
/// that used to go unsaid is `kept`: those are the server's own fields the
/// prune could not remove without reshaping the document, and one of them is
/// `metadata.resourceVersion` -- an optimistic-lock precondition nobody asked
/// for, on an apply whose confirmation said only "applied".
fn prune_note(payload: &k10s_edit::Payload) -> String {
    let mut note = String::new();
    if !payload.pruned.is_empty() {
        note.push_str(&format!(
            "; the server's own fields were not sent ({})",
            payload.pruned.join(", ")
        ));
    }
    if !payload.kept.is_empty() {
        note.push_str(&format!(
            "; these could not be removed without reshaping the document and went with it ({})",
            payload.kept.join(", ")
        ));
    }
    note
}

fn row_color(theme: &Theme, painted: &Painted<'_>) -> u32 {
    match (painted.origin, painted.side) {
        (Origin::Common, _) => theme.shell.editor_foreground,
        // A synthesised header or fold is chrome, not content.
        (_, None) => theme.shell.text_accent,
        // The base document only ever appears in a conflict, and there it is
        // the reference rather than either answer.
        (_, Some(Side::Base)) => theme.shell.text_muted,
        (Origin::Mine, Some(Side::Live)) => theme.shell.error,
        (Origin::Mine, Some(Side::Buffer)) => theme.shell.success,
        (Origin::Theirs, Some(Side::Live)) => theme.shell.warning,
        (Origin::Theirs, Some(Side::Buffer)) => theme.shell.text_muted,
        // Undeclared is not an error colour: nothing was taken from anyone and
        // no refusal is coming. The cluster's side is what is there and nobody
        // asked for, the buffer's is what would be sent.
        (Origin::Undeclared, Some(Side::Live)) => theme.shell.warning,
        (Origin::Undeclared, Some(Side::Buffer)) => theme.shell.success,
        (Origin::Conflict, _) => theme.shell.error,
    }
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let rows: Vec<(SharedString, u32)> = self
            .state
            .visible()
            .map(|painted| {
                let color = row_color(&theme, &painted);
                (SharedString::from(painted.rendered()), color)
            })
            .collect();

        div()
            .id("diff-view")
            .key_context("Diff")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme.shell.editor_background))
            .font_family(fonts.buffer_family.clone())
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.resize(
                                f32::from(bounds.size.width),
                                f32::from(bounds.size.height),
                                cx,
                            );
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_action(cx.listener(|this, _: &crate::DocScrollUp, _, cx| {
                this.state.scroll_by(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::DocScrollDown, _, cx| {
                this.state.scroll_by(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::DocPageUp, _, cx| {
                this.state.page_by(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::DocPageDown, _, cx| {
                this.state.page_by(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::DocHome, _, cx| {
                this.state.home();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::DocEnd, _, cx| {
                this.state.end();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::NextChange, _, cx| {
                if !this.state.next_change() {
                    this.status = Some("no further changes".to_string());
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::PrevChange, _, cx| {
                if !this.state.prev_change() {
                    this.status = Some("no earlier changes".to_string());
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::ToggleFolded, _, cx| {
                this.state.toggle_folded();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::KeepTheirs, _, cx| {
                this.keep_theirs(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::Refresh, _, cx| {
                let dry_run = this.state.mode() == Some(Mode::DryRun);
                this.refresh_from_editor(dry_run, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::ApplyDryRun, _, cx| {
                this.refresh_from_editor(true, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::DiffAgainstLive, _, cx| {
                this.refresh_from_editor(false, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::ApplyToCluster, _, cx| {
                this.apply(false, cx);
            }))
            // The rule that a force needs a conflict that named the fields is
            // in `refuse` with every other precondition, not here: a guard
            // inside a key handler guards one of the ways in.
            .on_action(cx.listener(|this, _: &crate::ForceApply, _, cx| {
                this.apply(true, cx);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let row = k10s_theme::typography(cx).line_height();
                let delta = f32::from(event.delta.pixel_delta(px(row)).y);
                this.state.scroll_by(-(delta / row).round() as i64);
                cx.notify();
            }))
            .child(
                div()
                    .id("diff-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .p(px(CONTENT_PADDING))
                    .flex()
                    .flex_col()
                    .role(Role::Document)
                    .aria_label(self.title.clone())
                    .children(rows.into_iter().map(|(text, color)| {
                        div()
                            .h(px(fonts.line_height()))
                            .flex_none()
                            .overflow_hidden()
                            .text_size(px(fonts.buffer_size))
                            .whitespace_nowrap()
                            .text_color(rgb(color))
                            .child(text)
                    })),
            )
            .child(
                div()
                    .h(px(STATUS_BAR_HEIGHT))
                    .flex_none()
                    .px(px(CONTENT_PADDING))
                    .flex()
                    .items_center()
                    .bg(rgb(theme.shell.panel_background))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.small()))
                    .text_color(if self.status.is_some() {
                        rgb(theme.shell.text)
                    } else {
                        rgb(theme.shell.text_muted)
                    })
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(SharedString::from(self.status_line())),
            )
    }
}

impl crate::item::Item for DiffView {
    fn title(&self) -> SharedString {
        DiffView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        DiffView::focus_handle(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
