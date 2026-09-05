//! The embedded terminal: an exec or attach session rendered as a cell grid.
//!
//! The VT machinery is `alacritty_terminal` -- the ROADMAP forbids writing a
//! parser -- fed raw bytes from an [`ExecSession`] behind the provider seam.
//! All terminal logic lives in the pure [`TerminalState`] (grid from bytes,
//! resize, cursor, bounded history) and the pure [`key_bytes`] input encoding,
//! both tested with no window and no transport; the gpui view is a thin shell
//! that paints the visible grid as monospace rows and forwards keystrokes.
//! Cell foreground and background from the grid reach [`StyledText`] as
//! highlight runs. Scrollback is alacritty's own history, capped at
//! [`SCROLLBACK`] lines, so a noisy session cannot grow without bound. The
//! `Terminal` key context captures everything except the item-management
//! chords (see `keybindings()`), so plain letters and escape reach the remote
//! shell instead of dispatching commands.

use std::rc::Rc;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, point_to_viewport};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

use gpui::{
    Context, FocusHandle, FontStyle, FontWeight, HighlightStyle, IntoElement, KeyDownEvent,
    Keystroke, ParentElement, Render, Role, ScrollWheelEvent, SharedString, StrikethroughStyle,
    Styled, StyledText, TextRun, UnderlineStyle, Window, canvas, div, font, prelude::*, px, rgb,
};
use k10s_theme::ShellTheme;

use crate::provider::{ExecEvent, ExecRequest, ExecSession, ReadProvider};
use crate::ui::{
    CONTENT_PADDING, PANEL_FOOTER_HEIGHT, PANEL_HEADER_HEIGHT, Viewport, panel_header,
};

/// How many lines of history the grid keeps above the visible screen. The
/// bound is the memory ceiling: a session that prints forever still occupies
/// a fixed number of cells.
pub const SCROLLBACK: usize = 5000;

