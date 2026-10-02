//! Fixed-cost GPUI furniture for the Starmap canvas.

use gpui::{
    Action, Context, Div, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, Role,
    SharedString, Stateful, Styled, Toggled, Window, div, prelude::*, px, rgb, svg,
};
use k10s_core::Severity;
use k10s_theme::{Theme, Typography};

use crate::overlay::{OverlayKind, place_sparkline, sparkline_quads};
use crate::{FitView, ToggleEdges, ToggleLegend, ZoomIn, ZoomOut};

use super::{
    Chrome, Density, DetailBand, HOVER_SPARK, HOVER_WIDTH, HoverAnchor, HoverInfo,
    hover_card_height,
};

const EDGE_ICON: &str = "icons/link.svg";
const LEGEND_ICON: &str = "icons/info.svg";
const ZOOM_OUT_ICON: &str = "icons/square_minus.svg";
const FIT_ICON: &str = "icons/crosshair.svg";
const ZOOM_IN_ICON: &str = "icons/square_plus.svg";

const PANEL_RADIUS: f32 = 10.0;
const CONTROL_SIZE: f32 = 30.0;

impl Render for Chrome {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let state = self.state.clone();

        div()
            .size_full()
            .relative()
            .children((state.density != Density::Minimal).then(|| {
                summary_panel(
                    state.summary,
                    state.band,
                    state.overlay_kind,
                    state.density == Density::Full,
                    &theme,
                    &fonts,
                )
            }))
            .child(controls(state.edges_on, state.legend_on, &theme, &fonts))
            .children(
                (state.legend_on && state.density == Density::Full)
                    .then(|| legend(state.overlay_kind, &theme, &fonts)),
            )
            .children(
                state
                    .hover
                    .map(|(info, anchor)| hover_card(info, anchor, &theme, &fonts)),
            )
            .children(state.empty.then(|| {
                empty_state(
                    "No mapped workloads",
                    "This scene is valid but contains no workload namespaces.",
                    &theme,
                    &fonts,
                )
            }))
    }
}

fn panel(theme: &Theme) -> Div {
    div()
        .bg(rgb(theme.shell.elevated_surface_background).alpha(0.94))
        .border_1()
        .border_color(rgb(theme.shell.border_variant))
        .rounded(px(PANEL_RADIUS))
        .shadow_md()
}

fn summary_panel(
    summary: SharedString,
    active: DetailBand,
    overlay_kind: Option<OverlayKind>,
    expanded: bool,
    theme: &Theme,
    fonts: &Typography,
) -> impl IntoElement {
    panel(theme)
        .id("map-summary")
        .absolute()
        .top(px(14.0))
        .left(px(14.0))
        .max_w(px(430.0))
        .px(px(12.0))
        .py(px(9.0))
        .flex()
        .flex_col()
        .gap(px(7.0))
        .role(Role::Status)
        .aria_label("Starmap scale and resource summary")
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .font_family(fonts.display_family.clone())
                        .text_size(px(fonts.ui_size))
                        .text_color(rgb(theme.shell.text))
                        .child("STARMAP"),
                )
                .child(
                    div()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded_full()
                        .bg(rgb(theme.shell.element_selected))
                        .text_size(px(fonts.xsmall()))
                        .text_color(rgb(theme.shell.text_accent))
                        .child(active.label()),
                )
                .when_some(overlay_kind, |row, kind| {
                    row.child(
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded_full()
                            .bg(rgb(theme.shell.element_selected))
                            .text_size(px(fonts.xsmall()))
                            .text_color(rgb(theme.shell.text_accent))
                            .child(kind.badge()),
                    )
                })
                .when(expanded, |row| {
                    row.child(
                        div()
                            .text_size(px(fonts.xsmall()))
                            .text_color(rgb(theme.shell.text_muted))
                            .child(active.description()),
                    )
                    .when_some(overlay_kind, |row, kind| {
                        row.child(
                            div()
                                .text_size(px(fonts.xsmall()))
                                .text_color(rgb(theme.shell.text_muted))
                                .child(kind.blurb()),
                        )
                    })
                }),
        )
        .when(expanded, |card| {
            card.child(
                div()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(summary),
            )
            .child(scale_rail(active, theme, fonts))
        })
}

fn scale_rail(active: DetailBand, theme: &Theme, fonts: &Typography) -> impl IntoElement {
    let tooltip_size = fonts.xsmall();
    div().w_full().flex().items_center().gap(px(4.0)).children(
        DetailBand::ALL
            .into_iter()
            .enumerate()
            .map(|(index, band)| {
                let selected = band == active;
                div()
                    .id(("map-scale", index))
                    .flex_1()
                    .min_w(px(0.0))
                    .h(px(3.0))
                    .rounded_full()
                    .bg(rgb(if selected {
                        theme.shell.text_accent
                    } else {
                        theme.shell.border_variant
                    }))
                    .tooltip(move |_, cx| {
                        cx.new(move |_| MapTooltip::plain(band.label(), tooltip_size))
                            .into()
                    })
            }),
    )
}

