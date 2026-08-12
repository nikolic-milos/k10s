//! Settings: one pure data schema, layered over defaults, validated loudly.
//!
//! The schema is plain data with no UI dependency; the store is "defaults,
//! then the user's file wins", Zed-style. The file is JSON with comments and
//! trailing commas allowed, because a settings file a person edits by hand
//! will have both. Everything wrong with the file becomes a labelled note --
//! an unknown field, a wrong type, an unknown theme name -- and the value
//! falls back to its default; a broken settings file can degrade the session,
//! never break it. This module never touches the filesystem: the app reads
//! bytes and applies outcomes, so every path through here is a unit test.
//!
//! Two settings cannot be fully judged from the text alone. A theme name is
//! resolved against the registry, and a font family is resolved against the
//! typefaces the text system actually has; both of those live outside a pure
//! parser, so both are separate passes ([`resolve_families`]) that take the
//! world as an argument and stay unit-testable.

use k10s_theme::{
    Appearance, BUFFER_FONT_SIZE_RANGE, LINE_HEIGHT_RANGE, Overrides, Typography,
    UI_FONT_SIZE_RANGE,
};

/// Which side of the light/dark switch a window paints on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// Follow the desktop. Resolved through the window's appearance on every
    /// `appearance_changed`, never sampled once at startup -- a person who
    /// flips their desktop at sunset expects the editor to follow.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    fn parse(text: &str) -> Option<ThemeMode> {
        match text.trim() {
            t if t.eq_ignore_ascii_case("system") => Some(ThemeMode::System),
            t if t.eq_ignore_ascii_case("light") => Some(ThemeMode::Light),
            t if t.eq_ignore_ascii_case("dark") => Some(ThemeMode::Dark),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::System => "system",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }
}

/// `"theme"` is either a name or a light/dark pair with a mode, which is the
/// shape Zed settled on: one key, two ways to use it, and no second key that
/// silently overrides the first.
#[derive(Debug, Clone, PartialEq)]
pub enum ThemeSelection {
    Fixed(String),
    Pair {
        mode: ThemeMode,
        light: String,
        dark: String,
    },
}

impl ThemeSelection {
    /// The name to resolve for a window currently showing `appearance`.
    pub fn name(&self, appearance: Appearance) -> &str {
        match self {
            ThemeSelection::Fixed(name) => name,
            ThemeSelection::Pair { mode, light, dark } => match mode {
                ThemeMode::Light => light,
                ThemeMode::Dark => dark,
                ThemeMode::System => match appearance {
                    Appearance::Light => light,
                    Appearance::Dark => dark,
                },
            },
        }
    }

    /// Whether the window's appearance is an input, and therefore whether a
    /// change of desktop theme has to re-resolve.
    pub fn follows_system(&self) -> bool {
        matches!(
            self,
            ThemeSelection::Pair {
                mode: ThemeMode::System,
                ..
            }
        )
    }
}

pub const DEFAULT_DARK_THEME: &str = "k10s-dark";
pub const DEFAULT_LIGHT_THEME: &str = "k10s-light";

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    // A name resolved through the theme registry; unknown names fall back to
    // the default theme with a note.
    pub theme: ThemeSelection,
    // Typefaces and sizes. Owned by settings rather than by a theme, because a
    // person picks a typeface once and then tries every theme with it.
    pub typography: Typography,
    // A style patch applied on top of whichever theme resolved, for the person
    // who likes a theme except for one colour.
    pub theme_overrides: Overrides,
    // Dock sizes in logical pixels, clamped to something usable so a typo
    // cannot produce an invisible or screen-eating dock.
    pub left_dock_width: f32,
    pub right_dock_width: f32,
    pub bottom_dock_height: f32,
    // Whether non-essential motion is suppressed: the camera arrives instead of
    // flying, and any gpui animation renders in its static state. A setting
    // rather than a platform query because there is no platform query to make --
    // gpui holds the flag and expects to be told, and being told is what this
    // field is. Published onto gpui rather than read separately by each view, so
    // one answer reaches the whole application.
    pub reduce_motion: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            theme: ThemeSelection::Fixed(DEFAULT_DARK_THEME.to_string()),
            typography: Typography::default(),
            theme_overrides: Overrides::default(),
            left_dock_width: 260.0,
            right_dock_width: 320.0,
            bottom_dock_height: 240.0,
            reduce_motion: false,
        }
    }
}

