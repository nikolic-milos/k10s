//! The themes this binary ships with.
//!
//! `k10s-dark` and `k10s-light` are the brand, derived rather than sampled:
//! the wordmark's Pantone 2738 C is `#1800ad`, which measures 12.79:1 on white
//! and 1.64:1 on black. That single pair of numbers is the whole argument for
//! the brand rule "black and blue only on white", and it is also why the dark
//! theme cannot use the brand blue as an accent. The dark accent is the same
//! hue lifted in lightness -- `hsl(248.3, 100%, 72%)` = `#8470ff`, 5.03:1 on
//! the editor background -- so it is a stated derivation of the brand colour
//! rather than a second brand colour. The neutrals of both themes sit on hue
//! 248 at 8% saturation, which is what makes the greys read as belonging to
//! the mark without reading as blue.
//!
//! `one-dark` is Zed's bundled theme at the pinned fork revision, transcribed
//! from `assets/themes/one/one.json` rather than approximated. Its values are
//! pinned by a test and must not be "improved": several of them do not clear
//! WCAG AA, and `contrast.rs` records each shortfall with its measured ratio
//! instead of hiding it.

use std::sync::LazyLock;

use crate::{Appearance, MapTheme, ShellTheme, SyntaxTheme, Theme};

// Vendor marks: the colour a tool is recognised by is that tool's property,
// not ours, so the dark themes carry the logo values unchanged. Every one of
// them clears 3:1 on the k10s-dark canvas already.
const VENDOR_TOOLS: [u32; 35] = [
    0x9e96b8, 0x017cee, 0xef7b4d, 0x1287b1, 0xffcc01, 0xf24c53, 0x8cb3bf, 0xac6199, 0x419eda,
    0x49bda5, 0x0e83c8, 0x5468ff, 0xf46800, 0x60b932, 0x466bb0, 0x66cfe3, 0xd24939, 0x9c9a9b,
    0xafafaf, 0x8cb3bf, 0x326ce5, 0x8ca4ab, 0xc72e49, 0x47a248, 0x4479a1, 0x27aae1, 0x009639,
    0x8c8c8c, 0x4169e1, 0xe6522c, 0xff6600, 0xff4438, 0x8c8c8c, 0x24a1c1, 0xffec6e,
];

// The same marks on a white canvas, where most of them vanish -- Airflow's
// yellow measures 1.09:1 on the light map background. Each was darkened along
// its own hue and saturation, and only as far as it had to go, so the tool
// stays recognisable and stays visible; the fourteen that already cleared are
// untouched.
const VENDOR_TOOLS_ON_LIGHT: [u32; 35] = [
    0x8d84ac, 0x017cee, 0xeb5a21, 0x1287b1, 0xa78500, 0xf24c53, 0x5a91a2, 0xac6199, 0x298fd1,
    0x379985, 0x0e83c8, 0x5468ff, 0xe56100, 0x509b2a, 0x466bb0, 0x2096ac, 0xd24939, 0x8c898a,
    0x898989, 0x5a91a2, 0x326ce5, 0x708e97, 0xc72e49, 0x449b45, 0x4479a1, 0x1b92c4, 0x009639,
    0x898989, 0x4169e1, 0xe6522c, 0xe85d00, 0xff3f33, 0x898989, 0x2194b2, 0x9c8700,
];

