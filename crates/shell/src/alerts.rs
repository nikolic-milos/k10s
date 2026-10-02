//! Alert labels joined to the current map, and a reviewed silence for a pick.
//!
//! Joins require both namespace and pod. Silence matchers are exact equality
//! on the selected namespace or pod; an owner name never becomes a pod regex.
//! The endpoint, provider, matchers, and requested interval stay fixed through
//! confirmation. Editing the reason discards the review.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    Context, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Render, Role, SharedString, Styled, Window, div, prelude::*, px, rgb,
};
use k10s_core::{Level, SceneSnapshot};

use crate::provider::{
    AlertMatcher, AlertmanagerEndpoint, ReadProvider, SilenceOutcome, SilenceRequest,
};
use crate::selection::Selection;
use crate::{CancelInput, CommitInput, DeleteInputChar};

pub(crate) fn pod_uid(snapshot: &SceneSnapshot, namespace: &str, pod: &str) -> Option<Arc<str>> {
    if namespace.is_empty() || pod.is_empty() {
        return None;
    }
    let region = snapshot
        .regions
        .iter()
        .enumerate()
        .position(|(slot, region)| {
            region.label.as_ref() == namespace
                && snapshot
                    .ids
                    .regions
                    .get(slot)
                    .is_some_and(|uid| !uid.is_empty())
        })?;
    let mut found = None;
    snapshot.for_each_region_block(region, |block, _| {
        snapshot.for_each_block_cell(block, |cell, node| {
            if node.label.as_ref() == pod
                && let Some(uid) = snapshot.ids.cells.get(cell).filter(|uid| !uid.is_empty())
            {
                found = Some(uid.clone());
            }
        });
    });
    found
}

pub(crate) fn matchers(selection: &Selection) -> Result<Vec<AlertMatcher>, String> {
    let equal = |name: &str, value: &str| AlertMatcher {
        name: name.to_string(),
        value: value.to_string(),
        is_regex: false,
        is_equal: true,
    };
    if selection.name.is_empty() || selection.uid.is_empty() {
        return Err("the selection no longer names an object".to_string());
    }
    match selection.level {
        Level::Region => Ok(vec![equal("namespace", &selection.name)]),
        Level::Cell => {
            let namespace = selection.namespace.as_deref().filter(|name| !name.is_empty())
                .ok_or_else(|| "the selected pod has no namespace".to_string())?;
            Ok(vec![equal("namespace", namespace), equal("pod", &selection.name)])
        }
        Level::Block | Level::Sat => Err(
            "pick a namespace or pod to silence its alerts; this selection has no exact alert matchers".to_string(),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Editing,
    Reviewing,
    Ready,
    Writing,
    Finished,
}

struct SilenceState {
    endpoint: AlertmanagerEndpoint,
    matchers: Vec<AlertMatcher>,
    comment: String,
    phase: Phase,
    request: Option<SilenceRequest>,
    interval: Option<std::ops::Range<String>>,
    status: String,
}

impl SilenceState {
    fn new(endpoint: AlertmanagerEndpoint, matchers: Vec<AlertMatcher>) -> Self {
        Self {
            endpoint,
            matchers,
            comment: String::new(),
            phase: Phase::Editing,
            request: None,
            interval: None,
            status: "Type a reason. Enter reviews a one-hour silence. Ctrl-U clears the reason; Ctrl-V pastes.".to_string(),
        }
    }

    fn editable(&self) -> bool {
        matches!(self.phase, Phase::Editing | Phase::Ready)
    }

    fn edit(&mut self, comment: String) {
        if !self.editable() || self.comment == comment {
            return;
        }
        self.comment = comment;
        self.cancel_review();
    }

    fn append_reason(&mut self, text: &str) {
        if !self.editable() {
            return;
        }
        let addition: String = text
            .chars()
            .filter(|ch| !ch.is_control())
            .take(1025)
            .collect();
        if self.comment.chars().count() + addition.chars().count() > 1024 {
            self.cancel_review();
            self.status =
                "the reason exceeds 1024 characters; that input was not added".to_string();
            return;
        }
        self.edit(format!("{}{addition}", self.comment));
    }

    fn cancel_review(&mut self) {
        if self.editable() {
            self.phase = Phase::Editing;
            self.request = None;
            self.interval = None;
            self.status = "Enter reviews the current reason and matchers.".to_string();
        }
    }

    fn submit(&mut self, now: SystemTime) -> Option<(SilenceRequest, bool)> {
        match self.phase {
            Phase::Editing => {
                if self.comment.trim().is_empty() {
                    self.status = "a silence needs a reason".to_string();
                    return None;
                }
                let Ok(since_epoch) = now.duration_since(UNIX_EPOCH) else {
                    self.status =
                        "the system clock is before 1970; a silence cannot be dated".to_string();
                    return None;
                };
                let start = UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs());
                let request = SilenceRequest {
                    endpoint: self.endpoint.clone(),
                    matchers: self.matchers.clone(),
                    window: start..start + Duration::from_secs(3600),
                    created_by: "k10s".to_string(),
                    comment: self.comment.trim().to_string(),
                };
                self.comment.clone_from(&request.comment);
                self.request = Some(request.clone());
                self.phase = Phase::Reviewing;
                self.status = "checking the silence for review...".to_string();
                Some((request, false))
            }
            Phase::Ready => {
                let request = self.request.as_ref()?;
                if now >= request.window.end {
                    self.cancel_review();
                    self.status =
                        "this review has expired; Enter prepares a new interval".to_string();
                    return None;
                }
                self.phase = Phase::Writing;
                self.status = "creating the reviewed silence...".to_string();
                Some((request.clone(), true))
            }
            Phase::Reviewing | Phase::Writing | Phase::Finished => None,
        }
    }

    fn adopt(&mut self, outcome: SilenceOutcome) {
        match outcome {
            SilenceOutcome::NeedsConfirm { starts_at, ends_at }
                if self.phase == Phase::Reviewing =>
            {
                self.interval = Some(starts_at..ends_at);
                self.phase = Phase::Ready;
                self.status = "Enter again confirms these matchers and this interval. Escape cancels the review.".to_string();
            }
            SilenceOutcome::Applied { id } if self.phase == Phase::Writing => {
                self.phase = Phase::Finished;
                self.status = format!("created silence {id}");
            }
            SilenceOutcome::Denied { what, why } => {
                self.phase = Phase::Finished;
                self.status = format!("{what}: {why}");
            }
            SilenceOutcome::Failed(why) => {
                self.phase = Phase::Finished;
                self.status = why;
            }
            _ => {
                self.phase = Phase::Finished;
                self.status =
                    "the silence reply did not match the operation being reviewed".to_string();
            }
        }
    }
}

pub(crate) struct SilenceView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    context: String,
    state: SilenceState,
}

