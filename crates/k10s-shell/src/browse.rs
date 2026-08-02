//! The browser: any discovered kind, as a list view.
//!
//! Two phases over one table machine -- pick a kind (filterable, with the
//! probe's verdict shown on the row), then read its server-rendered Table.
//! A forbidden kind stays openable and answers with the denial the server
//! actually returns; the picker's tag is a warning, not a wall. The node
//! capacity view is the same machine over a client-computed page. Enter
//! describes the selected row; `l` follows logs when the row is a pod.
//! Everything repaints on notify only.

use std::rc::Rc;

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Render, ScrollWheelEvent, SharedString, Styled, Window, div, prelude::*, px,
    rgb,
};

use crate::provider::{
    DescribeRequest, KindRow, ReadProvider, TableColumn, TableOutcome, TablePage, TableRow,
};
use crate::table::TableState;
use crate::{
    Back, CancelInput, CommitInput, DeleteInputChar, EnterFilter, LogsRow, OpenRow, Refresh,
    RowDown, RowEnd, RowHome, RowPageDown, RowPageUp, RowUp,
};

const BG: u32 = 0x0e0c17;
const TEXT: u32 = 0xcfcae6;
const DIM: u32 = 0x6e6890;
const HEAD: u32 = 0xb8b2d9;
const SELECTED_BG: u32 = 0x2c2842;
const STATUS: u32 = 0xb8b2d9;
const ROW_PX: f32 = 16.0;
const CHROME_PX: f32 = 110.0;
const MONO: &str = "JetBrains Mono";

enum TableSource {
    Kind(KindRow),
    Nodes,
}

