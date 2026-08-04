//! The command palette: a fuzzy query over the action registry.
//!
//! Declaring actions properly made this nearly free, exactly as the ROADMAP
//! predicted: the candidate list is every registered `k10s_shell::` and
//! `k10s_map::` action, humanized the way Zed does it ("shell: open
//! browser"), scored by a small subsequence matcher that rewards word starts
//! and contiguity. Confirming restores focus to whoever had it and then
//! dispatches, so the command lands exactly where the keystroke would have.
//! Scoring and humanizing are pure and tested; the view is the usual thin
//! layer that repaints on notify.

use gpui::{
    App, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement, Render, Role, SharedString, Styled, Window, div, prelude::*, px,
    rgb,
};

use crate::ui::{LIST_ROW_HEIGHT, MODAL_MAX_HEIGHT, MODAL_WIDTH};
use crate::{CancelInput, CommitInput, DeleteInputChar, RowDown, RowUp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub display: String,
}

pub fn commands(cx: &App) -> Vec<Command> {
    let mut commands: Vec<Command> = cx
        .all_action_names()
        .iter()
        .filter(|name| name.starts_with("k10s_shell::") || name.starts_with("k10s_map::"))
        .filter(|name| palette_visible(name))
        .map(|name| Command {
            name,
            display: humanize(name),
        })
        .collect();
    commands.sort_by(|a, b| a.display.cmp(&b.display));
    commands
}

fn palette_visible(name: &str) -> bool {
    // Editor plumbing (per-keystroke motion and edits) stays out; the
    // commands a person would search for -- save, find, replace, complete,
    // comment, undo -- stay in.
    const EDITOR_VISIBLE: [&str; 11] = [
        "k10s_shell::EditorSave",
        "k10s_shell::EditorSaveAs",
        "k10s_shell::EditorFind",
        "k10s_shell::EditorReplace",
        "k10s_shell::EditorReplaceAll",
        "k10s_shell::EditorComplete",
        "k10s_shell::EditorToggleComment",
        "k10s_shell::EditorUndo",
        "k10s_shell::EditorRedo",
        "k10s_shell::EditorSelectAll",
        "k10s_shell::EditorSelectNext",
    ];
    if name.starts_with("k10s_shell::Editor") {
        return EDITOR_VISIBLE.contains(&name);
    }
    !matches!(
        name,
        "k10s_shell::OpenPalette"
            | "k10s_shell::PickParent"
            | "k10s_shell::RowUp"
            | "k10s_shell::RowDown"
            | "k10s_shell::RowPageUp"
            | "k10s_shell::RowPageDown"
            | "k10s_shell::RowHome"
            | "k10s_shell::RowEnd"
            | "k10s_shell::DocScrollUp"
            | "k10s_shell::DocScrollDown"
            | "k10s_shell::DocPageUp"
            | "k10s_shell::DocPageDown"
            | "k10s_shell::DocHome"
            | "k10s_shell::DocEnd"
            | "k10s_shell::CommitInput"
            | "k10s_shell::CancelInput"
            | "k10s_shell::DeleteInputChar"
            | "k10s_shell::CancelDoc"
    )
}