/// The default. Neutrals on hue 248 at 8% saturation, from `#141417` at the
/// editor to `#ececef` at full-strength text, with the lifted brand violet
/// `#8470ff` as the only accent.
pub static K10S_DARK: LazyLock<Theme> = LazyLock::new(|| Theme {
    name: "k10s-dark".into(),
    appearance: Appearance::Dark,
    map: MapTheme {
        bg: 0x141417,
        hex_line: 0x2a2930,
        ns_fill: 0x1c1b21,
        ns_border: 0x413f4c,
        edge: 0x86b6f0,
        hud_bg: 0x1f1e24,
        hud_text: 0xc6c4d2,
        curve_core: 0xececef,
        curve_core_alpha: 0.72,
        curve_glow: 0x8470ff,
        curve_glow_alpha: 0.18,
        card_header_fill: 0x2a2930,
        region_label: 0xafa9d1,
        workload_label: 0xbfb9da,
        sat_label: 0xcecae2,
        sat_detail_label: 0xb1abce,
        pod_label: 0xb0abc9,
        unknown_kind: 0x9391a4,
        kind_colors: [
            0x86b6f0, 0xb98cf0, 0x71c9d4, 0x8ad2a7, 0xdcae76, 0x62c98d, 0x9391a4, 0xc79bf2,
            0x9fd68c, 0x7bb4f5, 0xe0c98d, 0x64c9d6, 0xb5b2c2,
        ],
        tool_colors: VENDOR_TOOLS,
        pod_severity: [0x7fd3a0, 0xdcb46e, 0xef7c8a, 0x9391a4],
        workload_fill: [0x1b2822, 0x2a2419, 0x2c1d21, 0x22212a],
        workload_border: [
            (0x7fd3a0, 0.55),
            (0xdcb46e, 0.75),
            (0xef7c8a, 0.85),
            (0x9391a4, 0.6),
        ],
        heat_fill: [0x1b2822, 0x4a3d1c, 0x4b2027],
        heat_border_base: 0x413f4c,
        heat_border_hot: 0xef7c8a,
    },
    shell: ShellTheme {
        background: 0x1f1e24,
        surface_background: 0x19191d,
        elevated_surface_background: 0x25242b,
        editor_background: 0x141417,
        toolbar_background: 0x141417,
        panel_background: 0x19191d,
        status_bar_background: 0x1f1e24,
        tab_bar_background: 0x19191d,
        tab_inactive_background: 0x19191d,
        tab_active_background: 0x141417,
        border: 0x37363f,
        border_variant: 0x2a2930,
        border_focused: 0x6551ec,
        element_background: 0x25242b,
        element_hover: 0x2e2d34,
        element_active: 0x3e3d48,
        element_selected: 0x3e3d48,
        text: 0xececef,
        text_muted: 0xaeacb9,
        text_placeholder: 0x8e8b9c,
        text_accent: 0x8470ff,
        editor_foreground: 0xd2d0dc,
        search_match_background: (0x8470ff, 0.40),
        terminal_background: 0x141417,
        terminal_foreground: 0xd2d0dc,
        terminal_bright_foreground: 0xf4f3f7,
        terminal_dim_foreground: 0x78758a,
        terminal_ansi: [
            0x1f1e24, 0xef7c8a, 0x7fd3a0, 0xdcb46e, 0x86a9ff, 0xb98cf0, 0x64c9d6, 0xc6c4d2,
            0x6c6a7d, 0xf59aa5, 0x9fe2b9, 0xefcd8e, 0xa9c3ff, 0xd0aef8, 0x8fdde8, 0xf7f6fa,
        ],
        terminal_ansi_dim: [
            0x2a2930, 0x9a4f59, 0x51876a, 0x8f7448, 0x566ba6, 0x775a9b, 0x40818b, 0x807e91,
        ],
        cursor: 0x8470ff,
        success: 0x7fd3a0,
        warning: 0xdcb46e,
        error: 0xef7c8a,
    },
    syntax: SyntaxTheme {
        property: 0xa294ff,
        string: 0x8ad2a7,
        number: 0xdcae76,
        boolean: 0xdcae76,
        constant: 0xe0c98d,
        comment: 0x8b889c,
        label: 0x86b6f0,
        type_name: 0x71c9d4,
        attribute: 0x86b6f0,
        punctuation: 0xb5b2c2,
        punctuation_special: 0xe895a1,
        line_number: 0x817f92,
        active_line_number: 0xdedce6,
        active_line_background: (0x25242b, 0.6),
        selection_background: (0x8470ff, 0.28),
    },
});

