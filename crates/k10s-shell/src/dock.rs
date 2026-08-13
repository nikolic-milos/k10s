//! Dock state: an openable side container of tabbed panels.
//!
//! A dock is layout policy, not rendering: which panels it holds, which one is
//! active, whether it is open. The tab arithmetic underneath is [`Pane`], shared
//! with the centre row; what a dock adds is the open flag and the two rules that
//! depend on it -- being handed work shows the dock, and running out of panels
//! shuts it. Generic over the panel type so every rule here is a unit test: the
//! workspace instantiates it with entity-backed panels and only does gpui on top.

use crate::pane::Pane;

#[derive(Debug)]
pub struct Dock<T> {
    open: bool,
    panels: Pane<T>,
}

impl<T> Default for Dock<T> {
    fn default() -> Self {
        Dock {
            open: false,
            panels: Pane::default(),
        }
    }
}

impl<T> Dock<T> {
    /// Open *and* holding something. A dock with nothing in it has nothing to
    /// show, whatever the flag says, so this is the question the chrome asks
    /// rather than the flag itself.
    pub fn is_open(&self) -> bool {
        self.open && !self.panels.is_empty()
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    pub fn len(&self) -> usize {
        self.panels.len()
    }

    pub fn panels(&self) -> impl Iterator<Item = (usize, &T)> {
        self.panels.iter()
    }

    /// The panel an index names, or nothing when it names none. What a caller
    /// that already has an index from [`Dock::find`] should ask, rather than
    /// counting that far into [`Dock::panels`] again.
    pub fn get(&self, index: usize) -> Option<&T> {
        self.panels.get(index)
    }

    pub fn active_index(&self) -> usize {
        self.panels.active_index()
    }

    pub fn active(&self) -> Option<&T> {
        self.panels.active()
    }

    // Adding a panel shows it: a dock that stays shut after being handed
    // work would read as a command that did nothing.
    pub fn push(&mut self, panel: T) -> usize {
        let index = self.panels.push(panel);
        self.open = true;
        index
    }

    pub fn activate(&mut self, index: usize) {
        if self.panels.activate(index) {
            self.open = true;
        }
    }

    pub fn find(&self, matches: impl Fn(&T) -> bool) -> Option<usize> {
        self.panels.find(matches)
    }

    /// Drop every panel a predicate rejects, keeping the selection on a
    /// survivor, and shut the dock if that emptied it. Answers how many went.
    ///
    /// Clearing the flag is belt-and-braces rather than the thing that hides an
    /// emptied dock: [`Dock::is_open`] already answers false while there is
    /// nothing to show, so no test can tell this line from its absence. What it
    /// buys is a flag that never claims a dock is open while it holds nothing,
    /// which is the state `toggle` would otherwise flip *away* from.
    pub fn retain(&mut self, keep: impl Fn(&T) -> bool) -> usize {
        let dropped = self.panels.retain(keep);
        if self.panels.is_empty() {
            self.open = false;
        }
        dropped
    }

    pub fn remove(&mut self, index: usize) -> Option<T> {
        let removed = self.panels.remove(index)?;
        if self.panels.is_empty() {
            self.open = false;
        }
        Some(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dock_opens_when_handed_work_and_shuts_when_emptied() {
        let mut dock: Dock<&str> = Dock::default();
        assert!(!dock.is_open());
        dock.toggle();
        assert!(!dock.is_open(), "an empty dock has nothing to show");

        dock.push("logs");
        assert!(dock.is_open());
        assert_eq!(dock.active(), Some(&"logs"));

        dock.toggle();
        assert!(!dock.is_open());
        dock.toggle();
        assert!(dock.is_open(), "toggling remembers the contents");

        dock.remove(0);
        assert!(!dock.is_open());
        assert!(dock.is_empty());
    }

    #[test]
    fn an_index_from_find_names_the_same_panel_get_answers_with() {
        let mut dock: Dock<&str> = Dock::default();
        dock.push("logs");
        dock.push("forwards");
        dock.push("terminal");

        let index = dock.find(|panel| *panel == "forwards").expect("the panel");
        assert_eq!(dock.get(index), Some(&"forwards"));
        assert_eq!(
            dock.get(index),
            dock.panels().nth(index).map(|(_, panel)| panel),
            "the two ways of resolving an index have to agree"
        );
        assert_eq!(
            dock.get(3),
            None,
            "an index that names no panel answers none"
        );
    }

    #[test]
    fn closing_the_active_panel_selects_the_neighbour_not_slot_zero() {
        let mut dock: Dock<&str> = Dock::default();
        dock.push("a");
        dock.push("b");
        dock.push("c");
        assert_eq!(dock.active(), Some(&"c"));

        dock.remove(2);
        assert_eq!(dock.active(), Some(&"b"));

        dock.push("c2");
        dock.activate(0);
        dock.remove(2);
        assert_eq!(
            dock.active(),
            Some(&"a"),
            "removing behind the active panel must not move the selection"
        );

        assert!(dock.remove(9).is_none());
    }

    #[test]
    fn retaining_keeps_the_selection_on_a_survivor_and_shuts_an_emptied_dock() {
        let mut dock: Dock<&str> = Dock::default();
        for panel in ["keep-a", "drop-b", "keep-c", "drop-d"] {
            dock.push(panel);
        }
        dock.activate(2);
        assert_eq!(dock.retain(|panel| panel.starts_with("keep")), 2);
        assert_eq!(
            dock.active(),
            Some(&"keep-c"),
            "a survivor that was selected stays selected under its new index"
        );

        dock.push("drop-e");
        assert_eq!(dock.active(), Some(&"drop-e"));
        dock.retain(|panel| panel.starts_with("keep"));
        assert_eq!(
            dock.active(),
            Some(&"keep-c"),
            "when the selected panel goes, its neighbour takes the selection"
        );

        assert_eq!(dock.retain(|_| false), 2);
        assert!(
            !dock.is_open(),
            "a dock emptied by a retain shuts like one emptied by a remove"
        );
        assert_eq!(dock.active_index(), 0);
    }

    #[test]
    fn activation_is_clamped_and_opens_the_dock() {
        let mut dock: Dock<&str> = Dock::default();
        dock.push("a");
        dock.push("b");
        dock.set_open(false);
        dock.activate(9);
        assert!(!dock.is_open(), "an out-of-range activation is a no-op");
        dock.activate(0);
        assert!(dock.is_open());
        assert_eq!(dock.active(), Some(&"a"));
        assert_eq!(dock.find(|p| *p == "b"), Some(1));
        assert_eq!(dock.len(), 2);
    }

    #[test]
    fn a_removal_that_names_no_panel_leaves_the_dock_alone() {
        // `remove` shuts an emptied dock, so the early return for a bad index
        // has to happen before that rule and not after it.
        let mut dock: Dock<&str> = Dock::default();
        dock.push("only");
        assert!(dock.remove(4).is_none());
        assert!(dock.is_open(), "a no-op removal must not shut the dock");
        assert_eq!(dock.len(), 1);
    }
}