impl SilenceView {
    pub(crate) fn new(
        provider: Rc<dyn ReadProvider>,
        context: Option<String>,
        endpoint: AlertmanagerEndpoint,
        matchers: Vec<AlertMatcher>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus: cx.focus_handle(),
            provider,
            context: context.map_or_else(
                || "this cluster's in-cluster account".to_string(),
                |name| format!("context {name}"),
            ),
            state: SilenceState::new(endpoint, matchers),
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if let Some((request, confirm)) = self.state.submit(SystemTime::now()) {
            let (tx, rx) = futures::channel::oneshot::channel();
            self.provider.create_silence(
                &request,
                confirm,
                Box::new(move |outcome| {
                    let _ = tx.send(outcome);
                }),
            );
            cx.spawn(async move |this, cx| {
                let outcome = rx.await.unwrap_or_else(|_| {
                    SilenceOutcome::Failed(
                        "the silence request was dropped; check Alertmanager before retrying"
                            .to_string(),
                    )
                });
                let _ = this.update(cx, |this, cx| {
                    this.state.adopt(outcome);
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }
}

impl crate::item::Item for SilenceView {
    fn title(&self) -> SharedString {
        "silence alerts".into()
    }
    fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SilenceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let endpoint = &self.state.endpoint;
        let button = match self.state.phase {
            Phase::Editing => "Review silence",
            Phase::Reviewing => "Preparing review...",
            Phase::Ready => "Confirm silence",
            Phase::Writing => "Creating silence...",
            Phase::Finished => "Finished",
        };
        div()
            .id("silence-view")
            .key_context("Typing")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(16.0))
            .overflow_y_scroll()
            .bg(rgb(theme.shell.panel_background))
            .text_color(rgb(theme.shell.text))
            .font_family(fonts.ui_family.clone())
            .text_size(px(fonts.ui_size))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| this.submit(cx)))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                this.state.cancel_review();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                let mut comment = this.state.comment.clone();
                comment.pop();
                this.state.edit(comment);
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !this.state.editable() {
                    return;
                }
                let key = &event.keystroke;
                if key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform {
                    match key.key.as_str() {
                        "u" => this.state.edit(String::new()),
                        "v" => {
                            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                                this.state.append_reason(&text);
                            }
                        }
                        _ => return,
                    }
                    cx.notify();
                    return;
                }
                if key.modifiers.control || key.modifiers.alt || key.modifiers.platform {
                    return;
                }
                if let Some(text) = &key.key_char {
                    this.state.append_reason(text);
                    cx.notify();
                }
            }))
            .child(crate::ui::panel_header(
                &theme,
                &fonts,
                format!("Silence alerts in {}", self.context),
            ))
            .child(format!(
                "Alertmanager {}/{}:{}",
                endpoint.namespace, endpoint.service, endpoint.port
            ))
            .child("All of these equality matchers must match an alert:")
            .children(self.state.matchers.iter().map(|matcher| {
                div()
                    .font_family(fonts.buffer_family.clone())
                    .child(format!("{} = {:?}", matcher.name, matcher.value))
            }))
            .child("The silence also covers new alerts with these same labels.")
            .child(
                div()
                    .id("silence-reason")
                    .role(Role::TextInput)
                    .aria_label("Silence reason")
                    .p(px(8.0))
                    .border_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .child(format!("Reason: {}", self.state.comment)),
            )
            .child("Created by: k10s")
            .children(self.state.interval.as_ref().map(|interval| {
                div().child(format!(
                    "Requested UTC interval: {} to {}",
                    interval.start, interval.end
                ))
            }))
            .child("Alertmanager starts the silence when it accepts the confirmation. The end time stays fixed.")
            .child(
                div()
                    .id("silence-submit")
                    .role(Role::Button)
                    .aria_label(button)
                    .p(px(8.0))
                    .bg(rgb(theme.shell.element_selected))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| this.submit(cx)),
                    )
                    .child(button),
            )
            .child(self.state.status.clone())
    }
}

#[cfg(test)]
#[path = "alerts_test.rs"]
mod tests;