pub struct TerminalState {
    term: Term<VoidListener>,
    parser: Processor,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalStyle {
    foreground: u32,
    background: u32,
    bold: bool,
    italic: bool,
    dim: bool,
    underline: bool,
    undercurl: bool,
    strikeout: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalRun {
    range: std::ops::Range<usize>,
    style: TerminalStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalLine {
    text: String,
    runs: Vec<TerminalRun>,
}

impl TerminalStyle {
    fn highlight(self, shell: &ShellTheme) -> HighlightStyle {
        let mut foreground = rgb(self.foreground);
        if self.dim {
            foreground.a *= 0.7;
        }
        HighlightStyle {
            color: Some(foreground.into()),
            background_color: (self.background != shell.terminal_background)
                .then(|| rgb(self.background).into()),
            font_weight: self.bold.then_some(FontWeight::BOLD),
            font_style: self.italic.then_some(FontStyle::Italic),
            underline: self.underline.then_some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(foreground.into()),
                wavy: self.undercurl,
            }),
            strikethrough: self.strikeout.then_some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(foreground.into()),
            }),
            fade_out: None,
        }
    }
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16) -> TerminalState {
        Self::with_history(cols, rows, SCROLLBACK)
    }

    fn with_history(cols: u16, rows: u16, scrolling_history: usize) -> TerminalState {
        let (cols, rows) = (cols.max(2), rows.max(2));
        let config = Config {
            scrolling_history,
            ..Config::default()
        };
        let size = TermSize::new(cols as usize, rows as usize);
        TerminalState {
            term: Term::new(config, &size, VoidListener),
            parser: Processor::new(),
            cols,
            rows,
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    // True when the size actually changed, so the caller knows to tell the
    // remote side exactly once.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let (cols, rows) = (cols.max(2), rows.max(2));
        if (cols, rows) == (self.cols, self.rows) {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.term
            .resize(TermSize::new(cols as usize, rows as usize));
        true
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    // The visible screen as text rows, trailing blanks trimmed. Used by tests
    // that only care that bytes became characters; the painter reads
    // [`TerminalState::styled_lines`], which carries each cell's fg/bg.
    pub fn lines(&self) -> Vec<String> {
        let grid = self.term.grid();
        (0..self.rows as usize)
            .map(|row| {
                let line = &grid[self.display_line(row)];
                let mut text: String = (0..self.cols as usize)
                    .map(|col| {
                        let c = line[Column(col)].c;
                        if c == '\0' { ' ' } else { c }
                    })
                    .collect();
                while text.ends_with(' ') {
                    text.pop();
                }
                text
            })
            .collect()
    }

    fn styled_lines(&self, shell: &ShellTheme, show_cursor: bool) -> Vec<TerminalLine> {
        let grid = self.term.grid();
        let cursor = point_to_viewport(grid.display_offset(), grid.cursor.point)
            .map(|point| (point.line, point.column.0));
        (0..self.rows as usize)
            .map(|row| {
                let line = &grid[self.display_line(row)];
                let end = (0..self.cols as usize)
                    .rfind(|column| {
                        let cell = &line[Column(*column)];
                        let style = terminal_style(cell, shell);
                        (!matches!(cell.c, '\0' | ' '))
                            || style.background != shell.terminal_background
                            || (show_cursor && cursor == Some((row, *column)))
                    })
                    .map_or(0, |column| column + 1);
                let mut text = String::with_capacity(end);
                let mut runs: Vec<TerminalRun> = Vec::new();
                for column in 0..end {
                    let cell = &line[Column(column)];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    let start = text.len();
                    text.push(if cell.c == '\0' { ' ' } else { cell.c });
                    if let Some(zerowidth) = cell.zerowidth() {
                        text.extend(zerowidth);
                    }
                    let mut style = terminal_style(cell, shell);
                    if show_cursor && cursor == Some((row, column)) {
                        style.foreground = shell.terminal_background;
                        style.background = shell.cursor;
                    }
                    let end = text.len();
                    match runs.last_mut() {
                        Some(run) if run.style == style => run.range.end = end,
                        _ => runs.push(TerminalRun {
                            range: start..end,
                            style,
                        }),
                    }
                }
                TerminalLine { text, runs }
            })
            .collect()
    }

    fn display_line(&self, row: usize) -> Line {
        Line(row as i32 - self.term.grid().display_offset() as i32)
    }

    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    // True when the viewport actually moved, so the view knows to repaint.
    pub fn scroll_delta(&mut self, lines: i32) -> bool {
        if lines == 0 {
            return false;
        }
        let before = self.display_offset();
        self.term.scroll_display(Scroll::Delta(lines));
        before != self.display_offset()
    }

    pub fn scroll_page(&mut self, toward_history: bool) -> bool {
        let before = self.display_offset();
        self.term.scroll_display(if toward_history {
            Scroll::PageUp
        } else {
            Scroll::PageDown
        });
        before != self.display_offset()
    }

    pub fn scroll_to_bottom(&mut self) -> bool {
        let before = self.display_offset();
        self.term.scroll_display(Scroll::Bottom);
        before != self.display_offset()
    }

    // (row, column) of the cursor on the visible screen.
    pub fn cursor(&self) -> (usize, usize) {
        let point = self.term.grid().cursor.point;
        match point_to_viewport(self.term.grid().display_offset(), point) {
            Some(point) => (point.line, point.column.0),
            None => (point.line.0.max(0) as usize, point.column.0),
        }
    }
}

fn terminal_style(cell: &Cell, shell: &ShellTheme) -> TerminalStyle {
    let mut foreground = terminal_color(cell.fg, shell);
    let mut background = terminal_color(cell.bg, shell);
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut foreground, &mut background);
    }
    if cell.flags.contains(Flags::HIDDEN) {
        foreground = background;
    }
    TerminalStyle {
        foreground,
        background,
        bold: cell.flags.contains(Flags::BOLD),
        italic: cell.flags.contains(Flags::ITALIC),
        dim: cell.flags.contains(Flags::DIM),
        underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
        undercurl: cell.flags.contains(Flags::UNDERCURL),
        strikeout: cell.flags.contains(Flags::STRIKEOUT),
    }
}

