//! Opening things by path or by fuzzy name: Zed's two open flows.
//!
//! Both read the disk on the background executor and think on the UI thread.
//! The path picker lists a directory once and then filters that listing on
//! every keystroke -- re-reading the folder per character is how a picker
//! freezes on a network mount -- and the file finder scans once when it opens.
//! Names are joined from the entries themselves, so a file whose name is not
//! valid UTF-8 still opens; only what the row displays is lossy.
//!
//! A keystroke costs no syscall, but `enter` may: [`PickerState::confirm`]
//! answers from the rows only while they are authoritative -- the listing for
//! the directory the input names has landed, and it succeeded -- and asks the
//! `Fs` otherwise, because a listing still in flight or a folder that will not
//! list must not turn a path that exists into "no such path". That is one
//! question on one deliberate keystroke, which is what makes it affordable
//! there and nowhere else. A typed path is absolute or refused, since a bare
//! name would resolve against whatever directory the process was started in,
//! and a folder is never a save target.
//!
//! The path picker is a modal over typed paths -- the trailing segment
//! filters its directory's entries and tab completes the highlighted one --
//! and [`PickerMode`] decides only what `enter` means on each row shape:
//! opening a file descends into folders, opening a folder confirms them (with
//! a `.` row for the one being looked at), and saving trusts the typed name
//! but never a row the user did not type, so `enter` straight after opening
//! save-as cannot overwrite whatever happened to be first. The file finder is
//! fuzzy search over a bounded scan of the opened folder: files, depth, and
//! listed matches are all capped, every cap is stated in the modal, and
//! unreadable folders are counted rather than dropped. Both are state machines
//! over the `Fs` seam under thin palette-shaped views, pure on every keystroke,
//! and both answer through events the workspace routes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    Context, FocusHandle, IntoElement, KeyDownEvent, ParentElement, Render, Role, SharedString,
    Styled, StyledText, Window, div, prelude::*, px, rgb,
};

use crate::fs::{DirEntry, Fs};
use crate::palette::fuzzy_match;
use crate::ui::{MODAL_MAX_HEIGHT, MODAL_WIDTH};
use crate::{CancelInput, CommitInput, DeleteInputChar, PickParent, RowDown, RowUp};

const VISIBLE_ROWS: usize = 10;
const MAX_SCANNED_FILES: usize = 10_000;
const MAX_SCAN_DEPTH: usize = 16;
const MAX_SHOWN_MATCHES: usize = 256;
const SKIPPED_DIRS: [&str; 5] = [".git", "target", "node_modules", ".tmp", ".cache"];

pub enum PickerEvent {
    Dismissed,
    Confirmed(PathBuf),
}

// What the picker is being asked for. The mode decides only what `enter`
// means on each row shape; listing, filtering, and completion are identical,
// which is why a fourth mode would be one match arm rather than a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    OpenFile,
    OpenFolder,
    Save,
}

// The row that means "the folder I am looking at", so confirming a folder is
// a highlighted row like everything else rather than a special keystroke.
pub(crate) const HERE: &str = ".";

#[derive(Debug, PartialEq, Eq)]
pub enum PickerAction {
    Descend(String),
    Open(PathBuf),
    Reject(String),
}

// What came back for one listing request, or nothing while it is still in
// flight. A failure keeps its message, because the note it puts on screen has
// to outlive the keystrokes that follow inside the same folder.
#[derive(Debug)]
enum Answer {
    Listed,
    Failed(String),
}

// The newest listing this state asked for: which directory, and whether its
// answer is in yet. Only one answer is ever adopted, so a request the user
// moved past and came back to cannot land a second time and take the
// highlighted row with it.
#[derive(Debug)]
struct Listing {
    dir: String,
    answer: Option<Answer>,
}

#[derive(Debug)]
pub struct PickerState {
    pub input: String,
    pub entries: Vec<DirEntry>,
    pub matches: Vec<usize>,
    pub selected: usize,
    pub note: Option<String>,
    pub mode: PickerMode,
    // Where `entries` came from, so a keystroke inside a directory already
    // listed costs no syscall and `confirm` knows whether the rows can answer.
    listing: Option<Listing>,
}

impl PickerState {
    pub fn new(seed: &Path, mode: PickerMode) -> PickerState {
        let mut input = seed.to_string_lossy().into_owned();
        if !input.ends_with('/') {
            input.push('/');
        }
        PickerState {
            input,
            entries: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            note: None,
            mode,
            listing: None,
        }
    }

    pub(crate) fn split(&self) -> (String, String) {
        match self.input.rfind('/') {
            Some(slash) => (
                self.input[..=slash].to_string(),
                self.input[slash + 1..].to_string(),
            ),
            None => (String::new(), self.input.clone()),
        }
    }