fn controls(
    edges_on: bool,
    legend_on: bool,
    theme: &Theme,
    fonts: &Typography,
) -> impl IntoElement {
    panel(theme)
        .id("map-controls")
        .absolute()
        .top(px(14.0))
        .right(px(14.0))
        .p(px(4.0))
        .flex()
        .items_center()
        .gap(px(2.0))
        .role(Role::Toolbar)
        .aria_label("Starmap controls")
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
            cx.stop_propagation()
        })
        .child(action_button(
            "map-edges",
            EDGE_ICON,
            "Connections",
            Some(edges_on),
            ToggleEdges,
            theme,
            fonts,
        ))
        .child(action_button(
            "map-legend",
            LEGEND_ICON,
            "Legend",
            Some(legend_on),
            ToggleLegend,
            theme,
            fonts,
        ))
        .child(separator(theme))
        .child(action_button(
            "map-zoom-out",
            ZOOM_OUT_ICON,
            "Zoom Out",
            None,
            ZoomOut,
            theme,
            fonts,
        ))
        .child(action_button(
            "map-fit",
            FIT_ICON,
            "Fit Starmap",
            None,
            FitView,
            theme,
            fonts,
        ))
        .child(action_button(
            "map-zoom-in",
            ZOOM_IN_ICON,
            "Zoom In",
            None,
            ZoomIn,
            theme,
            fonts,
        ))
}

fn separator(theme: &Theme) -> impl IntoElement {
    div()
        .w(px(1.0))
        .h(px(18.0))
        .mx(px(3.0))
        .bg(rgb(theme.shell.border_variant))
}

fn action_button<A: Action + Clone>(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    active: Option<bool>,
    action: A,
    theme: &Theme,
    fonts: &Typography,
) -> Stateful<Div> {
    let tooltip_action = action.clone();
    let tooltip_size = fonts.xsmall();
    let selected = active.unwrap_or(false);
    div()
        .id(id)
        .size(px(CONTROL_SIZE))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(label)
        .when_some(active, |button, active| {
            button.aria_toggled(if active {
                Toggled::True
            } else {
                Toggled::False
            })
        })
        .when(selected, |button| {
            button.bg(rgb(theme.shell.element_selected))
        })
        .hover(|button| button.bg(rgb(theme.shell.element_hover)))
        .child(svg().path(icon).size(px(15.0)).text_color(rgb(if selected {
            theme.shell.text_accent
        } else {
            theme.shell.text_muted
        })))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(action.clone()), cx);
        })
        .tooltip(move |window, cx| {
            cx.new(|_| MapTooltip::for_action(label, &tooltip_action, tooltip_size, window))
                .into()
        })
}

fn swatch_label(severity: Severity, overlay: bool) -> &'static str {
    match (overlay, severity) {
        (false, Severity::Ok) => "Healthy",
        (false, Severity::Warn) => "Warning",
        (false, Severity::Err) => "Critical",
        (false, Severity::Unknown) => "Unknown",
        (true, Severity::Ok) => "Ok",
        (true, Severity::Warn) => "Warn",
        (true, Severity::Err) => "Err",
        (true, Severity::Unknown) => "Unknown",
    }
}