const DOCK_SIZE_RANGE: std::ops::RangeInclusive<f32> =
    crate::ui::MIN_DOCK_SIZE..=crate::ui::MAX_DOCK_SIZE;

// The one place settings meet gpui: the app publishes the loaded value,
// views read it per render, and an absent global is the defaults.
pub struct ActiveSettings(pub Settings);

impl gpui::Global for ActiveSettings {}

pub fn active(cx: &gpui::App) -> &Settings {
    match cx.try_global::<ActiveSettings>() {
        Some(active) => &active.0,
        None => default_settings(),
    }
}

fn default_settings() -> &'static Settings {
    static DEFAULT: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(Settings::default)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub settings: Settings,
    pub notes: Vec<String>,
}

pub fn parse(text: &str) -> Loaded {
    let mut settings = Settings::default();
    let mut notes = Vec::new();

    let stripped = strip_jsonc(text);
    if stripped.trim().is_empty() {
        return Loaded { settings, notes };
    }
    let value: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(value) => value,
        Err(error) => {
            notes.push(format!(
                "settings file ignored: not valid JSON ({error}); running on defaults"
            ));
            return Loaded { settings, notes };
        }
    };
    let Some(map) = value.as_object() else {
        notes.push("settings file ignored: the top level must be an object".to_string());
        return Loaded { settings, notes };
    };

    for (key, value) in map {
        match key.as_str() {
            "theme" => theme(value, &mut settings.theme, &mut notes),
            "ui_font_family" => family(value, &mut settings.typography.ui_family, key, &mut notes),
            "buffer_font_family" => family(
                value,
                &mut settings.typography.buffer_family,
                key,
                &mut notes,
            ),
            "display_font_family" => family(
                value,
                &mut settings.typography.display_family,
                key,
                &mut notes,
            ),
            "ui_font_size" => number(
                value,
                &mut settings.typography.ui_size,
                key,
                UI_FONT_SIZE_RANGE,
                &mut notes,
            ),
            "buffer_font_size" => number(
                value,
                &mut settings.typography.buffer_size,
                key,
                BUFFER_FONT_SIZE_RANGE,
                &mut notes,
            ),
            "buffer_line_height" => number(
                value,
                &mut settings.typography.buffer_line_height,
                key,
                LINE_HEIGHT_RANGE,
                &mut notes,
            ),
            "experimental.theme_overrides" => {
                let (overrides, inner) = k10s_theme::parse_overrides(value);
                settings.theme_overrides = overrides;
                notes.extend(inner);
            }
            "left_dock_width" => number(
                value,
                &mut settings.left_dock_width,
                key,
                DOCK_SIZE_RANGE,
                &mut notes,
            ),
            "right_dock_width" => number(
                value,
                &mut settings.right_dock_width,
                key,
                DOCK_SIZE_RANGE,
                &mut notes,
            ),
            "bottom_dock_height" => number(
                value,
                &mut settings.bottom_dock_height,
                key,
                DOCK_SIZE_RANGE,
                &mut notes,
            ),
            "reduce_motion" => boolean(value, &mut settings.reduce_motion, key, &mut notes),
            unknown => notes.push(format!(
                "settings field {unknown:?} is not one this version knows; ignored"
            )),
        }
    }
    Loaded { settings, notes }
}

fn theme(value: &serde_json::Value, into: &mut ThemeSelection, notes: &mut Vec<String>) {
    if let Some(name) = value.as_str() {
        *into = ThemeSelection::Fixed(name.to_string());
        return;
    }
    let Some(map) = value.as_object() else {
        notes.push(format!(
            "settings field \"theme\" must be a name or a {{mode, light, dark}} object, got \
             {value}; keeping {:?}",
            into.name(Appearance::Dark)
        ));
        return;
    };
    let mut mode = ThemeMode::default();
    let mut light = DEFAULT_LIGHT_THEME.to_string();
    let mut dark = DEFAULT_DARK_THEME.to_string();
    for (key, value) in map {
        match key.as_str() {
            "mode" => match value.as_str().and_then(ThemeMode::parse) {
                Some(parsed) => mode = parsed,
                None => notes.push(format!(
                    "settings field \"theme.mode\" must be \"system\", \"light\" or \"dark\", got \
                     {value}; keeping {}",
                    mode.as_str()
                )),
            },
            "light" => match value.as_str() {
                Some(name) => light = name.to_string(),
                None => notes.push(format!(
                    "settings field \"theme.light\" must be a string, got {value}; keeping \
                     {light:?}"
                )),
            },
            "dark" => match value.as_str() {
                Some(name) => dark = name.to_string(),
                None => notes.push(format!(
                    "settings field \"theme.dark\" must be a string, got {value}; keeping {dark:?}"
                )),
            },
            unknown => notes.push(format!(
                "settings field \"theme.{unknown}\" is not one this version knows; ignored"
            )),
        }
    }
    *into = ThemeSelection::Pair { mode, light, dark };
}

