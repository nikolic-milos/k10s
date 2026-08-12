//! Every colour and typeface in the product, as data a user can replace.
//!
//! A [`Theme`] is owned and refcounted rather than `&'static`, because the
//! themes that matter most are the ones nobody compiled: `~/.config/k10s/
//! themes/*.json` produces exactly the same value the built-ins do. Loading is
//! pure over `&str` -- the app reads bytes, this crate returns a theme and a
//! list of labelled notes -- so a malformed theme degrades a session and never
//! ends one, and every path through the loader is a unit test with no
//! filesystem.
//!
//! Two things here are load-bearing for the map's paint path and must survive
//! any refactor. [`MapTheme`] is a plain `Copy` struct of scalars, so the walk
//! reads fields rather than chasing an `Arc`; and the derived values a region
//! loop needs are hoisted once per frame ([`MapTheme::heat_ramp`]) rather than
//! recomputed per region. An earlier extraction that skipped the hoist cost
//! 14-18% on the zoom-0 fit walk, which is why both are stated rather than
//! assumed.
//!
//! Fonts are deliberately *not* part of a theme. A person changes their
//! typeface independently of their colours, so [`Typography`] is a setting
//! that this crate only publishes, in the same global-per-window shape as the
//! active theme.

mod builtin;
mod load;
mod registry;

use std::sync::Arc;

use gpui::{Rgba, SharedString};
use k10s_core::{BUILTIN_KIND_COUNT, BUILTIN_TOOL_COUNT, KindId, Severity, ToolId};

pub use builtin::{K10S_DARK, K10S_LIGHT, ONE_DARK, builtin_family};
pub use load::{LoadedFamily, Overrides, parse_color, parse_family, parse_overrides, strip_jsonc};
pub use registry::ThemeRegistry;

/// Whether a theme is meant for a light or a dark surface. This is the axis
/// the `"system"` theme mode resolves against, and the axis a user theme file
/// declares so the registry knows which built-in it inherits from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Appearance {
    Light,
    #[default]
    Dark,
}

impl Appearance {
    pub fn as_str(self) -> &'static str {
        match self {
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }

    pub fn parse(text: &str) -> Option<Appearance> {
        match text.trim() {
            t if t.eq_ignore_ascii_case("light") => Some(Appearance::Light),
            t if t.eq_ignore_ascii_case("dark") => Some(Appearance::Dark),
            _ => None,
        }
    }
}

