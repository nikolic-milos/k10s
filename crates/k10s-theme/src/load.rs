//! Reading a theme file, purely.
//!
//! The shape is Zed's -- a family with an author and a list of variants, each
//! declaring an appearance and a partial `style` -- because that is the file a
//! person already has, and because a theme is published as a family even when
//! it holds one variant. Every key is optional: a variant inherits the built-in
//! default for its appearance and patches what it names, so a six-line file is
//! a working theme rather than a hundred-line transcription.
//!
//! Nothing here touches the filesystem, and nothing here returns an error. A
//! key this build does not know, a colour that is not a colour, an array of
//! the wrong length: each becomes one labelled note and leaves the inherited
//! value in place. That is the same contract the settings file has, for the
//! same reason -- a file a person edits by hand is a file that will be wrong
//! at some point, and being wrong must cost them a line, not a session.

use std::sync::Arc;

use serde_json::Value;

use crate::{Appearance, K10S_DARK, K10S_LIGHT, Theme, ThemeFamily};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LoadedFamily {
    pub family: Option<ThemeFamily>,
    pub notes: Vec<String>,
}

/// A validated `experimental.theme_overrides` block: the same style patch a
/// theme file carries, applied on top of whatever theme is active. It is
/// validated when the settings file is read -- so the notes arrive with the
/// rest of them -- and replayed silently onto each theme afterwards.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overrides {
    style: serde_json::Map<String, Value>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.style.is_empty()
    }

    pub fn apply(&self, theme: &mut Theme) {
        if self.style.is_empty() {
            return;
        }
        let mut discarded = Vec::new();
        apply_style(theme, &self.style, &mut discarded);
    }
}

/// Validate an overrides block against a scratch theme so every complaint is
/// made once, at the moment the settings file is read.
pub fn parse_overrides(value: &Value) -> (Overrides, Vec<String>) {
    let mut notes = Vec::new();
    let Some(style) = value.as_object() else {
        notes.push(format!(
            "settings field \"experimental.theme_overrides\" must be an object, got {value}; \
             ignored"
        ));
        return (Overrides::default(), notes);
    };
    let mut scratch = K10S_DARK.clone();
    apply_style(&mut scratch, style, &mut notes);
    (
        Overrides {
            style: style.clone(),
        },
        notes,
    )
}

pub fn parse_family(text: &str) -> LoadedFamily {
    let mut notes = Vec::new();
    let stripped = strip_jsonc(text);
    if stripped.trim().is_empty() {
        return LoadedFamily {
            family: None,
            notes,
        };
    }
    let value: Value = match serde_json::from_str(&stripped) {
        Ok(value) => value,
        Err(error) => {
            notes.push(format!("not valid JSON ({error}); the file is ignored"));
            return LoadedFamily {
                family: None,
                notes,
            };
        }
    };
    let Some(map) = value.as_object() else {
        notes.push("the top level of a theme file must be an object".to_string());
        return LoadedFamily {
            family: None,
            notes,
        };
    };

    let mut name = "user".to_string();
    let mut author = String::new();
    let mut themes = Vec::new();
    for (key, value) in map {
        match key.as_str() {
            "name" => match value.as_str() {
                Some(text) if !text.trim().is_empty() => name = text.trim().to_string(),
                _ => notes.push(format!(
                    "family \"name\" must be a non-empty string, got {value}; calling it {name:?}"
                )),
            },
            "author" => match value.as_str() {
                Some(text) => author = text.to_string(),
                None => notes.push(format!(
                    "family \"author\" must be a string, got {value}; ignored"
                )),
            },
            "themes" => match value.as_array() {
                Some(entries) => {
                    for entry in entries {
                        if let Some(theme) = parse_theme(entry, &mut notes) {
                            themes.push(Arc::new(theme));
                        }
                    }
                }
                None => notes.push(format!(
                    "family \"themes\" must be an array, got {value}; the file is ignored"
                )),
            },
            unknown => notes.push(format!(
                "theme file key {unknown:?} is not one this version knows; ignored"
            )),
        }
    }

    if themes.is_empty() {
        notes.push(format!(
            "theme family {name:?} declares no usable themes; nothing was registered"
        ));
        return LoadedFamily {
            family: None,
            notes,
        };
    }
    LoadedFamily {
        family: Some(ThemeFamily {
            name: name.into(),
            author: author.into(),
            themes,
        }),
        notes,
    }
}