/// The rules read literally: `#ffffff` behind the text, `#000000` in it, and
/// the brand blue itself as the accent, which is the one surface the brand
/// permits it on.
pub static K10S_LIGHT: LazyLock<Theme> = LazyLock::new(|| Theme {
    name: "k10s-light".into(),
    appearance: Appearance::Light,
    map: MapTheme {
        bg: 0xf4f4f7,
        hex_line: 0xd7d7e3,
        ns_fill: 0xfafafb,
        ns_border: 0xbdbdcc,
        edge: 0x1a44c0,
        hud_bg: 0xffffff,
        hud_text: 0x3f3f4c,
        curve_core: 0x241a9c,
        curve_core_alpha: 0.72,
        curve_glow: 0x1800ad,
        curve_glow_alpha: 0.14,
        card_header_fill: 0xe7e7ee,
        region_label: 0x35353f,
        workload_label: 0x3f3f4c,
        sat_label: 0x35353f,
        sat_detail_label: 0x4a4a57,
        pod_label: 0x4a4a57,
        unknown_kind: 0x64647a,
        kind_colors: [
            0x1a44c0, 0x8b2bb0, 0x0a6570, 0x136b3f, 0x8a4b12, 0x0f6b3a, 0x64647a, 0x7b2fa0,
            0x2c6b20, 0x14509e, 0x6d4b09, 0x076a75, 0x3f3f4c,
        ],
        tool_colors: VENDOR_TOOLS_ON_LIGHT,
        pod_severity: [0x136b3f, 0x7a4a00, 0xb3261e, 0x64647a],
        workload_fill: [0xe4f1e9, 0xf6ecd9, 0xfbe7e6, 0xececf2],
        workload_border: [
            (0x136b3f, 0.55),
            (0x7a4a00, 0.75),
            (0xb3261e, 0.85),
            (0x64647a, 0.6),
        ],
        heat_fill: [0xe4f1e9, 0xf3dfae, 0xf6c9c5],
        heat_border_base: 0xbdbdcc,
        heat_border_hot: 0xb3261e,
    },
    shell: ShellTheme {
        background: 0xf4f4f7,
        surface_background: 0xfafafb,
        elevated_surface_background: 0xffffff,
        editor_background: 0xffffff,
        toolbar_background: 0xffffff,
        panel_background: 0xfafafb,
        status_bar_background: 0xf4f4f7,
        tab_bar_background: 0xf4f4f7,
        tab_inactive_background: 0xf4f4f7,
        tab_active_background: 0xffffff,
        border: 0xd7d7e0,
        border_variant: 0xe7e7ee,
        border_focused: 0x1800ad,
        element_background: 0xf4f4f7,
        element_hover: 0xededf3,
        element_active: 0xe0e0ea,
        element_selected: 0xe0e0ea,
        text: 0x000000,
        text_muted: 0x4a4a57,
        text_placeholder: 0x716e87,
        text_accent: 0x1800ad,
        editor_foreground: 0x17171d,
        search_match_background: (0x1800ad, 0.22),
        terminal_background: 0xffffff,
        terminal_foreground: 0x17171d,
        terminal_bright_foreground: 0x000000,
        terminal_dim_foreground: 0x6d6a81,
        terminal_ansi: [
            0x17171d, 0xb3261e, 0x136b3f, 0x7a4a00, 0x1a44c0, 0x8b2bb0, 0x076a75, 0x4a4a57,
            0x5c5a70, 0x8f1d18, 0x0d5230, 0x5c3800, 0x1800ad, 0x6d1f8a, 0x05545c, 0x000000,
        ],
        terminal_ansi_dim: [
            0x9d9dab, 0xd97b74, 0x63a683, 0xb08c4d, 0x7a90d6, 0xb87ecc, 0x64a5ac, 0x8f8f9c,
        ],
        cursor: 0x1800ad,
        success: 0x136b3f,
        warning: 0x7a4a00,
        error: 0xb3261e,
    },
    syntax: SyntaxTheme {
        property: 0x241a9c,
        string: 0x136b3f,
        number: 0x8a4b12,
        boolean: 0x8a4b12,
        constant: 0x6d4b09,
        comment: 0x5c5a70,
        label: 0x0f5aa8,
        type_name: 0x0a6570,
        attribute: 0x0f5aa8,
        punctuation: 0x3f3f4c,
        punctuation_special: 0xa02a3a,
        line_number: 0x6d6a81,
        active_line_number: 0x17171d,
        active_line_background: (0xededf3, 0.9),
        selection_background: (0x1800ad, 0.18),
    },
});