/// gpui reports four appearances because macOS has two flavours of each; the
/// only distinction a colour scheme cares about is light or dark.
impl From<gpui::WindowAppearance> for Appearance {
    fn from(appearance: gpui::WindowAppearance) -> Appearance {
        match appearance {
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => {
                Appearance::Light
            }
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => Appearance::Dark,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapTheme {
    pub bg: u32,
    pub hex_line: u32,
    pub ns_fill: u32,
    pub ns_border: u32,
    pub edge: u32,
    pub hud_bg: u32,
    pub hud_text: u32,
    pub curve_core: u32,
    pub curve_core_alpha: f32,
    pub curve_glow: u32,
    pub curve_glow_alpha: f32,
    pub card_header_fill: u32,
    /// The tint the workload kind glyph is drawn in. gpui rasterizes an SVG as
    /// an alpha mask, so an icon is one colour and that colour is this one --
    /// vendor marks take their own hue from [`MapTheme::tool_colors`] instead.
    pub wl_icon: u32,
    /// The ring drawn around whatever the pointer is over, and around whatever
    /// is selected. Both are painted outside the frame walk, so neither is part
    /// of the cull oracle's counters; they are still theme values because an
    /// affordance a user cannot recolour is an affordance that fails somebody.
    pub hover_ring: u32,
    pub selection_ring: u32,
    // The five label colours the walk paints text with. They are theme fields
    // rather than the literals they used to be because a light canvas needs
    // dark labels, and because a colour that only exists inside a loop is a
    // colour no contrast test can see.
    pub region_label: u32,
    pub workload_label: u32,
    pub sat_label: u32,
    pub sat_detail_label: u32,
    pub pod_label: u32,
    pub unknown_kind: u32,
    pub kind_colors: [u32; BUILTIN_KIND_COUNT as usize],
    pub tool_colors: [u32; BUILTIN_TOOL_COUNT as usize],
    // Indexed by severity: Ok, Warn, Err, Unknown.
    pub pod_severity: [u32; 4],
    pub workload_fill: [u32; 4],
    pub workload_border: [(u32, f32); 4],
    // Healthy -> warm -> hot stops for the region heat ramp.
    pub heat_fill: [u32; 3],
    pub heat_border_base: u32,
    pub heat_border_hot: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShellTheme {
    // Keep the names aligned with Zed's ThemeColors schema. Views choose a
    // semantic surface instead of treating every dark rectangle as the same
    // colour; that is what keeps tabs, panels, editors, and modals coherent as
    // the shell grows.
    pub background: u32,
    pub surface_background: u32,
    pub elevated_surface_background: u32,
    pub editor_background: u32,
    pub toolbar_background: u32,
    pub panel_background: u32,
    pub status_bar_background: u32,
    pub tab_bar_background: u32,
    pub tab_inactive_background: u32,
    pub tab_active_background: u32,
    pub border: u32,
    pub border_variant: u32,
    pub border_focused: u32,
    pub element_background: u32,
    pub element_hover: u32,
    pub element_active: u32,
    pub element_selected: u32,
    pub text: u32,
    pub text_muted: u32,
    pub text_placeholder: u32,
    pub text_accent: u32,
    pub editor_foreground: u32,
    pub search_match_background: (u32, f32),
    pub terminal_background: u32,
    pub terminal_foreground: u32,
    pub terminal_bright_foreground: u32,
    pub terminal_dim_foreground: u32,
    pub terminal_ansi: [u32; 16],
    pub terminal_ansi_dim: [u32; 8],
    pub cursor: u32,
    pub success: u32,
    pub warning: u32,
    pub error: u32,
}

// The editor's token colours, mirroring Zed's SyntaxTheme keys for the
// captures the YAML grammar emits, plus the editor-only chrome (line
// numbers, active line, selection). Error and warning stay on ShellTheme:
// they are workspace-wide states, not syntax.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntaxTheme {
    pub property: u32,
    pub string: u32,
    pub number: u32,
    pub boolean: u32,
    pub constant: u32,
    pub comment: u32,
    pub label: u32,
    pub type_name: u32,
    pub attribute: u32,
    pub punctuation: u32,
    pub punctuation_special: u32,
    pub line_number: u32,
    pub active_line_number: u32,
    pub active_line_background: (u32, f32),
    pub selection_background: (u32, f32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: SharedString,
    pub appearance: Appearance,
    pub map: MapTheme,
    pub shell: ShellTheme,
    pub syntax: SyntaxTheme,
}

/// A `themes/*.json` file: one author's set of variants under one name, which
/// is the unit Zed publishes and therefore the unit a user copies in.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeFamily {
    pub name: SharedString,
    pub author: SharedString,
    pub themes: Vec<Arc<Theme>>,
}

/// The typefaces and sizes the shell paints with. These are settings rather
/// than theme fields -- a person picks a typeface once and then tries every
/// theme with it -- but they live here because the map paints text too and
/// must not depend on the crate that parses the settings file.
#[derive(Debug, Clone, PartialEq)]
pub struct Typography {
    pub ui_family: SharedString,
    pub ui_size: f32,
    /// The headline face. It exists as a setting rather than a constant
    /// because the map sets namespace names in it at display sizes, and a
    /// person who dislikes a display face dislikes it everywhere at once.
    pub display_family: SharedString,
    pub buffer_family: SharedString,
    pub buffer_size: f32,
    /// A multiple of the buffer size, the way every editor states it; the
    /// shell wants whole pixels, so [`Typography::line_height`] rounds.
    pub buffer_line_height: f32,
}

/// The map's type ladder, in screen pixels, derived once per frame from
/// [`Typography`] and then read as plain scalars by the frame walk.
///
/// It is a `Copy` struct of scalars for the same reason [`MapTheme`] is: the
/// walk reads fields per node and must not chase a pointer or recompute a
/// derivation there. Namespace names are the one dynamic size on the map --
/// they scale with how much of the screen the island covers -- so the ladder
/// carries the band rather than a value, and the walk quantises inside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapType {
    /// Namespace names, smallest and largest the band allows.
    pub region_min: f32,
    pub region_max: f32,
    pub workload: f32,
    pub pod: f32,
    pub sat: f32,
    pub sat_detail: f32,
    /// A multiple of the label size. Map labels are single lines placed against
    /// a rect, so this sets the box a line is centred in, not leading between
    /// rows.
    pub line_height: f32,
}

/// The default UI face is Inter and the default mono face is Lilex, both
/// shipped by `k10s-assets`; League Spartan is the display face and is
/// registered but never a default, because it is a headline typeface.
///
/// Inter is spelled with its optical size because that is what the files
/// declare: Inter 4's static cuts name the family after the size they were
/// drawn for, and the 18 pt cut is the one shipped. The name a person types is
/// "Inter", which [`FAMILY_ALIASES`] resolves, so both work and neither is a
/// silent platform fallback.
pub const DEFAULT_UI_FAMILY: &str = "Inter 18pt";
pub const DEFAULT_BUFFER_FAMILY: &str = "Lilex";
pub const DISPLAY_FAMILY: &str = "League Spartan";

/// Friendly names for families whose files declare something longer. This is
/// not a fallback list -- each pair is the same typeface under two spellings,
/// so resolving through it is silent, while a family nobody has is still a
/// note.
pub const FAMILY_ALIASES: [(&str, &str); 1] = [("Inter", DEFAULT_UI_FAMILY)];

pub const UI_FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 8.0..=32.0;
pub const BUFFER_FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 8.0..=48.0;
pub const LINE_HEIGHT_RANGE: std::ops::RangeInclusive<f32> = 1.0..=2.5;

impl Default for Typography {
    fn default() -> Typography {
        Typography {
            ui_family: DEFAULT_UI_FAMILY.into(),
            ui_size: 14.0,
            display_family: DISPLAY_FAMILY.into(),
            buffer_family: DEFAULT_BUFFER_FAMILY.into(),
            buffer_size: 15.0,
            buffer_line_height: 1.35,
        }
    }
}

impl Typography {
    /// Secondary labels: Zed's density steps down 2 px at a time from the UI
    /// size, so a user who scales the UI scales the whole ladder with it.
    pub fn small(&self) -> f32 {
        (self.ui_size - 2.0).max(6.0)
    }

    /// Keycaps and counters, the smallest legible step.
    pub fn xsmall(&self) -> f32 {
        (self.ui_size - 4.0).max(6.0)
    }

    /// The display step: the one place the product says its own name at size,
    /// on the launch screen. A multiple rather than a fixed 28 px, because a
    /// person who scales the interface scales this with it like every other
    /// step on the ladder.
    pub fn display(&self) -> f32 {
        self.ui_size * 2.0
    }

    /// The buffer row height in whole pixels. Rows are laid out and hit-tested
    /// against this, so it must be the same integer everywhere or a click
    /// lands on the wrong line at the bottom of a long file.
    pub fn line_height(&self) -> f32 {
        (self.buffer_size * self.buffer_line_height)
            .round()
            .max(1.0)
    }

    /// The map's ladder, on the same steps as the shell's so that scaling the
    /// interface scales the map with it. The two smallest sizes floor at 6 px
    /// exactly as [`Typography::xsmall`] does: a label nobody can read is worse
    /// than a label nobody drew.
    pub fn map(&self) -> MapType {
        MapType {
            region_min: self.ui_size,
            region_max: self.ui_size * 3.0,
            workload: self.small(),
            pod: self.xsmall(),
            sat: self.xsmall(),
            sat_detail: (self.ui_size - 5.5).max(6.0),
            line_height: 1.35,
        }
    }
}

/// Snap a screen size to a coarse ladder.
///
/// Every dynamically sized thing on the map goes through this, and it is not a
/// nicety. gpui keys its sprite atlas on the rasterized pixel size of an SVG
/// and its shaped-line cache on the font size, so a size that varies smoothly
/// with zoom mints a fresh atlas tile and a fresh shaped line on every frame of
/// a zoom -- a per-frame allocation and GPU upload, on the one path that is
/// measured for having neither.
///
/// The ladder is geometric rather than linear because it has to cover 8 px to
/// 96 px without either crowding the small end or stepping visibly at the large
/// one; `SIZE_STEPS` is the shipped set and `quantize` returns a member of it.
pub const SIZE_STEPS: [f32; 15] = [
    8.0, 10.0, 12.0, 14.0, 16.0, 20.0, 24.0, 28.0, 32.0, 40.0, 48.0, 56.0, 64.0, 80.0, 96.0,
];

#[inline]
pub fn quantize(size: f32) -> f32 {
    // A binary search rather than a scan. It is called once per glyph inside the
    // frame walk, and the walk is the one loop in this product with a committed
    // ratio gate on it; fifteen compares where four will do is the kind of thing
    // that shows up there and nowhere else.
    let at = SIZE_STEPS.partition_point(|step| *step <= size);
    SIZE_STEPS[at.saturating_sub(1)]
}

// The heat ramp's stops, channel-converted once per frame: the per-region
// calls then run on floats already in place, which is what lets a themed
// ramp cost what the compile-time constants used to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatRamp {
    fill: [Rgba; 3],
    border_base: Rgba,
    border_hot: Rgba,
}

impl HeatRamp {
    #[inline]
    pub fn color(&self, frac: f32) -> Rgba {
        let t = (frac * 2.5).clamp(0.0, 1.0);
        lerp_rgba(
            lerp_rgba(self.fill[0], self.fill[1], (t * 2.0).min(1.0)),
            self.fill[2],
            (t * 2.0 - 1.0).max(0.0),
        )
    }

