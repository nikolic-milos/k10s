//! The command palette: a fuzzy query over the action registry.
//!
//! Declaring actions properly made this nearly free, exactly as the ROADMAP
//! predicted: the candidate list is every registered `k10s_shell::` and
//! `k10s_map::` action, humanized the way Zed does it ("shell: open
//! browser"), scored by a small subsequence matcher that rewards word starts
//! and contiguity. Confirming restores focus to whoever had it and then
//! dispatches, so the command lands exactly where the keystroke would have.
//! The centered surface picker uses the same matcher with named entries from
//! cached discovery. Confirming opens or focuses the selected surface. Scoring
//! and humanizing are pure and tested; the view repaints on notify.

use gpui::{
    App, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement, Render, Role, ScrollWheelEvent, SharedString, Styled, Window,
    div, prelude::*, px, rgb, svg,
};

use crate::activity::ActivityId;
use crate::surfaces::Surface;
use crate::ui::{LIST_ROW_HEIGHT, MODAL_MAX_HEIGHT, MODAL_WIDTH};
use crate::workspace::ObservabilityNavigation;
use crate::{
    CancelInput, CommitInput, DeleteInputChar, RowDown, RowEnd, RowHome, RowPageDown, RowPageUp,
    RowUp,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub display: String,
}

/// The k10s actions among every name gpui registered. gpui lists an action's
/// deprecated aliases among its names; an old name kept so that a keymap
/// written before a rename still binds is not an action of its own, so the
/// palette does not show it and the keymap schema does not complete it.
pub(crate) fn k10s_action_names<'a>(
    names: &'a [&'static str],
    deprecated: impl Fn(&str) -> bool + 'a,
) -> impl Iterator<Item = &'static str> + 'a {
    names
        .iter()
        .copied()
        .filter(|name| name.starts_with("k10s_shell::") || name.starts_with("k10s_map::"))
        .filter(move |name| !deprecated(name))
}

/// The k10s actions this build registered, for the palette and the keymap
/// schema alike, so the two cannot disagree about what an action is.
pub(crate) fn k10s_actions(cx: &App) -> impl Iterator<Item = &'static str> + '_ {
    let deprecated = cx.deprecated_actions_to_preferred_actions();
    k10s_action_names(cx.all_action_names(), |name| deprecated.contains_key(name))
}