fn legend(
    overlay_kind: Option<OverlayKind>,
    theme: &Theme,
    fonts: &Typography,
) -> impl IntoElement {
    let title = overlay_kind.map_or("HEALTH", OverlayKind::legend_title);
    let aria = overlay_kind.map_or("Health legend", OverlayKind::legend_aria);
    let overlay = overlay_kind.is_some();
    panel(theme)
        .id("map-health-legend")
        .absolute()
        .bottom(px(14.0))
        .left(px(14.0))
        .px(px(10.0))
        .py(px(8.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .role(Role::Legend)
        .aria_label(aria)
        .child(
            div()
                .text_size(px(fonts.xsmall()))
                .text_color(rgb(theme.shell.text_muted))
                .child(title),
        )
        .children(
            [
                (Severity::Ok, "✓", swatch_label(Severity::Ok, overlay)),
                (Severity::Warn, "!", swatch_label(Severity::Warn, overlay)),
                (Severity::Err, "×", swatch_label(Severity::Err, overlay)),
                (
                    Severity::Unknown,
                    "?",
                    swatch_label(Severity::Unknown, overlay),
                ),
            ]
            .into_iter()
            .map(|(severity, mark, label)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .size(px(16.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(theme.map.pod_color(severity))
                            .text_size(px(fonts.xsmall()))
                            .text_color(rgb(theme.map.bg))
                            .child(mark),
                    )
                    .child(
                        div()
                            .text_size(px(fonts.xsmall()))
                            .text_color(rgb(theme.shell.text_muted))
                            .child(label),
                    )
            }),
        )
}

fn hover_card(
    info: HoverInfo,
    anchor: HoverAnchor,
    theme: &Theme,
    fonts: &Typography,
) -> impl IntoElement {
    let color = info.color(theme);
    let card_h = hover_card_height(&info);
    let overlay_kind = info.overlay_kind;
    let overlay_label = info.overlay_label.clone();
    let overlay_spark = info.overlay_spark.clone();
    let overlay_tint = info.overlay_tint;
    panel(theme)
        .absolute()
        .left(px(anchor.left))
        .top(px(anchor.top))
        .w(px(HOVER_WIDTH))
        .min_h(px(card_h))
        .px(px(10.0))
        .py(px(8.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .child(
            div()
                .size(px(24.0))
                .flex_none()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(color.alpha(0.18))
                .border_1()
                .border_color(color)
                .text_size(px(fonts.small()))
                .text_color(color)
                .child(info.mark),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_size(px(fonts.ui_size))
                                .text_color(rgb(theme.shell.text))
                                .child(info.name),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(fonts.xsmall()))
                                .text_color(color)
                                .child(info.status),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_size(px(fonts.xsmall()))
                        .text_color(rgb(theme.shell.text_muted))
                        .child(info.kind)
                        .children(info.namespace.map(|namespace| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .child("·")
                                .child(namespace)
                        }))
                        .children(info.owner.map(|owner| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .child("/")
                                .child(owner)
                        })),
                )
                .when(overlay_kind.is_some() || overlay_label.is_some(), |col| {
                    col.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_size(px(fonts.xsmall()))
                            .text_color(rgb(theme.shell.text_accent))
                            .children(overlay_kind.map(OverlayKind::badge))
                            .children(overlay_label.map(|label| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .child("·")
                                    .child(label)
                            })),
                    )
                })
                .when(overlay_spark.len() >= 2, |col| {
                    col.child(spark_strip(&overlay_spark, overlay_tint, theme))
                }),
        )
}

fn spark_strip(
    unit: &[k10s_theme::Point],
    tint: Option<Severity>,
    theme: &Theme,
) -> impl IntoElement {
    let color = tint.map_or_else(
        || rgb(theme.shell.text_accent),
        |severity| theme.map.pod_color(severity),
    );
    let w = HOVER_WIDTH - 20.0;
    let dest = k10s_core::Rect::new(0.0, 0.0, w, HOVER_SPARK);
    let placed = place_sparkline(unit, dest);
    div().relative().w(px(w)).h(px(HOVER_SPARK)).children(
        sparkline_quads(&placed, 1.5).into_iter().map(move |rect| {
            div()
                .absolute()
                .left(px(rect.x))
                .top(px(rect.y))
                .w(px(rect.w.max(1.5)))
                .h(px(rect.h.max(1.5)))
                .bg(color)
        }),
    )
}

fn empty_state(
    title: &'static str,
    body: &'static str,
    theme: &Theme,
    fonts: &Typography,
) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .left_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            panel(theme)
                .id("map-empty-state")
                .w(px(360.0))
                .px(px(24.0))
                .py(px(20.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .role(Role::Status)
                .aria_label(title)
                .child(
                    div()
                        .font_family(fonts.display_family.clone())
                        .text_size(px(fonts.ui_size + 2.0))
                        .text_color(rgb(theme.shell.text))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(fonts.small()))
                        .text_color(rgb(theme.shell.text_muted))
                        .child(body),
                ),
        )
}

struct MapTooltip {
    label: SharedString,
    key: Option<SharedString>,
    size: f32,
}

impl MapTooltip {
    fn plain(label: &'static str, size: f32) -> MapTooltip {
        MapTooltip {
            label: label.into(),
            key: None,
            size,
        }
    }

    fn for_action(
        label: &'static str,
        action: &dyn Action,
        size: f32,
        window: &Window,
    ) -> MapTooltip {
        let key = window
            .bindings_for_action(action)
            .into_iter()
            .next()
            .map(|binding| {
                binding
                    .keystrokes()
                    .iter()
                    .map(|stroke| stroke.inner().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .into()
            });
        MapTooltip {
            label: label.into(),
            key,
            size,
        }
    }
}

impl Render for MapTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        div()
            .px(px(8.0))
            .py(px(5.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .bg(rgb(theme.shell.elevated_surface_background))
            .border_1()
            .border_color(rgb(theme.shell.border_variant))
            .rounded(px(6.0))
            .shadow_md()
            .text_size(px(self.size))
            .text_color(rgb(theme.shell.text))
            .child(self.label.clone())
            .children(self.key.clone().map(|key| {
                div()
                    .px(px(4.0))
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(rgb(theme.shell.element_background))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(key)
            }))
    }
}
