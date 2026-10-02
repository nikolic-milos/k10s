//! The files panel: an opened folder as a left-dock tree.
//!
//! Zed's project panel at k10s scope: navigation, not file management --
//! expand, collapse, open in the editor. The tree is a flattened row list
//! rebuilt from the `Fs` seam on demand and refreshed on action (`r`), the
//! same deliberate no-watcher policy as the forwards view: a stale listing
//! labelled with how to refresh beats a filesystem watcher subsystem.
//!
//! Reading and deciding are separate: [`read_tree`] walks the open folders on
//! the background executor and hands back one listing, and [`FilesState::apply`]
//! is the pure step that adopts it, keeps the cursor on its file, and keeps the
//! expansion the user chose even for folders no row survived to represent.
//! Expanding a deep tree on the UI thread is how a panel freezes on a network
//! mount. The state machine is tested against the fake filesystem; the view
//! reuses the Browse key context so the tree navigates exactly like every other
//! list in the shell.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    Context, FocusHandle, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, Role,
    ScrollWheelEvent, SharedString, Styled, Window, canvas, div, prelude::*, px, rgb,
};

use crate::fs::Fs;
use crate::ui::{
    LIST_ROW_HEIGHT, PANEL_FOOTER_HEIGHT, PANEL_HEADER_HEIGHT, Viewport, panel_header,
};
use crate::{Back, OpenRow, Refresh, RowDown, RowEnd, RowHome, RowPageDown, RowPageUp, RowUp};

const MAX_ROWS: usize = 20_000;

pub enum FilesEvent {
    OpenFile(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNode {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

// One reading of the tree, gathered off the UI thread.
#[derive(Debug, Default)]
pub struct TreeListing {
    pub rows: Vec<FileNode>,
    pub notes: Vec<String>,
    pub truncated: bool,
}

// Walk the open folders. Blocking, so it belongs on the background executor:
// one unreadable network mount inside a deep tree is seconds of syscalls.
pub fn read_tree(root: &Path, expanded: &BTreeSet<PathBuf>, fs: &dyn Fs) -> TreeListing {
    let mut listing = TreeListing::default();
    fill(&mut listing, root, 0, expanded, fs);
    if listing.rows.is_empty() && listing.notes.is_empty() {
        listing.notes.push("empty folder".to_string());
    }
    listing
}

fn fill(
    listing: &mut TreeListing,
    dir: &Path,
    depth: usize,
    expanded: &BTreeSet<PathBuf>,
    fs: &dyn Fs,
) {
    let entries = match fs.list_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            // Name the folder: one unreadable subdirectory inside a deep tree
            // is not findable from the error alone.
            let note = format!("{}: {error}", dir.display());
            if !listing.notes.contains(&note) {
                listing.notes.push(note);
            }
            return;
        }
    };
    for entry in entries {
        if listing.rows.len() >= MAX_ROWS {
            listing.truncated = true;
            return;
        }
        let path = dir.join(&entry.name);
        let is_expanded = entry.is_dir && expanded.contains(&path);
        listing.rows.push(FileNode {
            name: entry.label(),
            path: path.clone(),
            depth,
            is_dir: entry.is_dir,
            expanded: is_expanded,
        });
        if is_expanded {
            fill(listing, &path, depth + 1, expanded, fs);
        }
    }
}

// What a row press asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Toggle {
    Open(PathBuf),
    Reread,
    Nothing,
}

#[derive(Debug, Default)]
pub struct FilesState {
    pub rows: Vec<FileNode>,
    // Expansion is the user's, so it outlives a listing that could not be read
    // or was cut by the row cap, and it is the only truth about what is open --
    // rows are derived from it.
    pub expanded: BTreeSet<PathBuf>,
    pub selected: usize,
    pub top: usize,
    pub viewport: usize,
    pub notes: Vec<String>,
    pub truncated: bool,
}

impl FilesState {
    pub fn apply(&mut self, listing: TreeListing) {
        // A row inserted above the cursor must not move it: a build that writes
        // a new file is not the user changing their selection. A cursor whose own
        // file has gone keeps its row number, which is where the eye left it.
        let anchor = self.rows.get(self.selected).map(|row| row.path.clone());
        self.rows = listing.rows;
        self.notes = listing.notes;
        self.truncated = listing.truncated;
        self.selected = anchor
            .and_then(|path| self.rows.iter().position(|row| row.path == path))
            .unwrap_or(self.selected)
            .min(self.rows.len().saturating_sub(1));
        self.clamp();
    }

