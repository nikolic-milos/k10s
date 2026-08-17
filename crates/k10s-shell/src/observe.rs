//! Grafana queries, optional PromQL, and Loki, rendered natively.
//!
//! We run the expressions a dashboard already named. Unsupported panels
//! open the system browser when a Bound URL exists. Loki and Prometheus
//! panes hide when those tools are absent. Nothing here is a webview.

use std::rc::Rc;

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window, canvas, div,
    prelude::*, px, rgb,
};

use crate::lists::InventoryEvent;
use crate::provider::{
    GrafanaOutcome, GrafanaPanelKind, GrafanaPanelRow, LokiOutcome, PromOutcome, QueryDialect,
    ReadProvider, TableColumn, TablePage, TableRow, ToolPresence,
};
use crate::table::TableState;
use crate::tag::ItemTag;
use crate::text::TextState;
use crate::ui::{
    CONTENT_PADDING, LIST_ROW_HEIGHT, PANEL_HEADER_HEIGHT, STATUS_BAR_HEIGHT, Viewport,
    panel_header,
};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EnterFilter, OpenRow, Refresh, RowDown, RowEnd,
    RowHome, RowPageDown, RowPageUp, RowUp,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;
const QUERY_HEIGHT: f32 = 28.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryKind {
    Prom,
    Loki,
}

pub struct ObserveView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    table: TableState,
    panels: Vec<GrafanaPanelRow>,
    grafana: ToolPresence,
    prometheus: ToolPresence,
    loki: ToolPresence,
    results: TextState,
    query: String,
    query_kind: QueryKind,
    typing: bool,
    filtering: bool,
    loading: bool,
    status: Option<String>,
    generation: u64,
    /// Queries age separately from reloads: running a query must not stale
    /// an in-flight probe or dashboard fetch, or the panel list is lost.
    query_generation: u64,
    viewport: Viewport,
}

impl EventEmitter<InventoryEvent> for ObserveView {}