    #[inline]
    pub fn border(&self, frac: f32) -> Rgba {
        let t = (frac * 4.0).clamp(0.0, 1.0);
        lerp_rgba(self.border_base, self.border_hot, t * 0.8)
    }
}

#[inline]
fn lerp_rgba(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

fn opaque(c: u32) -> Rgba {
    Rgba {
        r: channel(c, 16),
        g: channel(c, 8),
        b: channel(c, 0),
        a: 1.0,
    }
}

impl MapTheme {
    pub fn heat_ramp(&self) -> HeatRamp {
        HeatRamp {
            fill: [
                opaque(self.heat_fill[0]),
                opaque(self.heat_fill[1]),
                opaque(self.heat_fill[2]),
            ],
            border_base: opaque(self.heat_border_base),
            border_hot: opaque(self.heat_border_hot),
        }
    }

    #[inline]
    pub fn pod_color(&self, severity: Severity) -> Rgba {
        gpui::rgb(self.pod_severity[severity_index(severity)])
    }

    #[inline]
    pub fn workload_colors(&self, severity: Severity) -> (Rgba, Rgba) {
        let at = severity_index(severity);
        let (border, alpha) = self.workload_border[at];
        (
            gpui::rgb(self.workload_fill[at]),
            gpui::rgb(border).alpha(alpha),
        )
    }

    #[inline]
    pub fn kind_color(&self, kind: KindId) -> Rgba {
        gpui::rgb(
            self.kind_colors
                .get(kind.0 as usize)
                .copied()
                .unwrap_or(self.unknown_kind),
        )
    }

    #[inline]
    pub fn tool_color(&self, tool: ToolId) -> Rgba {
        gpui::rgb(
            self.tool_colors
                .get(tool.0 as usize)
                .copied()
                .unwrap_or(self.unknown_kind),
        )
    }

    pub fn heat_color(&self, frac: f32) -> Rgba {
        self.heat_ramp().color(frac)
    }

    pub fn heat_border(&self, frac: f32) -> Rgba {
        self.heat_ramp().border(frac)
    }
}

#[inline(always)]
const fn severity_index(severity: Severity) -> usize {
    match severity {
        Severity::Ok => 0,
        Severity::Warn => 1,
        Severity::Err => 2,
        Severity::Unknown => 3,
    }
}

pub fn scale_alpha(mut c: Rgba, a: f32) -> Rgba {
    c.a *= a;
    c
}

pub fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    if t <= 0.0 {
        return a;
    }
    if t >= 1.0 {
        return b;
    }
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

fn channel(c: u32, shift: u32) -> f32 {
    ((c >> shift) & 0xff) as f32 / 255.0
}

// The three places themes and typography meet gpui: the app publishes what it
// resolved, views read it per render, the paint path reads it once per frame.
// Absent (headless tests, early startup) each falls back to a default, so no
// code path can panic on a missing global.
pub struct ActiveTheme(pub Arc<Theme>);

impl gpui::Global for ActiveTheme {}

pub struct ActiveTypography(pub Typography);

impl gpui::Global for ActiveTypography {}

/// The registry is a global too, because the settings schema completes theme
/// names and has to offer the ones a user dropped in `themes/`, not only the
/// ones this build compiled.
pub struct ActiveRegistry(pub Arc<ThemeRegistry>);

impl gpui::Global for ActiveRegistry {}

pub fn active(cx: &gpui::App) -> &Arc<Theme> {
    match cx.try_global::<ActiveTheme>() {
        Some(active) => &active.0,
        None => default_theme(),
    }
}

pub fn typography(cx: &gpui::App) -> &Typography {
    match cx.try_global::<ActiveTypography>() {
        Some(active) => &active.0,
        None => default_typography(),
    }
}

pub fn registry(cx: &gpui::App) -> &Arc<ThemeRegistry> {
    match cx.try_global::<ActiveRegistry>() {
        Some(active) => &active.0,
        None => default_registry(),
    }
}

/// The theme a session falls back to when nothing has been published yet.
pub fn default_theme() -> &'static Arc<Theme> {
    static DEFAULT: std::sync::OnceLock<Arc<Theme>> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(K10S_DARK.clone()))
}

