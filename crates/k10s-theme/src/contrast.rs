//! The readability of every shipped theme, as a test.
//!
//! A theme that cannot be read is a bug no compiler catches and no reviewer
//! reliably catches either, because the eye adapts to whatever is on screen.
//! So the relative-luminance formula from WCAG 2.1 lives here and every
//! foreground/background pair a shipped theme puts on screen is measured
//! against a floor:
//!
//! * `TEXT` (4.5:1) -- anything a person reads: body text, muted and
//!   placeholder labels, terminal output, every syntax token.
//! * `UI` (3:1) -- the non-text marks that carry meaning: the focus ring, the
//!   caret, severity dots, the glyph colour that says which kind a node is.
//!   This is WCAG 1.4.11's threshold and its scope.
//! * `SEPARATOR` (1.2:1) -- rules and grid lines. 1.4.11 does not cover
//!   decoration, and a 3:1 hairline is not a border, it is a fence; what a
//!   separator must not be is invisible, which is exactly what this catches.
//!
//! Dim is out of that scope on purpose. SGR 2 is defined as recessive, so
//! `terminal_dim_foreground` and `terminal_ansi_dim` -- like ansi slots 0 and 8
//! below -- cannot be held to a reading floor without turning them into
//! something that is not dim. What a theme owes them is that they stay dimmer
//! than the foreground they recede from, which is a palette relation rather
//! than a WCAG one.
//!
//! `one-dark` is transcribed from Zed and thirty-three of its pairs do not
//! clear their floor -- most of them vendor tool marks, which that theme
//! carries at their logo values on canvases the logos were never designed
//! for. Rather than quietly excluding it, each shortfall is recorded
//! below with the ratio it actually measures. A waiver that drifts fails, and a
//! waiver that starts passing fails too, so the list can only shrink. The two
//! brand themes carry none, and a test asserts that separately.

use crate::{Theme, ThemeRegistry};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Floor {
    Text,
    Ui,
    Separator,
}

impl Floor {
    fn value(self) -> f64 {
        match self {
            Floor::Text => 4.5,
            Floor::Ui => 3.0,
            Floor::Separator => 1.2,
        }
    }
}

