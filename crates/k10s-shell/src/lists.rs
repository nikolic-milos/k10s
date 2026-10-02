//! Helm, Argo, Flux, policy, Harbor, and declared mesh as native tables.
//!
//! These adapters already fetch on the data plane. This view is the same
//! [`TableState`] machine browse and nodes use, over pages those fetches
//! already produce. Helm is always a pane: a cluster without release Secrets
//! is an empty list, not absence. Values stay off this table: a key on a
//! Helm row starts a reveal. Adapters that are not served emit
//! [`InventoryEvent::NotServed`] so the workspace can take the pane down
//! rather than leave an empty broken one. A 403 stays a labelled status.
//!
//! A Helm stored-manifest apply is armed by the first `u` and sent by the
//! second, once: the [`Latch`] refuses every press while it is out, for as
//! long as this view lives, and the workspace keeps the write's record so a
//! Releases tab reopened meanwhile refuses too and shows the answer.

use std::rc::Rc;

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window, canvas, div,
    prelude::*, px, rgb,
};

use crate::confirm::{Latch, Press, Slot, StatusText, nothing_listed};
use crate::provider::{ReadProvider, TableOutcome, TablePage};
use crate::table::TableState;
use crate::tag::ItemTag;
use crate::ui::{LIST_ROW_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header, status_slot};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EnterFilter, HelmDiff, HelmRollback, OpenRow,
    Refresh, RevealHelm, RowDown, RowEnd, RowHome, RowPageDown, RowPageUp, RowUp,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryKind {
    Helm,
    Argo,
    Flux,
    Policy,
    Harbor,
    Mesh,
}

impl InventoryKind {
    pub fn tag(self) -> ItemTag {
        match self {
            InventoryKind::Helm => ItemTag::Releases,
            InventoryKind::Argo => ItemTag::Argo,
            InventoryKind::Flux => ItemTag::Flux,
            InventoryKind::Policy => ItemTag::Policy,
            InventoryKind::Harbor => ItemTag::Harbor,
            InventoryKind::Mesh => ItemTag::Mesh,
        }
    }

    fn title(self) -> &'static str {
        match self {
            InventoryKind::Helm => "helm releases",
            InventoryKind::Argo => "argo",
            InventoryKind::Flux => "flux",
            InventoryKind::Policy => "policy",
            InventoryKind::Harbor => "harbor",
            InventoryKind::Mesh => "mesh",
        }
    }

    fn empty_hint(self) -> &'static str {
        match self {
            InventoryKind::Helm => "no Helm releases are stored in this cluster",
            InventoryKind::Argo => "no Argo CD Applications are in this cluster",
            InventoryKind::Flux => "no Flux objects are stored in this cluster",
            InventoryKind::Policy => "no policy reports are in this cluster",
            InventoryKind::Harbor => "no Harbor projects are in this cluster",
            InventoryKind::Mesh => "no declared mesh objects are in this cluster",
        }
    }

    fn absent_what(self) -> &'static str {
        match self {
            InventoryKind::Helm => "Helm",
            InventoryKind::Argo => "Argo CD",
            InventoryKind::Flux => "Flux",
            InventoryKind::Policy => "Policy reports",
            InventoryKind::Harbor => "Harbor",
            InventoryKind::Mesh => "Mesh",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryEvent {
    RevealPod {
        namespace: String,
        pod: String,
    },
    NotServed {
        tag: ItemTag,
        what: &'static str,
    },
    RevealRelease {
        namespace: Option<String>,
        name: String,
        revision: u32,
    },
    DiffRelease {
        namespace: Option<String>,
        name: String,
        from: u32,
        to: u32,
    },
    RollbackRelease {
        namespace: Option<String>,
        name: String,
        revision: u32,
    },
}

pub struct InventoryView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    kind: InventoryKind,
    table: TableState,
    loading: bool,
    status: Option<SharedString>,
    // A listing that was denied or failed. Its own line, never hidden behind
    // a write's line and never beside "nothing here".
    fetch_label: Option<SharedString>,
    filtering: bool,
    latch: Latch<(Option<String>, String, u32)>,
    status_text: StatusText,
    generation: u64,
    viewport: Viewport,
}

impl EventEmitter<InventoryEvent> for InventoryView {}

