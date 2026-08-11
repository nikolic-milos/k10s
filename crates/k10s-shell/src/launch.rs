//! The launch screen: choosing what this window is going to show.
//!
//! k10s used to decide that on the command line and `exit(1)` when the answer
//! was wrong, which meant the first thing a new user saw was a shell prompt
//! explaining that their kubeconfig has no current-context. The choice belongs
//! on screen: the contexts this process can see, an entry to open a kubeconfig
//! that is somewhere else, and the generated starmap, which is the one option
//! that always works and is therefore the one that must always be listed.
//!
//! [`LaunchState`] is the whole decision and it is pure: the rows, which one is
//! highlighted, what the query keeps, and what each entry *means* -- the exact
//! [`ConnectRequest`] that row would send -- with no window, no disk, and no
//! cluster. Keyboard and mouse both go through it, so the two cannot disagree.
//! The view around it holds no logic worth testing and the workspace owns the
//! I/O, because reading a kubeconfig, minting a credential and generating a
//! scene are all things that must not happen on the GPUI thread.
//!
//! Two rules shape the rest. Nothing here may dead-end: this is the screen
//! someone sees when their cluster is unreachable, so a scan that fails, a file
//! that will not parse, a source that declares no contexts and a connection
//! that is refused are all labelled states with the rows still live underneath.
//! And a kubeconfig holds credentials -- `token`, `client-key-data`, an exec
//! plugin's arguments -- so the only fields that reach a row are the ones
//! [`crate::provider::ContextRow`] carries, and [`detail`] is the one place
//! that decides what of those is shown.

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Render, Role, SharedString, Styled, Window, div, img, prelude::*, px, rgb,
};

use crate::palette::fuzzy_match;
use crate::provider::{ConfigSource, ConnectRequest, ContextRow, ScanOutcome, ScanRequest};
use crate::ui::{LAUNCH_LOGO_SIZE, LIST_ROW_HEIGHT, MODAL_MAX_HEIGHT, MODAL_WIDTH, brand_logo};
use crate::{CancelInput, CommitInput, DeleteInputChar, RowDown, RowUp};

/// What one selectable entry does when it is confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Connect. The request is built when the row is built, so the row and what
    /// it does cannot drift apart: a context listed out of a file the user
    /// opened carries that file, not whatever `KUBECONFIG` says.
    Context {
        request: ConnectRequest,
        label: String,
        current: bool,
        detail: Option<String>,
    },
    /// Ask for a kubeconfig somewhere else on disk.
    OpenKubeconfig,
    /// The generated starmap. Always offered, because it is the only entry that
    /// cannot fail for a reason outside this machine.
    Demo,
}

/// A line on the screen. Only a [`Choice`] can be highlighted or confirmed;
/// the other two are painted and skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Where the contexts under it came from.
    Source(String),
    /// A sentence where entries would have been.
    Note(String),
    Choice(Choice),
}

/// What the screen is doing, in the words it will use to say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The first scan has not answered yet.
    Scanning,
    Ready,
    /// Nothing could be read. Distinct from finding no kubeconfig: a file that
    /// will not parse is a fixable mistake and says which one it is.
    Unreadable(String),
    /// A connect is in flight, and to what.
    Connecting(String),
    /// The last attempt was refused, and why. The rows stay live under it.
    Refused(String),
    /// The generator is running off-thread.
    Generating,
}

/// The two entries that are always offered, in the order they are always in:
/// the way to reach a kubeconfig this process cannot see, and the way that
/// needs no cluster at all. A filter never hides them -- a typo must not be
/// able to remove the way out of this screen.
const FIXED_CHOICES: [Choice; 2] = [Choice::OpenKubeconfig, Choice::Demo];

pub struct LaunchState {
    pub query: String,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub status: Status,
    // Every source scanned so far, each with the request that produced it, so
    // filtering costs no disk and a context knows how to connect itself. A
    // second scan of the same request replaces its sources rather than stacking
    // a second copy of the same file.
    sources: Vec<(ScanRequest, ConfigSource)>,
}