    // The directory the input now names, when that is not the one already
    // asked for. The rows are dropped first, so a keystroke never filters
    // another folder's entries.
    pub fn begin_listing(&mut self) -> Option<String> {
        let (dir, _) = self.split();
        if let Some(listing) = self.listing.as_ref().filter(|listing| listing.dir == dir) {
            // Inside the folder already asked for, the only note that still
            // applies is the one that folder produced: anything else is the
            // answer to an `enter` the user has since typed past.
            self.note = match &listing.answer {
                Some(Answer::Failed(error)) => Some(error.clone()),
                _ => None,
            };
            return None;
        }
        self.entries.clear();
        self.matches.clear();
        self.selected = 0;
        self.note = None;
        self.listing = Some(Listing {
            dir: dir.clone(),
            answer: None,
        });
        Some(dir)
    }

    // A listing came back. Ignored when the typed path has moved on, so a slow
    // directory cannot overwrite the one the user is looking at now, and
    // ignored when this directory's answer is already in: a second one is a
    // request the user stepped away from and back to, finishing late, and
    // adopting it would reset the row they have highlighted since.
    pub fn listed(&mut self, dir: &str, listed: std::io::Result<Vec<DirEntry>>) -> bool {
        let (wanted, _) = self.split();
        let awaited = self
            .listing
            .as_ref()
            .is_some_and(|listing| listing.dir == dir && listing.answer.is_none());
        if wanted != dir || !awaited {
            return false;
        }
        let answer = match listed {
            Ok(entries) => {
                self.note = None;
                self.entries = entries;
                if self.mode == PickerMode::OpenFolder {
                    self.entries
                        .insert(0, DirEntry::new(std::ffi::OsStr::new(HERE), true));
                }
                Answer::Listed
            }
            Err(error) => {
                let error = format!("{error}");
                self.note = Some(error.clone());
                self.entries = Vec::new();
                Answer::Failed(error)
            }
        };
        self.listing = Some(Listing {
            dir: dir.to_string(),
            answer: Some(answer),
        });
        true
    }

    // Whether `entries` are the contents of the directory the input names.
    // They are not while its listing is in flight, and not when that listing
    // failed -- a folder that will not list still holds files that open -- so
    // anything answering a question about the disk has to ask the disk instead.
    pub fn listing_is_authoritative(&self) -> bool {
        let (dir, _) = self.split();
        self.listing.as_ref().is_some_and(|listing| {
            listing.dir == dir && matches!(listing.answer, Some(Answer::Listed))
        })
    }

    // Scoring the listing against the trailing segment: pure, and the only
    // thing a keystroke costs.
    pub fn refilter(&mut self) {
        let (_, segment) = self.split();
        let mut scored: Vec<(i64, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if segment.is_empty() {
                    Some((0, index))
                } else {
                    fuzzy_match(&segment, &entry.label()).map(|(score, _)| (score, index))
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
    }

    fn selected_entry(&self) -> Option<&DirEntry> {
        self.matches
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))
    }

    pub(crate) fn complete_selected(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if entry.name == std::ffi::OsStr::new(HERE) {
            return;
        }
        let (dir, _) = self.split();
        self.input = format!(
            "{dir}{}{}",
            entry.label(),
            if entry.is_dir { "/" } else { "" }
        );
    }