fn parse_theme(value: &Value, notes: &mut Vec<String>) -> Option<Theme> {
    let Some(map) = value.as_object() else {
        notes.push(format!(
            "each theme must be an object, got {value}; skipped"
        ));
        return None;
    };
    let name = match map.get("name").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => {
            notes.push("a theme with no \"name\" cannot be selected; skipped".to_string());
            return None;
        }
    };
    let appearance = match map.get("appearance") {
        None => Appearance::Dark,
        Some(value) => match value.as_str().and_then(Appearance::parse) {
            Some(appearance) => appearance,
            None => {
                notes.push(format!(
                    "theme {name:?} has appearance {value}, which is neither \"light\" nor \
                     \"dark\"; treating it as dark"
                ));
                Appearance::Dark
            }
        },
    };

    // An absent key inherits, so the starting point is the built-in of the
    // same appearance rather than an empty struct: that is what makes a
    // six-line theme file a complete theme.
    let mut theme = match appearance {
        Appearance::Light => K10S_LIGHT.clone(),
        Appearance::Dark => K10S_DARK.clone(),
    };
    theme.name = name.clone().into();
    theme.appearance = appearance;

    for key in map.keys() {
        if !matches!(key.as_str(), "name" | "appearance" | "style") {
            notes.push(format!(
                "theme {name:?} key {key:?} is not one this version knows; ignored"
            ));
        }
    }
    match map.get("style") {
        None => {}
        Some(Value::Object(style)) => {
            let mut inner = Vec::new();
            apply_style(&mut theme, style, &mut inner);
            notes.extend(
                inner
                    .into_iter()
                    .map(|note| format!("theme {name:?}: {note}")),
            );
        }
        Some(other) => notes.push(format!(
            "theme {name:?} has a \"style\" that is not an object ({other}); it inherits \
             everything"
        )),
    }
    Some(theme)
}

fn apply_style(theme: &mut Theme, style: &serde_json::Map<String, Value>, notes: &mut Vec<String>) {
    for (key, value) in style {
        match key.as_str() {
            "syntax" => match value.as_object() {
                Some(map) => apply_syntax(theme, map, notes),
                None => notes.push(format!(
                    "\"syntax\" must be an object, got {value}; ignored"
                )),
            },
            "map" => match value.as_object() {
                Some(map) => apply_map(theme, map, notes),
                None => notes.push(format!("\"map\" must be an object, got {value}; ignored")),
            },
            _ => apply_shell(theme, key, value, notes),
        }
    }
}

macro_rules! solid {
    ($into:expr, $key:expr, $value:expr, $notes:expr) => {
        match color(($key), ($value), ($notes)) {
            Some((rgb, alpha)) => {
                $into = rgb;
                opaque_note(($key), alpha, ($notes));
            }
            None => {}
        }
    };
}

macro_rules! blended {
    ($into:expr, $key:expr, $value:expr, $notes:expr) => {
        match color(($key), ($value), ($notes)) {
            Some(pair) => $into = pair,
            None => {}
        }
    };
}

