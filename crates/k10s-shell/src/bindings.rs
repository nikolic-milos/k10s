//! The default keymap: which keystroke reaches which action, in which context.
//!
//! Two rules shape the whole table. Every unmodified letter is either a map
//! command or something the embedded terminal has to be able to type, so a
//! workspace command that wants a letter takes a capital one -- and a capital is
//! `shift-<letter>`, which is *also* how that letter is typed, so both forms
//! need guarding. That guarding is [`input_suppressors`], derived from the table
//! rather than listed beside it: bindings dispatch before `on_key_down`, and a
//! `NoAction` binding in a deeper context suppresses the ancestor's, so an input
//! mode captures a key by out-nesting the command that wanted it. Deriving it
//! means a binding a *person* adds to their keymap is guarded automatically, and
//! an explicit binding in an input context always wins.
//!
//! Keystroke `Display` is the human form ("shift-F", uppercased letters) and is
//! never what anything here matches on: the canonical form is built from
//! `key()` and `modifiers()`.

use std::collections::BTreeSet;

use gpui::{KeyBinding, NoAction};

use crate::actions::*;

pub fn keybindings() -> Vec<KeyBinding> {
    let workspace = Some("Workspace");
    let browse = Some("Browse");
    let doc = Some("Doc");
    let typing = Some("Typing");
    let editor = Some("Editor");
    let diff = Some("Diff");
    let mut bindings = vec![
        KeyBinding::new("ctrl-shift-p", OpenPalette, workspace),
        KeyBinding::new("i", ToggleInspector, workspace),
        KeyBinding::new("escape", ClearSelection, workspace),
        KeyBinding::new("b", OpenBrowser, workspace),
        KeyBinding::new("n", OpenNodes, workspace),
        // shift-f, not f: the map holds default focus and binds f to FitView,
        // so a plain-f workspace command would be unreachable. Capital F is
        // also the browser's row-forward mnemonic, so F means forwards
        // everywhere.
        KeyBinding::new("shift-f", OpenForwards, workspace),
        // H for Helm, capitalised for the same reason F is: every unmodified
        // letter here is either a map command or something the terminal has to be
        // able to type.
        KeyBinding::new("shift-h", OpenReleases, workspace),
        KeyBinding::new("d", DescribeSelection, workspace),
        KeyBinding::new("l", LogsSelection, workspace),
        KeyBinding::new("s", ExecSelection, workspace),
        KeyBinding::new("ctrl-b", ToggleLeftDock, workspace),
        KeyBinding::new("ctrl-alt-b", ToggleRightDock, workspace),
        KeyBinding::new("ctrl-j", ToggleBottomDock, workspace),
        KeyBinding::new("ctrl-`", ToggleTerminal, workspace),
        KeyBinding::new("ctrl-q", Quit, workspace),
        KeyBinding::new("ctrl-tab", NextItem, workspace),
        KeyBinding::new("ctrl-shift-tab", PrevItem, workspace),
        KeyBinding::new("ctrl-w", CloseItem, workspace),
        KeyBinding::new("up", RowUp, browse),
        KeyBinding::new("down", RowDown, browse),
        KeyBinding::new("pageup", RowPageUp, browse),
        KeyBinding::new("pagedown", RowPageDown, browse),
        KeyBinding::new("home", RowHome, browse),
        KeyBinding::new("end", RowEnd, browse),
        KeyBinding::new("enter", OpenRow, browse),
        KeyBinding::new("l", LogsRow, browse),
        KeyBinding::new("s", ExecRow, browse),
        KeyBinding::new("r", Refresh, browse),
        KeyBinding::new("m", LoadMore, browse),
        KeyBinding::new("shift-f", StartForward, browse),
        KeyBinding::new("x", StopForward, browse),
        KeyBinding::new("/", EnterFilter, browse),
        KeyBinding::new("escape", Back, browse),
        KeyBinding::new("up", DocScrollUp, doc),
        KeyBinding::new("down", DocScrollDown, doc),
        KeyBinding::new("pageup", DocPageUp, doc),
        KeyBinding::new("pagedown", DocPageDown, doc),
        KeyBinding::new("home", DocHome, doc),
        KeyBinding::new("end", DocEnd, doc),
        KeyBinding::new("/", EnterSearch, doc),
        KeyBinding::new("n", NextMatch, doc),
        KeyBinding::new("shift-n", PrevMatch, doc),
        KeyBinding::new("f", ToggleFollow, doc),
        KeyBinding::new("t", ToggleTimestamps, doc),
        KeyBinding::new("c", CycleContainer, doc),
        KeyBinding::new("p", TogglePrevious, doc),
        KeyBinding::new("r", Reload, doc),
        KeyBinding::new("escape", CancelDoc, doc),
        KeyBinding::new("enter", CommitInput, typing),
        KeyBinding::new("escape", CancelInput, typing),
        KeyBinding::new("backspace", DeleteInputChar, typing),
        KeyBinding::new("shift-enter", PrevMatch, typing),
        KeyBinding::new("ctrl-enter", EditorReplaceAll, typing),
        KeyBinding::new("alt-r", EditorToggleRegex, typing),
        KeyBinding::new("y", EditSelection, workspace),
        KeyBinding::new("y", EditRow, browse),
        KeyBinding::new("up", EditorUp, editor),
        KeyBinding::new("down", EditorDown, editor),
        KeyBinding::new("left", EditorLeft, editor),
        KeyBinding::new("right", EditorRight, editor),
        KeyBinding::new("ctrl-left", EditorWordLeft, editor),
        KeyBinding::new("ctrl-right", EditorWordRight, editor),
        KeyBinding::new("home", EditorHome, editor),
        KeyBinding::new("end", EditorEnd, editor),
        KeyBinding::new("pageup", EditorPageUp, editor),
        KeyBinding::new("pagedown", EditorPageDown, editor),
        KeyBinding::new("ctrl-home", EditorDocStart, editor),
        KeyBinding::new("ctrl-end", EditorDocEnd, editor),
        KeyBinding::new("shift-up", EditorSelectUp, editor),
        KeyBinding::new("shift-down", EditorSelectDown, editor),
        KeyBinding::new("shift-left", EditorSelectLeft, editor),
        KeyBinding::new("shift-right", EditorSelectRight, editor),
        KeyBinding::new("ctrl-shift-left", EditorSelectWordLeft, editor),
        KeyBinding::new("ctrl-shift-right", EditorSelectWordRight, editor),
        KeyBinding::new("shift-home", EditorSelectHome, editor),
        KeyBinding::new("shift-end", EditorSelectEnd, editor),
        KeyBinding::new("ctrl-a", EditorSelectAll, editor),
        KeyBinding::new("backspace", EditorBackspace, editor),
        KeyBinding::new("shift-backspace", EditorBackspace, editor),
        KeyBinding::new("delete", EditorDelete, editor),
        KeyBinding::new("enter", EditorNewline, editor),
        KeyBinding::new("shift-enter", EditorNewline, editor),
        KeyBinding::new("tab", EditorTab, editor),
        KeyBinding::new("shift-tab", EditorShiftTab, editor),
        KeyBinding::new("ctrl-z", EditorUndo, editor),
        KeyBinding::new("ctrl-shift-z", EditorRedo, editor),
        KeyBinding::new("ctrl-shift-k", EditorDeleteLine, editor),
        KeyBinding::new("ctrl-/", EditorToggleComment, editor),
        KeyBinding::new("ctrl-alt-up", EditorCursorAbove, editor),
        KeyBinding::new("ctrl-alt-down", EditorCursorBelow, editor),
        KeyBinding::new("ctrl-d", EditorSelectNext, editor),
        KeyBinding::new("ctrl-space", EditorComplete, editor),
        KeyBinding::new("ctrl-f", EditorFind, editor),
        KeyBinding::new("ctrl-h", EditorReplace, editor),
        KeyBinding::new("f3", NextMatch, editor),
        KeyBinding::new("shift-f3", PrevMatch, editor),
        KeyBinding::new("escape", EditorCancel, editor),
        KeyBinding::new("ctrl-s", EditorSave, editor),
        KeyBinding::new("ctrl-shift-s", EditorSaveAs, editor),
        KeyBinding::new("ctrl-alt-d", DiffAgainstLive, editor),
        // The diff surface reuses the document scrolling actions rather than
        // declaring six of its own: the keys a reader already knows move a
        // diff the same way they move a describe document.
        KeyBinding::new("up", DocScrollUp, diff),
        KeyBinding::new("down", DocScrollDown, diff),
        KeyBinding::new("pageup", DocPageUp, diff),
        KeyBinding::new("pagedown", DocPageDown, diff),
        KeyBinding::new("home", DocHome, diff),
        KeyBinding::new("end", DocEnd, diff),
        KeyBinding::new("n", NextChange, diff),
        KeyBinding::new("shift-n", PrevChange, diff),
        KeyBinding::new("c", ToggleFolded, diff),
        // The one key in this context that changes a document. It edits the
        // buffer rather than the cluster, so it needs no second press: undo is
        // in the editor where the change lands.
        KeyBinding::new("t", KeepTheirs, diff),
        KeyBinding::new("r", Refresh, diff),
        KeyBinding::new("ctrl-alt-d", DiffAgainstLive, diff),
        KeyBinding::new("ctrl-alt-r", ApplyDryRun, diff),
        // The write, and the one that takes a field from another manager. Both
        // need a second press; neither can answer the other's question.
        KeyBinding::new("ctrl-s", ApplyToCluster, diff),
        KeyBinding::new("ctrl-shift-s", ForceApply, diff),
        KeyBinding::new("ctrl-o", OpenFile, workspace),
        KeyBinding::new("ctrl-shift-o", OpenFolder, workspace),
        KeyBinding::new("ctrl-alt-n", NewFile, workspace),
        KeyBinding::new("ctrl-p", FindFile, workspace),
        KeyBinding::new("ctrl-,", OpenSettings, workspace),
        KeyBinding::new("ctrl-k ctrl-s", OpenKeymap, workspace),
        // Zed's ctrl-k prefix, whose only other tenant here is the keymap file.
        // A chord rather than a plain chord-free key because every unmodified
        // letter is either a map command or something the terminal has to be
        // able to type, and the launch screen is not worth taking one from.
        KeyBinding::new("ctrl-k ctrl-c", ChooseCluster, workspace),
        KeyBinding::new("up", RowUp, Some("Palette")),
        KeyBinding::new("down", RowDown, Some("Palette")),
        KeyBinding::new("enter", CommitInput, Some("Palette")),
        KeyBinding::new("escape", CancelInput, Some("Palette")),
        KeyBinding::new("backspace", DeleteInputChar, Some("Palette")),
        KeyBinding::new("ctrl-up", PickParent, Some("Palette")),
    ];
    bindings.extend(k10s_map::keybindings());
    let suppressors = input_suppressors(bindings.iter());
    bindings.extend(suppressors);
    bindings
}