impl ObserveView {
    pub fn new(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> ObserveView {
        let mut view = ObserveView {
            focus: cx.focus_handle(),
            provider,
            table: TableState::new(),
            panels: Vec::new(),
            grafana: ToolPresence::Missing,
            prometheus: ToolPresence::Missing,
            loki: ToolPresence::Missing,
            results: TextState::new(crate::text::MAX_LOG_LINES),
            query: String::new(),
            query_kind: QueryKind::Prom,
            typing: false,
            filtering: false,
            loading: true,
            status: None,
            generation: 0,
            query_generation: 0,
            viewport: Viewport::default(),
        };
        view.reload(cx);
        view
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn title(&self) -> SharedString {
        "observe".into()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.status = None;
        self.probe(generation, cx);
        self.fetch_grafana(generation, cx);
    }

    fn probe(&mut self, generation: u64, cx: &mut Context<Self>) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.probe_observe(Box::new(move |reach| {
            let _ = tx.send(reach);
        }));
        cx.spawn(async move |this, cx| {
            if let Ok(reach) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.prometheus = reach.prometheus;
                    this.loki = reach.loki;
                    this.apply_absence(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn fetch_grafana(&mut self, generation: u64, cx: &mut Context<Self>) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_grafana(Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }));
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.loading = false;
                    match outcome {
                        GrafanaOutcome::Catalog { panels, truncated } => {
                            this.grafana = ToolPresence::Ready;
                            this.panels = panels;
                            this.table.set_page(panels_page(&this.panels, truncated));
                        }
                        GrafanaOutcome::Absent => {
                            this.grafana = ToolPresence::Missing;
                            this.panels.clear();
                            this.table.set_page(TablePage::default());
                        }
                        // A blocked refresh clears the old catalog like the
                        // Absent arm: stale panels that still run would show
                        // an answer the status line says cannot be fetched.
                        GrafanaOutcome::Denied(what) => {
                            this.grafana = ToolPresence::Blocked;
                            this.panels.clear();
                            this.table.set_page(TablePage::default());
                            this.status = Some(format!("{what}: access denied for this account"));
                        }
                        GrafanaOutcome::Failed(why) => {
                            this.grafana = ToolPresence::Blocked;
                            this.panels.clear();
                            this.table.set_page(TablePage::default());
                            this.status = Some(why);
                        }
                    }
                    this.apply_absence(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn apply_absence(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        if self.grafana == ToolPresence::Missing
            && self.prometheus == ToolPresence::Missing
            && self.loki == ToolPresence::Missing
        {
            cx.emit(InventoryEvent::NotServed {
                tag: ItemTag::Observe,
                what: "Grafana, Loki, and Prometheus",
            });
        }
    }

    fn run_selected(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.selected_panel_index() else {
            return;
        };
        let panel = self.panels[index].clone();
        if panel.kind == GrafanaPanelKind::Unsupported || panel.expr.is_empty() {
            if let Some(url) = &panel.browser_url {
                self.status = Some(if browser_url_allowed(url) {
                    cx.open_url(url);
                    // "handed", not "opened": the launcher's success is not
                    // observable from here, and the old spawn-based claim
                    // reported success it never saw.
                    format!("handed {url} to the system browser")
                } else {
                    format!("refused to hand this URL to the system browser: {url}")
                });
            } else {
                self.status = Some(
                    "this panel needs Grafana's engine; no system-browser URL is bound".into(),
                );
            }
            cx.notify();
            return;
        }
        match panel.dialect {
            QueryDialect::LogQL => {
                if self.loki == ToolPresence::Missing {
                    self.status = Some("Loki is not in this cluster".into());
                    cx.notify();
                    return;
                }
                self.query = panel.expr;
                self.query_kind = QueryKind::Loki;
                self.run_loki(cx);
            }
            QueryDialect::PromQL | QueryDialect::Unknown => {
                if self.prometheus == ToolPresence::Missing {
                    self.status = Some("Prometheus is not in this cluster".into());
                    cx.notify();
                    return;
                }
                self.query = panel.expr;
                self.query_kind = QueryKind::Prom;
                self.run_prom(cx);
            }
            QueryDialect::TraceQL => {
                self.status = Some(
                    "TraceQL stays a trace-id lookup; open traces from the command palette".into(),
                );
                cx.notify();
            }
        }
    }

    fn run_query_box(&mut self, cx: &mut Context<Self>) {
        let expr = self.query.trim().to_string();
        if expr.is_empty() {
            self.status = Some("the query box is empty".into());
            cx.notify();
            return;
        }
        match self.query_kind {
            QueryKind::Prom => self.run_prom(cx),
            QueryKind::Loki => self.run_loki(cx),
        }
    }

    fn run_prom(&mut self, cx: &mut Context<Self>) {
        if self.prometheus == ToolPresence::Missing {
            self.status = Some("Prometheus is not in this cluster".into());
            cx.notify();
            return;
        }
        self.query_generation += 1;
        let generation = self.query_generation;
        let expr = self.query.clone();
        self.status = Some("running PromQL...".into());
        cx.notify();
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.query_promql(
            expr,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.query_generation != generation {
                        return;
                    }
                    match outcome {
                        PromOutcome::Series {
                            series,
                            truncated,
                            dropped_series,
                        } => {
                            this.results.set_lines(render_series(&series));
                            this.status = if truncated {
                                Some(format!(
                                    "Prometheus truncated the answer ({dropped_series} series dropped)"
                                ))
                            } else {
                                None
                            };
                        }
                        PromOutcome::Absent => {
                            this.prometheus = ToolPresence::Missing;
                            this.status = Some("Prometheus is not in this cluster".into());
                            this.apply_absence(cx);
                        }
                        PromOutcome::Denied(what) => {
                            this.status =
                                Some(format!("{what}: access denied for this account"));
                        }
                        PromOutcome::Failed(why) => this.status = Some(why),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn run_loki(&mut self, cx: &mut Context<Self>) {
        if self.loki == ToolPresence::Missing {
            self.status = Some("Loki is not in this cluster".into());
            cx.notify();
            return;
        }
        self.query_generation += 1;
        let generation = self.query_generation;
        let query = self.query.clone();
        self.status = Some("running LogQL...".into());
        cx.notify();
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.query_loki(
            query,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.query_generation != generation {
                        return;
                    }
                    match outcome {
                        LokiOutcome::Logs { lines, truncated } => {
                            this.results.set_lines(lines);
                            this.status = if truncated {
                                Some("Loki truncated the answer at its cap".into())
                            } else {
                                None
                            };
                        }
                        LokiOutcome::Absent => {
                            this.loki = ToolPresence::Missing;
                            this.status = Some("Loki is not in this cluster".into());
                            this.apply_absence(cx);
                        }
                        LokiOutcome::Denied(what) => {
                            this.status = Some(format!("{what}: access denied for this account"));
                        }
                        LokiOutcome::Failed(why) => this.status = Some(why),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn selected_panel_index(&self) -> Option<usize> {
        let uid = self.table.selected_row()?.uid.as_str();
        self.panels.iter().position(|panel| panel_uid(panel) == uid)
    }

    fn breadcrumb(&self) -> String {
        let mut crumb = format!("observe: {} queries", self.table.visible_rows());
        if self.loading {
            crumb.push_str("  loading...");
        }
        if self.grafana == ToolPresence::Missing {
            crumb.push_str("  grafana hidden");
        }
        if self.prometheus == ToolPresence::Missing {
            crumb.push_str("  promql hidden");
        }
        if self.loki == ToolPresence::Missing {
            crumb.push_str("  loki hidden");
        }
        if self.filtering {
            crumb.push_str(&format!("  filter: {}_", self.table.filter));
        } else if self.typing {
            crumb.push_str(&format!("  query: {}_", self.query));
        }
        crumb
    }

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let chrome = PANEL_HEADER_HEIGHT
            + TABLE_HEADER_HEIGHT
            + QUERY_HEIGHT
            + STATUS_BAR_HEIGHT
            + CONTENT_PADDING;
        let rows = self.viewport.rows(chrome, 0.0, LIST_ROW_HEIGHT, 400).max(4) / 2;
        self.table.set_viewport(rows.max(4));
        self.results.set_viewport(rows.max(4));
        cx.notify();
    }
}

impl crate::item::Item for ObserveView {
    fn title(&self) -> SharedString {
        ObserveView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        ObserveView::focus_handle(self)
    }
}

impl Render for ObserveView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let show_grafana = self.grafana != ToolPresence::Missing;
        let show_query =
            self.prometheus != ToolPresence::Missing || self.loki != ToolPresence::Missing;
        let query_label = match self.query_kind {
            QueryKind::Prom => "PromQL",
            QueryKind::Loki => "LogQL",
        };

        div()
            .id("observe-view")
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
                if this.grafana == ToolPresence::Missing {
                    this.results.scroll_by(-1);
                } else {
                    this.table.move_selection(-1);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                if this.grafana == ToolPresence::Missing {
                    this.results.scroll_by(1);
                } else {
                    this.table.move_selection(1);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageUp, _, cx| {
                if this.grafana == ToolPresence::Missing {
                    this.results.page_up();
                } else {
                    this.table.page_by(-1);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageDown, _, cx| {
                if this.grafana == ToolPresence::Missing {
                    this.results.page_down();
                } else {
                    this.table.page_by(1);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowHome, _, cx| {
                if this.grafana == ToolPresence::Missing {
                    this.results.home();
                } else {
                    this.table.select_first();
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowEnd, _, cx| {
                if this.grafana == ToolPresence::Missing {
                    this.results.end();
                } else {
                    this.table.select_last();
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| {
                if this.typing {
                    this.typing = false;
                    this.run_query_box(cx);
                } else {
                    this.run_selected(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.reload(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EnterFilter, _, cx| {
                this.filtering = true;
                this.typing = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                if this.typing {
                    this.typing = false;
                    this.run_query_box(cx);
                } else {
                    this.filtering = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                // Escape undoes only the mode being edited: cancelling the
                // query box must not wipe an unrelated table filter.
                if this.typing {
                    this.typing = false;
                } else {
                    this.filtering = false;
                    this.table.clear_filter();
                }
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
                    if keystroke.key == "q" && show_query_hint(this) {
                        this.typing = true;
                        if this.prometheus != ToolPresence::Missing {
                            this.query_kind = QueryKind::Prom;
                        } else {
                            this.query_kind = QueryKind::Loki;
                        }
                        cx.notify();
                    } else if keystroke.key == "tab" && show_query_hint(this) {
                        this.query_kind = match this.query_kind {
                            QueryKind::Prom if this.loki != ToolPresence::Missing => {
                                QueryKind::Loki
                            }
                            QueryKind::Loki if this.prometheus != ToolPresence::Missing => {
                                QueryKind::Prom
                            }
                            other => other,
                        };
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
            .children(show_grafana.then(|| {
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
                    .child(self.table.header_line())
            }))
            .children(show_grafana.then(|| {
                div()
                    .id("observe-rows")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .role(Role::ListBox)
                    .aria_label("Grafana queries")
                    .children(self.table.visible_lines().into_iter().enumerate().map(
                        |(offset, (selected, line))| {
                            let mut row = div()
                                .id(("observe-row", offset))
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
            }))
            .children(show_query.then(|| {
                div()
                    .h(px(QUERY_HEIGHT))
                    .flex_none()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .bg(rgb(theme.shell.editor_background))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .font_family(fonts.buffer_family.clone())
                    .text_size(px(fonts.small()))
                    .child(format!("{query_label}: "))
                    .child(if self.typing {
                        format!("{}_", self.query)
                    } else if self.query.is_empty() {
                        "q types a query · tab switches PromQL/LogQL".to_string()
                    } else {
                        self.query.clone()
                    })
            }))
            .child(
                div()
                    .id("observe-results")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .bg(rgb(theme.shell.editor_background))
                    .children(self.results.visible().map(|(index, line)| {
                        div()
                            .id(("observe-result", index))
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
                            .child(line.to_string())
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
                    .child("enter runs · q query · / filter · r refresh"),
            )
    }
}

fn show_query_hint(view: &ObserveView) -> bool {
    view.prometheus != ToolPresence::Missing || view.loki != ToolPresence::Missing
}

fn panel_uid(panel: &GrafanaPanelRow) -> String {
    format!("{}:{}:{}", panel.dashboard_uid, panel.panel_id, panel.title)
}

fn panels_page(panels: &[GrafanaPanelRow], truncated: bool) -> TablePage {
    let columns = ["Dashboard", "Panel", "Kind", "Dialect"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let rows = panels
        .iter()
        .map(|panel| TableRow {
            cells: vec![
                panel.dashboard_title.clone(),
                panel.title.clone(),
                kind_word(panel.kind).to_string(),
                dialect_word(panel.dialect).to_string(),
            ],
            name: panel.title.clone(),
            namespace: Some(panel.dashboard_title.clone()),
            uid: panel_uid(panel),
        })
        .collect();
    TablePage {
        columns,
        rows,
        truncated,
        continue_token: None,
    }
}

fn kind_word(kind: GrafanaPanelKind) -> &'static str {
    match kind {
        GrafanaPanelKind::Timeseries => "timeseries",
        GrafanaPanelKind::Stat => "stat",
        GrafanaPanelKind::Gauge => "gauge",
        GrafanaPanelKind::Table => "table",
        GrafanaPanelKind::Logs => "logs",
        GrafanaPanelKind::Heatmap => "heatmap",
        GrafanaPanelKind::Bar => "bar",
        GrafanaPanelKind::Unsupported => "unsupported",
    }
}

fn dialect_word(dialect: QueryDialect) -> &'static str {
    match dialect {
        QueryDialect::PromQL => "promql",
        QueryDialect::LogQL => "logql",
        QueryDialect::TraceQL => "traceql",
        QueryDialect::Unknown => "query",
    }
}

fn render_series(series: &[crate::provider::PromSeriesView]) -> Vec<String> {
    if series.is_empty() {
        return vec!["this PromQL returned no series".to_string()];
    }
    let mut lines = Vec::new();
    for item in series {
        let values: Vec<f64> = item.points.iter().map(|(_, value)| *value).collect();
        let last = values.last().copied();
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let last = last
            .map(|value| format!("{value:.4}"))
            .unwrap_or_else(|| "-".to_string());
        lines.push(format!(
            "{}  last={last}  min={min:.4}  max={max:.4}  {} pts  {}",
            item.labels,
            item.points.len(),
            spark_text(&values)
        ));
    }
    lines
}

fn spark_text(values: &[f64]) -> String {
    const BARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.len() < 2 {
        return String::new();
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(f64::EPSILON);
    values
        .iter()
        .step_by((values.len() / 48).max(1))
        .map(|value| {
            let t = ((*value - min) / span * (BARS.len() - 1) as f64).round() as usize;
            BARS[t.min(BARS.len() - 1)]
        })
        .collect()
}

/// Only an http(s) URL may reach the system browser. This gate sits in
/// front of `cx.open_url` because these strings are shaped by fetched
/// cluster objects, and gpui's launcher does no scheme filtering of its
/// own. (Launching itself is gpui's job — its platform paths avoid cmd.exe
/// re-parsing and reap the child, which the old hand-rolled spawn did not.)
pub(crate) fn browser_url_allowed(url: &str) -> bool {
    let http = url.starts_with("https://") || url.starts_with("http://");
    http && !url.chars().any(char::is_whitespace) && !url.contains('"')
}