impl LaunchState {
    pub fn new() -> LaunchState {
        let mut state = LaunchState {
            query: String::new(),
            rows: Vec::new(),
            selected: 0,
            status: Status::Scanning,
            sources: Vec::new(),
        };
        state.rebuild();
        state
    }

    /// A scan came back. The request is carried through rather than remembered,
    /// so two scans that finish out of order each land under their own header.
    pub fn scanned(&mut self, request: &ScanRequest, outcome: ScanOutcome) {
        let found = match outcome {
            ScanOutcome::Sources(sources) => sources,
            ScanOutcome::Failed(why) => {
                // A named file that will not read is the note; the detected
                // sources that did read stay listed, because one bad file must
                // not empty the screen.
                self.status = Status::Unreadable(why);
                self.rebuild();
                return;
            }
        };
        // Whether the highlight is only where it is for want of anywhere better.
        // Before a scan lands there is nothing to be on but the two fixed
        // entries, so a scan that finally produced contexts takes the highlight
        // -- and a scan that landed under a row the user has since chosen must
        // not move it.
        let waiting = !matches!(self.selected_choice(), Some(Choice::Context { .. }));
        self.sources.retain(|(asked, _)| asked != request);
        self.sources
            .extend(found.into_iter().map(|source| (request.clone(), source)));
        self.status = Status::Ready;
        self.rebuild();
        if waiting {
            self.focus_default();
        }
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.rebuild();
    }

    pub fn push_char(&mut self, text: &str) {
        self.query.push_str(text);
        self.rebuild();
    }

    pub fn delete_char(&mut self) {
        self.query.pop();
        self.rebuild();
    }

    pub fn select_next(&mut self) {
        if let Some(next) = self.next_choice(self.selected, 1) {
            self.selected = next;
        }
    }

    pub fn select_previous(&mut self) {
        if let Some(previous) = self.next_choice(self.selected, -1) {
            self.selected = previous;
        }
    }

    /// A click. Ignored on a header or a note, so the mouse can reach exactly
    /// what the keyboard can reach and nothing more.
    pub fn select(&mut self, index: usize) -> bool {
        if !matches!(self.rows.get(index), Some(Row::Choice(_))) {
            return false;
        }
        self.selected = index;
        true
    }

    pub fn choice_at(&self, index: usize) -> Option<&Choice> {
        match self.rows.get(index) {
            Some(Row::Choice(choice)) => Some(choice),
            _ => None,
        }
    }

    pub fn selected_choice(&self) -> Option<&Choice> {
        self.choice_at(self.selected)
    }

    /// Confirm the highlighted entry, and record that the attempt is running.
    /// Refused while one already is: a second `enter` on a connect that has not
    /// answered would open a second connection to answer the same question.
    pub fn confirm(&mut self) -> Option<Choice> {
        if matches!(self.status, Status::Connecting(_) | Status::Generating) {
            return None;
        }
        let choice = self.selected_choice()?.clone();
        match &choice {
            Choice::Context { label, .. } => self.status = Status::Connecting(label.clone()),
            Choice::Demo => self.status = Status::Generating,
            // Opening a picker is not an attempt at anything: nothing is in
            // flight, the rows do not change, and `enter` has to keep working
            // on this same row when the picker is dismissed.
            Choice::OpenKubeconfig => return Some(choice),
        }
        self.rebuild();
        Some(choice)
    }

    /// The attempt failed. The rows stay exactly as they were, including the
    /// highlight, so the obvious next act -- try the one below it -- is one
    /// keystroke away.
    pub fn refused(&mut self, why: String) {
        self.status = Status::Refused(why);
        self.rebuild();
    }

    /// A scan was asked for. Said out loud, because the alternative is a screen
    /// that looks finished while a network mount decides whether to answer.
    pub fn rescanning(&mut self) {
        self.status = Status::Scanning;
        self.rebuild();
    }