pub fn commands(cx: &App) -> Vec<Command> {
    let mut commands: Vec<Command> = k10s_actions(cx)
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

/// The same score as [`fuzzy_match`], without building the ranges or the
/// character vector it builds them from.
///
/// It exists because ranking a cluster is a different problem from ranking a
/// command list. Fifty thousand candidates scanned per keystroke through
/// `fuzzy_match` is a hundred thousand allocations for ranges nobody will read:
/// only the handful of rows that survive the sort are ever highlighted. Measured
/// at 51,600 candidates, going through this instead took the keystroke from
/// 5.50 ms to well inside where it belongs.
///
/// The duplication is the cost of that, and it is held by
/// `the_two_scorers_never_disagree`, which sweeps both over the same corpus:
/// two scorers that drift apart would sort the list one way and highlight it
/// another, which looks like a ranking bug and is not.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut chars = candidate
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c));
    let mut score = 0i64;
    // Character positions, not byte offsets, because that is what the distance
    // and adjacency terms below are counted in.
    let mut at = 0usize;
    let mut previous_hit: Option<usize> = None;
    // The character immediately before `at`. One forward pass over the candidate
    // serves every needle, because `at` only ever moves forward.
    let mut before: Option<char> = None;
    for needle in query.chars().flat_map(|c| c.to_lowercase()) {
        if needle == ' ' {
            continue;
        }
        let mut found = at;
        loop {
            let c = chars.next()?;
            if c == needle {
                break;
            }
            before = Some(c);
            found += 1;
        }
        score += 10;
        if found == 0 || matches!(before, Some(' ' | ':')) {
            score += 8;
        }
        if previous_hit == Some(found.wrapping_sub(1)) {
            score += 5;
        }
        score -= (found - at) as i64;
        previous_hit = Some(found);
        before = Some(needle);
        at = found + 1;
    }
    Some(score)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaletteEvent {
    Dismissed,
    Confirmed(&'static str),
    SurfaceConfirmed(ActivityId),
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
    surfaces: Option<SurfaceMenu>,
    wheel_remainder: f32,
}

struct SurfaceMenu {
    navigation: ObservabilityNavigation,
    entries: Vec<Surface>,
}

fn query_matches(query: &str, commands: &[Command], surfaces: Option<&SurfaceMenu>) -> Vec<Match> {
    let mut scored: Vec<(i64, Match)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            fuzzy_match(query, &command.display)
                .or_else(|| {
                    let entry = surfaces?.entries.get(index)?;
                    fuzzy_score(query, crate::surfaces::search_terms(entry.activity.id))
                        .map(|score| (score, Vec::new()))
                })
                .map(|(score, hits)| {
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
    scored.into_iter().map(|(_, matched)| matched).collect()
}

fn retained_selection(name: Option<&str>, commands: &[Command], matches: &[Match]) -> usize {
    name.and_then(|name| {
        matches
            .iter()
            .position(|matched| commands[matched.command].name == name)
    })
    .unwrap_or(0)
}

fn wheel_rows(remainder: &mut f32, delta: f32, row_height: f32) -> isize {
    *remainder -= delta;
    let rows = (*remainder / row_height).trunc() as isize;
    *remainder -= rows as f32 * row_height;
    rows
}

impl EventEmitter<PaletteEvent> for PaletteView {}

const VISIBLE_ROWS: usize = 12;

impl PaletteView {
    pub fn new(cx: &mut Context<Self>) -> PaletteView {
        let commands = commands(cx);
        Self::with_commands(commands, None, cx)
    }

    pub(crate) fn for_surfaces(
        navigation: ObservabilityNavigation,
        state: crate::activity::RailState,
        cx: &mut Context<Self>,
    ) -> Self {
        let menu = SurfaceMenu {
            navigation,
            entries: crate::surfaces::entries(&navigation),
        };
        let selected = crate::surfaces::initial_selection(&menu.entries, state);
        let mut view = Self::with_commands(Self::surface_commands(&menu), Some(menu), cx);
        view.selected = selected;
        view
    }

    fn surface_commands(menu: &SurfaceMenu) -> Vec<Command> {
        menu.entries
            .iter()
            .map(|entry| Command {
                name: entry.activity.id.action().name(),
                display: entry.activity.label.to_owned(),
            })
            .collect()
    }

    fn with_commands(
        commands: Vec<Command>,
        surfaces: Option<SurfaceMenu>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = PaletteView {
            focus: cx.focus_handle(),
            commands,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            surfaces,
            wheel_remainder: 0.0,
        };
        view.requery();
        view
    }

    pub(crate) fn is_surface_picker(&self) -> bool {
        self.surfaces.is_some()
    }

    pub(crate) fn update_surfaces(
        &mut self,
        navigation: ObservabilityNavigation,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = &self.surfaces else { return };
        if menu.navigation == navigation {
            return;
        }
        let selected = self
            .matches
            .get(self.selected)
            .map(|matched| self.commands[matched.command].name);
        let menu = SurfaceMenu {
            navigation,
            entries: crate::surfaces::entries(&navigation),
        };
        self.commands = Self::surface_commands(&menu);
        self.surfaces = Some(menu);
        self.requery();
        self.selected = retained_selection(selected, &self.commands, &self.matches);
        cx.notify();
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn requery(&mut self) {
        self.matches = query_matches(&self.query, &self.commands, self.surfaces.as_ref());
        self.selected = 0;
        self.wheel_remainder = 0.0;
    }

    fn move_selection(&mut self, delta: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.matches.len().saturating_sub(1));
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let event = match self.matches.get(self.selected) {
            Some(matched) => match &self.surfaces {
                Some(menu) => {
                    PaletteEvent::SurfaceConfirmed(menu.entries[matched.command].activity.id)
                }
                None => PaletteEvent::Confirmed(self.commands[matched.command].name),
            },
            None => PaletteEvent::Dismissed,
        };
        cx.emit(event);
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

fn palette_row_height(fonts: &k10s_theme::Typography, surfaces: bool) -> f32 {
    if surfaces {
        crate::ui::navigation_row_height(fonts, true)
    } else {
        LIST_ROW_HEIGHT.max(crate::ui::ui_line_height(fonts.small()) + crate::ui::CONTENT_PADDING)
    }
}

fn palette_query_height(fonts: &k10s_theme::Typography, surfaces: bool) -> f32 {
    let minimum: f32 = if surfaces { 48.0 } else { 36.0 };
    minimum.max(crate::ui::ui_line_height(fonts.ui_size) + 16.0)
}

impl Render for PaletteView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let surfaces = self.is_surface_picker();
        let row_height = palette_row_height(&fonts, surfaces);
        let query_height = palette_query_height(&fonts, surfaces);
        let maximum_height = if surfaces { 560.0 } else { MODAL_MAX_HEIGHT };
        let maximum_height =
            maximum_height.min((f32::from(window.viewport_size().height) - 48.0).max(0.0));
        let visible_rows =
            crate::ui::capacity(maximum_height, query_height + 8.0, row_height, VISIBLE_ROWS)
                .max(1);
        let first = self
            .selected
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(
                self.matches
                    .len()
                    .saturating_sub(visible_rows.min(self.matches.len())),
            );
        struct Row {
            at: usize,
            selected: bool,
            display: String,
            hits: Vec<std::ops::Range<usize>>,
            hint: Option<String>,
            surface: Option<Surface>,
        }
        let window_rows: Vec<Row> = self
            .matches
            .iter()
            .enumerate()
            .skip(first)
            .take(visible_rows)
            .map(|(at, matched)| {
                let command = &self.commands[matched.command];
                Row {
                    at,
                    selected: at == self.selected,
                    display: command.display.clone(),
                    hits: matched.hits.clone(),
                    hint: Self::hint(command, window, cx),
                    surface: self
                        .surfaces
                        .as_ref()
                        .map(|menu| menu.entries[matched.command]),
                }
            })
            .collect();

        div()
            .id("command-palette")
            .key_context("Palette")
            .track_focus(&self.focus)
            .w(px(MODAL_WIDTH))
            .max_w_full()
            .max_h(px(maximum_height))
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
            .aria_label(if surfaces {
                "Open a surface"
            } else {
                "Command palette"
            })
            .on_action(cx.listener(|this, _: &RowUp, _, cx| {
                this.move_selection(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                this.move_selection(1);
                cx.notify();
            }))
            .on_action(cx.listener(move |this, _: &RowPageUp, _, cx| {
                this.move_selection(-(visible_rows as isize));
                cx.notify();
            }))
            .on_action(cx.listener(move |this, _: &RowPageDown, _, cx| {
                this.move_selection(visible_rows as isize);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowHome, _, cx| {
                this.selected = 0;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowEnd, _, cx| {
                this.selected = this.matches.len().saturating_sub(1);
                cx.notify();
            }))
            .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                let delta = f32::from(event.delta.pixel_delta(px(row_height)).y);
                let rows = wheel_rows(&mut this.wheel_remainder, delta, row_height);
                let previous = this.selected;
                this.move_selection(rows);
                if this.selected != previous {
                    cx.notify();
                }
                cx.stop_propagation();
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
                if let Some(key_char) = &keystroke.key_char
                    && !key_char.is_empty()
                {
                    this.query.push_str(key_char);
                    this.requery();
                    cx.notify();
                    cx.stop_propagation();
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
                    .h(px(fonts.ui_size.max(18.0)))
                    .flex_none()
                    .bg(rgb(theme.shell.cursor));
                let row = div()
                    .id("command-query")
                    .h(px(query_height))
                    .flex_none()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .text_size(px(fonts.ui_size))
                    .line_height(px(crate::ui::ui_line_height(fonts.ui_size)))
                    .border_b_1()
                    .border_color(rgb(theme.shell.border))
                    .role(Role::TextInput)
                    .aria_label(if surfaces {
                        "Find a tool or view"
                    } else {
                        "Command query"
                    });
                if self.query.is_empty() {
                    row.child(caret).child(
                        div()
                            .text_color(rgb(theme.shell.text_placeholder))
                            .whitespace_nowrap()
                            .child(if surfaces {
                                "Find a tool or view…"
                            } else {
                                "Execute a command…"
                            }),
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
                    .aria_label(if surfaces {
                        "Tools and views"
                    } else {
                        "Commands"
                    })
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
                            .h(px(row_height))
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
                            .line_height(px(crate::ui::ui_line_height(fonts.small())))
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
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(12.0))
                                    .min_w(px(0.0))
                                    .children(row.surface.map(|entry| {
                                        svg()
                                            .path(entry.activity.icon)
                                            .size(px(24.0))
                                            .flex_none()
                                            .text_color(rgb(theme.shell.text_muted))
                                    }))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .min_w(px(0.0))
                                            .overflow_hidden()
                                            .child(
                                                gpui::StyledText::new(row.display)
                                                    .with_highlights(highlights),
                                            )
                                            .children(
                                                row.surface.and_then(|entry| entry.notice).map(
                                                    |notice| {
                                                        div()
                                                            .text_size(px(fonts.xsmall()))
                                                            .line_height(px(
                                                                crate::ui::ui_line_height(
                                                                    fonts.xsmall(),
                                                                ),
                                                            ))
                                                            .text_color(rgb(theme.shell.text_muted))
                                                            .child(notice)
                                                    },
                                                ),
                                            ),
                                    ),
                            )
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
                            .h(px(row_height))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .text_size(px(fonts.small()))
                            .line_height(px(crate::ui::ui_line_height(fonts.small())))
                            .text_color(rgb(theme.shell.text_muted))
                            .child(if surfaces {
                                "No tool or view matches"
                            } else {
                                "No command matches"
                            })
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::ActivityId;
    use k10s_provider::ToolPresence;

    #[test]
    fn both_palette_modes_contain_scaled_text_and_share_their_wheel_geometry() {
        for half_pixels in 16..=64 {
            let fonts = k10s_theme::Typography {
                ui_size: half_pixels as f32 * 0.5,
                ..Default::default()
            };
            for surfaces in [false, true] {
                let row = palette_row_height(&fonts, surfaces);
                let query = palette_query_height(&fonts, surfaces);
                assert!(row >= crate::ui::ui_line_height(fonts.small()) + 8.0);
                assert!(query >= crate::ui::ui_line_height(fonts.ui_size) + 16.0);
                if surfaces {
                    assert!(
                        row >= crate::ui::ui_line_height(fonts.small())
                            + crate::ui::ui_line_height(fonts.xsmall())
                            + 8.0
                    );
                }
                let mut remainder = 0.0;
                assert_eq!(wheel_rows(&mut remainder, -row * 0.5, row), 0);
                assert_eq!(wheel_rows(&mut remainder, -row * 0.5, row), 1);
                assert_eq!(remainder, 0.0);
            }
        }
    }

    #[test]
    fn pixel_wheel_events_accumulate_rows_and_preserve_direction_changes() {
        let mut remainder = 0.0;
        assert_eq!(wheel_rows(&mut remainder, -20.0, 44.0), 0);
        assert_eq!(wheel_rows(&mut remainder, -20.0, 44.0), 0);
        assert_eq!(wheel_rows(&mut remainder, -20.0, 44.0), 1);
        assert!((remainder - 16.0).abs() < 0.001);
        assert_eq!(wheel_rows(&mut remainder, 16.0, 44.0), 0);
        assert!(remainder.abs() < 0.001);
        assert_eq!(wheel_rows(&mut remainder, 44.0, 44.0), -1);
        assert_eq!(wheel_rows(&mut remainder, -132.0, 44.0), 3);
        assert_eq!(wheel_rows(&mut remainder, 0.0, 44.0), 0);
        assert!(remainder.abs() < 0.001);
    }

    fn menu(navigation: ObservabilityNavigation) -> SurfaceMenu {
        SurfaceMenu {
            navigation,
            entries: crate::surfaces::entries(&navigation),
        }
    }

    #[test]
    fn surface_queries_find_backend_names_without_highlighting_unrelated_label_bytes() {
        let menu = menu(ObservabilityNavigation::default());
        let commands = PaletteView::surface_commands(&menu);
        for (query, id) in [
            ("Prometheus", ActivityId::Observe),
            ("Grafana", ActivityId::Observe),
            ("Loki", ActivityId::Observe),
            ("Tempo", ActivityId::Traces),
            ("Jaeger", ActivityId::Traces),
            ("OpenTelemetry", ActivityId::Collectors),
        ] {
            let matches = query_matches(query, &commands, Some(&menu));
            assert!(!matches.is_empty(), "{query}");
            assert_eq!(menu.entries[matches[0].command].activity.id, id, "{query}");
            assert!(matches[0].hits.is_empty(), "{query}");
        }
        let matches = query_matches("Traces", &commands, Some(&menu));
        assert_eq!(
            menu.entries[matches[0].command].activity.id,
            ActivityId::Traces
        );
        assert_eq!(matches[0].hits, vec![0..6]);
        assert!(query_matches("Tempo", &commands, None).is_empty());
        assert!(query_matches("no-such-tool", &commands, Some(&menu)).is_empty());
    }

    #[test]
    fn discovery_updates_keep_the_selected_action_when_earlier_rows_disappear() {
        let unknown = menu(ObservabilityNavigation::default());
        let commands = PaletteView::surface_commands(&unknown);
        let matches = query_matches("", &commands, Some(&unknown));
        let selected = unknown
            .entries
            .iter()
            .position(|entry| entry.activity.id == ActivityId::Traces)
            .unwrap();
        let name = commands[matches[selected].command].name;
        let updated = menu(ObservabilityNavigation {
            observe: Some(ToolPresence::Missing),
            traces: Some(ToolPresence::Blocked),
            collectors: Some(ToolPresence::Ready),
        });
        let commands = PaletteView::surface_commands(&updated);
        let matches = query_matches("", &commands, Some(&updated));
        let selected = retained_selection(Some(name), &commands, &matches);
        assert_eq!(
            updated.entries[matches[selected].command].activity.id,
            ActivityId::Traces
        );
        assert_eq!(
            updated.entries[matches[selected].command].notice,
            Some("Unavailable")
        );
        let observe = unknown
            .entries
            .iter()
            .position(|entry| entry.activity.id == ActivityId::Observe)
            .unwrap();
        let old_commands = PaletteView::surface_commands(&unknown);
        assert_eq!(
            retained_selection(Some(old_commands[observe].name), &commands, &matches),
            0
        );
        assert!(query_matches("Prometheus", &commands, Some(&updated)).is_empty());
        assert_eq!(retained_selection(Some(name), &commands, &[]), 0);
    }

    #[test]
    fn action_names_humanize_the_way_zed_users_expect() {
        assert_eq!(humanize("k10s_shell::OpenBrowser"), "shell: open browser");
        assert_eq!(humanize("k10s_map::FitView"), "map: fit view");
        assert_eq!(
            humanize("k10s_shell::ToggleTimestamps"),
            "shell: toggle timestamps"
        );
    }

    // Two scorers, one score. They are written apart on purpose -- one builds the
    // ranges a row highlights, the other refuses to allocate at all -- and the
    // only thing making that safe is that they never disagree. If they drifted,
    // a list would sort by one and highlight by the other, which reads as a
    // ranking bug and is not one.
    #[test]
    fn the_two_scorers_never_disagree() {
        let candidates = [
            "",
            "a",
            " leading space",
            "shell: open browser",
            "map: fit view",
            "prod/checkout-7-12",
            "team-3/checkout-0",
            "UPPER CASE THING",
            "colon:separated:words",
            "réservé-café",
            "日本語のラベル",
            "trailing space ",
            "aaaaaaaaaaaaaaaaaaaa",
        ];
        let queries = [
            "",
            " ",
            "a",
            "z",
            "ob",
            "open",
            "openbrowser",
            "browser open",
            "fit",
            "c7",
            "checkout012",
            "team3checkout0",
            "REserve",
            "café",
            "本語",
            "  spaced  out  ",
            "aaaaaaaaaaaaaaaaaaaaa",
        ];
        for candidate in candidates {
            for query in queries {
                assert_eq!(
                    fuzzy_score(query, candidate),
                    fuzzy_match(query, candidate).map(|(score, _)| score),
                    "the scorers disagree on {query:?} against {candidate:?}"
                );
            }
        }
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

    // The launch screen is reopenable *because* it is a command, so the palette
    // listing it is the feature, not a side effect of one. `OpenPalette` is
    // hidden by name and this reads similarly enough to be hidden with it by
    // accident.
    #[test]
    fn the_cluster_chooser_is_searchable_by_the_words_someone_would_type() {
        assert!(palette_visible("k10s_shell::ChooseCluster"));
        assert_eq!(
            humanize("k10s_shell::ChooseCluster"),
            "shell: choose cluster"
        );
        for query in ["cluster", "choose", "chc"] {
            assert!(
                fuzzy_score(query, "shell: choose cluster").is_some(),
                "{query:?} finds nothing"
            );
        }
    }

    // §6.7 asks that a feature be keyboard reachable *and* in the palette. The
    // write path is the one where discoverability matters most, and the trap is
    // structural: anything named `Editor*` is hidden unless it is listed, so a
    // diff or apply command named that way would vanish silently.
    #[test]
    fn the_write_path_commands_are_all_reachable_from_the_palette() {
        for visible in [
            "k10s_shell::DiffAgainstLoadedCopy",
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
            humanize("k10s_shell::DiffAgainstLoadedCopy"),
            "shell: diff against loaded copy"
        );
    }

    // The local comparison and taking a hunk are against the copy this editor
    // read, and their names say so; a keymap written for an old name still
    // binds it.
    #[test]
    fn the_local_diff_and_take_keep_their_old_names_as_aliases() {
        assert_eq!(
            <crate::DiffAgainstLoadedCopy as gpui::Action>::deprecated_aliases(),
            ["k10s_shell::DiffAgainstLive"]
        );
        assert_eq!(
            <crate::TakeFromLoadedCopy as gpui::Action>::deprecated_aliases(),
            ["k10s_shell::KeepTheirs"]
        );
        assert_eq!(
            humanize("k10s_shell::TakeFromLoadedCopy"),
            "shell: take from loaded copy"
        );
    }

    // An alias is a name gpui resolves, not a command: the palette and the
    // keymap schema both leave it out.
    #[test]
    fn an_old_name_kept_for_keymaps_is_not_offered_as_an_action() {
        let names = [
            "k10s_shell::DiffAgainstLive",
            "k10s_shell::DiffAgainstLoadedCopy",
            "menu::Confirm",
            "k10s_map::FitView",
        ];
        let offered: Vec<_> =
            k10s_action_names(&names, |name| name == "k10s_shell::DiffAgainstLive").collect();
        assert_eq!(
            offered,
            ["k10s_shell::DiffAgainstLoadedCopy", "k10s_map::FitView"]
        );
    }
}