    pub fn toggle_selected(&mut self) -> Toggle {
        let Some(row) = self.rows.get(self.selected) else {
            return Toggle::Nothing;
        };
        if !row.is_dir {
            return Toggle::Open(row.path.clone());
        }
        if !self.expanded.remove(&row.path) {
            self.expanded.insert(row.path.clone());
        }
        Toggle::Reread
    }

    // Collapsing an open folder, or climbing to the parent of anything else.
    // True when the tree has to be read again.
    pub fn collapse_selected(&mut self) -> bool {
        let Some(row) = self.rows.get(self.selected) else {
            return false;
        };
        // Whether the folder is open is asked of the expansion set, not of the
        // row: a row is drawn from a listing, and a listing still in flight when
        // the set changed draws it shut while it is open. Trusting the row made
        // enter collapse a folder the user had just opened.
        if row.is_dir && self.expanded.contains(&row.path) {
            self.expanded.remove(&row.path);
            return true;
        }
        let depth = row.depth;
        if depth == 0 {
            return false;
        }
        if let Some(parent) = self.rows[..self.selected]
            .iter()
            .rposition(|candidate| candidate.depth < depth)
        {
            self.selected = parent;
            self.clamp();
        }
        false
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
        self.clamp();
    }

    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        self.clamp();
    }

    fn clamp(&mut self) {
        let last_top = self.rows.len().saturating_sub(self.viewport.max(1));
        self.top = self.top.min(last_top);
        if self.selected < self.top {
            self.top = self.selected;
        }
        let last = self.top + self.viewport.saturating_sub(1);
        if self.selected > last {
            self.top = self.selected + 1 - self.viewport.max(1);
        }
    }
}

pub struct FilesView {
    focus: FocusHandle,
    fs: Arc<dyn Fs>,
    root: PathBuf,
    state: FilesState,
    viewport: Viewport,
    // Which read the panel is waiting for. Opening another folder, or toggling
    // again before the first read lands, leaves two walks in flight, and the
    // slower one must not be the one that decides what the tree shows.
    generation: u64,
}

impl gpui::EventEmitter<FilesEvent> for FilesView {}

impl FilesView {
    pub fn new(fs: Arc<dyn Fs>, root: PathBuf, cx: &mut Context<Self>) -> FilesView {
        let mut state = FilesState::default();
        state.set_viewport(20);
        let mut view = FilesView {
            focus: cx.focus_handle(),
            fs,
            root,
            state,
            viewport: Viewport::default(),
            generation: 0,
        };
        view.reread(cx);
        view
    }

