//! The furniture the shell draws around whatever it is hosting.
//!
//! Title bar, application menu, status bar, the two tab strips, the inspector,
//! the modal scrim, the drag handles and the window controls -- everything the
//! window is made of that is not an item. It is all Zed's, deliberately: the
//! densities and the behaviours come from the pinned revision rather than being
//! invented a second time, which is why the geometry constants live in
//! [`crate::ui`] and not here. [`crate::render`] is what assembles these into a
//! window; this module only knows how to draw each one.

use gpui::{
    App, ClickEvent, Context, Decorations, DragMoveEvent, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Render, Role, SharedString, Styled, Window, div,
    img, prelude::*, px, rgb, svg,
};

use k10s_core::Level;
use k10s_theme::{Theme, Typography};

use crate::dock::Dock;
use crate::hosting::Tab;
use crate::provider::{Bytes, Detail, Millicores, UsageOutcome, UsageSample, UsageSource};
use crate::selection::Selection;
use crate::settings;
use crate::ui::{
    self, DockSizes, MAX_DOCK_SIZE, MIN_DOCK_SIZE, MODAL_TOP, RESIZE_HANDLE_SIZE,
    STATUS_BAR_HEIGHT, TAB_HEIGHT, TITLE_MARK_SIZE, brand_mark, icon_button, panel_header,
    title_bar_height,
};
use crate::workspace::Workspace;
use crate::{
    ChooseCluster, OpenBrowser, OpenForwards, OpenNodes, OpenPalette, OpenReleases, Quit,
    ToggleBottomDock, ToggleLeftDock, ToggleRightDock, ToggleTerminal,
};

#[derive(Clone, Copy)]
pub(crate) enum DockEdge {
    Left,
    Right,
    Bottom,
}

#[derive(Clone)]
pub(crate) struct DraggedDockResize(pub(crate) DockEdge);

