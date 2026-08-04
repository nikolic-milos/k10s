//! Dock state: an openable side container of tabbed panels.
//!
//! A dock is layout policy, not rendering: which panels it holds, which one
//! is active, whether it is open. Opening a panel opens the dock; closing the
//! active panel keeps the neighbour selected instead of snapping to zero;
//! toggling remembers its contents. Generic over the panel type so every rule
//! here is a unit test -- the workspace instantiates it with entity-backed
//! panels and only does gpui on top.

#[derive(Debug)]
pub struct Dock<T> {
    open: bool,
    active: usize,
    panels: Vec<T>,
}

impl<T> Default for Dock<T> {
    fn default() -> Self {
        Dock {
            open: false,
            active: 0,
            panels: Vec::new(),
        }
    }
}

impl<T> Dock<T> {
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
        self.panels.iter().enumerate()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&T> {
        self.panels.get(self.active)
    }

    // Adding a panel shows it: a dock that stays shut after being handed
    // work would read as a command that did nothing.
    pub fn push(&mut self, panel: T) -> usize {
        self.panels.push(panel);
        self.active = self.panels.len() - 1;
        self.open = true;
        self.active
    }

    pub fn activate(&mut self, index: usize) {
        if index < self.panels.len() {
            self.active = index;
            self.open = true;
        }
    }

    pub fn find(&self, matches: impl Fn(&T) -> bool) -> Option<usize> {
        self.panels.iter().position(matches)
    }

    /// Drop every panel a predicate rejects and return how many went, keeping
    /// the selection on a panel that survived. One act rather than a sequence of
    /// removals whose indices shift under each other -- which is what a whole
    /// class of panel ceasing to be true is: a cluster leaving takes its tables
    /// with it, all at once.
    pub fn retain(&mut self, keep: impl Fn(&T) -> bool) -> usize {
        let before = self.panels.len();
        let active_survives = self.active().is_some_and(&keep);
        // Where the active panel lands: however many survivors sit in front of
        // it. When it is one of the casualties, that index is the neighbour
        // that took its place, which is the same rule `remove` follows.
        let ahead = self.panels[..self.active.min(before)]
            .iter()
            .filter(|panel| keep(panel))
            .count();
        self.panels.retain(&keep);
        self.active = if active_survives {
            ahead
        } else {
            ahead.min(self.panels.len().saturating_sub(1))
        };
        if self.panels.is_empty() {
            self.active = 0;
            self.open = false;
        }
        before - self.panels.len()
    }

    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.panels.len() {
            return None;
        }
        let removed = self.panels.remove(index);
        if self.active >= index && self.active > 0 {
            self.active -= 1;
        }
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
}