impl InventoryView {
    pub fn helm(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> InventoryView {
        InventoryView::open(InventoryKind::Helm, provider, cx)
    }

    pub fn argo(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> InventoryView {
        InventoryView::open(InventoryKind::Argo, provider, cx)
    }

    pub fn flux(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> InventoryView {
        InventoryView::open(InventoryKind::Flux, provider, cx)
    }

    pub fn policy(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> InventoryView {
        InventoryView::open(InventoryKind::Policy, provider, cx)
    }

    pub fn harbor(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> InventoryView {
        InventoryView::open(InventoryKind::Harbor, provider, cx)
    }

    pub fn mesh(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> InventoryView {
        InventoryView::open(InventoryKind::Mesh, provider, cx)
    }

    fn open(
        kind: InventoryKind,
        provider: Rc<dyn ReadProvider>,
        cx: &mut Context<Self>,
    ) -> InventoryView {
        let mut view = InventoryView {
            focus: cx.focus_handle(),
            provider,
            kind,
            table: TableState::new(),
            loading: true,
            status: None,
            fetch_label: None,
            filtering: false,
            latch: Latch::Idle,
            status_text: StatusText::default(),
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
        self.kind.title().into()
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.status = None;
        self.fetch_label = None;
        self.latch.disarm();
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        match self.kind {
            InventoryKind::Helm => self.provider.fetch_releases(reply),
            InventoryKind::Argo => self.provider.fetch_argo(reply),
            InventoryKind::Flux => self.provider.fetch_flux(reply),
            InventoryKind::Policy => self.provider.fetch_policy(reply),
            InventoryKind::Harbor => self.provider.fetch_harbor(reply),
            InventoryKind::Mesh => self.provider.fetch_mesh(reply),
        }
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.loading = false;
                    match outcome {
                        TableOutcome::Table(page) => this.table.set_page(page),
                        TableOutcome::Absent => {
                            this.table.set_page(TablePage::default());
                            cx.emit(InventoryEvent::NotServed {
                                tag: this.kind.tag(),
                                what: this.kind.absent_what(),
                            });
                        }
                        TableOutcome::Denied(what) => {
                            this.table.set_page(TablePage::default());
                            this.fetch_label =
                                Some(format!("{what}: access denied for this account").into());
                        }
                        TableOutcome::Failed(why) => {
                            this.table.set_page(TablePage::default());
                            this.fetch_label = Some(why.into());
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn breadcrumb(&self) -> String {
        let mut crumb = format!("{}: {} rows", self.kind.title(), self.table.visible_rows());
        if self.loading {
            crumb.push_str("  loading...");
        }
        if self.table.truncated() {
            crumb.push_str("  (the listing stopped at its ceiling)");
        }
        if self.filtering {
            crumb.push_str(&format!("  filter: {}_", self.table.filter));
        } else if !self.table.filter.is_empty() {
            crumb.push_str(&format!("  filter: {}", self.table.filter));
        }
        crumb
    }

    fn selected_release(&self) -> Option<(Option<String>, String, u32)> {
        if self.kind != InventoryKind::Helm {
            return None;
        }
        let revision = self.table.selected_cell("Revision")?.parse().ok()?;
        let row = self.table.selected_row()?;
        Some((row.namespace.clone(), row.name.clone(), revision))
    }

    fn emit_reveal(&mut self, cx: &mut Context<Self>) {
        let Some((namespace, name, revision)) = self.selected_release() else {
            return;
        };
        self.latch.disarm();
        cx.emit(InventoryEvent::RevealRelease {
            namespace,
            name,
            revision,
        });
    }

    fn emit_diff(&mut self, cx: &mut Context<Self>) {
        let Some((namespace, name, revision)) = self.selected_release() else {
            return;
        };
        self.latch.disarm();
        if revision <= 1 {
            self.status =
                Some(format!("{name} revision {revision} has no previous revision to diff").into());
            cx.notify();
            return;
        }
        cx.emit(InventoryEvent::DiffRelease {
            namespace,
            name,
            from: revision - 1,
            to: revision,
        });
    }

    // Nothing to ask the server first, so the first `u` arms and forms both
    // lines, and the second sends the stored triple once.
    fn emit_rollback(&mut self, cx: &mut Context<Self>) {
        let Some((namespace, name, revision)) = self.selected_release() else {
            return;
        };
        let subject = rollback_subject(namespace.as_deref(), &name, revision);
        match self.latch.press(&subject) {
            Press::Ask => {
                let (armed, sent) = rollback_lines(&name, revision);
                self.status = None;
                self.latch
                    .arm(subject, (namespace, name, revision), armed, sent);
            }
            Press::Go((namespace, name, revision)) => {
                self.status = None;
                cx.emit(InventoryEvent::RollbackRelease {
                    namespace,
                    name,
                    revision,
                });
            }
            Press::Refused => {}
        }
        cx.notify();
    }

    fn selected_subject(&self) -> Option<String> {
        let (namespace, name, revision) = self.selected_release()?;
        Some(rollback_subject(namespace.as_deref(), &name, revision))
    }

    /// The stored-manifest apply this view sent or resumed has its answer.
    pub(crate) fn rollback_answered(&mut self, answer: SharedString, cx: &mut Context<Self>) {
        self.latch.settle(self.selected_subject().as_deref());
        self.status = Some(answer);
        cx.notify();
    }

    /// Opened while a stored-manifest apply is out: refuse presses and show
    /// its line until its answer lands.
    pub(crate) fn resume_rollback(&mut self, namespace: Option<&str>, name: &str, revision: u32) {
        let (_, sent) = rollback_lines(name, revision);
        self.latch
            .resume(rollback_subject(namespace, name, revision), sent);
    }

    fn hint(&self) -> &'static str {
        match self.kind {
            InventoryKind::Helm => "v reveal · shift-v diff · u rollback · / filter · r refresh",
            _ => "/ filter · r refresh · esc close filter",
        }
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

/// Which stored-manifest apply a press is about, in the latch's terms.
fn rollback_subject(namespace: Option<&str>, name: &str, revision: u32) -> String {
    format!("{}/{name}/{revision}", namespace.unwrap_or(""))
}

/// The armed and sent lines of a stored-manifest apply, formed once.
fn rollback_lines(name: &str, revision: u32) -> (SharedString, SharedString) {
    (
        format!(
            "u again applies {name} revision {revision}: not helm rollback (hooks will not run)"
        )
        .into(),
        format!("applying {name} revision {revision}: sent, waiting for the server").into(),
    )
}

impl crate::item::Item for InventoryView {
    fn title(&self) -> SharedString {
        InventoryView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        InventoryView::focus_handle(self)
    }
}

impl Render for InventoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let empty = !self.loading
            && nothing_listed(
                self.table.total_rows(),
                self.status.as_ref(),
                self.fetch_label.as_ref(),
            );
        let slot = status_slot(
            &theme,
            &fonts,
            self.status_text.text(Slot::of(
                &self.latch,
                self.status.as_ref(),
                self.fetch_label.as_ref(),
            )),
        );

        div()
            .id("inventory-view")
            .key_context(if self.filtering {
                "Typing"
            } else if self.kind == InventoryKind::Helm {
                // Browse plus the release commands: only the Helm inventory
                // listens for v/u/shift-v, so only it offers them.
                "Browse Releases"
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
                this.emit_reveal(cx);
            }))
            .on_action(cx.listener(|this, _: &RevealHelm, _, cx| {
                this.emit_reveal(cx);
            }))
            .on_action(cx.listener(|this, _: &HelmDiff, _, cx| {
                this.emit_diff(cx);
            }))
            .on_action(cx.listener(|this, _: &HelmRollback, _, cx| {
                this.emit_rollback(cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.fetch(cx);
                cx.notify();
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
                            .id("inventory-rows")
                            .flex_1()
                            .min_h(px(LIST_ROW_HEIGHT))
                            .overflow_hidden()
                            .relative()
                            .flex()
                            .flex_col()
                            .role(Role::ListBox)
                            .aria_label(self.kind.title())
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
                                        .id(("inventory-row", offset))
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
                            .children(empty.then(|| {
                                div()
                                    .p(px(12.0))
                                    .text_size(px(fonts.ui_size))
                                    .text_color(rgb(theme.shell.text_muted))
                                    .child(self.kind.empty_hint())
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
                    .child(self.hint()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_manifest_apply_is_asked_and_sent_in_these_words() {
        assert_eq!(rollback_subject(Some("prod"), "web", 3), "prod/web/3");
        assert_eq!(rollback_subject(None, "web", 3), "/web/3");
        let (armed, sent) = rollback_lines("web", 3);
        assert_eq!(
            armed.as_ref(),
            "u again applies web revision 3: not helm rollback (hooks will not run)"
        );
        assert_eq!(
            sent.as_ref(),
            "applying web revision 3: sent, waiting for the server"
        );
    }
}
