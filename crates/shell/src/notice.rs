//! A bounded reader for status and catalogue notices.

use std::rc::Rc;

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, MouseButton, ParentElement, Render, Role,
    ScrollHandle, SharedString, Styled, Window, div, point, prelude::*, px, rgb,
};

use crate::ui::{MODAL_MAX_HEIGHT, MODAL_WIDTH};
use crate::{
    CancelInput, DocPageDown, DocPageUp, DocScrollDown, DocScrollUp, NoticeFocusNext,
    NoticeFocusPrevious, RowDown, RowUp,
};

// A server sentence may be much longer than the window. Shape only one bounded
// part at a time while retaining the original bytes for the next part.
const PART_CHARS: usize = 256;

pub(crate) trait NoticeSource {
    fn notice_count(&self) -> usize;
    fn notice_at(&self, index: usize) -> Option<(&str, &str)>;
}

impl NoticeSource for Vec<String> {
    fn notice_count(&self) -> usize {
        Vec::len(self)
    }

    fn notice_at(&self, index: usize) -> Option<(&str, &str)> {
        self.as_slice().get(index).map(|text| ("", text.as_str()))
    }
}

/// Immutable notice text retained independently of its original view.
#[derive(Clone)]
pub struct NoticeSnapshot(Rc<dyn NoticeSource>);

impl NoticeSnapshot {
    pub(crate) fn new(source: impl NoticeSource + 'static) -> Self {
        Self(Rc::new(source))
    }

    #[cfg(test)]
    pub(crate) fn shared(source: Rc<impl NoticeSource + 'static>) -> Self {
        Self(source)
    }

    /// The number of notices in this snapshot.
    pub fn len(&self) -> usize {
        self.0.notice_count()
    }

    /// Whether this snapshot has no notices.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The heading and full text of one notice, if present.
    pub fn get(&self, index: usize) -> Option<(&str, &str)> {
        self.0.notice_at(index)
    }
}

struct Cursor {
    notices: NoticeSnapshot,
    index: usize,
    start: usize,
    end: usize,
    part: usize,
    // One checkpoint per crossed notice, independent of its length.
    history: Vec<(usize, usize)>,
}

impl Cursor {
    fn new(notices: NoticeSnapshot) -> Option<Self> {
        let end = Self::part_end(notices.get(0)?.1, 0);
        Some(Self {
            notices,
            index: 0,
            start: 0,
            end,
            part: 1,
            history: Vec::new(),
        })
    }

    fn heading(&self) -> &str {
        self.notices.get(self.index).expect("valid notice index").0
    }

    fn text(&self) -> &str {
        self.notices.get(self.index).expect("valid notice index").1
    }

    fn part_end(text: &str, start: usize) -> usize {
        text[start..]
            .char_indices()
            .nth(PART_CHARS)
            .map_or(text.len(), |(offset, _)| start + offset)
    }

    fn current(&self) -> &str {
        &self.text()[self.start..self.end]
    }

    fn has_previous(&self) -> bool {
        self.start > 0 || self.index > 0
    }

    fn has_next(&self) -> bool {
        self.end < self.text().len() || self.index + 1 < self.notices.len()
    }

    fn next(&mut self) -> bool {
        let end = self.end;
        if end == self.text().len() && self.index + 1 == self.notices.len() {
            return false;
        }
        if end < self.text().len() {
            self.start = end;
            self.part += 1;
        } else {
            self.history.push((self.start, self.part));
            self.index += 1;
            self.start = 0;
            self.part = 1;
        }
        self.end = Self::part_end(self.text(), self.start);
        true
    }

    fn previous(&mut self) -> bool {
        if self.start > 0 {
            self.end = self.start;
            self.start = self.text()[..self.end]
                .char_indices()
                .rev()
                .nth(PART_CHARS - 1)
                .map_or(0, |(offset, _)| offset);
            self.part -= 1;
        } else if let Some((start, part)) = self.history.pop() {
            self.index -= 1;
            self.start = start;
            self.end = self.text().len();
            self.part = part;
        } else {
            return false;
        }
        true
    }
}