fn terminal_color(color: Color, shell: &ShellTheme) -> u32 {
    match color {
        Color::Spec(color) => packed_rgb(color),
        Color::Indexed(index) => indexed_color(index, shell),
        Color::Named(NamedColor::Foreground) => shell.terminal_foreground,
        Color::Named(NamedColor::BrightForeground) => shell.terminal_bright_foreground,
        Color::Named(NamedColor::DimForeground) => shell.terminal_dim_foreground,
        Color::Named(NamedColor::Background) => shell.terminal_background,
        Color::Named(NamedColor::Cursor) => shell.cursor,
        Color::Named(named) => {
            let index = named as usize;
            if index < shell.terminal_ansi.len() {
                shell.terminal_ansi[index]
            } else {
                let dim_start = NamedColor::DimBlack as usize;
                let dim_end = NamedColor::DimWhite as usize;
                if (dim_start..=dim_end).contains(&index) {
                    shell.terminal_ansi_dim[index - dim_start]
                } else {
                    shell.terminal_foreground
                }
            }
        }
    }
}

fn packed_rgb(color: Rgb) -> u32 {
    (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)
}

fn indexed_color(index: u8, shell: &ShellTheme) -> u32 {
    match index {
        0..=15 => shell.terminal_ansi[index as usize],
        16..=231 => {
            let index = index - 16;
            let channel = |value: u8| [0, 95, 135, 175, 215, 255][value as usize];
            packed_rgb(Rgb {
                r: channel(index / 36),
                g: channel((index / 6) % 6),
                b: channel(index % 6),
            })
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            packed_rgb(Rgb {
                r: value,
                g: value,
                b: value,
            })
        }
    }
}

// One keystroke as the bytes a terminal expects, or None for keys that have
// no encoding (bare modifiers, chords we do not map).
pub fn key_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = &keystroke.modifiers;
    let bytes: Vec<u8> = if modifiers.control {
        control_bytes(keystroke.key.as_str())?
    } else {
        match keystroke.key.as_str() {
            "enter" => b"\r".to_vec(),
            "tab" => b"\t".to_vec(),
            "backspace" => b"\x7f".to_vec(),
            "escape" => b"\x1b".to_vec(),
            "up" => b"\x1b[A".to_vec(),
            "down" => b"\x1b[B".to_vec(),
            "right" => b"\x1b[C".to_vec(),
            "left" => b"\x1b[D".to_vec(),
            "home" => b"\x1b[H".to_vec(),
            "end" => b"\x1b[F".to_vec(),
            "pageup" => b"\x1b[5~".to_vec(),
            "pagedown" => b"\x1b[6~".to_vec(),
            "delete" => b"\x1b[3~".to_vec(),
            "insert" => b"\x1b[2~".to_vec(),
            "f1" => b"\x1bOP".to_vec(),
            "f2" => b"\x1bOQ".to_vec(),
            "f3" => b"\x1bOR".to_vec(),
            "f4" => b"\x1bOS".to_vec(),
            "f5" => b"\x1b[15~".to_vec(),
            "f6" => b"\x1b[17~".to_vec(),
            "f7" => b"\x1b[18~".to_vec(),
            "f8" => b"\x1b[19~".to_vec(),
            "f9" => b"\x1b[20~".to_vec(),
            "f10" => b"\x1b[21~".to_vec(),
            "f11" => b"\x1b[23~".to_vec(),
            "f12" => b"\x1b[24~".to_vec(),
            _ => keystroke.key_char.as_deref()?.as_bytes().to_vec(),
        }
    };
    if modifiers.alt {
        // Meta sends ESC first, the way xterm does.
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        return Some(out);
    }
    Some(bytes)
}

fn control_bytes(key: &str) -> Option<Vec<u8>> {
    if key.len() == 1 {
        let c = key.as_bytes()[0].to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            return Some(vec![c - b'a' + 1]);
        }
    }
    match key {
        "space" | "@" => Some(vec![0x00]),
        "[" => Some(vec![0x1b]),
        "\\" => Some(vec![0x1c]),
        "]" => Some(vec![0x1d]),
        _ => None,
    }
}

