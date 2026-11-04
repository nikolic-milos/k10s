//! Workload investigations resolve Kubernetes identity before querying.
//!
//! Native plots and the inspector share samples, source and time bounds.
//! Each request belongs to one generation and is aborted when replaced or
//! hidden. Unsupported Grafana semantics remain labelled and raw queries
//! remain explicit. A span link retains its recorded identity and time range
//! independently of current Kubernetes metadata.

use crate::chart::{ChartData, ChartView, display_message, display_notice_groups};
use crate::notice::NoticeSource;
use crate::table::{MAX_ROWS, TableState, TableText};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EnterFilter, ObserveCatalogDetails,
    ObserveFocusLogs, ObserveFocusNext, ObserveFocusPanels, ObserveFocusPrevious,
    ObserveOpenGrafana, ObserveOtlpLogs, ObservePodLabelLogs, ObserveRangeFifteenMinutes,
    ObserveRangeOneHour, ObserveReadiness, ObserveScopeDetails, ObserveSourceDetails,
    ObserveToggleDialect, ObserveToggleResolution, OpenQueryDetails, OpenRow, Refresh, RowDown,
    RowEnd, RowHome, RowPageDown, RowPageUp, RowUp,
};
use gpui::{
    ClickEvent, Context, Entity, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window, div, prelude::*,
    px, rgb,
};
use k10s_provider::{
    ClusterProvider, DescribeRequest, GrafanaOutcome, GrafanaPanelKind, GrafanaPanelRow,
    GrafanaRowIdentity, GrafanaRowSource, LogEntry, LokiOutcome, LokiRequest, ObserveScope,
    PromOutcome, PromRequest, PromSeriesView, QueryDialect, QueryWindow, RequestStop, ScopeOutcome,
    SpanLogQuery, TableColumn, TablePage, TableRow, ToolPresence,
};
use std::{
    rc::Rc,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub use crate::notice::NoticeSnapshot;

pub enum ObserveEvent {
    Metrics {
        uid: String,
        data: Option<Arc<ChartData>>,
    },
    Trace {
        id: String,
        scope: Option<ObserveScope>,
    },
    GrafanaRoute {
        request: k10s_provider::ForwardRequest,
        path: String,
    },
    CatalogDetails(NoticeSnapshot),
    QueryDetails(NoticeSnapshot),
    ScopeDetails(NoticeSnapshot),
    SourceDetails(NoticeSnapshot),
    NotServed,
    Presence(ToolPresence),
    TracePresence(ToolPresence),
}

pub(crate) enum ObserveEntry {
    General,
    SpanLogs(Rc<SpanLogQuery>),
    RetainedLogs(DescribeRequest),
}

#[derive(Clone, Copy)]
enum QueryKind {
    Prom,
    Loki,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObserveTable {
    Panels,
    Logs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusStop {
    Root,
    Panels,
    Logs,
    Scope,
    Source,
    Catalog,
    Query,
}

const FOCUS_STOPS: [FocusStop; 7] = [
    FocusStop::Root,
    FocusStop::Panels,
    FocusStop::Logs,
    FocusStop::Scope,
    FocusStop::Source,
    FocusStop::Catalog,
    FocusStop::Query,
];

fn next_focus(current: FocusStop, forward: bool, available: [bool; 6]) -> FocusStop {
    let start = FOCUS_STOPS
        .iter()
        .position(|stop| *stop == current)
        .expect("known focus stop");
    for step in 1..=FOCUS_STOPS.len() {
        let index = if forward {
            (start + step) % FOCUS_STOPS.len()
        } else {
            (start + FOCUS_STOPS.len() - step) % FOCUS_STOPS.len()
        };
        if index == 0 || available[index - 1] {
            return FOCUS_STOPS[index];
        }
    }
    unreachable!("root focus is always available")
}

fn visible_table(
    preferred: Option<ObserveTable>,
    grafana: bool,
    panels_expanded: bool,
    has_logs: bool,
) -> Option<ObserveTable> {
    match preferred {
        Some(ObserveTable::Panels) if grafana && panels_expanded => preferred,
        Some(ObserveTable::Logs) if has_logs => preferred,
        _ => None,
    }
}

fn input_focus_stop(typing: bool, table: Option<ObserveTable>) -> FocusStop {
    if typing {
        FocusStop::Root
    } else {
        match table {
            Some(ObserveTable::Panels) => FocusStop::Panels,
            Some(ObserveTable::Logs) => FocusStop::Logs,
            None => FocusStop::Root,
        }
    }
}

fn table_after_scroll(
    preferred: Option<ObserveTable>,
    table: ObserveTable,
    filtering: bool,
) -> Option<ObserveTable> {
    if filtering { preferred } else { Some(table) }
}

fn preferred_table_after_focus(
    current: FocusStop,
    target: FocusStop,
    preferred: Option<ObserveTable>,
    visible: Option<ObserveTable>,
) -> Option<ObserveTable> {
    match target {
        FocusStop::Root if current == FocusStop::Root => preferred,
        FocusStop::Root => None,
        FocusStop::Panels => Some(ObserveTable::Panels),
        FocusStop::Logs => Some(ObserveTable::Logs),
        _ => visible,
    }
}

fn table_available(table: ObserveTable, grafana: bool, has_logs: bool) -> bool {
    match table {
        ObserveTable::Panels => grafana,
        ObserveTable::Logs => has_logs,
    }
}

fn prepare_table_selection(typing: &mut bool, filtering: &mut bool, changed_table: bool) -> bool {
    let was_typing = std::mem::take(typing);
    if changed_table {
        *filtering = false;
    }
    was_typing || changed_table
}

fn leave_input(typing: &mut bool, filtering: &mut bool) -> bool {
    let changed = *typing || *filtering;
    *typing = false;
    *filtering = false;
    changed
}

fn leave_input_for_notice_activation(
    event: &ClickEvent,
    typing: &mut bool,
    filtering: &mut bool,
) -> bool {
    !matches!(event, ClickEvent::Keyboard(_)) && leave_input(typing, filtering)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelInputAction {
    Ignore,
    LeaveInput,
    FocusRoot,
}

fn cancel_input_action(
    root_focused: bool,
    notice_focused: bool,
    editing: bool,
) -> CancelInputAction {
    if notice_focused && !editing {
        CancelInputAction::FocusRoot
    } else if (root_focused || notice_focused) && editing {
        CancelInputAction::LeaveInput
    } else {
        CancelInputAction::Ignore
    }
}

fn unavailable_notice_focus<T: PartialEq>(
    previous: &T,
    handles: &[T; 4],
    available: [bool; 4],
) -> bool {
    handles
        .iter()
        .zip(available)
        .any(|(handle, available)| handle == previous && !available)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenTableAction {
    ExpandPanels,
    RunPanel,
    FollowTrace,
}

fn table_open_action(
    preferred: Option<ObserveTable>,
    grafana: bool,
    panels_expanded: bool,
    has_logs: bool,
) -> Option<OpenTableAction> {
    match visible_table(preferred, grafana, panels_expanded, has_logs) {
        Some(ObserveTable::Panels) => Some(OpenTableAction::RunPanel),
        Some(ObserveTable::Logs) => Some(OpenTableAction::FollowTrace),
        None if preferred != Some(ObserveTable::Logs) && grafana && !panels_expanded => {
            Some(OpenTableAction::ExpandPanels)
        }
        None => None,
    }
}

impl ObserveTable {
    fn state<'a>(self, panels: &'a mut TableState, logs: &'a mut TableState) -> &'a mut TableState {
        match self {
            Self::Panels => panels,
            Self::Logs => logs,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Panels => "Panel table selected",
            Self::Logs => "Log table selected",
        }
    }
}

#[derive(Clone)]
enum QueryPlan {
    Readiness,
    Panel(Box<GrafanaPanelRow>),
    Prom(String),
    Loki(String),
    ScopedLogs(bool),
    SpanLogs(Rc<SpanLogQuery>),
}

impl QueryPlan {
    fn requires_scope(&self) -> bool {
        matches!(self, Self::Readiness | Self::Panel(_) | Self::ScopedLogs(_))
    }

    fn refresh_panel(&mut self, panels: &[GrafanaPanelRow]) -> bool {
        let Self::Panel(previous) = self else {
            return false;
        };
        let mut matching = panels.iter().filter(|panel| {
            panel.dashboard_uid == previous.dashboard_uid
                && panel.identity == previous.identity
                && panel.source == previous.source
                && panel.ref_id == previous.ref_id
        });
        let current = matching.next().filter(|_| matching.next().is_none());
        if current == Some(previous.as_ref()) {
            return false;
        }
        *self = current.map_or(Self::Readiness, |panel| {
            Self::Panel(Box::new(panel.clone()))
        });
        true
    }

    fn take_scope_query(
        &self,
        pending: &mut Option<u64>,
        generation: u64,
        scope_ready: bool,
        catalog_pending: bool,
    ) -> bool {
        if *pending != Some(generation)
            || !scope_ready
            || !self.requires_scope()
            || (catalog_pending && matches!(self, Self::Panel(_)))
        {
            return false;
        }
        *pending = None;
        true
    }
}

fn advance_query(generation: &mut u64, pending_scope: &mut Option<u64>, retain_scope: bool) {
    let resume_scope = retain_scope && *pending_scope == Some(*generation);
    *generation += 1;
    *pending_scope = resume_scope.then_some(*generation);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryIdentity {
    generation: u64,
    request: Option<DescribeRequest>,
    window: QueryWindow,
}

struct QueryFailure {
    source: &'static str,
    dialect: &'static str,
    expression: String,
    range: String,
    why: String,
}

impl QueryFailure {
    fn new(
        source: &'static str,
        dialect: &'static str,
        expression: String,
        window: QueryWindow,
        why: String,
    ) -> Self {
        let range = format!(
            "{} to {} UTC; view resolution {} ms",
            crate::chart::clock(window.start_ms),
            crate::chart::clock(window.end_ms),
            window.step_ms,
        );
        Self {
            source,
            dialect,
            expression,
            range,
            why,
        }
    }
}

impl NoticeSource for QueryFailure {
    fn notice_count(&self) -> usize {
        3
    }

    fn notice_at(&self, index: usize) -> Option<(&str, &str)> {
        match index {
            0 => Some((self.source, &self.why)),
            1 => Some((self.dialect, &self.expression)),
            2 => Some(("Requested range", &self.range)),
            _ => None,
        }
    }
}

struct FailureDetails {
    source: &'static str,
    why: String,
}

impl NoticeSource for FailureDetails {
    fn notice_count(&self) -> usize {
        1
    }

    fn notice_at(&self, index: usize) -> Option<(&str, &str)> {
        (index == 0).then_some((self.source, self.why.as_str()))
    }
}

fn failure_details(source: &'static str, why: String) -> NoticeSnapshot {
    NoticeSnapshot::new(FailureDetails { source, why })
}

impl QueryIdentity {
    fn current(
        &self,
        generation: u64,
        request: Option<&DescribeRequest>,
        window: QueryWindow,
        visible: bool,
    ) -> bool {
        visible
            && self.generation == generation
            && self.request.as_ref() == request
            && self.window == window
    }
}

pub struct ObserveView {
    focus: FocusHandle,
    notice_focus: [FocusHandle; 4],
    notice_context: gpui::KeyContext,
    notice_input_context: gpui::KeyContext,
    provider: Rc<dyn ClusterProvider>,
    table: TableState,
    table_text: TableText,
    panels: Vec<GrafanaPanelRow>,
    logs: Vec<LogEntry>,
    log_table: TableState,
    log_text: TableText,
    table_focus: Option<ObserveTable>,
    panels_expanded: bool,
    chart: Entity<ChartView>,
    request: Option<DescribeRequest>,
    scope: Option<ObserveScope>,
    window: QueryWindow,
    range_ms: i64,
    query: String,
    query_label: SharedString,
    query_label_typing: bool,
    query_kind: QueryKind,
    plan: QueryPlan,
    typing: bool,
    filtering: bool,
    status: SharedString,
    reach_notice: SharedString,
    source_details: Option<NoticeSnapshot>,
    catalog_notice: SharedString,
    catalog_details: Option<NoticeSnapshot>,
    query_failure: Option<NoticeSnapshot>,
    scope_label: SharedString,
    scope_details: Option<NoticeSnapshot>,
    visible: bool,
    generation: u64,
    catalog_revision: u64,
    query_generation: u64,
    scope_generation: u64,
    scope_query: Option<u64>,
    scope_stop: Option<RequestStop>,
    catalog_stop: Option<RequestStop>,
    probe_stop: Option<RequestStop>,
    query_stop: Option<RequestStop>,
    grafana: ToolPresence,
    prometheus: ToolPresence,
    loki: ToolPresence,
    catalog_pending: bool,
    probe_pending: bool,
    catalogue_enabled: bool,
    entry_pending: bool,
}

impl EventEmitter<ObserveEvent> for ObserveView {}

impl ObserveView {
    pub fn new(
        provider: Rc<dyn ClusterProvider>,
        selection: Option<&crate::selection::Selection>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::for_entry(provider, selection, ObserveEntry::General, cx)
    }

    pub(crate) fn for_entry(
        provider: Rc<dyn ClusterProvider>,
        selection: Option<&crate::selection::Selection>,
        entry: ObserveEntry,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            focus: cx.focus_handle(),
            notice_focus: std::array::from_fn(|_| cx.focus_handle()),
            notice_context: gpui::KeyContext::parse(observe_key_context(false, false, true))
                .expect("notice control context"),
            notice_input_context: gpui::KeyContext::parse(observe_key_context(true, false, true))
                .expect("notice control input context"),
            provider,
            table: TableState::new(),
            table_text: TableText::default(),
            panels: Vec::new(),
            logs: Vec::new(),
            log_table: TableState::new(),
            log_text: TableText::default(),
            table_focus: Some(ObserveTable::Panels),
            panels_expanded: false,
            chart: cx.new(|_| ChartView::new(false)),
            request: selection.map(|selection| selection.describe_request()),
            scope: None,
            window: recent_window(900_000, 15_000),
            range_ms: 900_000,
            query: String::new(),
            query_label: SharedString::default(),
            query_label_typing: false,
            query_kind: QueryKind::Prom,
            plan: QueryPlan::Readiness,
            typing: false,
            filtering: false,
            status: "Discovering observability".into(),
            reach_notice: SharedString::default(),
            source_details: None,
            catalog_notice: SharedString::default(),
            catalog_details: None,
            query_failure: None,
            scope_label: "Select a pod or workload".into(),
            scope_details: None,
            visible: false,
            generation: 0,
            catalog_revision: 0,
            query_generation: 0,
            scope_generation: 0,
            scope_query: None,
            scope_stop: None,
            catalog_stop: None,
            probe_stop: None,
            query_stop: None,
            grafana: ToolPresence::Missing,
            prometheus: ToolPresence::Missing,
            loki: ToolPresence::Missing,
            catalog_pending: false,
            probe_pending: false,
            catalogue_enabled: false,
            entry_pending: true,
        };
        view.table.set_viewport(6);
        view.log_table.set_viewport(8);
        view.prepare_entry(entry, cx);
        view
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    pub fn title(&self) -> SharedString {
        "Observe".into()
    }

    pub(crate) fn metrics(&self, cx: &gpui::App) -> Option<Arc<ChartData>> {
        self.chart.read(cx).data().cloned()
    }
    pub fn set_selection(
        &mut self,
        selection: Option<&crate::selection::Selection>,
        cx: &mut Context<Self>,
    ) {
        let request = selection.map(|selection| selection.describe_request());
        if self.request == request {
            return;
        }
        self.cancel_query(cx);
        self.scope_stop = None;
        self.scope_generation += 1;
        self.request = request;
        self.window = recent_window(self.range_ms, self.window.step_ms);
        self.plan = QueryPlan::Readiness;
        self.entry_pending = true;
        self.scope = None;
        self.scope_label = "Select a pod or workload".into();
        self.scope_details = None;
        if self.visible {
            self.resolve(cx);
        }
        cx.notify();
    }

    fn cancel_query(&mut self, cx: &mut Context<Self>) {
        advance_query(&mut self.query_generation, &mut self.scope_query, false);
        self.clear_query(cx);
    }

    fn clear_query(&mut self, cx: &mut Context<Self>) {
        self.query_stop = None;
        self.query_failure = None;
        self.chart.update(cx, |chart, cx| chart.set_data(None, cx));
        self.logs.clear();
        if self.table_focus == Some(ObserveTable::Logs) {
            self.table_focus = None;
        }
        self.log_table.set_page(TablePage::default());
        if let Some(request) = &self.request {
            cx.emit(ObserveEvent::Metrics {
                uid: request.uid.clone(),
                data: None,
            });
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.entry_pending = true;
            return;
        }
        self.entry_pending = false;
        self.cancel_query(cx);
        self.generation += 1;
        self.scope_stop = None;
        self.catalog_stop = None;
        self.probe_stop = None;
        self.scope = None;
        self.scope_details = None;
        self.source_details = None;
        self.catalog_details = None;
        self.window = match &self.plan {
            QueryPlan::SpanLogs(query) => query.request.window,
            _ => recent_window(self.range_ms, self.window.step_ms),
        };
        if !self.catalogue_enabled {
            self.catalog_pending = false;
            self.probe_pending = false;
            self.reach_notice = SharedString::default();
            self.catalog_notice = SharedString::default();
            self.scope_generation += 1;
            self.resolve(cx);
            return;
        }
        self.reach_notice = "Checking observability sources".into();
        self.catalog_notice = "Reading Grafana panels".into();
        let generation = self.generation;
        self.catalog_pending = true;
        self.probe_pending = true;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.probe_stop = Some(self.provider.probe_observe(Box::new(move |reach| {
            let _ = tx.send(reach);
        })));
        cx.spawn(async move |this, cx| {
            if let Ok(reach) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation || !this.visible {
                        return;
                    }
                    this.probe_stop = None;
                    this.probe_pending = false;
                    this.prometheus = reach.prometheus;
                    this.loki = reach.loki;
                    cx.emit(ObserveEvent::TracePresence(reach.traces));
                    if let Some(why) = reach.error {
                        this.reach_notice = failure_notice("Source discovery", &why);
                        this.source_details = Some(failure_details("Source discovery", why));
                    } else {
                        this.reach_notice = SharedString::default();
                        this.source_details = None;
                    }
                    this.apply_absence(cx);
                    cx.notify();
                });
            }
        })
        .detach();
        let (tx, rx) = futures::channel::oneshot::channel();
        self.catalog_stop = Some(self.provider.fetch_grafana(Box::new(move |outcome| {
            let _ = tx.send(outcome);
        })));
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation || !this.visible {
                        return;
                    }
                    this.catalog_stop = None;
                    this.catalog_pending = false;
                    let current_panels = match &outcome {
                        GrafanaOutcome::Catalog { panels, .. } => panels.as_slice(),
                        _ => &[],
                    };
                    if this.plan.refresh_panel(current_panels) {
                        let panel_available = matches!(this.plan, QueryPlan::Panel(_));
                        advance_query(
                            &mut this.query_generation,
                            &mut this.scope_query,
                            panel_available,
                        );
                        this.clear_query(cx);
                        if panel_available {
                            this.status =
                                "Grafana catalogue updated; previous panel samples cleared".into();
                        } else {
                            this.status =
                                "Selected Grafana panel is missing or ambiguous in the refreshed catalogue"
                                    .into();
                        }
                    }
                    this.panels.clear();
                    match outcome {
                        GrafanaOutcome::Catalog {
                            panels,
                            omitted,
                            truncated,
                            notices,
                        } => {
                            this.grafana = ToolPresence::Ready;
                            this.panels = panels;
                            this.catalog_revision += 1;
                            this.table.set_page(panels_page(
                                &this.panels,
                                truncated || omitted > 0,
                                this.catalog_revision,
                            ));
                            this.table.select_first();
                            (this.catalog_notice, this.catalog_details) = catalog_notices(
                                this.table.total_rows(),
                                truncated,
                                omitted.saturating_add(
                                    this.panels.len().saturating_sub(this.table.total_rows()),
                                ),
                                notices,
                            );
                        }
                        GrafanaOutcome::Absent => {
                            this.grafana = ToolPresence::Missing;
                            this.table.set_page(TablePage::default());
                            this.catalog_notice = SharedString::default();
                            this.catalog_details = None;
                        }
                        GrafanaOutcome::Denied(what) => {
                            this.grafana = ToolPresence::Blocked;
                            let notice = format!("{what}: access denied for this account");
                            this.catalog_notice = notice.clone().into();
                            this.catalog_details = Some(NoticeSnapshot::new(vec![notice]));
                            this.table.set_page(TablePage::default());
                        }
                        GrafanaOutcome::Failed(why) => {
                            this.grafana = ToolPresence::Blocked;
                            this.catalog_notice = failure_notice("Grafana catalog failed", &why);
                            this.catalog_details =
                                Some(failure_details("Grafana catalog failed", why));
                            this.table.set_page(TablePage::default());
                        }
                    }
                    this.resume_scope_query(cx);
                    this.apply_absence(cx);
                    cx.notify();
                });
            }
        })
        .detach();
        self.scope_generation += 1;
        self.resolve(cx);
    }

    fn change_range(&mut self, cx: &mut Context<Self>) {
        if matches!(self.plan, QueryPlan::SpanLogs(_)) {
            self.plan = QueryPlan::Loki(self.query.clone());
            self.scope_label = "Kubernetes scope is not resolved".into();
        }
        self.window = recent_window(self.range_ms, self.window.step_ms);
        self.cancel_query(cx);
        self.scope_stop = None;
        self.scope = None;
        self.scope_generation += 1;
        self.resolve(cx);
    }

    fn set_range(&mut self, range_ms: i64, cx: &mut Context<Self>) {
        self.range_ms = range_ms;
        self.change_range(cx);
    }

    fn toggle_resolution(&mut self, cx: &mut Context<Self>) {
        self.window.step_ms = if self.window.step_ms == 15_000 {
            30_000
        } else {
            15_000
        };
        self.change_range(cx);
    }

    fn toggle_dialect(&mut self, cx: &mut Context<Self>) {
        self.cancel_query(cx);
        self.query_kind = match self.query_kind {
            QueryKind::Prom => QueryKind::Loki,
            QueryKind::Loki => QueryKind::Prom,
        };
        self.status = "Query dialect changed; run explicitly".into();
        cx.notify();
    }

    fn visible_table(&self) -> Option<ObserveTable> {
        visible_table(
            self.table_focus,
            self.grafana != ToolPresence::Missing,
            self.panels_expanded,
            !self.logs.is_empty(),
        )
    }

    fn select_table(&mut self, table: ObserveTable) -> bool {
        let changed = prepare_table_selection(
            &mut self.typing,
            &mut self.filtering,
            self.table_focus != Some(table),
        );
        self.table_focus = Some(table);
        changed
    }

    fn focus_table(&mut self, table: ObserveTable, cx: &mut Context<Self>) -> bool {
        if !table_available(
            table,
            self.grafana != ToolPresence::Missing,
            !self.logs.is_empty(),
        ) {
            return false;
        }
        let panels_expanded = table == ObserveTable::Panels;
        let changed = self.select_table(table);
        if changed || self.panels_expanded != panels_expanded {
            self.panels_expanded = panels_expanded;
            cx.notify();
        }
        true
    }

    fn notice_availability(&self, cx: &gpui::App) -> [bool; 4] {
        [
            self.scope_details.is_some(),
            !self.reach_notice.is_empty() && self.source_details.is_some(),
            !self.catalog_notice.is_empty() && self.catalog_details.is_some(),
            self.query_failure.is_some() || self.chart.read(cx).data().is_some(),
        ]
    }

    pub(crate) fn notice_focus_fallback(
        &self,
        previous: &FocusHandle,
        cx: &gpui::App,
    ) -> Option<FocusHandle> {
        unavailable_notice_focus(previous, &self.notice_focus, self.notice_availability(cx))
            .then(|| self.focus.clone())
    }

    fn focus_next(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let notices = self.notice_availability(cx);
        let available = [
            table_available(
                ObserveTable::Panels,
                self.grafana != ToolPresence::Missing,
                !self.logs.is_empty(),
            ),
            table_available(
                ObserveTable::Logs,
                self.grafana != ToolPresence::Missing,
                !self.logs.is_empty(),
            ),
            notices[0],
            notices[1],
            notices[2],
            notices[3],
        ];
        let current = self
            .notice_focus
            .iter()
            .position(|handle| handle.is_focused(window))
            .map(|index| FOCUS_STOPS[index + 3])
            .unwrap_or_else(|| input_focus_stop(self.typing, self.visible_table()));
        let target = next_focus(current, forward, available);
        let preferred =
            preferred_table_after_focus(current, target, self.table_focus, self.visible_table());
        match target {
            FocusStop::Root => {
                if self.table_focus != preferred {
                    self.table_focus = preferred;
                    cx.notify();
                }
                window.focus(&self.focus, cx);
            }
            FocusStop::Panels => {
                self.focus_table(ObserveTable::Panels, cx);
                window.focus(&self.focus, cx);
            }
            FocusStop::Logs => {
                self.focus_table(ObserveTable::Logs, cx);
                window.focus(&self.focus, cx);
            }
            _ => {
                if self.table_focus != preferred {
                    self.table_focus = preferred;
                    cx.notify();
                }
                let index = FOCUS_STOPS
                    .iter()
                    .position(|stop| *stop == target)
                    .expect("known notice control")
                    - 3;
                window.focus(&self.notice_focus[index], cx);
            }
        }
    }

    fn prepare_notice_open(&mut self, cx: &mut Context<Self>) {
        if self.visible_table().is_none() && self.table_focus.take().is_some() {
            cx.notify();
        }
    }

    fn leave_input(&mut self, cx: &mut Context<Self>) {
        if leave_input(&mut self.typing, &mut self.filtering) {
            cx.notify();
        }
    }

    fn preserve_input_focus(&self, window: &mut Window) {
        if self.typing || self.filtering {
            // GPUI transfers focus during bubbling, but still records the click.
            window.prevent_default();
        }
    }

    fn focus_notice_on_activation(
        &mut self,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.notice_availability(cx)[index] {
            return false;
        }
        window.focus(&self.notice_focus[index], cx);
        if leave_input_for_notice_activation(event, &mut self.typing, &mut self.filtering) {
            cx.notify();
        }
        true
    }

    fn apply_absence(&self, cx: &mut Context<Self>) {
        let Some(presence) = surface_presence(
            self.catalogue_enabled,
            self.catalog_pending || self.probe_pending,
            self.grafana,
            self.prometheus,
            self.loki,
        ) else {
            return;
        };
        if presence == ToolPresence::Missing {
            cx.emit(ObserveEvent::NotServed);
        } else {
            cx.emit(ObserveEvent::Presence(presence));
        }
    }

    fn resolve(&mut self, cx: &mut Context<Self>) {
        self.scope_details = None;
        if !self.visible {
            self.entry_pending = true;
            return;
        }
        self.entry_pending = false;
        if matches!(self.plan, QueryPlan::SpanLogs(_)) {
            self.run_plan(cx);
            return;
        }
        let requires_scope = self.plan.requires_scope();
        if !requires_scope {
            self.run_plan(cx);
            if !self.catalogue_enabled {
                return;
            }
        }
        let Some(request) = self.request.clone() else {
            if requires_scope {
                self.scope_label = "No current Kubernetes selection".into();
                self.status = "Select a pod or workload".into();
                cx.notify();
            }
            return;
        };
        let generation = self.scope_generation;
        if requires_scope {
            self.scope_query = Some(self.query_generation);
            self.status = "Resolving current pod identities".into();
        }
        self.scope_label = "Resolving current pod identities".into();
        let (tx, rx) = futures::channel::oneshot::channel();
        self.scope_stop = Some(self.provider.resolve_observe_scope(
            request.clone(),
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        ));
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.scope_generation != generation
                        || !this.visible
                        || this.request.as_ref() != Some(&request)
                    {
                        return;
                    }
                    this.scope_stop = None;
                    let resume_query = this.scope_query == Some(this.query_generation);
                    match outcome {
                        ScopeOutcome::Ready(scope) => {
                            if scope.request != request {
                                this.scope_label =
                                    "Resolved metadata does not match the selected resource".into();
                                if resume_query {
                                    this.scope_query = None;
                                    this.status = "Scope unavailable".into();
                                }
                                cx.notify();
                                return;
                            }
                            this.scope_label = format!(
                                "{}/{}: {} current {}",
                                scope.namespace,
                                scope.request.name,
                                scope.pods.len(),
                                if scope.pods.len() == 1 { "pod" } else { "pods" },
                            )
                            .into();
                            this.scope = Some(scope);
                            if !this.resume_scope_query(cx) && resume_query {
                                this.status = "Reading refreshed Grafana panels".into();
                            }
                        }
                        ScopeOutcome::Denied(what) => {
                            this.scope_label =
                                format!("{what}: access denied for this account").into();
                            if resume_query {
                                this.scope_query = None;
                                this.status = "Scope unavailable".into();
                            }
                        }
                        ScopeOutcome::Failed(why) => {
                            this.scope_label = failure_notice("Kubernetes scope failed", &why);
                            this.scope_details =
                                Some(failure_details("Kubernetes scope failed", why));
                            if resume_query {
                                this.scope_query = None;
                                this.status = "Scope unavailable".into();
                            }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn resume_scope_query(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.plan.take_scope_query(
            &mut self.scope_query,
            self.query_generation,
            self.scope.is_some(),
            self.catalog_pending,
        ) {
            return false;
        }
        self.run_plan(cx);
        true
    }

    fn begin_query(&mut self, cx: &mut Context<Self>) -> QueryIdentity {
        self.cancel_query(cx);
        self.panels_expanded = false;
        self.status = "Fetching fixed range".into();
        cx.notify();
        QueryIdentity {
            generation: self.query_generation,
            request: self.request.clone(),
            window: self.window,
        }
    }

    fn receive_prom(
        &mut self,
        rx: futures::channel::oneshot::Receiver<PromOutcome>,
        identity: QueryIdentity,
        title: String,
        unit: String,
        reducer: Option<String>,
        scoped_by_uid: bool,
        cx: &mut Context<Self>,
    ) {
        let window = identity.window;
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if !identity.current(
                        this.query_generation,
                        this.request.as_ref(),
                        this.window,
                        this.visible,
                    ) {
                        return;
                    }
                    this.query_stop = None;
                    match outcome {
                        PromOutcome::Series {
                            series,
                            truncated,
                            dropped_series,
                            possible_server_limit,
                            warnings,
                            infos,
                            warnings_omitted,
                            infos_omitted,
                            source,
                            step_ms,
                            expr,
                            timing,
                        } => {
                            let notices = prom_notices(
                                warnings,
                                infos,
                                metric_association(this.scope.as_ref(), &series, scoped_by_uid),
                                truncated.then_some(dropped_series),
                                possible_server_limit,
                                warnings_omitted,
                                infos_omitted,
                            );
                            this.query.clone_from(&expr);
                            let data = Arc::new(ChartData {
                                series,
                                window: QueryWindow { step_ms, ..window },
                                title,
                                unit,
                                source,
                                expr,
                                reducer,
                                notices,
                                timing,
                            });
                            this.chart.update(cx, |chart, cx| {
                                chart.set_data(Some(data.clone()), cx);
                            });
                            if let Some(request) = &identity.request {
                                cx.emit(ObserveEvent::Metrics {
                                    uid: request.uid.clone(),
                                    data: Some(data),
                                });
                            }
                            this.status = "Fixed range; refresh explicitly to advance time".into();
                        }
                        PromOutcome::Absent => {
                            this.prometheus = ToolPresence::Missing;
                            this.status = "Prometheus is not served by this cluster".into();
                        }
                        PromOutcome::Denied(what) => {
                            this.status = format!("{what}: access denied for this account").into();
                        }
                        PromOutcome::Failed(why) => {
                            this.status = failure_notice("Prometheus query failed", &why);
                            this.query_failure = Some(NoticeSnapshot::new(QueryFailure::new(
                                "Prometheus query failed",
                                if matches!(&this.plan, QueryPlan::Panel(_)) {
                                    "Grafana panel PromQL before variable expansion"
                                } else {
                                    "Requested PromQL"
                                },
                                this.query.clone(),
                                window,
                                why,
                            )));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn run_readiness(&mut self, cx: &mut Context<Self>) {
        self.plan = QueryPlan::Readiness;
        if self.scope.is_none() {
            self.cancel_query(cx);
            self.scope_stop = None;
            self.scope_generation += 1;
            self.resolve(cx);
            return;
        }
        let Some(expr) = self.scope.as_ref().and_then(ObserveScope::readiness_query) else {
            self.cancel_query(cx);
            self.status = "No current pods belong to this workload".into();
            cx.notify();
            return;
        };
        self.query = expr.clone();
        self.query_kind = QueryKind::Prom;
        let generation = self.begin_query(cx);
        let (tx, rx) = futures::channel::oneshot::channel();
        self.query_stop = Some(self.provider.query_promql(
            PromRequest {
                expr,
                window: self.window,
            },
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        ));
        self.receive_prom(
            rx,
            generation,
            "Pod readiness".into(),
            "none".into(),
            None,
            true,
            cx,
        );
    }

    fn run_selected(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self
            .table
            .selected_row()
            .and_then(|row| catalog_row_index(&row.uid, self.catalog_revision))
            .and_then(|index| self.panels.get(index))
            .cloned()
        else {
            return;
        };
        self.run_panel(panel, cx);
    }

    fn run_plan(&mut self, cx: &mut Context<Self>) {
        match self.plan.clone() {
            QueryPlan::Readiness => self.run_readiness(cx),
            QueryPlan::Panel(panel) => self.run_panel(*panel, cx),
            QueryPlan::Prom(query) => {
                self.query = query;
                self.query_kind = QueryKind::Prom;
                self.raw_query(cx);
            }
            QueryPlan::Loki(query) => {
                self.query = query;
                self.query_kind = QueryKind::Loki;
                self.raw_query(cx);
            }
            QueryPlan::ScopedLogs(otlp) => self.scoped_logs(otlp, cx),
            QueryPlan::SpanLogs(_) => self.run_loki(cx),
        }
    }

    fn run_panel(&mut self, mut panel: GrafanaPanelRow, cx: &mut Context<Self>) {
        self.plan = QueryPlan::Panel(Box::new(panel.clone()));
        self.query = panel.expr.clone();
        self.query_kind = QueryKind::Prom;
        if let Some(why) = panel.unsupported.take() {
            self.cancel_query(cx);
            self.panels_expanded = true;
            let source = match panel.identity {
                GrafanaRowIdentity::Panel { .. } => "Unsupported panel",
                GrafanaRowIdentity::Dashboard => "Dashboard",
            };
            self.status = failure_notice(source, &why);
            self.query_failure = Some(failure_details(source, why));
            cx.notify();
            return;
        }
        if !panel_semantics_supported(&panel) {
            self.cancel_query(cx);
            self.panels_expanded = true;
            self.status = "Unsupported panel semantics; use Open in Grafana or Raw query".into();
            cx.notify();
            return;
        }
        if panel.query_source.is_none() {
            self.cancel_query(cx);
            self.panels_expanded = true;
            self.status = GRAFANA_SOURCE_UNVERIFIED.into();
            cx.notify();
            return;
        }
        let Some(scope) = self.scope.clone() else {
            self.cancel_query(cx);
            self.scope_stop = None;
            self.scope_generation += 1;
            self.resolve(cx);
            return;
        };
        let generation = self.begin_query(cx);
        let (tx, rx) = futures::channel::oneshot::channel();
        let title = panel_query_title(&panel);
        let unit = panel.unit.clone();
        let reducer = (panel.kind == GrafanaPanelKind::Stat).then(|| panel.reducer.clone());
        self.query_stop = Some(self.provider.query_panel(
            panel,
            scope,
            self.window,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        ));
        self.receive_prom(rx, generation, title, unit, reducer, false, cx);
    }

    fn open_grafana(&mut self, cx: &mut Context<Self>) {
        if self.grafana == ToolPresence::Missing {
            return;
        }
        if !self.panels_expanded {
            self.focus_table(ObserveTable::Panels, cx);
            return;
        }
        let Some(index) = self
            .table
            .selected_row()
            .and_then(|row| catalog_row_index(&row.uid, self.catalog_revision))
        else {
            return;
        };
        let panel = self.panels.get(index);
        if let Some(panel) = panel
            && let Some(request) = &panel.browser_forward
            && let Some(path) = grafana_browser_path(panel)
        {
            cx.emit(ObserveEvent::GrafanaRoute {
                request: request.clone(),
                path,
            });
            return;
        }
        self.query_failure = None;
        self.status = match panel.and_then(|panel| panel.browser_url.as_deref()) {
            Some(url) if browser_url_allowed(url) => {
                cx.open_url(url);
                "Grafana route handed to the system browser".into()
            }
            _ => "No browser URL is configured; bind a reachable Grafana URL or service forward"
                .into(),
        };
        cx.notify();
    }

    fn raw_query(&mut self, cx: &mut Context<Self>) {
        if self.query.trim().is_empty() {
            self.cancel_query(cx);
            self.status = "Enter a query".into();
            cx.notify();
            return;
        }
        if matches!(self.plan, QueryPlan::SpanLogs(_)) {
            self.scope_label = "Kubernetes scope is not resolved".into();
            self.scope_details = None;
        }
        if self.request.is_none() {
            self.scope_label = "No current Kubernetes selection".into();
            self.scope_details = None;
        }
        match self.query_kind {
            QueryKind::Prom => {
                self.plan = QueryPlan::Prom(self.query.clone());
                let generation = self.begin_query(cx);
                let (tx, rx) = futures::channel::oneshot::channel();
                self.query_stop = Some(self.provider.query_promql(
                    PromRequest {
                        expr: self.query.clone(),
                        window: self.window,
                    },
                    Box::new(move |outcome| {
                        let _ = tx.send(outcome);
                    }),
                ));
                self.receive_prom(
                    rx,
                    generation,
                    "Raw PromQL".into(),
                    String::new(),
                    None,
                    false,
                    cx,
                );
            }
            QueryKind::Loki => {
                self.plan = QueryPlan::Loki(self.query.clone());
                self.run_loki(cx);
            }
        }
    }

    fn scoped_logs(&mut self, otlp: bool, cx: &mut Context<Self>) {
        self.plan = QueryPlan::ScopedLogs(otlp);
        if self.scope.is_none() {
            self.cancel_query(cx);
            self.scope_stop = None;
            self.scope_generation += 1;
            self.resolve(cx);
            return;
        }
        let query = self.scope.as_ref().and_then(|scope| {
            if otlp {
                scope.otlp_logs_query()
            } else {
                scope.logs_query()
            }
        });
        let Some(query) = query else {
            self.cancel_query(cx);
            self.status = "No current pods in scope".into();
            cx.notify();
            return;
        };
        self.query = query;
        self.query_kind = QueryKind::Loki;
        self.run_loki(cx);
    }

    pub(crate) fn prepare_entry(&mut self, entry: ObserveEntry, cx: &mut Context<Self>) {
        if matches!(&entry, ObserveEntry::General) && self.catalogue_enabled {
            return;
        }
        self.cancel_query(cx);
        self.visible = false;
        self.entry_pending = true;
        self.generation += 1;
        self.scope_generation += 1;
        self.scope_stop = None;
        self.catalog_stop = None;
        self.probe_stop = None;
        if self.catalog_pending {
            self.catalog_notice = SharedString::default();
            self.catalog_details = None;
        }
        self.catalog_pending = false;
        self.probe_pending = false;
        self.typing = false;
        self.filtering = false;
        self.scope = None;
        self.scope_details = None;
        self.source_details = None;
        match entry {
            ObserveEntry::General => self.catalogue_enabled = true,
            ObserveEntry::SpanLogs(query) => {
                self.catalogue_enabled = false;
                self.scope_label =
                    format!("Recorded span resource: {}/{}", query.namespace, query.pod).into();
                self.window = query.request.window;
                self.query.clone_from(&query.request.query);
                self.query_kind = QueryKind::Loki;
                self.plan = QueryPlan::SpanLogs(query);
            }
            ObserveEntry::RetainedLogs(request) => {
                self.catalogue_enabled = false;
                self.request = Some(request);
                self.window = recent_window(self.range_ms, self.window.step_ms);
                self.query_kind = QueryKind::Loki;
                self.plan = QueryPlan::ScopedLogs(true);
            }
        }
    }

    fn open_catalog_details(&mut self, cx: &mut Context<Self>) {
        if let Some(notices) = self.catalog_details.clone() {
            self.prepare_notice_open(cx);
            cx.emit(ObserveEvent::CatalogDetails(notices));
        }
    }

    fn open_scope_details(&mut self, cx: &mut Context<Self>) {
        if let Some(notices) = self.scope_details.clone() {
            self.prepare_notice_open(cx);
            cx.emit(ObserveEvent::ScopeDetails(notices));
        }
    }

    fn open_source_details(&mut self, cx: &mut Context<Self>) {
        if let Some(notices) = self.source_details.clone() {
            self.prepare_notice_open(cx);
            cx.emit(ObserveEvent::SourceDetails(notices));
        }
    }

    fn open_query_details(&mut self, cx: &mut Context<Self>) {
        let details = self
            .query_failure
            .clone()
            .or_else(|| self.chart.read(cx).notice_details());
        if let Some(details) = details {
            self.prepare_notice_open(cx);
            cx.emit(ObserveEvent::QueryDetails(details));
        }
    }

    fn run_loki(&mut self, cx: &mut Context<Self>) {
        let identity = self.begin_query(cx);
        self.select_table(ObserveTable::Logs);
        let (tx, rx) = futures::channel::oneshot::channel();
        let request = match &self.plan {
            QueryPlan::SpanLogs(query) => query.request.clone(),
            _ => LokiRequest {
                query: self.query.clone(),
                window: self.window,
                limit: 200,
            },
        };
        self.query_stop = Some(self.provider.query_loki(
            request,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        ));
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if !identity.current(
                        this.query_generation,
                        this.request.as_ref(),
                        this.window,
                        this.visible,
                    ) {
                        return;
                    }
                    this.query_stop = None;
                    match outcome {
                        LokiOutcome::Logs {
                            entries,
                            truncated,
                            dropped_streams,
                            dropped_lines,
                            clipped_lines,
                            limit_reached,
                            source,
                        } => {
                            this.loki = ToolPresence::Ready;
                            if !this.catalogue_enabled {
                                cx.emit(ObserveEvent::Presence(ToolPresence::Ready));
                            }
                            this.status = format!(
                                "{}: {} lines{}{}; {dropped_streams} streams omitted, \
                                {dropped_lines} lines omitted, {clipped_lines} lines clipped",
                                display_message(&source),
                                entries.len(),
                                if truncated { "; truncated" } else { "" },
                                if limit_reached {
                                    "; query limit reached"
                                } else {
                                    ""
                                },
                            )
                            .into();
                            if let QueryPlan::SpanLogs(query) = &this.plan {
                                let relation = if query.pod_uid.is_empty() {
                                    "query filters name and trace ID; Kubernetes UID unavailable"
                                } else {
                                    "query filters trace resource UID and trace ID"
                                };
                                this.status = format!(
                                    "{}; trace {}, span {}; {relation}; {} to {} UTC",
                                    this.status,
                                    query.trace_id,
                                    query.span_id,
                                    crate::chart::clock(query.request.window.start_ms),
                                    crate::chart::clock(query.request.window.end_ms),
                                )
                                .into();
                            }
                            let origin = match &this.plan {
                                QueryPlan::SpanLogs(query) => Some(query.as_ref()),
                                _ => None,
                            };
                            this.log_table.set_page(logs_page(
                                &entries,
                                this.scope.as_ref(),
                                origin,
                            ));
                            this.logs = entries;
                        }
                        LokiOutcome::Absent => {
                            this.loki = ToolPresence::Missing;
                            this.status = "Loki is not served by this cluster".into();
                        }
                        LokiOutcome::Denied(what) => {
                            this.loki = ToolPresence::Blocked;
                            this.status = format!("{what}: access denied for this account").into();
                        }
                        LokiOutcome::Failed(why) => {
                            this.loki = ToolPresence::Blocked;
                            this.status = failure_notice("Loki query failed", &why);
                            this.query_failure = Some(NoticeSnapshot::new(QueryFailure::new(
                                "Loki query failed",
                                "Requested LogQL",
                                this.query.clone(),
                                identity.window,
                                why,
                            )));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn follow_trace(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.log_table.selected_row() else {
            return;
        };
        let Some(entry) = self
            .logs
            .get(row.uid.parse::<usize>().unwrap_or(usize::MAX))
        else {
            return;
        };
        if let Some(id) = &entry.trace_id {
            cx.emit(ObserveEvent::Trace {
                id: id.clone(),
                scope: self.scope.clone(),
            });
        } else {
            self.status = "This line has no trace ID".into();
            cx.notify();
        }
    }

    fn keyboard_table(&self, window: &Window) -> Option<ObserveTable> {
        if self.focus.is_focused(window) {
            self.visible_table()
        } else {
            None
        }
    }

    fn active_table(&mut self, window: &Window) -> Option<&mut TableState> {
        let table = self.keyboard_table(window)?;
        Some(table.state(&mut self.table, &mut self.log_table))
    }

    fn edit_query(&mut self, cx: &mut Context<Self>) {
        self.cancel_query(cx);
        self.typing = true;
        self.filtering = false;
        self.status = "Query edited; run explicitly".into();
        cx.notify();
    }
}

impl crate::item::Item for ObserveView {
    fn title(&self) -> SharedString {
        self.title()
    }
    fn focus_handle(&self) -> FocusHandle {
        self.focus_handle()
    }
    fn visibility_changed(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            if self.entry_pending
                || (self.catalogue_enabled && (self.catalog_pending || self.probe_pending))
            {
                self.reload(cx);
            }
        } else {
            self.entry_pending |= self.scope_stop.is_some() || self.query_stop.is_some();
            self.generation += 1;
            self.scope_generation += 1;
            advance_query(&mut self.query_generation, &mut self.scope_query, false);
            if self.scope_stop.take().is_some() {
                self.scope = None;
            }
            self.catalog_stop = None;
            self.probe_stop = None;
            if self.query_stop.take().is_some() {
                self.status =
                    "Request cancelled while this pane is hidden; refresh to retry".into();
            }
            self.chart.update(cx, |chart, _| chart.pause_preparation());
        }
    }
}

impl Render for ObserveView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.filtering && self.visible_table().is_none() {
            self.filtering = false;
        }
        if self.panels_expanded {
            self.table_text.update(&self.table);
        }
        self.log_text.update(&self.log_table);
        let cached_query = if self.query_label_typing {
            self.query_label.strip_suffix('_').unwrap_or("")
        } else {
            self.query_label.as_ref()
        };
        if self.query_label_typing != self.typing || cached_query != self.query {
            self.query_label = if self.typing {
                format!("{}_", self.query).into()
            } else {
                self.query.clone().into()
            };
            self.query_label_typing = self.typing;
        }
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let available = self.notice_availability(cx);
        if self
            .notice_focus
            .iter()
            .zip(available)
            .any(|(handle, available)| !available && handle.is_focused(window))
        {
            window.focus(&self.focus, cx);
        }
        let notice_focused = self
            .notice_focus
            .iter()
            .any(|handle| handle.is_focused(window));
        let key_context = if notice_focused {
            if self.typing || self.filtering {
                self.notice_input_context.clone()
            } else {
                self.notice_context.clone()
            }
        } else {
            gpui::KeyContext::parse(observe_key_context(self.typing, self.filtering, false))
                .expect("observe context")
        };
        let query_details_available = available[3];
        let button = |id, label| {
            div()
                .id(id)
                .px(px(8.0))
                .py(px(4.0))
                .cursor_pointer()
                .whitespace_nowrap()
                .hover(|style| style.bg(rgb(theme.shell.element_hover)))
                .child(label)
        };
        div()
            .id("observe")
            .key_context(key_context)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(12.0))
            .bg(rgb(theme.shell.editor_background))
            .font_family(fonts.ui_family.clone())
            .text_size(px(fonts.ui_size))
            .text_color(rgb(theme.shell.text))
            .on_action(cx.listener(|this, _: &ObserveRangeFifteenMinutes, _, cx| {
                this.set_range(900_000, cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveRangeOneHour, _, cx| {
                this.set_range(3_600_000, cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveToggleResolution, _, cx| {
                this.toggle_resolution(cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveToggleDialect, _, cx| {
                this.toggle_dialect(cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveReadiness, _, cx| {
                this.run_readiness(cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveOtlpLogs, _, cx| {
                this.scoped_logs(true, cx);
            }))
            .on_action(cx.listener(|this, _: &ObservePodLabelLogs, _, cx| {
                this.scoped_logs(false, cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveOpenGrafana, window, cx| {
                if this.focus.is_focused(window) {
                    this.open_grafana(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ObserveFocusPanels, window, cx| {
                if this.focus_table(ObserveTable::Panels, cx) {
                    window.focus(&this.focus, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ObserveFocusLogs, window, cx| {
                if this.focus_table(ObserveTable::Logs, cx) {
                    window.focus(&this.focus, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ObserveFocusNext, window, cx| {
                this.focus_next(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveFocusPrevious, window, cx| {
                this.focus_next(false, window, cx);
            }))
            .when(self.catalog_details.is_some(), |view| {
                view.on_action(cx.listener(|this, _: &ObserveCatalogDetails, _, cx| {
                    this.open_catalog_details(cx);
                }))
            })
            .when(self.scope_details.is_some(), |view| {
                view.on_action(cx.listener(|this, _: &ObserveScopeDetails, _, cx| {
                    this.open_scope_details(cx);
                }))
            })
            .when(self.source_details.is_some(), |view| {
                view.on_action(cx.listener(|this, _: &ObserveSourceDetails, _, cx| {
                    this.open_source_details(cx);
                }))
            })
            .when(query_details_available, |view| {
                view.on_action(cx.listener(|this, _: &OpenQueryDetails, _, cx| {
                    this.open_query_details(cx);
                }))
            })
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.reload(cx)))
            .on_action(cx.listener(|this, _: &RowUp, window, cx| {
                if let Some(table) = this.active_table(window) {
                    table.move_selection(-1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &RowDown, window, cx| {
                if let Some(table) = this.active_table(window) {
                    table.move_selection(1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &OpenRow, window, cx| {
                if !this.focus.is_focused(window) {
                    return;
                }
                if this.typing {
                    this.typing = false;
                    this.raw_query(cx);
                } else {
                    match table_open_action(
                        this.table_focus,
                        this.grafana != ToolPresence::Missing,
                        this.panels_expanded,
                        !this.logs.is_empty(),
                    ) {
                        Some(OpenTableAction::ExpandPanels) => {
                            this.focus_table(ObserveTable::Panels, cx);
                        }
                        Some(OpenTableAction::RunPanel) => this.run_selected(cx),
                        Some(OpenTableAction::FollowTrace) => this.follow_trace(cx),
                        None => {}
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &CommitInput, window, cx| {
                if !this.focus.is_focused(window) {
                    return;
                }
                if this.typing {
                    this.typing = false;
                    this.raw_query(cx);
                }
                this.filtering = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelInput, window, cx| {
                let notice_focused = this
                    .notice_focus
                    .iter()
                    .any(|handle| handle.is_focused(window));
                match cancel_input_action(
                    this.focus.is_focused(window),
                    notice_focused,
                    this.typing || this.filtering,
                ) {
                    CancelInputAction::Ignore => {}
                    CancelInputAction::LeaveInput => this.leave_input(cx),
                    CancelInputAction::FocusRoot => window.focus(&this.focus, cx),
                }
            }))
            .on_action(cx.listener(|this, _: &EnterFilter, window, cx| {
                if this.keyboard_table(window).is_none() {
                    return;
                }
                window.focus(&this.focus, cx);
                this.filtering = true;
                this.typing = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, window, cx| {
                if this.typing && this.focus.is_focused(window) {
                    this.query.pop();
                    this.cancel_query(cx);
                    cx.notify();
                } else if this.filtering
                    && let Some(table) = this.active_table(window)
                {
                    table.pop_filter();
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.control || event.keystroke.modifiers.alt {
                    return;
                }
                if this.typing || this.filtering {
                    if !this.focus.is_focused(window) {
                        return;
                    }
                    if let Some(character) = &event.keystroke.key_char {
                        if this.typing {
                            this.query.push_str(character);
                            this.cancel_query(cx);
                            cx.notify();
                        } else if let Some(table) = this.active_table(window) {
                            table.push_filter(character);
                            cx.notify();
                        }
                        cx.stop_propagation();
                    }
                } else if event.keystroke.key == "q" {
                    window.focus(&this.focus, cx);
                    this.edit_query(cx);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &RowPageUp, window, cx| {
                if let Some(table) = this.active_table(window) {
                    table.page_by(-1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &RowPageDown, window, cx| {
                if let Some(table) = this.active_table(window) {
                    table.page_by(1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &RowHome, window, cx| {
                if let Some(table) = this.active_table(window) {
                    table.select_first();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &RowEnd, window, cx| {
                if let Some(table) = this.active_table(window) {
                    table.select_last();
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(self.scope_label.clone()),
                    )
                    .children(self.scope_details.as_ref().map(|_| {
                        button("scope-notice-details", "Details  Ctrl Alt K")
                            .flex_none()
                            .track_focus(&self.notice_focus[0])
                            .when(self.notice_focus[0].is_focused(window), |button| {
                                button.bg(rgb(theme.shell.element_hover))
                            })
                            .role(Role::Button)
                            .aria_label("Open Kubernetes scope details")
                            .aria_description("Use Ctrl Alt K outside query or filter input")
                            .aria_keyshortcuts("Ctrl+Alt+K")
                            .capture_any_mouse_down(cx.listener(|this, _, window, _| {
                                this.preserve_input_focus(window);
                            }))
                            .on_click(cx.listener(|this, event, window, cx| {
                                if this.focus_notice_on_activation(0, event, window, cx) {
                                    this.open_scope_details(cx);
                                }
                            }))
                    })),
            )
            .children((!self.reach_notice.is_empty()).then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(self.reach_notice.clone()),
                    )
                    .children(self.source_details.as_ref().map(|_| {
                        button("source-notice-details", "Details  Ctrl Alt D")
                            .flex_none()
                            .track_focus(&self.notice_focus[1])
                            .when(self.notice_focus[1].is_focused(window), |button| {
                                button.bg(rgb(theme.shell.element_hover))
                            })
                            .role(Role::Button)
                            .aria_label("Open source discovery details")
                            .aria_description("Use Ctrl Alt D outside query or filter input")
                            .aria_keyshortcuts("Ctrl+Alt+D")
                            .capture_any_mouse_down(cx.listener(|this, _, window, _| {
                                this.preserve_input_focus(window);
                            }))
                            .on_click(cx.listener(|this, event, window, cx| {
                                if this.focus_notice_on_activation(1, event, window, cx) {
                                    this.open_source_details(cx);
                                }
                            }))
                    }))
            }))
            .children((!self.catalog_notice.is_empty()).then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .child(self.catalog_notice.clone()),
                    )
                    .children(self.catalog_details.as_ref().map(|_| {
                        button("catalog-notice-details", "Details  Ctrl Alt G")
                            .flex_none()
                            .track_focus(&self.notice_focus[2])
                            .when(self.notice_focus[2].is_focused(window), |button| {
                                button.bg(rgb(theme.shell.element_hover))
                            })
                            .role(Role::Button)
                            .aria_label("Open Grafana notice details")
                            .aria_description("Use Ctrl Alt G outside query or filter input")
                            .aria_keyshortcuts("Ctrl+Alt+G")
                            .capture_any_mouse_down(cx.listener(|this, _, window, _| {
                                this.preserve_input_focus(window);
                            }))
                            .on_click(cx.listener(|this, event, window, cx| {
                                if this.focus_notice_on_activation(2, event, window, cx) {
                                    this.open_catalog_details(cx);
                                }
                            }))
                    }))
            }))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .flex_wrap()
                    .child(button("range-15m", "15 minutes").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.set_range(900_000, cx);
                        },
                    )))
                    .child(
                        button("range-1h", "1 hour").on_click(cx.listener(|this, _, _, cx| {
                            this.set_range(3_600_000, cx);
                        })),
                    )
                    .child(
                        button(
                            "step",
                            if self.window.step_ms == 15_000 {
                                "15s resolution"
                            } else {
                                "30s resolution"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_resolution(cx);
                        })),
                    )
                    .child(
                        button("observe-refresh", "Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .children((self.prometheus != ToolPresence::Missing).then(|| {
                        button("readiness", "Pod readiness")
                            .on_click(cx.listener(|this, _, _, cx| this.run_readiness(cx)))
                    }))
                    .children((self.loki != ToolPresence::Missing).then(|| {
                        button("otlp-logs", "OTLP logs")
                            .on_click(cx.listener(|this, _, _, cx| this.scoped_logs(true, cx)))
                    }))
                    .children((self.loki != ToolPresence::Missing).then(|| {
                        button("pod-logs", "Pod label logs")
                            .on_click(cx.listener(|this, _, _, cx| this.scoped_logs(false, cx)))
                    })),
            )
            .children((self.grafana != ToolPresence::Missing).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        button(
                            "panel-catalog-toggle",
                            match (
                                self.panels_expanded,
                                self.catalogue_enabled && !self.catalog_pending,
                            ) {
                                (false, true) => "Show Grafana panels",
                                (true, true) => "Hide Grafana panels",
                                (false, false) => "Show cached Grafana panels",
                                (true, false) => "Hide cached Grafana panels",
                            },
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.panels_expanded = !this.panels_expanded;
                            if this.panels_expanded {
                                this.select_table(ObserveTable::Panels);
                                window.focus(&this.focus, cx);
                            } else if this.table_focus == Some(ObserveTable::Panels) {
                                this.filtering = false;
                            }
                            cx.notify();
                        })),
                    )
                    .when(self.panels_expanded, |catalog| {
                        catalog
                            .on_scroll_wheel(cx.listener(
                                |this, event: &ScrollWheelEvent, _, cx| {
                                    this.table_focus = table_after_scroll(
                                        this.table_focus,
                                        ObserveTable::Panels,
                                        this.filtering,
                                    );
                                    let delta = f32::from(event.delta.pixel_delta(px(24.0)).y);
                                    this.table.move_selection(-(delta / 24.0).round() as i64);
                                    cx.stop_propagation();
                                    cx.notify();
                                },
                            ))
                            .child(
                                div()
                                    .font_family(fonts.buffer_family.clone())
                                    .text_color(rgb(theme.shell.text_muted))
                                    .child(SharedString::from(self.table_text.header())),
                            )
                            .children(self.table_text.lines().iter().enumerate().map(
                                |(offset, line)| {
                                    let selected = self.table.line_selected(offset);
                                    div()
                                        .id(("panel", offset))
                                        .font_family(fonts.buffer_family.clone())
                                        .px(px(6.0))
                                        .py(px(3.0))
                                        .overflow_hidden()
                                        .cursor_pointer()
                                        .bg(rgb(if selected {
                                            theme.shell.element_selected
                                        } else {
                                            theme.shell.editor_background
                                        }))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                this.select_table(ObserveTable::Panels);
                                                this.table.select_visible_offset(offset);
                                                window.focus(&this.focus, cx);
                                                cx.notify();
                                            }),
                                        )
                                        .child(SharedString::from(line.clone()))
                                },
                            ))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.0))
                                    .child(button("panel-run", "Run panel").on_click(
                                        cx.listener(|this, _, _, cx| this.run_selected(cx)),
                                    ))
                                    .child(button("grafana-open", "Open in Grafana").on_click(
                                        cx.listener(|this, _, _, cx| this.open_grafana(cx)),
                                    )),
                            )
                    })
            }))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(button("query-edit", "Raw query").on_click(cx.listener(
                        |this, _, window, cx| {
                            window.focus(&this.focus, cx);
                            this.edit_query(cx);
                        },
                    )))
                    .child(
                        button(
                            "query-dialect",
                            match self.query_kind {
                                QueryKind::Prom => "PromQL",
                                QueryKind::Loki => "LogQL",
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_dialect(cx);
                        })),
                    )
                    .child(
                        button("query-run", "Run raw query")
                            .on_click(cx.listener(|this, _, _, cx| this.raw_query(cx))),
                    ),
            )
            .child(
                div()
                    .overflow_hidden()
                    .font_family(fonts.buffer_family.clone())
                    .child(self.query_label.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w(px(0.0))
                    .child(div().flex_1().min_w(px(0.0)).child(self.status.clone()))
                    .children(query_details_available.then(|| {
                        button("query-notice-details", "Details  Ctrl Alt Q")
                            .flex_none()
                            .track_focus(&self.notice_focus[3])
                            .when(self.notice_focus[3].is_focused(window), |button| {
                                button.bg(rgb(theme.shell.element_hover))
                            })
                            .role(Role::Button)
                            .aria_label("Open query details")
                            .aria_description("Use Ctrl Alt Q outside query or filter input")
                            .aria_keyshortcuts("Ctrl+Alt+Q")
                            .capture_any_mouse_down(cx.listener(|this, _, window, _| {
                                this.preserve_input_focus(window);
                            }))
                            .on_click(cx.listener(|this, event, window, cx| {
                                if this.focus_notice_on_activation(3, event, window, cx) {
                                    this.open_query_details(cx);
                                }
                            }))
                    })),
            )
            .child(
                div()
                    .when(self.focus.is_focused(window), |label| {
                        label.bg(rgb(theme.shell.element_hover))
                    })
                    .child(
                        self.visible_table()
                            .map_or("No table selected", ObserveTable::label),
                    ),
            )
            .child(
                div()
                    .id("observe-result")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .children(
                        self.chart
                            .read(cx)
                            .data()
                            .is_some()
                            .then(|| self.chart.clone()),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        if !this.logs.is_empty() {
                            this.table_focus = table_after_scroll(
                                this.table_focus,
                                ObserveTable::Logs,
                                this.filtering,
                            );
                            let delta = f32::from(event.delta.pixel_delta(px(28.0)).y);
                            this.log_table
                                .move_selection(-(delta / 28.0).round() as i64);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .children((!self.logs.is_empty()).then(|| {
                        div()
                            .font_family(fonts.buffer_family.clone())
                            .text_color(rgb(theme.shell.text_muted))
                            .child(SharedString::from(self.log_text.header()))
                    }))
                    .children(
                        self.log_text
                            .lines()
                            .iter()
                            .enumerate()
                            .map(|(offset, line)| {
                                let selected = self.log_table.line_selected(offset);
                                div()
                                    .id(("log", offset))
                                    .font_family(fonts.buffer_family.clone())
                                    .overflow_hidden()
                                    .px(px(6.0))
                                    .py(px(4.0))
                                    .bg(rgb(if selected {
                                        theme.shell.element_selected
                                    } else {
                                        theme.shell.editor_background
                                    }))
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            this.select_table(ObserveTable::Logs);
                                            this.log_table.select_visible_offset(offset);
                                            window.focus(&this.focus, cx);
                                            cx.notify();
                                        }),
                                    )
                                    .child(SharedString::from(line.clone()))
                            }),
                    )
                    .children((!self.logs.is_empty()).then(|| {
                        button("follow-trace", "Open selected trace")
                            .on_click(cx.listener(|this, _, _, cx| this.follow_trace(cx)))
                    })),
            )
    }
}

fn observe_key_context(typing: bool, filtering: bool, notice_focused: bool) -> &'static str {
    if notice_focused && (typing || filtering) {
        "Typing ObserveNoticeControl"
    } else if notice_focused {
        "Browse Observe ObserveNoticeControl"
    } else if typing || filtering {
        "Typing ObserveInput"
    } else {
        "Browse Observe"
    }
}

fn surface_presence(
    catalogue_enabled: bool,
    pending: bool,
    grafana: ToolPresence,
    prometheus: ToolPresence,
    loki: ToolPresence,
) -> Option<ToolPresence> {
    if !catalogue_enabled || pending {
        return None;
    }
    let tools = [grafana, prometheus, loki];
    Some(
        if tools
            .iter()
            .all(|presence| *presence == ToolPresence::Missing)
        {
            ToolPresence::Missing
        } else if tools.contains(&ToolPresence::Ready) {
            ToolPresence::Ready
        } else {
            ToolPresence::Blocked
        },
    )
}

fn recent_window(range_ms: i64, step_ms: u64) -> QueryWindow {
    let end_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    QueryWindow {
        start_ms: (end_ms - range_ms).max(0),
        end_ms,
        step_ms,
    }
}
fn catalog_row_index(uid: &str, revision: u64) -> Option<usize> {
    let (row_revision, index) = uid.split_once(':')?;
    if row_revision.parse::<u64>().ok()? != revision {
        return None;
    }
    index.parse::<usize>().ok()
}

fn grafana_browser_path(panel: &GrafanaPanelRow) -> Option<String> {
    if panel.dashboard_uid.is_empty() {
        return Some("/".to_string());
    }
    match (panel.source, panel.identity) {
        (GrafanaRowSource::Grafana, GrafanaRowIdentity::Panel { id: Some(id), .. }) => {
            k10s_provider::grafana_panel_path(&panel.dashboard_uid, id)
        }
        _ => k10s_provider::grafana_dashboard_path(&panel.dashboard_uid),
    }
}

fn panel_query_title(panel: &GrafanaPanelRow) -> String {
    match panel.source {
        GrafanaRowSource::Grafana => panel.title.clone(),
        GrafanaRowSource::ConfigMap => format!("ConfigMap: {}", panel.title),
        GrafanaRowSource::Unknown => format!("Unverified dashboard: {}", panel.title),
    }
}

fn panel_semantics_supported(panel: &GrafanaPanelRow) -> bool {
    matches!(
        panel.kind,
        GrafanaPanelKind::Timeseries | GrafanaPanelKind::Stat
    ) && panel.dialect == QueryDialect::PromQL
        && !panel.transformed
}

const GRAFANA_SOURCE_UNVERIFIED: &str =
    "Grafana source is unverified or refreshing; refresh the catalogue before querying";

fn panel_state(panel: &GrafanaPanelRow) -> String {
    let unsupported = panel
        .unsupported
        .as_deref()
        .or_else(|| {
            panel
                .query_source
                .is_none()
                .then_some(GRAFANA_SOURCE_UNVERIFIED)
        })
        .or_else(|| {
            (!panel_semantics_supported(panel))
                .then_some("Unsupported panel semantics; use Open in Grafana or Raw query")
        });
    let link_notice = if panel.dashboard_uid.is_empty() {
        Some("Dashboard UID unavailable; no verified deep link")
    } else if panel.source != GrafanaRowSource::Grafana {
        Some("Panel not verified in Grafana")
    } else if matches!(panel.identity, GrafanaRowIdentity::Panel { id: None, .. }) {
        Some("Panel ID unavailable; no panel deep link")
    } else {
        None
    };
    if let Some(link_notice) = link_notice {
        let mut state = link_notice.to_string();
        if let Some(why) = unsupported {
            state.push_str("; ");
            state.push_str(&display_message(why));
        } else if panel.source != GrafanaRowSource::Grafana {
            state.push_str("; query supported");
        }
        state
    } else {
        unsupported.map_or_else(|| "Supported".to_string(), display_message)
    }
}

fn panels_page(panels: &[GrafanaPanelRow], truncated: bool, revision: u64) -> TablePage {
    TablePage {
        columns: [
            "Dashboard",
            "Source",
            "Panel",
            "Target",
            "Datasource",
            "State",
        ]
        .into_iter()
        .map(|name| TableColumn {
            name: name.into(),
            wide: false,
        })
        .collect(),
        rows: panels
            .iter()
            .take(MAX_ROWS)
            .enumerate()
            .map(|(index, panel)| TableRow {
                name: panel.title.clone(),
                namespace: Some(panel.dashboard_title.clone()),
                uid: format!("{revision}:{index}"),
                cells: vec![
                    panel.dashboard_title.clone(),
                    match (panel.source, panel.identity) {
                        (GrafanaRowSource::Grafana, GrafanaRowIdentity::Dashboard) => {
                            "Grafana search"
                        }
                        (GrafanaRowSource::Grafana, _) => "Grafana API",
                        (GrafanaRowSource::ConfigMap, _) => "ConfigMap",
                        (GrafanaRowSource::Unknown, _) => "Unknown",
                    }
                    .to_string(),
                    panel.title.clone(),
                    panel.ref_id.clone(),
                    panel.datasource_uid.clone().unwrap_or_default(),
                    panel_state(panel),
                ],
            })
            .collect(),
        truncated: truncated || panels.len() > MAX_ROWS,
        continue_token: None,
    }
}

fn catalog_notices(
    entries: usize,
    truncated: bool,
    table_omitted: usize,
    mut notices: Vec<String>,
) -> (SharedString, Option<NoticeSnapshot>) {
    if truncated {
        notices.insert(
            0,
            format!(
                "Grafana source or dashboard data was truncated; {entries} entries in the table"
            ),
        );
    }
    if table_omitted > 0 {
        let summary = if truncated {
            format!("{table_omitted} entries omitted by the table limit")
        } else {
            format!(
                "{entries} entries in the table; {table_omitted} entries omitted by the table limit"
            )
        };
        notices.insert(usize::from(truncated), summary);
    }
    if !truncated && table_omitted == 0 && entries == 0 {
        let summary = if notices.is_empty() {
            "No Grafana query panels returned"
        } else {
            "No readable Grafana query panels; the catalog may be incomplete"
        };
        notices.insert(0, summary.into());
    }
    let summary = display_notice_groups(&[&notices]).into();
    let details = (!notices.is_empty()).then(|| NoticeSnapshot::new(notices));
    (summary, details)
}

fn failure_notice(source: &str, why: &str) -> SharedString {
    format!("{source}: {}", display_message(why)).into()
}

fn prom_notices(
    mut warnings: Vec<String>,
    mut infos: Vec<String>,
    association: String,
    dropped_series: Option<usize>,
    possible_server_limit: Option<usize>,
    warnings_omitted: usize,
    infos_omitted: usize,
) -> Vec<String> {
    let mut notices = vec![association];
    if let Some(dropped_series) = dropped_series {
        notices.push(format!(
            "{dropped_series} returned series omitted locally; query data is incomplete"
        ));
    }
    if let Some(limit) = possible_server_limit {
        notices.push(format!("Prometheus range query reached the requested {limit}-series limit; completeness is unknown"));
    }
    if warnings_omitted > 0 {
        notices.push(format!(
            "{warnings_omitted} Prometheus warning annotations omitted by the local limit"
        ));
    }
    if infos_omitted > 0 {
        notices.push(format!(
            "{infos_omitted} Prometheus info annotations omitted by the local limit"
        ));
    }
    if !warnings.is_empty() {
        notices.push("Prometheus warnings".into());
        notices.append(&mut warnings);
    }
    if !infos.is_empty() {
        notices.push("Prometheus info".into());
        notices.append(&mut infos);
    }
    notices
}
fn metric_association(
    scope: Option<&ObserveScope>,
    series: &[PromSeriesView],
    scoped_by_uid: bool,
) -> String {
    if series.is_empty() {
        return "No returned series to associate with a Kubernetes UID".into();
    }
    let Some(scope) = scope else {
        return "Kubernetes UID association is unproven".into();
    };
    let matched = series
        .iter()
        .filter(|series| {
            let attribute = |names: &[&str]| {
                names.iter().find_map(|name| {
                    series
                        .label_set
                        .iter()
                        .find(|(key, _)| key.as_str() == *name)
                        .map(|(_, value)| value.as_str())
                })
            };
            attribute(&["k8s_namespace_name", "namespace"]) == Some(scope.namespace.as_str())
                && scope.pods.iter().any(|pod| {
                    attribute(&["k8s_pod_name", "pod"]) == Some(pod.name.as_str())
                        && attribute(&["k8s_pod_uid", "uid"]) == Some(pod.uid.as_str())
                })
        })
        .count();
    if matched == series.len() {
        format!("All {matched} series labels match current pod UIDs")
    } else if matched > 0 {
        format!(
            "{matched}/{} series labels match current pod UIDs; other series are unverified",
            series.len()
        )
    } else if scoped_by_uid {
        "Query filters current pod UIDs; returned series labels do not prove their UID".into()
    } else {
        "Returned series lack a verified Kubernetes UID association".into()
    }
}

fn log_identity(entry: &LogEntry, scope: Option<&ObserveScope>) -> &'static str {
    let Some(uid) = entry
        .attribute("k8s_pod_uid")
        .or_else(|| entry.attribute("uid"))
    else {
        return if entry
            .attribute("k8s_pod_name")
            .or_else(|| entry.attribute("pod"))
            .is_some()
        {
            "Name only"
        } else {
            "No pod identity"
        };
    };
    let Some(scope) = scope else {
        return "UID association unproven";
    };
    let namespace = entry
        .attribute("k8s_namespace_name")
        .or_else(|| entry.attribute("namespace"));
    let name = entry
        .attribute("k8s_pod_name")
        .or_else(|| entry.attribute("pod"));
    if namespace == Some(scope.namespace.as_str())
        && scope
            .pods
            .iter()
            .any(|pod| name == Some(pod.name.as_str()) && uid == pod.uid.as_str())
    {
        "Selected UID"
    } else {
        "Other or unverified UID"
    }
}

fn span_log_identity(
    entry: &LogEntry,
    scope: Option<&ObserveScope>,
    origin: Option<&SpanLogQuery>,
) -> &'static str {
    if let Some(origin) = origin
        && !origin.pod_uid.is_empty()
        && entry.attribute("k8s_namespace_name") == Some(origin.namespace.as_str())
        && entry.attribute("k8s_pod_name") == Some(origin.pod.as_str())
        && entry.attribute("k8s_pod_uid") == Some(origin.pod_uid.as_str())
        && entry.trace_id.as_deref() == Some(origin.trace_id.as_str())
        && entry.timestamp_ns as u128 >= origin.request.window.start_ms as u128 * 1_000_000
        && (entry.timestamp_ns as u128) < origin.request.window.end_ms as u128 * 1_000_000
    {
        "Recorded span UID"
    } else {
        log_identity(entry, scope)
    }
}

fn logs_page(
    entries: &[LogEntry],
    scope: Option<&ObserveScope>,
    origin: Option<&SpanLogQuery>,
) -> TablePage {
    TablePage {
        columns: ["UTC", "Pod", "Identity", "Trace", "Line"]
            .into_iter()
            .map(|name| TableColumn {
                name: name.into(),
                wide: false,
            })
            .collect(),
        rows: entries
            .iter()
            .enumerate()
            .map(|(index, entry)| TableRow {
                name: entry.line.clone(),
                namespace: None,
                uid: index.to_string(),
                cells: vec![
                    crate::chart::clock((entry.timestamp_ns / 1_000_000) as i64),
                    entry
                        .attribute("k8s_pod_name")
                        .or_else(|| entry.attribute("pod"))
                        .unwrap_or("")
                        .into(),
                    span_log_identity(entry, scope, origin).into(),
                    entry.trace_id.clone().unwrap_or_default(),
                    entry.line.clone(),
                ],
            })
            .collect(),
        truncated: false,
        continue_token: None,
    }
}
pub(crate) fn browser_url_allowed(url: &str) -> bool {
    url.split_once("://").is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http")
    }) && !url.chars().any(|c| c.is_whitespace() || c.is_control())
        && !url.contains('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_rows_keep_unique_selection_and_only_use_returned_panel_ids() {
        let row = |identity, title: &str, expr: &str| GrafanaPanelRow {
            query_source: None,
            dashboard_uid: "pilot-native".into(),
            dashboard_title: "Pilot dashboard".into(),
            identity,
            source: GrafanaRowSource::Grafana,
            title: title.into(),
            kind: GrafanaPanelKind::Unsupported,
            expr: expr.into(),
            dialect: QueryDialect::Unknown,
            transformed: false,
            browser_url: None,
            browser_forward: None,
            datasource_uid: None,
            ref_id: "A".into(),
            unit: String::new(),
            reducer: String::new(),
            min_step: String::new(),
            min_interval: String::new(),
            unsupported: None,
        };
        let dashboard = row(GrafanaRowIdentity::Dashboard, "Dashboard", "");
        let panel = row(
            GrafanaRowIdentity::Panel {
                id: Some(0),
                position: 0,
            },
            "Zero panel",
            "up",
        );
        let first = row(
            GrafanaRowIdentity::Panel {
                id: None,
                position: 1,
            },
            "First missing ID",
            "first",
        );
        let mut second = row(
            GrafanaRowIdentity::Panel {
                id: None,
                position: 2,
            },
            "Second missing ID",
            "second",
        );
        let full_reason = "unsupported transformation ".repeat(100);
        second.unsupported = Some(full_reason.clone());
        assert_eq!(
            grafana_browser_path(&dashboard).as_deref(),
            Some("/d/pilot-native")
        );
        assert_eq!(
            grafana_browser_path(&panel).as_deref(),
            Some("/d/pilot-native?viewPanel=0")
        );
        assert_eq!(
            grafana_browser_path(&first).as_deref(),
            Some("/d/pilot-native")
        );
        let mut file = panel.clone();
        file.source = GrafanaRowSource::ConfigMap;
        assert_eq!(
            grafana_browser_path(&file).as_deref(),
            Some("/d/pilot-native")
        );
        assert!(panel_state(&file).starts_with("Panel not verified in Grafana"));
        file.dashboard_uid.clear();
        assert_eq!(grafana_browser_path(&file).as_deref(), Some("/"));
        assert!(panel_state(&file).starts_with("Dashboard UID unavailable"));
        let file_page = panels_page(&[file], false, 7);
        assert_eq!(file_page.rows[0].cells[1], "ConfigMap");
        let rows = vec![dashboard, panel, first, second];
        let page = panels_page(&rows, true, 7);
        assert!(page.truncated);
        assert_eq!(page.rows[0].cells[0], "Pilot dashboard");
        assert_eq!(page.rows[0].cells[1], "Grafana search");
        assert_eq!(page.rows[0].cells[2], "Dashboard");
        assert_eq!(page.rows[1].cells[1], "Grafana API");
        assert_eq!(page.rows[1].cells[2], "Zero panel");
        assert!(page.rows[2].cells[5].contains("Panel ID unavailable"));
        let state = &page.rows[3].cells[5];
        assert!(state.starts_with("Panel ID unavailable; no panel deep link; "));
        assert!(state.ends_with("[abbreviated]"));
        assert!(state.len() < full_reason.len());
        assert_eq!(rows[3].unsupported.as_deref(), Some(full_reason.as_str()));
        assert_ne!(page.rows[2].uid, page.rows[3].uid);
        let mut table = TableState::new();
        table.set_page(page);
        table.push_filter("Second missing ID");
        let uid = table.selected_row().expect("filtered row").uid.clone();
        let index = catalog_row_index(&uid, 7).expect("current catalog row");
        assert_eq!(index, 3);
        assert_eq!(rows[index].expr, "second");
        assert_eq!(catalog_row_index(&uid, 8), None);
        table.clear_filter();
        table.set_page(panels_page(&rows, true, 8));
        table.select_first();
        let selected = table.selected_row().expect("new catalog row");
        assert_eq!(catalog_row_index(&selected.uid, 8), Some(0));
    }

    #[test]
    fn catalog_table_reports_source_and_local_limits_without_materializing_extra_rows() {
        let panel = GrafanaPanelRow {
            query_source: None,
            dashboard_uid: "pilot".into(),
            dashboard_title: "Pilot".into(),
            identity: GrafanaRowIdentity::Panel {
                id: Some(1),
                position: 0,
            },
            source: GrafanaRowSource::Grafana,
            title: "CPU".into(),
            kind: GrafanaPanelKind::Timeseries,
            expr: "up".into(),
            dialect: QueryDialect::PromQL,
            transformed: false,
            browser_url: None,
            browser_forward: None,
            datasource_uid: None,
            ref_id: "A".into(),
            unit: String::new(),
            reducer: String::new(),
            min_step: String::new(),
            min_interval: String::new(),
            unsupported: None,
        };
        let rows = vec![panel; MAX_ROWS + 2];
        for count in [MAX_ROWS, MAX_ROWS + 2] {
            for source_truncated in [false, true] {
                let retained = count.min(MAX_ROWS);
                let omitted = count - retained;
                let page = panels_page(&rows[..count], source_truncated, 7);
                assert_eq!(page.rows.len(), retained);
                assert_eq!(page.truncated, source_truncated || omitted > 0);
                assert_eq!(
                    catalog_row_index(&page.rows[retained - 1].uid, 7),
                    Some(retained - 1)
                );
                let mut table = TableState::new();
                table.set_page(page);
                let (summary, _) = catalog_notices(
                    table.total_rows(),
                    source_truncated,
                    count - table.total_rows(),
                    Vec::new(),
                );
                if !source_truncated && omitted == 0 {
                    assert!(summary.is_empty());
                } else {
                    assert!(summary.contains(&format!("{retained} entries in the table")));
                    assert_eq!(
                        summary.contains("2 entries omitted by the table limit"),
                        omitted == 2
                    );
                }
            }
        }
    }

    #[test]
    fn catalog_state_matches_panel_execution_semantics() {
        let mut panel = GrafanaPanelRow {
            query_source: Some(std::sync::Arc::new(k10s_core::ServiceIdentity {
                namespace: "monitoring".into(),
                name: "grafana".into(),
                port: 3000,
                uid: "grafana-a".into(),
                resource_version: "1".into(),
            })),
            dashboard_uid: "pilot".into(),
            dashboard_title: "Pilot".into(),
            identity: GrafanaRowIdentity::Panel {
                id: Some(1),
                position: 0,
            },
            source: GrafanaRowSource::Grafana,
            title: "CPU".into(),
            kind: GrafanaPanelKind::Timeseries,
            expr: "up".into(),
            dialect: QueryDialect::PromQL,
            transformed: false,
            browser_url: None,
            browser_forward: None,
            datasource_uid: None,
            ref_id: "A".into(),
            unit: String::new(),
            reducer: String::new(),
            min_step: String::new(),
            min_interval: String::new(),
            unsupported: None,
        };
        assert_eq!(panel_state(&panel), "Supported");
        let source = panel.query_source.take();
        assert_eq!(panel_state(&panel), GRAFANA_SOURCE_UNVERIFIED);
        panel.source = GrafanaRowSource::ConfigMap;
        assert!(panel_state(&panel).contains(GRAFANA_SOURCE_UNVERIFIED));
        assert!(!panel_state(&panel).contains("query supported"));
        panel.query_source = source;
        panel.source = GrafanaRowSource::Grafana;
        assert_eq!(panel_query_title(&panel), "CPU");
        panel.source = GrafanaRowSource::ConfigMap;
        assert_eq!(panel_query_title(&panel), "ConfigMap: CPU");
        panel.source = GrafanaRowSource::Unknown;
        assert_eq!(panel_query_title(&panel), "Unverified dashboard: CPU");
        panel.source = GrafanaRowSource::Grafana;
        for dialect in [
            QueryDialect::Unknown,
            QueryDialect::LogQL,
            QueryDialect::TraceQL,
        ] {
            panel.dialect = dialect;
            assert!(panel_state(&panel).contains("Unsupported panel semantics"));
            assert!(!panel_semantics_supported(&panel));
        }
        panel.dialect = QueryDialect::PromQL;
        panel.transformed = true;
        assert!(panel_state(&panel).contains("Unsupported panel semantics"));
        panel.transformed = false;
        panel.kind = GrafanaPanelKind::Table;
        assert!(panel_state(&panel).contains("Unsupported panel semantics"));
        panel.identity = GrafanaRowIdentity::Panel {
            id: None,
            position: 0,
        };
        assert!(panel_state(&panel).starts_with("Panel ID unavailable; no panel deep link; "));
    }

    #[test]
    fn browser_links_accept_http_scheme_case_and_reject_control_characters() {
        for url in [
            "https://grafana.example/d/api",
            "HTTPS://grafana.example/d/api?orgId=1#panel",
            "HtTp://grafana.example",
        ] {
            assert!(browser_url_allowed(url), "{url:?}");
        }
        for url in [
            "file://grafana.example",
            "https://grafana.example/has space",
            "https://grafana.example/has\0control",
            "https://grafana.example/has\"quote",
        ] {
            assert!(!browser_url_allowed(url), "{url:?}");
        }
    }
    use k10s_provider::PodIdentity;

    fn request(uid: &str) -> DescribeRequest {
        DescribeRequest {
            kind: k10s_core::KindId::DEPLOYMENT,
            namespace: Some("prod".into()),
            name: "worker".into(),
            uid: uid.into(),
        }
    }

    fn scope() -> ObserveScope {
        ObserveScope {
            request: request("workload"),
            namespace: "prod".into(),
            pods: vec![PodIdentity {
                name: "worker-pod".into(),
                uid: "current-pod".into(),
                node: "node".into(),
            }],
        }
    }

    fn metric(namespace: &str, uid: &str) -> PromSeriesView {
        PromSeriesView {
            labels: "test labels".into(),
            label_set: vec![
                ("namespace".into(), namespace.into()),
                ("pod".into(), "worker-pod".into()),
                ("uid".into(), uid.into()),
            ],
            points: vec![(0, 1.0)],
            gaps: Vec::new(),
        }
    }

    #[test]
    fn query_failure_details_keep_the_full_reason_and_requested_range() {
        let why = format!("server refused the query: {} end", "é".repeat(1_000));
        let failure = Rc::new(QueryFailure::new(
            "Loki query failed",
            "Requested LogQL",
            "{service_name=\"api\"}".into(),
            QueryWindow {
                start_ms: 0,
                end_ms: 1_000,
                step_ms: 500,
            },
            why.clone(),
        ));
        let retained = Rc::downgrade(&failure);
        let reason_pointer = failure.why.as_ptr();
        let details = NoticeSnapshot::shared(failure.clone());
        assert_eq!(details.get(0), Some(("Loki query failed", why.as_str())));
        assert_eq!(details.get(0).unwrap().1.as_ptr(), reason_pointer);
        assert_eq!(
            details.get(1),
            Some(("Requested LogQL", "{service_name=\"api\"}"))
        );
        assert_eq!(
            details.get(2),
            Some((
                "Requested range",
                "1970-01-01 00:00:00.000 to 1970-01-01 00:00:01.000 UTC; view resolution 500 ms"
            ))
        );
        assert!(failure_notice("Loki query failed", &why).contains("[abbreviated]"));
        drop(failure);
        assert!(retained.upgrade().is_some());
        assert_eq!(details.get(0).unwrap().1.as_ptr(), reason_pointer);
        drop(details);
        assert!(retained.upgrade().is_none());
    }

    #[test]
    fn source_scope_and_panel_failures_retain_the_full_reason() {
        for source in [
            "Source discovery",
            "Kubernetes scope failed",
            "Unsupported panel",
            "Grafana catalog failed",
        ] {
            let why = format!("response=\"{}\"", "é".repeat(1_000));
            let pointer = why.as_ptr();
            let details = failure_details(source, why);
            assert_eq!(details.len(), 1);
            assert_eq!(details.get(0).unwrap().0, source);
            assert_eq!(details.get(0).unwrap().1.as_ptr(), pointer);
            assert_eq!(details.get(1), None);
        }
    }

    #[test]
    fn tab_cycle_skips_unavailable_stops_in_both_directions() {
        for bits in 0u8..64 {
            let available = std::array::from_fn(|index| bits & (1 << index) != 0);
            let expected: Vec<_> = FOCUS_STOPS
                .iter()
                .enumerate()
                .filter(|(index, _)| *index == 0 || available[*index - 1])
                .map(|(_, stop)| *stop)
                .collect();
            assert_eq!(expected[0], FocusStop::Root);
            for (index, &stop) in expected.iter().enumerate() {
                assert_eq!(
                    next_focus(stop, true, available),
                    expected[(index + 1) % expected.len()]
                );
                assert_eq!(
                    next_focus(stop, false, available),
                    expected[(index + expected.len() - 1) % expected.len()]
                );
            }
        }
        assert_eq!(
            next_focus(FocusStop::Query, true, [false; 6]),
            FocusStop::Root
        );
        assert_eq!(
            next_focus(FocusStop::Panels, false, [false; 6]),
            FocusStop::Root
        );
    }

    #[test]
    fn table_focus_tracks_visibility_and_preserves_a_pending_log_preference() {
        let panels = Some(ObserveTable::Panels);
        let logs = Some(ObserveTable::Logs);
        assert!(!table_available(ObserveTable::Panels, false, true));
        assert!(table_available(ObserveTable::Panels, true, false));
        assert!(!table_available(ObserveTable::Logs, true, false));
        assert!(table_available(ObserveTable::Logs, false, true));

        assert_eq!(visible_table(panels, false, true, true), None);
        assert_eq!(visible_table(panels, true, false, true), None);
        assert_eq!(visible_table(panels, true, true, false), panels);
        assert_eq!(visible_table(logs, false, false, false), None);
        assert_eq!(visible_table(logs, false, false, true), logs);
        assert_eq!(visible_table(logs, false, false, false), None);
        assert_eq!(visible_table(None, true, true, true), None);

        assert_eq!(
            preferred_table_after_focus(FocusStop::Root, FocusStop::Root, logs, None),
            logs
        );
        assert_eq!(
            preferred_table_after_focus(FocusStop::Root, FocusStop::Scope, logs, None),
            None
        );
        assert_eq!(
            preferred_table_after_focus(FocusStop::Panels, FocusStop::Scope, panels, panels),
            panels
        );
        assert_eq!(
            preferred_table_after_focus(FocusStop::Scope, FocusStop::Root, panels, panels),
            None
        );

        assert_eq!(
            next_focus(
                FocusStop::Root,
                true,
                [true, false, false, false, false, false]
            ),
            FocusStop::Panels
        );
        assert_eq!(
            next_focus(
                FocusStop::Root,
                true,
                [false, false, true, false, false, false]
            ),
            FocusStop::Scope
        );
    }

    #[test]
    fn enter_expands_a_collapsed_catalog_before_running_a_panel() {
        let panels = Some(ObserveTable::Panels);
        let logs = Some(ObserveTable::Logs);
        assert_eq!(
            table_open_action(panels, true, false, false),
            Some(OpenTableAction::ExpandPanels)
        );
        assert_eq!(
            table_open_action(None, true, false, false),
            Some(OpenTableAction::ExpandPanels)
        );
        assert_eq!(
            table_open_action(panels, true, true, false),
            Some(OpenTableAction::RunPanel)
        );
        assert_eq!(table_open_action(panels, false, false, false), None);
        assert_eq!(table_open_action(logs, true, false, false), None);
        assert_eq!(
            table_open_action(logs, true, false, true),
            Some(OpenTableAction::FollowTrace)
        );
    }

    #[test]
    fn selecting_a_table_leaves_query_input_and_preserves_matching_filter_input() {
        let mut typing = true;
        let mut filtering = false;
        assert!(prepare_table_selection(&mut typing, &mut filtering, false));
        assert!(!typing);
        assert!(!filtering);

        filtering = true;
        assert!(!prepare_table_selection(&mut typing, &mut filtering, false));
        assert!(filtering);
        assert!(prepare_table_selection(&mut typing, &mut filtering, true));
        assert!(!filtering);
    }

    #[test]
    fn raw_query_input_starts_the_focus_cycle_at_the_root() {
        for visible in [None, Some(ObserveTable::Panels), Some(ObserveTable::Logs)] {
            let current = input_focus_stop(true, visible);
            assert_eq!(current, FocusStop::Root);
            assert_eq!(
                next_focus(current, true, [true, true, false, false, false, false]),
                FocusStop::Panels
            );
            assert_eq!(
                next_focus(current, false, [true, true, false, false, false, false]),
                FocusStop::Logs
            );
        }
        assert_eq!(
            input_focus_stop(false, Some(ObserveTable::Panels)),
            FocusStop::Panels
        );
        assert_eq!(
            input_focus_stop(false, Some(ObserveTable::Logs)),
            FocusStop::Logs
        );
    }

    #[test]
    fn wheel_selection_preserves_the_filter_table_until_editing_ends() {
        for preferred in [None, Some(ObserveTable::Panels), Some(ObserveTable::Logs)] {
            for target in [ObserveTable::Panels, ObserveTable::Logs] {
                assert_eq!(table_after_scroll(preferred, target, true), preferred);
                assert_eq!(table_after_scroll(preferred, target, false), Some(target));
            }
        }
    }

    #[test]
    fn leaving_input_clears_each_editing_mode() {
        for (was_typing, was_filtering) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut typing = was_typing;
            let mut filtering = was_filtering;
            assert_eq!(
                leave_input(&mut typing, &mut filtering),
                was_typing || was_filtering
            );
            assert!(!typing);
            assert!(!filtering);
            assert!(!leave_input(&mut typing, &mut filtering));
        }
    }

    #[test]
    fn notice_keyboard_activation_preserves_editing_and_pointer_activation_leaves_it() {
        let events = [
            (
                ClickEvent::Keyboard(gpui::KeyboardClickEvent::default()),
                false,
            ),
            (ClickEvent::Mouse(gpui::MouseClickEvent::default()), true),
            (ClickEvent::Touch(gpui::TouchClickEvent::default()), true),
        ];
        for (event, leave) in &events {
            for (was_typing, was_filtering) in [(true, false), (false, true)] {
                let mut typing = was_typing;
                let mut filtering = was_filtering;
                assert_eq!(
                    leave_input_for_notice_activation(event, &mut typing, &mut filtering),
                    *leave
                );
                assert_eq!(typing, was_typing && !*leave);
                assert_eq!(filtering, was_filtering && !*leave);
            }
        }
    }

    #[test]
    fn escape_returns_from_a_notice_or_leaves_the_active_input() {
        assert_eq!(
            cancel_input_action(false, true, false),
            CancelInputAction::FocusRoot
        );
        assert_eq!(
            cancel_input_action(false, true, true),
            CancelInputAction::LeaveInput
        );
        assert_eq!(
            cancel_input_action(true, false, true),
            CancelInputAction::LeaveInput
        );
        assert_eq!(
            cancel_input_action(true, false, false),
            CancelInputAction::Ignore
        );
        assert_eq!(
            cancel_input_action(false, false, true),
            CancelInputAction::Ignore
        );
    }

    #[test]
    fn notice_focus_falls_back_only_for_the_removed_control() {
        let handles = [1, 2, 3, 4];
        for bits in 0u8..16 {
            let available = std::array::from_fn(|index| bits & (1 << index) != 0);
            for (index, handle) in handles.iter().enumerate() {
                assert_eq!(
                    unavailable_notice_focus(handle, &handles, available),
                    !available[index]
                );
            }
            assert!(!unavailable_notice_focus(&0, &handles, available));
            assert!(!unavailable_notice_focus(&5, &handles, available));
        }
    }

    #[test]
    fn focused_notice_controls_keep_enter_for_click_and_tab_for_navigation() {
        let keymap = gpui::Keymap::new(crate::keybindings());
        for (typing, filtering) in [(false, false), (true, false), (false, true), (true, true)] {
            let contexts = [
                gpui::KeyContext::parse("Workspace").expect("workspace context"),
                gpui::KeyContext::parse(observe_key_context(typing, filtering, true))
                    .expect("notice control context"),
            ];
            let bindings_for = |key| {
                keymap
                    .bindings_for_input(
                        &[gpui::Keystroke::parse(key).expect("valid shortcut")],
                        &contexts,
                    )
                    .0
            };
            assert!(bindings_for("enter").is_empty());
            assert!(bindings_for("space").is_empty());
            assert!(bindings_for("q").is_empty());
            assert_eq!(
                bindings_for("escape")
                    .first()
                    .map(|binding| binding.action().name()),
                Some("k10s_shell::CancelInput")
            );
            assert_eq!(
                bindings_for("tab")
                    .first()
                    .map(|binding| binding.action().name()),
                Some("k10s_shell::ObserveFocusNext")
            );
            assert_eq!(
                bindings_for("shift-tab")
                    .first()
                    .map(|binding| binding.action().name()),
                Some("k10s_shell::ObserveFocusPrevious")
            );
            if typing || filtering {
                for key in [
                    "1",
                    "2",
                    "x",
                    "t",
                    "p",
                    "o",
                    "shift-o",
                    "g",
                    "q",
                    "b",
                    "n",
                    "d",
                    "l",
                    "s",
                    "a",
                    "i",
                    "y",
                    "shift-f",
                    "/",
                    "ctrl-alt-s",
                    "ctrl-alt-i",
                ] {
                    assert!(
                        bindings_for(key).is_empty(),
                        "{key} must not run a workspace or Observe command while input is active"
                    );
                }
                for key in ["ctrl-tab", "ctrl-shift-tab", "ctrl-w"] {
                    assert!(
                        !bindings_for(key).is_empty(),
                        "{key} remains available while input is active"
                    );
                }
            }
        }
        let root_contexts = [
            gpui::KeyContext::parse("Workspace").expect("workspace context"),
            gpui::KeyContext::parse("Browse Observe").expect("observe context"),
        ];
        let enter = gpui::Keystroke::parse("enter").expect("valid shortcut");
        let bindings = keymap.bindings_for_input(&[enter], &root_contexts).0;
        assert_eq!(
            bindings.first().map(|binding| binding.action().name()),
            Some("k10s_shell::OpenRow")
        );
    }

    #[test]
    fn observe_shortcuts_leave_only_focus_navigation_active_during_input() {
        let keymap = gpui::Keymap::new(crate::keybindings());
        let commands = [
            ("1", "k10s_shell::ObserveRangeFifteenMinutes"),
            ("2", "k10s_shell::ObserveRangeOneHour"),
            ("x", "k10s_shell::ObserveToggleResolution"),
            ("t", "k10s_shell::ObserveToggleDialect"),
            ("p", "k10s_shell::ObserveReadiness"),
            ("o", "k10s_shell::ObserveOtlpLogs"),
            ("shift-o", "k10s_shell::ObservePodLabelLogs"),
            ("g", "k10s_shell::ObserveOpenGrafana"),
            ("tab", "k10s_shell::ObserveFocusNext"),
            ("shift-tab", "k10s_shell::ObserveFocusPrevious"),
            ("ctrl-alt-g", "k10s_shell::ObserveCatalogDetails"),
            ("ctrl-alt-q", "k10s_shell::OpenQueryDetails"),
            ("ctrl-alt-k", "k10s_shell::ObserveScopeDetails"),
            ("ctrl-alt-d", "k10s_shell::ObserveSourceDetails"),
        ];
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
        for (key, command) in commands {
            let bindings = bindings_for(key, observe_key_context(false, false, false));
            assert_eq!(
                bindings.first().map(|binding| binding.action().name()),
                Some(command)
            );
            for (typing, filtering) in [(true, false), (false, true), (true, true)] {
                let bindings = bindings_for(key, observe_key_context(typing, filtering, false));
                if matches!(key, "tab" | "shift-tab") {
                    assert_eq!(
                        bindings.first().map(|binding| binding.action().name()),
                        Some(command)
                    );
                } else {
                    assert!(
                        bindings.iter().all(|binding| {
                            let name = binding.action().name();
                            !name.starts_with("k10s_shell::Observe")
                                && name != "k10s_shell::OpenQueryDetails"
                        }),
                        "{key} must not run an Observe command while entering text"
                    );
                }
            }
            for context in ["Browse", "Map", "Ecosystem", "Doc", "Editor", "Terminal"] {
                assert!(bindings_for(key, context).iter().all(|binding| {
                    !binding.action().name().starts_with("k10s_shell::Observe")
                }));
            }
        }
        for (key, command) in [
            ("up", "k10s_shell::RowUp"),
            ("enter", "k10s_shell::OpenRow"),
            ("r", "k10s_shell::Refresh"),
            ("/", "k10s_shell::EnterFilter"),
        ] {
            assert_eq!(
                bindings_for(key, observe_key_context(false, false, false))
                    .first()
                    .map(|binding| binding.action().name()),
                Some(command)
            );
        }
        for (typing, filtering) in [(true, false), (false, true)] {
            for (key, command) in [
                ("enter", "k10s_shell::CommitInput"),
                ("escape", "k10s_shell::CancelInput"),
                ("backspace", "k10s_shell::DeleteInputChar"),
            ] {
                assert_eq!(
                    bindings_for(key, observe_key_context(typing, filtering, false))
                        .first()
                        .map(|binding| binding.action().name()),
                    Some(command)
                );
            }
        }
    }

    #[test]
    fn table_focus_routes_navigation_and_filtering_without_replacing_either_result() {
        let page = |names: &[&str]| TablePage {
            rows: names
                .iter()
                .map(|name| TableRow {
                    name: (*name).into(),
                    uid: (*name).into(),
                    namespace: None,
                    cells: vec![(*name).into()],
                })
                .collect(),
            ..TablePage::default()
        };
        let mut panels = TableState::new();
        panels.set_page(page(&["time-series", "stat"]));
        let mut logs = TableState::new();
        logs.set_page(page(&["first log", "linked trace log"]));
        let panel_revision = panels.content_revision();
        let log_revision = logs.content_revision();

        ObserveTable::Logs
            .state(&mut panels, &mut logs)
            .move_selection(1);
        assert_eq!(
            logs.selected_row().expect("selected log").uid,
            "linked trace log"
        );
        assert_eq!(
            panels.selected_row().expect("selected panel").uid,
            "time-series"
        );
        assert_eq!(panels.content_revision(), panel_revision);
        assert_eq!(logs.content_revision(), log_revision);

        ObserveTable::Panels
            .state(&mut panels, &mut logs)
            .push_filter("stat");
        assert_eq!(panels.visible_rows(), 1);
        assert_eq!(panels.selected_row().expect("filtered panel").uid, "stat");
        assert_eq!(logs.visible_rows(), 2);
        assert_eq!(
            logs.selected_row().expect("selected log").uid,
            "linked trace log"
        );
        assert_eq!(panels.total_rows(), 2);
        assert_eq!(logs.total_rows(), 2);
        assert_eq!(logs.content_revision(), log_revision);

        let filter_table = table_after_scroll(Some(ObserveTable::Panels), ObserveTable::Logs, true)
            .expect("filter table remains selected");
        filter_table.state(&mut panels, &mut logs).push_filter("s");
        assert_eq!(panels.filter, "stats");
        assert!(logs.filter.is_empty());
    }

    #[test]
    fn focused_loki_absence_never_classifies_the_unqueried_surface_as_absent() {
        for enabled in [false, true] {
            for pending in [false, true] {
                assert_eq!(
                    surface_presence(
                        enabled,
                        pending,
                        ToolPresence::Missing,
                        ToolPresence::Missing,
                        ToolPresence::Missing
                    ),
                    (enabled && !pending).then_some(ToolPresence::Missing),
                );
            }
        }
        assert_eq!(
            surface_presence(
                true,
                false,
                ToolPresence::Ready,
                ToolPresence::Missing,
                ToolPresence::Missing
            ),
            Some(ToolPresence::Ready),
        );
        assert_eq!(
            surface_presence(
                true,
                false,
                ToolPresence::Missing,
                ToolPresence::Blocked,
                ToolPresence::Missing
            ),
            Some(ToolPresence::Blocked),
        );
    }

    #[test]
    fn a_query_reply_requires_the_same_generation_uid_range_resolution_and_visibility() {
        let identity = QueryIdentity {
            generation: 4,
            request: Some(request("current")),
            window: QueryWindow {
                start_ms: 0,
                end_ms: 1000,
                step_ms: 10,
            },
        };
        assert!(identity.current(4, identity.request.as_ref(), identity.window, true));
        assert!(!identity.current(5, identity.request.as_ref(), identity.window, true));
        assert!(!identity.current(4, Some(&request("recreated")), identity.window, true));
        assert!(!identity.current(4, None, identity.window, true));
        assert!(!identity.current(
            4,
            identity.request.as_ref(),
            QueryWindow {
                end_ms: 2000,
                ..identity.window
            },
            true
        ));
        assert!(!identity.current(
            4,
            identity.request.as_ref(),
            QueryWindow {
                step_ms: 20,
                ..identity.window
            },
            true
        ));
        assert!(!identity.current(4, identity.request.as_ref(), identity.window, false));
    }

    #[test]
    fn returned_series_identity_requires_the_namespace_name_and_current_uid() {
        let scope = scope();
        assert_eq!(
            metric_association(Some(&scope), &[metric("prod", "current-pod")], false),
            "All 1 series labels match current pod UIDs"
        );
        assert!(
            metric_association(Some(&scope), &[metric("prod", "retired-pod")], false)
                .contains("lack a verified")
        );
        assert!(
            metric_association(Some(&scope), &[metric("other", "current-pod")], false)
                .contains("lack a verified")
        );
        assert!(
            metric_association(
                Some(&scope),
                &[metric("prod", "current-pod"), metric("prod", "retired-pod")],
                false
            )
            .starts_with("1/2 series labels match")
        );
        let mut nameless = metric("prod", "current-pod");
        nameless.label_set.retain(|(key, _)| key != "uid");
        assert!(
            metric_association(Some(&scope), &[nameless], true).contains("labels do not prove")
        );
        let mut otlp = metric("collector-namespace", "collector-uid");
        otlp.label_set.extend([
            ("k8s_namespace_name".into(), "prod".into()),
            ("k8s_pod_name".into(), "worker-pod".into()),
            ("k8s_pod_uid".into(), "current-pod".into()),
        ]);
        assert_eq!(
            metric_association(Some(&scope), &[otlp], false),
            "All 1 series labels match current pod UIDs"
        );
    }

    #[test]
    fn log_identity_checks_labels_and_metadata_without_proving_name_only_lines() {
        let scope = scope();
        let mut entry = LogEntry {
            timestamp_ns: 1_000_000,
            line: "actual line".into(),
            labels: Arc::from(vec![
                ("k8s_namespace_name".into(), "prod".into()),
                ("k8s_pod_name".into(), "worker-pod".into()),
                ("k8s_pod_uid".into(), "current-pod".into()),
            ]),
            metadata: Vec::new(),
            trace_id: Some("1234567890abcdef1234567890abcdef".into()),
        };
        assert_eq!(log_identity(&entry, Some(&scope)), "Selected UID");
        assert_eq!(
            logs_page(&[entry.clone()], Some(&scope), None).rows[0].cells[3],
            "1234567890abcdef1234567890abcdef"
        );
        entry
            .metadata
            .push(("k8s_pod_uid".into(), "retired-pod".into()));
        assert_eq!(
            log_identity(&entry, Some(&scope)),
            "Other or unverified UID"
        );
        entry.labels = Arc::from(vec![("pod".into(), "worker-pod".into())]);
        entry.metadata.clear();
        assert_eq!(log_identity(&entry, Some(&scope)), "Name only");
    }

    #[test]
    fn a_span_log_association_uses_returned_identity_trace_and_exact_time_bounds() {
        let trace_id = "1234567890abcdef1234567890abcdef";
        let mut origin = k10s_provider::SpanView {
            id: "a9a1b2c3d4e5f678".into(),
            parent: String::new(),
            name: "request".into(),
            service: "producer".into(),
            start_us: 2_000_123,
            duration_us: 2_345,
            status: "OK".into(),
            namespace: "prod".into(),
            pod: "worker-pod".into(),
            pod_uid: "recorded-pod".into(),
            node: String::new(),
        }
        .log_query(trace_id)
        .unwrap();
        let start = origin.request.window.start_ms as u64 * 1_000_000;
        let end = origin.request.window.end_ms as u64 * 1_000_000;
        let mut entry = LogEntry {
            timestamp_ns: start,
            line: "actual line".into(),
            labels: Arc::from(vec![
                ("k8s_namespace_name".into(), "prod".into()),
                ("k8s_pod_name".into(), "worker-pod".into()),
                ("k8s_pod_uid".into(), "recorded-pod".into()),
            ]),
            metadata: Vec::new(),
            trace_id: Some(trace_id.into()),
        };
        for scope in [None, Some(scope())] {
            for timestamp_ns in [start, end - 1] {
                entry.timestamp_ns = timestamp_ns;
                assert_eq!(
                    span_log_identity(&entry, scope.as_ref(), Some(&origin)),
                    "Recorded span UID"
                );
            }
            for timestamp_ns in [start - 1, end, end + 1] {
                entry.timestamp_ns = timestamp_ns;
                assert_ne!(
                    span_log_identity(&entry, scope.as_ref(), Some(&origin)),
                    "Recorded span UID"
                );
            }
        }
        entry.timestamp_ns = start;
        for key in ["k8s_namespace_name", "k8s_pod_name", "k8s_pod_uid"] {
            entry.metadata.push((key.into(), "different".into()));
            assert_ne!(
                span_log_identity(&entry, None, Some(&origin)),
                "Recorded span UID"
            );
            entry.metadata.clear();
        }
        for trace in [None, Some("00000000000000000000000000000001".into())] {
            entry.trace_id = trace;
            assert_ne!(
                span_log_identity(&entry, None, Some(&origin)),
                "Recorded span UID"
            );
        }
        entry.trace_id = Some(trace_id.into());
        origin.pod_uid.clear();
        assert_ne!(
            span_log_identity(&entry, None, Some(&origin)),
            "Recorded span UID"
        );
    }

    #[test]
    fn empty_metrics_do_not_claim_that_returned_series_lack_identity() {
        for scope in [None, Some(scope())] {
            for scoped_by_uid in [false, true] {
                assert_eq!(
                    metric_association(scope.as_ref(), &[], scoped_by_uid),
                    "No returned series to associate with a Kubernetes UID"
                );
            }
        }
    }

    #[test]
    fn partial_catalog_rows_keep_the_failed_source_sentence() {
        let (notice, details) = catalog_notices(
            2,
            false,
            0,
            vec!["configmaps: access denied for this account".into()],
        );
        assert_eq!(
            notice.as_ref(),
            "configmaps: access denied for this account"
        );
        assert_eq!(
            details.expect("details").get(0),
            Some(("", "configmaps: access denied for this account"))
        );
        assert!(
            catalog_notices(2, true, 0, vec!["provisioning list failed".into()])
                .0
                .contains(
                    "Grafana source or dashboard data was truncated; 2 entries in the table; provisioning list failed"
                )
        );
        let (summary, details) = catalog_notices(2, true, 0, Vec::new());
        assert!(summary.contains("Grafana source or dashboard data was truncated"));
        assert_eq!(
            details.expect("truncation details").get(0),
            Some(("", summary.as_ref()))
        );
        let (summary, details) =
            catalog_notices(5_000, true, 10, vec!["provisioning list failed".into()]);
        assert!(summary.contains("Grafana source or dashboard data was truncated"));
        assert!(summary.contains("10 entries omitted by the table limit"));
        let details = details.expect("separate source and table limits");
        assert_eq!(
            details.get(0),
            Some((
                "",
                "Grafana source or dashboard data was truncated; 5000 entries in the table",
            ))
        );
        assert_eq!(
            details.get(1),
            Some(("", "10 entries omitted by the table limit"))
        );
        assert_eq!(details.get(2), Some(("", "provisioning list failed")));
        let (summary, details) = catalog_notices(0, false, 0, Vec::new());
        assert!(summary.contains("No Grafana query panels"));
        assert_eq!(
            details.expect("empty result details").get(0),
            Some(("", summary.as_ref()))
        );
        let (summary, details) = catalog_notices(
            0,
            false,
            0,
            vec!["Grafana dashboard api: access denied for this account".into()],
        );
        assert!(summary.contains("catalog may be incomplete"));
        assert!(summary.contains("Grafana dashboard api: access denied"));
        assert_eq!(
            details.expect("incomplete catalog details").get(1),
            Some(("", "Grafana dashboard api: access denied for this account"))
        );
        let (summary, details) = catalog_notices(2, false, 0, Vec::new());
        assert!(summary.is_empty());
        assert!(details.is_none());
    }

    #[test]
    fn callback_notices_retain_server_strings_and_keep_identity_and_partial_state_first() {
        let warning = "server warning ".repeat((8 << 20) / 15);
        let warning_bytes = warning.len();
        let pointer = warning.as_ptr();
        let info = "informational server sentence".to_string();
        let notices = prom_notices(
            vec![warning],
            vec![info.clone()],
            "Kubernetes UID association is unproven".into(),
            Some(5),
            None,
            0,
            0,
        );
        assert_eq!(notices[2], "Prometheus warnings");
        assert_eq!(notices[3].as_ptr(), pointer);
        assert_eq!(notices[3].len(), warning_bytes);
        assert_eq!(notices[4], "Prometheus info");
        assert_eq!(notices[5], info);
        let shown = display_notice_groups(&[&notices]);
        assert!(shown.starts_with(
            "Kubernetes UID association is unproven; 5 returned series omitted locally; query data is incomplete; Prometheus warnings; server warning"
        ));
        assert!(shown.contains("[abbreviated]"));
        assert!(shown.len() <= crate::chart::MAX_NOTICE_TEXT_BYTES);
    }

    #[test]
    fn possible_server_limit_is_separate_from_known_local_omissions_and_full_annotations() {
        let warning = "server warning 界 ".repeat(100);
        let notices = prom_notices(
            vec![warning.clone(), "second warning".into()],
            vec!["server info".into(), "second info".into()],
            "Kubernetes UID association is unproven".into(),
            Some(1),
            Some(256),
            0,
            0,
        );
        assert_eq!(notices, [
            "Kubernetes UID association is unproven".to_string(),
            "1 returned series omitted locally; query data is incomplete".to_string(),
            "Prometheus range query reached the requested 256-series limit; completeness is unknown".to_string(),
            "Prometheus warnings".to_string(),
            warning,
            "second warning".to_string(),
            "Prometheus info".to_string(),
            "server info".to_string(),
            "second info".to_string(),
        ]);
        let only_possible = prom_notices(
            Vec::new(),
            Vec::new(),
            "association".into(),
            None,
            Some(256),
            0,
            0,
        );
        assert_eq!(only_possible.len(), 2);
        assert!(!only_possible.join("; ").contains("omitted"));
        assert!(only_possible[1].contains("completeness is unknown"));
    }

    #[test]
    fn annotation_omission_counts_precede_retained_chart_messages() {
        let notes = prom_notices(
            vec!["first warning".into(), "second warning".into()],
            vec!["first info".into()],
            "association".into(),
            Some(1),
            Some(256),
            3840,
            3,
        );
        let expected = [
            "association",
            "1 returned series omitted locally; query data is incomplete",
            "Prometheus range query reached the requested 256-series limit; completeness is unknown",
            "3840 Prometheus warning annotations omitted by the local limit",
            "3 Prometheus info annotations omitted by the local limit",
        ];
        assert!(notes.len() >= expected.len());
        for (actual, expected) in notes.iter().zip(expected) {
            assert_eq!(actual, expected);
        }
        let shown = display_notice_groups(&[&notes]);
        assert!(shown.contains("3840 Prometheus warning annotations omitted"));
        assert!(shown.contains("3 Prometheus info annotations omitted"));
        assert!(notes.contains(&"first warning".to_string()));
        assert!(notes.contains(&"first info".to_string()));
    }

    #[test]
    fn large_failed_source_sentences_are_labelled_and_bounded_instead_of_becoming_empty() {
        let failure = "界".repeat(100_000);
        let shown = failure_notice("Grafana catalog failed", &failure);
        assert!(shown.starts_with("Grafana catalog failed: 界"));
        assert!(shown.ends_with("[abbreviated]"));
        assert!(shown.len() <= crate::chart::MAX_NOTICE_BYTES + "Grafana catalog failed: ".len());
        assert_eq!(failure.len(), 300_000);
        assert_eq!(
            failure_notice("Loki query failed", "").as_ref(),
            "Loki query failed: Empty notice"
        );
        let (catalog, details) = catalog_notices(2, false, 0, vec![failure; 20]);
        assert!(catalog.contains("[abbreviated]"));
        assert!(catalog.contains("12 notices omitted"));
        assert!(catalog.len() <= crate::chart::MAX_NOTICE_TEXT_BYTES);
        assert_eq!(details.expect("all notices").len(), 20);
    }
    fn grafana_panel() -> GrafanaPanelRow {
        GrafanaPanelRow {
            query_source: Some(Arc::new(k10s_core::ServiceIdentity {
                namespace: "monitoring".into(),
                name: "grafana".into(),
                port: 3000,
                uid: "grafana-a".into(),
                resource_version: "1".into(),
            })),
            dashboard_uid: "pilot".into(),
            dashboard_title: "Pilot".into(),
            identity: GrafanaRowIdentity::Panel {
                id: Some(7),
                position: 0,
            },
            source: GrafanaRowSource::Grafana,
            title: "Requests".into(),
            kind: GrafanaPanelKind::Timeseries,
            expr: "up".into(),
            dialect: QueryDialect::PromQL,
            transformed: false,
            browser_url: None,
            browser_forward: None,
            datasource_uid: Some("prometheus".into()),
            ref_id: "A".into(),
            unit: String::new(),
            reducer: String::new(),
            min_step: String::new(),
            min_interval: String::new(),
            unsupported: None,
        }
    }

    #[test]
    fn a_refreshed_identical_panel_with_a_new_service_origin_invalidates_its_data_plan() {
        let mut panel = grafana_panel();
        let mut plan = QueryPlan::Panel(Box::new(panel.clone()));
        assert!(!plan.refresh_panel(std::slice::from_ref(&panel)));
        let original = panel.query_source.as_ref().unwrap();
        panel.query_source = Some(Arc::new(k10s_core::ServiceIdentity {
            uid: "grafana-b".into(),
            resource_version: "2".into(),
            ..original.as_ref().clone()
        }));
        assert!(
            plan.refresh_panel(std::slice::from_ref(&panel)),
            "the catalogue callback must cancel and clear data from the previous origin"
        );
        let QueryPlan::Panel(current) = &plan else {
            panic!("the current panel remains executable");
        };
        assert_eq!(current.as_ref(), &panel);
        assert!(!plan.refresh_panel(std::slice::from_ref(&panel)));
        assert!(plan.refresh_panel(&[]));
        assert!(matches!(plan, QueryPlan::Readiness));
        let mut raw = QueryPlan::Prom("up".into());
        assert!(!raw.refresh_panel(std::slice::from_ref(&panel)));
        assert!(matches!(raw, QueryPlan::Prom(_)));
    }

    #[test]
    fn a_refreshed_panel_requires_one_matching_target_key() {
        for source in [GrafanaRowSource::Grafana, GrafanaRowSource::ConfigMap] {
            let mut first = grafana_panel();
            first.source = source;
            let mut second = first.clone();
            second.ref_id = "B".into();
            second.expr = "sum(up)".into();
            let mut plan = QueryPlan::Panel(Box::new(second.clone()));
            assert!(!plan.refresh_panel(&[first.clone(), second.clone()]));
            assert!(!plan.refresh_panel(std::slice::from_ref(&second)));

            for mut duplicate in [first.clone(), second] {
                duplicate.ref_id.clone_from(&first.ref_id);
                let rows = [first.clone(), duplicate];
                for selected in &rows {
                    let mut plan = QueryPlan::Panel(Box::new(selected.clone()));
                    assert!(plan.refresh_panel(&rows));
                    assert!(
                        matches!(plan, QueryPlan::Readiness),
                        "identical or differing rows with one logical key cannot choose a target"
                    );
                }
            }
        }
    }

    #[test]
    fn a_panel_refresh_runs_once_after_catalogue_and_scope_in_either_order() {
        for catalogue_first in [false, true] {
            for changed_origin in [false, true] {
                let previous = grafana_panel();
                let mut current = previous.clone();
                if changed_origin {
                    current.query_source = Some(Arc::new(k10s_core::ServiceIdentity {
                        uid: "grafana-b".into(),
                        resource_version: "2".into(),
                        ..previous.query_source.as_deref().unwrap().clone()
                    }));
                }
                let mut plan = QueryPlan::Panel(Box::new(previous));
                let mut generation = 7;
                let mut pending = Some(generation);
                let mut scope_ready = false;
                let mut catalog_pending = true;
                let mut started = Vec::new();
                for (index, catalogue) in
                    [catalogue_first, !catalogue_first].into_iter().enumerate()
                {
                    if catalogue {
                        if plan.refresh_panel(std::slice::from_ref(&current)) {
                            advance_query(&mut generation, &mut pending, true);
                        }
                        catalog_pending = false;
                    } else {
                        scope_ready = true;
                    }
                    if plan.take_scope_query(&mut pending, generation, scope_ready, catalog_pending)
                    {
                        let QueryPlan::Panel(panel) = &plan else {
                            panic!("the refreshed panel remains selected");
                        };
                        started.push(panel.query_source.clone().unwrap());
                        advance_query(&mut generation, &mut pending, false);
                    }
                    assert_eq!(started.len(), index);
                }
                assert!(Arc::ptr_eq(
                    &started[0],
                    current.query_source.as_ref().unwrap()
                ));
                assert!(!plan.take_scope_query(&mut pending, generation, true, false));
            }
        }
    }

    #[test]
    fn cancelling_between_refresh_completions_never_restarts_the_panel() {
        for catalogue_first in [false, true] {
            let previous = grafana_panel();
            let mut current = previous.clone();
            current.query_source = Some(Arc::new(k10s_core::ServiceIdentity {
                uid: "grafana-b".into(),
                resource_version: "2".into(),
                ..previous.query_source.as_deref().unwrap().clone()
            }));
            let mut plan = QueryPlan::Panel(Box::new(previous));
            let mut generation = 7;
            let mut pending = Some(generation);
            let mut scope_ready = false;
            let mut catalog_pending = true;
            for catalogue in [catalogue_first, !catalogue_first] {
                if catalogue {
                    assert!(plan.refresh_panel(std::slice::from_ref(&current)));
                    advance_query(&mut generation, &mut pending, true);
                    catalog_pending = false;
                } else {
                    scope_ready = true;
                }
                assert!(!plan.take_scope_query(
                    &mut pending,
                    generation,
                    scope_ready,
                    catalog_pending,
                ));
                advance_query(&mut generation, &mut pending, false);
            }
            assert!(pending.is_none());
            assert!(!plan.take_scope_query(&mut pending, generation, true, false));
        }
        let mut generation = 8;
        let mut stale_scope = Some(7);
        advance_query(&mut generation, &mut stale_scope, true);
        assert!(stale_scope.is_none());
    }

    #[test]
    fn unavailable_catalogue_rows_never_resume_unrelated_readiness_or_raw_queries() {
        let panel = grafana_panel();
        let mut different = panel.clone();
        different.expr = "sum(up)".into();
        let identical = [panel.clone(), panel.clone()];
        let ambiguous = [panel, different];
        for panels in [&[][..], &identical[..], &ambiguous[..]] {
            for catalogue_first in [false, true] {
                let mut plan = QueryPlan::Panel(Box::new(grafana_panel()));
                let mut generation = 7;
                let mut pending = Some(generation);
                let mut scope_ready = false;
                let mut catalog_pending = true;
                for catalogue in [catalogue_first, !catalogue_first] {
                    if catalogue {
                        assert!(plan.refresh_panel(panels));
                        advance_query(&mut generation, &mut pending, false);
                        catalog_pending = false;
                    } else {
                        scope_ready = true;
                    }
                    assert!(!plan.take_scope_query(
                        &mut pending,
                        generation,
                        scope_ready,
                        catalog_pending,
                    ));
                }
                assert!(matches!(plan, QueryPlan::Readiness));
                assert!(pending.is_none());
            }
        }
        for mut plan in [QueryPlan::Prom("up".into()), QueryPlan::Loki("{}".into())] {
            let mut pending = None;
            assert!(!plan.refresh_panel(&[grafana_panel()]));
            assert!(!plan.take_scope_query(&mut pending, 7, true, false));
            assert!(matches!(plan, QueryPlan::Prom(_) | QueryPlan::Loki(_)));
        }
    }
}
