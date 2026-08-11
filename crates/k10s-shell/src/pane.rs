//! A row of tabs with one of them selected.
//!
//! What stays selected when a tab is added, removed, or a whole class of them
//! stops being true at once is pure index arithmetic, and it is the same
//! arithmetic wherever tabs appear. Keeping it here, generic over what a tab
//! holds, makes every rule a unit test rather than something to get right once
//! per pane: the centre row used to inline its own copy of all of it -- the
//! wraparound, the clamp after a close, the selection repair after a cluster
//! left -- and that copy was the one nothing tested. [`crate::dock::Dock`] is
//! this plus an open flag.

#[derive(Debug)]
pub struct Pane<T> {
    active: usize,
    items: Vec<T>,
}

impl<T> Default for Pane<T> {
    fn default() -> Self {
        Pane {
            active: 0,
            items: Vec::new(),
        }
    }
}

impl<T> Pane<T> {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&T> {
        self.items.get(self.active)
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.items.get(index)
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, &T)> {
        self.items.iter().enumerate()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.items.iter_mut()
    }

    pub fn find(&self, matches: impl Fn(&T) -> bool) -> Option<usize> {
        self.items.iter().position(matches)
    }

    /// Add a tab and select it. Opening something the user asked for and
    /// leaving it unselected would read as a command that did nothing.
    pub fn push(&mut self, item: T) -> usize {
        self.items.push(item);
        self.active = self.items.len() - 1;
        self.active
    }

    /// Select a tab, ignoring an index that names none. Answers whether the
    /// selection landed, which is what a dock needs in order to decide whether
    /// it should also open.
    pub fn activate(&mut self, index: usize) -> bool {
        if index >= self.items.len() {
            return false;
        }
        self.active = index;
        true
    }

    /// The tab a forward cycle reaches, wrapping at the end. `None` when there
    /// are no tabs: an empty pane has nowhere to cycle to, and the caller that
    /// used to compute this inline divided by the length to find out.
    pub fn next_index(&self) -> Option<usize> {
        (!self.items.is_empty()).then(|| (self.active + 1) % self.items.len())
    }