impl Render for DraggedDockResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Workspace {
    pub(crate) fn row(
        theme: &Theme,
        fonts: &Typography,
        label: &'static str,
        value: impl Into<SharedString>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(label),
            )
            .child(
                div()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(fonts.ui_size))
                    .text_color(rgb(theme.shell.text))
                    .child(value.into()),
            )
    }

    pub(crate) fn inspector(
        &self,
        theme: &Theme,
        fonts: &Typography,
        width: f32,
        selection: Option<Selection>,
    ) -> impl IntoElement {
        let body = match selection {
            None => div()
                .flex_1()
                .min_h(px(0.0))
                .p(px(12.0))
                .text_size(px(fonts.ui_size))
                .text_color(rgb(theme.shell.text_muted))
                .child("Nothing selected. Select a resource on the map.")
                .into_any_element(),
            Some(selection) => {
                let mut body = div()
                    .id("inspector-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .p(px(12.0))
                    .child(
                        div()
                            .text_size(px(fonts.ui_size))
                            .text_color(rgb(theme.shell.text))
                            .child(format!("{} {}", selection.kind, selection.name)),
                    )
                    .child(Self::row(theme, &fonts, "Name", selection.name.to_string()))
                    .child(Self::row(theme, &fonts, "Kind", selection.kind));
                if let Some(namespace) = selection.namespace.as_deref() {
                    body = body.child(Self::row(theme, &fonts, "Namespace", namespace.to_string()));
                }
                if let Some(owner) = selection.owner.as_deref() {
                    body = body.child(Self::row(theme, &fonts, "Owner", owner.to_string()));
                }
                if !selection.uid.is_empty() {
                    body = body.child(Self::row(theme, &fonts, "UID", selection.uid.to_string()));
                }
                if selection.usage_target().is_some() {
                    body = body.child(Self::usage_section(theme, &fonts, self.usage.as_ref()));
                }
                body = body.child(Self::detail_section(
                    theme,
                    &fonts,
                    "Events",
                    self.events.as_ref(),
                    |detail| {
                        let Detail::Events(rows) = detail else {
                            return Vec::new();
                        };
                        rows.iter()
                            .map(|row| {
                                format!(
                                    "{} x{} {} — {}",
                                    row.kind, row.count, row.reason, row.message
                                )
                            })
                            .collect()
                    },
                ));
                if selection.level == Level::Cell {
                    body = body.child(Self::detail_section(
                        theme,
                        &fonts,
                        "Log tail",
                        self.log.as_ref(),
                        |detail| {
                            let Detail::Log(lines) = detail else {
                                return Vec::new();
                            };
                            lines.iter().rev().take(12).rev().cloned().collect()
                        },
                    ));
                }
                body.child(
                    div()
                        .text_size(px(fonts.small()))
                        .text_color(rgb(theme.shell.text_muted))
                        .child(if self.connected {
                            "d describe · l logs · s shell"
                        } else {
                            "Events and logs require a cluster connection."
                        }),
                )
                .into_any_element()
            }
        };

        div()
            .id("inspector")
            .w(px(width))
            .h_full()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme.shell.panel_background))
            .border_l_1()
            .border_color(rgb(theme.shell.border))
            .role(Role::Complementary)
            .aria_label("Inspector")
            .child(panel_header(theme, fonts, "Inspector"))
            .child(body)
            .child(Self::resize_handle(DockEdge::Right))
    }

    pub(crate) fn detail_section(
        theme: &Theme,
        fonts: &Typography,
        title: &'static str,
        detail: Option<&Detail>,
        rows: impl Fn(&Detail) -> Vec<String>,
    ) -> impl IntoElement {
        let mut section = div().flex().flex_col().gap(px(4.0)).child(
            div()
                .text_size(px(fonts.small()))
                .text_color(rgb(theme.shell.text_muted))
                .child(title),
        );
        section = match detail {
            None => section.child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child("Loading…"),
            ),
            Some(Detail::Denied(what)) => section.child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text))
                    .child(format!("{what}: access denied for this account")),
            ),
            Some(Detail::Failed(why)) => section.child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text))
                    .child(why.clone()),
            ),
            Some(detail) => {
                let lines = rows(detail);
                if lines.is_empty() {
                    section.child(
                        div()
                            .text_size(px(fonts.small()))
                            .text_color(rgb(theme.shell.text_muted))
                            .child("None"),
                    )
                } else {
                    section.children(lines.into_iter().map(|line| {
                        div()
                            .text_size(px(fonts.small()))
                            .line_height(px(18.0))
                            .text_color(rgb(theme.shell.text))
                            .child(line)
                    }))
                }
            }
        };
        section
    }

    // The usage panel's labelled states, in `detail_section`'s manner but
    // over its own outcome: absence renders muted like loading -- it is a
    // fact about the cluster, not an alarm -- while a denial or a failure
    // keeps the text colour a person reads first.
    pub(crate) fn usage_section(
        theme: &Theme,
        fonts: &Typography,
        usage: Option<&UsageOutcome>,
    ) -> impl IntoElement {
        let section = div().flex().flex_col().gap(px(4.0)).child(
            div()
                .text_size(px(fonts.small()))
                .text_color(rgb(theme.shell.text_muted))
                .child("Usage"),
        );
        let muted = |section: gpui::Div, text: String| {
            section.child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(text),
            )
        };
        let plain = |section: gpui::Div, text: String| {
            section.child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text))
                    .child(text),
            )
        };
        match usage {
            None => muted(section, "Loading…".to_string()),
            Some(UsageOutcome::Denied(what)) => {
                plain(section, format!("{what}: access denied for this account"))
            }
            Some(UsageOutcome::Failed(why)) => plain(section, why.clone()),
            Some(UsageOutcome::Absent(why)) => muted(section, why.clone()),
            Some(UsageOutcome::Usage(sample)) => {
                section.children(usage_lines(sample).into_iter().map(|line| {
                    div()
                        .text_size(px(fonts.small()))
                        .line_height(px(18.0))
                        .text_color(rgb(theme.shell.text))
                        .child(line)
                }))
            }
        }
    }

    pub(crate) fn status_line(&self) -> String {
        status_line(Status {
            connected: self.connected,
            context: self.context.as_deref(),
            selection: self
                .selection
                .as_ref()
                .map(|selection| (selection.kind, selection.name.as_ref())),
            folder: self.files_root.as_deref(),
            panels_below: self.bottom.len(),
            note: self.status_note.as_deref(),
        })
    }

    pub(crate) fn item_view(tab: &Tab) -> gpui::AnyElement {
        tab.view.to_any().into_any_element()
    }

    // A modal's scrim: click-outside dismisses, click-inside does not, and
    // the sheet lands where every other modal lands.
    pub(crate) fn modal_scrim(
        view: gpui::AnyElement,
        dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, dismiss)
            .child(
                div()
                    .absolute()
                    .top(px(MODAL_TOP))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(view),
            )
    }

    // Zed's dock toggle button: the panel's own icon, lit while its dock is
    // showing it, tooltip carrying the action's live keybinding, and the
    // click dispatching the same action the key would.
    pub(crate) fn panel_button<A: gpui::Action + Clone>(
        id: &'static str,
        icon: &'static str,
        label: &'static str,
        active: bool,
        action: A,
        theme: &Theme,
    ) -> impl IntoElement {
        let tooltip_action = action.clone();
        icon_button(id, icon, label, active, theme)
            .on_click(move |_, window, cx| {
                window.dispatch_action(Box::new(action.clone()), cx);
            })
            .tooltip(move |window, cx| {
                let tooltip = ui::Tooltip::with_binding(label, &tooltip_action, window);
                cx.new(move |_| tooltip).into()
            })
    }

    /// The sentence the title bar's state dot is about.
    ///
    /// Which cluster, not merely that there is one: somebody holding a prod and a
    /// staging context has to be able to answer "which of these am I about to
    /// apply to" by looking rather than by remembering, and a cluster is chosen on
    /// screen now, so the command line they started with no longer says.
    pub(crate) fn connection_label(connected: bool, context: Option<&str>) -> SharedString {
        match (connected, context) {
            (true, Some(context)) => context.to_string().into(),
            // A service account has no context name, and saying "connected" twice
            // over -- once as a dot, once as a word -- says nothing.
            (true, None) => "in-cluster".into(),
            (false, _) => "local starmap".into(),
        }
    }

    pub(crate) fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if self.viewport.update(width, height) {
            cx.notify();
        }
    }

    pub(crate) fn requested_dock_sizes(&self, cx: &App) -> DockSizes {
        self.dock_size_override.unwrap_or_else(|| {
            let settings = settings::active(cx);
            DockSizes {
                left: settings.left_dock_width,
                right: settings.right_dock_width,
                bottom: settings.bottom_dock_height,
            }
        })
    }

    pub(crate) fn resize_dock(
        &mut self,
        event: &DragMoveEvent<DraggedDockResize>,
        cx: &mut Context<Self>,
    ) {
        let sizes = dragged_dock_sizes(
            self.requested_dock_sizes(cx),
            event.drag(cx).0,
            f32::from(event.event.position.x),
            f32::from(event.event.position.y),
            self.viewport,
        );
        if self.dock_size_override != Some(sizes) {
            self.dock_size_override = Some(sizes);
            cx.notify();
        }
    }

    pub(crate) fn resize_handle(edge: DockEdge) -> gpui::AnyElement {
        let (id, handle) = match edge {
            DockEdge::Left => (
                "left-dock-resize",
                div()
                    .right_0()
                    .top_0()
                    .h_full()
                    .w(px(RESIZE_HANDLE_SIZE))
                    .cursor_col_resize(),
            ),
            DockEdge::Right => (
                "right-dock-resize",
                div()
                    .left_0()
                    .top_0()
                    .h_full()
                    .w(px(RESIZE_HANDLE_SIZE))
                    .cursor_col_resize(),
            ),
            DockEdge::Bottom => (
                "bottom-dock-resize",
                div()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h(px(RESIZE_HANDLE_SIZE))
                    .cursor_row_resize(),
            ),
        };
        handle
            .id(id)
            .absolute()
            .on_drag(DraggedDockResize(edge), |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .occlude()
            .into_any_element()
    }

    // The Linux window control strip, shown exactly when the compositor
    // hands us client-side decorations -- Zed's own bail-out rule. GNOME
    // style: 20 px circular hovers around 16 px glyphs, Zed's shipped icons.
    pub(crate) fn window_controls(theme: &Theme, window: &Window) -> Option<impl IntoElement> {
        if !matches!(window.window_decorations(), Decorations::Client { .. }) {
            return None;
        }
        let supported = window.window_controls();
        let maximize_icon = if window.is_maximized() {
            "icons/generic_restore.svg"
        } else {
            "icons/generic_maximize.svg"
        };
        let control = |id: &'static str, icon: &'static str, act: fn(&mut Window)| {
            div()
                .id(id)
                .size(px(20.0))
                .rounded_full()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .role(Role::Button)
                .aria_label(id)
                .hover(|button| button.bg(rgb(theme.shell.element_hover)))
                .child(
                    svg()
                        .path(icon)
                        .size(px(16.0))
                        .text_color(rgb(theme.shell.text_muted)),
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    act(window);
                })
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .pl(px(12.0))
                // A press on a control must not arm the window drag.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(supported.minimize.then(|| {
                    control("minimize", "icons/generic_minimize.svg", |window| {
                        window.minimize_window()
                    })
                }))
                .children(
                    supported
                        .maximize
                        .then(|| control("maximize", maximize_icon, |window| window.zoom_window())),
                )
                .child(control("close", "icons/generic_close.svg", |window| {
                    window.remove_window()
                })),
        )
    }

    pub(crate) fn title_bar(
        &self,
        theme: &Theme,
        fonts: &Typography,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active_title = self
            .center
            .active()
            .map(|tab| tab.view.title(cx))
            .unwrap_or_else(|| "Starmap".into());
        let connection = Self::connection_label(self.connected, self.context.as_deref());

        div()
            .id("title-bar")
            .h(px(title_bar_height(window)))
            .w_full()
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .px(px(6.0))
            .bg(rgb(theme.shell.background))
            .role(Role::Toolbar)
            .aria_label("Title bar")
            // Zed's drag-to-move state machine: arm on mouse down, hand the
            // window to the compositor on the first movement, disarm on
            // every other outcome. Interactive children stop propagation on
            // mouse down so a button press never starts a move.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, _| this.should_move = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.should_move = false),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.should_move = false))
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, window, _| {
                if this.should_move {
                    this.should_move = false;
                    window.start_window_move();
                }
            }))
            .on_click(|event: &ClickEvent, window, _| {
                if event.click_count() == 2 {
                    window.zoom_window();
                }
            })
            .when(window.window_controls().window_menu, |bar| {
                bar.on_mouse_down(MouseButton::Right, |event: &MouseDownEvent, window, _| {
                    window.show_window_menu(event.position);
                })
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        icon_button(
                            "app-menu",
                            "icons/menu.svg",
                            "Application menu",
                            self.app_menu_open,
                            theme,
                        )
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.app_menu_open = !this.app_menu_open;
                            cx.notify();
                        }))
                        .tooltip(|_, cx| {
                            cx.new(|_| ui::Tooltip {
                                label: "Application Menu".into(),
                                key: None,
                            })
                            .into()
                        }),
                    )
                    // The brand lockup: the symbol, then the wordmark. The
                    // symbol rather than the helm because the wheel's spokes
                    // mush at this size, and the appearance picks the artwork
                    // rather than a tint, because a tinted brand colour is an
                    // approximation of a brand colour.
                    .child(
                        img(brand_mark(theme.appearance))
                            .size(px(TITLE_MARK_SIZE))
                            .flex_none(),
                    )
                    // The one place the product says its own name, so the
                    // one place the display face belongs: League Spartan is a
                    // headline typeface and reads as noise anywhere else.
                    .child(
                        div()
                            .font_family(k10s_theme::DISPLAY_FAMILY)
                            .text_size(px(fonts.ui_size))
                            .text_color(rgb(theme.shell.text))
                            .child("k10s"),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(active_title),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    // The state dot sits with the sentence it is about. It used
                    // to sit beside the mark, where a coloured dot next to a
                    // logo is decoration; next to the name of the thing it
                    // describes it is an indicator.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().size(px(8.0)).flex_none().rounded_full().bg(rgb(
                                if self.connected {
                                    theme.shell.success
                                } else {
                                    theme.shell.text_muted
                                },
                            )))
                            .child(
                                div()
                                    .text_size(px(fonts.small()))
                                    .text_color(rgb(theme.shell.text_muted))
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(connection),
                            ),
                    )
                    .children(Self::window_controls(theme, window)),
            )
    }

    // The burger's dropdown: the workspace commands with their bindings,
    // anchored under the title bar the way Zed deploys its application menu.
    // Click-out and escape dismiss; confirming dispatches and dismisses.
    pub(crate) fn app_menu(
        &self,
        theme: &Theme,
        fonts: &Typography,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let entry = |id: usize, label: &'static str, action: Box<dyn gpui::Action>| {
            let key = window
                .bindings_for_action(action.as_ref())
                .into_iter()
                .next()
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|keystroke| keystroke.inner().to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                });
            div()
                .id(("app-menu-item", id))
                .h(px(26.0))
                .px(px(10.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(16.0))
                .cursor_pointer()
                .hover(|item| item.bg(rgb(theme.shell.element_hover)))
                .text_size(px(fonts.small()))
                .text_color(rgb(theme.shell.text))
                .role(Role::MenuItem)
                .aria_label(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.app_menu_open = false;
                    window.dispatch_action(action.boxed_clone(), cx);
                    cx.notify();
                }))
                .child(label)
                .children(key.map(|key| {
                    div()
                        .text_size(px(fonts.xsmall()))
                        .text_color(rgb(theme.shell.text_muted))
                        .child(SharedString::from(key))
                }))
        };
        let separator = || {
            div()
                .h(px(1.0))
                .my(px(4.0))
                .flex_none()
                .bg(rgb(theme.shell.border_variant))
        };

        div()
            .id("app-menu-backdrop")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.app_menu_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("app-menu")
                    .absolute()
                    .top(px(title_bar_height(window)))
                    .left(px(6.0))
                    .w(px(280.0))
                    .flex()
                    .flex_col()
                    .py(px(4.0))
                    .bg(rgb(theme.shell.elevated_surface_background))
                    .border_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .rounded(px(8.0))
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .role(Role::Menu)
                    .aria_label("Application menu")
                    .child(entry(0, "Command Palette…", Box::new(OpenPalette)))
                    .child(entry(1, "Choose Cluster…", Box::new(ChooseCluster)))
                    .child(separator())
                    .child(entry(2, "Browse Resources", Box::new(OpenBrowser)))
                    .child(entry(3, "Node Capacity", Box::new(OpenNodes)))
                    .child(entry(4, "Port Forwards", Box::new(OpenForwards)))
                    .child(entry(5, "Helm Releases", Box::new(OpenReleases)))
                    .child(entry(6, "Terminal", Box::new(ToggleTerminal)))
                    .child(separator())
                    .child(entry(7, "Toggle Left Dock", Box::new(ToggleLeftDock)))
                    .child(entry(8, "Toggle Bottom Dock", Box::new(ToggleBottomDock)))
                    .child(entry(9, "Toggle Inspector", Box::new(ToggleRightDock)))
                    .child(separator())
                    .child(entry(10, "Quit", Box::new(Quit))),
            )
    }

    // A panel with multiple items gets Zed's 32 px tab strip. Individual
    // panels still own their toolbars, just as Zed's terminal and project
    // panels do.
    pub(crate) fn dock_tabs(
        &self,
        dock: &Dock<Tab>,
        id: &'static str,
        activate: fn(&mut Self, usize, &mut Window, &mut Context<Self>),
        theme: &Theme,
        fonts: &Typography,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if dock.len() < 2 {
            return None;
        }
        let active = dock.active_index();
        Some(
            div()
                .id(id)
                .h(px(TAB_HEIGHT))
                .flex_none()
                .flex()
                .flex_row()
                .overflow_x_hidden()
                .bg(rgb(theme.shell.tab_bar_background))
                .border_b_1()
                .border_color(rgb(theme.shell.border))
                .role(Role::TabList)
                .aria_label("Dock tabs")
                .children(dock.panels().map(|(index, tab)| {
                    let selected = index == active;
                    div()
                        // A workspace can show left and bottom tab strips at
                        // once. Include the strip identity so GPUI never
                        // aliases interaction state between equal indices.
                        .id((id, index))
                        .px(px(12.0))
                        .h_full()
                        .flex_none()
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .bg(rgb(if selected {
                            theme.shell.tab_active_background
                        } else {
                            theme.shell.tab_inactive_background
                        }))
                        .border_r_1()
                        .when(!selected, |tab| tab.border_b_1())
                        .border_color(rgb(theme.shell.border))
                        .hover(|tab| tab.bg(rgb(theme.shell.element_hover)))
                        .text_size(px(fonts.ui_size))
                        .text_color(rgb(if selected {
                            theme.shell.text
                        } else {
                            theme.shell.text_muted
                        }))
                        .role(Role::Tab)
                        .aria_selected(selected)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut Self, _: &MouseDownEvent, window, cx| {
                                activate(this, index, window, cx);
                            }),
                        )
                        .child(tab.view.title(cx))
                }))
                .into_any_element(),
        )
    }

    pub(crate) fn tab_bar(
        &self,
        theme: &Theme,
        fonts: &Typography,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active = self.center.active_index();
        div()
            .id("center-tabs")
            .h(px(TAB_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .flex_row()
            .overflow_x_hidden()
            .bg(rgb(theme.shell.tab_bar_background))
            .border_b_1()
            .border_color(rgb(theme.shell.border))
            .role(Role::TabList)
            .aria_label("Center tabs")
            .children(self.center.iter().map(|(index, tab)| {
                let selected = index == active;
                div()
                    .id(("center-tab", index))
                    .px(px(12.0))
                    .h_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .bg(rgb(if selected {
                        theme.shell.tab_active_background
                    } else {
                        theme.shell.tab_inactive_background
                    }))
                    .border_r_1()
                    .when(!selected, |tab| tab.border_b_1())
                    .border_color(rgb(theme.shell.border))
                    .hover(|tab| tab.bg(rgb(theme.shell.element_hover)))
                    .text_size(px(fonts.ui_size))
                    .text_color(rgb(if selected {
                        theme.shell.text
                    } else {
                        theme.shell.text_muted
                    }))
                    .role(Role::Tab)
                    .aria_selected(selected)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut Self, _: &MouseDownEvent, window, cx| {
                            this.activate_center(index, window, cx);
                        }),
                    )
                    .child(tab.view.title(cx))
            }))
    }
}