/// The contexts in which a keystroke means the character it prints rather than
/// the command someone bound to it.
const INPUT_CONTEXTS: [&str; 4] = ["Typing", "Palette", "Terminal", "Editor"];

/// Produce the deeper-context guards that keep an ancestor workspace command
/// from stealing keystrokes that type text (plain and shifted alike). This is
/// derived rather than listed by hand so newly added default and user
/// bindings are safe automatically. Explicit bindings in an input context win
/// and are never replaced.
pub fn input_suppressors<'a>(
    bindings: impl IntoIterator<Item = &'a KeyBinding>,
) -> Vec<KeyBinding> {
    let bindings: Vec<&KeyBinding> = bindings.into_iter().collect();

    let protected: BTreeSet<(String, String)> = bindings
        .iter()
        .filter_map(|binding| {
            let context = context_of(binding)?;
            if !INPUT_CONTEXTS.contains(&context.as_str()) {
                return None;
            }
            Some((context, typing_key(binding)?))
        })
        .collect();
    let captured: BTreeSet<String> = bindings
        .iter()
        .filter(|binding| matches!(context_of(binding).as_deref(), None | Some("Workspace")))
        .filter_map(|binding| typing_key(binding))
        .collect();

    captured
        .into_iter()
        .flat_map(|key| {
            INPUT_CONTEXTS.into_iter().filter_map({
                let protected = &protected;
                move |context| {
                    (!protected.contains(&(context.to_string(), key.clone())))
                        .then(|| KeyBinding::new(&key, NoAction, Some(context)))
                }
            })
        })
        .collect()
}

