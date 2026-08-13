//! The loadable keymap: Zed's file format over this app's actions.
//!
//! A keymap file is a JSON array of sections -- `{"context": "Browse",
//! "bindings": {"j": "k10s_shell::RowDown"}}` -- with comments and trailing
//! commas allowed, the format Zed users already know. User bindings are bound
//! after the defaults, and later bindings win, so the file overrides without
//! erasing; binding a key to `null` unbinds it in that context. Parsing is
//! pure and fully unit-tested; only the last step (naming a registered
//! action, parsing a keystroke) needs an `App`, and everything wrong becomes
//! a labelled note, never a panic -- a broken keymap degrades, it does not
//! take the session down.

use gpui::{App, DummyKeyboardMapper, KeyBinding, NoAction};

use crate::settings::strip_jsonc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingAction {
    Named { name: String, data: Option<String> },
    // "key": null -- suppress whatever an earlier layer bound.
    Unbound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBinding {
    pub context: Option<String>,
    pub keystrokes: String,
    pub action: BindingAction,
}

pub fn parse_keymap(text: &str) -> (Vec<ParsedBinding>, Vec<String>) {
    let mut parsed = Vec::new();
    let mut notes = Vec::new();

    let stripped = strip_jsonc(text);
    if stripped.trim().is_empty() {
        return (parsed, notes);
    }
    let value: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(value) => value,
        Err(error) => {
            notes.push(format!("keymap file ignored: not valid JSON ({error})"));
            return (parsed, notes);
        }
    };
    let Some(sections) = value.as_array() else {
        notes.push("keymap file ignored: the top level must be an array of sections".to_string());
        return (parsed, notes);
    };

    for (index, section) in sections.iter().enumerate() {
        let Some(section) = section.as_object() else {
            notes.push(format!("keymap section {index} ignored: not an object"));
            continue;
        };
        let context = match section.get("context") {
            None => None,
            Some(serde_json::Value::String(context)) => Some(context.clone()),
            Some(other) => {
                notes.push(format!(
                    "keymap section {index} ignored: \"context\" must be a string, got {other}"
                ));
                continue;
            }
        };
        let Some(bindings) = section.get("bindings").and_then(|b| b.as_object()) else {
            notes.push(format!(
                "keymap section {index} ignored: no \"bindings\" object"
            ));
            continue;
        };
        for (keystrokes, action) in bindings {
            let action = match action {
                serde_json::Value::Null => BindingAction::Unbound,
                serde_json::Value::String(name) => BindingAction::Named {
                    name: name.clone(),
                    data: None,
                },
                serde_json::Value::Array(parts) => match (parts.first(), parts.get(1)) {
                    (Some(serde_json::Value::String(name)), Some(data)) if parts.len() == 2 => {
                        BindingAction::Named {
                            name: name.clone(),
                            data: Some(data.to_string()),
                        }
                    }
                    _ => {
                        notes.push(format!(
                            "keymap binding {keystrokes:?} ignored: an array action must be \
                             [\"name\", {{args}}]"
                        ));
                        continue;
                    }
                },
                other => {
                    notes.push(format!(
                        "keymap binding {keystrokes:?} ignored: expected an action name, \
                         [\"name\", {{args}}], or null, got {other}"
                    ));
                    continue;
                }
            };
            parsed.push(ParsedBinding {
                context: context.clone(),
                keystrokes: keystrokes.clone(),
                action,
            });
        }
    }
    (parsed, notes)
}

pub fn build(parsed: &[ParsedBinding], cx: &App) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings = Vec::new();
    let mut notes = Vec::new();
    for binding in parsed {
        let action = match &binding.action {
            BindingAction::Unbound => Ok(Box::new(NoAction) as Box<dyn gpui::Action>),
            BindingAction::Named { name, data } => {
                let data = match data {
                    None => None,
                    Some(text) => match serde_json::from_str(text) {
                        Ok(value) => Some(value),
                        Err(error) => {
                            notes.push(format!(
                                "keymap binding {:?} ignored: bad action arguments ({error})",
                                binding.keystrokes
                            ));
                            continue;
                        }
                    },
                };
                cx.build_action(name, data)
            }
        };
        let action = match action {
            Ok(action) => action,
            Err(error) => {
                notes.push(format!(
                    "keymap binding {:?} ignored: {error}",
                    binding.keystrokes
                ));
                continue;
            }
        };
        let predicate = match &binding.context {
            None => None,
            Some(context) => match gpui::KeyBindingContextPredicate::parse(context) {
                Ok(predicate) => Some(predicate.into()),
                Err(error) => {
                    notes.push(format!(
                        "keymap binding {:?} ignored: bad context {:?} ({error})",
                        binding.keystrokes, context
                    ));
                    continue;
                }
            },
        };
        match KeyBinding::load(
            &binding.keystrokes,
            action,
            predicate,
            false,
            None,
            &DummyKeyboardMapper,
        ) {
            Ok(binding) => bindings.push(binding),
            Err(error) => notes.push(format!(
                "keymap binding {:?} ignored: {error}",
                binding.keystrokes
            )),
        }
    }
    (bindings, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zed_file_shape_parses_into_contexted_bindings() {
        let (parsed, notes) = parse_keymap(
            r#"// my keymap
            [
                {
                    "context": "Browse",
                    "bindings": {
                        "j": "k10s_shell::RowDown",
                        "k": "k10s_shell::RowUp", // vim-ish
                    }
                },
                {
                    "bindings": { "ctrl-q": null }
                },
            ]"#,
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].context.as_deref(), Some("Browse"));
        assert_eq!(
            parsed[0].action,
            BindingAction::Named {
                name: "k10s_shell::RowDown".to_string(),
                data: None,
            }
        );
        assert_eq!(parsed[2].context, None);
        assert_eq!(parsed[2].action, BindingAction::Unbound);
    }

    #[test]
    fn an_array_action_carries_its_arguments() {
        let (parsed, notes) =
            parse_keymap(r#"[{"bindings": {"x": ["some::Action", {"deep": true}]}}]"#);
        assert!(notes.is_empty(), "{notes:?}");
        let BindingAction::Named { name, data } = &parsed[0].action else {
            panic!("{parsed:?}");
        };
        assert_eq!(name, "some::Action");
        assert_eq!(data.as_deref(), Some(r#"{"deep":true}"#));
    }

    #[test]
    fn every_malformed_shape_is_a_note_not_a_panic() {
        let cases = [
            ("{}", "must be an array"),
            ("[3]", "not an object"),
            (r#"[{"context": 3, "bindings": {}}]"#, "must be a string"),
            (r#"[{"context": "X"}]"#, "no \"bindings\""),
            (r#"[{"bindings": {"a": 3}}]"#, "expected an action name"),
            (r#"[{"bindings": {"a": ["only-name"]}}]"#, "array action"),
            ("not json", "not valid JSON"),
        ];
        for (text, want) in cases {
            let (parsed, notes) = parse_keymap(text);
            assert!(parsed.is_empty(), "{text}: {parsed:?}");
            assert!(
                notes.iter().any(|note| note.contains(want)),
                "{text}: {notes:?}"
            );
        }
        let (parsed, notes) = parse_keymap("");
        assert!(parsed.is_empty() && notes.is_empty());
    }
}
