//! The embedded terminal: an exec session rendered as a cell grid.
//!
//! The VT machinery is `alacritty_terminal` -- the ROADMAP forbids writing a
//! parser -- fed raw bytes from an [`ExecSession`] behind the provider seam.
//! All terminal logic lives in the pure [`TerminalState`] (grid from bytes,
//! resize, cursor) and the pure [`key_bytes`] input encoding, both tested
//! with no window and no transport; the gpui view is a thin shell that
//! paints the visible grid as monospace rows and forwards keystrokes. The
//! grid keeps no scrollback: what the screen holds is what exists, which is
//! also its memory bound. The `Terminal` key context captures everything
//! except the item-management chords (see `keybindings()`), so plain letters
//! and escape reach the remote shell instead of dispatching commands.

use std::rc::Rc;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

use gpui::{
    Context, FocusHandle, FontStyle, FontWeight, HighlightStyle, IntoElement, KeyDownEvent,
    Keystroke, ParentElement, Render, Role, SharedString, StrikethroughStyle, Styled, StyledText,
    TextRun, UnderlineStyle, Window, canvas, div, font, prelude::*, px, rgb,
};
use k10s_theme::ShellTheme;

use crate::provider::{ExecEvent, ExecRequest, ExecSession, ReadProvider};
use crate::ui::{
    CONTENT_PADDING, PANEL_FOOTER_HEIGHT, PANEL_HEADER_HEIGHT, Viewport, panel_header,
};

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
        let (cols, rows) = (cols.max(2), rows.max(2));
        let config = Config {
            // No scrollback: the visible screen is the whole buffer, and
            // the whole memory bound.
            scrolling_history: 0,
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

    // The visible screen as text rows, trailing blanks trimmed. Colors and
    // attributes are parsed (they must not corrupt the text) but not yet
    // carried to the painter; that is stated in the ROADMAP, not hidden.
    pub fn lines(&self) -> Vec<String> {
        let grid = self.term.grid();
        (0..self.rows as usize)
            .map(|row| {
                let line = &grid[Line(row as i32)];
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
        let cursor = self.cursor();
        (0..self.rows as usize)
            .map(|row| {
                let line = &grid[Line(row as i32)];
                let end = (0..self.cols as usize)
                    .rfind(|column| {
                        let cell = &line[Column(*column)];
                        let style = terminal_style(cell, shell);
                        (!matches!(cell.c, '\0' | ' '))
                            || style.background != shell.terminal_background
                            || (show_cursor && cursor == (row, *column))
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
                    if show_cursor && cursor == (row, column) {
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

    // (row, column) of the cursor on the visible screen.
    pub fn cursor(&self) -> (usize, usize) {
        let point = self.term.grid().cursor.point;
        (point.line.0.max(0) as usize, point.column.0)
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
// no encoding (bare modifiers, function rows we do not map).
pub fn key_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = &keystroke.modifiers;
    if modifiers.control {
        let key = keystroke.key.as_str();
        if key.len() == 1 {
            let c = key.as_bytes()[0].to_ascii_lowercase();
            if c.is_ascii_lowercase() {
                return Some(vec![c - b'a' + 1]);
            }
        }
        return match key {
            "space" | "@" => Some(vec![0x00]),
            "[" => Some(vec![0x1b]),
            "\\" => Some(vec![0x1c]),
            "]" => Some(vec![0x1d]),
            _ => None,
        };
    }
    let bytes: Vec<u8> = match keystroke.key.as_str() {
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
        _ => keystroke.key_char.as_deref()?.as_bytes().to_vec(),
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

    /// The user's own shell in the same view: the transport is the only
    /// difference between a local terminal and a cluster exec.
    #[cfg(unix)]
    pub fn local(cx: &mut Context<Self>) -> TerminalView {
        Self::with_transport("terminal".into(), crate::pty::spawn_local_shell, cx)
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
        // A stalled UI drops output on the floor rather than buffering the
        // far side without bound; the terminal is a screen, not a log.
        view.session = Some(start(Box::new(move |event| {
            let mut tx = tx.clone();
            let _ = tx.try_send(event);
        })));
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| {
                if let Some(session) = &this.session
                    && let Some(bytes) = key_bytes(&event.keystroke)
                {
                    // No notify: the echo comes back as output and paints
                    // then, so a quiet session paints nothing.
                    session.write(&bytes);
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