/// Zed's bundled One Dark at the pinned fork revision. The shell values below
/// deliberately mirror `assets/themes/one/one.json`; the map keeps its domain
/// encodings but borrows the same editor, panel and border tokens so the two
/// halves read as one workspace.
pub static ONE_DARK: LazyLock<Theme> = LazyLock::new(|| Theme {
    name: "one-dark".into(),
    appearance: Appearance::Dark,
    map: MapTheme {
        bg: 0x282c33,
        hex_line: 0x363c46,
        ns_fill: 0x2e343e,
        ns_border: 0x464b57,
        edge: 0x74ade8,
        hud_bg: 0x2f343e,
        hud_text: 0xacb2be,
        curve_core: 0xdce0e5,
        curve_core_alpha: 0.72,
        curve_glow: 0x74ade8,
        curve_glow_alpha: 0.16,
        card_header_fill: 0x363c46,
        region_label: 0xacb2be,
        workload_label: 0xc8ccd4,
        sat_label: 0xdce0e5,
        sat_detail_label: 0xacb2be,
        pod_label: 0xa9afbc,
        unknown_kind: 0x878a98,
        kind_colors: [
            0x74ade8, 0xb477cf, 0x6eb4bf, 0xa1c181, 0xdec184, 0x27a657, 0x464b57, 0xc678dd,
            0x98c379, 0x61afef, 0xe5c07b, 0x56b6c2, 0xa9afbc,
        ],
        tool_colors: VENDOR_TOOLS,
        pod_severity: [0xa1c181, 0xdec184, 0xd07277, 0x878a98],
        workload_fill: [0x2f3b35, 0x403b31, 0x3f3035, 0x30343d],
        workload_border: [
            (0xa1c181, 0.55),
            (0xdec184, 0.75),
            (0xd07277, 0.85),
            (0x878a98, 0.6),
        ],
        // The warm stop used to be `#665731`, two ramp steps lighter than
        // both of its neighbours, so a warming region flashed brighter than a
        // hot one and no label could clear 4.5:1 on it. Zed has no map, so
        // these three are ours to correct; the shell and syntax values above
        // are the transcription and stay pinned.
        heat_fill: [0x2f3b35, 0x4c4124, 0x5a3035],
        heat_border_base: 0x464b57,
        heat_border_hot: 0xd07277,
    },
    shell: ShellTheme {
        background: 0x3b414d,
        surface_background: 0x2f343e,
        elevated_surface_background: 0x2f343e,
        editor_background: 0x282c33,
        toolbar_background: 0x282c33,
        panel_background: 0x2f343e,
        status_bar_background: 0x3b414d,
        tab_bar_background: 0x2f343e,
        tab_inactive_background: 0x2f343e,
        tab_active_background: 0x282c33,
        border: 0x464b57,
        border_variant: 0x363c46,
        border_focused: 0x47679e,
        element_background: 0x2e343e,
        element_hover: 0x363c46,
        element_active: 0x454a56,
        element_selected: 0x454a56,
        text: 0xdce0e5,
        text_muted: 0xa9afbc,
        text_placeholder: 0x878a98,
        text_accent: 0x74ade8,
        editor_foreground: 0xacb2be,
        search_match_background: (0x74ade8, 0.40),
        terminal_background: 0x282c34,
        terminal_foreground: 0xabb2bf,
        terminal_bright_foreground: 0xdce0e5,
        terminal_dim_foreground: 0x636d83,
        terminal_ansi: [
            0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
            0x636d83, 0xea858b, 0xaad581, 0xffd885, 0x85c1ff, 0xd398eb, 0x6ed5de, 0xfafafa,
        ],
        terminal_ansi_dim: [
            0x3b3f4a, 0xa7545a, 0x6d8f59, 0xb8985b, 0x457cad, 0x8d54a0, 0x3c818a, 0x8f969b,
        ],
        cursor: 0x74ade8,
        success: 0xa1c181,
        warning: 0xdec184,
        error: 0xd07277,
    },
    syntax: SyntaxTheme {
        property: 0xd07277,
        string: 0xa1c181,
        number: 0xbf956a,
        boolean: 0xbf956a,
        constant: 0xdfc184,
        comment: 0x5d636f,
        label: 0x74ade8,
        type_name: 0x6eb4bf,
        attribute: 0x74ade8,
        punctuation: 0xb2b9c6,
        punctuation_special: 0xb1574b,
        line_number: 0x4e5a5f,
        active_line_number: 0xd0d4da,
        active_line_background: (0x2f343e, 0.75),
        selection_background: (0x74ade8, 0.24),
    },
});

