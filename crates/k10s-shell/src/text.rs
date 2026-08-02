//! The text view: describe documents and live log feeds.
//!
//! F needs a place for text to live, deliberately smaller than the editor
//! §5.2 will eventually want: a virtualized, read-only pane over a bounded
//! ring of lines with scrolling, follow, and substring search. All behaviour
//! lives in the pure [`TextState`] so it is tested without a window; the
//! gpui view is a thin shell that renders only the visible slice and repaints
//! on notify -- a quiet feed paints nothing.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    Context, FocusHandle, IntoElement, KeyDownEvent, ParentElement, Render, ScrollWheelEvent,
    SharedString, Styled, Window, div, prelude::*, px, rgb,
};

use crate::provider::{
    ContainersOutcome, DescribeRequest, DocOutcome, LogChunk, LogRequest, LogStop, ReadProvider,
};
use crate::{
    CancelDoc, CancelInput, CommitInput, CycleContainer, DeleteInputChar, DocEnd, DocHome,
    DocPageDown, DocPageUp, DocScrollDown, DocScrollUp, EnterSearch, NextMatch, PrevMatch, Reload,
    ToggleFollow, TogglePrevious, ToggleTimestamps,
};

pub const MAX_LOG_LINES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub query: String,
    matches: Vec<usize>,
    current: usize,
}

#[derive(Debug)]
pub struct TextState {
    lines: VecDeque<String>,
    max_lines: usize,
    top: usize,
    follow: bool,
    viewport: usize,
    dropped: u64,
    search: Option<Search>,
}

impl TextState {
    pub fn new(max_lines: usize) -> TextState {
        TextState {
            lines: VecDeque::new(),
            max_lines,
            top: 0,
            follow: false,
            viewport: 1,
            dropped: 0,
            search: None,
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn top(&self) -> usize {
        self.top
    }

    pub fn following(&self) -> bool {
        self.follow
    }

    pub fn set_lines(&mut self, lines: Vec<String>) {
        self.lines = lines.into();
        self.top = 0;
        self.dropped = 0;
        if let Some(search) = &self.search {
            let query = search.query.clone();
            self.set_search(Some(query));
        }
        self.clamp();
    }

    pub fn append(&mut self, batch: Vec<String>) {
        if batch.is_empty() {
            return;
        }
        let first_new = self.lines.len();
        self.lines.extend(batch);
        if let Some(search) = &mut self.search {
            let needle = search.query.to_lowercase();
            for index in first_new..self.lines.len() {
                if self.lines[index].to_lowercase().contains(&needle) {
                    search.matches.push(index);
                }
            }
        }
        let over = self.lines.len().saturating_sub(self.max_lines);
        if over > 0 {
            self.lines.drain(..over);
            self.dropped += over as u64;
            self.top = self.top.saturating_sub(over);
            if let Some(search) = &mut self.search {
                let removed = search.matches.iter().take_while(|m| **m < over).count();
                search.matches.drain(..removed);
                for entry in &mut search.matches {
                    *entry -= over;
                }
                search.current = search.current.saturating_sub(removed);
            }
        }
        if self.follow {
            self.top = self.bottom_top();
        }
        self.clamp();
    }

    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        if self.follow {
            self.top = self.bottom_top();
        }
        self.clamp();
    }

    fn bottom_top(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport)
    }

    pub fn at_bottom(&self) -> bool {
        self.top >= self.bottom_top()
    }

    fn clamp(&mut self) {
        self.top = self.top.min(self.bottom_top());
        if let Some(search) = &mut self.search {
            search.current = search.current.min(search.matches.len().saturating_sub(1));
        }
    }

    pub fn scroll_by(&mut self, delta: i64) {
        self.follow = false;
        let top = self.top as i64 + delta;
        self.top = top.clamp(0, self.bottom_top() as i64) as usize;
    }

    pub fn page_up(&mut self) {
        self.scroll_by(-((self.viewport.saturating_sub(1)).max(1) as i64));
    }

    pub fn page_down(&mut self) {
        self.scroll_by((self.viewport.saturating_sub(1)).max(1) as i64);
    }

    pub fn home(&mut self) {
        self.follow = false;
        self.top = 0;
    }

    pub fn end(&mut self) {
        self.top = self.bottom_top();
    }

