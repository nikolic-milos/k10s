//! Assembling the window, and the one element every command is listened for on.
//!
//! Two things are deliberate here. The action listeners all hang off the
//! workspace element rather than off the map: an `.on_action` costs allocations
//! on every paint, and the map's element build sits inside the full-GPUI-paint
//! allocation ratchet, so the map keeps only its `key_context` and the commands
//! are heard here. And the whole of the chrome is skipped under `bench` -- a
//! measurement window is the map and nothing else, so a flight is timing the
//! thing it claims to be timing.

use gpui::{
    Context, DragMoveEvent, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, Role,
    SharedString, StyleRefinement, Styled, Window, canvas, div, prelude::*, px, rgb,
};

use crate::chrome::{DockEdge, DraggedDockResize};
use crate::finder::PickerMode;
use crate::hosting::ConfigFile;
use crate::selection::{LogTarget, Selection};
use crate::tag::ItemTag;
use crate::ui::{ACTIVITY_BAR_WIDTH, DockSizes, MODAL_TOP, STATUS_BAR_HEIGHT, key_hint};
use crate::workspace::{PickerPurpose, Workspace};
use crate::{
    AttachSelection, ChooseCluster, ClearSelection, CloseItem, DescribeSelection, EditSelection,
    ExecSelection, FindCluster, FindFile, LoadSavedView, LogsSelection, NewFile, NextItem,
    OpenArgo, OpenBrowser, OpenDay2, OpenFile, OpenFlux, OpenFolder, OpenForwards, OpenKeymap,
    OpenNodes, OpenPalette, OpenReleases, OpenSettings, PrevItem, Quit, ShowStarmap,
    ToggleBottomDock, ToggleInspector, ToggleLeftDock, ToggleRightDock, ToggleTerminal,
};

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let requested = self.requested_dock_sizes(cx);
        let workspace = cx.entity();
        let mut layout_viewport = self.viewport;
        if !self.bench {
            layout_viewport.width = (layout_viewport.width - ACTIVITY_BAR_WIDTH).max(0.0);
        }
        let sizes = DockSizes::resolve(
            layout_viewport,
            requested,
            self.left.is_open(),
            self.inspector_open,
            self.bottom.is_open(),
        );
        let map_active = self
            .center
            .active()
            .is_none_or(|tab| tab.tag == ItemTag::Map);
        let map_chrome = (!self.bench && map_active).then(|| {
            self.map.read(cx).chrome_view().cached(
                StyleRefinement::default()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left_0(),
            )
        });
        let content: gpui::AnyElement = self
            .center
            .active()
            .map(Self::item_view)
            .unwrap_or_else(|| self.map.clone().into_any_element());
        let left = (!self.bench && self.left.is_open()).then(|| {
            div()
                .id("left-dock")
                .w(px(sizes.left))
                .h_full()
                .relative()
                .flex_none()
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(rgb(theme.shell.panel_background))
                .border_r_1()
                .border_color(rgb(theme.shell.border))
                .role(Role::Complementary)
                .aria_label("Left dock")
                .children(self.dock_tabs(
                    &self.left,
                    "left-dock-tabs",
                    Self::activate_left,
                    &theme,
                    &fonts,
                    cx,
                ))
                .children(
                    self.left
                        .active()
                        .map(|tab| div().flex_1().min_h(px(0.0)).child(Self::item_view(tab))),
                )
                .child(Self::resize_handle(DockEdge::Left))
        });
        let bottom = (!self.bench && self.bottom.is_open()).then(|| {
            div()
                .id("bottom-dock")
                .h(px(sizes.bottom))
                .w_full()
                .relative()
                .flex_none()
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(rgb(theme.shell.panel_background))
                .border_t_1()
                .border_color(rgb(theme.shell.border))
                .role(Role::Complementary)
                .aria_label("Bottom dock")
                .children(self.dock_tabs(
                    &self.bottom,
                    "bottom-dock-tabs",
                    Self::activate_bottom,
                    &theme,
                    &fonts,
                    cx,
                ))
                .children(
                    self.bottom
                        .active()
                        .map(|tab| div().flex_1().min_h(px(0.0)).child(Self::item_view(tab))),
                )
                .child(Self::resize_handle(DockEdge::Bottom))
        });
        let right = (!self.bench && self.inspector_open)
            .then(|| self.inspector(&theme, &fonts, sizes.right, self.selection.clone()));
        let status = (!self.bench).then(|| {
            div()
                .id("status-bar")
                .h(px(STATUS_BAR_HEIGHT))
                .w_full()
                .flex_none()
                .px(px(8.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .bg(rgb(theme.shell.status_bar_background))
                .border_t_1()
                .border_color(rgb(theme.shell.border))
                .text_size(px(fonts.small()))
                .text_color(rgb(theme.shell.text_muted))
                .role(Role::Toolbar)
                .aria_label("Status bar")
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(SharedString::from(self.status_line())),
                )
                .child(key_hint(&theme, &fonts, "Ctrl Shift P", "Commands"))
        });
        let viewport_observer = (!self.bench).then(|| {
            canvas(
                move |bounds, _, cx| {
                    let _ = workspace.update(cx, |this, cx| {
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
            .size_full()
        });
        let center = div()
            .h_full()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .children((!self.bench).then(|| self.tab_bar(&theme, &fonts, cx)))
            .child(
                div()
                    .id("workspace-content")
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .role(Role::Main)
                    .aria_label("Workspace")
                    .child(content)
                    .children(map_chrome),
            )
            .children(bottom);
        let title_bar = (!self.bench).then(|| self.title_bar(&theme, &fonts, window, cx));
        let app_menu = (self.app_menu_open && !self.bench)
            .then(|| self.app_menu(&theme, &fonts, window, cx).into_any_element());
        let palette = self.palette.view().map(|view| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.close_palette(window, cx);
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(MODAL_TOP))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(view.clone()),
                )
        });
        // The picker and the finder share the palette's scrim, dismissal,
        // and placement: three modals, one chrome.
        let picker = self.picker.view().map(|view| {
            Self::modal_scrim(
                view.clone().into_any_element(),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_picker(window, cx);
                }),
            )
        });
        let finder = self.finder.view().map(|view| {
            Self::modal_scrim(
                view.clone().into_any_element(),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_finder(window, cx);
                }),
            )
        });
        let cluster_finder = self.cluster_finder.view().map(|view| {
            Self::modal_scrim(
                view.clone().into_any_element(),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_cluster_finder(window, cx);
                }),
            )
        });
        // The chooser stands down while it is asking for a file: two sheets at
        // the same place is one sheet with a lid on it. It is only unpainted, not
        // closed, so dismissing the picker brings back the list and the highlight
        // exactly as they were.
        let launch = self
            .launch
            .view()
            .filter(|_| !self.picker.is_open())
            .map(|view| {
                Self::modal_scrim(
                    view.clone().into_any_element(),
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.dismiss_launch(window, cx);
                    }),
                )
            });
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme.shell.background))
            .font_family(fonts.ui_family.clone())
            .text_size(px(fonts.ui_size))
            .text_color(rgb(theme.shell.text))
            .key_context("Workspace")
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedDockResize>, _, cx| {
                    this.resize_dock(event, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.toggle_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ChooseCluster, window, cx| {
                this.toggle_launch(window, cx);
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ToggleChurn, _, cx| {
                this.map.update(cx, |map, cx| map.toggle_churn(cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ToggleEdges, _, cx| {
                this.map.update(cx, |map, cx| map.toggle_edges(cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ToggleHud, _, cx| {
                this.map.update(cx, |map, cx| map.toggle_hud(cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ToggleLegend, _, cx| {
                this.map.update(cx, |map, cx| map.toggle_legend(cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::CycleOverlay, _, cx| {
                this.cycle_overlay(cx);
            }))
            .on_action(cx.listener(|this, _: &k10s_map::FitView, window, cx| {
                this.map.update(cx, |map, cx| map.fit(window, cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ZoomIn, window, cx| {
                this.map.update(cx, |map, cx| map.zoom_in(window, cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ZoomOut, window, cx| {
                this.map.update(cx, |map, cx| map.zoom_out(window, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| {
                this.toggle_inspector(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleLeftDock, _, cx| {
                this.left.toggle();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleRightDock, _, cx| {
                this.toggle_inspector(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleBottomDock, _, cx| {
                this.bottom.toggle();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                if this.app_menu_open {
                    this.app_menu_open = false;
                    cx.notify();
                    return;
                }
                if this.map.update(cx, |map, cx| map.cancel_flight(cx)) {
                    return;
                }
                if this.selection.take().is_some() {
                    this.inspector_open = false;
                    this.refresh_detail(cx);
                    this.map.update(cx, |map, cx| map.set_selection(None, cx));
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
            .on_action(cx.listener(|this, _: &ShowStarmap, window, cx| {
                this.activate_activity(crate::activity::ActivityId::Starmap, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenBrowser, window, cx| {
                this.activate_activity(crate::activity::ActivityId::Resources, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenNodes, window, cx| {
                this.activate_activity(crate::activity::ActivityId::Nodes, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenForwards, window, cx| {
                this.activate_activity(crate::activity::ActivityId::Forwards, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenReleases, window, cx| {
                this.activate_activity(crate::activity::ActivityId::Releases, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenArgo, window, cx| {
                this.activate_activity(crate::activity::ActivityId::Argo, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenFlux, window, cx| {
                this.activate_activity(crate::activity::ActivityId::Flux, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenDay2, window, cx| {
                this.activate_activity(crate::activity::ActivityId::Day2, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                this.toggle_terminal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DescribeSelection, window, cx| {
                if let Some(selection) = this.selection.clone() {
                    this.open_doc(selection.describe_request(), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &EditSelection, window, cx| {
                if let Some(selection) = this.selection.clone() {
                    this.open_editor(selection.describe_request(), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| {
                this.status_note = None;
                let seed = crate::workspace::seed_dir(this.files_root.as_deref());
                this.open_picker(seed, PickerMode::OpenFile, PickerPurpose::Open, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenFolder, window, cx| {
                this.status_note = None;
                let seed = crate::workspace::seed_dir(this.files_root.as_deref());
                this.open_picker(
                    seed,
                    PickerMode::OpenFolder,
                    PickerPurpose::Open,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &FindFile, window, cx| {
                this.open_finder(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FindCluster, window, cx| {
                this.open_cluster_finder(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewFile, window, cx| {
                this.status_note = None;
                this.new_file(window, cx);
            }))
            .on_action(cx.listener(|this, _: &LoadSavedView, window, cx| {
                this.open_saved_view_picker(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.open_config(ConfigFile::Settings, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenKeymap, window, cx| {
                this.open_config(ConfigFile::Keymap, window, cx);
            }))
            .on_action(cx.listener(|this, _: &LogsSelection, window, cx| {
                match this.selection.as_ref().and_then(Selection::log_target) {
                    Some(LogTarget::Pod { namespace, name }) => {
                        this.open_logs(namespace, name, window, cx)
                    }
                    // A workload's logs are the merged follows of its pods;
                    // a kind without a pod selector answers with a labelled
                    // failure from the data plane, not a guess here.
                    Some(LogTarget::Workload(request)) => {
                        this.open_workload_logs(request, window, cx)
                    }
                    None => {}
                }
            }))
            .on_action(cx.listener(|this, _: &ExecSelection, window, cx| {
                if let Some((namespace, name)) = this.selection.as_ref().and_then(Selection::pod) {
                    this.open_terminal(namespace, name, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &AttachSelection, window, cx| {
                if let Some((namespace, name)) = this.selection.as_ref().and_then(Selection::pod) {
                    this.open_attach(namespace, name, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextItem, window, cx| {
                if let Some(next) = this.center.next_index() {
                    this.activate_center(next, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PrevItem, window, cx| {
                if let Some(previous) = this.center.previous_index() {
                    this.activate_center(previous, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseItem, window, cx| {
                this.close_focused(window, cx);
            }))
            .children(viewport_observer)
            .children(title_bar)
            .child({
                let rail = (!self.bench).then(|| self.activity_rail(&theme).into_any_element());
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .children(rail)
                    .children(left)
                    .child(center)
                    .children(right)
            })
            .children(status)
            .children(app_menu)
            .children(palette)
            .children(launch)
            .children(picker)
            .children(finder)
            .children(cluster_finder)
    }
}

impl Workspace {
    // The right dock *is* the inspector, and two actions open it: the `i`
    // mnemonic and the dock chord. One place to flip it, so a rule added to one
    // of them cannot go missing from the other -- a closed inspector must not
    // keep a usage poll alive under it.
    pub(crate) fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector_open = !self.inspector_open;
        self.sync_usage_poll(cx);
        cx.notify();
    }
}