fn apply_shell(theme: &mut Theme, key: &str, value: &Value, notes: &mut Vec<String>) {
    let shell = &mut theme.shell;
    match key {
        "background" => solid!(shell.background, key, value, notes),
        "surface_background" | "surface.background" => {
            solid!(shell.surface_background, key, value, notes)
        }
        "elevated_surface_background" | "elevated_surface.background" => {
            solid!(shell.elevated_surface_background, key, value, notes)
        }
        "editor_background" | "editor.background" => {
            solid!(shell.editor_background, key, value, notes)
        }
        "toolbar_background" | "toolbar.background" => {
            solid!(shell.toolbar_background, key, value, notes)
        }
        "panel_background" | "panel.background" => {
            solid!(shell.panel_background, key, value, notes)
        }
        "status_bar_background" | "status_bar.background" => {
            solid!(shell.status_bar_background, key, value, notes)
        }
        "tab_bar_background" | "tab_bar.background" => {
            solid!(shell.tab_bar_background, key, value, notes)
        }
        "tab_inactive_background" | "tab.inactive_background" => {
            solid!(shell.tab_inactive_background, key, value, notes)
        }
        "tab_active_background" | "tab.active_background" => {
            solid!(shell.tab_active_background, key, value, notes)
        }
        "border" => solid!(shell.border, key, value, notes),
        "border_variant" | "border.variant" => solid!(shell.border_variant, key, value, notes),
        "border_focused" | "border.focused" => solid!(shell.border_focused, key, value, notes),
        "element_background" | "element.background" => {
            solid!(shell.element_background, key, value, notes)
        }
        "element_hover" | "element.hover" => solid!(shell.element_hover, key, value, notes),
        "element_active" | "element.active" => solid!(shell.element_active, key, value, notes),
        "element_selected" | "element.selected" => {
            solid!(shell.element_selected, key, value, notes)
        }
        "text" => solid!(shell.text, key, value, notes),
        "text_muted" | "text.muted" => solid!(shell.text_muted, key, value, notes),
        "text_placeholder" | "text.placeholder" => {
            solid!(shell.text_placeholder, key, value, notes)
        }
        "text_accent" | "text.accent" => solid!(shell.text_accent, key, value, notes),
        "editor_foreground" | "editor.foreground" => {
            solid!(shell.editor_foreground, key, value, notes)
        }
        "search_match_background" | "search.match_background" => {
            blended!(shell.search_match_background, key, value, notes)
        }
        "terminal_background" | "terminal.background" => {
            solid!(shell.terminal_background, key, value, notes)
        }
        "terminal_foreground" | "terminal.foreground" => {
            solid!(shell.terminal_foreground, key, value, notes)
        }
        "terminal_bright_foreground" | "terminal.bright_foreground" => {
            solid!(shell.terminal_bright_foreground, key, value, notes)
        }
        "terminal_dim_foreground" | "terminal.dim_foreground" => {
            solid!(shell.terminal_dim_foreground, key, value, notes)
        }
        "terminal_ansi" => colors_into(&mut shell.terminal_ansi, key, value, notes),
        "terminal_ansi_dim" => colors_into(&mut shell.terminal_ansi_dim, key, value, notes),
        "cursor" => solid!(shell.cursor, key, value, notes),
        "success" => solid!(shell.success, key, value, notes),
        "warning" => solid!(shell.warning, key, value, notes),
        "error" => solid!(shell.error, key, value, notes),
        unknown => notes.push(format!(
            "style key {unknown:?} is not one this version knows; ignored"
        )),
    }
}

fn apply_syntax(
    theme: &mut Theme,
    style: &serde_json::Map<String, Value>,
    notes: &mut Vec<String>,
) {
    let syntax = &mut theme.syntax;
    for (key, raw) in style {
        // Zed writes each token as `{ "color": "#..." }` so it can carry a
        // font style beside the colour; a bare string is the shorter form a
        // person actually types. Both mean the same thing here.
        let value = match raw {
            Value::Object(map) => match map.get("color") {
                Some(color) => color,
                None => {
                    notes.push(format!(
                        "syntax token {key:?} is an object with no \"color\"; ignored"
                    ));
                    continue;
                }
            },
            other => other,
        };
        match key.as_str() {
            "property" => solid!(syntax.property, key, value, notes),
            "string" => solid!(syntax.string, key, value, notes),
            "number" => solid!(syntax.number, key, value, notes),
            "boolean" => solid!(syntax.boolean, key, value, notes),
            "constant" => solid!(syntax.constant, key, value, notes),
            "comment" => solid!(syntax.comment, key, value, notes),
            "label" => solid!(syntax.label, key, value, notes),
            "type" | "type_name" => solid!(syntax.type_name, key, value, notes),
            "attribute" => solid!(syntax.attribute, key, value, notes),
            "punctuation" => solid!(syntax.punctuation, key, value, notes),
            "punctuation.special" | "punctuation_special" => {
                solid!(syntax.punctuation_special, key, value, notes)
            }
            "line_number" => solid!(syntax.line_number, key, value, notes),
            "active_line_number" => solid!(syntax.active_line_number, key, value, notes),
            "active_line_background" => {
                blended!(syntax.active_line_background, key, value, notes)
            }
            "selection_background" => blended!(syntax.selection_background, key, value, notes),
            unknown => notes.push(format!(
                "syntax token {unknown:?} is not one this version knows; ignored"
            )),
        }
    }
}