    pub fn confirm(&self, fs: &dyn Fs) -> PickerAction {
        let (dir, segment) = self.split();
        // Everything below joins onto the typed directory, and an empty one is
        // a bare name: it would land wherever the process happens to have been
        // started, which is never the folder the user is looking at.
        if dir.is_empty() {
            return PickerAction::Reject("type an absolute path".to_string());
        }
        // A highlighted row is what the eye is on, so it wins -- except while
        // saving, where a typed name that is not the highlighted row is the
        // new file the user means to create.
        if let Some(entry) = self.selected_entry()
            && !(self.mode == PickerMode::Save && (segment.is_empty() || entry.label() != segment))
        {
            if entry.name == std::ffi::OsStr::new(HERE) {
                let here = dir.trim_end_matches('/');
                return if here.is_empty() {
                    PickerAction::Open(PathBuf::from("/"))
                } else {
                    PickerAction::Open(PathBuf::from(here))
                };
            }
            // Joined from the name, never from the row's lossy label: a file
            // whose name is not valid UTF-8 has to open the file it names, not
            // a path spelled with replacement characters.
            let path = Path::new(&dir).join(&entry.name);
            return match (entry.is_dir, self.mode) {
                (true, PickerMode::OpenFolder) => PickerAction::Open(path),
                (true, _) => PickerAction::Descend(format!("{dir}{}/", entry.label())),
                (false, PickerMode::OpenFolder) => {
                    PickerAction::Reject(format!("{} is a file; pick a folder", entry.label()))
                }
                (false, _) => PickerAction::Open(path),
            };
        }
        if segment.is_empty() {
            return PickerAction::Reject(match self.mode {
                PickerMode::OpenFolder => "pick a folder".to_string(),
                _ => "type a file name".to_string(),
            });
        }
        // The rows answer for the typed name only while they are this
        // directory's contents; otherwise the disk does. One stat on an
        // explicit `enter` is affordable, and answering "no such path" about a
        // file that is sitting there is not.
        let authoritative = self.listing_is_authoritative();
        let listed = if authoritative {
            self.entries.iter().find(|entry| entry.label() == segment)
        } else {
            None
        };
        // A name tab-completed from a lossy label is that entry's name, so the
        // path is built from the entry here too.
        let path = match listed {
            Some(entry) => Path::new(&dir).join(&entry.name),
            None => PathBuf::from(self.input.clone()),
        };
        let (exists, is_dir) = match (authoritative, listed) {
            (true, Some(entry)) => (true, entry.is_dir),
            (true, None) => (false, false),
            (false, _) => (fs.exists(&path), fs.is_dir(&path)),
        };
        if self.mode == PickerMode::Save {
            // A folder cannot be written to, and accepting one only moves the
            // failure into the editor's save.
            return if is_dir {
                PickerAction::Reject(format!("{} is a folder; type a file name", self.input))
            } else {
                PickerAction::Open(path)
            };
        }
        match (self.mode, exists, is_dir) {
            (PickerMode::OpenFolder, true, true) => PickerAction::Open(path),
            (PickerMode::OpenFolder, _, _) => {
                PickerAction::Reject(format!("no such folder: {}", self.input))
            }
            // A typed folder gets the answer a highlighted one gets: step into
            // it, rather than hand the editor a directory to read.
            (_, true, true) => PickerAction::Descend(format!("{}/", self.input)),
            (_, true, false) => PickerAction::Open(path),
            (_, false, _) => PickerAction::Reject(format!("no such path: {}", self.input)),
        }
    }

    // A half-typed name goes before the folder does, because ctrl-up on a typo
    // is a correction, not a request to leave.
    pub(crate) fn parent(&mut self) {
        let (dir, segment) = self.split();
        if !segment.is_empty() {
            self.input = dir;
            return;
        }
        let trimmed = dir.trim_end_matches('/');
        if let Some(slash) = trimmed.rfind('/') {
            self.input = trimmed[..=slash].to_string();
        }
    }
}

pub struct PathPickerView {
    focus: FocusHandle,
    fs: Arc<dyn Fs>,
    state: PickerState,
    title: &'static str,
}

impl gpui::EventEmitter<PickerEvent> for PathPickerView {}

