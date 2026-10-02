//! The diff item: what an apply would change, reviewed before it happens.
//!
//! Two comparisons live in one view because they answer the same question at
//! different distances. The local one is three-way -- the copy of the object
//! this editor read, the object as it was last declared, and the buffer -- and
//! its value is the classification: a hunk only the buffer changed is an edit,
//! a hunk only the copy changed is drift since the last apply, as that copy
//! shows it, and a hunk both changed is a collision. The dry-run one is two-way
//! and authoritative: the API server is handed the payload with `?dryRun=All`,
//! and what comes back is what it *would* store, defaulting and admission
//! webhooks included, so the diff is against the real outcome rather than a
//! client's guess at merge semantics.
//! Its left side is the object as the server holds it, read with the dry run,
//! never the copy the editor loaded: a write that landed since, an apply of
//! this view's that went unconfirmed included, shows there instead of hiding
//! in the review. The local comparison is against the loaded copy and
//! authorises nothing.
//!
//! An apply needs a server dry run and two deliberate presses. After an
//! ownership conflict, the first force press asks for a forced dry run. Its
//! answer arms nothing; two more force presses confirm and take the fields
//! the server named. [`ApplyGate`] arms under the name of the question, so a
//! different press re-asks instead of firing. Any recompute disarms because
//! the thing being confirmed is no longer what is on screen.
//!
//! Every precondition a *press* has to clear is in one pure function,
//! [`prepare`], and none of them is anywhere else. That is not tidiness: the two
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
//! and [`prepare`] -- so the destructive rules are tested without a window. The
//! rows are built once per comparison and only the visible ones are ever turned
//! into text, so a diff of a megabyte costs a vector rather than a second copy
//! of the document.

use std::borrow::Cow;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Context, Div, Element, ElementId, FocusHandle, GlobalElementId,
    Hitbox, InspectorElementId, IntoElement, LayoutId, ParentElement, Pixels, Render, Role,
    ScrollHandle, ScrollWheelEvent, SharedString, Styled, WeakEntity, Window, canvas, div,
    prelude::*, px, relative, rgb,
};

use k10s_edit::diff::{self, Origin, Side, Verdict};
use k10s_theme::Theme;

use crate::confirm::write_target;
use crate::diff_gate::{
    ApplyGate, Armed, Flight, Keepable, Preparation, Ready, Review, Reviewed, Seen, Sent, Step,
    dry_run_left, gone_note, kept_note, landed_note, may_still_land, prepare, raced_note,
    refuse_keep, reviewed, seen_after, stale_object_note,
};
use crate::editor::{BufferStamp, DiffSources, EditorView};
use crate::ui::{CONTENT_PADDING, STATUS_BAR_HEIGHT, Viewport};
use k10s_provider::{
    ApplyOutcome, ApplyRequest, ClusterProvider, Conflicted, DescribeRequest, Reply,
};

// How much unchanged text stays visible around a change when the folded view is
// on. Three lines is git's default and is enough to place a hunk in a manifest.
pub(crate) const CONTEXT_LINES: usize = 3;

// A conflict list is server data; the panel that renders it is bounded like
// every other buffer in the shell.
pub(crate) const MAX_SHOWN_CAUSES: usize = 12;

/// Which comparison is on screen. The same rows mean different things in each,
/// so the labelling is not cosmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The copy this editor read, last-applied and the buffer, from the editor
    /// alone. It authorises no write.
    Local,
    /// The object as the dry run read it against what the server answered it
    /// would store.
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

#[derive(Debug)]
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
    // Formatted when the comparison changes, and borrowed by every render of
    // the status line; neither depends on the fold, so a fold formats nothing.
    summary: String,
    hints: String,
}

impl Default for DiffState {
    fn default() -> DiffState {
        DiffState::new()
    }
}