fn apply_map(theme: &mut Theme, style: &serde_json::Map<String, Value>, notes: &mut Vec<String>) {
    let map = &mut theme.map;
    for (key, value) in style {
        match key.as_str() {
            "bg" => solid!(map.bg, key, value, notes),
            "hex_line" => solid!(map.hex_line, key, value, notes),
            "ns_fill" => solid!(map.ns_fill, key, value, notes),
            "ns_border" => solid!(map.ns_border, key, value, notes),
            "edge" => solid!(map.edge, key, value, notes),
            "hud_bg" => solid!(map.hud_bg, key, value, notes),
            "hud_text" => solid!(map.hud_text, key, value, notes),
            "curve_core" => solid!(map.curve_core, key, value, notes),
            "curve_glow" => solid!(map.curve_glow, key, value, notes),
            "curve_core_alpha" => unit(&mut map.curve_core_alpha, key, value, notes),
            "curve_glow_alpha" => unit(&mut map.curve_glow_alpha, key, value, notes),
            "card_header_fill" => solid!(map.card_header_fill, key, value, notes),
            "wl_icon" => solid!(map.wl_icon, key, value, notes),
            "hover_ring" => solid!(map.hover_ring, key, value, notes),
            "selection_ring" => solid!(map.selection_ring, key, value, notes),
            "region_label" => solid!(map.region_label, key, value, notes),
            "workload_label" => solid!(map.workload_label, key, value, notes),
            "sat_label" => solid!(map.sat_label, key, value, notes),
            "sat_detail_label" => solid!(map.sat_detail_label, key, value, notes),
            "pod_label" => solid!(map.pod_label, key, value, notes),
            "unknown_kind" => solid!(map.unknown_kind, key, value, notes),
            "kind_colors" => colors_into(&mut map.kind_colors, key, value, notes),
            "tool_colors" => colors_into(&mut map.tool_colors, key, value, notes),
            "pod_severity" => colors_into(&mut map.pod_severity, key, value, notes),
            "workload_fill" => colors_into(&mut map.workload_fill, key, value, notes),
            "workload_border" => blends_into(&mut map.workload_border, key, value, notes),
            "heat_fill" => colors_into(&mut map.heat_fill, key, value, notes),
            "heat_border_base" => solid!(map.heat_border_base, key, value, notes),
            "heat_border_hot" => solid!(map.heat_border_hot, key, value, notes),
            unknown => notes.push(format!(
                "map key {unknown:?} is not one this version knows; ignored"
            )),
        }
    }
}

fn unit(into: &mut f32, key: &str, value: &Value, notes: &mut Vec<String>) {
    match value.as_f64() {
        Some(alpha) if (0.0..=1.0).contains(&alpha) => *into = alpha as f32,
        Some(alpha) => notes.push(format!(
            "{key:?} = {alpha} is not between 0 and 1; keeping {into}"
        )),
        None => notes.push(format!(
            "{key:?} must be a number between 0 and 1, got {value}; keeping {into}"
        )),
    }
}

fn colors_into<const N: usize>(
    into: &mut [u32; N],
    key: &str,
    value: &Value,
    notes: &mut Vec<String>,
) {
    let Some(items) = value.as_array() else {
        notes.push(format!(
            "{key:?} must be an array of {N} colours, got {value}; ignored"
        ));
        return;
    };
    if items.len() != N {
        notes.push(format!(
            "{key:?} needs exactly {N} colours, got {}; ignored",
            items.len()
        ));
        return;
    }
    // All or nothing: a half-applied ramp is a worse answer than the one the
    // theme inherited, so the whole array is staged and only then committed.
    let mut staged = *into;
    let before = notes.len();
    let mut alpha = 1.0f32;
    for (slot, item) in staged.iter_mut().zip(items) {
        if let Some((rgb, item_alpha)) = color(key, item, notes) {
            *slot = rgb;
            alpha = alpha.min(item_alpha);
        }
    }
    // Counted before the note below, because the commit is decided by whether
    // anything failed to parse and an ignored alpha is not a failure.
    if notes.len() == before {
        *into = staged;
        opaque_note(key, alpha, notes);
    }
}