    // Read the open folders on the background executor and adopt the result on
    // the UI thread. Every path that changes what is open ends here.
    fn reread(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let fs = self.fs.clone();
        let root = self.root.clone();
        let expanded = self.state.expanded.clone();
        cx.spawn(async move |this, cx| {
            let listing = cx
                .background_executor()
                .spawn(async move { read_tree(&root, &expanded, fs.as_ref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    // A newer read is on its way; this one describes a folder or
                    // an expansion the panel has already moved past.
                    return;
                }
                this.state.apply(listing);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn title(&self) -> SharedString {
        self.root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.to_string_lossy().into_owned())
            .into()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let rows = self.state.viewport;
        self.root = root;
        self.state = FilesState::default();
        self.state.set_viewport(rows.max(1));
        self.reread(cx);
        cx.notify();
    }

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if !self.viewport.update(width, height) {
            return;
        }
        let rows = self.viewport.rows(
            PANEL_HEADER_HEIGHT + PANEL_FOOTER_HEIGHT,
            8.0,
            LIST_ROW_HEIGHT,
            2_000,
        );
        self.state.set_viewport(rows.max(4));
        cx.notify();
    }

    fn status(&self) -> String {
        let mut pieces = vec![format!("{} entries", self.state.rows.len())];
        if self.state.truncated {
            pieces.push(format!("listing capped at {MAX_ROWS}"));
        }
        pieces.extend(self.state.notes.iter().cloned());
        pieces.push("r refreshes".to_string());
        pieces.join("  ·  ")
    }
}

impl Render for FilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let view = cx.entity();
        let selected = self.state.selected;
        let rows: Vec<(usize, FileNode)> = self
            .state
            .rows
            .iter()
            .enumerate()
            .skip(self.state.top)
            .take(self.state.viewport)
            .map(|(index, row)| (index, row.clone()))
            .collect();
        div()
            .id("files-panel")
            .key_context("Browse")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(theme.shell.panel_background))
            .font_family(fonts.ui_family.clone())
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.resize(
                                f32::from(bounds.size.width),
                                f32::from(bounds.size.height),
                                cx,
                            );
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_action(cx.listener(|this, _: &RowUp, _, cx| {
                this.state.move_selection(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                this.state.move_selection(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageUp, _, cx| {
                this.state
                    .move_selection(-(this.state.viewport.saturating_sub(1).max(1) as isize));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowPageDown, _, cx| {
                this.state
                    .move_selection(this.state.viewport.saturating_sub(1).max(1) as isize);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowHome, _, cx| {
                this.state.selected = 0;
                this.state.move_selection(0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowEnd, _, cx| {
                this.state.selected = this.state.rows.len().saturating_sub(1);
                this.state.move_selection(0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| {
                match this.state.toggle_selected() {
                    Toggle::Open(path) => cx.emit(FilesEvent::OpenFile(path)),
                    Toggle::Reread => this.reread(cx),
                    Toggle::Nothing => {}
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Back, _, cx| {
                if this.state.collapse_selected() {
                    this.reread(cx);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.reread(cx);
                cx.notify();
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = f32::from(event.delta.pixel_delta(px(LIST_ROW_HEIGHT)).y);
                this.state
                    .move_selection(-(delta / LIST_ROW_HEIGHT).round() as isize);
                cx.notify();
            }))
            .child(panel_header(&theme, &fonts, self.title()))
            .child(
                div()
                    .id("files-rows")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .py(px(4.0))
                    .flex()
                    .flex_col()
                    .role(Role::ListBox)
                    .aria_label("Files")
                    .children(rows.into_iter().map(|(index, row)| {
                        let marker = if row.is_dir {
                            if row.expanded { "▾ " } else { "▸ " }
                        } else {
                            "  "
                        };
                        let label = SharedString::from(format!("{marker}{}", row.name));
                        let is_selected = index == selected;
                        let mut line = div()
                            .id(("files-row", index))
                            .h(px(LIST_ROW_HEIGHT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .pl(px(8.0 + row.depth as f32 * 14.0))
                            .pr(px(8.0))
                            .text_size(px(fonts.small()))
                            .text_color(if row.is_dir {
                                rgb(theme.shell.text)
                            } else {
                                rgb(theme.shell.text_muted)
                            })
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .cursor_pointer()
                            .role(Role::ListBoxOption)
                            .aria_label(label.clone())
                            .aria_selected(is_selected)
                            .hover(|style| style.bg(rgb(theme.shell.element_hover)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    this.state.selected = index;
                                    match this.state.toggle_selected() {
                                        Toggle::Open(path) => cx.emit(FilesEvent::OpenFile(path)),
                                        Toggle::Reread => this.reread(cx),
                                        Toggle::Nothing => {}
                                    }
                                    cx.notify();
                                }),
                            );
                        if is_selected {
                            line = line.bg(rgb(theme.shell.element_selected));
                        }
                        line.child(label)
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .py(px(4.0))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(SharedString::from(self.status())),
            )
    }
}

impl crate::item::Item for FilesView {
    fn title(&self) -> SharedString {
        FilesView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        FilesView::focus_handle(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::fake::FakeFs;

    // The two steps the panel takes: read what is open, then adopt it.
    fn load(state: &mut FilesState, root: &str, fs: &dyn Fs) {
        let listing = read_tree(Path::new(root), &state.expanded, fs);
        state.apply(listing);
    }

    fn fs() -> FakeFs {
        FakeFs::with_files(&[
            ("/work/base/app.yaml", "a"),
            ("/work/base/svc.yaml", "b"),
            ("/work/overlays/prod/patch.yaml", "c"),
            ("/work/README.md", "d"),
        ])
    }

    fn names(state: &FilesState) -> Vec<(usize, &str)> {
        state
            .rows
            .iter()
            .map(|row| (row.depth, row.name.as_str()))
            .collect()
    }

    #[test]
    fn the_tree_lists_folders_first_and_expands_in_place() {
        let fs = fs();
        let mut state = FilesState::default();
        state.set_viewport(10);
        load(&mut state, "/work", &fs);
        assert_eq!(
            names(&state),
            [(0, "base"), (0, "overlays"), (0, "README.md")]
        );
        state.selected = 0;
        assert_eq!(state.toggle_selected(), Toggle::Reread);
        load(&mut state, "/work", &fs);
        assert_eq!(
            names(&state),
            [
                (0, "base"),
                (1, "app.yaml"),
                (1, "svc.yaml"),
                (0, "overlays"),
                (0, "README.md")
            ]
        );
    }

    #[test]
    fn opening_a_file_returns_its_path_instead_of_toggling() {
        let fs = fs();
        let mut state = FilesState::default();
        state.set_viewport(10);
        load(&mut state, "/work", &fs);
        state.selected = 2;
        assert_eq!(
            state.toggle_selected(),
            Toggle::Open(PathBuf::from("/work/README.md"))
        );
    }

    #[test]
    fn refresh_preserves_expansion_and_sees_new_files() {
        let fs = fs();
        let mut state = FilesState::default();
        state.set_viewport(10);
        load(&mut state, "/work", &fs);
        state.selected = 0;
        state.toggle_selected();
        load(&mut state, "/work", &fs);
        fs.touch("/work/base/new.yaml", "n");
        load(&mut state, "/work", &fs);
        assert!(
            state
                .rows
                .iter()
                .any(|row| row.name == "new.yaml" && row.depth == 1),
            "the refreshed tree keeps base expanded and lists the new file: {:?}",
            names(&state)
        );
    }

    #[test]
    fn back_collapses_or_climbs_to_the_parent() {
        let fs = fs();
        let mut state = FilesState::default();
        state.set_viewport(10);
        load(&mut state, "/work", &fs);
        state.selected = 0;
        state.toggle_selected();
        load(&mut state, "/work", &fs);
        state.selected = 2;
        if state.collapse_selected() {
            load(&mut state, "/work", &fs);
        }
        assert_eq!(state.selected, 0, "a file's back goes to its parent folder");
        if state.collapse_selected() {
            load(&mut state, "/work", &fs);
        }
        assert_eq!(
            names(&state),
            [(0, "base"), (0, "overlays"), (0, "README.md")],
            "a folder's back collapses it"
        );
    }

    #[test]
    fn a_folder_answers_from_the_expansion_set_not_from_a_stale_row() {
        // Between a toggle and the read it asks for, the rows still describe the
        // previous state. Trusting the row there made the folder the user had
        // just opened collapse on the next press, and take two to open.
        let fs = fs();
        let mut state = FilesState::default();
        state.set_viewport(10);
        load(&mut state, "/work", &fs);
        state.selected = 0;
        assert_eq!(state.toggle_selected(), Toggle::Reread);
        assert!(state.expanded.contains(Path::new("/work/base")));
        assert!(
            !state.rows[0].expanded,
            "the row is still drawn from the listing before the toggle"
        );
        assert!(
            state.collapse_selected(),
            "and back collapses the folder rather than climbing away from it"
        );
        assert!(!state.expanded.contains(Path::new("/work/base")));
    }

    #[test]
    fn a_vanished_root_is_a_labelled_note_never_a_panic() {
        let fs = FakeFs::default();
        let mut state = FilesState::default();
        state.set_viewport(10);
        load(&mut state, "/gone", &fs);
        assert!(state.rows.is_empty());
        assert!(
            state.notes.iter().any(|note| note.contains("/gone")),
            "the note names the folder: {:?}",
            state.notes
        );
    }

    #[test]
    fn the_cursor_follows_its_file_when_a_refresh_inserts_a_row_above_it() {
        let fs = fs();
        let mut state = FilesState::default();
        state.set_viewport(10);
        load(&mut state, "/work", &fs);
        state.selected = 2;
        assert_eq!(state.rows[state.selected].name, "README.md");
        fs.touch("/work/AAA.yaml", "a");
        load(&mut state, "/work", &fs);
        assert_eq!(
            state.rows[state.selected].name, "README.md",
            "a new file above the cursor must not move it onto another file"
        );
    }

    #[test]
    fn the_view_never_scrolls_past_the_last_row() {
        let fs = fs();
        let mut state = FilesState::default();
        state.set_viewport(2);
        load(&mut state, "/work", &fs);
        state.selected = 0;
        state.toggle_selected();
        load(&mut state, "/work", &fs);
        state.move_selection(4);
        let scrolled = state.top;
        assert!(scrolled > 0, "a small viewport scrolls");
        state.selected = 0;
        state.toggle_selected();
        load(&mut state, "/work", &fs);
        assert!(
            state.top + state.viewport <= state.rows.len().max(state.viewport),
            "collapsing back must not leave a screenful of blank: top {} rows {}",
            state.top,
            state.rows.len()
        );
    }
}
