//! The text view: describe documents and live log feeds.
//!
//! F needs a place for text to live, deliberately smaller than the editor
//! §5.2 will eventually want: a virtualized, read-only pane over a bounded
//! ring of lines with scrolling, follow, and regex search (case-insensitive,
//! with a bounded compiled-pattern size; an invalid pattern is a labelled
//! state in the status line, never a panic or a silently empty result). All
//! behaviour lives in the pure [`TextState`] so it is tested without a
//! window; the gpui view is a thin shell that renders only the visible slice
//! and repaints on notify -- a quiet feed paints nothing.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    Context, FocusHandle, IntoElement, KeyDownEvent, ParentElement, Render, Role, ScrollWheelEvent,
    SharedString, Styled, Window, canvas, div, prelude::*, px, rgb,
};

use crate::provider::{
    ContainersOutcome, DescribeRequest, DocOutcome, LogChunk, LogRequest, LogStop, ReadProvider,
    Reply, WorkloadLogRequest,
};
use crate::ui::{CONTENT_PADDING, PANEL_HEADER_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{
    CancelDoc, CancelInput, CommitInput, CycleContainer, DeleteInputChar, DocEnd, DocHome,
    DocPageDown, DocPageUp, DocScrollDown, DocScrollUp, EnterSearch, NextMatch, PrevMatch, Reload,
    ToggleFollow, TogglePrevious, ToggleTimestamps,
};

pub const MAX_LOG_LINES: usize = 10_000;

// A hostile or fat-fingered pattern must not balloon the compiled program;
// the regex crate's default limit is 10 MiB, which is not "bounded" in this
// repo's sense.
const MAX_PATTERN_BYTES: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct Search {
    pub query: String,
    // A failed compile keeps the query visible and the error printable; it
    // matches nothing rather than something surprising.
    pattern: Result<regex::Regex, String>,
    matches: Vec<usize>,
    current: usize,
}

impl Search {
    fn compile(query: &str) -> Result<regex::Regex, String> {
        regex::RegexBuilder::new(query)
            .case_insensitive(true)
            .size_limit(MAX_PATTERN_BYTES)
            .build()
            .map_err(|error| one_line(&error.to_string()))
    }

    fn matches_line(&self, line: &str) -> bool {
        match &self.pattern {
            Ok(regex) => regex.is_match(line),
            Err(_) => false,
        }
    }
}

// Regex errors print multi-line with a caret; a status line gets one line.
fn one_line(text: &str) -> String {
    const MAX: usize = 120;
    let flat: Vec<&str> = text.split_whitespace().collect();
    let mut out = flat.join(" ");
    if out.chars().count() > MAX {
        out = out.chars().take(MAX).collect();
        out.push('\u{2026}');
    }
    out
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
            for index in first_new..self.lines.len() {
                if search.matches_line(&self.lines[index]) {
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
                let mut search = Search {
                    pattern: Search::compile(&query),
                    query,
                    matches: Vec::new(),
                    current: 0,
                };
                search.matches = self
                    .lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| search.matches_line(line))
                    .map(|(index, _)| index)
                    .collect();
                search.current = search
                    .matches
                    .iter()
                    .position(|m| *m >= self.top)
                    .unwrap_or(0)
                    .min(search.matches.len().saturating_sub(1));
                self.search = Some(search);
                self.jump_to_current();
            }
        }
    }

    pub fn search(&self) -> Option<(&str, usize, usize)> {
        self.search
            .as_ref()
            .map(|s| (s.query.as_str(), s.current + 1, s.matches.len()))
    }

    // The labelled invalid-pattern state: Some(reason) while the query does
    // not compile, in which case the search is live but matches nothing.
    pub fn search_error(&self) -> Option<&str> {
        self.search
            .as_ref()
            .and_then(|s| s.pattern.as_ref().err().map(String::as_str))
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
    if kubelet_stamped(line.as_bytes())
        && let Some(space) = line.find(' ')
    {
        return &line[space + 1..];
    }
    line
}

/// The whole `YYYY-MM-DDThh:mm:ss` head, not its first three characters: what
/// gets thrown away here is the start of a log line, and a line that merely
/// begins with four digits and a dash is not a stamped one.
fn kubelet_stamped(bytes: &[u8]) -> bool {
    bytes.len() > 19
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[10] == b'T'
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b':'
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[16] == b':'
        && bytes[17..19].iter().all(u8::is_ascii_digit)
}

enum Source {
    Doc(DescribeRequest),
    // Helm's stored releases. A document rather than a list view because that is
    // what it is: an inventory with a history under each entry, read-only, with
    // the same scrolling and regex search every other document here has.
    Releases,
    Logs(LogSource),
}

// One pod's stream, or the provider's merge over a workload's pods. The pod
// feed owns container cycling and `previous`; the merged feed has neither --
// those are per-pod questions.
enum Feed {
    Pod(LogRequest),
    Workload(WorkloadLogRequest),
}