fn blends_into<const N: usize>(
    into: &mut [(u32, f32); N],
    key: &str,
    value: &Value,
    notes: &mut Vec<String>,
) {
    let Some(items) = value.as_array() else {
        notes.push(format!(
            "{key:?} must be an array of {N} colours, got {value}; ignored"
        ));
        return;
    };
    if items.len() != N {
        notes.push(format!(
            "{key:?} needs exactly {N} colours, got {}; ignored",
            items.len()
        ));
        return;
    }
    let mut staged = *into;
    let before = notes.len();
    for (slot, item) in staged.iter_mut().zip(items) {
        if let Some(pair) = color(key, item, notes) {
            *slot = pair;
        }
    }
    if notes.len() == before {
        *into = staged;
    }
}

// An eight-digit colour on a key that is painted opaque is accepted, because
// refusing it would throw away the hue somebody meant. It is still not what
// they wrote, so it is a note like everything else that did not land the way
// the file says.
fn opaque_note(key: &str, alpha: f32, notes: &mut Vec<String>) {
    if alpha < 1.0 {
        notes.push(format!(
            "{key:?} is painted opaque, so its alpha is ignored; use a colour without one to \
             say so"
        ));
    }
}

fn color(key: &str, value: &Value, notes: &mut Vec<String>) -> Option<(u32, f32)> {
    match value.as_str().and_then(parse_color) {
        Some(pair) => Some(pair),
        None => {
            notes.push(format!(
                "{key:?} = {value} is not a colour like \"#1800ad\" or \"#1800ad80\"; ignored"
            ));
            None
        }
    }
}

/// `#rgb`, `#rgba`, `#rrggbb` and `#rrggbbaa`, with the `#` optional, into a
/// packed `0xRRGGBB` and a separate alpha -- separate because the paint path
/// wants the opaque value and applies alpha itself.
pub fn parse_color(text: &str) -> Option<(u32, f32)> {
    let text = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let widen = |c: char| {
        let digit = c.to_digit(16)?;
        Some(digit * 17)
    };
    let bytes: Vec<u32> = match text.len() {
        3 | 4 => text.chars().map(widen).collect::<Option<Vec<u32>>>()?,
        6 | 8 => (0..text.len() / 2)
            .map(|i| u32::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
            .collect::<Option<Vec<u32>>>()?,
        _ => return None,
    };
    let rgb = (bytes[0] << 16) | (bytes[1] << 8) | bytes[2];
    let alpha = bytes.get(3).map_or(1.0, |a| *a as f32 / 255.0);
    Some((rgb, alpha))
}

// JSON with comments and trailing commas, reduced to strict JSON. String
// contents (including escapes) pass through untouched; an unterminated block
// comment simply ends the document, which the JSON parse then reports. Themes
// and settings share it because both are files a person edits by hand.
pub fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b);
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                out.push(b);
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b',' => {
                // A trailing comma is one whose next meaningful byte closes
                // the container; whitespace and comments may sit between.
                let mut j = i + 1;
                loop {
                    match bytes.get(j) {
                        Some(b' ' | b'\t' | b'\r' | b'\n') => j += 1,
                        Some(b'/') if bytes.get(j + 1) == Some(&b'/') => {
                            while j < bytes.len() && bytes[j] != b'\n' {
                                j += 1;
                            }
                        }
                        Some(b'/') if bytes.get(j + 1) == Some(&b'*') => {
                            j += 2;
                            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/')
                            {
                                j += 1;
                            }
                            j = (j + 2).min(bytes.len());
                        }
                        _ => break,
                    }
                }
                if !matches!(bytes.get(j), Some(b'}' | b']')) {
                    out.push(b);
                }
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
#[path = "load_test.rs"]
mod tests;
