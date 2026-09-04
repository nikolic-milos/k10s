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
    ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window, canvas, div,
    prelude::*, px, rgb,
};

use crate::provider::{
    DescribeRequest, ForwardRequest, KindRow, ReadProvider, TableColumn, TableOutcome, TablePage,
    TableRow,
};
use crate::table::TableState;
use crate::ui::{LIST_ROW_HEIGHT, PANEL_HEADER_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{
    Back, CancelInput, CommitInput, DeleteInputChar, EnterFilter, ExecRow, LoadMore, LogsRow,
    OpenRow, Refresh, RowDown, RowEnd, RowHome, RowPageDown, RowPageUp, RowUp, StartForward,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;
const VIEW_CHROME_HEIGHT: f32 = PANEL_HEADER_HEIGHT + TABLE_HEADER_HEIGHT + STATUS_BAR_HEIGHT;

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
    OpenEdit(DescribeRequest),
    OpenLogs {
        namespace: String,
        pod: String,
    },
    OpenWorkloadLogs {
        namespace: String,
        kind: k10s_core::KindId,
        name: String,
    },
    OpenLocalCommand(LocalCommand),
    StartForward(ForwardRequest),
    OpenExec {
        namespace: String,
        pod: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCommand {
    pub title: String,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum TalosRead {
    Dmesg,
    Services,
}

impl TalosRead {
    fn command(self) -> &'static str {
        match self {
            TalosRead::Dmesg => "dmesg",
            TalosRead::Services => "service",
        }
    }
}

// The built-in kinds whose spec.selector selects pods, so a merged log
// follow can find them. A CronJob's pods hang off its Jobs, not a selector.
fn selects_pods(kind: &str) -> bool {
    matches!(
        kind,
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job"
    )
}

/// Which row actions a kind offers, stated once so the key that fires and the
/// hint that advertises it cannot disagree. A shell needs a tty, so a pod;
/// logs need pods to read, so a pod or a kind whose selector finds them; a
/// forward needs a port, so a pod directly or a service through its selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowActions {
    pub(crate) logs: bool,
    pub(crate) shell: bool,
    pub(crate) forward: bool,
}

pub(crate) fn row_actions(kind: &str) -> RowActions {
    RowActions {
        logs: kind == "Pod" || selects_pods(kind),
        shell: kind == "Pod",
        forward: kind == "Pod" || kind == "Service",
    }
}

pub struct BrowseView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    kinds: Vec<KindRow>,
    table: TableState,
    phase: Phase,
    filtering: bool,
    generation: u64,
    viewport: Viewport,
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
            viewport: Viewport::default(),
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
            viewport: Viewport::default(),
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
                    TableSource::Kind(kind) => self.provider.fetch_table(kind.id, None, reply),
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
                            TableOutcome::Absent => {
                                this.table.set_page(TablePage::default());
                                *status = Some("not served by this cluster".to_string());
                            }
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

    // The explicit next page: only a kind table can continue (the node table
    // has no token), only when the server offered one, and never while a
    // fetch is already in flight.
    fn fetch_more(&mut self, cx: &mut Context<Self>) {
        let Phase::Table {
            source: TableSource::Kind(kind),
            loading: loading @ false,
            ..
        } = &mut self.phase
        else {
            return;
        };
        let Some(token) = self.table.continue_token().map(str::to_string) else {
            return;
        };
        *loading = true;
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_table(
            kind.id,
            Some(token),
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
                    if let Phase::Table {
                        loading, status, ..
                    } = &mut this.phase
                    {
                        *loading = false;
                        match outcome {
                            TableOutcome::Table(page) => this.table.append_page(page),
                            TableOutcome::Absent => {
                                *status = Some("not served by this cluster".to_string());
                            }
                            TableOutcome::Denied(what) => {
                                *status = Some(format!("{what}: access denied for this account"));
                            }
                            TableOutcome::Failed(why) => *status = Some(why),
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

    fn edit_selected(&mut self, cx: &mut Context<Self>) {
        let Phase::Table {
            source: TableSource::Kind(kind),
            ..
        } = &self.phase
        else {
            return;
        };
        let Some(row) = self.table.selected_row() else {
            return;
        };
        cx.emit(BrowseEvent::OpenEdit(describe_request(kind.id, row)));
    }

    fn logs_selected(&mut self, cx: &mut Context<Self>) {
        let Phase::Table {
            source: TableSource::Kind(kind),
            ..
        } = &self.phase
        else {
            return;
        };
        let Some(row) = self.table.selected_row() else {
            return;
        };
        let Some(namespace) = row.namespace.clone() else {
            return;
        };
        if !row_actions(&kind.kind).logs {
            return;
        }
        if kind.kind == "Pod" {
            cx.emit(BrowseEvent::OpenLogs {
                namespace,
                pod: row.name.clone(),
            });
        } else {
            cx.emit(BrowseEvent::OpenWorkloadLogs {
                namespace,
                kind: kind.id,
                name: row.name.clone(),
            });
        }
    }

    // A shell opens into a pod row; other kinds have no tty to offer.
    fn exec_selected(&mut self, cx: &mut Context<Self>) {
        let Phase::Table {
            source: TableSource::Kind(kind),
            ..
        } = &self.phase
        else {
            return;
        };
        if !row_actions(&kind.kind).shell {
            return;
        }
        let Some(row) = self.table.selected_row() else {
            return;
        };
        let Some(namespace) = row.namespace.clone() else {
            return;
        };
        cx.emit(BrowseEvent::OpenExec {
            namespace,
            pod: row.name.clone(),
        });
    }

    // A forward starts from what a row names: a pod directly, a service
    // through its selector. Anything else has no port to offer.
    fn forward_selected(&mut self, cx: &mut Context<Self>) {
        let Phase::Table {
            source: TableSource::Kind(kind),
            ..
        } = &self.phase
        else {
            return;
        };
        if !row_actions(&kind.kind).forward {
            return;
        }
        let Some(row) = self.table.selected_row() else {
            return;
        };
        let Some(namespace) = row.namespace.clone() else {
            return;
        };
        cx.emit(BrowseEvent::StartForward(ForwardRequest {
            namespace,
            name: row.name.clone(),
            service: kind.kind == "Service",
        }));
    }

    fn talos_selected(&mut self, read: TalosRead, cx: &mut Context<Self>) {
        let Phase::Table {
            source: TableSource::Nodes,
            status,
            ..
        } = &mut self.phase
        else {
            return;
        };
        let Some(row) = self.table.selected_row() else {
            return;
        };
        let os = self.table.selected_cell("OS").unwrap_or_default();
        if !contains_ascii_folded(os, "talos") {
            return;
        }
        let address = self.table.selected_cell("Address").unwrap_or_default();
        if address.is_empty() {
            *status = Some("this Talos node reports no reachable node address".to_string());
            cx.notify();
            return;
        }
        if !talosctl_available() {
            *status =
                Some("talosctl is not on PATH; install it to open machine diagnostics".to_string());
            cx.notify();
            return;
        }
        cx.emit(BrowseEvent::OpenLocalCommand(talos_command(
            row.name.as_str(),
            address,
            read,
        )));
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
        if self.table.capped() {
            crumb.push_str(&format!(
                "  (holding the first {} rows; filter to narrow)",
                crate::table::MAX_ROWS
            ));
        } else if self.table.continue_token().is_some() {
            crumb.push_str("  (more on the server; m loads the next page)");
        } else if self.table.truncated() {
            crumb.push_str("  (first page only; the list is larger)");
        }
        if self.filtering {
            crumb.push_str(&format!("  filter: {}_", self.table.filter));
        } else if !self.table.filter.is_empty() {
            crumb.push_str(&format!("  filter: {}", self.table.filter));
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
        continue_token: None,
    }
}

impl crate::item::Item for BrowseView {
    fn title(&self) -> SharedString {
        BrowseView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        BrowseView::focus_handle(self)
    }
}

impl Render for BrowseView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();

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
            .id("browse-view")
            .key_context(if self.filtering { "Typing" } else { "Browse" })
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
                this.open_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &LogsRow, _, cx| {
                this.logs_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::EditRow, _, cx| {
                this.edit_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &StartForward, _, cx| {
                this.forward_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &ExecRow, _, cx| {
                this.exec_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::TalosDmesg, _, cx| {
                this.talos_selected(TalosRead::Dmesg, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::TalosServices, _, cx| {
                this.talos_selected(TalosRead::Services, cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.fetch(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &LoadMore, _, cx| {
                this.fetch_more(cx);
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
                let delta = f32::from(event.delta.pixel_delta(px(LIST_ROW_HEIGHT)).y);
                this.table
                    .move_selection(-(delta / LIST_ROW_HEIGHT).round() as i64);
                cx.notify();
            }))
            .child(panel_header(&theme, &fonts, self.breadcrumb()))
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
                    .id("browse-rows")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .role(Role::ListBox)
                    .aria_label("Resources")
                    .children(self.table.visible_lines().into_iter().enumerate().map(
                        |(offset, (selected, line))| {
                            let mut row = div()
                                .id(("browse-row", offset))
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
                    .children(empty_hint.map(|hint| {
                        div()
                            .p(px(12.0))
                            .text_size(px(fonts.ui_size))
                            .text_color(rgb(theme.shell.text_muted))
                            .child(hint)
                    }))
                    .children(status.map(|status| {
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
                    .child(match &self.phase {
                        Phase::Kinds => {
                            "enter open · / filter · r refresh · esc close filter".to_string()
                        }
                        Phase::Table { source, .. } => {
                            let mut hints =
                                "enter describe · / filter · r refresh · esc back".to_string();
                            if self.table.continue_token().is_some() {
                                hints.push_str(" · m more");
                            }
                            if let TableSource::Kind(kind) = source {
                                hints.push_str(" · y edit");
                                let actions = row_actions(&kind.kind);
                                if actions.logs {
                                    hints.push_str(" · l logs");
                                }
                                if actions.shell {
                                    hints.push_str(" · s shell");
                                }
                                if actions.forward {
                                    hints.push_str(" · F forward");
                                }
                            } else if self
                                .table
                                .selected_cell("OS")
                                .is_some_and(|os| contains_ascii_folded(os, "talos"))
                            {
                                hints.push_str(" · D dmesg · S services");
                            }
                            hints
                        }
                    }),
            )
    }
}

fn talos_command(node: &str, address: &str, read: TalosRead) -> LocalCommand {
    LocalCommand {
        title: format!("talos {} {node}", read.command()),
        program: "talosctl".to_string(),
        args: vec![
            "--nodes".to_string(),
            address.to_string(),
            read.command().to_string(),
        ],
    }
}

fn contains_ascii_folded(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(unix)]
fn talosctl_available() -> bool {
    crate::pty::command_on_path("talosctl")
}

#[cfg(not(unix))]
fn talosctl_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talos_commands_keep_the_node_target_as_one_argv_value() {
        let command = talos_command(
            "control-plane-1",
            "10.0.0.7;printf should-not-run",
            TalosRead::Dmesg,
        );

        assert_eq!(command.program, "talosctl");
        assert_eq!(
            command.args,
            ["--nodes", "10.0.0.7;printf should-not-run", "dmesg"]
        );
        assert!(!command.args.iter().any(|arg| arg == "-c"));
    }

    #[test]
    fn service_inventory_uses_the_read_only_singular_cli_command() {
        let command = talos_command("worker-1", "10.0.0.8", TalosRead::Services);
        assert_eq!(command.args, ["--nodes", "10.0.0.8", "service"]);
    }

    // The policy the keys and the hints both read. A change here is a change
    // to what a person can do from a row, and it must not happen twice.
    #[test]
    fn row_actions_follow_what_a_kind_can_answer() {
        let all = RowActions {
            logs: true,
            shell: true,
            forward: true,
        };
        let none = RowActions {
            logs: false,
            shell: false,
            forward: false,
        };
        assert_eq!(row_actions("Pod"), all);
        for workload in [
            "Deployment",
            "StatefulSet",
            "DaemonSet",
            "ReplicaSet",
            "Job",
        ] {
            assert_eq!(
                row_actions(workload),
                RowActions { logs: true, ..none },
                "{workload} selects pods, so its logs merge; nothing else applies"
            );
        }
        assert_eq!(
            row_actions("Service"),
            RowActions {
                forward: true,
                ..none
            },
            "a service forwards through its selector and offers nothing else"
        );
        for other in [
            "CronJob",
            "ConfigMap",
            "Secret",
            "Namespace",
            "Node",
            "Ingress",
        ] {
            assert_eq!(row_actions(other), none, "{other}");
        }
    }
}