impl PathPickerView {
    pub fn new(
        fs: Arc<dyn Fs>,
        seed: PathBuf,
        mode: PickerMode,
        cx: &mut Context<Self>,
    ) -> PathPickerView {
        let title = match mode {
            PickerMode::OpenFile => "Open file",
            PickerMode::OpenFolder => "Open folder",
            PickerMode::Save => "Save as",
        };
        let mut view = PathPickerView {
            focus: cx.focus_handle(),
            fs,
            state: PickerState::new(&seed, mode),
            title,
        };
        view.input_changed(cx);
        view
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    // The input moved. A directory that is already listed only needs filtering;
    // a new one is read on the background executor, because a listing is a
    // syscall and a keystroke is a frame.
    fn input_changed(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.state.begin_listing() else {
            self.state.refilter();
            cx.notify();
            return;
        };
        cx.notify();
        if dir.is_empty() {
            self.state.listed(
                &dir,
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "type an absolute path",
                )),
            );
            self.state.refilter();
            cx.notify();
            return;
        }
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            let read = {
                let fs = fs.clone();
                let dir = dir.clone();
                cx.background_executor()
                    .spawn(async move { fs.list_dir(Path::new(&dir)) })
                    .await
            };
            let _ = this.update(cx, |this, cx| {
                if this.state.listed(&dir, read) {
                    this.state.refilter();
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let action = self.state.confirm(self.fs.as_ref());
        match action {
            PickerAction::Descend(input) => {
                self.state.input = input;
                self.input_changed(cx);
            }
            PickerAction::Open(path) => cx.emit(PickerEvent::Confirmed(path)),
            PickerAction::Reject(note) => self.state.note = Some(note),
        }
        cx.notify();
    }
}

impl Render for PathPickerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let first = self
            .state
            .selected
            .saturating_sub(VISIBLE_ROWS.saturating_sub(1))
            .min(
                self.state
                    .matches
                    .len()
                    .saturating_sub(VISIBLE_ROWS.min(self.state.matches.len())),
            );
        let rows: Vec<(usize, String, bool)> = self
            .state
            .matches
            .iter()
            .enumerate()
            .skip(first)
            .take(VISIBLE_ROWS)
            .filter_map(|(at, index)| {
                self.state.entries.get(*index).map(|entry| {
                    let name = if entry.is_dir {
                        format!("{}/", entry.label())
                    } else {
                        entry.label()
                    };
                    (at, name, entry.is_dir)
                })
            })
            .collect();
        let selected = self.state.selected;
        div()
            .id("path-picker")
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
            .aria_label(SharedString::from(self.title))
            .on_action(cx.listener(|this, _: &RowUp, _, cx| {
                this.state.selected = this.state.selected.saturating_sub(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RowDown, _, cx| {
                if this.state.selected + 1 < this.state.matches.len() {
                    this.state.selected += 1;
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CommitInput, _, cx| {
                this.confirm(cx);
            }))
            .on_action(cx.listener(|_, _: &CancelInput, _, cx| {
                cx.emit(PickerEvent::Dismissed);
            }))
            .on_action(cx.listener(|this, _: &DeleteInputChar, _, cx| {
                this.state.input.pop();
                this.input_changed(cx);
            }))
            .on_action(cx.listener(|this, _: &PickParent, _, cx| {
                this.state.parent();
                this.input_changed(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let keystroke = &event.keystroke;
                if keystroke.modifiers.control || keystroke.modifiers.alt {
                    return;
                }
                if keystroke.key == "tab" {
                    this.state.complete_selected();
                    this.input_changed(cx);
                    return;
                }
                if let Some(key_char) = &keystroke.key_char {
                    this.state.input.push_str(key_char);
                    this.input_changed(cx);
                }
            }))
            .child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_b_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.ui_size))
                    .text_color(rgb(theme.shell.text))
                    .font_family(fonts.buffer_family.clone())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(SharedString::from(format!("{}▌", self.state.input))),
            )
            .children(rows.into_iter().map(|(at, name, is_dir)| {
                let mut row = div()
                    .px(px(12.0))
                    .h(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .text_size(px(fonts.small()))
                    .font_family(fonts.buffer_family.clone())
                    .text_color(if is_dir {
                        rgb(theme.shell.text_accent)
                    } else {
                        rgb(theme.shell.text)
                    })
                    .whitespace_nowrap()
                    .overflow_hidden();
                if at == selected {
                    row = row.bg(rgb(theme.shell.element_selected));
                }
                row.child(SharedString::from(name))
            }))
            .children(self.state.note.clone().map(|note| {
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.warning))
                    .child(SharedString::from(note))
            }))
            .child(
                div()
                    .px(px(12.0))
                    .py(px(4.0))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(SharedString::from(match self.state.mode {
                        PickerMode::Save => {
                            "enter saves the typed name · tab enters a folder · ctrl-up goes up"
                        }
                        PickerMode::OpenFolder => {
                            "enter opens the highlighted folder (. is this one) · tab descends"
                        }
                        PickerMode::OpenFile => "enter opens · tab completes · ctrl-up goes up",
                    })),
            )
    }
}

// ---------------------------------------------------------------------------
// The fuzzy file finder over an opened folder.
// ---------------------------------------------------------------------------

pub enum FinderEvent {
    Dismissed,
    Confirmed(PathBuf),
}

pub struct Scan {
    pub files: Vec<ScannedFile>,
    pub capped: bool,
    pub unreadable: usize,
}

// One scanned file: the path relative to the root, joined from the entry names
// themselves so it opens whatever they are, and the text a row shows and a
// query matches against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub label: String,
}

pub fn scan_root(fs: &dyn Fs, root: &Path) -> Scan {
    let mut files: Vec<ScannedFile> = Vec::new();
    let mut queue: Vec<(PathBuf, PathBuf, String, usize)> =
        vec![(root.to_path_buf(), PathBuf::new(), String::new(), 0)];
    let mut capped = false;
    let mut unreadable = 0usize;
    while let Some((dir, relative, prefix, depth)) = queue.pop() {
        if files.len() >= MAX_SCANNED_FILES {
            capped = true;
            break;
        }
        let Ok(entries) = fs.list_dir(&dir) else {
            unreadable += 1;
            continue;
        };
        for entry in entries {
            let label = entry.label();
            if entry.is_dir {
                let ignored = SKIPPED_DIRS.contains(&label.as_str()) || label.starts_with('.');
                if ignored {
                    // Deliberately out of scope, so not a truncation.
                } else if depth + 1 > MAX_SCAN_DEPTH {
                    capped = true;
                } else {
                    queue.push((
                        dir.join(&entry.name),
                        relative.join(&entry.name),
                        format!("{prefix}{label}/"),
                        depth + 1,
                    ));
                }
            } else if files.len() < MAX_SCANNED_FILES {
                files.push(ScannedFile {
                    path: relative.join(&entry.name),
                    label: format!("{prefix}{label}"),
                });
            } else {
                capped = true;
            }
        }
    }
    files.sort_by(|a, b| a.label.cmp(&b.label).then(a.path.cmp(&b.path)));
    Scan {
        files,
        capped,
        unreadable,
    }
}