fn context_of(binding: &KeyBinding) -> Option<String> {
    binding.predicate().map(|predicate| predicate.to_string())
}

/// The canonical spelling of a binding whose single keystroke would otherwise
/// type text, or `None` if it would not. No chording modifier, but shift stays:
/// `shift-f` is how an F is typed, so a shift-letter workspace command must be
/// guarded exactly like a plain one.
fn typing_key(binding: &KeyBinding) -> Option<String> {
    let [stroke] = binding.keystrokes() else {
        return None;
    };
    let modifiers = stroke.modifiers();
    if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
        return None;
    }
    Some(if modifiers.shift {
        format!("shift-{}", stroke.key())
    } else {
        stroke.key().to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KEY_CONTEXTS;

    fn scoped_to<'a>(
        bindings: &'a [KeyBinding],
        context: &'a str,
    ) -> impl Iterator<Item = &'a KeyBinding> {
        bindings
            .iter()
            .filter(move |binding| context_of(binding).as_deref() == Some(context))
    }

    /// The same canonical form [`typing_key`] builds, for tests that have
    /// already established the binding is a single typing keystroke.
    fn typed(binding: &KeyBinding) -> String {
        typing_key(binding).expect("the caller filtered to single typing keystrokes")
    }

    fn types_text(binding: &KeyBinding) -> bool {
        typing_key(binding).is_some()
    }

    /// Whether a chord stays live in a context -- the ways out of an input mode
    /// must never be suppressed, or there is no way out.
    fn binds_chord(bindings: &[KeyBinding], context: &str, chord: &str) -> bool {
        scoped_to(bindings, context).any(|binding| format!("{}", binding.keystrokes()[0]) == chord)
    }

    #[test]
    fn every_binding_names_a_context_the_shell_actually_sets() {
        let bindings = keybindings();
        assert!(!bindings.is_empty());
        for binding in &bindings {
            let predicate = context_of(binding).unwrap_or_default();
            assert!(
                KEY_CONTEXTS.contains(&predicate.as_str()),
                "a binding is scoped to an unknown context: {predicate:?}"
            );
        }
    }

    #[test]
    fn the_terminal_captures_every_typing_workspace_binding_but_keeps_the_chords() {
        let bindings = keybindings();
        let workspace_typing: Vec<String> = scoped_to(&bindings, "Workspace")
            .filter(|binding| types_text(binding))
            .map(typed)
            .collect();
        assert!(
            workspace_typing.contains(&"escape".to_string()),
            "escape must reach the shell process, vim depends on it"
        );
        assert!(
            workspace_typing.contains(&"shift-f".to_string()),
            "shift-f is how an F is typed and must be guarded like a plain letter"
        );
        for key in &workspace_typing {
            assert!(
                scoped_to(&bindings, "Terminal").any(|binding| {
                    types_text(binding)
                        && typed(binding) == *key
                        && gpui::is_no_action(binding.action())
                }),
                "pressing {key:?} in a terminal would dispatch a command instead of typing"
            );
        }
        for chord in ["ctrl-tab", "ctrl-shift-tab", "ctrl-w"] {
            assert!(
                !binds_chord(&bindings, "Terminal", chord),
                "{chord} is the way out of a terminal and must stay live"
            );
        }
    }

    #[test]
    fn the_editor_types_every_plain_workspace_key_but_keeps_its_own_escape() {
        let bindings = keybindings();
        let editor_binding = |key: &str| {
            scoped_to(&bindings, "Editor")
                .filter(|binding| types_text(binding))
                .find(|binding| typed(binding) == key)
        };
        for key in ["b", "n", "d", "l", "s", "i", "y", "shift-f"] {
            let binding = editor_binding(key)
                .unwrap_or_else(|| panic!("{key} needs a guard or typing it runs a command"));
            assert!(
                gpui::is_no_action(binding.action()),
                "pressing {key} in the editor must type, not dispatch"
            );
        }
        let escape = editor_binding("escape").expect("escape is bound in the editor");
        assert!(
            !gpui::is_no_action(escape.action()),
            "escape is the editor's cancel, an explicit binding the suppressors must not replace"
        );
        for chord in ["ctrl-tab", "ctrl-shift-tab", "ctrl-w"] {
            assert!(
                !binds_chord(&bindings, "Editor", chord),
                "{chord} is the way out of the editor and must stay live"
            );
        }
    }

    #[test]
    fn the_default_map_focus_shadows_no_workspace_key() {
        let bindings = keybindings();
        let strokes_in = |context: &str| -> BTreeSet<String> {
            scoped_to(&bindings, context)
                .filter(|binding| !gpui::is_no_action(binding.action()))
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|stroke| stroke.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect()
        };
        let map = strokes_in("Map");
        let workspace = strokes_in("Workspace");
        let shadowed: Vec<&String> = map.intersection(&workspace).collect();
        assert!(
            shadowed.is_empty(),
            "the map holds default focus, so these Workspace bindings could never fire: \
             {shadowed:?}"
        );
    }

    #[test]
    fn typing_mode_suppresses_every_plain_letter_the_workspace_binds() {
        let bindings = keybindings();
        let workspace_letters: Vec<String> = scoped_to(&bindings, "Workspace")
            .filter(|binding| {
                binding.keystrokes().len() == 1
                    && binding.keystrokes()[0].modifiers().number_of_modifiers() == 0
                    && binding.keystrokes()[0].key().chars().count() == 1
            })
            .map(|binding| binding.keystrokes()[0].key().to_string())
            .collect();
        assert!(!workspace_letters.is_empty());
        for letter in &workspace_letters {
            for context in ["Typing", "Palette"] {
                assert!(
                    scoped_to(&bindings, context).any(|binding| {
                        binding.keystrokes().len() == 1
                            && binding.keystrokes()[0].key() == letter
                            && gpui::is_no_action(binding.action())
                    }),
                    "typing a {letter:?} into a {context} input would dispatch a command instead"
                );
            }
        }
    }

    #[test]
    fn user_workspace_bindings_are_guarded_without_overriding_explicit_input_bindings() {
        let mut all = keybindings();
        all.extend([
            KeyBinding::new("z", NoAction, Some("Workspace")),
            KeyBinding::new("q", NoAction, Some("Workspace")),
            KeyBinding::new("q", NoAction, Some("Palette")),
        ]);
        let suppressors = input_suppressors(all.iter());
        let has = |key: &str, context: &str| {
            suppressors.iter().any(|binding| {
                context_of(binding).as_deref() == Some(context)
                    && binding.keystrokes().len() == 1
                    && binding.keystrokes()[0].key() == key
                    && gpui::is_no_action(binding.action())
            })
        };

        for context in ["Typing", "Palette", "Terminal"] {
            assert!(has("z", context), "z is not guarded in {context}");
        }
        assert!(has("q", "Typing"));
        assert!(has("q", "Terminal"));
        assert!(
            !has("q", "Palette"),
            "an explicit Palette binding must remain authoritative"
        );
    }

    #[test]
    fn a_chorded_binding_is_never_mistaken_for_one_that_types() {
        // `ctrl-k ctrl-c` is two keystrokes and `ctrl-o` is one with a chording
        // modifier. Neither types a character, so neither may be suppressed --
        // suppressing a chord in an input context is how a command disappears.
        let bindings = keybindings();
        let chord = bindings
            .iter()
            .find(|binding| binding.keystrokes().len() == 2)
            .expect("the keymap has at least one two-stroke chord");
        assert!(
            typing_key(chord).is_none(),
            "a two-stroke chord does not type a character"
        );

        let control = scoped_to(&bindings, "Workspace")
            .find(|binding| {
                binding.keystrokes().len() == 1 && binding.keystrokes()[0].modifiers().control
            })
            .expect("the keymap has a single-stroke control binding");
        assert!(typing_key(control).is_none());

        for context in INPUT_CONTEXTS {
            assert!(
                !binds_chord(&bindings, context, "ctrl-o"),
                "ctrl-o types nothing, so {context} must not have been handed a guard for it"
            );
        }
    }
}