    /// The line under the list, in the words it will be read in. Absent when
    /// there is nothing to say, so the sheet does not reserve a row for a
    /// sentence that is not there.
    pub fn footer(&self) -> Option<String> {
        match &self.status {
            Status::Ready => None,
            Status::Scanning => Some("Looking for a kubeconfig…".to_string()),
            Status::Connecting(what) => Some(format!("Connecting to {what}…")),
            Status::Generating => Some("Generating a starmap…".to_string()),
            Status::Refused(why) => Some(why.clone()),
            // A failed scan stands in for the rows when there are none, and
            // saying it twice reads as two different problems. Saying it *nowhere*
            // is the bug this asks about: with another source already listed there
            // are rows, so a file that would not open had nothing on screen at
            // all and read as a file that opened fine.
            Status::Unreadable(why) => {
                (!self.rows.contains(&Row::Note(why.clone()))).then(|| why.clone())
            }
        }
    }

    // The highlight when nothing is worth holding: the source's own
    // current-context, which is what a person means by "my cluster", else the
    // first entry there is.
    fn focus_default(&mut self) {
        let preferred = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Choice(Choice::Context { current: true, .. })))
            .or_else(|| self.first_choice());
        if let Some(index) = preferred {
            self.selected = index;
        }
    }

    fn rebuild(&mut self) {
        // Only a row that is still there keeps the highlight, and it is compared
        // by value: a source re-read into different rows moves the highlight to
        // whichever of them is the same entry, not to whatever took its index.
        let held = self.rows.get(self.selected).cloned();
        self.rows.clear();
        for (request, source) in &self.sources {
            let matching: Vec<&ContextRow> = source
                .contexts
                .iter()
                .filter(|context| fuzzy_match(&self.query, &context.name).is_some())
                .collect();
            // A header over nothing is noise. Under a filter the whole source
            // goes; with no filter it stays and says what it declares, because
            // then its emptiness is the answer.
            if matching.is_empty() && !self.query.is_empty() {
                continue;
            }
            self.rows.push(Row::Source(source.label.clone()));
            if source.contexts.is_empty() {
                self.rows.push(if source.implicit {
                    Row::Choice(Choice::Context {
                        request: ConnectRequest {
                            source: request.clone(),
                            context: None,
                        },
                        label: "Connect with this account".to_string(),
                        current: true,
                        detail: None,
                    })
                } else {
                    Row::Note(
                        source
                            .note
                            .clone()
                            .unwrap_or_else(|| "declares no contexts".to_string()),
                    )
                });
                continue;
            }
            for context in matching {
                self.rows.push(Row::Choice(Choice::Context {
                    request: ConnectRequest {
                        source: request.clone(),
                        context: Some(context.name.clone()),
                    },
                    label: context.name.clone(),
                    current: context.current,
                    detail: detail(context),
                }));
            }
        }
        if self.sources.is_empty() {
            self.rows.push(Row::Note(self.empty_note()));
        }
        self.rows
            .extend(FIXED_CHOICES.iter().cloned().map(Row::Choice));
        self.selected = held
            .and_then(|row| self.rows.iter().position(|candidate| *candidate == row))
            .or_else(|| self.first_choice())
            .unwrap_or(0);
    }

    // What stands where the contexts would be when there are none at all. The
    // failure text wins, because "no kubeconfig found" is the wrong sentence
    // for a kubeconfig that exists and would not parse.
    fn empty_note(&self) -> String {
        match &self.status {
            Status::Scanning => "Looking for a kubeconfig…".to_string(),
            Status::Unreadable(why) => why.clone(),
            _ => "No kubeconfig found. Set KUBECONFIG, create ~/.kube/config, or open one below."
                .to_string(),
        }
    }

    fn first_choice(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Choice(_)))
    }

    // The next selectable row in one direction, or None at the end. Headers and
    // notes are stepped over rather than landed on, and the ends hold rather
    // than wrap: a list that wraps under a held arrow key never stops moving.
    fn next_choice(&self, from: usize, step: isize) -> Option<usize> {
        let mut at = from as isize;
        loop {
            at += step;
            if at < 0 || at as usize >= self.rows.len() {
                return None;
            }
            if matches!(self.rows[at as usize], Row::Choice(_)) {
                return Some(at as usize);
            }
        }
    }
}

