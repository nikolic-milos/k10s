//! Zed-compatible shell primitives.
//!
//! These are the geometry contracts shared by every shell view. Keeping them
//! here prevents a new panel from inventing a nearly-Zed row height or a third
//! kind of header. The values mirror the default density in the pinned Zed
//! revision: a 16 px rem, 32 px tabs, a 34 px platform title bar. Type sizes
//! are *not* here -- they moved to `k10s_theme::Typography`, because a person
//! scales their interface and the whole ladder has to move with it. The status
//! bar keeps its default floor and grows to contain its text and key hint.

use gpui::{
    Context, DefiniteLength, Div, IntoElement, ParentElement, Render, SharedString, Stateful,
    Styled, Window, div, prelude::*, px, rgb, svg,
};
use k10s_theme::{Appearance, Theme, Typography};

pub const TITLE_BAR_HEIGHT: f32 = 34.0;
pub const TAB_HEIGHT: f32 = 32.0;
pub const PANEL_HEADER_HEIGHT: f32 = 32.0;
pub const TOOLBAR_HEIGHT: f32 = 36.0;
pub const STATUS_BAR_HEIGHT: f32 = 30.0;
pub(crate) const STATUS_BAR_BORDER: f32 = 1.0;
const KEY_HINT_PADDING_Y: f32 = 1.0;
const KEY_HINT_BORDER: f32 = 1.0;
pub const ACTIVITY_BAR_WIDTH: f32 = 40.0;
pub const PANEL_FOOTER_HEIGHT: f32 = 24.0;

pub const LIST_ROW_HEIGHT: f32 = 28.0;

pub const CONTENT_PADDING: f32 = 8.0;
pub const MODAL_TOP: f32 = 80.0;
pub const MODAL_WIDTH: f32 = 544.0;
pub const MODAL_MAX_HEIGHT: f32 = 384.0;
pub const RESIZE_HANDLE_SIZE: f32 = 6.0;
pub const MIN_DOCK_SIZE: f32 = 120.0;
pub const MAX_DOCK_SIZE: f32 = 800.0;
pub const MIN_CENTER_WIDTH: f32 = 320.0;
pub const MIN_CENTER_HEIGHT: f32 = 160.0;

/// The title bar's mark. 18 px is the size the symbol was cut for; the helm
/// with its spokes is unreadable there, which is why the brand kit ships both
/// and why only the launch screen gets the wheel.
pub const TITLE_MARK_SIZE: f32 = 18.0;
/// The launch screen's helm.
pub const LAUNCH_LOGO_SIZE: f32 = 88.0;

/// The brand bitmaps, named by the appearance they are drawn *on*. These are
/// paths into the asset source the application installs, exactly like the
/// window-control SVGs below: the shell draws bytes it does not carry, and the
/// same four literals are pinned by a test in `k10s-assets`, because a rename
/// on one side only is a missing image with no error anywhere.
///
/// Two files rather than one tinted file. The artwork is flat brand blue on a
/// transparent field; tinting a bitmap to fit a theme turns a brand colour into
/// an approximation of itself, and blue on a dark background is a smudge.
pub fn brand_mark(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Light => "brand/mark-light.png",
        Appearance::Dark => "brand/mark-dark.png",
    }
}