    pub fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        if self.follow {
            self.top = self.bottom_top();
        }
    }

    pub fn set_search(&mut self, query: Option<String>) {
        match query {
            None => self.search = None,
            Some(query) if query.is_empty() => self.search = None,
            Some(query) => {
                let needle = query.to_lowercase();
                let matches: Vec<usize> = self
                    .lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.to_lowercase().contains(&needle))
                    .map(|(index, _)| index)
                    .collect();
                let current = matches
                    .iter()
                    .position(|m| *m >= self.top)
                    .unwrap_or(0)
                    .min(matches.len().saturating_sub(1));
                self.search = Some(Search {
                    query,
                    matches,
                    current,
                });
                self.jump_to_current();
            }
        }
    }

    pub fn search(&self) -> Option<(&str, usize, usize)> {
        self.search
            .as_ref()
            .map(|s| (s.query.as_str(), s.current + 1, s.matches.len()))
    }

    pub fn next_match(&mut self) {
        if let Some(search) = &mut self.search
            && !search.matches.is_empty()
        {
            search.current = (search.current + 1) % search.matches.len();
            self.jump_to_current();
        }
    }

    pub fn prev_match(&mut self) {
        if let Some(search) = &mut self.search
            && !search.matches.is_empty()
        {
            search.current = (search.current + search.matches.len() - 1) % search.matches.len();
            self.jump_to_current();
        }
    }

    fn jump_to_current(&mut self) {
        let Some(target) = self.current_match_line() else {
            return;
        };
        self.follow = false;
        self.top = target
            .saturating_sub(self.viewport / 3)
            .min(self.bottom_top());
    }

    pub fn current_match_line(&self) -> Option<usize> {
        let search = self.search.as_ref()?;
        search.matches.get(search.current).copied()
    }

    pub fn visible(&self) -> impl Iterator<Item = (usize, &str)> {
        self.lines
            .iter()
            .enumerate()
            .skip(self.top)
            .take(self.viewport)
            .map(|(index, line)| (index, line.as_str()))
    }
}

// "2026-08-02T05:00:01Z ready" -> "ready", but only when the head actually
// looks like the timestamp the kubelet prepends.
pub fn strip_timestamp(line: &str) -> &str {
    let bytes = line.as_bytes();
    if bytes.len() > 11
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[10] == b'T'
        && let Some(space) = line.find(' ')
    {
        return &line[space + 1..];
    }
    line
}

const DOC_BG: u32 = 0x0e0c17;
const DOC_TEXT: u32 = 0xcfcae6;
const DOC_DIM: u32 = 0x6e6890;
const DOC_MATCH_BG: u32 = 0x3a2f5c;
const DOC_STATUS: u32 = 0xb8b2d9;
const ROW_PX: f32 = 16.0;
const CHROME_PX: f32 = 92.0;
const MONO: &str = "JetBrains Mono";

enum Source {
    Doc(DescribeRequest),
    Logs(LogSource),
}

struct LogSource {
    request: LogRequest,
    containers: Vec<String>,
    stop: Option<LogStop>,
    ended: Option<String>,
    dropped_by_ui: Arc<AtomicU64>,
}

pub struct TextView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    title: SharedString,
    state: TextState,
    source: Source,
    status: Option<String>,
    searching: bool,
    input: String,
    show_timestamps: bool,
    generation: u64,
}

impl TextView {
    pub fn doc(
        provider: Rc<dyn ReadProvider>,
        request: DescribeRequest,
        cx: &mut Context<Self>,
    ) -> TextView {
        let mut view = TextView {
            focus: cx.focus_handle(),
            provider,
            title: format!("describe {}", request.name).into(),
            state: TextState::new(usize::MAX),
            source: Source::Doc(request),
            status: Some("loading...".to_string()),
            searching: false,
            input: String::new(),
            show_timestamps: true,
            generation: 0,
        };
        view.reload(cx);
        view
    }

