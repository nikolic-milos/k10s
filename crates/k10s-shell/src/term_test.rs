//! The terminal over its transport seam: bytes become grid lines without ansi
//! corrupting them, cells keep Zed's palette backgrounds and attributes, the
//! grid wraps at its width and reports a resize exactly once, keystrokes
//! encode the way a terminal encodes them, and a view too slow to keep up
//! loses screen content but never learns of the end too late.

use super::*;
use gpui::rgb;
use std::sync::{Arc, Mutex};

// The fake transport's session half: records what the terminal writes.
#[derive(Clone, Default)]
struct FakeSession {
    written: Arc<Mutex<Vec<u8>>>,
    resized: Arc<Mutex<Vec<(u16, u16)>>>,
}

impl ExecSession for FakeSession {
    fn write(&self, bytes: &[u8]) {
        self.written.lock().unwrap().extend_from_slice(bytes);
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.resized.lock().unwrap().push((cols, rows));
    }
}

fn keystroke(key: &str) -> Keystroke {
    Keystroke {
        modifiers: Default::default(),
        key: key.to_string(),
        key_char: (key.chars().count() == 1).then(|| key.to_string()),
    }
}

fn ctrl(key: &str) -> Keystroke {
    Keystroke {
        modifiers: gpui::Modifiers {
            control: true,
            ..Default::default()
        },
        key: key.to_string(),
        key_char: None,
    }
}

#[test]
fn transport_bytes_become_grid_lines_and_ansi_does_not_corrupt_them() {
    let mut state = TerminalState::new(40, 10);
    state.advance(b"$ ls\r\ntotal 0\r\n");
    let lines = state.lines();
    assert_eq!(lines[0], "$ ls");
    assert_eq!(lines[1], "total 0");
    assert_eq!(state.cursor(), (2, 0), "the prompt line is next");

    state.advance(b"\x1b[2J\x1b[H\x1b[31mred\x1b[0m plain");
    let lines = state.lines();
    assert_eq!(
        lines[0], "red plain",
        "colors and clears parse without corrupting text"
    );
    assert_eq!(lines[1], "", "the clear cleared");
}

#[test]
fn ansi_cells_keep_zeds_palette_backgrounds_and_attributes() {
    let shell = &k10s_theme::ONE_DARK.shell;
    let mut state = TerminalState::new(40, 4);
    state.advance(b"\x1b[1;2;3;4;9;31;44mX\x1b[0m \x1b[38;5;120mY\x1b[0m");

    let lines = state.styled_lines(shell, false);
    assert_eq!(lines[0].text, "X Y");
    let styled = lines[0].runs[0].style;
    assert_eq!(styled.foreground, shell.terminal_ansi[1]);
    assert_eq!(styled.background, shell.terminal_ansi[4]);
    assert!(styled.bold && styled.italic && styled.dim);
    assert!(styled.underline && styled.strikeout);
    assert_eq!(
        lines[0].runs.last().unwrap().style.foreground,
        0x87ff87,
        "the xterm 6×6×6 cube is resolved exactly"
    );

    let cube = lines[0].runs.last().unwrap().style.highlight(shell);
    assert_eq!(
        cube.color,
        Some(rgb(0x87ff87).into()),
        "cell fg reaches the HighlightStyle StyledText paints"
    );
    let painted = lines[0].runs[0].style.highlight(shell);
    assert_eq!(
        painted.background_color,
        Some(rgb(shell.terminal_ansi[4]).into()),
        "cell bg reaches the HighlightStyle StyledText paints"
    );

    assert_eq!(
        terminal_color(Color::Named(NamedColor::DimRed), shell),
        shell.terminal_ansi_dim[1]
    );
    assert_eq!(indexed_color(255, shell), 0xeeeeee);
}

#[test]
fn the_grid_wraps_at_its_width_and_resize_reports_exactly_once() {
    let mut state = TerminalState::new(10, 4);
    state.advance(b"0123456789ABC");
    let lines = state.lines();
    assert_eq!(lines[0], "0123456789");
    assert_eq!(lines[1], "ABC", "overflow wraps to the next row");

    assert!(state.resize(20, 6));
    assert_eq!(state.size(), (20, 6));
    assert!(!state.resize(20, 6), "an unchanged size is not a resize");
    assert!(state.resize(1, 1), "degenerate sizes clamp");
    assert_eq!(state.size(), (2, 2));
}

#[test]
fn keystrokes_encode_like_a_terminal_and_reach_the_fake_transport() {
    let session = FakeSession::default();

    for (stroke, expected) in [
        (keystroke("a"), b"a".to_vec()),
        (keystroke("enter"), b"\r".to_vec()),
        (keystroke("backspace"), b"\x7f".to_vec()),
        (keystroke("escape"), b"\x1b".to_vec()),
        (keystroke("up"), b"\x1b[A".to_vec()),
        (keystroke("pagedown"), b"\x1b[6~".to_vec()),
        (ctrl("c"), vec![0x03]),
        (ctrl("d"), vec![0x04]),
    ] {
        let bytes = key_bytes(&stroke).expect("an encoding");
        session.write(&bytes);
        assert_eq!(
            bytes, expected,
            "{:?} must encode terminal-style",
            stroke.key
        );
    }
    assert_eq!(
        *session.written.lock().unwrap(),
        b"a\r\x7f\x1b\x1b[A\x1b[6~\x03\x04".to_vec(),
        "everything the keys encoded reached the transport"
    );

    let mut alt = keystroke("b");
    alt.modifiers.alt = true;
    assert_eq!(
        key_bytes(&alt),
        Some(b"\x1bb".to_vec()),
        "meta prefixes ESC"
    );
    assert_eq!(
        key_bytes(&ctrl("f5")),
        None,
        "an unmapped chord types nothing rather than something wrong"
    );

    session.resize(120, 40);
    assert_eq!(*session.resized.lock().unwrap(), vec![(120, 40)]);
}