enum Phase {
    Kinds,
    Table {
        source: TableSource,
        loading: bool,
        status: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseEvent {
    OpenDoc(DescribeRequest),
    OpenLogs { namespace: String, pod: String },
}

pub struct BrowseView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    kinds: Vec<KindRow>,
    table: TableState,
    phase: Phase,
    filtering: bool,
    generation: u64,
}

impl EventEmitter<BrowseEvent> for BrowseView {}

impl BrowseView {
    pub fn kinds(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> BrowseView {
        let mut view = BrowseView {
            focus: cx.focus_handle(),
            kinds: provider.kinds(),
            provider,
            table: TableState::new(),
            phase: Phase::Kinds,
            filtering: false,
            generation: 0,
        };
        view.table.set_page(kinds_page(&view.kinds));
        view
    }

    pub fn nodes(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> BrowseView {
        let mut view = BrowseView {
            focus: cx.focus_handle(),
            kinds: provider.kinds(),
            provider,
            table: TableState::new(),
            phase: Phase::Table {
                source: TableSource::Nodes,
                loading: true,
                status: None,
            },
            filtering: false,
            generation: 0,
        };
        view.fetch(cx);
        view
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn title(&self) -> SharedString {
        match &self.phase {
            Phase::Kinds => "browse".into(),
            Phase::Table { source, .. } => match source {
                TableSource::Kind(kind) => format!("browse {}", kind.display).into(),
                TableSource::Nodes => "nodes".into(),
            },
        }
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = futures::channel::oneshot::channel();
        match &mut self.phase {
            Phase::Kinds => {
                self.kinds = self.provider.kinds();
                self.table.set_page(kinds_page(&self.kinds));
                return;
            }
            Phase::Table {
                source,
                loading,
                status,
            } => {
                *loading = true;
                *status = None;
                let reply = Box::new(move |outcome| {
                    let _ = tx.send(outcome);
                });
                match source {
                    TableSource::Kind(kind) => self.provider.fetch_table(kind.id, reply),
                    TableSource::Nodes => self.provider.fetch_node_table(reply),
                }
            }
        }
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    if let Phase::Table {
                        loading, status, ..
                    } = &mut this.phase
                    {
                        *loading = false;
                        match outcome {
                            TableOutcome::Table(page) => this.table.set_page(page),
                            TableOutcome::Denied(what) => {
                                this.table.set_page(TablePage::default());
                                *status = Some(format!("{what}: access denied for this account"));
                            }
                            TableOutcome::Failed(why) => {
                                this.table.set_page(TablePage::default());
                                *status = Some(why);
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        match &self.phase {
            Phase::Kinds => {
                let Some(row) = self.table.selected_row() else {
                    return;
                };
                let Some(kind) = self.kinds.iter().find(|k| k.display == row.uid) else {
                    return;
                };
                self.phase = Phase::Table {
                    source: TableSource::Kind(kind.clone()),
                    loading: true,
                    status: None,
                };
                self.table = TableState::new();
                self.fetch(cx);
                cx.notify();
            }
            Phase::Table { source, .. } => {
                let Some(row) = self.table.selected_row() else {
                    return;
                };
                let request = match source {
                    TableSource::Kind(kind) => describe_request(kind.id, row),
                    TableSource::Nodes => {
                        let Some(node_kind) = self.kinds.iter().find(|k| k.kind == "Node") else {
                            if let Phase::Table { status, .. } = &mut self.phase {
                                *status = Some(
                                    "this cluster does not serve Node, so a node cannot be \
                                     described"
                                        .to_string(),
                                );
                            }
                            cx.notify();
                            return;
                        };
                        describe_request(node_kind.id, row)
                    }
                };
                cx.emit(BrowseEvent::OpenDoc(request));
            }
        }
    }

    fn logs_selected(&mut self, cx: &mut Context<Self>) {
        let Phase::Table {
            source: TableSource::Kind(kind),
            ..
        } = &self.phase
        else {
            return;
        };
        if kind.kind != "Pod" {
            return;
        }
        let Some(row) = self.table.selected_row() else {
            return;
        };
        let Some(namespace) = row.namespace.clone() else {
            return;
        };
        cx.emit(BrowseEvent::OpenLogs {
            namespace,
            pod: row.name.clone(),
        });
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        match &self.phase {
            Phase::Table { .. } => {
                self.generation += 1;
                self.phase = Phase::Kinds;
                self.table = TableState::new();
                self.table.set_page(kinds_page(&self.kinds));
                cx.notify();
            }
            Phase::Kinds => cx.propagate(),
        }
    }

    fn breadcrumb(&self) -> String {
        let mut crumb = match &self.phase {
            Phase::Kinds => format!("browse: {} kinds", self.table.visible_rows()),
            Phase::Table {
                source: TableSource::Kind(kind),
                loading,
                ..
            } => {
                let mut text = format!(
                    "browse > {}: {} of {} rows",
                    kind.display,
                    self.table.visible_rows(),
                    self.table.total_rows(),
                );
                if kind.forbidden {
                    text.push_str("  (the probe says this account cannot read it)");
                }
                if *loading {
                    text.push_str("  loading...");
                }
                text
            }
            Phase::Table { loading, .. } => {
                let mut text = format!("nodes: {} rows", self.table.visible_rows());
                if *loading {
                    text.push_str("  loading...");
                }
                text
            }
        };
        if self.table.truncated() {
            crumb.push_str("  (first page only; the list is larger)");
        }
        if self.filtering {
            crumb.push_str(&format!("  filter: {}_", self.table.filter));
        } else if !self.table.filter.is_empty() {
            crumb.push_str(&format!("  filter: {}", self.table.filter));
        }
        crumb
    }
}

fn describe_request(kind: k10s_core::KindId, row: &TableRow) -> DescribeRequest {
    DescribeRequest {
        kind,
        namespace: row.namespace.clone(),
        name: row.name.clone(),
        uid: row.uid.clone(),
    }
}

fn kinds_page(kinds: &[KindRow]) -> TablePage {
    let columns = ["Resource", "Kind", "Scope", "Access"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let rows = kinds
        .iter()
        .map(|kind| TableRow {
            cells: vec![
                kind.display.clone(),
                kind.kind.clone(),
                if kind.namespaced {
                    "namespaced".to_string()
                } else {
                    "cluster".to_string()
                },
                if kind.forbidden {
                    "forbidden".to_string()
                } else {
                    String::new()
                },
            ],
            name: kind.display.clone(),
            namespace: None,
            uid: kind.display.clone(),
        })
        .collect();
    TablePage {
        columns,
        rows,
        truncated: false,
    }
}

impl Render for BrowseView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = f32::from(window.viewport_size().height);
        let rows = (((height - CHROME_PX) / ROW_PX) as usize).clamp(4, 400);
        self.table.set_viewport(rows);

        let empty_hint = match &self.phase {
            Phase::Kinds if self.kinds.is_empty() => {
                Some("no kinds to browse; no cluster connected")
            }
            _ => None,
        };
        let status = match &self.phase {
            Phase::Table { status, .. } => status.clone(),
            Phase::Kinds => None,
        };

        div()
            .key_context(if self.filtering { "Typing" } else { "Browse" })
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .font_family(MONO)
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
                this.open_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &LogsRow, _, cx| {
                this.logs_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.fetch(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EnterFilter, _, cx| {
                this.filtering = true;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Back, _, cx| {
                this.back(cx);
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                this.filtering = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                this.filtering = false;
                this.table.clear_filter();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                this.table.pop_filter();
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !this.filtering {
                    return;
                }
                let keystroke = &event.keystroke;
                if keystroke.modifiers.control || keystroke.modifiers.alt {
                    return;
                }
                if let Some(key_char) = &keystroke.key_char {
                    this.table.push_filter(key_char);
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = f32::from(event.delta.pixel_delta(px(ROW_PX)).y);
                this.table.move_selection(-(delta / ROW_PX) as i64);
                cx.notify();
            }))
            .child(
                div()
                    .h(px(22.0))
                    .px(px(8.0))
                    .text_size(px(11.0))
                    .text_color(rgb(STATUS))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(self.breadcrumb()),
            )
            .child(
                div()
                    .h(px(ROW_PX))
                    .px(px(8.0))
                    .text_size(px(11.0))
                    .text_color(rgb(HEAD))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(self.table.header_line()),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .children(self.table.visible_lines().into_iter().enumerate().map(
                        |(offset, (selected, line))| {
                            let mut row = div()
                                .h(px(ROW_PX))
                                .px(px(8.0))
                                .text_size(px(11.0))
                                .text_color(rgb(TEXT))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                        this.table.select_visible_offset(offset);
                                        cx.notify();
                                    }),
                                );
                            if selected {
                                row = row.bg(rgb(SELECTED_BG));
                            }
                            row.child(line)
                        },
                    ))
                    .children(empty_hint.map(|hint| {
                        div()
                            .p(px(8.0))
                            .text_size(px(11.0))
                            .text_color(rgb(DIM))
                            .child(hint)
                    }))
                    .children(status.map(|status| {
                        div()
                            .p(px(8.0))
                            .text_size(px(11.0))
                            .text_color(rgb(STATUS))
                            .child(status)
                    })),
            )
            .child(
                div()
                    .h(px(20.0))
                    .px(px(8.0))
                    .text_size(px(10.0))
                    .text_color(rgb(DIM))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(match &self.phase {
                        Phase::Kinds => {
                            "enter open · / filter · r refresh · esc close filter".to_string()
                        }
                        Phase::Table { source, .. } => {
                            let mut hints =
                                "enter describe · / filter · r refresh · esc back".to_string();
                            if let TableSource::Kind(kind) = source
                                && kind.kind == "Pod"
                            {
                                hints.push_str(" · l logs");
                            }
                            hints
                        }
                    }),
            )
    }
}
