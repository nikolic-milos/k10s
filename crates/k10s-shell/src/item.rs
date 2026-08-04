//! The workspace item seam: what the shell needs from anything it hosts.
//!
//! Mirrors Zed's `workspace::Item`/`ItemHandle` pair. A view earns a place in
//! the center row or a dock by implementing [`Item`] -- a title and a focus
//! handle -- and the workspace holds it as a boxed [`ItemHandle`], so adding
//! a new panel kind (a Grafana board, a Loki query, a YAML editor) never
//! touches workspace internals. The blanket impl erases `Entity<T>` behind
//! the trait; `to_any` hands back the renderable view, and a caller that
//! needs the concrete type gets it by downcasting, exactly as Zed does.

use gpui::{AnyView, App, Entity, EntityId, FocusHandle, Render, SharedString};

/// What a hosted view must answer for the workspace chrome: a tab title and
/// where focus lands when the tab activates. Implementing this is the entire
/// cost of docking a new view.
pub trait Item: Render {
    fn title(&self) -> SharedString;
    fn focus_handle(&self) -> FocusHandle;

    /// Whether this item holds work that closing would throw away. The
    /// workspace asks before it removes a tab, so an item that can lose
    /// something is never closed by a keystroke aimed somewhere else.
    fn is_dirty(&self) -> bool {
        false
    }
}

/// The type-erased half: the workspace stores every hosted view as one of
/// these, whatever its concrete type.
pub trait ItemHandle {
    fn title(&self, cx: &App) -> SharedString;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn is_dirty(&self, cx: &App) -> bool;
    fn item_id(&self) -> EntityId;
    fn to_any(&self) -> AnyView;
}

impl<T: Item> ItemHandle for Entity<T> {
    fn title(&self, cx: &App) -> SharedString {
        self.read(cx).title()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty()
    }

    fn item_id(&self) -> EntityId {
        self.entity_id()
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }
}