    /// The tab a backward cycle reaches, wrapping at the front.
    pub fn previous_index(&self) -> Option<usize> {
        (!self.items.is_empty()).then(|| (self.active + self.items.len() - 1) % self.items.len())
    }

    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);
        // Removing in front of the selection shifts it left by one so the same
        // tab stays selected; removing the selected tab itself leaves the index
        // pointing at whichever neighbour slid into its place.
        if self.active >= index && self.active > 0 {
            self.active -= 1;
        }
        Some(removed)
    }

    /// Drop every tab a predicate rejects and answer how many went, keeping the
    /// selection on a survivor. One act rather than a sequence of removals whose
    /// indices shift under each other -- which is what a whole class of tab
    /// ceasing to be true is: a cluster leaving takes its tables with it, all at
    /// once.
    pub fn retain(&mut self, keep: impl Fn(&T) -> bool) -> usize {
        let before = self.items.len();
        let active_survives = self.active().is_some_and(&keep);
        // Where the selection lands: however many survivors sit in front of it.
        // When it is one of the casualties, that index is the neighbour that
        // took its place, which is the rule `remove` follows too.
        let ahead = self.items[..self.active.min(before)]
            .iter()
            .filter(|item| keep(item))
            .count();
        self.items.retain(&keep);
        self.active = if active_survives {
            ahead
        } else {
            ahead.min(self.items.len().saturating_sub(1))
        };
        if self.items.is_empty() {
            self.active = 0;
        }
        before - self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_of(items: &[&'static str]) -> Pane<&'static str> {
        let mut pane = Pane::default();
        for item in items {
            pane.push(*item);
        }
        pane
    }

    #[test]
    fn a_new_tab_is_the_selected_one_and_an_unknown_index_selects_nothing() {
        let mut pane: Pane<&str> = Pane::default();
        assert!(pane.is_empty());
        assert_eq!(pane.active(), None);
        assert!(!pane.activate(0), "an empty pane has no tab zero to select");

        assert_eq!(pane.push("map"), 0);
        assert_eq!(pane.push("doc"), 1);
        assert_eq!(pane.active(), Some(&"doc"));
        assert_eq!(pane.len(), 2);

        assert!(!pane.activate(9), "an out-of-range activation is a no-op");
        assert_eq!(pane.active(), Some(&"doc"));
        assert!(pane.activate(0));
        assert_eq!(pane.active(), Some(&"map"));
        assert_eq!(pane.find(|item| *item == "doc"), Some(1));
        assert_eq!(pane.get(1), Some(&"doc"));
        assert_eq!(pane.get(9), None);
    }

    #[test]
    fn cycling_wraps_at_both_ends_and_answers_nothing_when_there_is_nothing() {
        let empty: Pane<&str> = Pane::default();
        assert_eq!(
            empty.next_index(),
            None,
            "cycling an empty pane must not compute an index out of a length of zero"
        );
        assert_eq!(empty.previous_index(), None);

        let mut pane = pane_of(&["a", "b", "c"]);
        assert_eq!(pane.active_index(), 2);
        assert_eq!(pane.next_index(), Some(0), "the end wraps to the front");
        assert_eq!(pane.previous_index(), Some(1));

        pane.activate(0);
        assert_eq!(pane.previous_index(), Some(2), "the front wraps to the end");
        assert_eq!(pane.next_index(), Some(1));

        let single = pane_of(&["only"]);
        assert_eq!(single.next_index(), Some(0));
        assert_eq!(single.previous_index(), Some(0));
    }

    #[test]
    fn removing_keeps_the_same_tab_selected_unless_it_is_the_one_that_went() {
        let mut pane = pane_of(&["a", "b", "c"]);
        assert_eq!(pane.remove(2), Some("c"));
        assert_eq!(
            pane.active(),
            Some(&"b"),
            "closing the selected tab lands on its neighbour, not on the front"
        );

        pane.push("c2");
        pane.activate(2);
        assert_eq!(pane.remove(0), Some("a"));
        assert_eq!(
            pane.active(),
            Some(&"c2"),
            "removing in front of the selection must not move it to another tab"
        );

        pane.activate(0);
        pane.remove(1);
        assert_eq!(
            pane.active(),
            Some(&"b"),
            "removing behind the selection leaves it exactly where it was"
        );

        assert_eq!(pane.remove(9), None);
        assert_eq!(pane.len(), 1);
    }

    #[test]
    fn removing_behind_a_selection_that_is_not_the_first_tab_does_not_move_it() {
        // The case the shift rule is guarded for. With the selection at zero,
        // "do not shift" and "cannot shift" look the same, so an index behind a
        // selection further along is the only shape that tells them apart.
        let mut pane = pane_of(&["a", "b", "c", "d"]);
        pane.activate(1);
        pane.remove(3);
        assert_eq!(pane.active(), Some(&"b"));
        assert_eq!(pane.active_index(), 1);

        pane.remove(2);
        assert_eq!(
            pane.active(),
            Some(&"b"),
            "closing a tab to the right of the selected one must not drag the \
             selection left with it"
        );
    }

    #[test]
    fn removing_the_last_tab_leaves_an_empty_pane_pointing_at_nothing() {
        let mut pane = pane_of(&["only"]);
        assert_eq!(pane.remove(0), Some("only"));
        assert!(pane.is_empty());
        assert_eq!(pane.active(), None);
        assert_eq!(
            pane.active_index(),
            0,
            "the index of an empty pane names no tab, and `active` is what says so"
        );
    }

    #[test]
    fn retaining_keeps_the_selection_on_a_survivor() {
        let mut pane = pane_of(&["keep-a", "drop-b", "keep-c", "drop-d"]);
        pane.activate(2);
        assert_eq!(pane.retain(|item| item.starts_with("keep")), 2);
        assert_eq!(
            pane.active(),
            Some(&"keep-c"),
            "a survivor that was selected stays selected under its new index"
        );

        pane.push("drop-e");
        assert_eq!(pane.active(), Some(&"drop-e"));
        pane.retain(|item| item.starts_with("keep"));
        assert_eq!(
            pane.active(),
            Some(&"keep-c"),
            "when the selected tab goes, its neighbour takes the selection"
        );

        assert_eq!(pane.retain(|_| false), 2);
        assert!(pane.is_empty());
        assert_eq!(pane.active_index(), 0);
    }

    #[test]
    fn retaining_nothing_away_is_not_a_selection_change() {
        let mut pane = pane_of(&["a", "b", "c"]);
        pane.activate(1);
        assert_eq!(pane.retain(|_| true), 0);
        assert_eq!(pane.active(), Some(&"b"));
        assert_eq!(pane.len(), 3);
    }

    #[test]
    fn retaining_only_tabs_behind_the_selection_lands_on_the_last_survivor() {
        // The selected tab and everything after it goes, so there is no
        // neighbour to inherit: the clamp is what stops the index dangling past
        // the end of what is left.
        let mut pane = pane_of(&["keep-a", "keep-b", "drop-c", "drop-d"]);
        pane.activate(3);
        assert_eq!(pane.retain(|item| item.starts_with("keep")), 2);
        assert_eq!(pane.active(), Some(&"keep-b"));
        assert_eq!(pane.active_index(), 1);
    }

    #[test]
    fn a_pane_hands_out_its_tabs_with_their_indices() {
        let mut pane = pane_of(&["a", "b"]);
        assert_eq!(
            pane.iter().collect::<Vec<_>>(),
            vec![(0, &"a"), (1, &"b")],
            "the tab strip paints by index, so the index travels with the tab"
        );
        for item in pane.iter_mut() {
            *item = "z";
        }
        assert_eq!(pane.get(0), Some(&"z"));
    }
}