// One matched file: its index in the scan, and where in its label the query hit
// so the row can accent those characters.
pub type Match = (usize, Vec<std::ops::Range<usize>>);

// Scoring the scanned files against a query: the whole cost of a keystroke in
// the finder, and pure, so it is the thing a benchmark measures. The second
// answer is whether the list was cut, which the modal states rather than
// showing a count that is not the count.
pub fn rank(files: &[ScannedFile], query: &str) -> (Vec<Match>, bool) {
    let mut scored: Vec<(i64, usize, Vec<std::ops::Range<usize>>)> = files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            if query.is_empty() {
                Some((0, index, Vec::new()))
            } else {
                fuzzy_match(query, &file.label).map(|(score, hits)| (score, index, hits))
            }
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let truncated = scored.len() > MAX_SHOWN_MATCHES;
    scored.truncate(MAX_SHOWN_MATCHES);
    let matches: Vec<Match> = scored
        .into_iter()
        .map(|(_, index, hits)| (index, hits))
        .collect();
    (matches, truncated)
}

pub struct FileFinderView {
    focus: FocusHandle,
    root: PathBuf,
    files: Vec<ScannedFile>,
    capped: bool,
    unreadable: usize,
    truncated: bool,
    scanning: bool,
    query: String,
    matches: Vec<Match>,
    selected: usize,
}

impl gpui::EventEmitter<FinderEvent> for FileFinderView {}

