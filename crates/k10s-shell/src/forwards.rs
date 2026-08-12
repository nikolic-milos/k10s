//! The forwards item: every active port-forward, with start and stop.
//!
//! A thin view over the provider's forward registry, reusing the pure table
//! machine. The registry is the truth: this view lists what it holds --
//! including forwards that died, shown with their reason until closed -- and
//! refreshes after every action and on `r`; it never polls, because an idle
//! shell must not paint. Starting arrives either from a pod or service row in
//! the browser or from a selection, as a [`ForwardRequest`] the provider
//! resolves; every outcome lands in the status line, labelled.

use std::rc::Rc;

use gpui::{
    Context, FocusHandle, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, Role,
    ScrollWheelEvent, SharedString, Styled, Window, canvas, div, prelude::*, px, rgb,
};

use crate::provider::{
    ForwardOutcome, ForwardRequest, ForwardRow, ForwardState, ReadProvider, TableColumn, TablePage,
    TableRow,
};
use crate::table::TableState;
use crate::ui::{LIST_ROW_HEIGHT, PANEL_HEADER_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{Refresh, RowDown, RowEnd, RowHome, RowPageDown, RowPageUp, RowUp, StopForward};

const TABLE_HEADER_HEIGHT: f32 = 28.0;
const VIEW_CHROME_HEIGHT: f32 = PANEL_HEADER_HEIGHT + TABLE_HEADER_HEIGHT + STATUS_BAR_HEIGHT;

pub struct ForwardsView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    table: TableState,
    status: Option<String>,
    viewport: Viewport,
}