pub fn brand_logo(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Light => "brand/logo-light.png",
        Appearance::Dark => "brand/logo-dark.png",
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    pub fn update(&mut self, width: f32, height: f32) -> bool {
        let next = Viewport {
            width: width.max(0.0),
            height: height.max(0.0),
        };
        if *self == next {
            return false;
        }
        *self = next;
        true
    }

    pub fn rows(self, chrome: f32, padding: f32, row_height: f32, cap: usize) -> usize {
        capacity(self.height, chrome + padding, row_height, cap)
    }

    pub fn columns(self, padding: f32, cell_width: f32, cap: usize) -> usize {
        capacity(self.width, padding, cell_width, cap)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DockSizes {
    pub left: f32,
    pub right: f32,
    pub bottom: f32,
}

impl DockSizes {
    /// What the open docks actually get, given what they asked for. This
    /// budgets the centre and nothing else: the sides are scaled together when
    /// they would leave less than [`MIN_CENTER_WIDTH`] between them, and the
    /// bottom is capped at what the vertical chrome leaves. [`MIN_DOCK_SIZE`]
    /// and [`MAX_DOCK_SIZE`] are deliberately not applied here -- they bound
    /// what a person can *ask* for, so the drag path and the settings loader
    /// own them, and a window too small to honour a legal request is still the
    /// centre's to win.
    pub fn resolve(
        viewport: Viewport,
        requested: DockSizes,
        left_open: bool,
        right_open: bool,
        bottom_open: bool,
        status_height: f32,
    ) -> DockSizes {
        let mut left = left_open.then_some(requested.left.max(0.0)).unwrap_or(0.0);
        let mut right = right_open
            .then_some(requested.right.max(0.0))
            .unwrap_or(0.0);
        let side_budget = (viewport.width - MIN_CENTER_WIDTH).max(0.0);
        let requested_sides = left + right;
        if requested_sides > side_budget && requested_sides > 0.0 {
            let scale = side_budget / requested_sides;
            left *= scale;
            right *= scale;
        }

        let vertical_chrome = TITLE_BAR_HEIGHT + TAB_HEIGHT + status_height;
        let bottom_budget = (viewport.height - vertical_chrome - MIN_CENTER_HEIGHT).max(0.0);
        let bottom = if bottom_open {
            requested.bottom.max(0.0).min(bottom_budget)
        } else {
            0.0
        };

        DockSizes {
            left,
            right,
            bottom,
        }
    }
}

fn capacity(available: f32, reserved: f32, unit: f32, cap: usize) -> usize {
    if !available.is_finite() || !reserved.is_finite() || !unit.is_finite() || unit <= 0.0 {
        return 1;
    }
    (((available - reserved).max(0.0) / unit).floor() as usize).clamp(1, cap.max(1))
}

/// The status owner and its children share one line height, so larger type
/// cannot escape the bar while the dock still reserves its old height.
pub(crate) fn status_bar_geometry(fonts: &Typography, window: &Window) -> (DefiniteLength, f32) {
    let line_height = gpui::phi();
    let line = |font_size| {
        f32::from(window.pixel_snap(line_height.to_pixels(px(font_size).into(), window.rem_size())))
    };
    (
        line_height,
        status_bar_height(line(fonts.small()), line(fonts.xsmall())),
    )
}

fn status_bar_height(text_line: f32, key_line: f32) -> f32 {
    let key_height = key_line + (KEY_HINT_PADDING_Y + KEY_HINT_BORDER) * 2.0;
    STATUS_BAR_HEIGHT.max(text_line.max(key_height) + STATUS_BAR_BORDER)
}

/// Zed's platform title bar height: a 1.75 rem floor that never drops below
/// the 34 px the traffic-light and control hit targets need.
pub fn title_bar_height(window: &Window) -> f32 {
    (1.75 * f32::from(window.rem_size())).max(TITLE_BAR_HEIGHT)
}

/// A Zed-shaped icon button: one of their shipped SVGs in a hoverable square
/// with a toggle state. The caller wires the click and the tooltip.
pub fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    label: &'static str,
    active: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(22.0))
        .rounded(px(4.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .role(gpui::Role::Button)
        .aria_label(label)
        .when(active, |button| {
            button.bg(rgb(theme.shell.element_selected))
        })
        .hover(|button| button.bg(rgb(theme.shell.element_hover)))
        .child(svg().path(icon).size(px(14.0)).text_color(rgb(if active {
            theme.shell.text
        } else {
            theme.shell.text_muted
        })))
}

/// A rail-sized icon button: the activity bar's hit target is larger than a
/// title-bar control so a cluster of them can be clicked without aiming.
pub fn rail_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    label: &'static str,
    active: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(32.0))
        .rounded(px(6.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .role(gpui::Role::Button)
        .aria_label(label)
        .when(active, |button| {
            button.bg(rgb(theme.shell.element_selected))
        })
        .hover(|button| button.bg(rgb(theme.shell.element_hover)))
        .child(svg().path(icon).size(px(18.0)).text_color(rgb(if active {
            theme.shell.text
        } else {
            theme.shell.text_muted
        })))
}

/// The hover tooltip an icon button shows: the action's name and, when one
/// exists, its keybinding -- Zed's `Tooltip::for_action` shape.
pub struct Tooltip {
    pub label: SharedString,
    pub key: Option<SharedString>,
}

impl Tooltip {
    pub fn with_binding(
        label: &'static str,
        action: &dyn gpui::Action,
        window: &Window,
    ) -> Tooltip {
        let key = window
            .bindings_for_action(action)
            .into_iter()
            .next()
            .map(|binding| {
                binding
                    .keystrokes()
                    .iter()
                    .map(|keystroke| keystroke.inner().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .into()
            });
        Tooltip {
            label: label.into(),
            key,
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        div()
            .px(px(8.0))
            .py(px(4.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .bg(rgb(theme.shell.elevated_surface_background))
            .border_1()
            .border_color(rgb(theme.shell.border_variant))
            .rounded(px(6.0))
            .shadow_md()
            .font_family(fonts.ui_family.clone())
            .text_size(px(fonts.small()))
            .text_color(rgb(theme.shell.text))
            .child(self.label.clone())
            .children(self.key.clone().map(|key| {
                div()
                    .px(px(4.0))
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(rgb(theme.shell.element_background))
                    .border_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.xsmall()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(key)
            }))
    }
}

pub fn panel_header(
    theme: &Theme,
    fonts: &Typography,
    title: impl Into<SharedString>,
) -> Stateful<Div> {
    let title = title.into();
    div()
        .id(title.clone())
        .h(px(PANEL_HEADER_HEIGHT))
        .flex_none()
        .px(px(12.0))
        .flex()
        .items_center()
        .bg(rgb(theme.shell.panel_background))
        .border_b_1()
        .border_color(rgb(theme.shell.border))
        .text_size(px(fonts.ui_size))
        .text_color(rgb(theme.shell.text))
        .whitespace_nowrap()
        .overflow_hidden()
        .role(gpui::Role::Label)
        .child(title)
}

/// A view's status slot under its rows, drawn only when something is in it:
/// the view's cached text preserves its label, line, hint and note in order.
pub(crate) fn status_slot(
    theme: &Theme,
    fonts: &Typography,
    text: Option<SharedString>,
) -> Option<Div> {
    text.map(|text| {
        div()
            .flex_none()
            .p(px(12.0))
            .text_size(px(fonts.ui_size))
            .text_color(rgb(theme.shell.text))
            .child(text)
    })
}

pub fn toolbar(theme: &Theme) -> Stateful<Div> {
    div()
        .id("toolbar")
        .h(px(TOOLBAR_HEIGHT))
        .flex_none()
        .px(px(10.0))
        .flex()
        .items_center()
        .bg(rgb(theme.shell.toolbar_background))
        .border_b_1()
        .border_color(rgb(theme.shell.border))
        .role(gpui::Role::Toolbar)
}

pub fn key_hint(
    theme: &Theme,
    fonts: &Typography,
    key: &'static str,
    label: &'static str,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .child(
            div()
                .px(px(4.0))
                .py(px(KEY_HINT_PADDING_Y))
                .rounded(px(3.0))
                .bg(rgb(theme.shell.element_background))
                .border(px(KEY_HINT_BORDER))
                .border_color(rgb(theme.shell.border_variant))
                .text_size(px(fonts.xsmall()))
                .text_color(rgb(theme.shell.text_muted))
                .child(key),
        )
        .child(
            div()
                .text_size(px(fonts.small()))
                .text_color(rgb(theme.shell.text_muted))
                .child(label),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_appearance_names_the_artwork_cut_for_it() {
        assert_eq!(brand_mark(Appearance::Light), "brand/mark-light.png");
        assert_eq!(brand_mark(Appearance::Dark), "brand/mark-dark.png");
        assert_eq!(brand_logo(Appearance::Light), "brand/logo-light.png");
        assert_eq!(brand_logo(Appearance::Dark), "brand/logo-dark.png");
    }

    #[test]
    fn capacity_uses_the_allocated_view_not_the_window() {
        let viewport = Viewport {
            width: 420.0,
            height: 240.0,
        };
        assert_eq!(viewport.rows(56.0, 16.0, 20.0, 200), 8);
        assert_eq!(viewport.columns(16.0, 8.0, 400), 50);
    }

    #[test]
    fn capacity_is_bounded_for_transient_and_invalid_layouts() {
        let zero = Viewport::default();
        assert_eq!(zero.rows(100.0, 16.0, 20.0, 200), 1);

        let invalid = Viewport {
            width: f32::NAN,
            height: f32::INFINITY,
        };
        assert_eq!(invalid.rows(0.0, 0.0, 20.0, 200), 1);
        assert_eq!(invalid.columns(0.0, 0.0, 400), 1);
    }

    #[test]
    fn docks_preserve_a_usable_center_on_small_windows() {
        let requested = DockSizes {
            left: 300.0,
            right: 400.0,
            bottom: 300.0,
        };
        let sizes = DockSizes::resolve(
            Viewport {
                width: 800.0,
                height: 600.0,
            },
            requested,
            true,
            true,
            true,
            STATUS_BAR_HEIGHT,
        );
        assert!((sizes.left + sizes.right - 480.0).abs() < 0.001);
        assert_eq!(sizes.bottom, 300.0);

        let tiny = DockSizes::resolve(
            Viewport {
                width: 240.0,
                height: 180.0,
            },
            requested,
            true,
            true,
            true,
            STATUS_BAR_HEIGHT,
        );
        assert_eq!(tiny.left + tiny.right, 0.0);
        assert_eq!(tiny.bottom, 0.0);
    }

    fn default_text_line(font_size: f32, scale: f32) -> f32 {
        let style = gpui::TextStyle {
            font_size: px(font_size).into(),
            ..Default::default()
        };
        let line = f32::from(style.line_height.to_pixels(style.font_size, px(16.0)));
        (line * scale).round() / scale
    }

    #[test]
    fn status_children_fit_across_supported_type_sizes_and_scales() {
        for half_pixels in 16..=64 {
            let fonts = Typography {
                ui_size: half_pixels as f32 * 0.5,
                ..Default::default()
            };
            for scale in [1.0, 1.25, 1.5, 2.0] {
                let text_line = default_text_line(fonts.small(), scale);
                let key_line = default_text_line(fonts.xsmall(), scale);
                let height = status_bar_height(text_line, key_line);
                assert!((30.0..=51.0).contains(&height));
                let content = height - 1.0;
                for child in [text_line, key_line + 4.0] {
                    let top = (content - child) * 0.5;
                    let bottom = top + child;
                    assert!(top >= 0.0 && bottom <= content);
                }
                if fonts.ui_size <= 14.0 {
                    assert_eq!(height, 30.0);
                }
            }
        }
    }

    #[test]
    fn large_type_reserves_the_observed_status_owner_and_dock_space() {
        let fonts = Typography {
            ui_size: 32.0,
            ..Default::default()
        };
        let status_height = status_bar_height(
            default_text_line(fonts.small(), 1.0),
            default_text_line(fonts.xsmall(), 1.0),
        );
        assert_eq!(status_height, 50.0);
        let sizes = DockSizes::resolve(
            Viewport {
                width: 800.0,
                height: 400.0,
            },
            DockSizes {
                left: 0.0,
                right: 0.0,
                bottom: 800.0,
            },
            false,
            false,
            true,
            status_height,
        );
        assert_eq!(sizes.bottom, 124.0);
        let center_height = 400.0 - 34.0 - 32.0 - status_height - sizes.bottom;
        assert_eq!(center_height, 160.0);
    }
}