/// The family the registry starts from. `k10s-dark` is first because a family's
/// first theme of an appearance is the one the registry hands back when a
/// setting asks for "the dark one".
pub fn builtin_family() -> crate::ThemeFamily {
    crate::ThemeFamily {
        name: "k10s".into(),
        author: "k10s".into(),
        themes: vec![
            std::sync::Arc::new(K10S_DARK.clone()),
            std::sync::Arc::new(K10S_LIGHT.clone()),
            std::sync::Arc::new(ONE_DARK.clone()),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_tokens_are_the_pinned_zed_one_dark_theme() {
        let shell = ONE_DARK.shell;
        assert_eq!(shell.background, 0x3b414d);
        assert_eq!(shell.editor_background, 0x282c33);
        assert_eq!(shell.panel_background, 0x2f343e);
        assert_eq!(shell.tab_active_background, 0x282c33);
        assert_eq!(shell.border, 0x464b57);
        assert_eq!(shell.text, 0xdce0e5);
        assert_eq!(shell.text_muted, 0xa9afbc);
        assert_eq!(shell.terminal_background, 0x282c34);
        assert_eq!(shell.terminal_foreground, 0xabb2bf);
        assert_eq!(shell.terminal_bright_foreground, 0xdce0e5);
        assert_eq!(shell.terminal_dim_foreground, 0x636d83);
        assert_eq!(shell.terminal_ansi[9], 0xea858b);
        assert_eq!(shell.terminal_ansi_dim[4], 0x457cad);
        assert_eq!(ONE_DARK.appearance, Appearance::Dark);
    }

    #[test]
    fn syntax_tokens_are_the_pinned_zed_one_dark_theme() {
        let syntax = ONE_DARK.syntax;
        assert_eq!(syntax.property, 0xd07277);
        assert_eq!(syntax.string, 0xa1c181);
        assert_eq!(syntax.number, 0xbf956a);
        assert_eq!(syntax.boolean, 0xbf956a);
        assert_eq!(syntax.constant, 0xdfc184);
        assert_eq!(syntax.comment, 0x5d636f);
        assert_eq!(syntax.label, 0x74ade8);
        assert_eq!(syntax.type_name, 0x6eb4bf);
        assert_eq!(syntax.attribute, 0x74ade8);
        assert_eq!(syntax.punctuation, 0xb2b9c6);
        assert_eq!(syntax.punctuation_special, 0xb1574b);
        assert_eq!(syntax.line_number, 0x4e5a5f);
        assert_eq!(syntax.active_line_number, 0xd0d4da);
        assert_eq!(syntax.active_line_background, (0x2f343e, 0.75));
        assert_eq!(syntax.selection_background, (0x74ade8, 0.24));
    }

    #[test]
    fn the_brand_anchors_are_the_measured_ones() {
        assert_eq!(
            K10S_LIGHT.shell.text_accent, 0x1800ad,
            "Pantone 2738 C, and the light theme is the only surface it is allowed on"
        );
        assert_eq!(
            K10S_DARK.shell.text_accent, 0x8470ff,
            "hsl(248.3, 100%, 72%): the same hue, lifted"
        );
        assert_eq!(K10S_LIGHT.shell.editor_background, 0xffffff);
        assert_eq!(K10S_LIGHT.shell.text, 0x000000);
        assert_eq!(K10S_DARK.shell.editor_background, 0x141417);
        assert_eq!(K10S_DARK.appearance, Appearance::Dark);
        assert_eq!(K10S_LIGHT.appearance, Appearance::Light);
    }

    #[test]
    fn a_vendor_mark_is_only_moved_when_the_canvas_hides_it() {
        assert_eq!(
            VENDOR_TOOLS[20], VENDOR_TOOLS_ON_LIGHT[20],
            "Kubernetes blue already clears 3:1 on white, so it is untouched"
        );
        assert_ne!(
            VENDOR_TOOLS[4], VENDOR_TOOLS_ON_LIGHT[4],
            "Airflow's yellow measures 1.09:1 on the light canvas and must move"
        );
    }
}
