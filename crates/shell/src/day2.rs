//! Day-2 operations as a native table: scale, rollout, cordon, drain, delete.
//!
//! The first enter asks what the press would do and never writes (a drain's
//! ask reads the node's pods). The second enter, on the same row, is the
//! confirmation, and it sends one write: `Latch` spends the confirmation
//! when it is taken and refuses every press until the answer lands, for as
//! long as this view lives. Caps still gate the far side. Rollout undo is not
//! offered: that would be a fake of kubectl and of Helm.

use std::rc::Rc;

use gpui::{
    Context, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Render, Role, ScrollWheelEvent, SharedString, Styled, Window, canvas, div, prelude::*, px, rgb,
};

use crate::confirm::{Latch, Press, Slot, StatusText, nothing_listed, write_target};
use crate::selection::Selection;
use crate::table::TableState;
use crate::ui::{LIST_ROW_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header, status_slot};
use crate::{
    Back, CancelInput, CommitInput, DeleteInputChar, EnterFilter, OpenRow, Refresh, RowDown,
    RowEnd, RowHome, RowPageDown, RowPageUp, RowUp,
};
use k10s_provider::{
    ClusterProvider, Day2Op, Day2Outcome, Day2Request, KindRow, TableColumn, TableOutcome,
    TablePage, TableRow,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Day2Target {
    pub kind_id: k10s_core::KindId,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
    pub current_replicas: Option<i32>,
}

// What a confirmation sends: the reviewed request, the prefix its answer is
// named by, and the ask's own summary, which an Applied answer that only
// repeats it is compared with.
struct Confirmed {
    request: Day2Request,
    prefix: SharedString,
    asked: String,
}

enum Phase {
    Targets {
        loading: bool,
    },
    Ops {
        target: Day2Target,
        ops: Vec<(String, Day2Op)>,
    },
}

pub struct Day2View {
    focus: FocusHandle,
    provider: Rc<dyn ClusterProvider>,
    // Which kubeconfig context these writes land in, for the confirm sentence.
    context: Option<String>,
    kinds: Vec<KindRow>,
    table: TableState,
    phase: Phase,
    status: Option<SharedString>,
    // A target kind that could not be listed. Its own line, so a write's
    // answer landing after Esc cannot erase the fact that the list is partial.
    targets_label: Option<SharedString>,
    latch: Latch<Confirmed>,
    status_text: StatusText,
    filtering: bool,
    generation: u64,
    pending: u32,
    collected: Vec<Day2Target>,
    viewport: Viewport,
}

impl Day2View {
    pub fn new(
        provider: Rc<dyn ClusterProvider>,
        selection: Option<&Selection>,
        context: Option<String>,
        cx: &mut Context<Self>,
    ) -> Day2View {
        let kinds = provider.kinds();
        let mut view = Day2View {
            focus: cx.focus_handle(),
            provider,
            context,
            kinds,
            table: TableState::new(),
            phase: Phase::Targets { loading: true },
            status: None,
            targets_label: None,
            latch: Latch::Idle,
            status_text: StatusText::default(),
            filtering: false,
            generation: 0,
            pending: 0,
            collected: Vec::new(),
            viewport: Viewport::default(),
        };
        if let Some(target) = selection.and_then(|sel| target_from_selection(sel, &view.kinds)) {
            view.show_ops(target);
        } else {
            view.fetch_targets(cx);
        }
        view
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn title(&self) -> SharedString {
        "day-2".into()
    }

    fn show_ops(&mut self, target: Day2Target) {
        let ops = ops_for(&target.kind, target.current_replicas);
        self.table = TableState::new();
        self.table.set_page(ops_page(&target, &ops));
        self.phase = Phase::Ops { target, ops };
        self.latch.disarm();
        // While a write is out every press is refused, so the way to ask would
        // be a false instruction; the latch's own line says what is happening.
        self.status = (!matches!(self.latch, Latch::Sent { .. }))
            .then(|| "enter asks; enter again confirms. nothing is written until then".into());
    }

    fn fetch_targets(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.collected.clear();
        self.latch.disarm();
        self.status = None;
        self.targets_label = None;
        self.phase = Phase::Targets { loading: true };
        self.table = TableState::new();

        let workloads: Vec<KindRow> = self
            .kinds
            .iter()
            .filter(|kind| {
                matches!(
                    kind.kind.as_str(),
                    "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet"
                )
            })
            .cloned()
            .collect();
        self.pending = 1 + workloads.len() as u32;

        self.start_nodes(generation, cx);
        for kind in workloads {
            self.start_kind(kind, generation, cx);
        }
    }

    fn start_nodes(&mut self, generation: u64, cx: &mut Context<Self>) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_node_table(Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }));
        let node_id = self
            .kinds
            .iter()
            .find(|kind| kind.kind == "Node")
            .map(|kind| kind.id)
            .unwrap_or(k10s_core::KindId::NODE);
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.absorb_targets(generation, "Node", node_id, outcome, cx);
                });
            }
        })
        .detach();
    }

    fn start_kind(&mut self, kind: KindRow, generation: u64, cx: &mut Context<Self>) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_table(
            kind.id,
            None,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.absorb_targets(generation, &kind.kind, kind.id, outcome, cx);
                });
            }
        })
        .detach();
    }

    fn absorb_targets(
        &mut self,
        generation: u64,
        kind_name: &str,
        kind_id: k10s_core::KindId,
        outcome: TableOutcome,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation {
            return;
        }
        match outcome {
            TableOutcome::Table(page) => {
                self.collected
                    .extend(targets_from_page(kind_name, kind_id, &page));
            }
            TableOutcome::Denied(what) => {
                let line = format!("{what}: access denied for this account");
                self.targets_label = Some(unlisted(self.targets_label.take(), line));
            }
            TableOutcome::Failed(why) => {
                let line = format!("{kind_name}: {why}");
                self.targets_label = Some(unlisted(self.targets_label.take(), line));
            }
            TableOutcome::Absent => {}
        }
        self.pending = self.pending.saturating_sub(1);
        if self.pending == 0 {
            if let Phase::Targets { loading } = &mut self.phase {
                *loading = false;
            }
            self.table.set_page(targets_page(&self.collected));
        }
        cx.notify();
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        match &self.phase {
            Phase::Targets { .. } => {
                let Some(row) = self.table.selected_row() else {
                    return;
                };
                let Some(target) = self
                    .collected
                    .iter()
                    .find(|target| target_uid(target) == row.uid)
                    .cloned()
                else {
                    return;
                };
                self.show_ops(target);
                cx.notify();
            }
            Phase::Ops { target, ops } => {
                let Some(row) = self.table.selected_row() else {
                    return;
                };
                let Some((_, op)) = ops.iter().find(|(name, _)| name == &row.uid) else {
                    return;
                };
                let request = Day2Request {
                    kind: target.kind_id,
                    namespace: target.namespace.clone(),
                    name: target.name.clone(),
                    op: op.clone(),
                    confirm: false,
                };
                let subject = subject_of(target, &row.uid);
                match self.latch.press(&subject) {
                    Press::Ask => {
                        self.status = None;
                        self.run(request, subject, None, cx);
                    }
                    Press::Go(Confirmed {
                        request,
                        prefix,
                        asked,
                    }) => {
                        self.status = None;
                        self.run(request, subject, Some((prefix, asked)), cx);
                    }
                    Press::Refused => {}
                }
            }
        }
    }

    // Which subject the selection is on, in the latch's terms: the target and
    // the op, so an arm cannot survive a change of either.
    fn selected_subject(&self) -> Option<String> {
        let Phase::Ops { target, .. } = &self.phase else {
            return None;
        };
        let row = self.table.selected_row()?;
        Some(subject_of(target, &row.uid))
    }

    // `confirmed` is a write's prefix and its ask's summary, and is what makes
    // this a write; an ask carries none. Where the reply goes is [`landing`]'s.
    fn run(
        &mut self,
        request: Day2Request,
        subject: String,
        confirmed: Option<(SharedString, String)>,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        let asked = self.generation;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.run_day2(
            &request,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            let answer = rx.await.ok();
            let _ = this.update(cx, |this, cx| {
                let selected = this.selected_subject();
                let write = confirmed
                    .as_ref()
                    .map(|(prefix, asked)| (prefix.as_ref(), asked.as_str()));
                match landing(
                    write,
                    asked == this.generation,
                    selected.as_deref() == Some(subject.as_str()),
                    answer,
                    &write_target(this.context.as_deref()),
                ) {
                    Landing::Dropped => return,
                    Landing::Settled(line) => {
                        this.latch.settle(selected.as_deref());
                        this.status = Some(line.into());
                    }
                    Landing::Arm { prefix, asked } => {
                        let (armed, sent) = (armed_line(&prefix), sent_line(&prefix));
                        let confirmed = Confirmed {
                            request: Day2Request {
                                confirm: true,
                                ..request
                            },
                            prefix: prefix.into(),
                            asked,
                        };
                        this.latch
                            .arm(subject, confirmed, armed.into(), sent.into());
                    }
                    Landing::Said(line) => this.status = Some(line.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        match &self.phase {
            Phase::Ops { .. } => {
                self.generation += 1;
                self.latch.disarm();
                self.phase = Phase::Targets { loading: false };
                self.table = TableState::new();
                self.table.set_page(targets_page(&self.collected));
                self.status = None;
                if self.collected.is_empty() {
                    self.fetch_targets(cx);
                }
                cx.notify();
            }
            Phase::Targets { .. } => cx.propagate(),
        }
    }

    fn breadcrumb(&self) -> String {
        let mut crumb = match &self.phase {
            Phase::Targets { loading } => {
                let mut text = format!("day-2: {} targets", self.table.visible_rows());
                if *loading {
                    text.push_str("  loading...");
                }
                text
            }
            Phase::Ops { target, .. } => {
                let where_ = match &target.namespace {
                    Some(namespace) => format!("{}/{}", namespace, target.name),
                    None => target.name.clone(),
                };
                format!(
                    "day-2 > {} {where_}: {} ops",
                    target.kind,
                    self.table.visible_rows()
                )
            }
        };
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
        let rows = self.viewport.rows(0.0, 0.0, LIST_ROW_HEIGHT, 400);
        self.table.set_viewport(rows);
        cx.notify();
    }
}

pub fn target_from_selection(selection: &Selection, kinds: &[KindRow]) -> Option<Day2Target> {
    let kind = kinds
        .iter()
        .find(|kind| kind.id == selection.kind_id)
        .map(|kind| kind.kind.clone())
        .unwrap_or_else(|| selection.kind.to_string());
    Some(Day2Target {
        kind_id: selection.kind_id,
        kind,
        namespace: selection.namespace.as_deref().map(str::to_string),
        name: selection.name.to_string(),
        current_replicas: None,
    })
}

pub fn ops_for(kind: &str, current: Option<i32>) -> Vec<(String, Day2Op)> {
    let mut ops = Vec::new();
    let scalable = matches!(
        kind,
        "Deployment" | "StatefulSet" | "ReplicaSet" | "deploy" | "sts"
    );
    let restartable = matches!(
        kind,
        "Deployment" | "StatefulSet" | "DaemonSet" | "deploy" | "sts" | "ds"
    );
    let deployment = matches!(kind, "Deployment" | "deploy");
    let node = matches!(kind, "Node" | "node");
    let pod = matches!(kind, "Pod" | "pod");

    if scalable {
        if let Some(current) = current {
            if current != 0 {
                ops.push((
                    "scale to 0".into(),
                    Day2Op::Scale {
                        current,
                        replicas: 0,
                    },
                ));
            }
            if current != 1 {
                ops.push((
                    "scale to 1".into(),
                    Day2Op::Scale {
                        current,
                        replicas: 1,
                    },
                ));
            }
        }
    }
    if restartable {
        ops.push(("restart".into(), Day2Op::Restart));
    }
    if deployment {
        ops.push(("pause".into(), Day2Op::Pause));
        ops.push(("resume".into(), Day2Op::Resume));
    }
    if node {
        ops.push((
            "cordon".into(),
            Day2Op::Cordon {
                unschedulable: true,
            },
        ));
        ops.push((
            "uncordon".into(),
            Day2Op::Cordon {
                unschedulable: false,
            },
        ));
        ops.push(("drain".into(), Day2Op::Drain { force: false }));
    }
    if pod {
        ops.push(("evict".into(), Day2Op::Evict));
        ops.push(("debug".into(), Day2Op::Debug));
    }
    ops.push(("delete".into(), Day2Op::Delete));
    ops
}

// The latch's subject: the target and the op together, so an arm cannot survive
// a change of either.
fn subject_of(target: &Day2Target, op: &str) -> String {
    format!("{}/{op}", target_uid(target))
}

/// Where a day-2 reply goes, decided from data alone. `write` is a confirmed
/// write's prefix and its ask's summary, None for an ask; `current` is whether
/// nothing newer was asked since; `still_selected` whether the row that pressed
/// is still the one selected; `answer` is None for a reply dropped unsent.
/// A write's answer always lands, whatever the view moved to, because the
/// write happened or may have. An ask's answer lands only while it is the
/// newest ask and its row is still selected, so nothing is armed, or said, for
/// a row the person left.
#[derive(Debug, PartialEq, Eq)]
enum Landing {
    Dropped,
    /// Settle the latch with the selection and show this.
    Settled(String),
    /// Arm the row, the confirmation named by `prefix`.
    Arm {
        prefix: String,
        asked: String,
    },
    Said(String),
}

fn landing(
    write: Option<(&str, &str)>,
    current: bool,
    still_selected: bool,
    answer: Option<Day2Outcome>,
    target: &str,
) -> Landing {
    if let Some((prefix, asked)) = write {
        return Landing::Settled(match answer {
            Some(outcome) => answered_line(prefix, asked, outcome),
            None => dropped_write_line(prefix),
        });
    }
    if !current || !still_selected {
        return Landing::Dropped;
    }
    match answer {
        // The confirm names the cluster as well as the object: the blast
        // radius of a day-2 write is the operation and where it lands.
        Some(Day2Outcome::NeedsConfirm { summary }) => Landing::Arm {
            prefix: format!("{summary} in {target}"),
            asked: summary,
        },
        Some(outcome) => Landing::Said(outcome_text(outcome)),
        None => Landing::Said("the ask ended without an answer; nothing was written".to_string()),
    }
}

// Every kind that could not be listed is named: a list missing one kind
// silently would read as complete.
fn unlisted(label: Option<SharedString>, line: String) -> SharedString {
    match label {
        Some(before) => format!("{before}; {line}").into(),
        None => line.into(),
    }
}

fn armed_line(prefix: &str) -> String {
    format!("{prefix}  (enter again to confirm)")
}

fn sent_line(prefix: &str) -> String {
    format!("{prefix}: sent, waiting for the server")
}

fn dropped_write_line(prefix: &str) -> String {
    format!(
        "{prefix}: no answer arrived; the write may have landed; read the object before retrying"
    )
}

// What an outcome says, in the server's words.
fn outcome_text(outcome: Day2Outcome) -> String {
    match outcome {
        Day2Outcome::NeedsConfirm { summary } => summary,
        Day2Outcome::Applied { summary, truncated } => {
            if truncated {
                format!("{summary}  (truncated)")
            } else {
                summary
            }
        }
        Day2Outcome::Denied { what, why } => format!("{what}: {why}"),
        Day2Outcome::Rejected { message } => message,
        Day2Outcome::Failed { why } => why,
    }
}

/// A confirmed write's answer, named by what was confirmed, so an answer
/// landing after the person moved on still says what it is about. An Applied
/// summary that only repeats the ask says nothing more, so the line says it
/// was applied; one that differs (a drain's count) is shown.
fn answered_line(prefix: &str, asked: &str, outcome: Day2Outcome) -> String {
    match outcome {
        Day2Outcome::Applied { summary, truncated } => {
            let truncated = if truncated { "  (truncated)" } else { "" };
            if summary == asked {
                format!("{prefix}: applied{truncated}")
            } else {
                format!("{prefix}: {summary}{truncated}")
            }
        }
        other => format!("{prefix}: {}", outcome_text(other)),
    }
}

fn target_uid(target: &Day2Target) -> String {
    match &target.namespace {
        Some(namespace) => format!("{}/{}/{}", target.kind, namespace, target.name),
        None => format!("{}/{}", target.kind, target.name),
    }
}

fn parse_ready(page: &TablePage, row: &TableRow) -> Option<i32> {
    let index = page
        .columns
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case("Ready"))?;
    let cell = row.cells.get(index)?;
    cell.split('/').next()?.parse().ok()
}

fn targets_from_page(kind: &str, kind_id: k10s_core::KindId, page: &TablePage) -> Vec<Day2Target> {
    page.rows
        .iter()
        .map(|row| Day2Target {
            kind_id,
            kind: kind.to_string(),
            namespace: row.namespace.clone(),
            name: row.name.clone(),
            current_replicas: parse_ready(page, row),
        })
        .collect()
}

fn targets_page(targets: &[Day2Target]) -> TablePage {
    let columns = ["Kind", "Namespace", "Name"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let rows = targets
        .iter()
        .map(|target| TableRow {
            cells: vec![
                target.kind.clone(),
                target.namespace.clone().unwrap_or_default(),
                target.name.clone(),
            ],
            name: target.name.clone(),
            namespace: target.namespace.clone(),
            uid: target_uid(target),
        })
        .collect();
    TablePage {
        columns,
        rows,
        truncated: false,
        continue_token: None,
    }
}

fn ops_page(target: &Day2Target, ops: &[(String, Day2Op)]) -> TablePage {
    let columns = ["Action", "Target"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let where_ = match &target.namespace {
        Some(namespace) => format!("{}/{}", namespace, target.name),
        None => target.name.clone(),
    };
    let rows = ops
        .iter()
        .map(|(name, _)| TableRow {
            cells: vec![name.clone(), format!("{} {where_}", target.kind)],
            name: name.clone(),
            namespace: target.namespace.clone(),
            uid: name.clone(),
        })
        .collect();
    TablePage {
        columns,
        rows,
        truncated: false,
        continue_token: None,
    }
}

impl crate::item::Item for Day2View {
    fn title(&self) -> SharedString {
        Day2View::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        Day2View::focus_handle(self)
    }
}

impl Render for Day2View {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let label = match self.phase {
            Phase::Targets { .. } => self.targets_label.as_ref(),
            Phase::Ops { .. } => None,
        };
        let empty = nothing_listed(self.table.total_rows(), self.status.as_ref(), label);
        let slot = status_slot(
            &theme,
            &fonts,
            self.status_text
                .text(Slot::of(&self.latch, self.status.as_ref(), label)),
        );
        let loading = matches!(self.phase, Phase::Targets { loading: true });

        div()
            .id("day2-view")
            .key_context(if self.filtering { "Typing" } else { "Browse" })
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(theme.shell.panel_background))
            .font_family(fonts.ui_family.clone())
            .text_color(rgb(theme.shell.text))
            .on_action(cx.listener(|this, _: &RowUp, _, cx| {
                this.latch.disarm();
                this.table.move_selection(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                this.latch.disarm();
                this.table.move_selection(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageUp, _, cx| {
                this.latch.disarm();
                this.table.page_by(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageDown, _, cx| {
                this.latch.disarm();
                this.table.page_by(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowHome, _, cx| {
                this.latch.disarm();
                this.table.select_first();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowEnd, _, cx| {
                this.latch.disarm();
                this.table.select_last();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| {
                this.open_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                match &this.phase {
                    Phase::Targets { .. } => this.fetch_targets(cx),
                    Phase::Ops { .. } => {}
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Back, _, cx| {
                this.back(cx);
            }))
            .on_action(cx.listener(|this, _: &EnterFilter, _, cx| {
                this.latch.disarm();
                this.filtering = true;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                this.latch.disarm();
                this.filtering = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                this.latch.disarm();
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
                // A touch that rounds to no row moves nothing and disarms
                // nothing.
                let step = -(delta / LIST_ROW_HEIGHT).round() as i64;
                if step != 0 {
                    this.latch.disarm();
                    this.table.move_selection(step);
                    cx.notify();
                }
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
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("day2-rows")
                            .flex_1()
                            .min_h(px(LIST_ROW_HEIGHT))
                            .overflow_hidden()
                            .relative()
                            .flex()
                            .flex_col()
                            .role(Role::ListBox)
                            .aria_label("Day-2 operations")
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
                            .children(self.table.visible_lines().into_iter().enumerate().map(
                                |(offset, (selected, line))| {
                                    let mut row = div()
                                        .id(("day2-row", offset))
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
                                                this.latch.disarm();
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
                            .children((empty && !loading).then(|| {
                                div()
                                    .p(px(12.0))
                                    .text_size(px(fonts.ui_size))
                                    .text_color(rgb(theme.shell.text_muted))
                                    .child("select a workload or node, or wait for the target list")
                            })),
                    )
                    .children(slot),
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
                        Phase::Targets { .. } => {
                            "enter pick · / filter · r refresh · esc close filter".to_string()
                        }
                        Phase::Ops { .. } => {
                            "enter ask · enter again confirm · / filter · esc back".to_string()
                        }
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deployment_offers_restart_pause_and_scale_but_not_undo() {
        let ops = ops_for("Deployment", Some(3));
        let names: Vec<&str> = ops.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"restart"));
        assert!(names.contains(&"pause"));
        assert!(names.contains(&"resume"));
        assert!(names.contains(&"scale to 0"));
        assert!(names.contains(&"scale to 1"));
        assert!(names.contains(&"delete"));
        assert!(
            !names.iter().any(|name| name.contains("undo")),
            "rollback is not invented: {names:?}"
        );
    }

    #[test]
    fn a_node_offers_cordon_and_drain_and_a_pod_offers_evict() {
        let node: Vec<_> = ops_for("Node", None)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(node.contains(&"cordon".to_string()));
        assert!(node.contains(&"drain".to_string()));
        let pod: Vec<_> = ops_for("Pod", None)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(pod.contains(&"evict".to_string()));
        assert!(pod.contains(&"debug".to_string()));
    }

    #[test]
    fn scale_is_omitted_when_the_replica_count_is_unknown() {
        let ops = ops_for("Deployment", None);
        assert!(
            ops.iter().all(|(name, _)| !name.starts_with("scale")),
            "a guessed replica count would lie in the confirm sentence"
        );
    }

    const TARGET: &str = "context lab";

    fn needs_confirm() -> Option<Day2Outcome> {
        Some(Day2Outcome::NeedsConfirm {
            summary: "restart deploy prod/api".to_string(),
        })
    }

    // A write's answer lands whether or not anything newer was asked and
    // whether or not its row is still selected: the write happened, or may
    // have. A reply dropped unsent says so.
    #[test]
    fn a_write_answer_always_lands() {
        let write = Some((
            "restart deploy prod/api in context lab",
            "restart deploy prod/api",
        ));
        for current in [true, false] {
            for still_selected in [true, false] {
                assert_eq!(
                    landing(
                        write,
                        current,
                        still_selected,
                        Some(Day2Outcome::Failed {
                            why: "no answer within 35 s".to_string()
                        }),
                        TARGET
                    ),
                    Landing::Settled(
                        "restart deploy prod/api in context lab: no answer within 35 s".to_string()
                    ),
                    "current {current}, selected {still_selected}"
                );
                assert_eq!(
                    landing(write, current, still_selected, None, TARGET),
                    Landing::Settled(
                        "restart deploy prod/api in context lab: no answer arrived; the write may \
                         have landed; read the object before retrying"
                            .to_string()
                    )
                );
            }
        }
    }

    // An ask's answer lands only while it is the newest ask and its row is
    // still selected: a stale answer, or one for a row the person left, arms
    // nothing and says nothing.
    #[test]
    fn an_ask_answer_lands_only_on_the_newest_ask_still_selected() {
        for (current, still_selected) in [(false, true), (true, false), (false, false)] {
            assert_eq!(
                landing(None, current, still_selected, needs_confirm(), TARGET),
                Landing::Dropped
            );
            assert_eq!(
                landing(
                    None,
                    current,
                    still_selected,
                    Some(Day2Outcome::Denied {
                        what: "drain",
                        why: "this account cannot create evictions".to_string()
                    }),
                    TARGET
                ),
                Landing::Dropped
            );
            assert_eq!(
                landing(None, current, still_selected, None, TARGET),
                Landing::Dropped
            );
        }
        assert_eq!(
            landing(None, true, true, needs_confirm(), TARGET),
            Landing::Arm {
                prefix: "restart deploy prod/api in context lab".to_string(),
                asked: "restart deploy prod/api".to_string(),
            }
        );
        assert_eq!(
            landing(
                None,
                true,
                true,
                Some(Day2Outcome::Denied {
                    what: "drain",
                    why: "this account cannot create evictions".to_string()
                }),
                TARGET
            ),
            Landing::Said("drain: this account cannot create evictions".to_string())
        );
        assert_eq!(
            landing(None, true, true, None, TARGET),
            Landing::Said("the ask ended without an answer; nothing was written".to_string())
        );
    }

    #[test]
    fn the_confirm_and_sent_lines_name_the_write_and_where_it_lands() {
        for target in [write_target(Some("lab")), write_target(None)] {
            let Landing::Arm { prefix, .. } = landing(None, true, true, needs_confirm(), &target)
            else {
                panic!("the newest ask on its row arms");
            };
            assert_eq!(
                armed_line(&prefix),
                format!("restart deploy prod/api in {target}  (enter again to confirm)")
            );
            assert_eq!(
                sent_line(&prefix),
                format!("restart deploy prod/api in {target}: sent, waiting for the server")
            );
        }
        assert_eq!(write_target(None), "this cluster's in-cluster account");
    }

    #[test]
    fn every_kind_that_could_not_be_listed_is_named() {
        let one = unlisted(None, "Node: access denied for this account".to_string());
        assert_eq!(one.as_ref(), "Node: access denied for this account");
        assert_eq!(
            unlisted(Some(one), "StatefulSet: the list timed out".to_string()).as_ref(),
            "Node: access denied for this account; StatefulSet: the list timed out"
        );
    }

    #[test]
    fn a_write_answer_names_what_was_confirmed() {
        let prefix = "scale deploy prod/api to 0 in context lab";
        let asked = "scale deploy prod/api to 0";
        // An Applied summary that repeats the ask says only that it applied.
        assert_eq!(
            answered_line(
                prefix,
                asked,
                Day2Outcome::Applied {
                    summary: asked.to_string(),
                    truncated: false,
                }
            ),
            "scale deploy prod/api to 0 in context lab: applied"
        );
        // One that says more, a drain's count, is shown.
        assert_eq!(
            answered_line(
                "drain node w1: cordon, then evict 2 pods in context lab",
                "drain node w1: cordon, then evict 2 pods",
                Day2Outcome::Applied {
                    summary: "cordoned w1; evicted 2 pods".to_string(),
                    truncated: true,
                }
            ),
            "drain node w1: cordon, then evict 2 pods in context lab: cordoned w1; evicted 2 pods  \
             (truncated)"
        );
        assert_eq!(
            answered_line(
                prefix,
                asked,
                Day2Outcome::Denied {
                    what: "scale",
                    why: "forbidden".to_string(),
                }
            ),
            "scale deploy prod/api to 0 in context lab: scale: forbidden"
        );
        assert_eq!(
            answered_line(
                prefix,
                asked,
                Day2Outcome::Failed {
                    why: "no answer within 35 s".to_string(),
                }
            ),
            "scale deploy prod/api to 0 in context lab: no answer within 35 s"
        );
    }

    #[test]
    fn ops_page_keys_rows_by_the_action_name() {
        let target = Day2Target {
            kind_id: k10s_core::KindId::DEPLOYMENT,
            kind: "Deployment".into(),
            namespace: Some("prod".into()),
            name: "api".into(),
            current_replicas: Some(2),
        };
        let ops = ops_for(&target.kind, target.current_replicas);
        let page = ops_page(&target, &ops);
        assert_eq!(page.rows[0].uid, page.rows[0].name);
        assert!(
            page.rows
                .iter()
                .any(|row| row.cells[1].contains("prod/api"))
        );
    }
}