// A stalled UI drops output on the floor rather than buffering the far side
// without bound; the terminal is a screen, not a log. How a session ended is
// not screen content, so those events go through a fresh sender, whose
// guaranteed slot no flood of output can take: a view that misses one shows a
// live cursor over a shell that is already gone.
fn exec_events(
    tx: futures::channel::mpsc::Sender<ExecEvent>,
) -> Box<dyn Fn(ExecEvent) + Send + Sync> {
    let tx = std::sync::Mutex::new(tx);
    Box::new(move |event| {
        let mut tx = tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = match event {
            ExecEvent::Output(_) => tx.try_send(event),
            ended => tx.clone().try_send(ended),
        };
    })
}

pub struct TerminalView {
    focus: FocusHandle,
    title: SharedString,
    state: TerminalState,
    session: Option<Box<dyn ExecSession>>,
    status: Option<String>,
    viewport: Viewport,
}

impl TerminalView {
    pub fn exec(
        provider: Rc<dyn ReadProvider>,
        request: ExecRequest,
        cx: &mut Context<Self>,
    ) -> TerminalView {
        let title = format!("exec {}", request.pod).into();
        Self::with_transport(
            title,
            move |on_event| provider.start_exec(&request, on_event),
            cx,
        )
    }

    /// Attach to the container's running process: same view as exec, stdin
    /// only, no TTY command. The flag rides on [`ExecRequest`] so the
    /// transport trait stays as it is.
    pub fn attach(
        provider: Rc<dyn ReadProvider>,
        request: ExecRequest,
        cx: &mut Context<Self>,
    ) -> TerminalView {
        let title = format!("attach {}", request.pod).into();
        Self::with_transport(
            title,
            move |on_event| provider.start_exec(&request, on_event),
            cx,
        )
    }

    /// The user's own shell in the same view: the transport is the only
    /// difference between a local terminal and a cluster exec.
    #[cfg(unix)]
    pub fn local(cx: &mut Context<Self>) -> TerminalView {
        Self::with_transport("terminal".into(), crate::pty::spawn_local_shell, cx)
    }

    #[cfg(unix)]
    pub fn command(
        title: String,
        program: String,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) -> TerminalView {
        Self::with_transport(
            title.into(),
            move |on_event| crate::pty::spawn_command(program, args, on_event),
            cx,
        )
    }

    // A PTY is the one transport this build does not open on Windows yet; the
    // view still exists so the terminal toggle answers with a labelled state
    // instead of a missing action.
    #[cfg(not(unix))]
    pub fn local(cx: &mut Context<Self>) -> TerminalView {
        Self::with_transport(
            "terminal".into(),
            |on_event| {
                on_event(ExecEvent::Failed(
                    "the local terminal needs a PTY, which this build does not open on this \
                     platform yet"
                        .to_string(),
                ));
                Box::new(crate::provider::NullExecSession)
            },
            cx,
        )
    }

    #[cfg(not(unix))]
    pub fn command(
        title: String,
        _: String,
        _: Vec<String>,
        cx: &mut Context<Self>,
    ) -> TerminalView {
        Self::with_transport(
            title.into(),
            |on_event| {
                on_event(ExecEvent::Failed(
                    "local machine commands are not available on this platform".to_string(),
                ));
                Box::new(crate::provider::NullExecSession)
            },
            cx,
        )
    }