pub(crate) enum NoticeEvent {
    Dismissed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Control {
    Previous,
    Next,
    Close,
}

fn next_control(current: Option<Control>, forward: bool, previous: bool, next: bool) -> Control {
    let controls = [Control::Previous, Control::Next, Control::Close];
    let enabled = |control| match control {
        Control::Previous => previous,
        Control::Next => next,
        Control::Close => true,
    };
    let start = current
        .and_then(|current| controls.iter().position(|control| *control == current))
        .unwrap_or(if forward { controls.len() - 1 } else { 0 });
    for step in 1..=controls.len() {
        let index = if forward {
            (start + step) % controls.len()
        } else {
            (start + controls.len() - step) % controls.len()
        };
        if enabled(controls[index]) {
            return controls[index];
        }
    }
    unreachable!("the close control is always enabled")
}

pub(crate) struct NoticeView {
    focus: FocusHandle,
    previous_focus: FocusHandle,
    next_focus: FocusHandle,
    close_focus: FocusHandle,
    title: &'static str,
    cursor: Cursor,
    heading: SharedString,
    text: SharedString,
    position: SharedString,
    scroll: ScrollHandle,
}

impl EventEmitter<NoticeEvent> for NoticeView {}

impl NoticeView {
    pub(crate) fn new(
        title: &'static str,
        notices: NoticeSnapshot,
        cx: &mut Context<Self>,
    ) -> Option<Self> {
        let cursor = Cursor::new(notices)?;
        let mut view = Self {
            focus: cx.focus_handle(),
            previous_focus: cx.focus_handle(),
            next_focus: cx.focus_handle(),
            close_focus: cx.focus_handle(),
            title,
            cursor,
            heading: SharedString::default(),
            text: SharedString::default(),
            position: SharedString::default(),
            scroll: ScrollHandle::new(),
        };
        view.update_text();
        Some(view)
    }

    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn update_text(&mut self) {
        self.heading = self.cursor.heading().into();
        self.text = if self.cursor.current().is_empty() {
            "Empty notice".into()
        } else {
            self.cursor.current().into()
        };
        self.position = format!(
            "Notice {} of {} | Part {}",
            self.cursor.index + 1,
            self.cursor.notices.len(),
            self.cursor.part
        )
        .into();
        self.scroll.set_offset(Default::default());
    }

    fn change(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let changed = if forward {
            self.cursor.next()
        } else {
            self.cursor.previous()
        };
        if changed {
            if (self.previous_focus.is_focused(window) && !self.cursor.has_previous())
                || (self.next_focus.is_focused(window) && !self.cursor.has_next())
            {
                window.focus(&self.close_focus, cx);
            }
            self.update_text();
            cx.notify();
        }
    }

    fn focus_control(&self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = if self.previous_focus.is_focused(window) {
            Some(Control::Previous)
        } else if self.next_focus.is_focused(window) {
            Some(Control::Next)
        } else if self.close_focus.is_focused(window) {
            Some(Control::Close)
        } else {
            None
        };
        let target = match next_control(
            current,
            forward,
            self.cursor.has_previous(),
            self.cursor.has_next(),
        ) {
            Control::Previous => &self.previous_focus,
            Control::Next => &self.next_focus,
            Control::Close => &self.close_focus,
        };
        window.focus(target, cx);
    }