    pub fn logs(
        provider: Rc<dyn ReadProvider>,
        request: LogRequest,
        cx: &mut Context<Self>,
    ) -> TextView {
        let mut view = TextView {
            focus: cx.focus_handle(),
            provider,
            title: format!("logs {}", request.pod).into(),
            state: TextState::new(MAX_LOG_LINES),
            source: Source::Logs(LogSource {
                request,
                containers: Vec::new(),
                stop: None,
                ended: None,
                dropped_by_ui: Arc::new(AtomicU64::new(0)),
            }),
            status: Some("looking up containers...".to_string()),
            searching: false,
            input: String::new(),
            show_timestamps: true,
            generation: 0,
        };
        view.state.toggle_follow();
        view.resolve_containers(cx);
        view
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        match &self.source {
            Source::Doc(request) => {
                self.status = Some("loading...".to_string());
                let (tx, rx) = futures::channel::oneshot::channel();
                self.provider.fetch_describe(
                    request,
                    Box::new(move |outcome| {
                        let _ = tx.send(outcome);
                    }),
                );
                cx.spawn(async move |this, cx| {
                    if let Ok(outcome) = rx.await {
                        let _ = this.update(cx, |this, cx| {
                            if this.generation != generation {
                                return;
                            }
                            match outcome {
                                DocOutcome::Doc { title, lines } => {
                                    this.title = title.into();
                                    this.state.set_lines(lines);
                                    this.status = None;
                                }
                                DocOutcome::Denied(what) => {
                                    this.status =
                                        Some(format!("{what}: access denied for this account"));
                                }
                                DocOutcome::Failed(why) => this.status = Some(why),
                            }
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
            Source::Logs(_) => self.start_follow(cx),
        }
    }

    fn resolve_containers(&mut self, cx: &mut Context<Self>) {
        let Source::Logs(logs) = &self.source else {
            return;
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_containers(
            &logs.request.namespace,
            &logs.request.pod,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            let outcome = rx
                .await
                .unwrap_or(ContainersOutcome::Containers(Vec::new()));
            let _ = this.update(cx, |this, cx| {
                if let Source::Logs(logs) = &mut this.source {
                    if let ContainersOutcome::Containers(containers) = outcome {
                        if logs.request.container.is_none() {
                            logs.request.container = containers.first().cloned();
                        }
                        logs.containers = containers;
                    }
                    // A denied pod read does not gate the follow: the logs
                    // subresource has its own RBAC answer, so ask it.
                    this.start_follow(cx);
                }
            });
        })
        .detach();
    }

    fn start_follow(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let Source::Logs(logs) = &mut self.source else {
            return;
        };
        logs.stop = None;
        logs.ended = None;
        self.state.set_lines(Vec::new());
        if !self.state.following() {
            self.state.toggle_follow();
        }
        self.status = None;

        let (tx, mut rx) = futures::channel::mpsc::channel::<LogChunk>(256);
        let dropped = logs.dropped_by_ui.clone();
        let stop = self.provider.follow_log(
            &logs.request,
            Box::new(move |chunk| {
                let mut tx = tx.clone();
                let lines = match &chunk {
                    LogChunk::Lines(lines) => lines.len() as u64,
                    _ => 0,
                };
                if tx.try_send(chunk).is_err() {
                    // The feed outran the UI; the count is shown, the
                    // newest lines win, and memory stays bounded.
                    dropped.fetch_add(lines, Ordering::Relaxed);
                }
            }),
        );
        if let Source::Logs(logs) = &mut self.source {
            logs.stop = Some(stop);
        }
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(chunk) = rx.next().await {
                let live = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return false;
                    }
                    this.apply_chunk(chunk, cx);
                    true
                });
                if !matches!(live, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    fn apply_chunk(&mut self, chunk: LogChunk, cx: &mut Context<Self>) {
        match chunk {
            LogChunk::Lines(lines) => self.state.append(lines),
            LogChunk::Ended(why) => {
                if let Source::Logs(logs) = &mut self.source {
                    logs.ended = Some(why);
                }
            }
            LogChunk::Denied(what) => {
                self.status = Some(format!("{what}: access denied for this account"));
            }
            LogChunk::Failed(why) => self.status = Some(why),
        }
        cx.notify();
    }

    fn restart_logs(
        &mut self,
        mutate: impl FnOnce(&mut LogRequest, &[String]),
        cx: &mut Context<Self>,
    ) {
        if let Source::Logs(logs) = &mut self.source {
            mutate(&mut logs.request, &logs.containers);
            self.start_follow(cx);
            cx.notify();
        }
    }

    fn status_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!("{} lines", self.state.len()));
        if self.state.dropped() > 0 {
            parts.push(format!("{} scrolled out", self.state.dropped()));
        }
        if self.searching {
            parts.push(format!("/{}_", self.input));
        } else if let Some((query, current, total)) = self.state.search() {
            if total == 0 {
                parts.push(format!("/{query} no matches"));
            } else {
                parts.push(format!("/{query} {current}/{total}"));
            }
        }
        if let Source::Logs(logs) = &self.source {
            if let Some(container) = &logs.request.container {
                parts.push(format!("container {container}"));
            }
            if logs.request.previous {
                parts.push("previous".to_string());
            }
            if self.state.following() {
                parts.push("follow".to_string());
            }
            let dropped = logs.dropped_by_ui.load(Ordering::Relaxed);
            if dropped > 0 {
                parts.push(format!("{dropped} dropped by backpressure"));
            }
            if let Some(why) = &logs.ended {
                parts.push(format!("ended: {why}"));
            }
            if !self.show_timestamps {
                parts.push("timestamps hidden".to_string());
            }
        }
        if let Some(status) = &self.status {
            parts.push(status.clone());
        }
        parts.join("  ·  ")
    }
}

impl Render for TextView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = f32::from(window.viewport_size().height);
        let rows = (((height - CHROME_PX) / ROW_PX) as usize).clamp(4, 400);
        self.state.set_viewport(rows);

        let is_logs = matches!(self.source, Source::Logs(_));
        let match_line = self.state.current_match_line();
        let show_timestamps = self.show_timestamps;
        let lines: Vec<_> = self
            .state
            .visible()
            .map(|(index, line)| {
                let text: SharedString = if is_logs && !show_timestamps {
                    strip_timestamp(line).to_string().into()
                } else {
                    line.to_string().into()
                };
                (index, text)
            })
            .collect();

        div()
            .key_context(if self.searching { "Typing" } else { "Doc" })
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(DOC_BG))
            .font_family(MONO)
            .on_action(cx.listener(|this, _: &DocScrollUp, _, cx| {
                this.state.scroll_by(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocScrollDown, _, cx| {
                this.state.scroll_by(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocPageUp, _, cx| {
                this.state.page_up();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocPageDown, _, cx| {
                this.state.page_down();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocHome, _, cx| {
                this.state.home();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocEnd, _, cx| {
                this.state.end();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EnterSearch, _, cx| {
                this.searching = true;
                this.input = this
                    .state
                    .search()
                    .map(|(query, ..)| query.to_string())
                    .unwrap_or_default();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextMatch, _, cx| {
                this.state.next_match();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrevMatch, _, cx| {
                this.state.prev_match();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleFollow, _, cx| {
                this.state.toggle_follow();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleTimestamps, _, cx| {
                this.show_timestamps = !this.show_timestamps;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CycleContainer, _, cx| {
                this.restart_logs(
                    |request, containers| {
                        if containers.is_empty() {
                            return;
                        }
                        let at = request
                            .container
                            .as_deref()
                            .and_then(|current| containers.iter().position(|c| c == current))
                            .unwrap_or(containers.len() - 1);
                        request.container = Some(containers[(at + 1) % containers.len()].clone());
                    },
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &TogglePrevious, _, cx| {
                this.restart_logs(|request, _| request.previous = !request.previous, cx);
            }))
            .on_action(cx.listener(|this, _: &Reload, _, cx| {
                this.reload(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelDoc, _, cx| {
                if this.state.search().is_some() {
                    this.state.set_search(None);
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                this.searching = false;
                this.state.set_search(Some(this.input.clone()));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                this.searching = false;
                this.input.clear();
                this.state.set_search(None);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                this.input.pop();
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !this.searching {
                    return;
                }
                let keystroke = &event.keystroke;
                if keystroke.modifiers.control || keystroke.modifiers.alt {
                    return;
                }
                if let Some(key_char) = &keystroke.key_char {
                    this.input.push_str(key_char);
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = f32::from(event.delta.pixel_delta(px(ROW_PX)).y);
                this.state.scroll_by(-(delta / ROW_PX) as i64);
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .children(lines.into_iter().map(|(index, text)| {
                        let mut line = div()
                            .h(px(ROW_PX))
                            .overflow_hidden()
                            .text_size(px(11.0))
                            .text_color(rgb(DOC_TEXT))
                            .whitespace_nowrap();
                        if Some(index) == match_line {
                            line = line.bg(rgb(DOC_MATCH_BG));
                        }
                        line.child(text)
                    })),
            )
            .child(
                div()
                    .h(px(20.0))
                    .px(px(8.0))
                    .text_size(px(10.0))
                    .text_color(if self.status.is_some() {
                        rgb(DOC_STATUS)
                    } else {
                        rgb(DOC_DIM)
                    })
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(self.status_line()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn the_ring_evicts_from_the_front_and_keeps_the_view_stable() {
        let mut state = TextState::new(10);
        state.set_viewport(4);
        state.append(numbered(10));
        state.scroll_by(2);
        assert_eq!(state.top(), 2);
        assert!(!state.following());

        state.append(vec!["line 10".to_string(), "line 11".to_string()]);
        assert_eq!(state.len(), 10);
        assert_eq!(state.dropped(), 2);
        assert_eq!(
            state.top(),
            0,
            "the anchor shifts with eviction so the same lines stay on screen"
        );
        assert_eq!(state.visible().next().unwrap().1, "line 2");
    }

    #[test]
    fn follow_sticks_to_the_bottom_until_the_user_scrolls() {
        let mut state = TextState::new(100);
        state.set_viewport(5);
        state.toggle_follow();
        state.append(numbered(20));
        assert_eq!(state.top(), 15, "follow rides the tail");
        assert!(state.at_bottom());

        state.scroll_by(-3);
        assert!(!state.following(), "scrolling up breaks follow");
        state.append(numbered(5));
        assert_eq!(state.top(), 12, "and the view stays put");

        state.toggle_follow();
        assert!(state.at_bottom());
    }

    #[test]
    fn search_is_case_insensitive_cycles_and_survives_appends() {
        let mut state = TextState::new(100);
        state.set_viewport(6);
        state.set_lines(vec![
            "listening".to_string(),
            "ERROR one".to_string(),
            "ready".to_string(),
            "error two".to_string(),
        ]);
        state.set_search(Some("error".to_string()));
        let (query, current, total) = state.search().expect("a search");
        assert_eq!((query, current, total), ("error", 1, 2));
        assert_eq!(state.current_match_line(), Some(1));

        state.next_match();
        assert_eq!(state.current_match_line(), Some(3));
        state.next_match();
        assert_eq!(state.current_match_line(), Some(1), "wraps");
        state.prev_match();
        assert_eq!(state.current_match_line(), Some(3));

        state.append(vec!["Error three".to_string()]);
        assert_eq!(state.search().unwrap().2, 3, "appends join the matches");

        state.set_search(None);
        assert!(state.search().is_none());
    }

    #[test]
    fn search_indices_survive_ring_eviction() {
        let mut state = TextState::new(4);
        state.append(vec![
            "error a".to_string(),
            "calm".to_string(),
            "error b".to_string(),
            "calm".to_string(),
        ]);
        state.set_search(Some("error".to_string()));
        assert_eq!(state.search().unwrap().2, 2);

        state.append(vec!["calm".to_string(), "error c".to_string()]);
        let (_, _, total) = state.search().expect("still searching");
        assert_eq!(total, 2, "the evicted match is gone, the new one counted");
        state.next_match();
        let line = state.current_match_line().expect("a match line");
        assert!(state.visible().count() > 0);
        assert!(line < state.len());
    }

    #[test]
    fn scrolling_clamps_and_pages_by_the_viewport() {
        let mut state = TextState::new(100);
        state.set_viewport(10);
        state.set_lines(numbered(25));
        state.scroll_by(-5);
        assert_eq!(state.top(), 0);
        state.page_down();
        assert_eq!(state.top(), 9);
        state.page_down();
        assert_eq!(state.top(), 15, "clamped to the last full screen");
        state.end();
        assert_eq!(state.top(), 15);
        state.home();
        assert_eq!(state.top(), 0);
        state.scroll_by(1000);
        assert_eq!(state.top(), 15);
    }

    #[test]
    fn the_kubelet_timestamp_is_stripped_only_when_it_is_one() {
        assert_eq!(strip_timestamp("2026-08-02T05:00:01Z ready"), "ready");
        assert_eq!(
            strip_timestamp("2026-08-02T05:00:01.123456789Z GET /healthz"),
            "GET /healthz"
        );
        assert_eq!(strip_timestamp("plain line"), "plain line");
        assert_eq!(
            strip_timestamp("12:00:01 not a date"),
            "12:00:01 not a date"
        );
        assert_eq!(strip_timestamp(""), "");
    }
}
