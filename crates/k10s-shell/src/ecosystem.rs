//! Every ecosystem family the data plane can list, in one pane.
//!
//! The left column is the families this cluster actually serves — a family
//! whose adapter answered "not installed" is not a row here, so the pane
//! shows what exists rather than a wall of empty sections. Selecting a
//! family shows its table through the same [`TableState`] machine the other
//! panes use. A denied or failed family keeps its row with a labelled
//! status: one broken adapter never hides the other fifteen. When nothing
//! is served at all the pane emits [`InventoryEvent::NotServed`] and the
//! workspace takes it down.

use std::rc::Rc;

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window, canvas, div,
    prelude::*, px, rgb, svg,
};

use crate::lists::InventoryEvent;
use crate::provider::{EcosystemEntry, ReadProvider, TableOutcome, TablePage};
use crate::table::TableState;
use crate::tag::ItemTag;
use crate::ui::{LIST_ROW_HEIGHT, PANEL_HEADER_HEIGHT, STATUS_BAR_HEIGHT, Viewport, panel_header};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EnterFilter, NextFamily, PrevFamily, Refresh,
    RowDown, RowEnd, RowHome, RowPageDown, RowPageUp, RowUp,
};

const TABLE_HEADER_HEIGHT: f32 = 28.0;
const VIEW_CHROME_HEIGHT: f32 = PANEL_HEADER_HEIGHT + TABLE_HEADER_HEIGHT + STATUS_BAR_HEIGHT;
const FAMILY_COLUMN_WIDTH: f32 = 196.0;
const FAMILY_ROW_HEIGHT: f32 = 28.0;

/// Presentation for one family id the data plane can answer with. The map's
/// monochrome brand masks carry recognition where a brand exists; families
/// without one use the window's own chrome set.
struct Family {
    id: &'static str,
    title: &'static str,
    glyph: &'static str,
}

const FAMILIES: &[Family] = &[
    Family {
        id: "cilium",
        title: "Cilium",
        glyph: "icons/tools/cilium.svg",
    },
    Family {
        id: "cilium-control",
        title: "Cilium control",
        glyph: "icons/tools/cilium.svg",
    },
    Family {
        id: "tetragon",
        title: "Tetragon",
        glyph: "icons/tools/tetragon.svg",
    },
    Family {
        id: "falco",
        title: "Falco",
        glyph: "icons/tools/falco.svg",
    },
    Family {
        id: "traefik",
        title: "Traefik",
        glyph: "icons/tools/traefikproxy.svg",
    },
    Family {
        id: "gateway",
        title: "Gateway API",
        glyph: "icons/tools/kubernetes.svg",
    },
    Family {
        id: "ingress",
        title: "Ingress",
        glyph: "icons/tools/kubernetes.svg",
    },
    Family {
        id: "proxies",
        title: "Proxies",
        glyph: "icons/tools/envoyproxy.svg",
    },
    Family {
        id: "kyverno",
        title: "Kyverno",
        glyph: "icons/tools/kyverno.svg",
    },
    Family {
        id: "eso",
        title: "External Secrets",
        glyph: "icons/lock.svg",
    },
    Family {
        id: "vault",
        title: "Vault / OpenBao",
        glyph: "icons/tools/vault.svg",
    },
    Family {
        id: "velero",
        title: "Velero",
        glyph: "icons/tools/velero.svg",
    },
    Family {
        id: "cnpg",
        title: "CloudNativePG",
        glyph: "icons/tools/postgresql.svg",
    },
    Family {
        id: "kargo",
        title: "Kargo",
        glyph: "icons/tools/kargo.svg",
    },
    Family {
        id: "otel",
        title: "OpenTelemetry",
        glyph: "icons/tools/opentelemetry.svg",
    },
    Family {
        id: "alertmanager",
        title: "Alertmanager",
        glyph: "icons/tools/prometheus.svg",
    },
];

struct FamilyEntry {
    meta: &'static Family,
    outcome: TableOutcome,
}