// "k10s_shell::OpenBrowser" -> "shell: open browser", the shape Zed users
// type from muscle memory.
pub fn humanize(name: &str) -> String {
    let (namespace, action) = name.split_once("::").unwrap_or(("", name));
    let namespace = namespace.strip_prefix("k10s_").unwrap_or(namespace);
    let mut out = String::with_capacity(name.len() + 4);
    out.push_str(namespace);
    out.push_str(": ");
    for (index, ch) in action.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                out.push(' ');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

// A case-insensitive subsequence match: None when the query does not appear
// in order; higher is better, favouring matches at word starts and runs of
// consecutive hits. The byte ranges name the hit characters in the original
// candidate so the list can paint them, the way Zed's picker highlights its
// fuzzy positions. Small on purpose -- ~40 commands do not need a smarter
// matcher, they need a predictable one.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i64, Vec<std::ops::Range<usize>>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let candidate: Vec<(std::ops::Range<usize>, char)> = candidate
        .char_indices()
        .map(|(start, c)| {
            (
                start..start + c.len_utf8(),
                c.to_lowercase().next().unwrap_or(c),
            )
        })
        .collect();
    let mut score = 0i64;
    let mut at = 0usize;
    let mut previous_hit: Option<usize> = None;
    let mut hits: Vec<std::ops::Range<usize>> = Vec::new();
    for needle in query.chars().flat_map(|c| c.to_lowercase()) {
        if needle == ' ' {
            continue;
        }
        let found = candidate[at..].iter().position(|(_, c)| *c == needle)? + at;
        score += 10;
        if found == 0 || matches!(candidate[found - 1].1, ' ' | ':') {
            score += 8;
        }
        if previous_hit == Some(found.wrapping_sub(1)) {
            score += 5;
        }
        score -= (found - at) as i64;
        previous_hit = Some(found);
        at = found + 1;
        let range = candidate[found].0.clone();
        match hits.last_mut() {
            Some(last) if last.end == range.start => last.end = range.end,
            _ => hits.push(range),
        }
    }
    Some((score, hits))
}

pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    fuzzy_match(query, candidate).map(|(score, _)| score)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEvent {
    Dismissed,
    Confirmed(&'static str),
}

// One surviving candidate: which command, and which bytes of its display
// string the query hit, kept so the row can paint the match.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Match {
    command: usize,
    hits: Vec<std::ops::Range<usize>>,
}

pub struct PaletteView {
    focus: FocusHandle,
    commands: Vec<Command>,
    query: String,
    matches: Vec<Match>,
    selected: usize,
}

impl EventEmitter<PaletteEvent> for PaletteView {}

const VISIBLE_ROWS: usize = 12;

impl PaletteView {
    pub fn new(cx: &mut Context<Self>) -> PaletteView {
        let commands = commands(cx);
        let mut view = PaletteView {
            focus: cx.focus_handle(),
            commands,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        view.requery();
        view
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn requery(&mut self) {
        let mut scored: Vec<(i64, Match)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(index, command)| {
                fuzzy_match(&self.query, &command.display).map(|(score, hits)| {
                    (
                        score,
                        Match {
                            command: index,
                            hits,
                        },
                    )
                })
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.command.cmp(&b.1.command)));
        self.matches = scored.into_iter().map(|(_, matched)| matched).collect();
        self.selected = 0;
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        match self.matches.get(self.selected) {
            Some(matched) => cx.emit(PaletteEvent::Confirmed(self.commands[matched.command].name)),
            None => cx.emit(PaletteEvent::Dismissed),
        }
    }

    fn hint(command: &Command, window: &Window, cx: &App) -> Option<String> {
        let action = cx.build_action(command.name, None).ok()?;
        let binding = window
            .bindings_for_action(action.as_ref())
            .into_iter()
            .next()?;
        let keys: Vec<String> = binding
            .keystrokes()
            .iter()
            .map(|keystroke| keystroke.inner().to_string())
            .collect();
        Some(keys.join(" "))
    }
}

impl Render for PaletteView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let first = self
            .selected
            .saturating_sub(VISIBLE_ROWS.saturating_sub(1))
            .min(
                self.matches
                    .len()
                    .saturating_sub(VISIBLE_ROWS.min(self.matches.len())),
            );
        struct Row {
            at: usize,
            selected: bool,
            display: String,
            hits: Vec<std::ops::Range<usize>>,
            hint: Option<String>,
        }
        let window_rows: Vec<Row> = self
            .matches
            .iter()
            .enumerate()
            .skip(first)
            .take(VISIBLE_ROWS)
            .map(|(at, matched)| {
                let command = &self.commands[matched.command];
                Row {
                    at,
                    selected: at == self.selected,
                    display: command.display.clone(),
                    hits: matched.hits.clone(),
                    hint: Self::hint(command, window, cx),
                }
            })
            .collect();