    fn scroll_by(&self, delta: gpui::Pixels, cx: &mut Context<Self>) {
        let before = self.scroll.offset();
        let y = (before.y + delta).clamp(-self.scroll.max_offset().y, px(0.0));
        if y != before.y {
            self.scroll.set_offset(point(before.x, y));
            cx.notify();
        }
    }
}

impl crate::modal::Overlay for NoticeView {
    fn focus_handle(&self) -> FocusHandle {
        self.focus_handle()
    }
}

impl Render for NoticeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let line_height = px(fonts.line_height());
        let button = |id: &'static str, label: &'static str, active| {
            div()
                .id(id)
                .px(px(8.0))
                .py(px(4.0))
                .text_color(rgb(if active {
                    theme.shell.text
                } else {
                    theme.shell.text_muted
                }))
                .when(active, |button| {
                    button
                        .role(Role::Button)
                        .aria_label(label)
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme.shell.element_hover)))
                })
                .child(label)
        };
        div()
            .id("notice-details")
            .key_context("NoticeDetails")
            .track_focus(&self.focus)
            .w(px(MODAL_WIDTH))
            .max_w_full()
            .h(px(MODAL_MAX_HEIGHT))
            .max_h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme.shell.elevated_surface_background))
            .border_1()
            .border_color(rgb(theme.shell.border_variant))
            .rounded(px(6.0))
            .font_family(fonts.ui_family.clone())
            .text_size(px(fonts.ui_size))
            .text_color(rgb(theme.shell.text))
            .role(Role::Dialog)
            .aria_label(self.title)
            .aria_description("Use left and right for notice parts, up and down to scroll, Tab for controls, and Escape to close")
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_action(cx.listener(|_, _: &CancelInput, _, cx| {
                cx.emit(NoticeEvent::Dismissed);
            }))
            .on_action(cx.listener(|this, _: &RowUp, window, cx| this.change(false, window, cx)))
            .on_action(cx.listener(|this, _: &RowDown, window, cx| this.change(true, window, cx)))
            .on_action(cx.listener(|this, _: &NoticeFocusNext, window, cx| {
                this.focus_control(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NoticeFocusPrevious, window, cx| {
                this.focus_control(false, window, cx);
            }))
            .on_action(cx.listener(move |this, _: &DocScrollUp, _, cx| {
                this.scroll_by(line_height, cx);
            }))
            .on_action(cx.listener(move |this, _: &DocScrollDown, _, cx| {
                this.scroll_by(-line_height, cx);
            }))
            .on_action(cx.listener(|this, _: &DocPageUp, _, cx| {
                this.scroll_by(this.scroll.bounds().size.height, cx);
            }))
            .on_action(cx.listener(|this, _: &DocPageDown, _, cx| {
                this.scroll_by(-this.scroll.bounds().size.height, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_b_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .child(self.title)
                    .child(
                        div()
                            .text_size(px(fonts.small()))
                            .text_color(rgb(theme.shell.text_muted))
                            .child("Snapshot at opening"),
                    ),
            )
            .child(
                div()
                    .id("notice-details-text")
                    .flex_1()
                    .min_h(px(0.0))
                    .min_w(px(0.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(px(12.0))
                    .py(px(10.0))
                    .whitespace_normal()
                    .role(Role::Document)
                    .aria_label("Notice text")
                    .children((!self.heading.is_empty()).then(|| {
                        div()
                            .text_color(rgb(theme.shell.text_muted))
                            .child(self.heading.clone())
                    }))
                    .child(self.text.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .py(px(4.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.small()))
                    .child(self.position.clone())
                    .child(
                        button("notice-previous", "Previous", self.cursor.has_previous()).when(
                            self.cursor.has_previous(),
                            |button| {
                                button
                                    .track_focus(&self.previous_focus)
                                    .focus_visible(|style| style.bg(rgb(theme.shell.element_hover)))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.change(false, window, cx);
                                    }))
                            },
                        ),
                    )
                    .child(button("notice-next", "Next", self.cursor.has_next()).when(
                        self.cursor.has_next(),
                        |button| {
                            button
                                .track_focus(&self.next_focus)
                                .focus_visible(|style| style.bg(rgb(theme.shell.element_hover)))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.change(true, window, cx);
                                }))
                        },
                    ))
                    .child("Tab: controls | Left/Right: pages | Up/Down: scroll")
                    .child(
                        button("notice-close", "Close", true)
                            .track_focus(&self.close_focus)
                            .focus_visible(|style| style.bg(rgb(theme.shell.element_hover)))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(NoticeEvent::Dismissed))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_notice_and_multibyte_character_is_reachable_in_both_directions() {
        let messages = Rc::new(vec![
            format!("{} end", "界a".repeat(300)),
            String::new(),
            "a final server sentence".to_string(),
        ]);
        let mut cursor = Cursor::new(NoticeSnapshot::shared(messages.clone())).expect("notices");
        let mut pages = Vec::new();
        loop {
            assert!(cursor.current().chars().count() <= PART_CHARS);
            pages.push((cursor.index, cursor.current().to_string()));
            if !cursor.next() {
                break;
            }
        }
        for (index, message) in messages.iter().enumerate() {
            let rebuilt: String = pages
                .iter()
                .filter(|(page_index, _)| *page_index == index)
                .map(|(_, text)| text.as_str())
                .collect();
            assert_eq!(&rebuilt, message);
        }
        for (index, text) in pages.iter().rev().skip(1) {
            assert!(cursor.previous());
            assert_eq!((cursor.index, cursor.current()), (*index, text.as_str()));
        }
        assert!(!cursor.previous());
        assert!(!cursor.has_previous());
    }

    #[test]
    fn empty_collection_cannot_open_a_reader() {
        assert!(Cursor::new(NoticeSnapshot::new(Vec::<String>::new())).is_none());
    }

    #[test]
    fn later_notices_are_retained_even_when_the_summary_omits_them() {
        let mut notices = vec!["a provisioning error".to_string(); 12];
        notices.push("Grafana search: access denied for this account".to_string());
        let notices = Rc::new(notices);
        let snapshot = NoticeSnapshot::shared(notices);
        let mut cursor = Cursor::new(snapshot.clone()).expect("notices");
        assert!(Rc::ptr_eq(&snapshot.0, &cursor.notices.0));
        for _ in 0..12 {
            assert!(cursor.next());
        }
        assert_eq!(
            cursor.current(),
            "Grafana search: access denied for this account"
        );
        assert!(!cursor.has_next());
        assert!(cursor.previous());
        assert_eq!(cursor.current(), "a provisioning error");
    }

    #[test]
    fn a_shared_failure_pages_without_copying_the_retained_reason() {
        struct Failure(Rc<String>);

        impl NoticeSource for Failure {
            fn notice_count(&self) -> usize {
                1
            }

            fn notice_at(&self, index: usize) -> Option<(&str, &str)> {
                (index == 0).then_some(("Source discovery", self.0.as_str()))
            }
        }

        let reason = Rc::new(format!("source=\"{}\"", "é".repeat(600)));
        let snapshot = NoticeSnapshot::new(Failure(reason.clone()));
        let mut cursor = Cursor::new(snapshot).expect("failure");
        assert_eq!(cursor.heading(), "Source discovery");
        assert_eq!(cursor.text().as_ptr(), reason.as_ptr());
        let mut pages = Vec::new();
        loop {
            assert!(cursor.current().chars().count() <= PART_CHARS);
            pages.push(cursor.current().to_owned());
            if !cursor.next() {
                break;
            }
        }
        assert_eq!(pages.concat(), reason.as_str());
        for page in pages.iter().rev().skip(1) {
            assert!(cursor.previous());
            assert_eq!(cursor.current(), page.as_str());
        }
        assert!(!cursor.previous());
    }

    #[test]
    fn history_grows_per_notice_not_per_part() {
        let long = "界".repeat(PART_CHARS * 1_000 + 7);
        let notices = Rc::new(vec![long.clone(), String::new(), long]);
        let mut cursor = Cursor::new(NoticeSnapshot::shared(notices)).expect("notices");
        while cursor.next() {
            assert_eq!(cursor.history.len(), cursor.index);
            assert!(cursor.current().chars().count() <= PART_CHARS);
        }
        assert_eq!(cursor.history.len(), 2);
        while cursor.previous() {
            assert_eq!(cursor.history.len(), cursor.index);
        }
        assert_eq!(cursor.index, 0);
        assert_eq!(cursor.start, 0);
    }

    #[test]
    fn tab_cycles_within_enabled_reader_controls() {
        assert_eq!(next_control(None, true, false, true), Control::Next);
        assert_eq!(next_control(None, false, false, true), Control::Close);
        assert_eq!(
            next_control(Some(Control::Next), true, true, true),
            Control::Close
        );
        assert_eq!(
            next_control(Some(Control::Close), true, true, true),
            Control::Previous
        );
        assert_eq!(
            next_control(Some(Control::Previous), false, true, true),
            Control::Close
        );
        assert_eq!(
            next_control(Some(Control::Close), true, false, false),
            Control::Close
        );
    }

    #[test]
    fn details_shortcuts_reach_navigation_and_dismissal_in_the_details_context() {
        let keymap = gpui::Keymap::new(crate::keybindings());
        for (key, action) in [
            ("left", "k10s_shell::RowUp"),
            ("right", "k10s_shell::RowDown"),
            ("up", "k10s_shell::DocScrollUp"),
            ("down", "k10s_shell::DocScrollDown"),
            ("pageup", "k10s_shell::DocPageUp"),
            ("pagedown", "k10s_shell::DocPageDown"),
            ("escape", "k10s_shell::CancelInput"),
            ("tab", "k10s_shell::NoticeFocusNext"),
            ("shift-tab", "k10s_shell::NoticeFocusPrevious"),
        ] {
            let bindings = keymap
                .bindings_for_input(
                    &[gpui::Keystroke::parse(key).expect("details shortcut")],
                    &[
                        gpui::KeyContext::parse("Workspace").expect("workspace context"),
                        gpui::KeyContext::parse("NoticeDetails").expect("details context"),
                    ],
                )
                .0;
            assert_eq!(
                bindings.first().map(|binding| binding.action().name()),
                Some(action)
            );
        }
    }

    #[test]
    fn status_details_shortcut_is_available_from_the_workspace() {
        let keymap = gpui::Keymap::new(crate::keybindings());
        let bindings = keymap
            .bindings_for_input(
                &[gpui::Keystroke::parse("ctrl-alt-s").expect("status shortcut")],
                &[gpui::KeyContext::parse("Workspace").expect("workspace context")],
            )
            .0;
        assert_eq!(
            bindings.first().map(|binding| binding.action().name()),
            Some("k10s_shell::OpenStatusDetails")
        );
        for context in ["Typing", "Editor", "Terminal", "Palette"] {
            let bindings = keymap
                .bindings_for_input(
                    &[gpui::Keystroke::parse("ctrl-alt-s").expect("status shortcut")],
                    &[
                        gpui::KeyContext::parse("Workspace").expect("workspace context"),
                        gpui::KeyContext::parse(context).expect("input context"),
                    ],
                )
                .0;
            assert!(
                bindings
                    .iter()
                    .all(|binding| { binding.action().name() != "k10s_shell::OpenStatusDetails" })
            );
        }
    }

    #[test]
    fn inspector_chart_details_shortcut_does_not_take_text_input() {
        let keymap = gpui::Keymap::new(crate::keybindings());
        let shortcut = gpui::Keystroke::parse("ctrl-alt-i").expect("chart details shortcut");
        let workspace = gpui::KeyContext::parse("Workspace").expect("workspace context");
        let bindings = keymap
            .bindings_for_input(
                std::slice::from_ref(&shortcut),
                std::slice::from_ref(&workspace),
            )
            .0;
        assert_eq!(
            bindings.first().map(|binding| binding.action().name()),
            Some("k10s_shell::OpenInspectorChartDetails")
        );
        for context in ["Typing", "Editor", "Terminal", "Palette"] {
            let input = gpui::KeyContext::parse(context).expect("input context");
            let bindings = keymap
                .bindings_for_input(std::slice::from_ref(&shortcut), &[workspace.clone(), input])
                .0;
            assert!(
                bindings
                    .iter()
                    .all(|binding| binding.action().name()
                        != "k10s_shell::OpenInspectorChartDetails")
            );
        }
    }
}
