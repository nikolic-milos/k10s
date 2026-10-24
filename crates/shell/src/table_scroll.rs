//! One horizontal viewport for a table's header and virtualized rows.
//!
//! GPUI owns the offset and content extent. Vertical wheel input stays with
//! the table's existing selection machine; only the laid-out rows are held.

use gpui::{
    Context, Div, IntoElement, ParentElement, Pixels, Role, ScrollHandle, Styled, div, point,
    prelude::*, px, rgb,
};
use k10s_theme::{Theme, Typography};

#[derive(Clone, Default)]
pub(crate) struct TableScroll {
    handle: ScrollHandle,
}

pub(crate) fn table_key_context(input: bool, releases: bool) -> &'static str {
    match (input, releases) {
        (true, _) => "Typing",
        (false, true) => "Browse Releases Table",
        (false, false) => "Browse Table",
    }
}

impl TableScroll {
    pub fn left(&self) -> bool {
        self.shift(self.handle.bounds().size.width / 2.0)
    }

    pub fn right(&self) -> bool {
        self.shift(-self.handle.bounds().size.width / 2.0)
    }

    fn shift(&self, delta: Pixels) -> bool {
        let before = self.handle.offset();
        let x = (before.x + delta).clamp(-self.handle.max_offset().x, px(0.0));
        if x == before.x {
            return false;
        }
        self.handle.set_offset(point(x, px(0.0)));
        true
    }

    pub fn render<T: 'static>(
        &self,
        header: impl IntoElement,
        rows: impl IntoElement,
        theme: &Theme,
        fonts: &Typography,
        cx: &mut Context<T>,
    ) -> Div {
        let mut viewport = div()
            .id("table-columns")
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .overflow_x_scroll()
            .track_scroll(&self.handle)
            .child(
                div()
                    .flex_none()
                    .min_w_full()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(rows),
            );
        // GPUI otherwise turns an ordinary vertical wheel into horizontal motion.
        viewport.style().restrict_scroll_to_axis = Some(true);

        let left = self.clone();
        let right = self.clone();
        let button = |id: &'static str, text: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(6.0))
                .py(px(2.0))
                .cursor_pointer()
                .role(Role::Button)
                .aria_label(label)
                .text_color(rgb(theme.shell.text))
                .hover(|style| style.bg(rgb(theme.shell.element_hover)))
                .child(text)
        };

        div()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(viewport)
            .child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(3.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child("Columns:")
                    .child(
                        button("table-columns-left", "Left", "Scroll columns left").on_click(
                            cx.listener(move |_, _, _, cx| {
                                if left.left() {
                                    cx.notify();
                                }
                            }),
                        ),
                    )
                    .child(
                        button("table-columns-right", "Right", "Scroll columns right").on_click(
                            cx.listener(move |_, _, _, cx| {
                                if right.right() {
                                    cx.notify();
                                }
                            }),
                        ),
                    )
                    .child("Left/Right keys"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_keys_belong_to_tables_and_leave_input_and_other_views_alone() {
        let keymap = gpui::Keymap::new(crate::keybindings());
        let bindings_for = |key, context: &str| {
            keymap
                .bindings_for_input(
                    &[gpui::Keystroke::parse(key).expect("valid shortcut")],
                    &[
                        gpui::KeyContext::parse("Workspace").expect("valid context"),
                        gpui::KeyContext::parse(context).expect("valid context"),
                    ],
                )
                .0
        };
        for (key, command) in [
            ("left", "k10s_shell::TableScrollLeft"),
            ("right", "k10s_shell::TableScrollRight"),
        ] {
            for releases in [false, true] {
                assert_eq!(
                    bindings_for(key, table_key_context(false, releases))
                        .first()
                        .map(|binding| binding.action().name()),
                    Some(command)
                );
                assert!(
                    bindings_for(key, table_key_context(true, releases))
                        .iter()
                        .all(|binding| binding.action().name() != command)
                );
            }
            for context in [
                "Browse",
                "Observe",
                "Editor",
                "Terminal",
                "Ecosystem",
                "Map",
            ] {
                assert!(
                    bindings_for(key, context)
                        .iter()
                        .all(|binding| binding.action().name() != command)
                );
            }
        }
        for releases in [false, true] {
            assert_eq!(
                bindings_for("down", table_key_context(false, releases))
                    .first()
                    .map(|binding| binding.action().name()),
                Some("k10s_shell::RowDown")
            );
            assert_eq!(
                bindings_for("backspace", table_key_context(true, releases))
                    .first()
                    .map(|binding| binding.action().name()),
                Some("k10s_shell::DeleteInputChar")
            );
        }
        for (key, command) in [
            ("left", "k10s_shell::EditorLeft"),
            ("right", "k10s_shell::EditorRight"),
        ] {
            assert_eq!(
                bindings_for(key, "Editor")
                    .first()
                    .map(|binding| binding.action().name()),
                Some(command)
            );
        }
        assert_eq!(
            bindings_for("v", table_key_context(false, true))
                .first()
                .map(|binding| binding.action().name()),
            Some("k10s_shell::RevealHelm")
        );
    }
}
