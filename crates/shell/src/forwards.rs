//! The forwards item: every active port-forward, with start and stop.
//!
//! A thin view over the provider's forward registry, reusing the pure table
//! machine. The registry is the truth: this view lists what it holds --
//! including forwards that died, shown with their reason until closed -- and
//! refreshes after every action and on `r`; it never polls, because an idle
//! shell must not paint. Starting arrives either from a pod or service row in
//! the browser or from a selection, as a [`ForwardRequest`] the provider
//! resolves; every outcome lands in the status line, labelled, and the line is
//! about the newest start rather than whichever open happened to answer last.

use std::rc::Rc;

use gpui::{
    Context, FocusHandle, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, Role,
    ScrollWheelEvent, SharedString, Styled, Window, canvas, div, prelude::*, px, rgb,
};

use crate::table::TableState;
use crate::ui::{LIST_ROW_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{Refresh, RowDown, RowEnd, RowHome, RowPageDown, RowPageUp, RowUp, StopForward};
use k10s_provider::{
    ClusterProvider, ForwardLease, ForwardOutcome, ForwardRequest, ForwardRow, ForwardStartOutcome,
    ForwardState, ForwardTerminal, ProviderSlot, RequestStop, TableColumn, TablePage, TableRow,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;
const NO_FORWARDS: &str = "no forwards open; F on a pod or service row in the browser starts one";

pub struct ForwardsView {
    focus: FocusHandle,
    slot: Rc<ProviderSlot>,
    table: TableState,
    status: Option<String>,
    viewport: Viewport,
    browser: Option<PendingBrowser>,
    browser_watch: Option<BrowserWatch>,
    // Which start request the status line is about. Two opens can be in flight
    // at once and can answer out of order, and the sentence on screen has to be
    // about the one the user asked for last, not whichever finished last.
    generation: u64,
}

struct PendingBrowser {
    _stop: RequestStop,
    lease: Option<ForwardLease>,
}

struct BrowserWatch {
    _stop: RequestStop,
    id: u64,
    provider: Rc<dyn ClusterProvider>,
}

impl ForwardsView {
    pub fn new(
        slot: Rc<ProviderSlot>,
        start: Option<ForwardRequest>,
        cx: &mut Context<Self>,
    ) -> ForwardsView {
        let mut view = ForwardsView {
            focus: cx.focus_handle(),
            slot,
            table: TableState::new(),
            status: None,
            viewport: Viewport::default(),
            browser: None,
            browser_watch: None,
            generation: 0,
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
            .set_page(forwards_page(&self.slot.current().list_forwards()));
    }

    pub fn start(&mut self, request: ForwardRequest, cx: &mut Context<Self>) {
        self.start_route(request, None, cx);
    }

    pub fn open_browser(&mut self, request: ForwardRequest, path: String, cx: &mut Context<Self>) {
        self.start_route(request, Some(path), cx);
    }

    fn start_route(
        &mut self,
        request: ForwardRequest,
        browser_path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        self.browser_watch = None;
        if self.browser.take().is_some() {
            self.refresh();
        }
        self.status = Some(if browser_path.is_some() {
            format!(
                "opening a forward to {}/{}; x cancels",
                request.namespace, request.name
            )
        } else {
            format!(
                "opening a forward to {}/{}...",
                request.namespace, request.name
            )
        });
        let provider = self.slot.current();
        if let Some(path) = browser_path {
            self.start_browser(provider, request, path, generation, cx);
            cx.notify();
            return;
        }
        let (tx, rx) = futures::channel::oneshot::channel();
        provider.open_forward(
            &request,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation == generation && Rc::ptr_eq(&provider, &this.slot.current())
                    {
                        this.status = Some(match outcome {
                            ForwardOutcome::Opened(row) => forward_status(&row),
                            ForwardOutcome::Denied(what) => {
                                format!("{what}: access denied for this account")
                            }
                            ForwardOutcome::Failed(why) => why,
                        });
                    }
                    this.refresh();
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn start_browser(
        &mut self,
        provider: Rc<dyn ClusterProvider>,
        request: ForwardRequest,
        path: String,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let (tx, rx) = futures::channel::oneshot::channel();
        let stop = provider.open_forward_cancellable(
            &request,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        self.browser = Some(PendingBrowser {
            _stop: stop,
            lease: None,
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await.unwrap_or_else(|_| {
                ForwardStartOutcome::Failed("the forward request ended without a result".into())
            });
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    drop(outcome);
                    this.refresh();
                    cx.notify();
                    return;
                }
                this.browser = None;
                if !Rc::ptr_eq(&provider, &this.slot.current()) {
                    drop(outcome);
                    this.status = Some("connection changed before the browser opened".into());
                } else {
                    match outcome {
                        ForwardStartOutcome::Started(lease) => {
                            this.wait_for_browser(provider, lease, path, generation, cx);
                        }
                        ForwardStartOutcome::Denied(what) => {
                            this.status = Some(format!("{what}: access denied for this account"));
                        }
                        ForwardStartOutcome::Failed(why) => this.status = Some(why),
                    }
                }
                this.refresh();
                cx.notify();
            });
        })
        .detach();
    }

    fn wait_for_browser(
        &mut self,
        provider: Rc<dyn ClusterProvider>,
        lease: ForwardLease,
        path: String,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        self.status = Some("waiting for the local forward listener; x cancels".into());
        let (tx, rx) = futures::channel::oneshot::channel();
        let stop = provider.wait_forward_ready(
            lease.row().id,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        self.browser = Some(PendingBrowser {
            _stop: stop,
            lease: Some(lease),
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await.unwrap_or_else(|_| {
                ForwardOutcome::Failed("the listener wait ended without a result".into())
            });
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let Some(mut pending) = this.browser.take() else {
                    return;
                };
                let Some(lease) = pending.lease.take() else {
                    return;
                };
                drop(pending);
                let provider_current = Rc::ptr_eq(&provider, &this.slot.current());
                let (opened, status) = if !provider_current {
                    (None, "connection changed before the browser opened".into())
                } else {
                    match browser_source_outcome(outcome, lease.row(), |source| {
                        provider.grafana_source_is_current(source)
                    }) {
                        ForwardOutcome::Opened(row)
                            if browser_handoff_allowed(
                                &this.slot,
                                &provider,
                                lease.row(),
                                &row,
                                |id| provider.forward_active(id),
                            ) =>
                        {
                            cx.open_url(&format!("http://127.0.0.1:{}{path}", row.local_port));
                            (Some(row.id), format!(
                                "local listener open on 127.0.0.1:{}; remote connection is not yet verified",
                                row.local_port
                            ))
                        }
                        ForwardOutcome::Opened(_) => {
                            (None, "the local forward is no longer active; browser was not opened".into())
                        }
                        ForwardOutcome::Denied(what) => {
                            (None, format!("{what}: access denied for this account"))
                        }
                        ForwardOutcome::Failed(why) => (None, why),
                    }
                };
                if let Some(id) = opened {
                    lease.keep();
                    this.watch_browser(provider, id, generation, cx);
                } else {
                    drop(lease);
                }
                this.status = Some(status);
                this.refresh();
                cx.notify();
            });
        })
        .detach();
    }

    fn watch_browser(
        &mut self,
        provider: Rc<dyn ClusterProvider>,
        id: u64,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let (tx, rx) = futures::channel::oneshot::channel();
        let stop = provider.wait_forward_terminal(
            id,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        self.browser_watch = Some(BrowserWatch {
            _stop: stop,
            id,
            provider: provider.clone(),
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation
                    || !Rc::ptr_eq(&provider, &this.slot.current())
                    || !this.browser_watch.as_ref().is_some_and(|watch| {
                        watch.id == id && Rc::ptr_eq(&watch.provider, &provider)
                    })
                {
                    return;
                }
                this.browser_watch = None;
                this.status = Some(match outcome {
                    Ok(ForwardTerminal::Dead(why)) => format!("forward failed: {why}"),
                    Ok(ForwardTerminal::Closed) => "forward closed".into(),
                    Err(_) => "the forward status watch ended without a result".into(),
                });
                this.refresh();
                cx.notify();
            });
        })
        .detach();
    }

    fn stop_selected(&mut self, cx: &mut Context<Self>) {
        if self.browser.take().is_some() {
            self.generation += 1;
            self.status = Some("browser forward request cancelled".into());
            self.refresh();
            cx.notify();
            return;
        }
        let Some(id) = self
            .table
            .selected_row()
            .and_then(|row| row.uid.parse().ok())
        else {
            return;
        };
        let provider = self.slot.current();
        if provider.close_forward(id) {
            if self
                .browser_watch
                .as_ref()
                .is_some_and(|watch| watch.id == id && Rc::ptr_eq(&watch.provider, &provider))
            {
                self.generation += 1;
                self.browser_watch = None;
            }
            self.status = Some("forward closed".to_string());
        }
        self.refresh();
        cx.notify();
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

fn browser_handoff_allowed(
    slot: &ProviderSlot,
    provider: &Rc<dyn ClusterProvider>,
    expected: &ForwardRow,
    ready: &ForwardRow,
    is_active: impl FnOnce(u64) -> bool,
) -> bool {
    Rc::ptr_eq(provider, &slot.current())
        && ready.state == ForwardState::Active
        && expected.id == ready.id
        && expected.namespace == ready.namespace
        && expected.pod == ready.pod
        && ready.local_port != 0
        && (expected.local_port == ready.local_port
            || (expected.local_port == 0 && expected.state == ForwardState::Opening))
        && expected.remote_port == ready.remote_port
        && expected.expected_service == ready.expected_service
        && is_active(ready.id)
}

fn browser_source_current(
    row: &ForwardRow,
    is_current: impl FnOnce(&k10s_core::ServiceIdentity) -> bool,
) -> bool {
    row.expected_service.as_deref().is_none_or(is_current)
}

fn browser_source_outcome(
    outcome: ForwardOutcome,
    row: &ForwardRow,
    is_current: impl FnOnce(&k10s_core::ServiceIdentity) -> bool,
) -> ForwardOutcome {
    match outcome {
        ForwardOutcome::Opened(_) if !browser_source_current(row, is_current) => {
            ForwardOutcome::Failed(
                "Grafana source is unverified or refreshing; refresh the catalogue before opening the browser"
                    .into(),
            )
        }
        outcome => outcome,
    }
}

fn local_address(port: u16) -> String {
    if port == 0 {
        "auto".into()
    } else {
        format!("127.0.0.1:{port}")
    }
}

fn forward_status(row: &ForwardRow) -> String {
    match &row.state {
        ForwardState::Opening if row.local_port == 0 => {
            format!("opening auto -> {}:{}", row.pod, row.remote_port)
        }
        ForwardState::Opening => format!(
            "opening 127.0.0.1:{} -> {}:{}",
            row.local_port, row.pod, row.remote_port
        ),
        ForwardState::Active => format!(
            "local listener open on 127.0.0.1:{} -> {}:{}",
            row.local_port, row.pod, row.remote_port
        ),
        ForwardState::Dead(why) => format!("forward failed: {why}"),
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
                local_address(row.local_port),
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
            .key_context("Browse Forwards")
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
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("forward-rows")
                            .flex_1()
                            .min_h(px(LIST_ROW_HEIGHT))
                            .overflow_hidden()
                            .relative()
                            .flex()
                            .flex_col()
                            .role(Role::ListBox)
                            .aria_label("Port forwards")
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
                                div()
                                    .p(px(12.0))
                                    .text_size(px(fonts.ui_size))
                                    .text_color(rgb(theme.shell.text_muted))
                                    .child(NO_FORWARDS)
                            })),
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
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child("x stop or cancel · r refresh"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_refreshing_blocks_only_successful_listener_outcomes() {
        let source = std::sync::Arc::new(k10s_core::ServiceIdentity {
            namespace: "monitoring".into(),
            name: "grafana".into(),
            port: 3000,
            uid: "grafana-a".into(),
            resource_version: "1".into(),
        });
        let row = ForwardRow {
            id: 7,
            namespace: "monitoring".into(),
            pod: "grafana".into(),
            local_port: 3000,
            remote_port: 3000,
            expected_service: Some(source),
            state: ForwardState::Active,
        };
        for outcome in [
            ForwardOutcome::Denied("pods/portforward"),
            ForwardOutcome::Failed("Address already in use (os error 98)".into()),
        ] {
            let calls = std::cell::Cell::new(0);
            let expected = outcome.clone();
            assert_eq!(
                browser_source_outcome(outcome, &row, |_| {
                    calls.set(calls.get() + 1);
                    false
                }),
                expected,
            );
            assert_eq!(calls.get(), 0, "a failed listener needs no source check");
        }
        assert!(matches!(
            browser_source_outcome(ForwardOutcome::Opened(row.clone()), &row, |_| false),
            ForwardOutcome::Failed(why) if why.contains("unverified or refreshing")
        ));
        assert_eq!(
            browser_source_outcome(ForwardOutcome::Opened(row.clone()), &row, |_| true),
            ForwardOutcome::Opened(row),
        );
    }

    #[test]
    fn browser_handoff_requires_the_same_provider_and_active_forward() {
        let original: Rc<dyn ClusterProvider> = Rc::new(k10s_provider::NullProvider);
        let replacement: Rc<dyn ClusterProvider> = Rc::new(k10s_provider::NullProvider);
        let slot = ProviderSlot::new(original.clone());
        let expected = ForwardRow {
            id: 0,
            namespace: "monitoring".into(),
            pod: "grafana".into(),
            local_port: 3000,
            remote_port: 3000,
            expected_service: None,
            state: ForwardState::Opening,
        };
        let mut ready = expected.clone();
        ready.state = ForwardState::Active;
        assert!(browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |id| id == 0
        ));
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| false
        ));
        ready.local_port = 3001;
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| true
        ));
        ready.local_port = 3000;
        ready.id = 1;
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| true
        ));
        ready.id = 0;
        ready.pod = "other-grafana".into();
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| true
        ));
        ready.pod = "grafana".into();
        ready.state = ForwardState::Dead("closed".into());
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| true
        ));
        ready.state = ForwardState::Active;
        slot.set(replacement);
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| { panic!("a replaced provider must not be queried") }
        ));
    }

    #[test]
    fn automatic_browser_handoff_requires_the_exact_owned_nonzero_listener() {
        let original: Rc<dyn ClusterProvider> = Rc::new(k10s_provider::NullProvider);
        let replacement: Rc<dyn ClusterProvider> = Rc::new(k10s_provider::NullProvider);
        let slot = ProviderSlot::new(original.clone());
        let source = std::sync::Arc::new(k10s_core::ServiceIdentity {
            namespace: "monitoring".into(),
            name: "grafana".into(),
            port: 80,
            uid: "service-a".into(),
            resource_version: "1".into(),
        });
        let expected = ForwardRow {
            id: 7,
            namespace: "monitoring".into(),
            pod: "grafana-a".into(),
            local_port: 0,
            remote_port: 3000,
            expected_service: Some(source),
            state: ForwardState::Opening,
        };
        let mut ready = expected.clone();
        ready.local_port = 43123;
        ready.state = ForwardState::Active;
        assert!(browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| true
        ));
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| false
        ));
        for changed in 0..7 {
            let mut invalid = ready.clone();
            match changed {
                0 => invalid.local_port = 0,
                1 => invalid.id += 1,
                2 => invalid.namespace = "other".into(),
                3 => invalid.pod = "grafana-b".into(),
                4 => invalid.remote_port = 3001,
                5 => {
                    std::sync::Arc::make_mut(invalid.expected_service.as_mut().unwrap()).uid =
                        "service-b".into()
                }
                6 => invalid.state = ForwardState::Opening,
                _ => unreachable!(),
            }
            assert!(!browser_handoff_allowed(
                &slot,
                &original,
                &expected,
                &invalid,
                |_| true
            ));
        }
        let mut dead = expected.clone();
        dead.state = ForwardState::Dead("closed before handoff".into());
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &dead,
            &ready,
            |_| true
        ));
        assert_eq!(local_address(expected.local_port), "auto");
        assert_eq!(local_address(ready.local_port), "127.0.0.1:43123");
        slot.set(replacement);
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| panic!("a replaced provider must not be queried")
        ));
    }

    #[test]
    fn a_forward_row_renders_its_lifecycle_state_including_death() {
        let rows = vec![
            ForwardRow {
                id: 7,
                namespace: "prod".to_string(),
                pod: "api-1".to_string(),
                local_port: 8080,
                remote_port: 80,
                expected_service: None,
                state: ForwardState::Active,
            },
            ForwardRow {
                id: 9,
                namespace: "prod".to_string(),
                pod: "web-1".to_string(),
                local_port: 3000,
                remote_port: 3000,
                expected_service: None,
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
    #[test]
    fn browser_handoff_keeps_its_origin_and_rechecks_the_verified_source() {
        use std::sync::Arc;
        let original: Rc<dyn ClusterProvider> = Rc::new(k10s_provider::NullProvider);
        let slot = ProviderSlot::new(original.clone());
        let source = Arc::new(k10s_core::ServiceIdentity {
            namespace: "monitoring".into(),
            name: "grafana".into(),
            port: 3000,
            uid: "grafana-a".into(),
            resource_version: "1".into(),
        });
        let expected = ForwardRow {
            id: 7,
            namespace: "monitoring".into(),
            pod: "grafana-pod".into(),
            local_port: 3000,
            remote_port: 3000,
            expected_service: Some(source.clone()),
            state: ForwardState::Opening,
        };
        let mut ready = expected.clone();
        ready.state = ForwardState::Active;
        assert!(browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| true
        ));
        assert!(browser_source_current(&expected, |identity| identity == source.as_ref()));
        assert!(!browser_source_current(&expected, |_| false));
        let mut replacement = source.as_ref().clone();
        replacement.uid = "grafana-b".into();
        replacement.resource_version = "2".into();
        assert!(!browser_source_current(&expected, |identity| identity == &replacement));
        ready.expected_service = Some(Arc::new(replacement));
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| panic!("another source must be refused before the listener check")
        ));
        ready.expected_service = None;
        assert!(!browser_handoff_allowed(
            &slot,
            &original,
            &expected,
            &ready,
            |_| true
        ));
        let mut ordinary = expected;
        ordinary.expected_service = None;
        assert!(browser_source_current(&ordinary, |_| panic!(
            "ordinary forwards have no catalogue source to verify"
        )));
    }
}