// A family name is only a name here: whether the text system has it is a
// question this module cannot ask, so `resolve_families` asks it later.
fn family(
    value: &serde_json::Value,
    into: &mut gpui::SharedString,
    key: &str,
    notes: &mut Vec<String>,
) {
    match value.as_str() {
        Some(name) if !name.trim().is_empty() => *into = name.trim().to_string().into(),
        _ => notes.push(format!(
            "settings field {key:?} must be a non-empty string, got {value}; keeping {into:?}"
        )),
    }
}

fn boolean(value: &serde_json::Value, into: &mut bool, key: &str, notes: &mut Vec<String>) {
    match value.as_bool() {
        Some(on) => *into = on,
        None => notes.push(format!(
            "settings field {key:?} must be true or false, got {value}; keeping {into}"
        )),
    }
}

fn number(
    value: &serde_json::Value,
    into: &mut f32,
    key: &str,
    range: std::ops::RangeInclusive<f32>,
    notes: &mut Vec<String>,
) {
    match value.as_f64() {
        Some(size) if range.contains(&(size as f32)) => *into = size as f32,
        Some(size) => {
            let clamped = (size as f32).clamp(*range.start(), *range.end());
            notes.push(format!(
                "settings field {key:?} = {size} is outside {range:?}; clamped to {clamped}"
            ));
            *into = clamped;
        }
        None => notes.push(format!(
            "settings field {key:?} must be a number, got {value}; keeping {into}"
        )),
    }
}

/// Check the two font families against what the text system really has,
/// resolve the friendly spelling of a shipped face, and fall back with a note
/// when a family is genuinely absent.
///
/// This is the pass that keeps a typo out of the render path. gpui does not
/// fail on an unknown family; it substitutes a platform face, which looks
/// almost right and is not the typeface the theme was measured with. Being
/// told is the whole point, so the note names the family and the fallback.
pub fn resolve_families(settings: &mut Settings, available: &[String]) -> Vec<String> {
    let mut notes = Vec::new();
    let has = |name: &str| available.iter().any(|available| available == name);
    let mut check = |family: &mut gpui::SharedString, default: &str, key: &str| {
        if has(family.as_ref()) {
            return;
        }
        // The same typeface under a shorter name is not a fallback, so it
        // resolves without a word.
        if let Some((_, declared)) = k10s_theme::FAMILY_ALIASES
            .iter()
            .find(|(alias, declared)| *alias == family.as_ref() && has(declared))
        {
            *family = (*declared).into();
            return;
        }
        notes.push(format!(
            "settings field {key:?} names {family:?}, which this text system does not have; \
             falling back to {default:?}"
        ));
        *family = default.into();
    };
    check(
        &mut settings.typography.ui_family,
        k10s_theme::DEFAULT_UI_FAMILY,
        "ui_font_family",
    );
    check(
        &mut settings.typography.buffer_family,
        k10s_theme::DEFAULT_BUFFER_FAMILY,
        "buffer_font_family",
    );
    check(
        &mut settings.typography.display_family,
        k10s_theme::DISPLAY_FAMILY,
        "display_font_family",
    );
    notes
}