/// Everything the status line is about, borrowed. A value rather than a method
/// on the workspace so the sentence can be checked without a window: what it
/// says when nothing is connected and nothing is selected is the state a person
/// sees most often, and it used to be reachable only by running the app.
pub(crate) struct Status<'a> {
    pub connected: bool,
    pub context: Option<&'a str>,
    pub selection: Option<(&'a str, &'a str)>,
    pub folder: Option<&'a std::path::Path>,
    pub panels_below: usize,
    pub note: Option<&'a str>,
}

pub(crate) fn status_line(status: Status<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(match (status.connected, status.context) {
        (true, Some(context)) => format!("connected to {context}"),
        (true, None) => "connected in-cluster".to_string(),
        (false, _) => "no cluster".to_string(),
    });
    if let Some((kind, name)) = status.selection {
        parts.push(format!("{kind} {name}"));
    }
    if let Some(root) = status.folder {
        parts.push(format!("folder {}", root.display()));
    }
    let open = status.panels_below;
    if open > 0 {
        parts.push(format!(
            "{open} panel{} below",
            if open == 1 { "" } else { "s" }
        ));
    }
    if let Some(note) = status.note {
        parts.push(note.to_string());
    }
    parts.join("  ·  ")
}

/// The usage sample as the lines the inspector shows, pure so the sentences
/// can be checked without a window. Percentages are computed here, at display
/// time, from the two typed values -- nothing upstream stores one -- and every
/// absence keeps its meaning: an unmeasured value says "sampling", a missing
/// request says nothing, and a missing limit says "no limit" rather than a
/// number that was never set.
pub(crate) fn usage_lines(sample: &UsageSample) -> Vec<String> {
    let cpu = |value: Millicores| (value.0, value.to_string());
    let memory = |value: Bytes| (value.0, value.to_string());
    let mut lines = vec![
        gauge(
            "CPU",
            sample.cpu.map(cpu),
            sample.cpu_request.map(cpu),
            sample.cpu_limit.map(cpu),
        ),
        gauge(
            "Memory",
            sample.memory.map(memory),
            sample.memory_request.map(memory),
            sample.memory_limit.map(memory),
        ),
    ];
    // A single fully-measured pod needs no coverage sentence; everything else
    // must say how much of the target the numbers describe.
    if sample.pods_total != 1 || sample.pods_measured != sample.pods_total {
        let mut coverage = format!(
            "{} of {} pods measured",
            sample.pods_measured, sample.pods_total
        );
        if sample.truncated {
            coverage.push_str("; more match than are polled");
        }
        lines.push(coverage);
    }
    if sample.source == UsageSource::Kubelet {
        lines.push("via the kubelet; metrics-server is not installed".to_string());
    }
    lines
}