impl DiffState {
    pub fn new() -> DiffState {
        let mut state = DiffState {
            live: String::new(),
            base: None,
            other: String::new(),
            mode: None,
            diff: diff::Diff::default(),
            cells: Vec::new(),
            top: 0,
            viewport: 4,
            folded: true,
            summary: String::new(),
            hints: String::new(),
        };
        state.summary = state.summary_text();
        state
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
        self.summary = self.summary_text();
        self.hints = self.hints_text();
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

    pub(crate) fn folded_note(&self) -> bool {
        self.folded && self.diff.verdict() == Verdict::Differs
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

    pub(crate) fn paint(&self, cell: Cell) -> Painted<'_> {
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

    pub(crate) fn note(&self, origin: Origin) -> &'static str {
        match (self.mode, origin) {
            (Some(Mode::DryRun), _) => "the apply would change this",
            (_, Origin::Mine) => "you changed this",
            (_, Origin::Theirs) => "changed since the last apply, in the copy this editor read",
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

    /// The buffer edit that takes one hunk as the copy this editor read has it.
    /// The two documents its ranges point into are this state's own, which is
    /// why the call lives here: in [`Mode::DryRun`] the right-hand document is
    /// the server's answer rather than the editor's buffer, so an edit derived
    /// from it would splice one document's ranges into another. [`refuse_keep`]
    /// is what keeps that press from arriving.
    pub(crate) fn keep(&self, hunk: usize) -> Option<diff::Keep> {
        debug_assert_eq!(
            self.mode,
            Some(Mode::Local),
            "only the local comparison's right-hand side is the editor's buffer"
        );
        diff::keep_theirs(&self.diff, hunk, &self.live, &self.other)
    }

    /// What the comparison found, for the status line.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// What the comparison says about itself that a press rarely turns on.
    pub fn hints(&self) -> &str {
        &self.hints
    }

    fn summary_text(&self) -> String {
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
            Mode::Local => "against the copy this editor read".to_string(),
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
                pieces.push(format!("{} changed since the last apply", counts.theirs));
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
        pieces.join("  ·  ")
    }

    fn hints_text(&self) -> String {
        let Some(mode) = self.mode else {
            return String::new();
        };
        let verdict = self.diff.verdict();
        if matches!(verdict, Verdict::Refused(_)) {
            return String::new();
        }
        let mut pieces = Vec::new();
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
            pieces.push("t takes a hunk from the copy this editor read".to_string());
        }
        pieces.join("  ·  ")
    }

    pub(crate) fn max_top(&self) -> usize {
        self.cells.len().saturating_sub(1)
    }

    // The first document row the viewport is showing. A note or a fold stands in
    // for the rows after it rather than being one, so the nearest following line
    // is what answers for it.
    pub(crate) fn row_at_top(&self) -> Option<usize> {
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
    pub(crate) fn cell_showing(&self, row: usize) -> usize {
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

    pub(crate) fn rebuild(&mut self) {
        self.cells = build(&self.diff, self.folded);
        self.top = self.top.min(self.max_top());
    }
}

// A hunk of unchanged text between two changes keeps the context nearest each of
// them and folds the middle; the run before the first change and the run after
// the last one only need the side facing a change.
pub(crate) fn build(diff: &diff::Diff, folded: bool) -> Vec<Cell> {
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

pub(crate) fn mark_of(side: Side, origin: Origin) -> char {
    match (origin, side) {
        (Origin::Common, _) => ' ',
        (_, Side::Live) => '-',
        (_, Side::Buffer) => '+',
        // The base is neither removed nor added: it is what both sides moved
        // away from.
        (_, Side::Base) => '|',
    }
}

// The status has one child and only needs a pixel offset. Div's tracked scroll
// handle rebuilds a vector of child bounds every prepaint for item navigation.
// Use the same interactivity with the child's size directly, so tracking the
// prompt adds no bounds allocation to a paint.
struct StatusBar {
    chrome: gpui::Stateful<Div>,
    text: AnyElement,
}

impl IntoElement for StatusBar {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for StatusBar {
    type RequestLayoutState = LayoutId;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        Some("diff-status".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, LayoutId) {
        let text = &mut self.text;
        let mut child = None;
        let layout = self.chrome.interactivity().request_layout(
            id,
            inspector,
            window,
            cx,
            |style, window, cx| {
                window.with_text_style(style.text_style().cloned(), |window| {
                    let text_layout = text.request_layout(window, cx);
                    child = Some(text_layout);
                    window.request_layout(style, [text_layout], cx)
                })
            },
        );
        (layout, child.expect("the status text has a layout"))
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut LayoutId,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        let content = window.layout_bounds(*child).size;
        let text = &mut self.text;
        self.chrome.interactivity().prepaint(
            id,
            inspector,
            bounds,
            content,
            window,
            cx,
            |_, offset, hitbox, window, cx| {
                window.with_element_offset(offset, |window| text.prepaint(window, cx));
                hitbox
            },
        )
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut LayoutId,
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let text = &mut self.text;
        self.chrome.interactivity().paint(
            id,
            inspector,
            bounds,
            hitbox.as_ref(),
            window,
            cx,
            |_, window, cx| text.paint(window, cx),
        );
    }
}

pub struct DiffView {
    focus: FocusHandle,
    provider: Rc<dyn ClusterProvider>,
    // The kubeconfig context this view's apply would write to, captured when the
    // review opened. It is in the armed sentence because "the cluster" reads the
    // same in every window somebody has open.
    context: Option<String>,
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
    // The object this editor read when the comparison opened, and what the
    // latest dry run's read said about it: the same one, a replacement, or none
    // at all. Both identities come from a server response.
    uid: Option<String>,
    seen: Seen,
    // A real apply from this view went unconfirmed and may still land. Shown on
    // the status line for as long as the view lives ([`may_still_land`]), so no
    // one-shot message, and no later answer, can erase it.
    unknown_write: bool,
    patchable: bool,
    status: Option<String>,
    status_scroll: ScrollHandle,
    status_text: SharedString,
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
        provider: Rc<dyn ClusterProvider>,
        editor: WeakEntity<EditorView>,
        sources: DiffSources,
        dry_run: bool,
        context: Option<String>,
        cx: &mut Context<Self>,
    ) -> DiffView {
        let mut view = DiffView {
            context,
            focus: cx.focus_handle(),
            provider,
            editor,
            request: sources.request.clone(),
            title: format!("{} (diff)", sources.title).into(),
            state: DiffState::new(),
            payload: k10s_edit::Payload::default(),
            stamp: sources.stamp,
            uid: None,
            seen: Seen::Unknown,
            unknown_write: false,
            patchable: false,
            status: None,
            status_scroll: ScrollHandle::new(),
            status_text: SharedString::default(),
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
        self.flight.forget_review();
        self.uid = uid;
        self.seen = Seen::Unknown;
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
    // judged in `prepare`, so that the rules are one pure function over data
    // rather than a sequence of early returns nobody can test.
    fn ready(&self, editor: Option<BufferStamp>) -> Ready<'_> {
        Ready {
            patchable: self.patchable,
            blocked: &self.payload.blocked,
            verdict: self.state.diff().verdict(),
            reviewed: self.stamp,
            editor,
            dry_run: self.flight.reviewed(),
            conflicts: self.conflict.len(),
            in_flight: self.flight.busy().then(|| self.flight.holder()),
            raced: self.seen == Seen::Raced,
        }
    }

    fn apply(&mut self, force: bool, cx: &mut Context<Self>) {
        let wanted = if force { Armed::Force } else { Armed::Apply };
        let editor = self
            .editor
            .upgrade()
            .map(|editor| editor.read(cx).buffer_stamp());
        match prepare(wanted, self.ready(editor)) {
            Err(why) => {
                self.gate.disarm();
                self.status = Some(why);
                cx.notify();
                return;
            }
            Ok(Preparation::PreviewForce) => {
                self.gate.disarm();
                self.send(true, true, cx);
                return;
            }
            Ok(Preparation::Confirm) => {}
        }
        match self
            .gate
            .step(wanted, self.status_scroll.offset().y == px(0.0))
        {
            Step::Reveal => {
                self.status_scroll.set_offset(Default::default());
                cx.notify();
            }
            // The prompt itself comes from the latch in `status_line`, so there
            // is nothing to remember here and nothing that can go stale.
            Step::Ask => cx.notify(),
            Step::Go => self.send(false, force, cx),
        }
    }

    // Take the hunk being read as the copy this editor read has it. The
    // classification names drift since the last apply; until this existed the
    // only way to keep it was to retype it, and the ranges to do it with were
    // already in the comparison.
    //
    // Nothing goes on the wire, so `prepare` is not the gate here and there is no
    // two-press latch: an edit to a buffer is undoable, and the editor's own undo
    // is what undoes it.
    fn take_from_loaded_copy(&mut self, cx: &mut Context<Self>) {
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
        // comparison is remade against the copy this editor read and the server is
        // asked again on purpose rather than automatically: this is an edit, and
        // edits are what the review comes after.
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
            force,
            note: format!(
                "{}{}",
                prune_note(&self.payload),
                if force { " (forced)" } else { "" }
            ),
            uid: self.uid.clone(),
        };
        self.status = Some(if dry_run && force {
            "previewing a forced apply; nothing is being written...".to_string()
        } else if dry_run {
            "asking the server what it would store...".to_string()
        } else {
            "applying...".to_string()
        });
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply: Reply<ApplyOutcome> = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        self.provider.apply(&request, reply);
        let sender = self.editor.clone();
        cx.spawn(async move |this, cx| {
            let answer = rx.await;
            // What landed is re-read by the editor that sent it, whether or not
            // this view is still open, and never by whichever editor a reused
            // tab points at now: its buffer is someone else's work.
            let reload = match &answer {
                Ok(ApplyOutcome::Applied { .. }) => Some(
                    sender
                        .update(cx, |editor, cx| {
                            if editor.reload_if_applied(sent.stamp, cx) {
                                Reload::Done
                            } else {
                                Reload::Moved
                            }
                        })
                        .unwrap_or(Reload::Gone),
                ),
                _ => None,
            };
            let unconfirmed = matches!(answer, Err(_) | Ok(ApplyOutcome::Unconfirmed(_)));
            let _ = this.update(cx, |this, cx| {
                // The wire is released by whichever request holds it, on every
                // path out of the await: a reply that is dropped rather than
                // sent would otherwise strand the ticket, which is the exact
                // failure the ticket replaced a flag to fix.
                let owed = this.flight.release(ticket);
                if sent.still_speaks(this.generation) {
                    // One rule for an answer and for a reply dropped unsent,
                    // which is `None`: what it leaves standing about this
                    // view's writes and about the object.
                    let answered = answer.as_ref().ok();
                    this.unknown_write = may_still_land(this.unknown_write, sent.dry_run, answered);
                    this.seen = seen_after(this.seen, sent.dry_run, answered);
                    match answer {
                        Ok(outcome) => this.settle(outcome, &sent, reload),
                        Err(_) => this.status = Some(dropped_note(sent.dry_run).to_string()),
                    }
                }
                // A comparison asked for while a real apply was out is not sent
                // after it: that apply's answer is the one sentence the person
                // must read, and the comparison would replace it a round trip
                // later. After a dry run, the owed one simply goes.
                if owed && sent.dry_run {
                    this.send(true, false, cx);
                } else if owed {
                    this.status = this
                        .status
                        .take()
                        .map(|status| owed_not_sent(status, unconfirmed));
                }
                cx.notify();
            });
        })
        .detach();
        // The in-flight sentence replaces the line that asked for this press,
        // the armed prompt or the conflict's offer of a forced preview, which
        // would otherwise stay on screen asking for the press just made.
        cx.notify();
    }

    // `reload` is what the sending editor did with a real apply that landed.
    fn settle(&mut self, outcome: ApplyOutcome, sent: &Sent, reload: Option<Reload>) {
        // A forced preview takes no fields. Keep the named owners for the
        // confirmation that follows it, including when the preview must retry.
        if !sent.dry_run || !sent.force {
            self.forget_conflicts();
        }
        if sent.dry_run {
            self.flight.forget_review();
        }
        match outcome {
            ApplyOutcome::DryRun { yaml, uid, live } => {
                // The left side is the object as the server holds it now, read
                // with the dry run, and what that read says about the object this
                // was opened from stands in `status_line` for as long as the
                // comparison does.
                let (left, seen) = dry_run_left(self.uid.as_deref(), live, uid.as_deref());
                self.seen = seen;
                self.state.set(Mode::DryRun, left, None, yaml);
                // Only a recorded review authorises a press; otherwise the stamp
                // stays unreviewed and `prepare` says why on the next one.
                self.status = match reviewed(self.state.diff().verdict(), sent.force, seen) {
                    Reviewed::Recorded(note) => {
                        self.flight.record_review(Review {
                            stamp: sent.stamp,
                            force: sent.force,
                        });
                        Some(note.to_string())
                    }
                    Reviewed::Refused(note) => Some(note),
                    // The line derives why from `seen`.
                    Reviewed::Raced => None,
                };
            }
            ApplyOutcome::Applied { uid } => {
                // A write that landed on a different object than the one this
                // document was opened from. An apply creates what is absent, so
                // this is not an error -- it is `kubectl apply`'s behaviour -- but
                // it is the one outcome nobody presses ctrl-s expecting.
                let landed = landed_note(sent.uid.as_deref(), uid.as_deref());
                // The object the cluster now holds is not necessarily the bytes
                // that were sent: defaulting and admission both ran. The sending
                // editor re-read it, but only while its buffer was still the one
                // applied, so an edit made while the apply was in flight survives.
                self.status = Some(applied_note(
                    &sent.note,
                    &landed,
                    reload.unwrap_or(Reload::Gone),
                ));
            }
            ApplyOutcome::Unrendered { why } => {
                self.status = Some(unrendered_note(&why));
            }
            ApplyOutcome::Conflict {
                message,
                causes,
                truncated,
            } => {
                self.conflict = causes;
                self.conflict_truncated = truncated;
                self.status = Some(format!(
                    "{message}; ctrl-shift-s previews taking {} from {}",
                    self.taken(),
                    managers(&self.conflict)
                ));
            }
            ApplyOutcome::Stale { message } => {
                self.status = Some(stale_note(&message));
            }
            ApplyOutcome::Rejected { message, causes } => {
                self.status = Some(rejected_note(&message, causes));
            }
            ApplyOutcome::Denied { what, why } => {
                self.status = Some(format!("{what} denied: {why}"));
            }
            ApplyOutcome::Unconfirmed(why) => {
                self.status = Some(unconfirmed_note(&why));
            }
            ApplyOutcome::Failed(why) => {
                self.status = Some(why);
            }
        }
    }

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let rows = self.viewport.rows(
            0.0,
            CONTENT_PADDING * 2.0,
            k10s_theme::typography(cx).line_height(),
            400,
        );
        self.state.set_viewport(rows);
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
        status_line(Line {
            summary: self.state.summary(),
            hints: self.state.hints(),
            blocked: &self.payload.blocked,
            kept: &self.payload.kept,
            armed: self.gate.armed(),
            conflict: &self.conflict,
            conflict_truncated: self.conflict_truncated,
            seen: self.seen,
            unknown_write: self.unknown_write,
            status: self.status.as_deref(),
            context: self.context.as_deref(),
            folded: self.state.folded_note(),
        })
    }
}

// Everything the status line reads, borrowed at one moment -- the same idiom
// as `diff_gate::Ready`, so the sentence the user acts on can be checked
// without a window.
pub(crate) struct Line<'a> {
    pub(crate) summary: &'a str,
    pub(crate) hints: &'a str,
    pub(crate) blocked: &'a [&'static str],
    pub(crate) kept: &'a [String],
    pub(crate) armed: Option<Armed>,
    pub(crate) conflict: &'a [Conflicted],
    pub(crate) conflict_truncated: bool,
    pub(crate) seen: Seen,
    /// A real apply from this view went unconfirmed and may still land.
    pub(crate) unknown_write: bool,
    pub(crate) status: Option<&'a str>,
    /// The kubeconfig context this write would land in. `None` is an
    /// in-cluster account, which has no context name to print.
    pub(crate) context: Option<&'a str>,
    /// Unchanged text is folded away in a comparison that differs.
    pub(crate) folded: bool,
}

// The pieces go in the order a person acts on them: what the next press does,
// what the comparison is, the payload's standing refusals of that press, the
// last answer, what stands about the object and this view's writes, who owns
// what, and last what the comparison says about itself. The line wraps rather
// than cutting, so the order is for reading, not for what survives a narrow
// window. Changed text returns the status to its top; unchanged text keeps
// its scroll position. One vector of borrowed and owned pieces, joined once.
pub(crate) fn status_line(at: Line<'_>) -> String {
    let mut pieces: Vec<Cow<'_, str>> = Vec::with_capacity(1 + usize::from(at.armed.is_some()));
    // Derived, never remembered: a one-shot message can be overwritten by
    // the next action while the latch stays armed, and a press that fires a
    // write with no prompt on screen is the one surprise this view must not
    // have.
    if let Some(armed) = at.armed {
        pieces.push(Cow::Owned(match armed {
            // Which cluster, by name, in the sentence that fires the write.
            // A confirm that says "the cluster" is the same sentence in every
            // context a person has open, and that is how the wrong one gets
            // written to.
            Armed::Apply => format!("ctrl-s again to apply this to {}", write_target(at.context)),
            Armed::Force => format!(
                "ctrl-shift-s again to take {} from their managers in {}",
                taken(at.conflict.len(), at.conflict_truncated),
                write_target(at.context)
            ),
        }));
    }
    pieces.push(Cow::Borrowed(at.summary));
    // The prune's own two answers, before the press rather than after it.
    // `kept` names server-owned fields still in the bytes -- a review that
    // says what was taken out and not what stayed describes a document that
    // is not the one being sent.
    if !at.blocked.is_empty() {
        pieces.push(Cow::Owned(format!(
            "cannot be applied: {}",
            at.blocked.join("; ")
        )));
    }
    if !at.kept.is_empty() {
        pieces.push(Cow::Owned(format!(
            "still carries the server's own {}",
            at.kept.join(", ")
        )));
    }
    if let Some(status) = at.status {
        pieces.push(Cow::Borrowed(status));
    }
    // Derived rather than remembered, for the same reason the armed prompt is:
    // it stands for as long as the comparison does, and a one-shot message
    // would be overwritten by the next thing that happened.
    match at.seen {
        Seen::Replaced => pieces.push(Cow::Borrowed(stale_object_note())),
        Seen::Gone => pieces.push(Cow::Borrowed(gone_note())),
        Seen::Raced => pieces.push(Cow::Borrowed(raced_note())),
        Seen::Same | Seen::Unknown => {}
    }
    if at.unknown_write {
        pieces.push(Cow::Borrowed(standing_note()));
    }
    if !at.conflict.is_empty() {
        let fields: Vec<String> = at
            .conflict
            .iter()
            .take(MAX_SHOWN_CAUSES)
            .map(|cause| format!("{} ({})", cause.field, cause.manager))
            .collect();
        let mut listed = fields.join(", ");
        if at.conflict.len() > MAX_SHOWN_CAUSES {
            listed.push_str(&format!(
                ", and {} more",
                at.conflict.len() - MAX_SHOWN_CAUSES
            ));
        }
        if at.conflict_truncated {
            listed
                .push_str(", and further fields the server named that this review does not carry");
        }
        pieces.push(Cow::Owned(format!("owned elsewhere: {listed}")));
    }
    if !at.hints.is_empty() {
        pieces.push(Cow::Borrowed(at.hints));
    }
    if at.folded {
        pieces.push(Cow::Borrowed("folded"));
    }
    pieces.join("  ·  ")
}

// Stands on the line from the first real apply of this view that went
// unconfirmed, for as long as the view lives: nothing read later can say that
// the write will not land.
pub(crate) fn standing_note() -> &'static str {
    "an apply from this view went unconfirmed and may still land"
}

// A real apply's answer, with the comparison asked for while it was out, which
// was not sent after it. An apply that went unconfirmed already offers
// ctrl-alt-r, and one sentence does not offer it twice.
pub(crate) fn owed_not_sent(status: String, unconfirmed: bool) -> String {
    let skipped = if unconfirmed {
        "the comparison asked for while it was out was not sent"
    } else {
        "the comparison asked for while it was out was not sent; ctrl-alt-r asks for it"
    };
    format!("{status}; {skipped}")
}

// The sentence a request whose answer never arrived shows. A dry run writes
// nothing; a real apply may have been served before the answer was lost, so
// its sentence must not read as a write that did not happen.
pub(crate) fn dropped_note(dry_run: bool) -> &'static str {
    if dry_run {
        "the dry run ended without an answer"
    } else {
        "the apply ended without an answer; it may have landed; ctrl-alt-r compares the buffer \
         with the object as it is now"
    }
}