pub use k10s_theme::strip_jsonc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_empty_file_is_the_defaults_with_no_complaint() {
        for text in ["", "   \n", "// nothing but a comment\n"] {
            let loaded = parse(text);
            assert_eq!(loaded.settings, Settings::default(), "{text:?}");
            assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        }
    }

    #[test]
    fn comments_and_trailing_commas_are_what_a_person_writes() {
        let loaded = parse(
            r#"// k10s settings
            {
                /* the theme block */
                "theme": "one-dark", // trailing comma next
            }"#,
        );
        assert_eq!(
            loaded.settings.theme,
            ThemeSelection::Fixed("one-dark".to_string())
        );
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
    }

    #[test]
    fn every_wrong_thing_is_a_note_and_the_value_falls_back() {
        let loaded = parse(r#"{"theme": 3, "cosmic_rays": true}"#);
        assert_eq!(loaded.settings.theme, Settings::default().theme);
        assert_eq!(loaded.notes.len(), 2, "{:?}", loaded.notes);
        assert!(loaded.notes[0].contains("must be a name or a"));
        assert!(loaded.notes[1].contains("cosmic_rays"));

        let broken = parse("{ not json at all");
        assert_eq!(broken.settings, Settings::default());
        assert!(broken.notes[0].contains("not valid JSON"));

        let array = parse("[1, 2]");
        assert!(array.notes[0].contains("must be an object"));
    }

    #[test]
    fn reduce_motion_reads_as_a_switch_and_says_so_when_it_is_not_one() {
        assert!(
            !Settings::default().reduce_motion,
            "motion is on unless somebody asked for it not to be"
        );
        let on = parse(r#"{"reduce_motion": true}"#);
        assert!(on.settings.reduce_motion);
        assert!(on.notes.is_empty(), "{:?}", on.notes);

        // A fault is a labelled note and the previous value stands, like every
        // other field here: a settings file with one bad line is still a
        // settings file, and losing the rest of it to a typo is the failure
        // this whole loader is shaped to avoid.
        let wrong = parse(r#"{"reduce_motion": "yes"}"#);
        assert!(!wrong.settings.reduce_motion);
        assert_eq!(wrong.notes.len(), 1, "{:?}", wrong.notes);
        assert!(wrong.notes[0].contains("must be true or false"));
    }

    #[test]
    fn dock_sizes_parse_clamp_and_complain() {
        let loaded = parse(r#"{"left_dock_width": 300, "bottom_dock_height": 20}"#);
        assert_eq!(loaded.settings.left_dock_width, 300.0);
        assert_eq!(
            loaded.settings.bottom_dock_height, 120.0,
            "an absurd size clamps instead of vanishing the dock"
        );
        assert_eq!(loaded.notes.len(), 1, "{:?}", loaded.notes);
        assert!(loaded.notes[0].contains("clamped"));

        let wrong = parse(r#"{"right_dock_width": "wide"}"#);
        assert_eq!(wrong.settings.right_dock_width, 320.0);
        assert!(wrong.notes[0].contains("must be a number"));
    }

    #[test]
    fn string_contents_survive_the_comment_stripper() {
        let loaded = parse(r#"{"theme": "no//comment /* here */ \" quoted"}"#);
        assert_eq!(
            loaded.settings.theme,
            ThemeSelection::Fixed(r#"no//comment /* here */ " quoted"#.to_string())
        );
        assert_eq!(
            strip_jsonc(r#"{"a": "b,}"}"#),
            r#"{"a": "b,}"}"#,
            "a comma inside a string is content"
        );
        assert_eq!(strip_jsonc("[1, /* x */ 2, ]"), "[1,  2 ]");
    }

    #[test]
    fn a_theme_pair_resolves_against_the_windows_appearance() {
        let loaded = parse(
            r#"{ "theme": { "mode": "system", "light": "k10s-light", "dark": "one-dark" } }"#,
        );
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        let selection = &loaded.settings.theme;
        assert!(selection.follows_system());
        assert_eq!(selection.name(Appearance::Light), "k10s-light");
        assert_eq!(selection.name(Appearance::Dark), "one-dark");

        let forced = parse(r#"{ "theme": { "mode": "light", "dark": "one-dark" } }"#);
        assert!(forced.notes.is_empty(), "{:?}", forced.notes);
        assert!(!forced.settings.theme.follows_system());
        assert_eq!(
            forced.settings.theme.name(Appearance::Dark),
            "k10s-light",
            "an explicit mode wins over the desktop"
        );

        let sparse = parse(r#"{ "theme": {} }"#);
        assert_eq!(
            sparse.settings.theme,
            ThemeSelection::Pair {
                mode: ThemeMode::System,
                light: DEFAULT_LIGHT_THEME.to_string(),
                dark: DEFAULT_DARK_THEME.to_string(),
            },
            "an empty object is the brand pair following the desktop"
        );

        let wrong = parse(r#"{ "theme": { "mode": "sepia", "invented": 1 } }"#);
        assert_eq!(wrong.notes.len(), 2, "{:?}", wrong.notes);
        assert!(wrong.notes.iter().any(|note| note.contains("sepia")));
        assert!(
            wrong
                .notes
                .iter()
                .any(|note| note.contains("theme.invented"))
        );
        assert!(wrong.settings.theme.follows_system());
    }

    #[test]
    fn typography_parses_clamps_and_derives_a_whole_pixel_row() {
        let loaded = parse(
            r#"{ "ui_font_family": "Inter", "ui_font_size": 16,
                 "buffer_font_family": "Lilex", "buffer_font_size": 18,
                 "buffer_line_height": 1.5 }"#,
        );
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        let type_ = &loaded.settings.typography;
        assert_eq!(type_.ui_size, 16.0);
        assert_eq!(type_.small(), 14.0);
        assert_eq!(type_.buffer_size, 18.0);
        assert_eq!(type_.line_height(), 27.0);

        let absurd = parse(r#"{ "ui_font_size": 400, "buffer_line_height": 0.1 }"#);
        assert_eq!(absurd.settings.typography.ui_size, 32.0);
        assert_eq!(absurd.settings.typography.buffer_line_height, 1.0);
        assert_eq!(absurd.notes.len(), 2, "{:?}", absurd.notes);
        assert!(absurd.notes.iter().all(|note| note.contains("clamped")));

        let empty = parse(r#"{ "ui_font_family": "  " }"#);
        assert_eq!(
            empty.settings.typography.ui_family,
            Typography::default().ui_family
        );
        assert!(empty.notes[0].contains("non-empty string"));
    }

    #[test]
    fn a_font_the_text_system_does_not_have_is_a_note_not_a_silent_substitution() {
        let installed = [
            "Inter 18pt".to_string(),
            "Lilex".to_string(),
            "League Spartan".to_string(),
        ];

        let mut typo = parse(r#"{ "ui_font_family": "Comic Sans MS" }"#).settings;
        let notes = resolve_families(&mut typo, &installed);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("Comic Sans MS"));
        assert!(notes[0].contains("Inter 18pt"));
        assert_eq!(typo.typography.ui_family, k10s_theme::DEFAULT_UI_FAMILY);

        let mut exact = parse(r#"{ "ui_font_family": "Lilex" }"#).settings;
        assert!(
            resolve_families(&mut exact, &installed).is_empty(),
            "a family the system has is left alone"
        );
        assert_eq!(exact.typography.ui_family, "Lilex");

        let mut friendly = parse(r#"{ "ui_font_family": "Inter" }"#).settings;
        assert!(
            resolve_families(&mut friendly, &installed).is_empty(),
            "the shipped face under its short name is not a fallback and says nothing"
        );
        assert_eq!(
            friendly.typography.ui_family, "Inter 18pt",
            "resolved to the name the font file declares, which is what gpui matches on"
        );

        let mut display = parse(r#"{ "display_font_family": "Papyrus" }"#).settings;
        let notes = resolve_families(&mut display, &installed);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("display_font_family"));
        assert_eq!(
            display.typography.display_family,
            k10s_theme::DISPLAY_FAMILY
        );

        let mut nothing = Settings::default();
        let notes = resolve_families(&mut nothing, &[]);
        assert_eq!(
            notes.len(),
            3,
            "a text system with no fonts at all complains about all three: {notes:?}"
        );
    }

    #[test]
    fn theme_overrides_are_validated_with_the_rest_of_the_file() {
        let loaded = parse(
            r##"{ "experimental.theme_overrides": { "editor_background": "#101014",
                   "invented": "#ffffff" } }"##,
        );
        assert_eq!(loaded.notes.len(), 1, "{:?}", loaded.notes);
        assert!(loaded.notes[0].contains("invented"));
        let mut theme = k10s_theme::K10S_DARK.clone();
        loaded.settings.theme_overrides.apply(&mut theme);
        assert_eq!(theme.shell.editor_background, 0x101014);
    }
}
