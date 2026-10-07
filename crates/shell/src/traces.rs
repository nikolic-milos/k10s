//! Trace-id lookup as a span table.
//!
//! Tempo and Jaeger answer through [`ClusterProvider::lookup_trace`]. Absence
//! takes the pane down. A log link retains its exact Kubernetes scope and
//! loses that association when the selection or trace ID changes.

use std::rc::Rc;

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window, canvas, div,
    prelude::*, px, rgb,
};

use crate::lists::InventoryEvent;
use crate::table::{TableState, TableText};
use crate::tag::ItemTag;
use crate::ui::{LIST_ROW_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EnterFilter, OpenRow, Refresh, RowDown, RowEnd,
    RowHome, RowPageDown, RowPageUp, RowUp,
};
use k10s_provider::{
    ClusterProvider, ObserveScope, RequestStop, SpanView, TableColumn, TablePage, TableRow,
    ToolPresence, TraceOutcome,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;
const QUERY_HEIGHT: f32 = 28.0;

struct TimelineRow {
    name: SharedString,
    duration: SharedString,
    offset: f32,
    width: f32,
}

pub struct TracesView {
    focus: FocusHandle,
    provider: Rc<dyn ClusterProvider>,
    table: TableState,
    table_text: TableText,
    query: String,
    typing: bool,
    filtering: bool,
    loading: bool,
    status: Option<SharedString>,
    generation: u64,
    probe_generation: u64,
    probe_pending: bool,
    probe_notice: SharedString,
    viewport: Viewport,
    stop: Option<RequestStop>,
    probe_stop: Option<RequestStop>,
    scope: Option<ObserveScope>,
    linked_id: Option<String>,
    association: SharedString,
    spans: Vec<SpanView>,
    timeline: Vec<TimelineRow>,
    text_key: Option<(u64, u64, bool, bool, bool, usize)>,
    breadcrumb: SharedString,
    query_label: SharedString,
    count_label: SharedString,
    visible: bool,
}

impl EventEmitter<InventoryEvent> for TracesView {}

impl TracesView {
    pub fn new(provider: Rc<dyn ClusterProvider>, cx: &mut Context<Self>) -> TracesView {
        let mut view = TracesView {
            focus: cx.focus_handle(),
            provider,
            table: TableState::new(),
            table_text: TableText::default(),
            query: String::new(),
            typing: false,
            filtering: false,
            loading: false,
            status: None,
            generation: 0,
            probe_generation: 0,
            probe_pending: false,
            probe_notice: SharedString::default(),
            viewport: Viewport::default(),
            stop: None,
            probe_stop: None,
            scope: None,
            linked_id: None,
            association: "No Kubernetes association requested".into(),
            spans: Vec::new(),
            timeline: Vec::new(),
            text_key: None,
            breadcrumb: SharedString::default(),
            query_label: SharedString::default(),
            count_label: SharedString::default(),
            visible: true,
        };
        view.probe(cx);
        view
    }

    fn probe(&mut self, cx: &mut Context<Self>) {
        self.probe_stop = None;
        self.probe_generation += 1;
        self.probe_pending = true;
        let generation = self.probe_generation;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.probe_stop = Some(self.provider.probe_observe(Box::new(move |reach| {
            let _ = tx.send(reach);
        })));
        cx.spawn(async move |this, cx| {
            if let Ok(reach) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.probe_generation != generation || !this.visible {
                        return;
                    }
                    this.probe_stop = None;
                    this.probe_pending = false;
                    this.probe_notice = reach.error.unwrap_or_default().into();
                    cx.notify();
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
        "Traces".into()
    }

    pub fn open_link(&mut self, id: String, scope: ObserveScope, cx: &mut Context<Self>) {
        self.linked_id = Some(id.clone());
        self.query = id;
        self.scope = Some(scope);
        self.lookup(cx);
    }

    pub(crate) fn set_selection(
        &mut self,
        selection: Option<&crate::selection::Selection>,
        cx: &mut Context<Self>,
    ) {
        let request = selection.map(|selection| selection.describe_request());
        if self
            .scope
            .as_ref()
            .is_some_and(|scope| Some(&scope.request) != request.as_ref())
        {
            self.cancel_lookup();
            self.scope = None;
            self.linked_id = None;
            self.association = "Selection changed; open a trace from current logs".into();
            self.status = None;
            cx.notify();
        }
    }

    fn cancel_lookup(&mut self) {
        self.stop = None;
        self.generation += 1;
        self.loading = false;
        self.spans.clear();
        self.timeline.clear();
        self.table.set_page(TablePage::default());
    }

    fn lookup(&mut self, cx: &mut Context<Self>) {
        self.cancel_lookup();
        let trace_id = self.query.trim().to_string();
        if self
            .linked_id
            .as_ref()
            .is_none_or(|id| !id.eq_ignore_ascii_case(&trace_id))
        {
            self.scope = None;
            self.linked_id = None;
        }
        self.association = if self.scope.is_some() {
            "Checking linked Kubernetes UID association".into()
        } else {
            "No Kubernetes association requested".into()
        };
        if trace_id.is_empty() {
            self.status = Some("type a trace id and press enter".into());
            cx.notify();
            return;
        }
        let generation = self.generation;
        self.loading = true;
        self.status = Some("looking up the trace...".into());
        let (tx, rx) = futures::channel::oneshot::channel();
        self.stop = Some(self.provider.lookup_trace(
            trace_id.clone(),
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        ));
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation
                        || !this.visible
                        || !this.query.trim().eq_ignore_ascii_case(&trace_id)
                    {
                        return;
                    }
                    this.loading = false;
                    this.stop = None;
                    match outcome {
                        TraceOutcome::Empty => {
                            let (association, status) = empty_trace(
                                &trace_id,
                                &mut this.query,
                                &mut this.table,
                                &mut this.timeline,
                                &mut this.spans,
                            );
                            this.association = association.into();
                            this.status = Some(status.into());
                        }
                        TraceOutcome::Trace {
                            trace_id: returned_id,
                            spans,
                        } => {
                            if !returned_id.eq_ignore_ascii_case(&trace_id) {
                                this.association = "Kubernetes UID association is unproven".into();
                                this.status = Some(
                                    "Returned trace ID does not match the requested trace".into(),
                                );
                            } else {
                                this.association = association(this.scope.as_ref(), &spans).into();
                                this.timeline = timeline(&spans);
                                this.table.set_page(spans_page(&returned_id, &spans));
                                this.status = spans
                                    .is_empty()
                                    .then(|| "No spans returned for this trace".into());
                                this.spans = spans;
                            }
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
                            this.status =
                                Some(format!("{what}: access denied for this account").into());
                        }
                        TraceOutcome::Failed(why) => {
                            this.table.set_page(TablePage::default());
                            this.status = Some(why.into());
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

    fn refresh_text(&mut self) {
        let key = (
            self.generation,
            self.table.content_revision(),
            self.typing,
            self.filtering,
            self.loading,
            self.table.visible_rows(),
        );
        if self.text_key == Some(key) {
            return;
        }
        self.breadcrumb = self.breadcrumb().into();
        self.query_label = if self.typing {
            format!("trace id: {}_", self.query).into()
        } else if self.query.is_empty() {
            "Trace ID".into()
        } else {
            format!("trace id: {}", self.query).into()
        };
        self.count_label = format!("{} spans", self.table.visible_rows()).into();
        self.text_key = Some(key);
    }

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let rows = self.viewport.rows(0.0, 0.0, LIST_ROW_HEIGHT, 400);
        self.table.set_viewport(rows);
        cx.notify();
    }

    fn edit_query(&mut self, cx: &mut Context<Self>) {
        self.cancel_lookup();
        self.typing = true;
        self.filtering = false;
        self.association = "Kubernetes UID association was not checked".into();
        self.status = Some("Enter a trace ID".into());
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
    fn visibility_changed(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if !visible {
            self.generation += 1;
            self.probe_generation += 1;
            if self.stop.is_some() {
                self.status =
                    Some("Request cancelled while this pane is hidden; refresh to retry".into());
                self.association = "Kubernetes UID association was not checked".into();
            }
            self.stop = None;
            self.probe_stop = None;
            self.loading = false;
        } else if self.probe_pending {
            self.probe(cx);
        }
    }
}

impl Render for TracesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.table_text.update(&self.table);
        self.refresh_text();
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
                    this.cancel_lookup();
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
                        this.edit_query(cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                if let Some(key_char) = &keystroke.key_char {
                    if this.typing {
                        this.query.push_str(key_char);
                        this.cancel_lookup();
                    } else {
                        this.table.push_filter(key_char);
                    }
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = f32::from(event.delta.pixel_delta(px(LIST_ROW_HEIGHT)).y);
                this.table
                    .move_selection(-(delta / LIST_ROW_HEIGHT).round() as i64);
                cx.notify();
            }))
            .child(panel_header(&theme, &fonts, self.breadcrumb.clone()))
            .child(
                div()
                    .px(px(12.0))
                    .text_size(px(fonts.ui_size))
                    .child(self.association.clone()),
            )
            .children(
                (!self.probe_notice.is_empty())
                    .then(|| div().px(px(12.0)).child(self.probe_notice.clone())),
            )
            .child(
                div()
                    .px(px(12.0))
                    .children(self.timeline.iter().map(|span| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(fonts.ui_size))
                            .child(
                                div()
                                    .w(px(180.0))
                                    .overflow_hidden()
                                    .child(span.name.clone()),
                            )
                            .child(
                                div().w_full().h(px(16.0)).relative().child(
                                    div()
                                        .absolute()
                                        .left(gpui::relative(span.offset))
                                        .w(gpui::relative(span.width))
                                        .h_full()
                                        .bg(rgb(theme.shell.text_accent)),
                                ),
                            )
                            .child(span.duration.clone())
                    })),
            )
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
                    .text_size(px(fonts.ui_size))
                    .child(self.query_label.clone()),
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
                    .text_size(px(fonts.ui_size))
                    .text_color(rgb(theme.shell.text_muted))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(SharedString::from(self.table_text.header())),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("trace-rows")
                            .flex_1()
                            .min_h(px(LIST_ROW_HEIGHT))
                            .overflow_hidden()
                            .relative()
                            .flex()
                            .flex_col()
                            .role(Role::ListBox)
                            .aria_label("spans")
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
                            .children(self.table_text.lines().iter().enumerate().map(
                                |(offset, line)| {
                                    let selected = self.table.line_selected(offset);
                                    let line = SharedString::from(line.clone());
                                    let mut row = div()
                                        .id(("trace-row", offset))
                                        .h(px(LIST_ROW_HEIGHT))
                                        .flex_none()
                                        .px(px(12.0))
                                        .flex()
                                        .items_center()
                                        .font_family(fonts.buffer_family.clone())
                                        .text_size(px(fonts.ui_size))
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
                            )),
                    )
                    .children(self.status.clone().map(|status| {
                        div()
                            .flex_none()
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
                    .text_size(px(fonts.ui_size))
                    .text_color(rgb(theme.shell.text_muted))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(self.count_label.clone()),
            )
    }
}

fn timeline(spans: &[SpanView]) -> Vec<TimelineRow> {
    let Some(start) = spans.iter().map(|span| span.start_us).min() else {
        return Vec::new();
    };
    let end = spans
        .iter()
        .map(|span| span.start_us.saturating_add(span.duration_us))
        .max()
        .unwrap_or(start);
    let duration = end.saturating_sub(start).max(1) as f64;
    spans
        .iter()
        .take(8)
        .map(|span| {
            let width = (span.duration_us as f64 / duration).clamp(0.002, 1.0) as f32;
            let offset = (span.start_us.saturating_sub(start) as f64 / duration) as f32;
            TimelineRow {
                name: span.name.clone().into(),
                duration: format_us(span.duration_us).into(),
                offset: offset.clamp(0.0, 1.0 - width),
                width,
            }
        })
        .collect()
}

fn empty_trace(
    requested_id: &str,
    query: &mut String,
    table: &mut TableState,
    timeline: &mut Vec<TimelineRow>,
    spans: &mut Vec<SpanView>,
) -> (&'static str, &'static str) {
    requested_id.clone_into(query);
    table.set_page(TablePage::default());
    timeline.clear();
    spans.clear();
    (
        "Kubernetes UID association is unproven",
        "No spans returned for this trace",
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_provider::{DescribeRequest, PodIdentity};

    fn span(start_us: u64, duration_us: u64, uid: &str) -> SpanView {
        SpanView {
            id: "span".into(),
            parent: String::new(),
            name: "operation".into(),
            service: "producer".into(),
            start_us,
            duration_us,
            status: "OK".into(),
            namespace: "prod".into(),
            pod: "producer".into(),
            pod_uid: uid.into(),
            node: "node".into(),
        }
    }

    fn scope() -> ObserveScope {
        ObserveScope {
            request: DescribeRequest {
                kind: k10s_core::KindId::POD,
                namespace: Some("prod".into()),
                name: "producer".into(),
                uid: "current".into(),
            },
            namespace: "prod".into(),
            pods: vec![PodIdentity {
                name: "producer".into(),
                uid: "current".into(),
                node: "node".into(),
            }],
        }
    }

    #[test]
    fn a_recreated_or_foreign_pod_is_never_claimed_as_the_linked_selection() {
        let scope = scope();
        assert!(
            association(Some(&scope), &[span(0, 100, "current")]).starts_with("All 1 spans match")
        );
        assert!(association(Some(&scope), &[span(0, 100, "retired")]).contains("unproven"));
        let mut foreign = span(0, 100, "current");
        foreign.namespace = "other".into();
        assert!(association(Some(&scope), &[foreign]).contains("unproven"));
        assert!(
            association(
                Some(&scope),
                &[span(0, 100, "current"), span(30, 20, "retired")]
            )
            .starts_with("1/2 spans match")
        );
        assert_eq!(
            association(None, &[span(0, 100, "current")]),
            "No Kubernetes association requested"
        );
        assert!(association(Some(&scope), &[]).contains("unproven"));
    }

    #[test]
    fn empty_trace_preserves_the_request_and_clears_prior_rows_and_association() {
        let requested = "1234567890abcdef1234567890abcdef";
        let mut query = "previous query".to_string();
        let mut spans = vec![span(0, 100, "current"), span(30, 20, "current")];
        let mut timeline = timeline(&spans);
        let mut table = TableState::new();
        table.set_page(spans_page("previous trace", &spans));
        let (association, status) =
            empty_trace(requested, &mut query, &mut table, &mut timeline, &mut spans);
        assert_eq!(query, requested);
        assert_eq!(table.total_rows(), 0);
        assert!(timeline.is_empty());
        assert!(spans.is_empty());
        assert_eq!(association, "Kubernetes UID association is unproven");
        assert_eq!(status, "No spans returned for this trace");
    }

    #[test]
    fn timeline_uses_all_span_extents_and_bounds_every_visible_mark() {
        let spans = [
            span(100, 40, "current"),
            span(0, 200, "current"),
            span(200, 0, "current"),
        ];
        let rows = timeline(&spans);
        assert_eq!(rows[0].offset, 0.5);
        assert_eq!(rows[0].width, 0.2);
        assert_eq!(rows[1].offset, 0.0);
        assert_eq!(rows[1].width, 1.0);
        assert!(
            rows.iter()
                .all(|row| row.offset >= 0.0 && row.width > 0.0 && row.offset + row.width <= 1.0)
        );
        let dense: Vec<_> = (0..100).map(|index| span(index, 1, "current")).collect();
        assert_eq!(timeline(&dense).len(), 8);
        assert!(timeline(&[]).is_empty());
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

fn association(scope: Option<&ObserveScope>, spans: &[SpanView]) -> String {
    let Some(scope) = scope else {
        return "No Kubernetes association requested".into();
    };
    let matched = spans
        .iter()
        .filter(|span| {
            span.namespace == scope.namespace
                && scope
                    .pods
                    .iter()
                    .any(|pod| pod.name == span.pod && pod.uid == span.pod_uid)
        })
        .count();
    if !spans.is_empty() && matched == spans.len() {
        format!("All {matched} spans match the selected namespace, pod name and UID")
    } else if matched > 0 {
        format!(
            "{matched}/{} spans match the selected pod UIDs; other spans belong to unverified resources",
            spans.len()
        )
    } else {
        "Trace ID matches the log link; Kubernetes UID association is unproven".into()
    }
}