pub fn default_typography() -> &'static Typography {
    static DEFAULT: std::sync::OnceLock<Typography> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(Typography::default)
}

fn default_registry() -> &'static Arc<ThemeRegistry> {
    static DEFAULT: std::sync::OnceLock<Arc<ThemeRegistry>> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(ThemeRegistry::builtin()))
}

#[cfg(test)]
mod contrast;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_kind_and_tool_has_a_colour() {
        for theme in ThemeRegistry::builtin().themes() {
            for kind in 0..BUILTIN_KIND_COUNT {
                assert_ne!(theme.map.kind_color(KindId(kind)).a, 0.0, "{}", theme.name);
            }
            for tool in 0..BUILTIN_TOOL_COUNT {
                assert_ne!(theme.map.tool_color(ToolId(tool)).a, 0.0, "{}", theme.name);
            }
            let unknown = theme.map.kind_color(KindId(9_999));
            assert_eq!(unknown, gpui::rgb(theme.map.unknown_kind));
        }
    }

    #[test]
    fn the_heat_ramp_is_clamped_at_both_ends() {
        let map = &K10S_DARK.map;
        assert_eq!(map.heat_color(-1.0), map.heat_color(0.0));
        assert_eq!(map.heat_color(2.0), map.heat_color(1.0));
        assert_eq!(map.heat_border(9.0), map.heat_border(1.0));
    }

    #[test]
    fn the_type_ladder_steps_down_from_the_ui_size_and_never_collapses() {
        let default = Typography::default();
        assert_eq!(default.ui_size, 14.0);
        assert_eq!(default.small(), 12.0);
        assert_eq!(default.xsmall(), 10.0);
        assert_eq!(
            default.line_height(),
            20.0,
            "the shipped density is Zed's 15/20 buffer row"
        );

        let tiny = Typography {
            ui_size: 8.0,
            buffer_size: 8.0,
            buffer_line_height: 1.0,
            ..Typography::default()
        };
        assert_eq!(tiny.xsmall(), 6.0, "the ladder floors instead of inverting");
        assert_eq!(tiny.line_height(), 8.0);
    }

    #[test]
    fn the_map_ladder_scales_with_the_ui_size_and_never_inverts() {
        let map = Typography::default().map();
        assert_eq!(
            (map.region_min, map.workload, map.pod, map.sat_detail),
            (14.0, 12.0, 10.0, 8.5)
        );
        assert!(map.region_max > map.region_min);

        // Every step of the ladder must stay ordered and above the legibility
        // floor at both ends of the settable UI range, or scaling the interface
        // down turns the map into six-pixel mush in one direction and puts a
        // pod label over its neighbour in the other.
        for ui_size in [*UI_FONT_SIZE_RANGE.start(), 14.0, *UI_FONT_SIZE_RANGE.end()] {
            let map = Typography {
                ui_size,
                ..Typography::default()
            }
            .map();
            assert!(
                map.sat_detail >= 6.0 && map.pod >= 6.0 && map.sat >= 6.0,
                "ui {ui_size}: {map:?} drops below the legibility floor"
            );
            assert!(
                map.sat_detail <= map.pod && map.pod <= map.workload,
                "ui {ui_size}: {map:?} is out of order"
            );
            assert!(map.region_min <= map.region_max);
        }
    }

    #[test]
    fn quantize_lands_on_the_ladder_and_never_above_what_was_asked() {
        for step in SIZE_STEPS {
            assert_eq!(quantize(step), step, "a ladder value must be its own step");
        }
        assert_eq!(quantize(0.0), SIZE_STEPS[0], "below the ladder floors");
        assert_eq!(quantize(-5.0), SIZE_STEPS[0]);
        assert_eq!(
            quantize(1_000.0),
            SIZE_STEPS[SIZE_STEPS.len() - 1],
            "above the ladder saturates"
        );

        // The property that matters for the sprite atlas and the shaped-line
        // cache: a size that varies smoothly produces a small, fixed set of
        // distinct answers, and never one larger than it was handed.
        let mut seen: Vec<f32> = Vec::new();
        let mut asked = 0.0f32;
        while asked <= 120.0 {
            let got = quantize(asked);
            assert!(
                got <= asked.max(SIZE_STEPS[0]),
                "quantize({asked}) rounded up to {got}"
            );
            if !seen.contains(&got) {
                seen.push(got);
            }
            asked += 0.05;
        }
        assert_eq!(
            seen.len(),
            SIZE_STEPS.len(),
            "a sweep of the whole range must reach every step and invent none"
        );
    }
}