impl FamilyEntry {
    fn badge(&self) -> String {
        match &self.outcome {
            TableOutcome::Table(page) => page.rows.len().to_string(),
            TableOutcome::Absent => String::new(),
            TableOutcome::Denied(_) => "denied".to_string(),
            TableOutcome::Failed(_) => "failed".to_string(),
        }
    }
}

pub struct EcosystemView {
    focus: FocusHandle,
    provider: Rc<dyn ReadProvider>,
    families: Vec<FamilyEntry>,
    selected: usize,
    table: TableState,
    loading: bool,
    status: Option<String>,
    filtering: bool,
    generation: u64,
    viewport: Viewport,
}

impl EventEmitter<InventoryEvent> for EcosystemView {}

impl EcosystemView {
    pub fn new(provider: Rc<dyn ReadProvider>, cx: &mut Context<Self>) -> EcosystemView {
        let mut view = EcosystemView {
            focus: cx.focus_handle(),
            provider,
            families: Vec::new(),
            selected: 0,
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

    fn fetch(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.status = None;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_ecosystem(Box::new(move |entries| {
            let _ = tx.send(entries);
        }));
        cx.spawn(async move |this, cx| {
            if let Ok(entries) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.loading = false;
                    this.adopt(entries, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn adopt(&mut self, entries: Vec<EcosystemEntry>, cx: &mut Context<Self>) {
        let keep = self.families.get(self.selected).map(|entry| entry.meta.id);
        self.families = FAMILIES
            .iter()
            .filter_map(|meta| {
                let entry = entries.iter().find(|entry| entry.id == meta.id)?;
                (entry.outcome != TableOutcome::Absent).then(|| FamilyEntry {
                    meta,
                    outcome: entry.outcome.clone(),
                })
            })
            .collect();
        if self.families.is_empty() {
            self.table.set_page(TablePage::default());
            cx.emit(InventoryEvent::NotServed {
                tag: ItemTag::Ecosystem,
                what: "ecosystem adapters",
            });
            return;
        }
        self.selected = keep
            .and_then(|id| self.families.iter().position(|entry| entry.meta.id == id))
            .unwrap_or(0);
        self.apply_selected();
    }

    fn apply_selected(&mut self) {
        let Some(entry) = self.families.get(self.selected) else {
            return;
        };
        match &entry.outcome {
            TableOutcome::Table(page) => {
                self.table.set_page(page.clone());
                self.status = None;
            }
            TableOutcome::Absent => {
                self.table.set_page(TablePage::default());
                self.status = None;
            }
            TableOutcome::Denied(what) => {
                self.table.set_page(TablePage::default());
                self.status = Some(format!("{what}: access denied for this account"));
            }
            TableOutcome::Failed(why) => {
                self.table.set_page(TablePage::default());
                self.status = Some(why.clone());
            }
        }
    }

    fn select_family(&mut self, index: usize) {
        if index >= self.families.len() || index == self.selected {
            return;
        }
        self.selected = index;
        self.table.clear_filter();
        self.filtering = false;
        self.apply_selected();
    }

    fn step_family(&mut self, delta: isize) {
        if self.families.is_empty() {
            return;
        }
        let len = self.families.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len) as usize;
        self.select_family(next);
    }

    fn breadcrumb(&self) -> String {
        let title = self
            .families
            .get(self.selected)
            .map(|entry| entry.meta.title)
            .unwrap_or("ecosystem");
        let mut crumb = format!("ecosystem: {title}, {} rows", self.table.visible_rows());
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

    fn family_column(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        div()
            .id("ecosystem-families")
            .w(px(FAMILY_COLUMN_WIDTH))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .py(px(4.0))
            .border_r_1()
            .border_color(rgb(theme.shell.border_variant))
            .role(Role::ListBox)
            .aria_label("Ecosystem families")
            .children(self.families.iter().enumerate().map(|(index, entry)| {
                let selected = index == self.selected;
                let mut row = div()
                    .id(("ecosystem-family", index))
                    .h(px(FAMILY_ROW_HEIGHT))
                    .flex_none()
                    .mx(px(4.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .role(Role::ListBoxOption)
                    .aria_label(entry.meta.title)
                    .aria_selected(selected)
                    .hover(|style| style.bg(rgb(theme.shell.element_hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.select_family(index);
                            cx.notify();
                        }),
                    );
                if selected {
                    row = row.bg(rgb(theme.shell.element_selected));
                }
                row.child(
                    svg()
                        .path(entry.meta.glyph)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(rgb(if selected {
                            theme.shell.text
                        } else {
                            theme.shell.text_muted
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(fonts.ui_size))
                        .text_color(rgb(theme.shell.text))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(entry.meta.title),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(fonts.small()))
                        .text_color(rgb(theme.shell.text_muted))
                        .child(entry.badge()),
                )
            }))
    }
}

impl crate::item::Item for EcosystemView {
    fn title(&self) -> SharedString {
        "ecosystem".into()
    }

    fn focus_handle(&self) -> FocusHandle {
        EcosystemView::focus_handle(self)
    }
}

impl Render for EcosystemView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let empty = self.table.total_rows() == 0 && self.status.is_none() && !self.loading;

        div()
            .id("ecosystem-view")
            .key_context(if self.filtering {
                "Typing"
            } else {
                "Ecosystem"
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
            .on_action(cx.listener(|this, _: &NextFamily, _, cx| {
                this.step_family(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrevFamily, _, cx| {
                this.step_family(-1);
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
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .overflow_hidden()
                    .child(self.family_column(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(TABLE_HEADER_HEIGHT))
                                    .flex_none()
                                    .px(px(12.0))
                                    .flex()
                                    .items_center()
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
                                    .id("ecosystem-rows")
                                    .flex_1()
                                    .min_h(px(0.0))
                                    .overflow_hidden()
                                    .flex()
                                    .flex_col()
                                    .role(Role::ListBox)
                                    .aria_label("Ecosystem objects")
                                    .children(
                                        self.table.visible_lines().into_iter().enumerate().map(
                                            |(offset, (selected, line))| {
                                                let mut row = div()
                                                    .id(("ecosystem-row", offset))
                                                    .h(px(LIST_ROW_HEIGHT))
                                                    .flex_none()
                                                    .px(px(12.0))
                                                    .flex()
                                                    .items_center()
                                                    .font_family(fonts.buffer_family.clone())
                                                    .text_size(px(fonts.small()))
                                                    .text_color(rgb(
                                                        theme.shell.editor_foreground,
                                                    ))
                                                    .whitespace_nowrap()
                                                    .overflow_hidden()
                                                    .cursor_pointer()
                                                    .role(Role::ListBoxOption)
                                                    .aria_label(line.clone())
                                                    .aria_selected(selected)
                                                    .hover(|style| {
                                                        style.bg(rgb(theme.shell.element_hover))
                                                    })
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(
                                                            move |this,
                                                                  _: &MouseDownEvent,
                                                                  _,
                                                                  cx| {
                                                                this.table
                                                                    .select_visible_offset(offset);
                                                                cx.notify();
                                                            },
                                                        ),
                                                    );
                                                if selected {
                                                    row = row.bg(rgb(theme.shell.element_selected));
                                                }
                                                row.child(line)
                                            },
                                        ),
                                    )
                                    .children(empty.then(|| {
                                        div()
                                            .p(px(12.0))
                                            .text_size(px(fonts.ui_size))
                                            .text_color(rgb(theme.shell.text_muted))
                                            .child("this family has no objects stored right now")
                                    }))
                                    .children(self.status.clone().map(|status| {
                                        div()
                                            .p(px(12.0))
                                            .text_size(px(fonts.ui_size))
                                            .text_color(rgb(theme.shell.text))
                                            .child(status)
                                    })),
                            ),
                    ),
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
                    .child("tab family · / filter · r refresh"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_id_the_data_plane_answers_has_a_presentation_row() {
        let mut ids: Vec<&str> = Vec::new();
        for family in FAMILIES {
            assert!(!ids.contains(&family.id), "duplicate family {}", family.id);
            assert!(!family.title.is_empty());
            assert!(
                family.glyph.starts_with("icons/"),
                "glyphs resolve through the asset chain: {}",
                family.glyph
            );
            ids.push(family.id);
        }
        assert_eq!(ids.len(), 16, "one row per data-plane family");
    }
}