struct LogSource {
    feed: Feed,
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
    viewport: Viewport,
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
            viewport: Viewport::default(),
        };
        view.reload(cx);
        view
    }

    pub fn releases(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> TextView {
        let mut view = TextView {
            focus: cx.focus_handle(),
            provider,
            title: "helm releases".into(),
            state: TextState::new(usize::MAX),
            source: Source::Releases,
            status: Some("loading...".to_string()),
            searching: false,
            input: String::new(),
            show_timestamps: true,
            generation: 0,
            viewport: Viewport::default(),
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
                feed: Feed::Pod(request),
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
            viewport: Viewport::default(),
        };
        view.state.toggle_follow();
        view.resolve_containers(cx);
        view
    }

    pub fn workload_logs(
        provider: Rc<dyn ReadProvider>,
        request: WorkloadLogRequest,
        cx: &mut Context<Self>,
    ) -> TextView {
        let mut view = TextView {
            focus: cx.focus_handle(),
            provider,
            title: format!(
                "logs {} {}",
                k10s_core::kind_short(request.kind),
                request.name
            )
            .into(),
            state: TextState::new(MAX_LOG_LINES),
            source: Source::Logs(LogSource {
                feed: Feed::Workload(request),
                containers: Vec::new(),
                stop: None,
                ended: None,
                dropped_by_ui: Arc::new(AtomicU64::new(0)),
            }),
            status: None,
            searching: false,
            input: String::new(),
            show_timestamps: true,
            generation: 0,
            viewport: Viewport::default(),
        };
        view.state.toggle_follow();
        view.start_follow(cx);
        view
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    // Both documents this view can hold arrive the same way and are shown the
    // same way; only the question differs, which is why the two live in one
    // branch rather than in two copies of the spawn below.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply: Reply<DocOutcome> = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        match &self.source {
            Source::Doc(request) => self.provider.fetch_describe(request, reply),
            Source::Releases => self.provider.fetch_releases(reply),
            Source::Logs(_) => {
                self.start_follow(cx);
                return;
            }
        }
        self.status = Some("loading...".to_string());
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
                            this.status = Some(format!("{what}: access denied for this account"));
                        }
                        DocOutcome::Failed(why) => this.status = Some(why),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn resolve_containers(&mut self, cx: &mut Context<Self>) {
        let Source::Logs(LogSource {
            feed: Feed::Pod(request),
            ..
        }) = &self.source
        else {
            return;
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_containers(
            &request.namespace,
            &request.pod,
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
                        if let Feed::Pod(request) = &mut logs.feed
                            && request.container.is_none()
                        {
                            request.container = containers.first().cloned();
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
        let on_chunk: Box<dyn Fn(LogChunk) + Send + Sync> = Box::new(move |chunk| {
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
        });
        let stop = match &logs.feed {
            Feed::Pod(request) => self.provider.follow_log(request, on_chunk),
            Feed::Workload(request) => self.provider.follow_workload_logs(request, on_chunk),
        };
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

    // Container cycling and `previous` are pod-feed knobs; on a workload
    // merge they do nothing rather than something surprising.
    fn restart_logs(
        &mut self,
        mutate: impl FnOnce(&mut LogRequest, &[String]),
        cx: &mut Context<Self>,
    ) {
        if let Source::Logs(logs) = &mut self.source
            && let Feed::Pod(request) = &mut logs.feed
        {
            mutate(request, &logs.containers);
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
            if let Some(reason) = self.state.search_error() {
                parts.push(format!("/{query} invalid pattern: {reason}"));
            } else if total == 0 {
                parts.push(format!("/{query} no matches"));
            } else {
                parts.push(format!("/{query} {current}/{total}"));
            }
        }
        if let Source::Logs(logs) = &self.source {
            match &logs.feed {
                Feed::Pod(request) => {
                    if let Some(container) = &request.container {
                        parts.push(format!("container {container}"));
                    }
                    if request.previous {
                        parts.push("previous".to_string());
                    }
                }
                Feed::Workload(_) => parts.push("merged pod follows".to_string()),
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

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let panel_header = if matches!(self.source, Source::Logs(_)) {
            PANEL_HEADER_HEIGHT
        } else {
            0.0
        };
        let rows = self.viewport.rows(
            panel_header + STATUS_BAR_HEIGHT,
            CONTENT_PADDING * 2.0,
            k10s_theme::typography(cx).line_height(),
            400,
        );
        self.state.set_viewport(rows.max(4));
        cx.notify();
    }
}

impl crate::item::Item for TextView {
    fn title(&self) -> SharedString {
        TextView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        TextView::focus_handle(self)
    }
}

impl Render for TextView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();

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
            .id("text-view")
            .key_context(if self.searching { "Typing" } else { "Doc" })
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(theme.shell.editor_background))
            .font_family(fonts.buffer_family.clone())
            .text_color(rgb(theme.shell.editor_foreground))
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
                let row = k10s_theme::typography(cx).line_height();
                let delta = f32::from(event.delta.pixel_delta(px(row)).y);
                this.state.scroll_by(-(delta / row).round() as i64);
                cx.notify();
            }))
            .children(is_logs.then(|| panel_header(&theme, &fonts, self.title.clone())))
            .child(
                div()
                    .id("text-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .role(if is_logs { Role::Log } else { Role::Document })
                    .aria_label(self.title.clone())
                    .children(lines.into_iter().map(|(index, text)| {
                        let mut line = div()
                            .h(px(fonts.line_height()))
                            .flex_none()
                            .overflow_hidden()
                            .text_size(px(fonts.buffer_size))
                            .text_color(rgb(theme.shell.editor_foreground))
                            .whitespace_nowrap();
                        if Some(index) == match_line {
                            let (color, alpha) = theme.shell.search_match_background;
                            line = line.bg(rgb(color).alpha(alpha));
                        }
                        line.child(text)
                    })),
            )
            .child(
                div()
                    .h(px(STATUS_BAR_HEIGHT))
                    .flex_none()
                    .px(px(8.0))
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
                    .child(self.status_line()),
            )
    }
}

#[cfg(test)]
#[path = "text_test.rs"]
mod tests;