// One resource's sentence: what is used, against what was asked for and what
// is allowed. Values arrive as (raw, rendered) pairs so the percentage and
// the text always describe the same number.
fn gauge(
    label: &str,
    used: Option<(u64, String)>,
    request: Option<(u64, String)>,
    limit: Option<(u64, String)>,
) -> String {
    let mut parts = vec![match &used {
        Some((_, text)) => format!("{label} {text}"),
        None => format!("{label} sampling…"),
    }];
    if let Some((base, text)) = &request {
        parts.push(format!(
            "request {text}{}",
            percent(used.as_ref().map(|(value, _)| *value), *base)
        ));
    }
    match &limit {
        Some((base, text)) => parts.push(format!(
            "limit {text}{}",
            percent(used.as_ref().map(|(value, _)| *value), *base)
        )),
        None => parts.push("no limit".to_string()),
    }
    parts.join(" · ")
}

// Rendered with the trailing space built in so an unmeasured or zero base
// simply contributes nothing to the sentence.
fn percent(used: Option<u64>, base: u64) -> String {
    match used {
        Some(value) if base > 0 => {
            format!(
                " ({}%)",
                (value as u128 * 100 + base as u128 / 2) / base as u128
            )
        }
        _ => String::new(),
    }
}

/// Where a dock edge lands when it is dragged to a point. Pure, because the
/// clamps are the whole of it: a drag past either bound has to stop rather than
/// let a dock eat the window or collapse to a line nobody can grab again.
pub(crate) fn dragged_dock_sizes(
    mut sizes: DockSizes,
    edge: DockEdge,
    position_x: f32,
    position_y: f32,
    viewport: ui::Viewport,
) -> DockSizes {
    match edge {
        DockEdge::Left => sizes.left = position_x.clamp(MIN_DOCK_SIZE, MAX_DOCK_SIZE),
        DockEdge::Right => {
            sizes.right = (viewport.width - position_x).clamp(MIN_DOCK_SIZE, MAX_DOCK_SIZE);
        }
        DockEdge::Bottom => {
            // The bottom dock is measured from the top of the status bar, not
            // from the bottom of the window: the bar is below it and does not
            // move.
            let body_bottom = viewport.height - STATUS_BAR_HEIGHT;
            sizes.bottom = (body_bottom - position_y).clamp(MIN_DOCK_SIZE, MAX_DOCK_SIZE);
        }
    }
    sizes
}

#[cfg(test)]
#[path = "chrome_test.rs"]
mod tests;