#[test]
fn function_rows_and_meta_chords_encode_the_way_xterm_does() {
    assert_eq!(key_bytes(&keystroke("f1")), Some(b"\x1bOP".to_vec()));
    assert_eq!(key_bytes(&keystroke("f5")), Some(b"\x1b[15~".to_vec()));
    assert_eq!(key_bytes(&keystroke("f12")), Some(b"\x1b[24~".to_vec()));

    let mut chord = ctrl("c");
    chord.modifiers.alt = true;
    assert_eq!(
        key_bytes(&chord),
        Some(vec![0x1b, 0x03]),
        "meta prefixes ESC on a control chord too, not only on plain keys"
    );

    let mut arrow = keystroke("up");
    arrow.modifiers.alt = true;
    assert_eq!(key_bytes(&arrow), Some(b"\x1b\x1b[A".to_vec()));
}

#[test]
fn styled_lines_invert_hide_and_keep_wide_characters_whole() {
    let shell = &k10s_theme::ONE_DARK.shell;
    let mut state = TerminalState::new(20, 3);
    state.advance("\x1b[7mI\x1b[0m\x1b[8mH\x1b[0m\u{6f22}e\u{301}".as_bytes());

    let lines = state.styled_lines(shell, false);
    assert_eq!(
        lines[0].text, "IH\u{6f22}e\u{301}",
        "a wide char paints once and its spacer cell is not painted at all"
    );

    let inverse = lines[0].runs[0].style;
    assert_eq!(inverse.foreground, shell.terminal_background);
    assert_eq!(inverse.background, shell.terminal_foreground);

    let hidden = lines[0].runs[1].style;
    assert_eq!(
        hidden.foreground, hidden.background,
        "a hidden cell paints its character in its own background"
    );

    let showing = state.styled_lines(shell, true);
    assert_eq!(
        state.cursor(),
        (0, 5),
        "the wide char cost the cursor two cells"
    );
    let cursor = showing[0].runs.last().unwrap().style;
    assert_eq!(
        (cursor.foreground, cursor.background),
        (shell.terminal_background, shell.cursor),
        "the cursor cell paints past the text in the cursor colour"
    );
}

#[test]
fn a_flooded_view_drops_output_but_never_how_the_session_ended() {
    let (tx, mut rx) = futures::channel::mpsc::channel::<ExecEvent>(4);
    let on_event = exec_events(tx);
    for _ in 0..64 {
        on_event(ExecEvent::Output(b"x".to_vec()));
    }
    on_event(ExecEvent::Ended("the shell exited".to_string()));
    drop(on_event);

    let mut outputs = 0;
    let mut ended = None;
    while let Ok(event) = rx.try_recv() {
        match event {
            ExecEvent::Output(_) => outputs += 1,
            ExecEvent::Ended(why) => ended = Some(why),
            other => panic!("nothing else was sent: {other:?}"),
        }
    }
    assert!(
        outputs < 64,
        "a view that cannot keep up drops screen content instead of buffering the far side"
    );
    assert_eq!(
        ended.as_deref(),
        Some("the shell exited"),
        "how the session ended is not screen content: a flood must never eat it"
    );
}

#[test]
fn scrollback_is_bounded_history_the_viewport_can_walk() {
    let mut state = TerminalState::with_history(12, 4, 8);
    for i in 0..12 {
        state.advance(format!("line-{i:02}\r\n").as_bytes());
    }
    let live = state.lines();
    assert!(
        live.iter().any(|line| line.contains("line-11")),
        "the live screen holds the newest rows: {live:?}"
    );
    assert!(
        live.iter().all(|line| !line.contains("line-00")),
        "the first rows have left the screen for history: {live:?}"
    );
    assert!(state.history_size() > 0);
    assert!(
        state.history_size() <= 8,
        "history is capped at the config the Term was built with: {}",
        state.history_size()
    );

    assert!(state.scroll_delta(state.history_size() as i32));
    let top = state.lines();
    assert_ne!(top, live, "the viewport actually moved into history");
    assert!(
        top.iter().any(|line| line.contains("line-01")),
        "scrolling into history shows the lines that left the screen: {top:?}"
    );
    assert!(state.scroll_to_bottom());
    assert_eq!(state.display_offset(), 0, "the live screen is offset zero");

    let mut capped = TerminalState::with_history(8, 3, 5);
    for i in 0..40 {
        capped.advance(format!("{i}\r\n").as_bytes());
    }
    assert!(
        capped.history_size() <= 5,
        "a flood cannot grow past the cap: {}",
        capped.history_size()
    );
}