// The sentence a real apply that went unconfirmed shows: that it may have
// landed and how to see what the server holds, then the reason, which for a
// lost connection is the client's whole error chain.
pub(crate) fn unconfirmed_note(why: &str) -> String {
    format!(
        "the apply may have landed; ctrl-alt-r compares the buffer with the object as it is now; \
         {why}"
    )
}

/// What the editor that sent a landed apply did with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reload {
    /// It started a re-read of the object, so the buffer will show what landed.
    Done,
    /// Its buffer is not the one that was applied any more, an edit or a
    /// save to a file since, so it was left as it is.
    Moved,
    /// It is closed, so nothing was re-read.
    Gone,
}

// The sentence a landed apply shows. `reload` is the sending editor's answer,
// not a request: re-reading only happens while the buffer is still the one that
// was applied, so an edit made while the apply was in flight survives it, and
// the sentence has to say which of the three happened.
pub(crate) fn applied_note(sent_note: &str, landed: &str, reload: Reload) -> String {
    match reload {
        Reload::Done => format!("applied as fieldManager k10s{sent_note}{landed}"),
        Reload::Moved => format!(
            "applied as fieldManager k10s{sent_note}{landed}; the buffer is not the one that \
             was applied any more, so it was left as it is"
        ),
        Reload::Gone => format!(
            "applied as fieldManager k10s{sent_note}{landed}; the editor it came from is closed, \
             so nothing was reloaded"
        ),
    }
}

