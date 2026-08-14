//! Helm, Argo, and Flux as native tables.
//!
//! These adapters already fetch on the data plane. This view is the same
//! [`TableState`] machine browse and nodes use, over pages those fetches
//! already produce. Helm is always a pane: a cluster without release Secrets
//! is an empty list, not absence. Argo and Flux that are not served emit
//! [`InventoryEvent::NotServed`] so the workspace can take the pane down
//! rather than leave an empty broken one. A 403 stays a labelled status.

use std::rc::Rc;

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window, canvas, div,
    prelude::*, px, rgb,
};

use crate::provider::{ReadProvider, TableOutcome, TablePage};
use crate::table::TableState;
use crate::tag::ItemTag;
use crate::ui::{LIST_ROW_HEIGHT, PANEL_HEADER_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EnterFilter, Refresh, RowDown, RowEnd, RowHome,
    RowPageDown, RowPageUp, RowUp,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;
const VIEW_CHROME_HEIGHT: f32 = PANEL_HEADER_HEIGHT + TABLE_HEADER_HEIGHT + STATUS_BAR_HEIGHT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryKind {
    Helm,
    Argo,
    Flux,
}

impl InventoryKind {
    pub fn tag(self) -> ItemTag {
        match self {
            InventoryKind::Helm => ItemTag::Releases,
            InventoryKind::Argo => ItemTag::Argo,
            InventoryKind::Flux => ItemTag::Flux,
        }
    }

    fn title(self) -> &'static str {
        match self {
            InventoryKind::Helm => "helm releases",
            InventoryKind::Argo => "argo",
            InventoryKind::Flux => "flux",
        }
    }

    fn empty_hint(self) -> &'static str {
        match self {
            InventoryKind::Helm => "no Helm releases are stored in this cluster",
            InventoryKind::Argo => "no Argo CD Applications are in this cluster",
            InventoryKind::Flux => "no Flux objects are stored in this cluster",
        }
    }

    fn absent_what(self) -> &'static str {
        match self {
            InventoryKind::Helm => "Helm",
            InventoryKind::Argo => "Argo CD",
            InventoryKind::Flux => "Flux",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryEvent {
    NotServed { tag: ItemTag, what: &'static str },
}

pub struct InventoryView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    kind: InventoryKind,
    table: TableState,
    loading: bool,
    status: Option<String>,
    filtering: bool,
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
        self.kind.title().into()
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.status = None;
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        match self.kind {
            InventoryKind::Helm => self.provider.fetch_releases(reply),
            InventoryKind::Argo => self.provider.fetch_argo(reply),
            InventoryKind::Flux => self.provider.fetch_flux(reply),
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
                            this.status = Some(format!("{what}: access denied for this account"));
                        }
                        TableOutcome::Failed(why) => {
                            this.table.set_page(TablePage::default());
                            this.status = Some(why);
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
        let empty = self.table.total_rows() == 0 && self.status.is_none() && !self.loading;

        div()
            .id("inventory-view")
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
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.fetch(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EnterFilter, _, cx| {
                this.filtering = true;
                cx.notify();
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
                    .id("inventory-rows")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .role(Role::ListBox)
                    .aria_label(self.kind.title())
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
                    .child("/ filter · r refresh · esc close filter"),
            )
    }
}
