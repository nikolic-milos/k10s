//! The terminal over its transport seam: bytes become grid lines without ansi
//! corrupting them, cells keep Zed's palette backgrounds and attributes, the
//! grid wraps at its width and reports a resize exactly once, and keystrokes
//! encode the way a terminal encodes them.

use super::*;
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
