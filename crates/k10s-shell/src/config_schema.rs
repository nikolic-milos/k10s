//! The schemas behind the settings and keymap files.
//!
//! Zed's settings surface is the settings file, so the editor has to know
//! that file's shape as well as it knows a Deployment's. These roots are
//! built in Rust rather than fetched, because the schema *is* this binary:
//! the theme enum comes from the theme registry, the context enum from the
//! contexts the shell actually sets, and the keymap's action names from the
//! live action registry, so completion can never advertise something this
//! build does not accept. Drift is a test failure, not a support ticket --
//! `every_advertised_setting_is_one_the_loader_accepts` feeds each key the
//! schema offers through the real parser and fails if it comes back
//! unknown.

use std::collections::BTreeMap;
use std::sync::Arc;

use k10s_edit::schema::{Additional, ScalarKind, SchemaNode, Shape};
use k10s_theme::{BUFFER_FONT_SIZE_RANGE, LINE_HEIGHT_RANGE, ThemeRegistry, UI_FONT_SIZE_RANGE};

use crate::ui::{MAX_DOCK_SIZE, MIN_DOCK_SIZE};

pub const SETTINGS_TEMPLATE: &str = "\
// k10s settings. Comments and trailing commas are fine.
// Every key completes with ctrl-space; unknown keys are flagged as you type.
{
  \"theme\": \"k10s-dark\",
  \"ui_font_family\": \"Inter\",
  \"ui_font_size\": 14,
  \"buffer_font_family\": \"Lilex\",
  \"buffer_font_size\": 15,
  \"buffer_line_height\": 1.35
}
";

// This used to spell the contexts out, and by the time anyone read it the
// sentence was wrong: `Diff` had been added to the shell and to the schema's
// enum and not to here. The list is not repeated now, because the file already
// has a live source for it -- the schema offers `crate::KEY_CONTEXTS` as the
// context field's enum, so ctrl-space in the file being described answers the
// question from this build rather than from whenever this string was last
// touched. A sentence that has to be maintained in step with a list is a
// sentence that will not be.
pub const KEYMAP_TEMPLATE: &str = "\
// k10s keymap. Each section binds keystrokes inside one context.
// Context names and action names both complete with ctrl-space.
// A null action unbinds a default.
[
  {
    \"context\": \"Workspace\",
    \"bindings\": {}
  }
]
";

fn object(properties: Vec<(&str, Arc<SchemaNode>)>) -> Arc<SchemaNode> {
    let mut map = BTreeMap::new();
    for (key, node) in properties {
        map.insert(key.to_string(), node);
    }
    Arc::new(SchemaNode {
        description: String::new(),
        shape: Shape::Object {
            properties: map,
            required: Vec::new(),
            // A settings key this build does not accept is a mistake worth
            // naming, so nothing unnamed belongs here.
            additional: Additional::Deny,
        },
        nullable: false,
    })
}

fn object_named(description: &str, properties: Vec<(&str, Arc<SchemaNode>)>) -> Arc<SchemaNode> {
    let mut node = object(properties);
    Arc::make_mut(&mut node).description = description.to_string();
    node
}

fn scalar(kind: ScalarKind, values: Vec<String>, description: &str) -> Arc<SchemaNode> {
    Arc::new(SchemaNode {
        description: description.to_string(),
        shape: Shape::Scalar { kind, values },
        nullable: false,
    })
}