impl Default for LaunchState {
    fn default() -> LaunchState {
        LaunchState::new()
    }
}

/// Everything about a context that may be shown besides its name: where the
/// server is, and which namespace the context defaults to. One function so the
/// decision has one place -- a kubeconfig's other fields are credentials, and
/// the reason this is not a `Display` impl is that a `Display` impl invites the
/// next field.
pub fn detail(context: &ContextRow) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(server) = &context.server {
        parts.push(server.clone());
    }
    if let Some(namespace) = &context.namespace {
        parts.push(format!("namespace {namespace}"));
    }
    (!parts.is_empty()).then(|| parts.join("  ·  "))
}

pub enum LaunchEvent {
    Dismissed,
    Chose(Choice),
}

pub struct LaunchView {
    focus: FocusHandle,
    state: LaunchState,
}

impl EventEmitter<LaunchEvent> for LaunchView {}

impl LaunchView {
    pub fn new(cx: &mut Context<Self>) -> LaunchView {
        LaunchView {
            focus: cx.focus_handle(),
            state: LaunchState::new(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn scanned(&mut self, request: &ScanRequest, outcome: ScanOutcome, cx: &mut Context<Self>) {
        self.state.scanned(request, outcome);
        cx.notify();
    }

    pub fn rescanning(&mut self, cx: &mut Context<Self>) {
        self.state.rescanning();
        cx.notify();
    }

    pub fn refused(&mut self, why: String, cx: &mut Context<Self>) {
        self.state.refused(why);
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(choice) = self.state.confirm() {
            cx.emit(LaunchEvent::Chose(choice));
            cx.notify();
        }
    }
}

impl Render for LaunchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let selected = self.state.selected;
        let footer = self.state.footer();

        div()
            .id("launch")
            .key_context("Palette")
            .track_focus(&self.focus)
            .w(px(MODAL_WIDTH))
            .max_w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme.shell.elevated_surface_background))
            .border_1()
            .border_color(rgb(theme.shell.border_variant))
            .rounded(px(12.0))
            .shadow_lg()
            .font_family(fonts.ui_family.clone())
            .role(Role::Dialog)
            .aria_label("Choose a cluster")
            .on_action(cx.listener(|this, _: &RowUp, _, cx| {
                this.state.select_previous();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                this.state.select_next();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                this.confirm(cx);
            }))
            .on_action(cx.listener(|_, _: &CancelInput, _, cx| {
                cx.emit(LaunchEvent::Dismissed);
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                this.state.delete_char();
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let keystroke = &event.keystroke;
                if keystroke.modifiers.control || keystroke.modifiers.alt {
                    return;
                }
                if let Some(key_char) = &keystroke.key_char {
                    this.state.push_char(key_char);
                    cx.notify();
                }
            }))
            .child(
                // The brand lockup: the helm, then the wordmark in the display
                // face. The mark is a bitmap and the name is type, which is why
                // one is sized in pixels and the other off the type ladder.
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.0))
                    .pt(px(28.0))
                    .pb(px(20.0))
                    .child(
                        img(brand_logo(theme.appearance))
                            .size(px(LAUNCH_LOGO_SIZE))
                            .flex_none(),
                    )
                    .child(
                        div()
                            .font_family(k10s_theme::DISPLAY_FAMILY)
                            .text_size(px(fonts.display()))
                            .text_color(rgb(theme.shell.text))
                            .child("k10s"),
                    ),
            )
            .children((!self.state.query.is_empty()).then(|| {
                div()
                    .id("launch-filter")
                    .h(px(28.0))
                    .flex_none()
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border))
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .role(Role::TextInput)
                    .aria_label("Filter contexts")
                    .child("filter")
                    .child(
                        div()
                            .text_color(rgb(theme.shell.text))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(SharedString::from(self.state.query.clone())),
                    )
                    .child(
                        div()
                            .w(px(2.0))
                            .h(px(14.0))
                            .flex_none()
                            .bg(rgb(theme.shell.cursor)),
                    )
            }))
            .child(
                div()
                    .id("launch-rows")
                    .max_h(px(MODAL_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .pb(px(8.0))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border))
                    .role(Role::ListBox)
                    .aria_label("Clusters")
                    .children(self.state.rows.iter().enumerate().map(|(at, row)| {
                        match row {
                            Row::Source(label) => div()
                                .h(px(LIST_ROW_HEIGHT))
                                .px(px(16.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .text_size(px(fonts.small()))
                                .text_color(rgb(theme.shell.text_muted))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .child(SharedString::from(label.clone()))
                                .into_any_element(),
                            Row::Note(note) => div()
                                .px(px(16.0))
                                .py(px(6.0))
                                .flex_none()
                                .text_size(px(fonts.small()))
                                .text_color(rgb(theme.shell.text_muted))
                                .child(SharedString::from(note.clone()))
                                .into_any_element(),
                            Row::Choice(choice) => {
                                let (label, current, detail) = match choice {
                                    Choice::Context {
                                        label,
                                        current,
                                        detail,
                                        ..
                                    } => (label.clone(), *current, detail.clone()),
                                    Choice::OpenKubeconfig => {
                                        ("Open another kubeconfig…".to_string(), false, None)
                                    }
                                    Choice::Demo => {
                                        ("Explore a generated starmap".to_string(), false, None)
                                    }
                                };
                                let chosen = at == selected;
                                div()
                                    .id(("launch-row", at))
                                    .h(px(LIST_ROW_HEIGHT))
                                    .px(px(16.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(12.0))
                                    .cursor_pointer()
                                    .bg(rgb(if chosen {
                                        theme.shell.element_selected
                                    } else {
                                        theme.shell.elevated_surface_background
                                    }))
                                    .hover(|row| row.bg(rgb(theme.shell.element_hover)))
                                    .text_size(px(fonts.ui_size))
                                    .text_color(rgb(theme.shell.text))
                                    .role(Role::ListBoxOption)
                                    .aria_label(label.clone())
                                    .aria_selected(chosen)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                            if this.state.select(at) {
                                                this.confirm(cx);
                                            }
                                        }),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .min_w(px(0.0))
                                            .overflow_hidden()
                                            .child(
                                                div()
                                                    .w(px(8.0))
                                                    .flex_none()
                                                    .text_color(rgb(if chosen {
                                                        theme.shell.text_accent
                                                    } else {
                                                        theme.shell.text_muted
                                                    }))
                                                    .child("▸"),
                                            )
                                            .child(
                                                div()
                                                    .whitespace_nowrap()
                                                    .overflow_hidden()
                                                    .child(SharedString::from(label)),
                                            )
                                            .children(detail.map(|detail| {
                                                div()
                                                    .text_size(px(fonts.xsmall()))
                                                    .text_color(rgb(theme.shell.text_muted))
                                                    .whitespace_nowrap()
                                                    .overflow_hidden()
                                                    .child(SharedString::from(detail))
                                            })),
                                    )
                                    .children(current.then(|| {
                                        div()
                                            .flex_none()
                                            .px(px(5.0))
                                            .py(px(1.0))
                                            .rounded(px(3.0))
                                            .bg(rgb(theme.shell.element_background))
                                            .border_1()
                                            .border_color(rgb(theme.shell.border_variant))
                                            .text_size(px(fonts.xsmall()))
                                            .text_color(rgb(theme.shell.text_muted))
                                            .child("current-context")
                                    }))
                                    .into_any_element()
                            }
                        }
                    })),
            )
            .children(footer.map(|footer| {
                div()
                    .flex_none()
                    .px(px(16.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border))
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text))
                    .child(SharedString::from(footer))
            }))
    }
}

#[cfg(test)]
#[path = "launch_test.rs"]
mod tests;