    fn with_transport(
        title: SharedString,
        start: impl FnOnce(Box<dyn Fn(ExecEvent) + Send + Sync>) -> Box<dyn ExecSession>,
        cx: &mut Context<Self>,
    ) -> TerminalView {
        let mut view = TerminalView {
            focus: cx.focus_handle(),
            title,
            state: TerminalState::new(80, 24),
            session: None,
            status: Some("connecting...".to_string()),
            viewport: Viewport::default(),
        };
        let (tx, mut rx) = futures::channel::mpsc::channel::<ExecEvent>(256);
        view.session = Some(start(exec_events(tx)));
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(event) = rx.next().await {
                if this.update(cx, |this, cx| this.apply(event, cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
        view
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn apply(&mut self, event: ExecEvent, cx: &mut Context<Self>) {
        match event {
            ExecEvent::Output(bytes) => {
                self.status = None;
                self.state.advance(&bytes);
            }
            ExecEvent::Ended(why) => {
                self.status = Some(format!("session ended: {why}"));
                self.session = None;
            }
            ExecEvent::Denied(what) => {
                self.status = Some(format!("{what}: access denied for this account"));
                self.session = None;
            }
            ExecEvent::Failed(why) => {
                self.status = Some(why);
                self.session = None;
            }
        }
        cx.notify();
    }

    fn resize(&mut self, width: f32, height: f32, cell_width: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let rows = self.viewport.rows(
            PANEL_HEADER_HEIGHT + PANEL_FOOTER_HEIGHT,
            CONTENT_PADDING * 2.0,
            k10s_theme::typography(cx).line_height(),
            200,
        ) as u16;
        let cols = self
            .viewport
            .columns(CONTENT_PADDING * 2.0, cell_width.max(1.0), 400) as u16;
        if self.state.resize(cols.max(2), rows.max(2)) {
            if let Some(session) = &self.session {
                session.resize(cols.max(2), rows.max(2));
            }
            cx.notify();
        }
    }
}

impl crate::item::Item for TerminalView {
    fn title(&self) -> SharedString {
        TerminalView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        TerminalView::focus_handle(self)
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let mono = font(fonts.buffer_family.clone());
        let foreground = rgb(theme.shell.terminal_foreground).into();

        let show_cursor = self.session.is_some();
        let lines = self.state.styled_lines(&theme.shell, show_cursor);

        div()
            .id("terminal-view")
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(theme.shell.terminal_background))
            .font_family(fonts.buffer_family.clone())
            .text_color(rgb(theme.shell.terminal_foreground))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let line = window.text_system().shape_line(
                            "M".into(),
                            px(fonts.buffer_size),
                            &[TextRun {
                                len: 1,
                                font: mono.clone(),
                                color: foreground,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        let _ = view.update(cx, |this, cx| {
                            this.resize(
                                f32::from(bounds.size.width),
                                f32::from(bounds.size.height),
                                f32::from(line.width()),
                                cx,
                            );
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let keystroke = &event.keystroke;
                if keystroke.modifiers.shift
                    && !keystroke.modifiers.control
                    && !keystroke.modifiers.alt
                {
                    match keystroke.key.as_str() {
                        "pageup" => {
                            if this.state.scroll_page(true) {
                                cx.notify();
                            }
                            return;
                        }
                        "pagedown" => {
                            if this.state.scroll_page(false) {
                                cx.notify();
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                if let Some(session) = &this.session
                    && let Some(bytes) = key_bytes(keystroke)
                {
                    // Jump to the live screen so what is typed is what is
                    // seen; the echo still paints the next output, so a
                    // quiet session still paints nothing on the write.
                    this.state.scroll_to_bottom();
                    session.write(&bytes);
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let row = k10s_theme::typography(cx).line_height();
                let delta = f32::from(event.delta.pixel_delta(px(row)).y);
                let lines = -(delta / row).round() as i32;
                if this.state.scroll_delta(lines) {
                    cx.notify();
                }
            }))
            .child(panel_header(&theme, &fonts, self.title.clone()))
            .child(
                div()
                    .id("terminal-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .role(Role::Terminal)
                    .aria_label(self.title.clone())
                    .children(lines.into_iter().map(|line| {
                        let highlights = line
                            .runs
                            .into_iter()
                            .map(|run| (run.range, run.style.highlight(&theme.shell)));
                        div()
                            .h(px(fonts.line_height()))
                            .flex_none()
                            .overflow_hidden()
                            .text_size(px(fonts.buffer_size))
                            .text_color(rgb(theme.shell.terminal_foreground))
                            .whitespace_nowrap()
                            .child(StyledText::new(line.text).with_highlights(highlights))
                    })),
            )
            .child(
                div()
                    .h(px(PANEL_FOOTER_HEIGHT))
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
                    .child(match &self.status {
                        Some(status) => status.clone(),
                        None => format!(
                            "{}x{} · keys go to the session · ctrl-w close · ctrl-tab switch",
                            self.state.size().0,
                            self.state.size().1
                        ),
                    }),
            )
    }
}

#[cfg(test)]
#[path = "term_test.rs"]
mod tests;
