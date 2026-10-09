//! Workload investigations resolve Kubernetes identity before querying.
//!
//! Native plots and the inspector share samples, source and time bounds.
//! Each request belongs to one generation and is aborted when replaced or
//! hidden. Unsupported Grafana semantics remain labelled and raw queries
//! remain explicit. A span link retains its recorded identity and time range
//! independently of current Kubernetes metadata.

use crate::chart::{ChartData, ChartView, display_message, display_notice_groups};
use crate::table::{TableState, TableText};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EnterFilter, ObserveFocusLogs, ObserveFocusPanels,
    ObserveOpenGrafana, ObserveOtlpLogs, ObservePodLabelLogs, ObserveRangeFifteenMinutes,
    ObserveRangeOneHour, ObserveReadiness, ObserveToggleDialect, ObserveToggleResolution, OpenRow,
    Refresh, RowDown, RowEnd, RowHome, RowPageDown, RowPageUp, RowUp,
};
use gpui::{
    Context, Entity, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Render, ScrollWheelEvent, SharedString, Styled, Window, div, prelude::*, px,
    rgb,
};
use k10s_provider::{
    ClusterProvider, DescribeRequest, GrafanaOutcome, GrafanaPanelKind, GrafanaPanelRow, LogEntry,
    LokiOutcome, LokiRequest, ObserveScope, PromOutcome, PromRequest, PromSeriesView, QueryDialect,
    QueryWindow, RequestStop, ScopeOutcome, SpanLogQuery, TableColumn, TablePage, TableRow,
    ToolPresence,
};
use std::{
    rc::Rc,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

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

impl ObserveTable {
    fn state<'a>(self, panels: &'a mut TableState, logs: &'a mut TableState) -> &'a mut TableState {
        match self {
            Self::Panels => panels,
            Self::Logs => logs,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Panels => "Panel table active",
            Self::Logs => "Log table active",
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryIdentity {
    generation: u64,
    request: Option<DescribeRequest>,
    window: QueryWindow,
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
    provider: Rc<dyn ClusterProvider>,
    table: TableState,
    table_text: TableText,
    panels: Vec<GrafanaPanelRow>,
    logs: Vec<LogEntry>,
    log_table: TableState,
    log_text: TableText,
    table_focus: ObserveTable,
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
    catalog_notice: SharedString,
    scope_label: SharedString,
    visible: bool,
    generation: u64,
    query_generation: u64,
    scope_generation: u64,
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
            provider,
            table: TableState::new(),
            table_text: TableText::default(),
            panels: Vec::new(),
            logs: Vec::new(),
            log_table: TableState::new(),
            log_text: TableText::default(),
            table_focus: ObserveTable::Panels,
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
            catalog_notice: SharedString::default(),
            scope_label: "Select a pod or workload".into(),
            visible: false,
            generation: 0,
            query_generation: 0,
            scope_generation: 0,
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
        if self.visible {
            self.resolve(cx);
        }
        cx.notify();
    }

    fn cancel_query(&mut self, cx: &mut Context<Self>) {
        self.query_stop = None;
        self.query_generation += 1;
        self.chart.update(cx, |chart, cx| chart.set_data(None, cx));
        self.logs.clear();
        self.table_focus = ObserveTable::Panels;
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
        self.window = match &self.plan {
            QueryPlan::SpanLogs(query) => query.request.window,
            _ => recent_window(self.range_ms, self.window.step_ms),
        };
        if !self.catalogue_enabled {
            self.catalog_pending = false;
            self.probe_pending = false;
            self.reach_notice = SharedString::default();
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
                    this.reach_notice = reach.error.map_or_else(SharedString::default, |why| {
                        failure_notice("Source discovery", &why)
                    });
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
                    this.panels.clear();
                    match outcome {
                        GrafanaOutcome::Catalog {
                            panels,
                            truncated,
                            notices,
                        } => {
                            this.grafana = ToolPresence::Ready;
                            this.panels = panels;
                            this.table.set_page(panels_page(&this.panels, truncated));
                            this.catalog_notice =
                                catalog_notice(this.panels.len(), truncated, notices);
                        }
                        GrafanaOutcome::Absent => {
                            this.grafana = ToolPresence::Missing;
                            this.table.set_page(TablePage::default());
                            this.catalog_notice = SharedString::default();
                        }
                        GrafanaOutcome::Denied(what) => {
                            this.grafana = ToolPresence::Blocked;
                            this.catalog_notice =
                                format!("{what}: access denied for this account").into();
                            this.table.set_page(TablePage::default());
                        }
                        GrafanaOutcome::Failed(why) => {
                            this.grafana = ToolPresence::Blocked;
                            this.catalog_notice = failure_notice("Grafana catalog failed", &why);
                            this.table.set_page(TablePage::default());
                        }
                    }
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

    fn focus_table(&mut self, table: ObserveTable, cx: &mut Context<Self>) {
        if self.table_focus != table {
            self.table_focus = table;
            cx.notify();
        }
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
                self.status = "Select a pod or workload".into();
            }
            return;
        };
        let generation = self.scope_generation;
        let query_generation = self.query_generation;
        if requires_scope {
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
                    match outcome {
                        ScopeOutcome::Ready(scope) => {
                            if scope.request != request {
                                this.scope_label =
                                    "Resolved metadata does not match the selected resource".into();
                                if requires_scope && this.query_generation == query_generation {
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
                            if requires_scope && this.query_generation == query_generation {
                                this.run_plan(cx);
                            }
                        }
                        ScopeOutcome::Denied(what) => {
                            this.scope_label =
                                format!("{what}: access denied for this account").into();
                            if requires_scope && this.query_generation == query_generation {
                                this.status = "Scope unavailable".into();
                            }
                        }
                        ScopeOutcome::Failed(why) => {
                            this.scope_label = failure_notice("Kubernetes scope failed", &why);
                            if requires_scope && this.query_generation == query_generation {
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

    fn begin_query(&mut self, cx: &mut Context<Self>) -> QueryIdentity {
        self.cancel_query(cx);
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
                            warnings,
                            infos,
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
                            this.status = failure_notice("Prometheus query failed", &why)
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
        let Some(uid) = self.table.selected_row().map(|row| row.uid.as_str()) else {
            return;
        };
        let Some(panel) = self
            .panels
            .iter()
            .find(|panel| panel_uid(panel) == uid)
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

    fn run_panel(&mut self, panel: GrafanaPanelRow, cx: &mut Context<Self>) {
        self.plan = QueryPlan::Panel(Box::new(panel.clone()));
        self.query = panel.expr.clone();
        self.query_kind = QueryKind::Prom;
        if let Some(why) = &panel.unsupported {
            self.cancel_query(cx);
            self.status = failure_notice("Unsupported panel", why);
            cx.notify();
            return;
        }
        if !matches!(
            panel.kind,
            GrafanaPanelKind::Timeseries | GrafanaPanelKind::Stat
        ) || panel.dialect != QueryDialect::PromQL
            || panel.transformed
        {
            self.cancel_query(cx);
            self.status = "Unsupported panel semantics; use Open in Grafana or Raw query".into();
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
        let title = panel.title.clone();
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
        let Some(uid) = self.table.selected_row().map(|row| row.uid.as_str()) else {
            return;
        };
        let panel = self.panels.iter().find(|panel| panel_uid(panel) == uid);
        if let Some(panel) = panel
            && let Some(request) = &panel.browser_forward
            && let Some(path) =
                k10s_provider::grafana_panel_path(&panel.dashboard_uid, panel.panel_id)
        {
            cx.emit(ObserveEvent::GrafanaRoute {
                request: request.clone(),
                path,
            });
            return;
        }
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
        }
        if self.request.is_none() {
            self.scope_label = "No current Kubernetes selection".into();
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
        }
        self.catalog_pending = false;
        self.probe_pending = false;
        self.typing = false;
        self.filtering = false;
        self.scope = None;
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

    fn run_loki(&mut self, cx: &mut Context<Self>) {
        let identity = self.begin_query(cx);
        self.table_focus = ObserveTable::Logs;
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
                            this.status = failure_notice("Loki query failed", &why)
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

    fn active_table(&mut self) -> &mut TableState {
        self.table_focus.state(&mut self.table, &mut self.log_table)
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
            self.query_generation += 1;
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.table_text.update(&self.table);
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
        let button = |id, label| {
            div()
                .id(id)
                .px(px(8.0))
                .py(px(4.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme.shell.element_hover)))
                .child(label)
        };
        div()
            .id("observe")
            .key_context(observe_key_context(self.typing, self.filtering))
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
            .on_action(cx.listener(|this, _: &ObserveOpenGrafana, _, cx| {
                this.open_grafana(cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveFocusPanels, _, cx| {
                this.focus_table(ObserveTable::Panels, cx);
            }))
            .on_action(cx.listener(|this, _: &ObserveFocusLogs, _, cx| {
                this.focus_table(ObserveTable::Logs, cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.reload(cx)))
            .on_action(cx.listener(|this, _: &RowUp, _, cx| {
                this.active_table().move_selection(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                this.active_table().move_selection(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| {
                if this.typing {
                    this.typing = false;
                    this.raw_query(cx);
                } else if this.table_focus == ObserveTable::Logs {
                    this.follow_trace(cx);
                } else {
                    this.run_selected(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                if this.typing {
                    this.typing = false;
                    this.raw_query(cx);
                }
                this.filtering = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                this.typing = false;
                this.filtering = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EnterFilter, _, cx| {
                this.filtering = true;
                this.typing = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                if this.typing {
                    this.query.pop();
                    this.cancel_query(cx);
                } else if this.filtering {
                    this.active_table().pop_filter();
                }
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.control || event.keystroke.modifiers.alt {
                    return;
                }
                if this.typing || this.filtering {
                    if let Some(character) = &event.keystroke.key_char {
                        if this.typing {
                            this.query.push_str(character);
                            this.cancel_query(cx);
                        } else {
                            this.active_table().push_filter(character);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }
                } else if event.keystroke.key == "q" {
                    this.edit_query(cx);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &RowPageUp, _, cx| {
                this.active_table().page_by(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageDown, _, cx| {
                this.active_table().page_by(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowHome, _, cx| {
                this.active_table().select_first();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowEnd, _, cx| {
                this.active_table().select_last();
                cx.notify();
            }))
            .child(div().child(self.scope_label.clone()))
            .children(
                (!self.reach_notice.is_empty()).then(|| div().child(self.reach_notice.clone())),
            )
            .children(
                (!self.catalog_notice.is_empty()).then(|| div().child(self.catalog_notice.clone())),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
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
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        this.table_focus = ObserveTable::Panels;
                        let delta = f32::from(event.delta.pixel_delta(px(24.0)).y);
                        this.table.move_selection(-(delta / 24.0).round() as i64);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(
                        div().child(if self.catalogue_enabled && !self.catalog_pending {
                            "Grafana panels"
                        } else {
                            "Cached Grafana panels"
                        }),
                    )
                    .child(
                        div()
                            .font_family(fonts.buffer_family.clone())
                            .text_color(rgb(theme.shell.text_muted))
                            .child(SharedString::from(self.table_text.header())),
                    )
                    .children(
                        self.table_text
                            .lines()
                            .iter()
                            .enumerate()
                            .map(|(offset, line)| {
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
                                        cx.listener(move |this, _, _, cx| {
                                            this.table_focus = ObserveTable::Panels;
                                            this.table.select_visible_offset(offset);
                                            cx.notify();
                                        }),
                                    )
                                    .child(SharedString::from(line.clone()))
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                button("panel-run", "Run panel")
                                    .on_click(cx.listener(|this, _, _, cx| this.run_selected(cx))),
                            )
                            .child(
                                button("grafana-open", "Open in Grafana")
                                    .on_click(cx.listener(|this, _, _, cx| this.open_grafana(cx))),
                            ),
                    )
            }))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(button("query-edit", "Raw query").on_click(cx.listener(
                        |this, _, _, cx| {
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
            .child(div().child(self.status.clone()))
            .child(div().child(self.table_focus.label()))
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
                            this.table_focus = ObserveTable::Logs;
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
                                        cx.listener(move |this, _, _, cx| {
                                            this.table_focus = ObserveTable::Logs;
                                            this.log_table.select_visible_offset(offset);
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

fn observe_key_context(typing: bool, filtering: bool) -> &'static str {
    if typing || filtering {
        "Typing"
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
fn panel_uid(panel: &GrafanaPanelRow) -> String {
    format!(
        "{}:{}:{}",
        panel.dashboard_uid, panel.panel_id, panel.ref_id
    )
}
fn panels_page(panels: &[GrafanaPanelRow], truncated: bool) -> TablePage {
    TablePage {
        columns: ["Dashboard", "Panel", "Target", "Datasource", "State"]
            .into_iter()
            .map(|name| TableColumn {
                name: name.into(),
                wide: false,
            })
            .collect(),
        rows: panels
            .iter()
            .map(|panel| TableRow {
                name: panel.title.clone(),
                namespace: Some(panel.dashboard_title.clone()),
                uid: panel_uid(panel),
                cells: vec![
                    panel.dashboard_title.clone(),
                    panel.title.clone(),
                    panel.ref_id.clone(),
                    panel.datasource_uid.clone().unwrap_or_default(),
                    panel
                        .unsupported
                        .clone()
                        .unwrap_or_else(|| "Supported".into()),
                ],
            })
            .collect(),
        truncated,
        continue_token: None,
    }
}

fn catalog_notice(targets: usize, truncated: bool, notices: Vec<String>) -> SharedString {
    let mut state = Vec::new();
    if truncated {
        state.push(format!(
            "Grafana catalog truncated; showing {targets} targets"
        ));
    } else if targets == 0 {
        state.push("No Grafana query panels returned".into());
    }
    display_notice_groups(&[&state, &notices]).into()
}

fn failure_notice(source: &str, why: &str) -> SharedString {
    format!("{source}: {}", display_message(why)).into()
}

fn prom_notices(
    mut warnings: Vec<String>,
    mut infos: Vec<String>,
    association: String,
    dropped_series: Option<usize>,
) -> Vec<String> {
    let mut notices = vec![association];
    if let Some(dropped_series) = dropped_series {
        notices.push(format!("Truncated: {dropped_series} series omitted"));
    }
    notices.append(&mut warnings);
    notices.append(&mut infos);
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
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(char::is_whitespace)
        && !url.contains('"')
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn observe_shortcuts_reach_commands_only_outside_query_and_filter_input() {
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
            ("tab", "k10s_shell::ObserveFocusLogs"),
            ("shift-tab", "k10s_shell::ObserveFocusPanels"),
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
            let bindings = bindings_for(key, observe_key_context(false, false));
            assert_eq!(
                bindings.first().map(|binding| binding.action().name()),
                Some(command)
            );
            for (typing, filtering) in [(true, false), (false, true), (true, true)] {
                assert!(
                    bindings_for(key, observe_key_context(typing, filtering))
                        .iter()
                        .all(|binding| !binding.action().name().starts_with("k10s_shell::Observe")),
                    "{key} must not run an Observe command while entering text"
                );
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
                bindings_for(key, observe_key_context(false, false))
                    .first()
                    .map(|binding| binding.action().name()),
                Some(command)
            );
        }
        for (key, command) in [
            ("enter", "k10s_shell::CommitInput"),
            ("escape", "k10s_shell::CancelInput"),
            ("backspace", "k10s_shell::DeleteInputChar"),
        ] {
            assert_eq!(
                bindings_for(key, observe_key_context(true, false))
                    .first()
                    .map(|binding| binding.action().name()),
                Some(command)
            );
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
        let notice = catalog_notice(
            2,
            false,
            vec!["configmaps: access denied for this account".into()],
        );
        assert_eq!(
            notice.as_ref(),
            "configmaps: access denied for this account"
        );
        assert!(
            catalog_notice(2, true, vec!["provisioning list failed".into()])
                .contains("Grafana catalog truncated; showing 2 targets; provisioning list failed")
        );
        assert!(catalog_notice(0, false, Vec::new()).contains("No Grafana query panels"));
        assert!(catalog_notice(2, false, Vec::new()).is_empty());
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
        );
        assert_eq!(notices[2].as_ptr(), pointer);
        assert_eq!(notices[2].len(), warning_bytes);
        assert_eq!(notices[3], info);
        let shown = display_notice_groups(&[&notices]);
        assert!(shown.starts_with(
            "Kubernetes UID association is unproven; Truncated: 5 series omitted; server warning"
        ));
        assert!(shown.contains("[abbreviated]"));
        assert!(shown.len() <= crate::chart::MAX_NOTICE_TEXT_BYTES);
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
        let catalog = catalog_notice(2, false, vec![failure; 20]);
        assert!(catalog.contains("[abbreviated]"));
        assert!(catalog.contains("12 notices omitted"));
        assert!(catalog.len() <= crate::chart::MAX_NOTICE_TEXT_BYTES);
    }
}