impl ForwardsView {
    pub fn new(
        provider: Rc<dyn ReadProvider>,
        start: Option<ForwardRequest>,
        cx: &mut Context<Self>,
    ) -> ForwardsView {
        let mut view = ForwardsView {
            focus: cx.focus_handle(),
            provider,
            table: TableState::new(),
            status: None,
            viewport: Viewport::default(),
        };
        view.refresh();
        if let Some(request) = start {
            view.start(request, cx);
        }
        view
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn title(&self) -> SharedString {
        "forwards".into()
    }

    fn refresh(&mut self) {
        self.table
            .set_page(forwards_page(&self.provider.list_forwards()));
    }

    pub fn start(&mut self, request: ForwardRequest, cx: &mut Context<Self>) {
        self.status = Some(format!(
            "opening a forward to {}/{}...",
            request.namespace, request.name
        ));
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.open_forward(
            &request,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.status = Some(match outcome {
                        ForwardOutcome::Opened(row) => format!(
                            "forwarding 127.0.0.1:{} -> {}:{}",
                            row.local_port, row.pod, row.remote_port
                        ),
                        ForwardOutcome::Denied(what) => {
                            format!("{what}: access denied for this account")
                        }
                        ForwardOutcome::Failed(why) => why,
                    });
                    this.refresh();
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn stop_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .table
            .selected_row()
            .and_then(|row| row.uid.parse().ok())
        else {
            return;
        };
        if self.provider.close_forward(id) {
            self.status = Some("forward closed".to_string());
        }
        self.refresh();
        cx.notify();
    }

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let rows = self
            .viewport
            .rows(VIEW_CHROME_HEIGHT, 0.0, LIST_ROW_HEIGHT, 400)
            .max(4);
        self.table.set_viewport(rows);
        cx.notify();
    }
}

fn state_text(state: &ForwardState) -> String {
    match state {
        ForwardState::Opening => "opening".to_string(),
        ForwardState::Active => "active".to_string(),
        ForwardState::Dead(why) => format!("dead: {why}"),
    }
}

fn forwards_page(rows: &[ForwardRow]) -> TablePage {
    let columns = ["Namespace", "Pod", "Local", "Remote", "State"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let rows = rows
        .iter()
        .map(|row| TableRow {
            cells: vec![
                row.namespace.clone(),
                row.pod.clone(),
                format!("127.0.0.1:{}", row.local_port),
                row.remote_port.to_string(),
                state_text(&row.state),
            ],
            name: row.pod.clone(),
            namespace: Some(row.namespace.clone()),
            uid: row.id.to_string(),
        })
        .collect();
    TablePage {
        columns,
        rows,
        truncated: false,
        continue_token: None,
    }
}

impl crate::item::Item for ForwardsView {
    fn title(&self) -> SharedString {
        ForwardsView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        ForwardsView::focus_handle(self)
    }
}

impl Render for ForwardsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let empty = self.table.total_rows() == 0;

        div()
            .id("forwards-view")
            .key_context("Browse")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(theme.shell.panel_background))
            .font_family(fonts.ui_family.clone())
            .text_color(rgb(theme.shell.text))
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
            .on_action(cx.listener(|this, _: &RowUp, _, cx| {
                this.table.move_selection(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                this.table.move_selection(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageUp, _, cx| {
                this.table.page_by(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageDown, _, cx| {
                this.table.page_by(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowHome, _, cx| {
                this.table.select_first();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowEnd, _, cx| {
                this.table.select_last();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.refresh();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &StopForward, _, cx| {
                this.stop_selected(cx);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = f32::from(event.delta.pixel_delta(px(LIST_ROW_HEIGHT)).y);
                this.table
                    .move_selection(-(delta / LIST_ROW_HEIGHT).round() as i64);
                cx.notify();
            }))
            .child(panel_header(
                &theme,
                &fonts,
                format!("forwards: {}", self.table.total_rows()),
            ))
            .child(
                div()
                    .h(px(TABLE_HEADER_HEIGHT))
                    .flex_none()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .bg(rgb(theme.shell.editor_background))
                    .border_b_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .font_family(fonts.buffer_family.clone())
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(self.table.header_line()),
            )
            .child(
                div()
                    .id("forward-rows")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .role(Role::ListBox)
                    .aria_label("Port forwards")
                    .children(self.table.visible_lines().into_iter().enumerate().map(
                        |(offset, (selected, line))| {
                            let mut row = div()
                                .id(("forward-row", offset))
                                .h(px(LIST_ROW_HEIGHT))
                                .flex_none()
                                .px(px(12.0))
                                .flex()
                                .items_center()
                                .font_family(fonts.buffer_family.clone())
                                .text_size(px(fonts.small()))
                                .text_color(rgb(theme.shell.editor_foreground))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .cursor_pointer()
                                .role(Role::ListBoxOption)
                                .aria_label(line.clone())
                                .aria_selected(selected)
                                .hover(|style| style.bg(rgb(theme.shell.element_hover)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                        this.table.select_visible_offset(offset);
                                        cx.notify();
                                    }),
                                );
                            if selected {
                                row = row.bg(rgb(theme.shell.element_selected));
                            }
                            row.child(line)
                        },
                    ))
                    .children(empty.then(|| {
                        div().p(px(12.0)).text_size(px(fonts.ui_size)).text_color(rgb(theme.shell.text_muted)).child(
                            "no forwards open; F on a pod or service row in the browser starts one",
                        )
                    }))
                    .children(self.status.clone().map(|status| {
                        div()
                            .p(px(12.0))
                            .text_size(px(fonts.ui_size))
                            .text_color(rgb(theme.shell.text))
                            .child(status)
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
                    .child("x stop · r refresh"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forward_row_renders_its_lifecycle_state_including_death() {
        let rows = vec![
            ForwardRow {
                id: 7,
                namespace: "prod".to_string(),
                pod: "api-1".to_string(),
                local_port: 8080,
                remote_port: 80,
                state: ForwardState::Active,
            },
            ForwardRow {
                id: 9,
                namespace: "prod".to_string(),
                pod: "web-1".to_string(),
                local_port: 3000,
                remote_port: 3000,
                state: ForwardState::Dead("the pod is gone".to_string()),
            },
        ];
        let page = forwards_page(&rows);
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0].uid, "7", "stop is keyed by registry id");
        assert_eq!(page.rows[0].cells[2], "127.0.0.1:8080");
        assert_eq!(page.rows[0].cells[4], "active");
        assert_eq!(
            page.rows[1].cells[4], "dead: the pod is gone",
            "a dead forward stays visible with its reason"
        );
    }
}