        div()
            .id("command-palette")
            .key_context("Palette")
            .track_focus(&self.focus)
            .w(px(MODAL_WIDTH))
            .max_w_full()
            .max_h(px(MODAL_MAX_HEIGHT))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme.shell.elevated_surface_background))
            .border_1()
            .border_color(rgb(theme.shell.border_variant))
            .rounded(px(8.0))
            .shadow_lg()
            .font_family(fonts.ui_family.clone())
            .role(Role::Dialog)
            .aria_label("Command palette")
            .on_action(cx.listener(|this, _: &RowUp, _, cx| {
                this.selected = this.selected.saturating_sub(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                if this.selected + 1 < this.matches.len() {
                    this.selected += 1;
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                this.confirm(cx);
            }))
            .on_action(cx.listener(|_, _: &CancelInput, _, cx| {
                cx.emit(PaletteEvent::Dismissed);
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                this.query.pop();
                this.requery();
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let keystroke = &event.keystroke;
                if keystroke.modifiers.control || keystroke.modifiers.alt {
                    return;
                }
                if let Some(key_char) = &keystroke.key_char {
                    this.query.push_str(key_char);
                    this.requery();
                    cx.notify();
                }
            }))
            .child({
                // The caret is a painted quad, never a text glyph: a glyph
                // caret comes from font fallback with its own advance and
                // side bearings, which reads as a phantom gap growing to the
                // right of the query. It sits flush after the text (before
                // the placeholder), exactly where Zed's editor paints its
                // bar cursor. Steady, not blinking: zero paints at idle is a
                // gated invariant even while a modal is open.
                let caret = div()
                    .w(px(2.0))
                    .h(px(18.0))
                    .flex_none()
                    .bg(rgb(theme.shell.cursor));
                let row = div()
                    .id("command-query")
                    .h(px(36.0))
                    .flex_none()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .text_size(px(fonts.ui_size))
                    .border_b_1()
                    .border_color(rgb(theme.shell.border))
                    .role(Role::TextInput)
                    .aria_label("Command query");
                if self.query.is_empty() {
                    row.child(caret).child(
                        div()
                            .text_color(rgb(theme.shell.text_placeholder))
                            .whitespace_nowrap()
                            .child("Execute a command…"),
                    )
                } else {
                    row.child(
                        div()
                            .text_color(rgb(theme.shell.text))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(SharedString::from(self.query.clone())),
                    )
                    .child(caret)
                }
            })
            .child(
                div()
                    .id("command-results")
                    .flex()
                    .flex_col()
                    .py(px(4.0))
                    .role(Role::ListBox)
                    .aria_label("Commands")
                    .children(window_rows.into_iter().map(|row| {
                        // The characters the query hit, painted accent the
                        // way Zed's HighlightedLabel shows fuzzy positions.
                        let highlights = row.hits.into_iter().map(|range| {
                            (
                                range,
                                gpui::HighlightStyle {
                                    color: Some(rgb(theme.shell.text_accent).into()),
                                    ..Default::default()
                                },
                            )
                        });
                        let at = row.at;
                        div()
                            .id(("command", at))
                            .h(px(LIST_ROW_HEIGHT))
                            .flex_none()
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .justify_between()
                            .cursor_pointer()
                            .bg(rgb(if row.selected {
                                theme.shell.element_selected
                            } else {
                                theme.shell.elevated_surface_background
                            }))
                            .hover(|hovered| hovered.bg(rgb(theme.shell.element_hover)))
                            .text_size(px(fonts.small()))
                            .text_color(rgb(theme.shell.text))
                            .role(Role::ListBoxOption)
                            .aria_label(row.display.clone())
                            .aria_selected(row.selected)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    this.selected = at;
                                    this.confirm(cx);
                                }),
                            )
                            .child(gpui::StyledText::new(row.display).with_highlights(highlights))
                            .children(row.hint.map(|hint| {
                                div()
                                    .px(px(4.0))
                                    .py(px(1.0))
                                    .rounded(px(3.0))
                                    .bg(rgb(theme.shell.element_background))
                                    .border_1()
                                    .border_color(rgb(theme.shell.border_variant))
                                    .text_size(px(fonts.xsmall()))
                                    .text_color(rgb(theme.shell.text_muted))
                                    .child(SharedString::from(hint))
                            }))
                    }))
                    .children(self.matches.is_empty().then(|| {
                        div()
                            .h(px(LIST_ROW_HEIGHT))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .text_size(px(fonts.small()))
                            .text_color(rgb(theme.shell.text_muted))
                            .child("No command matches")
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_humanize_the_way_zed_users_expect() {
        assert_eq!(humanize("k10s_shell::OpenBrowser"), "shell: open browser");
        assert_eq!(humanize("k10s_map::FitView"), "map: fit view");
        assert_eq!(
            humanize("k10s_shell::ToggleTimestamps"),
            "shell: toggle timestamps"
        );
    }

    #[test]
    fn the_matcher_is_a_subsequence_with_word_start_taste() {
        assert!(fuzzy_score("ob", "shell: open browser").is_some());
        assert!(fuzzy_score("browser open", "shell: open browser").is_none());
        assert!(fuzzy_score("zzz", "shell: open browser").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));

        let word_start = fuzzy_score("open", "shell: open browser").unwrap();
        let buried = fuzzy_score("open", "shell: reopen browser").unwrap();
        assert!(
            word_start > buried,
            "a word-start match must outrank a buried one: {word_start} vs {buried}"
        );

        let contiguous = fuzzy_score("fit", "map: fit view").unwrap();
        let scattered = fuzzy_score("fit", "shell: filter tables maybe").unwrap();
        assert!(contiguous > scattered, "{contiguous} vs {scattered}");
    }

    #[test]
    fn the_match_names_the_bytes_it_hit_so_the_row_can_paint_them() {
        let (_, hits) = fuzzy_match("ob", "shell: open browser").unwrap();
        assert_eq!(hits, vec![7..8, 12..13]);

        let (_, contiguous) = fuzzy_match("fit", "map: fit view").unwrap();
        assert_eq!(contiguous, vec![5..8], "adjacent hits merge into one range");

        let (_, none) = fuzzy_match("", "anything").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn a_case_insensitive_query_ignores_spaces() {
        assert_eq!(
            fuzzy_score("OpenBrowser", "shell: open browser"),
            fuzzy_score("openbrowser", "shell: open browser"),
        );
        assert!(fuzzy_score("open browser", "shell: open browser").is_some());
    }

    #[test]
    fn navigation_and_text_input_plumbing_stays_out_of_the_palette() {
        for hidden in [
            "k10s_shell::OpenPalette",
            "k10s_shell::RowDown",
            "k10s_shell::DocPageUp",
            "k10s_shell::DeleteInputChar",
        ] {
            assert!(!palette_visible(hidden), "{hidden}");
        }
        for visible in [
            "k10s_map::FitView",
            "k10s_shell::OpenBrowser",
            "k10s_shell::ToggleBottomDock",
            "k10s_shell::ToggleTimestamps",
        ] {
            assert!(palette_visible(visible), "{visible}");
        }
    }

    // §6.7 asks that a feature be keyboard reachable *and* in the palette. The
    // write path is the one where discoverability matters most, and the trap is
    // structural: anything named `Editor*` is hidden unless it is listed, so a
    // diff or apply command named that way would vanish silently.
    #[test]
    fn the_write_path_commands_are_all_reachable_from_the_palette() {
        for visible in [
            "k10s_shell::DiffAgainstLive",
            "k10s_shell::ApplyDryRun",
            "k10s_shell::ApplyToCluster",
            "k10s_shell::ForceApply",
            "k10s_shell::NextChange",
            "k10s_shell::PrevChange",
            "k10s_shell::ToggleFolded",
        ] {
            assert!(palette_visible(visible), "{visible}");
            assert!(
                !visible.starts_with("k10s_shell::Editor"),
                "{visible} would need adding to EDITOR_VISIBLE to be reachable"
            );
        }
        assert_eq!(
            humanize("k10s_shell::ApplyToCluster"),
            "shell: apply to cluster"
        );
        assert_eq!(
            humanize("k10s_shell::DiffAgainstLive"),
            "shell: diff against live"
        );
    }
}