// The settings keys this build accepts, each with the documentation the
// completion popup shows. One list, consumed by the schema and pinned by a
// test against the loader.
fn settings_fields(
    registry: &ThemeRegistry,
    families: &[String],
) -> Vec<(&'static str, Arc<SchemaNode>)> {
    // Straight from the registry, so a theme file a person dropped into
    // `themes/` completes exactly like a shipped one, and a name this build
    // cannot resolve is never offered.
    let themes: Vec<String> = registry
        .names()
        .into_iter()
        .map(|name| name.to_string())
        .collect();
    let theme_name = |what: &str| scalar(ScalarKind::Str, themes.clone(), what);
    // One key, two shapes: a name, or a mode with a name for each side of the
    // light/dark switch. Both are what the loader accepts, so both are offered.
    let theme = Arc::new(SchemaNode {
        description: "the theme this window paints with".to_string(),
        shape: Shape::Union(vec![
            theme_name("the theme this window paints with"),
            object_named(
                "a theme for each appearance, chosen by mode",
                vec![
                    (
                        "mode",
                        scalar(
                            ScalarKind::Str,
                            vec![
                                "system".to_string(),
                                "light".to_string(),
                                "dark".to_string(),
                            ],
                            "\"system\" follows the desktop; \"light\" and \"dark\" pin it",
                        ),
                    ),
                    ("light", theme_name("the theme used in a light appearance")),
                    ("dark", theme_name("the theme used in a dark appearance")),
                ],
            ),
        ]),
        nullable: false,
    });
    // The families the text system really has, offered rather than described,
    // because the spelling matters: the shipped Inter static declares its
    // optical size ("Inter 18pt") and nobody would guess that. The enum is one
    // member of a union whose other member is any string, since the loader
    // accepts any name and resolves it against this same list afterwards --
    // advertising only these would flag a fontconfig alias the platform does
    // resolve.
    let family = |what: &str| {
        Arc::new(SchemaNode {
            description: what.to_string(),
            shape: Shape::Union(vec![
                scalar(ScalarKind::Str, families.to_vec(), what),
                scalar(ScalarKind::Str, Vec::new(), "any family this system has"),
            ]),
            nullable: false,
        })
    };
    let dock_size = |what: &str| {
        scalar(
            ScalarKind::Number,
            Vec::new(),
            &format!("{what}, in pixels ({MIN_DOCK_SIZE} to {MAX_DOCK_SIZE})"),
        )
    };
    let range = |what: &str, bounds: &std::ops::RangeInclusive<f32>| {
        scalar(
            ScalarKind::Number,
            Vec::new(),
            &format!("{what} ({} to {})", bounds.start(), bounds.end()),
        )
    };
    // The overrides map is open on purpose: its keys are theme style names,
    // which the theme loader validates and reports on, and duplicating that
    // list here would be a second place to forget to update.
    let overrides = Arc::new(SchemaNode {
        description: "style keys layered on top of the chosen theme, as in a theme file"
            .to_string(),
        shape: Shape::Object {
            properties: BTreeMap::new(),
            required: Vec::new(),
            additional: Additional::Any,
        },
        nullable: false,
    });
    vec![
        ("theme", theme),
        (
            "ui_font_family",
            family("the typeface the interface is drawn in"),
        ),
        (
            "ui_font_size",
            range("interface text size", &UI_FONT_SIZE_RANGE),
        ),
        (
            "buffer_font_family",
            family("the typeface buffers and the terminal are drawn in"),
        ),
        (
            "display_font_family",
            family("the headline typeface, which the map sets namespace names in"),
        ),
        (
            "buffer_font_size",
            range("buffer text size", &BUFFER_FONT_SIZE_RANGE),
        ),
        (
            "buffer_line_height",
            range(
                "buffer row height, as a multiple of the buffer size",
                &LINE_HEIGHT_RANGE,
            ),
        ),
        ("experimental.theme_overrides", overrides),
        ("left_dock_width", dock_size("width of the left dock")),
        ("right_dock_width", dock_size("width of the right dock")),
        ("bottom_dock_height", dock_size("height of the bottom dock")),
        (
            "reduce_motion",
            scalar(
                ScalarKind::Boolean,
                Vec::new(),
                "arrive instead of animating: the camera stops flying and \
                 non-essential motion is drawn in its settled state",
            ),
        ),
    ]
}

pub fn settings_root(registry: &ThemeRegistry, families: &[String]) -> Arc<SchemaNode> {
    object(settings_fields(registry, families))
}

pub fn keymap_root(cx: &gpui::App) -> Arc<SchemaNode> {
    let mut actions: Vec<String> = cx
        .all_action_names()
        .iter()
        .filter(|name| name.starts_with("k10s_shell::") || name.starts_with("k10s_map::"))
        .map(|name| (*name).to_string())
        .collect();
    actions.sort();
    keymap_shape(actions)
}

