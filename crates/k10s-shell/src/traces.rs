//! Trace-id lookup as a span table.
//!
//! Tempo and Jaeger answer through [`ReadProvider::lookup_trace`]. Absence
//! takes the pane down. Nothing here is a web trace UI.

use std::rc::Rc;

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window, canvas, div,
    prelude::*, px, rgb,
};

use crate::lists::InventoryEvent;
use crate::provider::{
    ReadProvider, SpanView, TableColumn, TablePage, TableRow, ToolPresence, TraceOutcome,
};
use crate::table::TableState;
use crate::tag::ItemTag;
use crate::ui::{LIST_ROW_HEIGHT, PANEL_HEADER_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EnterFilter, OpenRow, Refresh, RowDown, RowEnd,
    RowHome, RowPageDown, RowPageUp, RowUp,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;
const QUERY_HEIGHT: f32 = 28.0;
const VIEW_CHROME_HEIGHT: f32 =
    PANEL_HEADER_HEIGHT + TABLE_HEADER_HEIGHT + QUERY_HEIGHT + STATUS_BAR_HEIGHT;

pub struct TracesView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    table: TableState,
    query: String,
    typing: bool,
    filtering: bool,
    loading: bool,
    status: Option<String>,
    generation: u64,
    viewport: Viewport,
}

impl EventEmitter<InventoryEvent> for TracesView {}

impl TracesView {
    pub fn new(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> TracesView {
        let mut view = TracesView {
            focus: cx.focus_handle(),
            provider,
            table: TableState::new(),
            query: String::new(),
            typing: false,
            filtering: false,
            loading: false,
            status: None,
            generation: 0,
            viewport: Viewport::default(),
        };
        view.probe(cx);
        view
    }

    fn probe(&mut self, cx: &mut Context<Self>) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.probe_observe(Box::new(move |reach| {
            let _ = tx.send(reach);
        }));
        cx.spawn(async move |this, cx| {
            if let Ok(reach) = rx.await {
                let _ = this.update(cx, |_this, cx| {
                    if reach.traces == ToolPresence::Missing {
                        cx.emit(InventoryEvent::NotServed {
                            tag: ItemTag::Traces,
                            what: "Tempo and Jaeger",
                        });
                    }
                });
            }
        })
        .detach();
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn title(&self) -> SharedString {
        "traces".into()
    }

    fn lookup(&mut self, cx: &mut Context<Self>) {
        let trace_id = self.query.trim().to_string();
        if trace_id.is_empty() {
            self.status = Some("type a trace id and press enter".into());
            cx.notify();
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.status = Some("looking up the trace...".into());
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.lookup_trace(
            trace_id,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.loading = false;
                    match outcome {
                        TraceOutcome::Trace { trace_id, spans } => {
                            this.table.set_page(spans_page(&trace_id, &spans));
                            this.status = None;
                        }
                        TraceOutcome::Absent => {
                            this.table.set_page(TablePage::default());
                            cx.emit(InventoryEvent::NotServed {
                                tag: ItemTag::Traces,
                                what: "Tempo and Jaeger",
                            });
                        }
                        TraceOutcome::Denied(what) => {
                            this.table.set_page(TablePage::default());
                            this.status = Some(format!("{what}: access denied for this account"));
                        }
                        TraceOutcome::Failed(why) => {
                            this.table.set_page(TablePage::default());
                            this.status = Some(why);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn breadcrumb(&self) -> String {
        let mut crumb = format!("traces: {} spans", self.table.visible_rows());
        if self.loading {
            crumb.push_str("  loading...");
        }
        if self.typing {
            crumb.push_str(&format!("  id: {}_", self.query));
        } else if self.filtering {
            crumb.push_str(&format!("  filter: {}_", self.table.filter));
        } else if !self.query.is_empty() {
            crumb.push_str(&format!("  id: {}", self.query));
        }
        crumb
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

impl crate::item::Item for TracesView {
    fn title(&self) -> SharedString {
        TracesView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        TracesView::focus_handle(self)
    }
}

impl Render for TracesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();

        div()
            .id("traces-view")
            .key_context(if self.typing || self.filtering {
                "Typing"
            } else {
                "Browse"
            })
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
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| {
                if this.typing {
                    this.typing = false;
                }
                this.lookup(cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.lookup(cx);
            }))
            .on_action(cx.listener(|this, _: &EnterFilter, _, cx| {
                this.filtering = true;
                this.typing = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                if this.typing {
                    this.typing = false;
                    this.lookup(cx);
                } else {
                    this.filtering = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                this.filtering = false;
                this.typing = false;
                this.table.clear_filter();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                if this.typing {
                    this.query.pop();
                } else {
                    this.table.pop_filter();
                }
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let keystroke = &event.keystroke;
                if keystroke.modifiers.control || keystroke.modifiers.alt {
                    return;
                }
                if !this.filtering && !this.typing {
                    if keystroke.key == "q" {
                        this.typing = true;
                        cx.notify();
                    }
                    return;
                }
                if let Some(key_char) = &keystroke.key_char {
                    if this.typing {
                        this.query.push_str(key_char);
                    } else {
                        this.table.push_filter(key_char);
                    }
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = f32::from(event.delta.pixel_delta(px(LIST_ROW_HEIGHT)).y);
                this.table
                    .move_selection(-(delta / LIST_ROW_HEIGHT).round() as i64);
                cx.notify();
            }))
            .child(panel_header(&theme, &fonts, self.breadcrumb()))
            .child(
                div()
                    .h(px(QUERY_HEIGHT))
                    .flex_none()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .bg(rgb(theme.shell.editor_background))
                    .border_b_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .font_family(fonts.buffer_family.clone())
                    .text_size(px(fonts.small()))
                    .child(if self.typing {
                        format!("trace id: {}_", self.query)
                    } else if self.query.is_empty() {
                        "q types a trace id · enter looks it up".to_string()
                    } else {
                        format!("trace id: {}", self.query)
                    }),
            )
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
                    .id("trace-rows")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .role(Role::ListBox)
                    .aria_label("spans")
                    .children(self.table.visible_lines().into_iter().enumerate().map(
                        |(offset, (selected, line))| {
                            let mut row = div()
                                .id(("trace-row", offset))
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
                    .child("q trace id · enter lookup · / filter"),
            )
    }
}

fn spans_page(trace_id: &str, spans: &[SpanView]) -> TablePage {
    let columns = ["Span", "Parent", "Name", "Service", "Duration", "Status"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let rows = spans
        .iter()
        .map(|span| TableRow {
            cells: vec![
                span.id.clone(),
                span.parent.clone(),
                span.name.clone(),
                span.service.clone(),
                format_us(span.duration_us),
                span.status.clone(),
            ],
            name: span.name.clone(),
            namespace: Some(span.service.clone()),
            uid: format!("{trace_id}/{}", span.id),
        })
        .collect();
    TablePage {
        columns,
        rows,
        truncated: false,
        continue_token: None,
    }
}

fn format_us(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.2}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}us")
    }
}
