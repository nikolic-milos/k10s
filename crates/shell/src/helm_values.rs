//! Owned Helm report documents: a revision diff, or a rollback report.
//!
//! A revision diff deliberately carries user-values lines — that is what a
//! values diff is — so this view is a reveal surface, not a values-free
//! one. What keeps it contained: the lines are one owned `String` an
//! action already produced (never a table page, a saved view, or a log
//! line), and the tab retires with its cluster like every other
//! [`crate::tag::ItemTag::HelmReport`]. Whole revealed documents leave
//! through [`crate::editor::EditorView::scratch`] instead and are dropped
//! with the [`crate::provider::HelmReveal`].

use gpui::{
    Context, FocusHandle, IntoElement, KeyDownEvent, ParentElement, Render, Role, ScrollWheelEvent,
    SharedString, Styled, Window, canvas, div, prelude::*, px, rgb,
};

use crate::text::TextState;
use crate::ui::{CONTENT_PADDING, PANEL_HEADER_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{
    CancelDoc, CancelInput, CommitInput, DeleteInputChar, DocEnd, DocHome, DocPageDown, DocPageUp,
    DocScrollDown, DocScrollUp, EnterSearch, NextMatch, PrevMatch,
};

pub struct HelmReportView {
    focus: FocusHandle,
    title: SharedString,
    state: TextState,
    searching: bool,
    input: String,
    viewport: Viewport,
}

impl HelmReportView {
    pub fn new(
        title: impl Into<String>,
        lines: Vec<String>,
        cx: &mut Context<Self>,
    ) -> HelmReportView {
        let mut state = TextState::new(usize::MAX);
        state.set_lines(lines);
        HelmReportView {
            focus: cx.focus_handle(),
            title: title.into().into(),
            state,
            searching: false,
            input: String::new(),
            viewport: Viewport::default(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn status_line(&self) -> String {
        let mut parts = vec![format!("{} lines", self.state.len())];
        if self.searching {
            parts.push(format!("/{}_", self.input));
        } else if let Some((query, current, total)) = self.state.search() {
            if let Some(reason) = self.state.search_error() {
                parts.push(format!("/{query} invalid pattern: {reason}"));
            } else if total == 0 {
                parts.push(format!("/{query} no matches"));
            } else {
                parts.push(format!("/{query} {current}/{total}"));
            }
        }
        parts.join("  ·  ")
    }

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let rows = self.viewport.rows(
            PANEL_HEADER_HEIGHT + STATUS_BAR_HEIGHT,
            CONTENT_PADDING * 2.0,
            k10s_theme::typography(cx).line_height(),
            400,
        );
        self.state.set_viewport(rows.max(4));
        cx.notify();
    }
}

impl crate::item::Item for HelmReportView {
    fn title(&self) -> SharedString {
        HelmReportView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        HelmReportView::focus_handle(self)
    }
}

impl Render for HelmReportView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let match_line = self.state.current_match_line();
        let lines: Vec<_> = self
            .state
            .visible()
            .map(|(index, line)| (index, SharedString::from(line.to_string())))
            .collect();

        div()
            .id("helm-report-view")
            .key_context(if self.searching { "Typing" } else { "Doc" })
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(theme.shell.editor_background))
            .font_family(fonts.buffer_family.clone())
            .text_color(rgb(theme.shell.editor_foreground))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.resize(
                                f32::from(bounds.size.width),
                                f32::from(bounds.size.height),
                                cx,
                            );
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_action(cx.listener(|this, _: &DocScrollUp, _, cx| {
                this.state.scroll_by(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocScrollDown, _, cx| {
                this.state.scroll_by(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocPageUp, _, cx| {
                this.state.page_up();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocPageDown, _, cx| {
                this.state.page_down();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocHome, _, cx| {
                this.state.home();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DocEnd, _, cx| {
                this.state.end();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EnterSearch, _, cx| {
                this.searching = true;
                this.input = this
                    .state
                    .search()
                    .map(|(query, ..)| query.to_string())
                    .unwrap_or_default();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextMatch, _, cx| {
                this.state.next_match();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrevMatch, _, cx| {
                this.state.prev_match();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelDoc, _, cx| {
                if this.state.search().is_some() {
                    this.state.set_search(None);
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                this.searching = false;
                this.state.set_search(Some(this.input.clone()));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                this.searching = false;
                this.input.clear();
                this.state.set_search(None);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                this.input.pop();
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !this.searching {
                    return;
                }
                let keystroke = &event.keystroke;
                if keystroke.modifiers.control || keystroke.modifiers.alt {
                    return;
                }
                if let Some(key_char) = &keystroke.key_char {
                    this.input.push_str(key_char);
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let row = k10s_theme::typography(cx).line_height();
                let delta = f32::from(event.delta.pixel_delta(px(row)).y);
                this.state.scroll_by(-(delta / row).round() as i64);
                cx.notify();
            }))
            .child(panel_header(&theme, &fonts, self.title.clone()))
            .child(
                div()
                    .id("helm-report-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .role(Role::Document)
                    .aria_label(self.title.clone())
                    .children(lines.into_iter().map(|(index, text)| {
                        let mut line = div()
                            .h(px(fonts.line_height()))
                            .flex_none()
                            .overflow_hidden()
                            .text_size(px(fonts.buffer_size))
                            .text_color(rgb(theme.shell.editor_foreground))
                            .whitespace_nowrap();
                        if Some(index) == match_line {
                            let (color, alpha) = theme.shell.search_match_background;
                            line = line.bg(rgb(color).alpha(alpha));
                        }
                        line.child(text)
                    })),
            )
            .child(
                div()
                    .h(px(STATUS_BAR_HEIGHT))
                    .flex_none()
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .bg(rgb(theme.shell.panel_background))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(self.status_line()),
            )
    }
}