fn keymap_shape(actions: Vec<String>) -> Arc<SchemaNode> {
    let contexts: Vec<String> = crate::KEY_CONTEXTS
        .iter()
        .map(|context| (*context).to_string())
        .collect();
    // The actions this build defines complete, but the loader hands the name to
    // gpui's own registry, which resolves anything registered -- `menu::Confirm`
    // among them. Advertising only ours would flag a binding the loader accepts,
    // so the enum is one member of a union whose other member is any string.
    let action = Arc::new(SchemaNode {
        description: "the action this keystroke dispatches".to_string(),
        shape: Shape::Union(vec![
            scalar(
                ScalarKind::Str,
                actions,
                "the action this keystroke dispatches",
            ),
            scalar(ScalarKind::Str, Vec::new(), "any registered action name"),
        ]),
        nullable: false,
    });
    // `["name", {args}]` is the documented form for an action with arguments,
    // and `null` unbinds a default -- the loader accepts all three.
    let with_arguments = Arc::new(SchemaNode {
        description: "an action name followed by its arguments".to_string(),
        shape: Shape::Array { items: None },
        nullable: false,
    });
    let bindings = Arc::new(SchemaNode {
        description: "keystrokes mapped to action names; null unbinds a default".to_string(),
        shape: Shape::Object {
            properties: BTreeMap::new(),
            required: Vec::new(),
            // Every keystroke is a name the user chooses, so the map is open --
            // and `null` is a documented member of the value union rather than
            // something the validator happens to let through.
            additional: Additional::Schema(Arc::new(SchemaNode {
                description: "the action this keystroke dispatches".to_string(),
                shape: Shape::Union(vec![action, with_arguments, SchemaNode::null()]),
                nullable: false,
            })),
        },
        nullable: false,
    });
    let section = Arc::new(SchemaNode {
        description: "one context's bindings".to_string(),
        shape: Shape::Object {
            properties: BTreeMap::from([
                (
                    "context".to_string(),
                    // The known contexts complete, but the loader hands the
                    // string to gpui's predicate parser, which also accepts
                    // boolean expressions over them.
                    Arc::new(SchemaNode {
                        description: "the key context these bindings apply in".to_string(),
                        shape: Shape::Union(vec![
                            scalar(
                                ScalarKind::Str,
                                contexts,
                                "the key context these bindings apply in",
                            ),
                            scalar(ScalarKind::Str, Vec::new(), "a context predicate"),
                        ]),
                        nullable: false,
                    }),
                ),
                ("bindings".to_string(), bindings),
            ]),
            required: Vec::from(["bindings".to_string()]),
            additional: Additional::Deny,
        },
        nullable: false,
    });
    Arc::new(SchemaNode {
        description: String::new(),
        shape: Shape::Array {
            items: Some(section),
        },
        nullable: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A stand-in for what a text system reports; the schema's job here is the
    // key and the shape, not the machine's font inventory.
    fn font_families() -> Vec<String> {
        vec!["Inter 18pt".to_string(), "Lilex".to_string()]
    }

    fn keys(root: &Arc<SchemaNode>) -> Vec<String> {
        let Shape::Object { properties, .. } = &root.shape else {
            panic!("the settings root is an object");
        };
        properties.keys().cloned().collect()
    }

    // Every key the schema offers, probed with a value of its own shape: the
    // point of a probe is the key, not the value, so a wrong-type note is
    // fine and an "unknown key" note is the failure.
    fn probe_for(key: &str) -> String {
        let value = match key {
            "theme" => "\"k10s-dark\"".to_string(),
            "ui_font_family" | "buffer_font_family" | "display_font_family" => {
                "\"Inter\"".to_string()
            }
            "experimental.theme_overrides" => "{}".to_string(),
            _ => "14".to_string(),
        };
        format!("{{ \"{key}\": {value} }}")
    }

    fn theme_names(root: &Arc<SchemaNode>) -> Vec<String> {
        let Shape::Object { properties, .. } = &root.shape else {
            panic!("the settings root is an object");
        };
        // The theme key is a union of a name and a light/dark pair; the names
        // are the first member's enum.
        let Shape::Union(members) = &properties["theme"].shape else {
            panic!("theme is a union of a name and a pair");
        };
        let Shape::Scalar { values, .. } = &members[0].shape else {
            panic!("the first member is the name enum");
        };
        values.clone()
    }

    #[test]
    fn every_advertised_setting_is_one_the_loader_accepts() {
        let registry = ThemeRegistry::builtin();
        let offered = keys(&settings_root(&registry, &font_families()));
        assert!(offered.len() >= 10, "{offered:?}");
        for key in offered {
            let loaded = crate::settings::parse(&probe_for(&key));
            assert!(
                !loaded
                    .notes
                    .iter()
                    .any(|note| note.contains("not one this version knows")),
                "the schema offers {key:?} but the loader rejects it: {:?}",
                loaded.notes
            );
        }
    }

    #[test]
    fn the_theme_enum_is_the_registry_itself() {
        let mut registry = ThemeRegistry::builtin();
        let loaded = k10s_theme::parse_family(r#"{ "themes": [ { "name": "Invented" } ] }"#);
        registry.add_family(loaded.family.expect("a family"));

        let values = theme_names(&settings_root(&registry, &font_families()));
        for theme in registry.themes() {
            assert!(
                values
                    .iter()
                    .any(|name| name.as_str() == theme.name.as_ref()),
                "every theme the registry has is offered: {} missing from {values:?}",
                theme.name
            );
        }
        assert!(
            values.iter().any(|name| name == "Invented"),
            "a theme a user dropped in themes/ completes like a shipped one: {values:?}"
        );
    }

    #[test]
    fn every_advertised_theme_is_one_the_registry_resolves() {
        let registry = ThemeRegistry::builtin();
        let values = theme_names(&settings_root(&registry, &font_families()));
        for name in &values {
            assert!(
                registry.get(name).is_some(),
                "the schema offers {name:?} but the registry cannot resolve it"
            );
        }
        assert!(
            values.iter().any(|name| name == "starmap-dark"),
            "the aliases the loader accepts are advertised too: {values:?}"
        );
    }

    #[test]
    fn the_theme_key_offers_both_shapes_the_loader_accepts() {
        use k10s_edit::complete::validate_with_root;
        use k10s_edit::{Rope, SchemaIndex, Syntax};
        let root = settings_root(&ThemeRegistry::builtin(), &font_families());
        let mut syntax = Syntax::json();
        for body in [
            r#"{ "theme": "k10s-light" }"#,
            r#"{ "theme": { "mode": "system", "light": "k10s-light", "dark": "k10s-dark" } }"#,
            r#"{ "theme": { "mode": "dark" } }"#,
        ] {
            let rope = Rope::from(body);
            syntax.reparse(&rope);
            let diagnostics = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root);
            assert!(
                diagnostics.is_empty(),
                "the loader accepts this, so the editor must too: {body} -> {diagnostics:?}"
            );
            assert!(
                crate::settings::parse(body).notes.is_empty(),
                "the loader really does accept it: {body}"
            );
        }
    }

    #[test]
    fn the_keymap_template_loads_and_validates_against_its_own_schema() {
        use k10s_edit::complete::validate_with_root;
        use k10s_edit::{Rope, SchemaIndex, Syntax};
        let rope = Rope::from(KEYMAP_TEMPLATE);
        let mut syntax = Syntax::json();
        syntax.reparse(&rope);
        // The action-name enum needs an App, which a unit test has no window
        // for; the shape under test here is the file's, not the registry's.
        let root = keymap_shape(Vec::new());
        let diagnostics = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root);
        assert!(
            diagnostics.is_empty(),
            "the template we write for a new user must be clean: {diagnostics:?}"
        );
        let (parsed, notes) = crate::keymap::parse_keymap(KEYMAP_TEMPLATE);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(parsed.is_empty(), "the template binds nothing yet");
    }

    #[test]
    fn every_documented_binding_form_validates() {
        use k10s_edit::complete::validate_with_root;
        use k10s_edit::{Rope, SchemaIndex, Syntax};
        let root = keymap_shape(vec!["k10s_shell::EditorSave".to_string()]);
        let mut syntax = Syntax::json();
        for body in [
            "[{ \"context\": \"Editor\", \"bindings\": { \"ctrl-s\": \"k10s_shell::EditorSave\" } }]",
            "[{ \"context\": \"Editor && !Typing\", \"bindings\": { \"ctrl-s\": null } }]",
            "[{ \"bindings\": { \"ctrl-s\": [\"k10s_shell::EditorSave\", {}] } }]",
        ] {
            let rope = Rope::from(body);
            syntax.reparse(&rope);
            let diagnostics = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root);
            assert!(
                diagnostics.is_empty(),
                "the loader accepts this, so the editor must too: {body} -> {diagnostics:?}"
            );
            let (_, notes) = crate::keymap::parse_keymap(body);
            assert!(
                notes.is_empty(),
                "the loader really does accept it: {notes:?}"
            );
        }
    }

    #[test]
    fn the_settings_template_validates_against_its_own_schema() {
        use k10s_edit::complete::validate_with_root;
        use k10s_edit::{Rope, SchemaIndex, Syntax};
        let rope = Rope::from(SETTINGS_TEMPLATE);
        let mut syntax = Syntax::json();
        syntax.reparse(&rope);
        let root = settings_root(&ThemeRegistry::builtin(), &font_families());
        let diagnostics = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root);
        assert!(
            diagnostics.is_empty(),
            "the template we write for a new user must be clean: {diagnostics:?}"
        );
        let loaded = crate::settings::parse(SETTINGS_TEMPLATE);
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
    }
}