// The server took the dry run and answered; only rendering that answer failed,
// so there is nothing to review, and nothing was written.
pub(crate) fn unrendered_note(why: &str) -> String {
    format!(
        "the server accepted this dry run, but its answer cannot be shown here, so there is \
         nothing to review: {why}"
    )
}

// A rejection names its causes when the server gave any, bounded, and falls
// back to the message when it gave none -- an empty refusal is still a refusal.
pub(crate) fn rejected_note(message: &str, causes: Vec<String>) -> String {
    let named: Vec<String> = causes.into_iter().take(MAX_SHOWN_CAUSES).collect();
    if named.is_empty() {
        format!("the server refused the document: {message}")
    } else {
        format!("the server refused the document: {}", named.join("; "))
    }
}

/// What a force would take, in the words the review can honestly use. The
/// server's cause list is bounded before it reaches here, so once it has been
/// cut the count held is a floor rather than a total: asking for consent to
/// take "32 fields" when the server named more would be asking for consent to
/// something narrower than the press actually does.
pub(crate) fn taken(count: usize, truncated: bool) -> String {
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
pub(crate) fn stale_note(message: &str) -> String {
    format!(
        "{message}; forcing cannot help and r only re-compares the same text -- open the object \
         again to read its current state"
    )
}

pub(crate) fn managers(causes: &[Conflicted]) -> String {
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
pub(crate) fn prune_note(payload: &k10s_edit::Payload) -> String {
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

pub(crate) fn row_color(theme: &Theme, painted: &Painted<'_>) -> u32 {
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
        // no refusal is coming. The left side is what is there and nobody asked
        // for, the buffer's is what would be sent.
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
        let status = self.status_line();
        if status != self.status_text.as_ref() {
            self.status_scroll.set_offset(Default::default());
            self.status_text = status.into();
        }
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
            .on_action(cx.listener(|this, _: &crate::TakeFromLoadedCopy, _, cx| {
                this.take_from_loaded_copy(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::Refresh, _, cx| {
                let dry_run = this.state.mode() == Some(Mode::DryRun);
                this.refresh_from_editor(dry_run, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::ApplyDryRun, _, cx| {
                this.refresh_from_editor(true, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::DiffAgainstLoadedCopy, _, cx| {
                    this.refresh_from_editor(false, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::ApplyToCluster, _, cx| {
                this.apply(false, cx);
            }))
            // The rule that a force needs a conflict that named the fields is
            // in `prepare` with every other precondition, not here: a guard
            // inside a key handler guards one of the ways in.
            .on_action(cx.listener(|this, _: &crate::ForceApply, _, cx| {
                this.apply(true, cx);
            }))
            .child(
                // The body measures itself: the status below wraps and takes
                // its height from here, and a row count taken from the whole
                // view would hide the last rows behind it. The canvas fills
                // this unpadded region, and the rows are its height less the
                // padding of the column inside it.
                div()
                    .id("diff-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .relative()
                    .role(Role::Document)
                    .aria_label(self.title.clone())
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        let row = k10s_theme::typography(cx).line_height();
                        let delta = f32::from(event.delta.pixel_delta(px(row)).y);
                        this.state.scroll_by(-(delta / row).round() as i64);
                        cx.notify();
                    }))
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
                    .child(div().p(px(CONTENT_PADDING)).flex().flex_col().children(
                        rows.into_iter().map(|(text, color)| {
                            div()
                                .h(px(fonts.line_height()))
                                .flex_none()
                                .overflow_hidden()
                                .text_size(px(fonts.buffer_size))
                                .whitespace_nowrap()
                                .text_color(rgb(color))
                                .child(text)
                        }),
                    )),
            )
            .child(
                // Wrapped, never cut, and never more than half the view: at a
                // narrow window the standing note, a conflict's key or a landed
                // note would otherwise be off screen with nothing to say so,
                // and a long answer would take the rows it is about. What does
                // not fit in that half scrolls. One row is the bar's height,
                // and the row inside it centres a line that fits in one.
                StatusBar {
                    chrome: div()
                        .id("diff-status")
                        .flex_none()
                        .max_h(relative(0.5))
                        .overflow_y_scroll()
                        .track_scroll(&self.status_scroll)
                        .bg(rgb(theme.shell.panel_background))
                        .border_t_1()
                        .border_color(rgb(theme.shell.border_variant)),
                    text: div()
                        .min_h(px(STATUS_BAR_HEIGHT))
                        .px(px(CONTENT_PADDING))
                        .flex()
                        .items_center()
                        .text_size(px(fonts.small()))
                        .text_color(if self.status.is_some() {
                            rgb(theme.shell.text)
                        } else {
                            rgb(theme.shell.text_muted)
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .child(self.status_text.clone()),
                        )
                        .into_any_element(),
                },
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
