//! The editor's element: how a buffer becomes a window's worth of pixels.
//!
//! Zed's split, in this crate's flat-module form -- [`crate::editor`] owns the
//! state machine and this owns the paint. Visible rows come from
//! [`crate::spans`] already resolved into disjoint styled runs, so nothing here
//! decides priority; it decides geometry, hit-testing, the gutter, the
//! completion menu, the search bar and the status line. Typing reaches the
//! buffer through `key_char` exactly like the terminal does; named keys and
//! chords arrive as `Editor`-context actions bound in [`crate::bindings`].

use std::ops::Range;

use gpui::{
    Context, HighlightStyle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Render, Role, ScrollWheelEvent, SharedString,
    Styled, StyledText, Window, canvas, div, prelude::*, px, rgb,
};

use k10s_edit::{Motion, Replacement, Selection};

use crate::dirty::CloseStep;
use crate::editor::{
    COMPLETION_WIDTH, EditorEvent, EditorView, MAX_DOC_CHARS, MAX_VISIBLE_COMPLETIONS, SearchBar,
    gutter_width, shape_plain,
};
use crate::spans::{compose_line, flag_style};
use crate::ui::{CONTENT_PADDING, STATUS_BAR_HEIGHT};
use crate::{
    CancelInput, CommitInput, DeleteInputChar, EditorBackspace, EditorComplete, EditorCursorAbove,
    EditorCursorBelow, EditorDelete, EditorDeleteLine, EditorDocEnd, EditorDocStart, EditorDown,
    EditorEnd, EditorFind, EditorHome, EditorLeft, EditorNewline, EditorPageDown, EditorPageUp,
    EditorRedo, EditorReplace, EditorReplaceAll, EditorRight, EditorSelectAll, EditorSelectDown,
    EditorSelectEnd, EditorSelectHome, EditorSelectLeft, EditorSelectNext, EditorSelectRight,
    EditorSelectUp, EditorSelectWordLeft, EditorSelectWordRight, EditorShiftTab, EditorTab,
    EditorToggleComment, EditorToggleRegex, EditorUndo, EditorUp, EditorWordLeft, EditorWordRight,
    NextMatch, PrevMatch, Reload,
};

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let rope = self.buffer.rope();
        let len_lines = rope.len_lines();
        let gutter = gutter_width(len_lines, &fonts, window);
        let primary_row = rope.byte_to_point(self.buffer.primary_selection().head).row;
        let last_row = (self.scroll_top + self.rows).min(len_lines);

        let mut rendered_rows = Vec::with_capacity(last_row.saturating_sub(self.scroll_top));
        let visible = self.viewport_layers(self.scroll_top..last_row);
        for (index, row) in (self.scroll_top..last_row).enumerate() {
            let line = rope.line(row);
            let layers = &visible[index];
            let mut padded = line.clone();
            padded.push(' ');
            let highlights: Vec<(Range<usize>, HighlightStyle)> =
                compose_line(padded.len(), layers)
                    .into_iter()
                    .map(|(range, flags)| (range, flag_style(&theme, flags)))
                    .collect();
            rendered_rows.push((row, padded, highlights));
        }

        let completion_popup = self.completion.as_ref().and_then(|menu| {
            let anchor_point = rope.byte_to_point(menu.anchor);
            if anchor_point.row < self.scroll_top || anchor_point.row >= last_row {
                return None;
            }
            let line = rope.line(anchor_point.row);
            let prefix = &line[..anchor_point.column.min(line.len())];
            let x =
                CONTENT_PADDING + gutter + f32::from(shape_plain(prefix, &fonts, window).width());
            let y = CONTENT_PADDING
                + ((anchor_point.row - self.scroll_top + 1) as f32) * fonts.line_height();
            let first = menu
                .selected
                .saturating_sub(MAX_VISIBLE_COMPLETIONS - 1)
                .min(menu.items.len().saturating_sub(MAX_VISIBLE_COMPLETIONS));
            let selected_docs = menu.items.get(menu.selected).and_then(|item| {
                if item.documentation.is_empty() {
                    None
                } else {
                    let mut docs: String = item.documentation.chars().take(MAX_DOC_CHARS).collect();
                    if docs.len() < item.documentation.len() {
                        docs.push('…');
                    }
                    Some(docs)
                }
            });
            Some(
                div()
                    .absolute()
                    .top(px(y))
                    .left(px(x))
                    .w(px(COMPLETION_WIDTH))
                    .flex()
                    .flex_col()
                    .bg(rgb(theme.shell.elevated_surface_background))
                    .border_1()
                    .border_color(rgb(theme.shell.border))
                    .rounded_md()
                    .overflow_hidden()
                    .text_size(px(fonts.small()))
                    .children(
                        menu.items
                            .iter()
                            .enumerate()
                            .skip(first)
                            .take(MAX_VISIBLE_COMPLETIONS)
                            .map(|(index, item)| {
                                let selected = index == menu.selected;
                                let mut row = div()
                                    .px(px(8.0))
                                    .h(px(22.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2();
                                if selected {
                                    row = row.bg(rgb(theme.shell.element_selected));
                                }
                                row.child(
                                    div()
                                        .text_color(rgb(theme.shell.text))
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .child(SharedString::from(if item.required {
                                            format!("{}*", item.label)
                                        } else {
                                            item.label.clone()
                                        })),
                                )
                                .child(
                                    div()
                                        .text_color(rgb(theme.shell.text_muted))
                                        .whitespace_nowrap()
                                        .child(SharedString::from(item.detail.clone())),
                                )
                            }),
                    )
                    .children(selected_docs.map(|docs| {
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .border_t_1()
                            .border_color(rgb(theme.shell.border_variant))
                            .text_color(rgb(theme.shell.text_muted))
                            .child(SharedString::from(docs))
                    })),
            )
        });

        div()
            .id("editor-view")
            .key_context(if self.searching() { "Typing" } else { "Editor" })
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(theme.shell.editor_background))
            .font_family(fonts.buffer_family.clone())
            .text_color(rgb(theme.shell.editor_foreground))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.resize(
                                f32::from(bounds.size.width),
                                f32::from(bounds.size.height),
                                (f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
                                cx,
                            );
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_action(cx.listener(|this, _: &EditorUp, _, cx| {
                this.move_or_navigate(Motion::Up, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorDown, _, cx| {
                this.move_or_navigate(Motion::Down, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorLeft, _, cx| {
                this.move_or_navigate(Motion::Left, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorRight, _, cx| {
                this.move_or_navigate(Motion::Right, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorWordLeft, _, cx| {
                this.move_or_navigate(Motion::WordLeft, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorWordRight, _, cx| {
                this.move_or_navigate(Motion::WordRight, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorHome, _, cx| {
                this.move_or_navigate(Motion::Home, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorEnd, _, cx| {
                this.move_or_navigate(Motion::End, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorDocStart, _, cx| {
                this.move_or_navigate(Motion::DocStart, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorDocEnd, _, cx| {
                this.move_or_navigate(Motion::DocEnd, false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorPageUp, _, cx| {
                let rows = this.rows.saturating_sub(1).max(1);
                this.move_or_navigate(Motion::PageUp(rows), false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorPageDown, _, cx| {
                let rows = this.rows.saturating_sub(1).max(1);
                this.move_or_navigate(Motion::PageDown(rows), false, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectUp, _, cx| {
                this.move_or_navigate(Motion::Up, true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectDown, _, cx| {
                this.move_or_navigate(Motion::Down, true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectLeft, _, cx| {
                this.move_or_navigate(Motion::Left, true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectRight, _, cx| {
                this.move_or_navigate(Motion::Right, true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectWordLeft, _, cx| {
                this.move_or_navigate(Motion::WordLeft, true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectWordRight, _, cx| {
                this.move_or_navigate(Motion::WordRight, true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectHome, _, cx| {
                this.move_or_navigate(Motion::Home, true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectEnd, _, cx| {
                this.move_or_navigate(Motion::End, true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorSelectAll, _, cx| {
                this.completion = None;
                this.buffer.select_all();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EditorBackspace, _, cx| {
                let splices = this.buffer.backspace();
                this.after_edit(splices, cx);
                if this.completion.is_some() {
                    this.trigger_completion(false, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &EditorDelete, _, cx| {
                this.completion = None;
                let splices = this.buffer.delete_forward();
                this.after_edit(splices, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorNewline, _, cx| {
                if this.accept_completion(cx) {
                    return;
                }
                let splices = this.buffer.newline();
                this.after_edit(splices, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorTab, _, cx| {
                if this.accept_completion(cx) {
                    return;
                }
                let splices = this.buffer.indent();
                this.after_edit(splices, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorShiftTab, _, cx| {
                this.completion = None;
                let splices = this.buffer.outdent();
                this.after_edit(splices, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorUndo, _, cx| {
                this.completion = None;
                if this.buffer.undo() {
                    this.after_restore(cx);
                } else {
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &EditorRedo, _, cx| {
                this.completion = None;
                if this.buffer.redo() {
                    this.after_restore(cx);
                } else {
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &EditorDeleteLine, _, cx| {
                this.completion = None;
                let splices = this.buffer.delete_lines();
                this.after_edit(splices, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorToggleComment, _, cx| {
                this.completion = None;
                this.toggle_comment(cx);
            }))
            .on_action(cx.listener(|this, _: &EditorCursorAbove, _, cx| {
                this.completion = None;
                this.buffer.add_cursor_vertically(false);
                this.ensure_visible();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EditorCursorBelow, _, cx| {
                this.completion = None;
                this.buffer.add_cursor_vertically(true);
                this.ensure_visible();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EditorSelectNext, _, cx| {
                this.completion = None;
                this.buffer.select_next_occurrence();
                this.ensure_visible();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EditorComplete, _, cx| {
                this.trigger_completion(true, cx);
            }))
            .on_action(cx.listener(|this, _: &EditorFind, _, cx| {
                this.completion = None;
                let mut bar = SearchBar::new();
                let primary = this.buffer.primary_selection();
                if !primary.is_caret() {
                    bar.input = this
                        .buffer
                        .rope()
                        .slice_to_string(primary.start()..primary.end());
                }
                this.search = Some(bar);
                this.search_changed(cx);
            }))
            .on_action(cx.listener(|this, _: &EditorReplace, _, cx| {
                this.completion = None;
                if let Some(search) = &mut this.search {
                    search.replace = Some(search.replace.clone().unwrap_or_default());
                    search.typing_replace = true;
                } else {
                    let mut bar = SearchBar::new();
                    bar.replace = Some(String::new());
                    this.search = Some(bar);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &EditorToggleRegex, _, cx| {
                if let Some(search) = &mut this.search {
                    search.regex = !search.regex;
                }
                this.search_changed(cx);
            }))
            .on_action(cx.listener(|this, _: &EditorReplaceAll, _, cx| {
                let replaced = match &mut this.search {
                    Some(search) => {
                        let replacement = search.replace.clone().unwrap_or_default();
                        search.state.replace_all(&mut this.buffer, &replacement)
                    }
                    None => Replacement::default(),
                };
                if replaced.happened() {
                    let count = replaced.count;
                    this.after_edit(replaced.splices, cx);
                    this.status = Some(format!("replaced {count}"));
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextMatch, _, cx| {
                if let Some(search) = &mut this.search {
                    search.state.refresh(&this.buffer);
                    search.state.next();
                    this.jump_to_match(cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &PrevMatch, _, cx| {
                if let Some(search) = &mut this.search {
                    search.state.refresh(&this.buffer);
                    search.state.prev();
                    this.jump_to_match(cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                if this.search.is_none() {
                    cx.propagate();
                    return;
                }
                let replacing = this
                    .search
                    .as_ref()
                    .is_some_and(|search| search.typing_replace);
                if replacing {
                    let replaced = match &mut this.search {
                        Some(search) => {
                            let replacement = search.replace.clone().unwrap_or_default();
                            search.state.replace_current(&mut this.buffer, &replacement)
                        }
                        None => Replacement::default(),
                    };
                    if replaced.happened() {
                        this.after_edit(replaced.splices, cx);
                    }
                    this.jump_to_match(cx);
                } else {
                    if let Some(search) = &mut this.search {
                        search.state.refresh(&this.buffer);
                    }
                    this.jump_to_match(cx);
                    if let Some(search) = &mut this.search {
                        search.state.next();
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &CancelInput, _, cx| {
                if this.search.is_some() {
                    this.search = None;
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                let Some(search) = &mut this.search else {
                    cx.propagate();
                    return;
                };
                if search.pop() {
                    this.search_changed(cx);
                } else {
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &crate::EditorCancel, _, cx| {
                this.cancel(cx);
            }))
            .on_action(cx.listener(|this, _: &Reload, _, cx| {
                this.reload(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::EditorSave, _, cx| {
                this.save(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::DiffAgainstLive, _, cx| {
                // The predicate, not the comparison: the workspace builds the
                // sources itself, and building them here only to drop them
                // copies the document twice and walks the tree for a prune
                // whose answer is thrown away.
                if this.has_live_version() {
                    cx.emit(EditorEvent::DiffRequested { dry_run: false });
                } else {
                    this.status =
                        Some("only a cluster document has a live version to diff".to_string());
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &crate::ApplyDryRun, _, cx| {
                if this.has_live_version() {
                    cx.emit(EditorEvent::DiffRequested { dry_run: true });
                } else {
                    this.status = Some("only a cluster document can be applied".to_string());
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|_, _: &crate::EditorSaveAs, _, cx| {
                cx.emit(EditorEvent::SaveAsRequested);
            }))
            .on_action(cx.listener(|this, _: &crate::CloseItem, _, cx| {
                let version = this.buffer.version();
                if this.dirty.close_step(version) == CloseStep::Warn {
                    this.status = Some("unsaved changes; ctrl-w again to discard".to_string());
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let keystroke = &event.keystroke;
                if keystroke.modifiers.control
                    || keystroke.modifiers.alt
                    || keystroke.modifiers.platform
                    || keystroke.modifiers.function
                {
                    return;
                }
                let Some(key_char) = keystroke.key_char.clone() else {
                    return;
                };
                if let Some(search) = &mut this.search {
                    if keystroke.key == "tab" {
                        if search.toggle_field() {
                            cx.notify();
                        }
                        return;
                    }
                    if search.push(&key_char) {
                        this.search_changed(cx);
                    } else {
                        cx.notify();
                    }
                    return;
                }
                this.insert_text(&key_char, cx);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let row = k10s_theme::typography(cx).line_height();
                let delta = f32::from(event.delta.pixel_delta(px(row)).y);
                this.scroll_by(-(delta / row).round() as i64);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.completion = None;
                    let offset =
                        this.offset_for_mouse(k10s_theme::typography(cx), window, event.position);
                    if event.click_count >= 2 {
                        this.buffer
                            .set_selections(vec![Selection::caret(offset)], 0);
                        this.buffer.select_next_occurrence();
                    } else if event.modifiers.shift {
                        let anchor = this.buffer.primary_selection().anchor;
                        this.buffer
                            .set_selections(vec![Selection::range(anchor, offset)], 0);
                    } else {
                        this.buffer
                            .set_selections(vec![Selection::caret(offset)], 0);
                        this.dragging = true;
                    }
                    window.focus(&this.focus, cx);
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if !this.dragging {
                    return;
                }
                let offset =
                    this.offset_for_mouse(k10s_theme::typography(cx), window, event.position);
                let anchor = this.buffer.primary_selection().anchor;
                this.buffer
                    .set_selections(vec![Selection::range(anchor, offset)], 0);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.dragging = false;
                }),
            )
            .child(
                div()
                    .id("editor-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .p(px(CONTENT_PADDING))
                    .flex()
                    .flex_col()
                    .role(Role::Document)
                    .aria_label(self.title.clone())
                    .children(rendered_rows.into_iter().map(|(row, padded, highlights)| {
                        let active = row == primary_row;
                        let number = SharedString::from(format!("{}", row + 1));
                        let mut line_div = div()
                            .h(px(fonts.line_height()))
                            .flex_none()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .text_size(px(fonts.buffer_size))
                            .whitespace_nowrap();
                        if active {
                            let (color, alpha) = theme.syntax.active_line_background;
                            line_div = line_div.bg(rgb(color).alpha(alpha));
                        }
                        line_div
                            .child(
                                div()
                                    .w(px(gutter))
                                    .flex_none()
                                    .pr(px(CONTENT_PADDING))
                                    .text_size(px(fonts.small()))
                                    .text_color(if active {
                                        rgb(theme.syntax.active_line_number)
                                    } else {
                                        rgb(theme.syntax.line_number)
                                    })
                                    .child(div().child(number).ml_auto()),
                            )
                            .child(
                                StyledText::new(SharedString::from(padded))
                                    .with_highlights(highlights),
                            )
                    })),
            )
            .children(completion_popup)
            .child(
                div()
                    .h(px(STATUS_BAR_HEIGHT))
                    .flex_none()
                    .px(px(CONTENT_PADDING))
                    .flex()
                    .items_center()
                    .bg(rgb(theme.shell.panel_background))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.small()))
                    .text_color(if self.status.is_some() || self.searching() {
                        rgb(theme.shell.text)
                    } else {
                        rgb(theme.shell.text_muted)
                    })
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(SharedString::from(self.status_line())),
            )
    }
}
