//! One overlay slot: the sheet on top, and the way back from it.
//!
//! The command palette, the cluster chooser, the path picker and the file finder
//! are the same object in four costumes -- at most one view, whatever
//! subscription keeps its events flowing, and where the keyboard was before it
//! opened. That third part is the one worth a type. It used to be three
//! `Option<FocusHandle>` fields for four consumers, with the picker and the
//! finder sharing one because they can never be up at the same time; a slot each
//! costs nothing and stops that from being a thing to know.
//!
//! Only the geometry is here. Which sheets exclude which, and what a dismissal
//! means, stays with the workspace, because those are policy and this is not.

use gpui::{App, Entity, FocusHandle, Render, Subscription, Window};

/// What the workspace needs from a view it shows as an overlay: where the
/// keyboard goes while the sheet is up.
pub trait Overlay: Render {
    fn focus_handle(&self) -> FocusHandle;
}

struct Shown<V: Overlay> {
    view: Entity<V>,
    subscription: Subscription,
    previous_focus: Option<FocusHandle>,
}

pub struct ModalSlot<V: Overlay> {
    shown: Option<Shown<V>>,
}

impl<V: Overlay> Default for ModalSlot<V> {
    fn default() -> Self {
        ModalSlot { shown: None }
    }
}

impl<V: Overlay + 'static> ModalSlot<V> {
    pub fn is_open(&self) -> bool {
        self.shown.is_some()
    }

    pub fn view(&self) -> Option<&Entity<V>> {
        self.shown.as_ref().map(|shown| &shown.view)
    }

    /// Show a view, remembering what had the keyboard so closing can hand it
    /// back. Replaces whatever was in the slot without restoring its focus --
    /// callers close the sheets they are displacing first, in the order they
    /// choose, because which sheet yields to which is their decision.
    pub fn open(
        &mut self,
        view: Entity<V>,
        subscription: Subscription,
        window: &mut Window,
        cx: &mut App,
    ) {
        let previous_focus = window.focused(cx);
        let focus = view.read(cx).focus_handle();
        self.shown = Some(Shown {
            view,
            subscription,
            previous_focus,
        });
        window.focus(&focus, cx);
    }

    /// Take the sheet down and give the keyboard back. Answers whether anything
    /// was open, so a caller only repaints when something actually happened.
    pub fn close(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let Some(Shown {
            view,
            subscription,
            previous_focus,
        }) = self.shown.take()
        else {
            return false;
        };
        // The view and its subscription go before the keyboard moves, which is
        // the order the four hand-written closers had: a sheet stops listening
        // as it leaves, not after whatever it hands focus to has it.
        drop((view, subscription));
        if let Some(previous) = previous_focus {
            window.focus(&previous, cx);
        }
        true
    }
}

impl Overlay for crate::palette::PaletteView {
    fn focus_handle(&self) -> FocusHandle {
        crate::palette::PaletteView::focus_handle(self)
    }
}

impl Overlay for crate::launch::LaunchView {
    fn focus_handle(&self) -> FocusHandle {
        crate::launch::LaunchView::focus_handle(self)
    }
}

impl Overlay for crate::finder::PathPickerView {
    fn focus_handle(&self) -> FocusHandle {
        crate::finder::PathPickerView::focus_handle(self)
    }
}

impl Overlay for crate::finder::FileFinderView {
    fn focus_handle(&self) -> FocusHandle {
        crate::finder::FileFinderView::focus_handle(self)
    }
}

impl Overlay for crate::finder::ClusterFinderView {
    fn focus_handle(&self) -> FocusHandle {
        crate::finder::ClusterFinderView::focus_handle(self)
    }
}
