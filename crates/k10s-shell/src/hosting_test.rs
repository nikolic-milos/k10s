//! The two keystroke policies of the item host, as values. Everything else in
//! hosting.rs needs a live window; these two rules are the ones a comment used
//! to have to explain because no test could.

use super::{CloseTarget, TerminalToggle, close_target, terminal_toggle};

#[test]
fn ctrl_w_prefers_the_bottom_then_the_left_then_the_centre() {
    assert_eq!(
        close_target(true, true, 1, 3, false, true),
        CloseTarget::Bottom
    );
    assert_eq!(
        close_target(false, true, 1, 3, false, true),
        CloseTarget::Left
    );
    assert_eq!(
        close_target(false, false, 1, 3, false, true),
        CloseTarget::Center(1)
    );
}

#[test]
fn the_map_never_closes_and_neither_does_the_last_tab() {
    assert_eq!(
        close_target(false, false, 0, 3, false, true),
        CloseTarget::Nothing,
        "index zero is the map"
    );
    assert_eq!(
        close_target(false, false, 0, 1, false, true),
        CloseTarget::Nothing,
        "a row of one is a row that stays"
    );
}

#[test]
fn dirty_and_unfocused_warns_instead_of_closing() {
    assert_eq!(
        close_target(false, false, 2, 3, true, false),
        CloseTarget::WarnDirty,
        "a keystroke aimed somewhere else never discards unsaved work"
    );
    assert_eq!(
        close_target(false, false, 2, 3, true, true),
        CloseTarget::Center(1),
        "the item's own guard handles a focused dirty close"
    );
}

#[test]
fn a_closed_centre_tab_lands_on_its_right_neighbour_clamped() {
    // The tab that slid into the closed slot is the one to the right, so the
    // index stays -- deliberately the opposite of Pane::remove's left
    // neighbour, which the docks use.
    assert_eq!(
        close_target(false, false, 1, 4, false, true),
        CloseTarget::Center(1)
    );
    assert_eq!(
        close_target(false, false, 3, 4, false, true),
        CloseTarget::Center(2),
        "closing the last tab clamps to the new end of the row"
    );
}

#[test]
fn hiding_the_terminal_demands_open_and_active_and_focused() {
    assert_eq!(terminal_toggle(None, true, 0, true), TerminalToggle::Spawn);
    assert_eq!(
        terminal_toggle(Some(2), true, 2, true),
        TerminalToggle::Hide
    );
    for (open, active, focus) in [
        (false, 2, true),
        (true, 1, true),
        (true, 2, false),
        (false, 1, false),
    ] {
        assert_eq!(
            terminal_toggle(Some(2), open, active, focus),
            TerminalToggle::Show(2),
            "open={open} active={active} focus={focus}: anything less than all \
             three brings the terminal forward rather than dismissing it"
        );
    }
}