fn channel(value: u32) -> f64 {
    let c = value as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 2.1 relative luminance of an `0xRRGGBB` colour.
fn luminance(color: u32) -> f64 {
    0.2126 * channel((color >> 16) & 0xff)
        + 0.7152 * channel((color >> 8) & 0xff)
        + 0.0722 * channel(color & 0xff)
}

fn contrast(a: u32, b: u32) -> f64 {
    let (hi, lo) = {
        let (x, y) = (luminance(a), luminance(b));
        if x >= y { (x, y) } else { (y, x) }
    };
    (hi + 0.05) / (lo + 0.05)
}

struct Pair {
    label: String,
    ratio: f64,
    floor: Floor,
}

fn audit(theme: &Theme) -> Vec<Pair> {
    let mut pairs = Vec::new();
    let mut check = |label: String, fg: u32, bg: u32, floor: Floor| {
        pairs.push(Pair {
            label,
            ratio: contrast(fg, bg),
            floor,
        });
    };
    let shell = &theme.shell;
    let editor = shell.editor_background;
    let panel = shell.panel_background;
    let elevated = shell.elevated_surface_background;

    check("text on editor".into(), shell.text, editor, Floor::Text);
    check("text on panel".into(), shell.text, panel, Floor::Text);
    check("text on elevated".into(), shell.text, elevated, Floor::Text);
    check(
        "text on selected".into(),
        shell.text,
        shell.element_selected,
        Floor::Text,
    );
    check(
        "muted on editor".into(),
        shell.text_muted,
        editor,
        Floor::Text,
    );
    check(
        "muted on panel".into(),
        shell.text_muted,
        panel,
        Floor::Text,
    );
    check(
        "placeholder on editor".into(),
        shell.text_placeholder,
        editor,
        Floor::Text,
    );
    check(
        "placeholder on elevated".into(),
        shell.text_placeholder,
        elevated,
        Floor::Text,
    );
    check(
        "accent on editor".into(),
        shell.text_accent,
        editor,
        Floor::Text,
    );
    check(
        "accent on panel".into(),
        shell.text_accent,
        panel,
        Floor::Text,
    );
    check(
        "editor_foreground on editor".into(),
        shell.editor_foreground,
        editor,
        Floor::Text,
    );
    check(
        "success on editor".into(),
        shell.success,
        editor,
        Floor::Text,
    );
    check(
        "warning on editor".into(),
        shell.warning,
        editor,
        Floor::Text,
    );
    check("error on editor".into(), shell.error, editor, Floor::Text);

    let term = shell.terminal_background;
    check(
        "terminal fg".into(),
        shell.terminal_foreground,
        term,
        Floor::Text,
    );
    check(
        "terminal bright fg".into(),
        shell.terminal_bright_foreground,
        term,
        Floor::Text,
    );
    // Slots 0 and 8 are the palette's two blacks: on a dark theme they *are*
    // the background, and SGR 2 dim is defined as recessive, so neither can be
    // held to a reading floor without making it something other than dim.
    for (index, color) in shell.terminal_ansi.iter().enumerate() {
        if index == 0 || index == 8 {
            continue;
        }
        check(format!("ansi[{index}]"), *color, term, Floor::Text);
    }

    let syntax = &theme.syntax;
    for (name, color) in [
        ("property", syntax.property),
        ("string", syntax.string),
        ("number", syntax.number),
        ("boolean", syntax.boolean),
        ("constant", syntax.constant),
        ("comment", syntax.comment),
        ("label", syntax.label),
        ("type_name", syntax.type_name),
        ("attribute", syntax.attribute),
        ("punctuation", syntax.punctuation),
        ("punctuation_special", syntax.punctuation_special),
        ("line_number", syntax.line_number),
        ("active_line_number", syntax.active_line_number),
    ] {
        check(format!("syntax {name}"), color, editor, Floor::Text);
    }

    check(
        "border_focused on editor".into(),
        shell.border_focused,
        editor,
        Floor::Ui,
    );
    check("cursor on editor".into(), shell.cursor, editor, Floor::Ui);
    check(
        "SEP border on editor".into(),
        shell.border,
        editor,
        Floor::Separator,
    );
    check(
        "SEP border on panel".into(),
        shell.border,
        panel,
        Floor::Separator,
    );
    check(
        "SEP border_variant on editor".into(),
        shell.border_variant,
        editor,
        Floor::Separator,
    );

    let map = &theme.map;
    check(
        "map hud_text on hud_bg".into(),
        map.hud_text,
        map.hud_bg,
        Floor::Text,
    );
    check("map edge on bg".into(), map.edge, map.bg, Floor::Ui);
    check(
        "map unknown_kind on bg".into(),
        map.unknown_kind,
        map.bg,
        Floor::Ui,
    );
    check(
        "map curve_core on bg".into(),
        map.curve_core,
        map.bg,
        Floor::Ui,
    );
    check(
        "map heat_border_hot on bg".into(),
        map.heat_border_hot,
        map.bg,
        Floor::Ui,
    );
    check(
        "SEP map ns_border on map bg".into(),
        map.ns_border,
        map.bg,
        Floor::Separator,
    );
    check(
        "SEP map hex_line on map bg".into(),
        map.hex_line,
        map.bg,
        Floor::Separator,
    );
    // Non-text marks a person has to make out against the canvas: the kind
    // glyph a card is identified by, and the two rings that say what the
    // pointer is on and what is selected.
    check("map wl_icon on bg".into(), map.wl_icon, map.bg, Floor::Ui);
    check(
        "map wl_icon on ns_fill".into(),
        map.wl_icon,
        map.ns_fill,
        Floor::Ui,
    );
    check(
        "map hover_ring on bg".into(),
        map.hover_ring,
        map.bg,
        Floor::Ui,
    );
    check(
        "map selection_ring on bg".into(),
        map.selection_ring,
        map.bg,
        Floor::Ui,
    );
    for (index, color) in map.kind_colors.iter().enumerate() {
        check(format!("map kind[{index}]"), *color, map.bg, Floor::Ui);
    }
    // A tool glyph is painted on the island fill (the medallion) and on the
    // card header (the detailed card), never on the raw map background. The
    // painted header is `card_header_fill` shaded toward `bg`, so measuring
    // against the unshaded fill is the conservative bound in both appearances.
    for (index, color) in map.tool_colors.iter().enumerate() {
        check(
            format!("map tool[{index}] on ns_fill"),
            *color,
            map.ns_fill,
            Floor::Ui,
        );
        check(
            format!("map tool[{index}] on header"),
            *color,
            map.card_header_fill,
            Floor::Ui,
        );
    }
    for (index, color) in map.pod_severity.iter().enumerate() {
        check(
            format!("map severity[{index}] on fill"),
            *color,
            map.workload_fill[index],
            Floor::Ui,
        );
        check(
            format!("map severity[{index}] on bg"),
            *color,
            map.bg,
            Floor::Ui,
        );
    }
    // A map label is read, so it is text, and the surface under it depends on
    // how healthy the thing it names is. Every surface each label can land on
    // is enumerated rather than reduced to the one that looked worst.
    check(
        "map region_label on ns_fill".into(),
        map.region_label,
        map.ns_fill,
        Floor::Text,
    );
    for (index, fill) in map.heat_fill.iter().enumerate() {
        check(
            format!("map region_label on heat_fill[{index}]"),
            map.region_label,
            *fill,
            Floor::Text,
        );
    }
    for (index, fill) in map.workload_fill.iter().enumerate() {
        check(
            format!("map workload_label on workload_fill[{index}]"),
            map.workload_label,
            *fill,
            Floor::Text,
        );
        check(
            format!("map pod_label on workload_fill[{index}]"),
            map.pod_label,
            *fill,
            Floor::Text,
        );
    }
    for (name, color) in [
        ("sat_label", map.sat_label),
        ("sat_detail_label", map.sat_detail_label),
    ] {
        check(
            format!("map {name} on ns_fill"),
            color,
            map.ns_fill,
            Floor::Text,
        );
        check(format!("map {name} on bg"), color, map.bg, Floor::Text);
    }
    pairs
}

// Zed's One Dark, measured. Every entry is a pair that does not clear its
// floor, pinned to the ratio it has today: the theme is a transcription and
// "fixing" it would make it a different theme, but the cost of importing it
// should be written down rather than felt.
const ONE_DARK_SHORTFALLS: &[(&str, f64)] = &[
    ("placeholder on editor", 4.08),
    ("placeholder on elevated", 3.64),
    ("error on editor", 4.24),
    ("ansi[1]", 4.38),
    ("syntax property", 4.24),
    ("syntax comment", 2.32),
    ("syntax punctuation_special", 2.89),
    ("syntax line_number", 1.97),
    ("border_focused on editor", 2.47),
    ("map kind[6]", 1.60),
    ("map tool[1] on header", 2.71),
    ("map tool[3] on header", 2.71),
    ("map tool[8] on ns_fill", 2.94),
    ("map tool[8] on header", 2.60),
    ("map tool[12] on header", 2.70),
    ("map tool[13] on ns_fill", 2.86),
    ("map tool[13] on header", 2.54),
    ("map tool[17] on ns_fill", 2.38),
    ("map tool[17] on header", 2.11),
    ("map tool[19] on ns_fill", 2.83),
    ("map tool[19] on header", 2.51),
    ("map tool[24] on ns_fill", 2.63),
    ("map tool[24] on header", 2.33),
    ("map tool[28] on ns_fill", 2.33),
    ("map tool[28] on header", 2.07),
    ("map tool[30] on ns_fill", 2.68),
    ("map tool[30] on header", 2.38),
    ("map tool[32] on header", 2.87),
    ("map tool[33] on ns_fill", 2.08),
    ("map tool[33] on header", 1.84),
    ("map tool[35] on ns_fill", 2.58),
    ("map tool[35] on header", 2.29),
    ("map tool[36] on header", 2.96),
];

#[test]
fn the_luminance_helper_agrees_with_the_specification() {
    assert!((contrast(0xffffff, 0x000000) - 21.0).abs() < 0.001);
    assert!((contrast(0x000000, 0x000000) - 1.0).abs() < 0.001);
    assert!(
        (contrast(0x1800ad, 0xffffff) - 12.79).abs() < 0.01,
        "the brand blue on white is the number the whole palette is derived from: {}",
        contrast(0x1800ad, 0xffffff)
    );
    assert!((contrast(0x8470ff, 0x141417) - 5.03).abs() < 0.01);
}

#[test]
fn every_shipped_theme_is_readable() {
    for theme in ThemeRegistry::builtin().themes() {
        let waivers: &[(&str, f64)] = if theme.name == "one-dark" {
            ONE_DARK_SHORTFALLS
        } else {
            &[]
        };
        let mut unused: Vec<&str> = waivers.iter().map(|(label, _)| *label).collect();
        for pair in audit(theme) {
            match waivers.iter().find(|(label, _)| *label == pair.label) {
                Some((_, recorded)) => {
                    unused.retain(|label| *label != pair.label);
                    assert!(
                        pair.ratio < pair.floor.value(),
                        "{}: {} now measures {:.2} and clears its {:?} floor -- delete the \
                         waiver rather than keeping a passing pair on the list",
                        theme.name,
                        pair.label,
                        pair.ratio,
                        pair.floor
                    );
                    assert!(
                        (pair.ratio - recorded).abs() < 0.01,
                        "{}: {} was recorded at {recorded:.2} and now measures {:.2}",
                        theme.name,
                        pair.label,
                        pair.ratio
                    );
                }
                None => assert!(
                    pair.ratio >= pair.floor.value(),
                    "{}: {} measures {:.2}, below the {:?} floor of {}",
                    theme.name,
                    pair.label,
                    pair.ratio,
                    pair.floor,
                    pair.floor.value()
                ),
            }
        }
        assert!(
            unused.is_empty(),
            "{}: these waivers name pairs the audit no longer produces: {unused:?}",
            theme.name
        );
    }
}

#[test]
fn dim_recedes_from_the_foreground_it_is_dim_of() {
    // The relation that replaces a reading floor for SGR 2. A "dim" token that
    // drifted brighter than the text it recedes from would be unaudited by the
    // pairs above and invisible to review, while being exactly backwards.
    for theme in ThemeRegistry::builtin().themes() {
        let shell = &theme.shell;
        let background = shell.terminal_background;
        // Measured against the canvas rather than by luminance, because dim on
        // a light theme means closer to white and on a dark one closer to
        // black; what it always means is less separated from the background.
        let recedes =
            |dim: u32, normal: u32| contrast(dim, background) < contrast(normal, background);
        assert!(
            recedes(shell.terminal_dim_foreground, shell.terminal_foreground),
            "{}: terminal_dim_foreground is not dimmer than terminal_foreground",
            theme.name
        );
        for (index, (dim, normal)) in shell
            .terminal_ansi_dim
            .iter()
            .zip(shell.terminal_ansi.iter())
            .enumerate()
            // Slot 0 is the palette's black, which on a dark theme *is* the
            // background: its dim form has to come up off the canvas to stay a
            // colour at all, for the same reason the audit above skips it.
            .skip(1)
        {
            assert!(
                recedes(*dim, *normal),
                "{}: ansi_dim[{index}] is not dimmer than ansi[{index}]",
                theme.name
            );
        }
    }
}

#[test]
fn the_brand_themes_carry_no_waivers_at_all() {
    for theme in ThemeRegistry::builtin().themes() {
        if theme.name == "one-dark" {
            continue;
        }
        let audited = audit(theme);
        assert!(
            audited.len() > 70,
            "{}: {} pairs",
            theme.name,
            audited.len()
        );
        let tightest = audited
            .iter()
            .min_by(|a, b| {
                (a.ratio / a.floor.value())
                    .partial_cmp(&(b.ratio / b.floor.value()))
                    .expect("ratios are finite")
            })
            .expect("the audit is not empty");
        assert!(
            tightest.ratio >= tightest.floor.value(),
            "{}: {} measures {:.2}",
            theme.name,
            tightest.label,
            tightest.ratio
        );
    }
}