impl FileFinderView {
    pub fn new(fs: Arc<dyn Fs>, root: PathBuf, cx: &mut Context<Self>) -> FileFinderView {
        let scan_root_path = root.clone();
        cx.spawn(async move |this, cx| {
            let scan = cx
                .background_executor()
                .spawn(async move { scan_root(fs.as_ref(), &scan_root_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.files = scan.files;
                this.capped = scan.capped;
                this.unreadable = scan.unreadable;
                this.scanning = false;
                this.requery();
                cx.notify();
            });
        })
        .detach();
        FileFinderView {
            focus: cx.focus_handle(),
            root,
            files: Vec::new(),
            capped: false,
            unreadable: 0,
            truncated: false,
            scanning: true,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn requery(&mut self) {
        let (matches, truncated) = rank(&self.files, &self.query);
        self.matches = matches;
        self.truncated = truncated;
        self.selected = 0;
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some((index, _)) = self.matches.get(self.selected)
            && let Some(found) = self.files.get(*index)
        {
            cx.emit(FinderEvent::Confirmed(self.root.join(&found.path)));
        }
    }
}

impl Render for FileFinderView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            text: String,
            hits: Vec<std::ops::Range<usize>>,
        }
        let rows: Vec<Row> = self
            .matches
            .iter()
            .enumerate()
            .skip(first)
            .take(VISIBLE_ROWS)
            .filter_map(|(at, (index, hits))| {
                self.files.get(*index).map(|file| Row {
                    at,
                    text: file.label.clone(),
                    hits: hits.clone(),
                })
            })
            .collect();
        let selected = self.selected;
        let status = if self.scanning {
            "scanning...".to_string()
        } else {
            let mut parts = vec![format!(
                "{} of {} files",
                self.matches.len(),
                self.files.len()
            )];
            if self.matches.is_empty() && !self.query.is_empty() {
                parts.push("no matches".to_string());
            }
            if self.truncated {
                parts.push(format!("only the first {MAX_SHOWN_MATCHES} are listed"));
            }
            if self.capped {
                parts.push(format!("scan capped at {MAX_SCANNED_FILES} files"));
            }
            if self.unreadable > 0 {
                parts.push(format!("{} folders unreadable", self.unreadable));
            }
            parts.join("  ·  ")
        };
        div()
            .id("file-finder")
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
            .aria_label("File finder")
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
                cx.emit(FinderEvent::Dismissed);
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
            .child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_b_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.ui_size))
                    .text_color(rgb(theme.shell.text))
                    .font_family(fonts.buffer_family.clone())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(SharedString::from(format!("{}▌", self.query))),
            )
            .children(rows.into_iter().map(|row| {
                let mut line = div()
                    .px(px(12.0))
                    .h(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .text_size(px(fonts.small()))
                    .font_family(fonts.buffer_family.clone())
                    .text_color(rgb(theme.shell.text))
                    .whitespace_nowrap()
                    .overflow_hidden();
                if row.at == selected {
                    line = line.bg(rgb(theme.shell.element_selected));
                }
                let accent: gpui::HighlightStyle = gpui::HighlightStyle {
                    color: Some(rgb(theme.shell.text_accent).into()),
                    ..Default::default()
                };
                line.child(
                    StyledText::new(SharedString::from(row.text))
                        .with_highlights(row.hits.into_iter().map(|hit| (hit, accent))),
                )
            }))
            .child(
                div()
                    .px(px(12.0))
                    .py(px(4.0))
                    .border_t_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(SharedString::from(status)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::fake::FakeFs;

    // The two steps the view takes, in the order it takes them: read the
    // directory only when the typed path names a new one, then filter.
    fn refresh(state: &mut PickerState, fs: &dyn Fs) {
        if let Some(dir) = state.begin_listing() {
            let listed = if dir.is_empty() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "type an absolute path",
                ))
            } else {
                fs.list_dir(Path::new(&dir))
            };
            state.listed(&dir, listed);
        }
        state.refilter();
    }

    // A filesystem that counts how often it was asked to list something.
    struct Counting {
        inner: FakeFs,
        listings: std::sync::atomic::AtomicUsize,
    }

    impl Fs for Counting {
        fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
            self.inner.read_to_string(path)
        }
        fn write(&self, path: &Path, text: &str) -> std::io::Result<()> {
            self.inner.write(path, text)
        }
        fn stamp(&self, path: &Path) -> std::io::Result<crate::fs::Stamp> {
            self.inner.stamp(path)
        }
        fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
            self.listings
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.list_dir(path)
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.inner.is_dir(path)
        }
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }
        fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
            self.inner.canonicalize(path)
        }
    }

    fn workspace_fs() -> FakeFs {
        FakeFs::with_files(&[
            ("/work/deploy.yaml", "a"),
            ("/work/svc.yaml", "b"),
            ("/work/overlays/prod/patch.yaml", "c"),
            ("/work/.git/config", "d"),
            ("/work/target/out.bin", "e"),
            ("/work/README.md", "f"),
        ])
    }

    #[test]
    fn the_picker_filters_completes_and_descends() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        refresh(&mut state, &fs);
        assert!(state.matches.len() >= 5, "everything lists: {state:?}");
        state.input.push_str("ov");
        refresh(&mut state, &fs);
        let (_, segment) = state.split();
        assert_eq!(segment, "ov");
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Descend("/work/overlays/".to_string()),
            "enter descends into the matched folder"
        );
        state.input = "/work/overlays/".to_string();
        refresh(&mut state, &fs);
        state.complete_selected();
        assert_eq!(
            state.input, "/work/overlays/prod/",
            "tab completes with a slash"
        );
        state.input.push_str("patch");
        refresh(&mut state, &fs);
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/overlays/prod/patch.yaml"))
        );
    }

    #[test]
    fn folder_mode_confirms_this_folder_through_its_own_row() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work/overlays"), PickerMode::OpenFolder);
        refresh(&mut state, &fs);
        assert_eq!(
            state.entries[0].label(),
            HERE,
            "the folder being looked at is the first row"
        );
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/overlays")),
            "enter on the dot row opens the folder itself"
        );
        state.selected = 1;
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/overlays/prod")),
            "enter on a child folder opens it, never descends, in folder mode"
        );
    }

    #[test]
    fn folder_mode_refuses_a_file_and_file_mode_descends_into_folders() {
        let fs = workspace_fs();
        let mut folder = PickerState::new(Path::new("/work"), PickerMode::OpenFolder);
        folder.input = "/work/READ".to_string();
        refresh(&mut folder, &fs);
        assert!(
            matches!(folder.confirm(&fs), PickerAction::Reject(note) if note.contains("pick a folder")),
            "a file cannot answer an open-folder request"
        );
        let mut file = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        file.input = "/work/ove".to_string();
        refresh(&mut file, &fs);
        assert_eq!(
            file.confirm(&fs),
            PickerAction::Descend("/work/overlays/".to_string())
        );
    }

    #[test]
    fn save_mode_takes_the_typed_path_and_open_mode_rejects_it() {
        let fs = workspace_fs();
        let mut save = PickerState::new(Path::new("/work"), PickerMode::Save);
        save.input = "/work/new-manifest.yaml".to_string();
        refresh(&mut save, &fs);
        assert_eq!(
            save.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/new-manifest.yaml")),
            "save-as trusts the typed name"
        );
        let mut open = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        open.input = "/work/nope.yaml".to_string();
        refresh(&mut open, &fs);
        assert!(
            matches!(open.confirm(&fs), PickerAction::Reject(_)),
            "open mode refuses a path that does not exist"
        );
    }

    #[test]
    fn parent_walks_up_one_directory() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work/overlays/prod"), PickerMode::OpenFile);
        refresh(&mut state, &fs);
        state.parent();
        assert_eq!(state.input, "/work/overlays/");
        state.parent();
        assert_eq!(state.input, "/work/");
    }

    #[test]
    fn ctrl_up_drops_a_half_typed_name_before_it_leaves_the_folder() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work/overlays"), PickerMode::OpenFile);
        state.input = "/work/overlays/pat".to_string();
        refresh(&mut state, &fs);
        state.parent();
        assert_eq!(
            state.input, "/work/overlays/",
            "the typo goes, the folder stays"
        );
        state.parent();
        assert_eq!(state.input, "/work/");
    }

    #[test]
    fn save_mode_never_confirms_a_highlighted_file_the_user_did_not_type() {
        let fs = workspace_fs();
        let mut save = PickerState::new(Path::new("/work"), PickerMode::Save);
        refresh(&mut save, &fs);
        assert!(
            matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("file name")),
            "enter straight after opening save-as must not overwrite row zero"
        );
        save.input = "/work/deploy.yaml".to_string();
        refresh(&mut save, &fs);
        assert_eq!(
            save.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/deploy.yaml")),
            "a fully typed name is still the answer"
        );
    }

    #[test]
    fn typing_inside_a_listed_directory_costs_no_second_listing() {
        // The picker used to read the folder on every character, which is a
        // syscall per keystroke and a freeze on anything but a local disk.
        let fs = Counting {
            inner: workspace_fs(),
            listings: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        refresh(&mut state, &fs);
        let after_open = fs.listings.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(after_open, 1, "opening reads the folder once");
        for character in "over".chars() {
            state.input.push(character);
            refresh(&mut state, &fs);
        }
        assert_eq!(
            fs.listings.load(std::sync::atomic::Ordering::Relaxed),
            after_open,
            "four keystrokes inside one folder read nothing"
        );
        assert!(
            state
                .matches
                .iter()
                .any(|index| state.entries[*index].label() == "overlays"),
            "and the filter still narrows: {state:?}"
        );
        state.input.push('/');
        refresh(&mut state, &fs);
        assert_eq!(
            fs.listings.load(std::sync::atomic::Ordering::Relaxed),
            after_open + 1,
            "crossing into a new folder reads that one"
        );
    }

    #[test]
    fn a_listing_that_arrives_late_never_replaces_the_folder_being_looked_at() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        let dir = state.begin_listing().expect("a folder to read");
        let listed = fs.list_dir(Path::new(&dir));
        // The user moved on while that was in flight.
        state.input = "/work/overlays/".to_string();
        assert!(
            !state.listed(&dir, listed),
            "the stale answer is dropped rather than shown under the new path"
        );
        assert!(state.entries.is_empty());
    }

    #[test]
    fn a_second_answer_for_the_folder_on_screen_keeps_the_highlighted_row() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        let first = state.begin_listing().expect("a folder to read");
        let slow = fs.list_dir(Path::new(&first));
        // Into a folder and straight back out, so two listings of /work/ are in
        // flight at once.
        state.input = "/work/overlays/".to_string();
        let _inner = state.begin_listing().expect("the folder stepped into");
        state.input = "/work/".to_string();
        let again = state.begin_listing().expect("stepping back asks again");
        assert!(state.listed(&again, fs.list_dir(Path::new(&again))));
        state.refilter();
        state.selected = 3;
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/README.md")),
            "the row the user arrowed to"
        );
        let adopted = state.listed(&first, slow);
        if adopted {
            // What the view does with an answer it accepts.
            state.refilter();
        }
        assert!(
            !adopted,
            "the older request finishing late is not news about this folder"
        );
        assert_eq!(state.selected, 3, "so the highlighted row survives it");
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/README.md")),
            "and enter opens what the eye is on, not row zero"
        );
    }

    #[test]
    fn stepping_back_out_of_a_folder_lists_the_one_left_behind_again() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        refresh(&mut state, &fs);
        assert!(!state.matches.is_empty());
        state.input = "/work/overlays/".to_string();
        let _inner = state.begin_listing().expect("the folder stepped into");
        state.input = "/work/".to_string();
        refresh(&mut state, &fs);
        assert!(
            !state.matches.is_empty(),
            "the rows dropped on the way in have to come back: {state:?}"
        );
    }

    #[test]
    fn a_file_whose_name_is_not_utf8_still_opens() {
        use std::os::unix::ffi::OsStrExt as _;
        // A real name carrying a byte that is not UTF-8: the row can only show
        // it lossily, so a path built from the row names nothing.
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.yaml");
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        let dir = state.begin_listing().expect("a folder to read");
        assert!(state.listed(&dir, Ok(vec![DirEntry::new(name, false)])));
        state.refilter();
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(Path::new("/work").join(name)),
            "the highlighted row opens the file it names"
        );
        state.complete_selected();
        state.refilter();
        assert!(
            state.input.contains('\u{fffd}'),
            "tab can only type the label: {}",
            state.input
        );
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(Path::new("/work").join(name)),
            "and a name tab-completed lossily still opens the real file"
        );
        // Nothing highlighted, so the answer comes from the typed name, which
        // is the row's label and has to resolve back to the row's name.
        state.matches.clear();
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(Path::new("/work").join(name))
        );
    }

    #[test]
    fn a_path_that_exists_opens_while_its_folder_is_still_being_listed() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        // Asked for and not back yet -- seconds of it, on a network mount --
        // so the rows are silent about the typed name rather than authoritative.
        let _asked = state.begin_listing().expect("a folder to read");
        state.input = "/work/deploy.yaml".to_string();
        assert!(!state.listing_is_authoritative());
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/deploy.yaml")),
            "a file that is sitting there is not 'no such path'"
        );
        let mut folder = PickerState::new(Path::new("/work"), PickerMode::OpenFolder);
        let _asked = folder.begin_listing().expect("a folder to read");
        folder.input = "/work/overlays".to_string();
        assert_eq!(
            folder.confirm(&fs),
            PickerAction::Open(PathBuf::from("/work/overlays")),
            "and neither is a folder that is"
        );
    }

    #[test]
    fn a_file_inside_a_folder_that_will_not_list_still_opens() {
        // A directory that is searchable but not listable, mode 0311: the
        // listing fails and the files inside it still read.
        let fs = FakeFs::with_files(&[("/locked/deploy.yaml", "a")]);
        let mut state = PickerState::new(Path::new("/locked"), PickerMode::OpenFile);
        let dir = state.begin_listing().expect("a folder to read");
        assert!(state.listed(
            &dir,
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        ));
        state.refilter();
        assert!(
            !state.listing_is_authoritative(),
            "a listing that failed is not the folder's contents"
        );
        state.input = "/locked/deploy.yaml".to_string();
        assert_eq!(
            state.confirm(&fs),
            PickerAction::Open(PathBuf::from("/locked/deploy.yaml"))
        );
        assert!(
            state.note.is_some(),
            "and the folder's own error is still on screen"
        );
    }

    #[test]
    fn save_refuses_a_folder_as_its_target() {
        let fs = workspace_fs();
        let mut save = PickerState::new(Path::new("/work"), PickerMode::Save);
        let _asked = save.begin_listing().expect("a folder to read");
        save.input = "/work/overlays".to_string();
        assert!(
            matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("folder")),
            "accepting a directory only moves 'is a directory' into the save"
        );
    }

    #[test]
    fn a_bare_name_is_refused_rather_than_written_beside_the_process() {
        let fs = workspace_fs();
        let mut save = PickerState::new(Path::new("/work"), PickerMode::Save);
        save.input = "notes.yaml".to_string();
        refresh(&mut save, &fs);
        assert!(
            matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("absolute")),
            "a name with no directory would land in the process working directory"
        );
        let mut open = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        open.input = "deploy.yaml".to_string();
        refresh(&mut open, &fs);
        assert!(
            matches!(open.confirm(&fs), PickerAction::Reject(note) if note.contains("absolute")),
            "and opening one is not resolved against it either"
        );
    }

    #[test]
    fn a_rejected_enter_is_forgotten_by_the_next_keystroke() {
        let fs = workspace_fs();
        let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
        state.input = "/work/nope.yaml".to_string();
        refresh(&mut state, &fs);
        let PickerAction::Reject(note) = state.confirm(&fs) else {
            panic!("a path that is not there is refused");
        };
        state.note = Some(note);
        state.input.push('x');
        refresh(&mut state, &fs);
        assert_eq!(
            state.note, None,
            "the answer to the last enter is not about this keystroke"
        );
        let mut missing = PickerState::new(Path::new("/nowhere"), PickerMode::OpenFile);
        refresh(&mut missing, &fs);
        let listing_error = missing.note.clone();
        assert!(
            listing_error.is_some(),
            "a folder that will not list says so"
        );
        missing.input.push('a');
        refresh(&mut missing, &fs);
        assert_eq!(
            missing.note, listing_error,
            "and keeps saying so while the user types inside it"
        );
    }

    #[test]
    fn the_scan_states_a_depth_truncation_and_counts_unreadable_folders() {
        let mut deep = String::from("/work");
        for level in 0..(MAX_SCAN_DEPTH + 2) {
            deep.push_str(&format!("/d{level}"));
        }
        deep.push_str("/buried.yaml");
        let fs = FakeFs::with_files(&[("/work/top.yaml", "t"), (deep.as_str(), "b")]);
        let scan = scan_root(&fs, Path::new("/work"));
        assert!(scan.files.iter().any(|file| file.label == "top.yaml"));
        assert!(
            scan.capped,
            "a subtree dropped for depth is a truncation the modal must state"
        );
    }

    #[test]
    fn the_scan_skips_ignored_directories_and_states_its_cap() {
        let fs = workspace_fs();
        let scan = scan_root(&fs, Path::new("/work"));
        assert!(scan.files.iter().any(|file| file.label == "deploy.yaml"));
        assert!(
            scan.files
                .iter()
                .any(|file| file.label == "overlays/prod/patch.yaml"
                    && file.path == Path::new("overlays/prod/patch.yaml"))
        );
        assert!(
            !scan
                .files
                .iter()
                .any(|file| file.label.contains(".git") || file.label.contains("target")),
            "ignored directories stay out: {:?}",
            scan.files
        );
        assert!(!scan.capped);
    }
}
