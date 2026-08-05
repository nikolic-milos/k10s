//! The YAML editor item: a thin gpui view over the `k10s-edit` engine.
//!
//! Everything that thinks lives in the engine -- rope, cursors, tree, schema
//! index -- and everything here projects: visible rows become styled runs
//! (syntax colours, selections, search matches, wavy diagnostics, a steady
//! run-styled caret, never a blinking one), the completion menu is a list
//! the engine ranked, and the status line labels every state. A cluster
//! document keeps the text the fetch returned beside the buffer it became, so
//! `diff_sources` can hand a diff three documents rather than two; ctrl-s on
//! one asks the server what it would store instead of writing anything.
//! Schemas arrive lazily through a per-workspace [`SchemaStore`] -- catalog
//! once, CRDs once, one document per group-version on demand -- and every
//! fetch outcome that is not `Ok` becomes a labelled note, never a silent
//! absence. Typing reaches the buffer through `key_char` exactly like the
//! terminal; named keys and chords arrive as `Editor`-context actions.
//! [`DirtyState`] owns the three moments that can throw work away -- closing,
//! reloading, and overwriting a file this buffer did not read -- and each
//! costs a second press that any edit in between disarms. Each arms under its
//! own name, so a press that answers a different question re-asks instead of
//! firing. A fourth moment lives outside this file -- the apply the diff view
//! confirms -- and it is keyed by [`BufferStamp`] rather than by a version,
//! because `Buffer::new` restarts versions at zero and a counter that restarts
//! cannot tell a document from its replacement.
//!
//! Three invariants the view is responsible for, each of which was once a bug:
//! every mutation ends in [`EditorView::committed`], whatever produced it, so
//! an undo cannot leave the metadata, the diagnostics, an armed confirmation or
//! the tab's dirty marker describing the previous text. Saves go through
//! [`SaveQueue`], one write in flight and only the newest text queued behind
//! it, because a write per keypress leaves the order to the executor and an
//! older write can land last on a buffer already marked clean. And nothing
//! blocking touches this thread: reads, writes, and even the conflict stamp run
//! on the background executor, and a viewport is one tree query walked forward
//! with the rows rather than a query and a full rescan per row.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Context, FocusHandle, HighlightStyle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Render, Role, ScrollWheelEvent, SharedString,
    Styled, StyledText, UnderlineStyle, WeakEntity, Window, canvas, div, prelude::*, px, rgb,
};

use k10s_edit::complete::{
    Completion, DiagnosticSeverity, complete, complete_with_root, doc_meta, validate,
    validate_with_root,
};
use k10s_edit::schema::SchemaNode;
use k10s_edit::{
    Buffer, CursorPosition, Diagnostic, DocMeta, EditGroup, LanguageKind, Motion, Replacement,
    Rope, SchemaIndex, SearchState, Selection, SelectionIntent, Syntax, TokenKind, completion_edit,
};

use k10s_theme::{Theme, Typography};

use crate::fs::{Fs, Stamp};
use crate::provider::{
    DescribeRequest, ManifestOutcome, ReadProvider, Reply, SchemaCatalogOutcome, SchemaSource,
    SchemaTextOutcome,
};
use crate::ui::{CONTENT_PADDING, STATUS_BAR_HEIGHT, Viewport};
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

const MAX_VISIBLE_COMPLETIONS: usize = 8;
const COMPLETION_WIDTH: f32 = 460.0;
const MAX_DOC_CHARS: usize = 280;

// One schema index per workspace: catalog and CRDs fetched once, one OpenAPI
// document per group-version fetched the first time an editor needs it, and
// every non-Ok outcome a labelled note. Shared by `Rc<RefCell>` because the
// provider handle itself never leaves the UI thread.
pub struct SchemaStore {
    pub index: SchemaIndex,
    catalog: Vec<SchemaSource>,
    requested_catalog: bool,
    requested_crds: bool,
    requested_documents: HashSet<String>,
    notes: Vec<String>,
}

impl Default for SchemaStore {
    fn default() -> SchemaStore {
        SchemaStore::new()
    }
}

impl SchemaStore {
    pub fn new() -> SchemaStore {
        SchemaStore {
            index: SchemaIndex::new(),
            catalog: Vec::new(),
            requested_catalog: false,
            requested_crds: false,
            requested_documents: HashSet::new(),
            notes: Vec::new(),
        }
    }

    fn note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    fn first_note(&self) -> Option<&str> {
        self.notes.first().map(String::as_str)
    }
}

struct CompletionMenu {
    items: Vec<Completion>,
    selected: usize,
    anchor: usize,
}

struct SearchBar {
    input: String,
    replace: Option<String>,
    regex: bool,
    state: SearchState,
    typing_replace: bool,
}

impl SearchBar {
    fn new() -> SearchBar {
        SearchBar {
            input: String::new(),
            replace: None,
            regex: false,
            state: SearchState::new("", false),
            typing_replace: false,
        }
    }

    fn recompile(&mut self) {
        self.state = SearchState::new(&self.input, self.regex);
    }
}

// Where a buffer's text lives: a cluster object read through the provider, a
// local file behind the Fs seam, or nowhere yet. New sources extend the enum
// and the three dispatchers (reload, save, schema scope), nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorSource {
    Cluster(DescribeRequest),
    File(PathBuf),
    Scratch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveStep {
    Write,
    Confirm(Overwrite),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overwrite {
    // Read, then changed underneath us.
    ChangedOnDisk,
    // Never read by this buffer, but something is already there.
    AlreadyExists,
}

impl Overwrite {
    fn note(self) -> &'static str {
        match self {
            Overwrite::ChangedOnDisk => "the file changed on disk; ctrl-s again to overwrite",
            Overwrite::AlreadyExists => "that file already exists; ctrl-s again to overwrite",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseStep {
    Close,
    Warn,
}

// Which destructive action is armed. One shared bit could not tell them apart,
// so a warning about a reload was answered by the next close: the buffer went
// away on a single press. Each action arms its own name, and a press that
// answers a different question re-asks rather than firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Armed {
    Close,
    Reload,
    Overwrite,
}

// Unsaved work, and the three destructive moments that need a second press:
// overwriting a file that changed underneath, reloading over unsaved work, and
// closing a buffer that has not been written. Each arms on the first press and
// fires on the second, and any edit disarms -- typing means the user is not
// answering the question any more. Pure, so the rules are tested without a
// window.
#[derive(Debug, Default)]
pub(crate) struct DirtyState {
    clean_version: Option<u64>,
    disk_stamp: Option<Stamp>,
    armed: Option<Armed>,
}

impl DirtyState {
    pub(crate) fn is_dirty(&self, version: u64) -> bool {
        self.clean_version != Some(version)
    }

    pub(crate) fn mark_clean(&mut self, version: u64, stamp: Option<Stamp>) {
        self.clean_version = Some(version);
        if stamp.is_some() {
            self.disk_stamp = stamp;
        }
        self.armed = None;
    }

    pub(crate) fn forget_disk(&mut self) {
        self.disk_stamp = None;
        if self.armed == Some(Armed::Overwrite) {
            self.armed = None;
        }
    }

    pub(crate) fn edited(&mut self) {
        self.armed = None;
    }

    // `on_disk` is the stamp read right now, or None when the path cannot be
    // stamped at all -- deleted out from under us, or never written. Neither
    // is a conflict: writing recreates exactly what the buffer holds.
    pub(crate) fn save_step(&mut self, on_disk: Option<Stamp>) -> SaveStep {
        if self.armed == Some(Armed::Overwrite) {
            self.armed = None;
            return SaveStep::Write;
        }
        let overwrite = match (self.disk_stamp, on_disk) {
            (Some(known), Some(now)) if known != now => Some(Overwrite::ChangedOnDisk),
            (None, Some(_)) => Some(Overwrite::AlreadyExists),
            _ => None,
        };
        match overwrite {
            Some(reason) => {
                self.armed = Some(Armed::Overwrite);
                SaveStep::Confirm(reason)
            }
            None => SaveStep::Write,
        }
    }

    // Reloading throws away unsaved work exactly like closing does, so it asks
    // the same way -- under its own name.
    pub(crate) fn reload_step(&mut self, version: u64) -> CloseStep {
        self.destructive_step(version, Armed::Reload)
    }

    pub(crate) fn close_step(&mut self, version: u64) -> CloseStep {
        self.destructive_step(version, Armed::Close)
    }

    fn destructive_step(&mut self, version: u64, action: Armed) -> CloseStep {
        if !self.is_dirty(version) {
            return CloseStep::Close;
        }
        if self.armed == Some(action) {
            self.armed = None;
            return CloseStep::Close;
        }
        self.armed = Some(action);
        CloseStep::Warn
    }
}

// One buffer's saves, strictly ordered. Spawning a write per press left the
// order to the executor: an older write could rename last while the newer
// buffer was already marked clean, so the file on disk was not what the editor
// said was saved. One write in flight, one queued behind it, and the queued one
// is always the newest text. Pure, so the ordering is tested without a window.
#[derive(Debug, Default)]
pub(crate) struct SaveQueue {
    // Which write owns the flight, so only that write can hand it on. A save
    // the queue has abandoned is still running, and letting it advance the
    // queue would start a second write beside one already in progress -- the
    // very race the queue exists to prevent.
    flight: Option<u64>,
    pending: Option<PendingSave>,
    issued: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSave {
    pub(crate) path: PathBuf,
    pub(crate) text: String,
    pub(crate) version: u64,
    // Which buffer this text came from. A reload or a save-as replaces the
    // buffer, and its versions restart, so a completion that arrives afterwards
    // describes a document that no longer exists: marking that version clean
    // hands the tab a false answer, and adopting its stamp invents a conflict.
    pub(crate) generation: u64,
    // Which flight this write is, so the queue can tell the write it is waiting
    // for from one it has already let go of.
    pub(crate) ticket: u64,
}

impl SaveQueue {
    // What to start now: the request itself when nothing is running, or
    // nothing, with the request held as the one that follows.
    pub(crate) fn request(&mut self, mut save: PendingSave) -> Option<PendingSave> {
        if self.flight.is_some() {
            // Only the newest text is worth writing; the presses in between
            // asked for versions this one supersedes.
            self.pending = Some(save);
            return None;
        }
        save.ticket = self.take_off();
        Some(save)
    }

    // A write finished, so the queued one may start -- but only if this is the
    // write the queue is actually waiting for.
    pub(crate) fn finished(&mut self, ticket: u64) -> Option<PendingSave> {
        if self.flight != Some(ticket) {
            return None;
        }
        match self.pending.take() {
            Some(mut next) => {
                next.ticket = self.take_off();
                Some(next)
            }
            None => {
                self.flight = None;
                None
            }
        }
    }

    // The queue lets go: a conflict needs answering first, or the work was
    // discarded. Whatever is still running keeps running -- it cannot be
    // recalled -- but it no longer speaks for the queue.
    pub(crate) fn abandon(&mut self) {
        self.flight = None;
        self.pending = None;
    }

    fn take_off(&mut self) -> u64 {
        self.issued += 1;
        self.flight = Some(self.issued);
        self.issued
    }
}

pub enum EditorEvent {
    SaveAsRequested,
    StateChanged,
    // A cluster document asked to be compared: against what it was fetched as
    // and what was last applied, and when `dry_run` also against what the
    // server says it would store. The workspace owns the diff item, so the
    // editor asks rather than opens.
    DiffRequested { dry_run: bool },
}

/// Which document, and which revision of it. `Buffer::new` restarts versions at
/// zero, so a version alone cannot tell one document from its replacement: a
/// diff made of the object as it was fetched would authorise an apply against
/// the object as it was re-fetched, and the same number of keystrokes is all it
/// takes to line the two counters up. The generation is bumped by every
/// identity change -- reload, save-as, a fresh fetch -- so the pair is unique
/// for the life of the view.
///
/// The fields are private and equality is the only operation, which is what
/// stops a caller inventing a stamp that authorises a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferStamp {
    generation: u64,
    version: u64,
}

impl BufferStamp {
    // Two distinguishable stamps for the pure tests of the rules that compare
    // them. Production has exactly one source, `EditorView::buffer_stamp`, and
    // that is the point of the private fields.
    #[cfg(test)]
    pub(crate) fn of(generation: u64, version: u64) -> BufferStamp {
        BufferStamp {
            generation,
            version,
        }
    }
}

// What a diff needs from the buffer that produced it. `live` is the text the
// fetch returned, held pristine: the buffer is what it became, and comparing a
// buffer with itself would show nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSources {
    pub request: DescribeRequest,
    pub title: String,
    // Which buffer this comparison is of. An apply reviewed at one stamp must
    // not send another: the diff is the review, and text that arrived after it
    // was never reviewed.
    pub stamp: BufferStamp,
    // Which editor it came from. A diff tab outlives the editor that opened it
    // and is reused by the editor that replaces it, so the handle has to travel
    // with the sources; a tab that kept its first one answered every later
    // press with "the editor this diff came from is gone" while that editor sat
    // open in the next tab.
    pub editor: WeakEntity<EditorView>,
    pub live: String,
    pub base: Option<String>,
    pub buffer: String,
    // Which object `live` was read from, as the server stated it in that read --
    // not the uid of whatever was selected to open this editor, which can name
    // an object that has since been replaced. An apply's answer carries the same
    // field, and that comparison is what tells an update from a recreation.
    pub uid: Option<String>,
    // The buffer with everything the server owns removed: what an apply sends.
    pub payload: k10s_edit::Payload,
    pub patchable: bool,
}

impl gpui::EventEmitter<EditorEvent> for EditorView {}

pub struct EditorView {
    focus: FocusHandle,
    // This view's own handle, so `diff_sources` can name the entity a
    // comparison belongs to. A diff is opened by the workspace, which holds the
    // strong handle; the sources it is built from have to carry the identity or
    // a reused diff tab keeps pointing at the editor it was first opened from.
    handle: WeakEntity<EditorView>,
    provider: Rc<dyn ReadProvider>,
    fs: Arc<dyn Fs>,
    source: EditorSource,
    title: SharedString,
    buffer: Buffer,
    syntax: Syntax,
    diagnostics: Vec<Diagnostic>,
    meta: DocMeta,
    schema: Rc<RefCell<SchemaStore>>,
    schema_root: Option<Arc<SchemaNode>>,
    // The cluster's own text for this object, as fetched. The buffer diverges
    // from it the moment anything is typed, and the diff needs both.
    live: Option<String>,
    // The `last-applied-configuration`, rendered: a three-way diff's base.
    base: Option<String>,
    // Which object `live` came from. Replaced by every read, which is what keeps
    // it from ageing into a claim about an object that has since been recreated.
    uid: Option<String>,
    patchable: bool,
    status_subresource: bool,
    completion: Option<CompletionMenu>,
    search: Option<SearchBar>,
    scroll_top: usize,
    status: Option<String>,
    dirty: DirtyState,
    // The dirtiness the workspace has been told about, so a keystroke that does
    // not change it costs nothing outside this view.
    reported_dirty: bool,
    saves: SaveQueue,
    // What a file that does not exist yet opens with: the settings and keymap
    // templates. Held rather than checked up front, so opening costs no stat on
    // the UI thread.
    template: Option<&'static str>,
    generation: u64,
    viewport: Viewport,
    rows: usize,
    origin: (f32, f32),
    dragging: bool,
}

impl EditorView {
    fn build(
        provider: Rc<dyn ReadProvider>,
        fs: Arc<dyn Fs>,
        schema: Rc<RefCell<SchemaStore>>,
        source: EditorSource,
        title: String,
        language: LanguageKind,
        cx: &mut Context<Self>,
    ) -> EditorView {
        EditorView {
            focus: cx.focus_handle(),
            handle: cx.weak_entity(),
            provider,
            fs,
            source,
            title: title.into(),
            buffer: Buffer::new(""),
            syntax: Syntax::new(language),
            diagnostics: Vec::new(),
            meta: DocMeta::default(),
            schema,
            schema_root: None,
            live: None,
            base: None,
            uid: None,
            patchable: false,
            status_subresource: false,
            completion: None,
            search: None,
            scroll_top: 0,
            status: None,
            dirty: DirtyState::default(),
            // A buffer with no clean point is dirty, which is the state a tab
            // opens showing.
            reported_dirty: true,
            saves: SaveQueue::default(),
            template: None,
            generation: 0,
            viewport: Viewport::default(),
            rows: 4,
            origin: (0.0, 0.0),
            dragging: false,
        }
    }

    pub fn cluster(
        provider: Rc<dyn ReadProvider>,
        fs: Arc<dyn Fs>,
        schema: Rc<RefCell<SchemaStore>>,
        request: DescribeRequest,
        cx: &mut Context<Self>,
    ) -> EditorView {
        let title = format!("{}.yaml", request.name);
        let mut view = Self::build(
            provider,
            fs,
            schema,
            EditorSource::Cluster(request),
            title,
            LanguageKind::Yaml,
            cx,
        );
        view.load(cx);
        view
    }

    pub fn file(
        provider: Rc<dyn ReadProvider>,
        fs: Arc<dyn Fs>,
        schema: Rc<RefCell<SchemaStore>>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> EditorView {
        let title = file_title(&path);
        let language = LanguageKind::from_file_name(&title);
        let mut view = Self::build(
            provider,
            fs,
            schema,
            EditorSource::File(path),
            title,
            language,
            cx,
        );
        view.load(cx);
        view
    }

    pub fn scratch(
        provider: Rc<dyn ReadProvider>,
        fs: Arc<dyn Fs>,
        schema: Rc<RefCell<SchemaStore>>,
        title: String,
        cx: &mut Context<Self>,
    ) -> EditorView {
        let language = LanguageKind::from_file_name(&title);
        let mut view = Self::build(
            provider,
            fs,
            schema,
            EditorSource::Scratch,
            title,
            language,
            cx,
        );
        view.syntax.reparse(view.buffer.rope());
        view.dirty.mark_clean(view.buffer.version(), None);
        view.ensure_schema(cx);
        view
    }

    // A fixed schema root -- settings and keymap files know their shape by
    // identity rather than by apiVersion/kind resolution.
    pub fn with_schema_root(mut self, root: Arc<SchemaNode>) -> EditorView {
        self.schema_root = Some(root);
        self
    }

    // Open a file that may not exist yet, seeding the buffer from a template
    // when it does not: how the settings and keymap files open before their
    // first save.
    pub fn file_or_template(
        provider: Rc<dyn ReadProvider>,
        fs: Arc<dyn Fs>,
        schema: Rc<RefCell<SchemaStore>>,
        path: PathBuf,
        template: &'static str,
        cx: &mut Context<Self>,
    ) -> EditorView {
        let title = file_title(&path);
        let language = LanguageKind::from_file_name(&title);
        let mut view = Self::build(
            provider,
            fs,
            schema,
            EditorSource::File(path),
            title,
            language,
            cx,
        );
        view.template = Some(template);
        view.load(cx);
        view
    }

    pub fn title(&self) -> SharedString {
        if self.is_dirty() {
            format!("● {}", self.title).into()
        } else {
            self.title.clone()
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn source(&self) -> &EditorSource {
        &self.source
    }

    /// Whether there is a live version of this buffer to compare against, which
    /// is the same question `diff_sources` answers by building the whole
    /// comparison -- two copies of the document plus a prune walk -- and then
    /// being asked only `is_some()`.
    pub fn has_live_version(&self) -> bool {
        matches!(self.source, EditorSource::Cluster(_)) && self.live.is_some()
    }

    // Everything a diff or an apply needs, or None when this buffer is not a
    // cluster object or has not finished loading -- a diff against a document
    // that never arrived would compare an empty buffer with nothing.
    pub fn diff_sources(&self) -> Option<DiffSources> {
        let EditorSource::Cluster(request) = &self.source else {
            return None;
        };
        let live = self.live.clone()?;
        let cursor = self.buffer.primary_selection().head;
        let document = self.syntax.document_index_at(self.buffer.rope(), cursor);
        Some(DiffSources {
            request: request.clone(),
            title: self.title.to_string(),
            stamp: self.buffer_stamp(),
            editor: self.handle.clone(),
            live,
            base: self.base.clone(),
            buffer: self.buffer.text(),
            uid: self.uid.clone(),
            payload: k10s_edit::apply::payload(
                self.buffer.rope(),
                &self.syntax,
                document,
                self.status_subresource,
            ),
            patchable: self.patchable,
        })
    }

    /// Which document and which revision of it, which is what identifies the
    /// text a comparison was made of.
    pub fn buffer_stamp(&self) -> BufferStamp {
        BufferStamp {
            generation: self.generation,
            version: self.buffer.version(),
        }
    }

    /// What an apply that landed leaves behind: the object the cluster now holds,
    /// which is not necessarily the bytes that were sent, because defaulting and
    /// admission both run. Re-reading is the only honest way to show it -- but
    /// only while the buffer is still the one that was applied. Anything typed
    /// since is work the apply never carried, and replacing it would throw away
    /// edits nobody was asked about -- including work in a buffer that merely
    /// reached the same version number after a reload restarted the count.
    pub fn reload_if_applied(&mut self, stamp: BufferStamp, cx: &mut Context<Self>) -> bool {
        if !matches!(self.source, EditorSource::Cluster(_)) || self.buffer_stamp() != stamp {
            return false;
        }
        self.saves.abandon();
        self.load(cx);
        true
    }

    /// Splice one hunk of a comparison into the buffer, while the buffer is still
    /// the one that comparison was made of. False when it is not, or when the
    /// range does not land on this text.
    ///
    /// The stamp check is the same rule an apply follows and for a sharper
    /// reason: an apply of text nobody reviewed is a bad write, but an *edit*
    /// whose byte ranges came from a document the buffer no longer holds is a
    /// splice at a meaningless offset -- it would corrupt the very text it was
    /// meant to correct. The bounds check keeps that from being a panic in the
    /// rope if the rule is ever broken elsewhere, since the ranges are data that
    /// travelled.
    ///
    /// The selection is preserved rather than collapsed: this is a structural
    /// edit like replace-all, not a keystroke, and the caret belongs where the
    /// person left it.
    pub fn keep_hunk(
        &mut self,
        stamp: BufferStamp,
        keep: k10s_edit::diff::Keep,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.buffer_stamp() != stamp {
            return false;
        }
        let text = self.buffer.rope();
        if keep.range.end > text.len()
            || !text.is_char_boundary(keep.range.start)
            || !text.is_char_boundary(keep.range.end)
        {
            debug_assert!(
                false,
                "a hunk range belongs to the buffer it was taken from"
            );
            return false;
        }
        let splices = self.buffer.edit(
            vec![(keep.range, keep.text)],
            EditGroup::Other,
            SelectionIntent::Preserve,
        );
        self.after_edit(splices, cx);
        true
    }

    fn is_dirty(&self) -> bool {
        self.dirty.is_dirty(self.buffer.version())
    }

    // Fetch the source into the buffer. A buffer that has never been loaded is
    // dirty by definition -- it has no clean point yet -- so opening must not go
    // through the guard below, or every file opens empty behind a warning about
    // unsaved changes it does not have.
    fn load(&mut self, cx: &mut Context<Self>) {
        match self.source.clone() {
            EditorSource::Cluster(request) => self.reload_cluster(request, cx),
            EditorSource::File(path) => self.reload_file(path, cx),
            EditorSource::Scratch => {
                self.status = Some("this buffer has no file to reload from".to_string());
                cx.notify();
            }
        }
    }

    // The reload action, which throws unsaved work away and so asks first. It
    // also empties the save queue: a save queued behind the in-flight one holds
    // the text the user has just chosen to discard, and writing it afterwards
    // would put the discarded version on disk.
    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.is_dirty() && self.dirty.reload_step(self.buffer.version()) == CloseStep::Warn {
            self.status = Some("unsaved changes; reload again to discard them".to_string());
            cx.notify();
            return;
        }
        self.saves.abandon();
        self.load(cx);
    }

    fn reload_cluster(&mut self, request: DescribeRequest, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.status = Some("loading...".to_string());
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply: Reply<ManifestOutcome> = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        self.provider.fetch_manifest(&request, reply);
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    match outcome {
                        ManifestOutcome::Manifest {
                            title,
                            yaml,
                            api_version,
                            kind,
                            last_applied,
                            patchable,
                            status_subresource,
                            uid,
                        } => {
                            this.title = title.into();
                            this.buffer = Buffer::new(&yaml);
                            this.syntax.reparse(this.buffer.rope());
                            this.live = Some(yaml);
                            this.base = last_applied;
                            this.uid = uid;
                            this.patchable = patchable;
                            this.status_subresource = status_subresource;
                            this.meta = DocMeta {
                                api_version: Some(api_version),
                                kind: Some(kind),
                            };
                            this.dirty.mark_clean(this.buffer.version(), None);
                            this.scroll_top = 0;
                            this.status = None;
                            this.resync_search();
                            this.publish_state(cx);
                            this.ensure_schema(cx);
                            this.schedule_validation(cx);
                        }
                        ManifestOutcome::Denied(what) => {
                            this.status = Some(format!("{what}: access denied for this account"));
                        }
                        ManifestOutcome::Failed(why) => this.status = Some(why),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn reload_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.status = Some("loading...".to_string());
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let text = fs.read_to_string(&path)?;
                    let stamp = fs.stamp(&path)?;
                    Ok::<(String, Stamp), std::io::Error>((text, stamp))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match loaded {
                    Ok((text, stamp)) => {
                        this.buffer = Buffer::new(&text);
                        this.syntax.reparse(this.buffer.rope());
                        this.refresh_meta();
                        this.dirty.mark_clean(this.buffer.version(), Some(stamp));
                        this.scroll_top = 0;
                        this.status = None;
                        this.resync_search();
                        this.publish_state(cx);
                        this.ensure_schema(cx);
                        this.schedule_validation(cx);
                    }
                    // A file that is not there yet is not a failure when the
                    // caller brought a template for exactly that case.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        match this.template {
                            Some(template) => {
                                this.buffer = Buffer::new(template);
                                this.syntax.reparse(this.buffer.rope());
                                this.refresh_meta();
                                this.dirty.forget_disk();
                                this.scroll_top = 0;
                                this.status = Some("new file; ctrl-s writes it".to_string());
                                this.resync_search();
                                this.publish_state(cx);
                                this.ensure_schema(cx);
                                this.schedule_validation(cx);
                            }
                            None => this.status = Some(format!("open failed: {error}")),
                        }
                    }
                    Err(error) => this.status = Some(format!("open failed: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_meta(&mut self) {
        let cursor = self.buffer.primary_selection().head;
        let document = self.syntax.document_index_at(self.buffer.rope(), cursor);
        self.meta = doc_meta(self.buffer.rope(), &self.syntax, document);
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.source.clone() {
            EditorSource::File(path) => self.save_to(path, cx),
            EditorSource::Scratch => {
                cx.emit(EditorEvent::SaveAsRequested);
            }
            // A cluster document is never written blind: ctrl-s asks the server
            // what it would store and opens that answer as a diff, and the
            // apply is a second, deliberate press inside it.
            EditorSource::Cluster(_) => {
                cx.emit(EditorEvent::DiffRequested { dry_run: true });
            }
        }
    }

    // The picker hands back a path: the buffer adopts it as its file, takes
    // its language from the extension, and saves.
    pub fn assign_path_and_save(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.title = file_title(&path).into();
        let language = LanguageKind::from_file_name(&self.title);
        if language != self.syntax.language() {
            self.syntax = Syntax::new(language);
            self.syntax.reparse(self.buffer.rope());
            self.schedule_validation(cx);
        }
        self.source = EditorSource::File(path.clone());
        // A fixed root is bound to a file's identity: once this buffer is a
        // different file, the settings or keymap schema is not its schema.
        self.schema_root = None;
        // The identity changed, so anything already in flight describes the
        // previous file and must not answer for this one.
        self.generation += 1;
        self.dirty.forget_disk();
        self.save_to(path, cx);
        self.reported_dirty = self.is_dirty();
        cx.emit(EditorEvent::StateChanged);
    }

    fn save_to(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let request = PendingSave {
            path,
            text: self.buffer.text(),
            version: self.buffer.version(),
            generation: self.generation,
            ticket: 0,
        };
        // A save already running owns the file, and it already answered the
        // conflict question; this text queues behind it.
        let Some(start) = self.saves.request(request) else {
            self.status = Some("saving...".to_string());
            cx.notify();
            return;
        };
        self.begin_save(start, cx);
    }

    fn begin_save(&mut self, save: PendingSave, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        self.status = Some("saving...".to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            // Stamping the file is a syscall, and on a network mount it is not
            // one a frame can wait for, so even the conflict check happens off
            // the UI thread.
            let stamped = {
                let fs = fs.clone();
                let path = save.path.clone();
                cx.background_executor()
                    .spawn(async move { fs.stamp(&path).ok() })
                    .await
            };
            let cleared = this.update(cx, |this, cx| {
                if save.generation != this.generation {
                    // The buffer this text came from is gone, so its conflict
                    // question is not one to ask. Whatever was asked for since
                    // owns the queue.
                    if let Some(next) = this.saves.finished(save.ticket) {
                        this.begin_save(next, cx);
                    }
                    return false;
                }
                match this.dirty.save_step(stamped) {
                    SaveStep::Confirm(reason) => {
                        // The user answers before anything is written, and the
                        // queue empties so the answer is a deliberate press
                        // rather than whatever was typed ahead.
                        this.saves.abandon();
                        this.status = Some(reason.note().to_string());
                        cx.notify();
                        false
                    }
                    SaveStep::Write => true,
                }
            });
            if !matches!(cleared, Ok(true)) {
                return;
            }
            let written = {
                let path = save.path.clone();
                let text = save.text.clone();
                cx.background_executor()
                    .spawn(async move {
                        fs.write(&path, &text)?;
                        // The write is what mattered; a stamp we cannot read
                        // only costs us the conflict check on the next save.
                        Ok::<Option<Stamp>, std::io::Error>(fs.stamp(&path).ok())
                    })
                    .await
            };
            let _ = this.update(cx, |this, cx| {
                // Save-as adopts a new path, and a reload replaces the buffer,
                // while the previous write is still in flight. That write was
                // asked for and is correct, but its result describes a document
                // this view is no longer showing: marking that version clean
                // hands the tab a false answer, and adopting the old file's
                // stamp invents a conflict on the next save.
                let still_ours = save.generation == this.generation
                    && matches!(&this.source, EditorSource::File(path) if *path == save.path);
                match written {
                    Ok(stamp) if still_ours => {
                        this.dirty.mark_clean(save.version, stamp);
                        this.status = stamp.is_none().then(|| {
                            "saved, but the file cannot be stamped; the next save will \
                             ask before overwriting"
                                .to_string()
                        });
                    }
                    Ok(_) => {}
                    Err(error) => this.status = Some(format!("save failed: {error}")),
                }
                this.publish_state(cx);
                cx.notify();
                if let Some(next) = this.saves.finished(save.ticket) {
                    this.begin_save(next, cx);
                }
            });
        })
        .detach();
    }

    fn ensure_schema(&mut self, cx: &mut Context<Self>) {
        if self.schema_root.is_some() {
            return;
        }
        let fetch_catalog = {
            let mut store = self.schema.borrow_mut();
            !std::mem::replace(&mut store.requested_catalog, true)
        };
        if fetch_catalog {
            let (tx, rx) = futures::channel::oneshot::channel();
            let reply: Reply<SchemaCatalogOutcome> = Box::new(move |outcome| {
                let _ = tx.send(outcome);
            });
            self.provider.fetch_schema_catalog(reply);
            cx.spawn(async move |this, cx| {
                if let Ok(outcome) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        {
                            let mut store = this.schema.borrow_mut();
                            match outcome {
                                SchemaCatalogOutcome::Catalog(sources) => {
                                    for source in &sources {
                                        store.index.add_api_version(&source.group_version);
                                    }
                                    store.catalog = sources;
                                }
                                SchemaCatalogOutcome::Denied(what) => {
                                    store.note(format!("{what}: access denied for this account"))
                                }
                                SchemaCatalogOutcome::Failed(why) => store.note(why),
                            }
                        }
                        this.ensure_documents(cx);
                        this.schedule_validation(cx);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        let fetch_crds = {
            let mut store = self.schema.borrow_mut();
            !std::mem::replace(&mut store.requested_crds, true)
        };
        if fetch_crds {
            let (tx, rx) = futures::channel::oneshot::channel();
            let reply: Reply<SchemaTextOutcome> = Box::new(move |outcome| {
                let _ = tx.send(outcome);
            });
            self.provider.fetch_crd_schemas(reply);
            cx.spawn(async move |this, cx| {
                if let Ok(outcome) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        {
                            let mut store = this.schema.borrow_mut();
                            match outcome {
                                SchemaTextOutcome::Text(json) => {
                                    if let Err(why) = store.index.add_crd_list(&json) {
                                        store.note(format!("CRD schemas: {why}"));
                                    }
                                }
                                SchemaTextOutcome::Denied(what) => {
                                    store.note(format!("{what}: access denied for this account"))
                                }
                                SchemaTextOutcome::Failed(why) => store.note(why),
                            }
                        }
                        this.schedule_validation(cx);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        self.ensure_documents(cx);
    }

    fn ensure_documents(&mut self, cx: &mut Context<Self>) {
        let Some(api_version) = self.meta.api_version.clone() else {
            return;
        };
        let url = {
            let mut store = self.schema.borrow_mut();
            let Some(source) = store
                .catalog
                .iter()
                .find(|source| source.group_version == api_version)
                .cloned()
            else {
                return;
            };
            if !store.requested_documents.insert(source.group_version) {
                return;
            }
            source.url
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        let reply: Reply<SchemaTextOutcome> = Box::new(move |outcome| {
            let _ = tx.send(outcome);
        });
        self.provider.fetch_schema_document(&url, reply);
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    {
                        let mut store = this.schema.borrow_mut();
                        match outcome {
                            SchemaTextOutcome::Text(json) => {
                                if let Err(why) = store.index.add_openapi_document(&json) {
                                    store.note(format!("schema document: {why}"));
                                }
                            }
                            SchemaTextOutcome::Denied(what) => {
                                store.note(format!("{what}: access denied for this account"));
                            }
                            SchemaTextOutcome::Failed(why) => store.note(why),
                        }
                    }
                    this.schedule_validation(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn revalidate(&mut self) {
        let store = self.schema.borrow();
        self.diagnostics = match &self.schema_root {
            Some(root) => validate_with_root(&store.index, self.buffer.rope(), &self.syntax, root),
            None => validate(&store.index, self.buffer.rope(), &self.syntax),
        };
    }

    // Small buffers validate on the keystroke; large ones validate after the
    // typing pauses, because a whole-document schema walk over a megabyte is
    // measured in milliseconds and does not belong between two characters.
    fn schedule_validation(&mut self, cx: &mut Context<Self>) {
        const SYNCHRONOUS_LIMIT: usize = 64 << 10;
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(150);
        if self.buffer.rope().len() <= SYNCHRONOUS_LIMIT {
            self.revalidate();
            return;
        }
        let version = self.buffer.version();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            let _ = this.update(cx, |this, cx| {
                // The typing stopped, so this is where the walk happens. Arming
                // another timer here instead meant a large buffer was never
                // validated at all and an idle tab woke seven times a second
                // for the rest of its life.
                if this.buffer.version() == version {
                    this.revalidate();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // Every mutation ends here, whatever made it. An edit that skipped this
    // left an armed overwrite confirmation alive across a real change, left the
    // document metadata and the diagnostics describing the previous text, and
    // never told the workspace its tab was dirty.
    fn committed(&mut self, cx: &mut Context<Self>) {
        self.refresh_meta();
        self.dirty.edited();
        if self.schema_root.is_none() {
            self.ensure_documents(cx);
        }
        self.schedule_validation(cx);
        if let Some(search) = &mut self.search {
            search.state.refresh(&self.buffer);
        }
        self.ensure_visible();
        self.publish_state(cx);
        cx.notify();
    }

    // Tell the workspace when the tab's label changes and only then: its handler
    // retags the tab and repaints the whole workspace, which is not something
    // every keystroke should cost when only the first one turns the dot on.
    fn publish_state(&mut self, cx: &mut Context<Self>) {
        let dirty = self.is_dirty();
        if dirty != self.reported_dirty {
            self.reported_dirty = dirty;
            cx.emit(EditorEvent::StateChanged);
        }
    }

    // A transaction that knows what it changed: the tree follows the splices
    // rather than being thrown away. No splices means the transaction was empty
    // -- backspace at the very start of the buffer -- and an empty transaction
    // is not a commit: reparsing and disarming a confirmation over it would be
    // work and a wrong answer.
    fn after_edit(&mut self, splices: Vec<k10s_edit::Splice>, cx: &mut Context<Self>) {
        if splices.is_empty() {
            cx.notify();
            return;
        }
        self.syntax.edit(self.buffer.rope(), &splices);
        self.committed(cx);
    }

    // A whole-text change with no splices to follow -- undo and redo restore a
    // snapshot -- so the tree is rebuilt before the same commit runs.
    fn after_restore(&mut self, cx: &mut Context<Self>) {
        self.syntax.reparse(self.buffer.rope());
        self.committed(cx);
    }

    fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let splices = self.buffer.insert(text);
        self.after_edit(splices, cx);
        let auto = text.chars().count() == 1
            && text.chars().all(|character| {
                character.is_alphanumeric() || matches!(character, '-' | '_' | '.' | '/')
            });
        if auto || (text == " " && self.value_position_ready()) {
            self.trigger_completion(false, cx);
        } else {
            self.completion = None;
        }
    }

    fn value_position_ready(&self) -> bool {
        let head = self.buffer.primary_selection().head;
        let context = self.syntax.context_at(self.buffer.rope(), head);
        context.position == CursorPosition::Value && context.prefix.is_empty()
    }

    fn trigger_completion(&mut self, explicit: bool, cx: &mut Context<Self>) {
        let head = self.buffer.primary_selection().head;
        let context = self.syntax.context_at(self.buffer.rope(), head);
        let meta = doc_meta(self.buffer.rope(), &self.syntax, context.document_index);
        let existing = if context.position == CursorPosition::Key {
            self.syntax
                .mapping_keys_at(self.buffer.rope(), context.document_index, &context.path)
        } else {
            Vec::new()
        };
        let items = {
            let store = self.schema.borrow();
            match &self.schema_root {
                Some(root) => complete_with_root(&store.index, root, &context, &existing),
                None => complete(&store.index, &meta, &context, &existing),
            }
        };
        if items.is_empty() {
            self.completion = None;
            if explicit {
                self.status = Some("no completions here".to_string());
            }
        } else {
            self.status = None;
            self.completion = Some(CompletionMenu {
                items,
                selected: 0,
                anchor: head.saturating_sub(context.prefix.len()),
            });
        }
        cx.notify();
    }

    fn accept_completion(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.completion.take() else {
            return false;
        };
        let Some(item) = menu.items.get(menu.selected) else {
            return false;
        };
        let head = self.buffer.primary_selection().head;
        let context = self.syntax.context_at(self.buffer.rope(), head);
        // The language owns the whole insertion -- range, delimiters, escaping,
        // indentation, and where the caret ends up -- so the view only applies
        // it. Accepting collapses to one cursor: the menu described one place.
        let edit = completion_edit(self.buffer.rope(), &context, item, head);
        let caret = edit.range.start + edit.caret;
        let splices = self.buffer.edit(
            vec![(edit.range, edit.text)],
            EditGroup::Other,
            SelectionIntent::Collapse,
        );
        self.buffer.set_selections(vec![Selection::caret(caret)], 0);
        self.after_edit(splices, cx);
        self.completion = None;
        if edit.reopen {
            self.trigger_completion(false, cx);
        }
        true
    }

    fn move_or_navigate(&mut self, motion: Motion, extend: bool, cx: &mut Context<Self>) {
        if !extend && let Some(menu) = &mut self.completion {
            match motion {
                Motion::Up => {
                    menu.selected = menu.selected.saturating_sub(1);
                    cx.notify();
                    return;
                }
                Motion::Down => {
                    menu.selected = (menu.selected + 1).min(menu.items.len() - 1);
                    cx.notify();
                    return;
                }
                _ => self.completion = None,
            }
        } else if self.completion.is_some() {
            self.completion = None;
        }
        self.buffer.move_cursors(motion, extend);
        self.ensure_visible();
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.completion.is_some() {
            self.completion = None;
        } else if self.search.is_some() {
            self.search = None;
        } else if !self.buffer.collapse_to_primary() {
            cx.propagate();
            return;
        }
        cx.notify();
    }

    fn searching(&self) -> bool {
        self.search.is_some()
    }

    // A load replaces the buffer and its versions restart, so a search bar left
    // open is holding the previous document's match ranges and would not notice:
    // `refresh` short-circuits on a version it has already seen.
    fn resync_search(&mut self) {
        if let Some(search) = &mut self.search {
            search.recompile();
            search.state.refresh(&self.buffer);
        }
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.recompile();
            search.state.refresh(&self.buffer);
            search
                .state
                .jump_from(self.buffer.primary_selection().start());
        }
        cx.notify();
    }

    fn jump_to_match(&mut self, cx: &mut Context<Self>) {
        let mut moved = false;
        if let Some(search) = &mut self.search {
            search.state.refresh(&self.buffer);
            if search.state.current().is_some() {
                search.state.select_current(&mut self.buffer);
                moved = true;
            }
        }
        if moved {
            self.ensure_visible();
        }
        cx.notify();
    }

    fn ensure_visible(&mut self) {
        let head = self.buffer.primary_selection().head;
        let row = self.buffer.rope().byte_to_point(head).row;
        let last = self.scroll_top + self.rows.saturating_sub(1);
        if row < self.scroll_top {
            self.scroll_top = row;
        } else if row > last {
            self.scroll_top = row + 1 - self.rows;
        }
        let max_top = self.buffer.rope().len_lines().saturating_sub(1);
        self.scroll_top = self.scroll_top.min(max_top);
    }

    fn scroll_by(&mut self, delta: i64) {
        let max_top = self
            .buffer
            .rope()
            .len_lines()
            .saturating_sub(self.rows.min(4));
        self.scroll_top = self
            .scroll_top
            .saturating_add_signed(delta as isize)
            .min(max_top);
    }

    fn resize(&mut self, width: f32, height: f32, origin: (f32, f32), cx: &mut Context<Self>) {
        self.origin = origin;
        if !self.viewport.update(width, height) {
            return;
        }
        let rows = self.viewport.rows(
            STATUS_BAR_HEIGHT,
            CONTENT_PADDING * 2.0,
            k10s_theme::typography(cx).line_height(),
            400,
        );
        self.rows = rows.max(4);
        cx.notify();
    }

    fn offset_for_mouse(
        &self,
        fonts: &Typography,
        window: &mut Window,
        position: gpui::Point<gpui::Pixels>,
    ) -> usize {
        let rope = self.buffer.rope();
        let y = f32::from(position.y) - self.origin.1 - CONTENT_PADDING;
        let row = if y < 0.0 {
            self.scroll_top
        } else {
            (self.scroll_top + (y / fonts.line_height()) as usize)
                .min(rope.len_lines().saturating_sub(1))
        };
        let gutter = gutter_width(rope.len_lines(), fonts, window);
        let x = f32::from(position.x) - self.origin.0 - CONTENT_PADDING - gutter;
        let line = rope.line(row);
        if x <= 0.0 || line.is_empty() {
            let start = rope.line_start(row);
            return if x <= 0.0 { start } else { start + line.len() };
        }
        let shaped = shape_plain(&line, fonts, window);
        let index = shaped.closest_index_for_x(px(x));
        rope.line_start(row) + index.min(line.len())
    }

    fn toggle_comment(&mut self, cx: &mut Context<Self>) {
        let rows = self.buffer.cursor_rows();
        let rope = self.buffer.rope();
        let all_commented = rows.iter().all(|row| {
            let line = rope.line(*row);
            line.trim().is_empty() || line.trim_start().starts_with('#')
        });
        let mut edits = Vec::new();
        for row in rows {
            let line = rope.line(row);
            let start = rope.line_start(row);
            let indent = line.len() - line.trim_start().len();
            if all_commented {
                let body = line.trim_start();
                if let Some(rest) = body.strip_prefix('#') {
                    let strip = 1 + if rest.starts_with(' ') { 1 } else { 0 };
                    edits.push((start + indent..start + indent + strip, String::new()));
                }
            } else if !line.trim().is_empty() {
                edits.push((start + indent..start + indent, "# ".to_string()));
            }
        }
        if !edits.is_empty() {
            let splices = self
                .buffer
                .edit(edits, EditGroup::Other, SelectionIntent::Preserve);
            self.after_edit(splices, cx);
        }
    }

    fn status_line(&self) -> String {
        let head = self.buffer.primary_selection().head;
        let point = self.buffer.rope().byte_to_point(head);
        let mut pieces = vec![format!("ln {}, col {}", point.row + 1, point.column + 1)];
        if self.buffer.selections().len() > 1 {
            pieces.push(format!("{} cursors", self.buffer.selections().len()));
        }
        if self.is_dirty() {
            pieces.push(match &self.source {
                EditorSource::Cluster(_) => "edited locally; ctrl-s reviews the apply".to_string(),
                EditorSource::File(_) => "unsaved changes".to_string(),
                EditorSource::Scratch => "unsaved; ctrl-s picks a file".to_string(),
            });
        }
        if let Some(search) = &self.search {
            let mode = if search.regex { "regex " } else { "" };
            let field = if search.typing_replace {
                format!(
                    "replace with {}_",
                    search.replace.clone().unwrap_or_default()
                )
            } else {
                format!("{mode}/{}_", search.input)
            };
            pieces.push(field);
            if let Some(error) = search.state.error() {
                pieces.push(format!("invalid pattern: {error}"));
            } else if search.state.matches().is_empty() && !search.input.is_empty() {
                pieces.push("no matches".to_string());
            } else if !search.input.is_empty() {
                pieces.push(format!(
                    "{}/{}",
                    search.state.current_index() + 1,
                    search.state.matches().len()
                ));
            }
        }
        let errors = self
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
            .count();
        let warnings = self.diagnostics.len() - errors;
        if errors > 0 {
            pieces.push(format!("{errors} errors"));
        }
        if warnings > 0 {
            pieces.push(format!("{warnings} warnings"));
        }
        {
            let store = self.schema.borrow();
            if let (Some(api_version), Some(kind)) = (&self.meta.api_version, &self.meta.kind) {
                if store.index.resolve_gvk(api_version, kind).is_some() {
                    pieces.push(format!("schema: {api_version} {kind}"));
                } else if let Some(note) = store.first_note() {
                    pieces.push(note.to_string());
                }
            }
        }
        if let Some(status) = &self.status {
            pieces.push(status.clone());
        }
        pieces.join("  ·  ")
    }
}

// Measured with the *active* buffer face and size, not a default: the gutter
// width and every x-to-offset hit test come from here, so measuring with a
// different font than the one painted put the caret in the wrong column as
// soon as the buffer font became a setting.
fn shape_plain(text: &str, fonts: &Typography, window: &mut Window) -> gpui::ShapedLine {
    let run = gpui::TextRun {
        len: text.len(),
        font: gpui::font(fonts.buffer_family.clone()),
        color: gpui::Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(text.to_string().into(), px(fonts.buffer_size), &[run], None)
}

fn file_title(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn gutter_width(len_lines: usize, fonts: &Typography, window: &mut Window) -> f32 {
    let digits = len_lines.max(1).ilog10() as usize + 1;
    let sample = "0".repeat(digits.max(2));
    f32::from(shape_plain(&sample, fonts, window).width()) + CONTENT_PADDING * 2.0
}

// ---------------------------------------------------------------------------
// Pure run composition: every decoration a visible line carries, resolved to
// disjoint spans with one priority order, testable without a window.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SpanFlags {
    pub token: Option<TokenKind>,
    pub selected: bool,
    pub caret: bool,
    pub matched: bool,
    pub current_match: bool,
    pub diagnostic: Option<DiagnosticSeverity>,
}

impl SpanFlags {
    fn any(&self) -> bool {
        self.token.is_some()
            || self.selected
            || self.caret
            || self.matched
            || self.current_match
            || self.diagnostic.is_some()
    }
}

#[derive(Debug, Default)]
pub struct LineLayers {
    pub tokens: Vec<(Range<usize>, TokenKind)>,
    pub selections: Vec<Range<usize>>,
    pub carets: Vec<Range<usize>>,
    pub matches: Vec<Range<usize>>,
    pub current_match: Option<Range<usize>>,
    pub diagnostics: Vec<(Range<usize>, DiagnosticSeverity)>,
}

pub fn compose_line(len: usize, layers: &LineLayers) -> Vec<(Range<usize>, SpanFlags)> {
    let mut boundaries = vec![0, len];
    let mut collect = |range: &Range<usize>| {
        boundaries.push(range.start.min(len));
        boundaries.push(range.end.min(len));
    };
    for (range, _) in &layers.tokens {
        collect(range);
    }
    for range in &layers.selections {
        collect(range);
    }
    for range in &layers.carets {
        collect(range);
    }
    for range in &layers.matches {
        collect(range);
    }
    if let Some(range) = &layers.current_match {
        collect(range);
    }
    for (range, _) in &layers.diagnostics {
        collect(range);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut spans = Vec::new();
    for pair in boundaries.windows(2) {
        let segment = pair[0]..pair[1];
        if segment.is_empty() {
            continue;
        }
        let covers =
            |range: &Range<usize>| range.start <= segment.start && range.end >= segment.end;
        let flags = SpanFlags {
            token: layers
                .tokens
                .iter()
                .rev()
                .find(|(range, _)| covers(range))
                .map(|(_, token)| *token),
            selected: layers.selections.iter().any(covers),
            caret: layers.carets.iter().any(covers),
            matched: layers.matches.iter().any(covers),
            current_match: layers.current_match.as_ref().is_some_and(covers),
            diagnostic: layers
                .diagnostics
                .iter()
                .find(|(range, _)| covers(range))
                .map(|(_, severity)| *severity),
        };
        if flags.any() {
            spans.push((segment, flags));
        }
    }
    spans
}

fn token_color(theme: &Theme, token: TokenKind) -> u32 {
    let syntax = &theme.syntax;
    match token {
        TokenKind::Property => syntax.property,
        TokenKind::Str => syntax.string,
        TokenKind::Number => syntax.number,
        TokenKind::Boolean => syntax.boolean,
        TokenKind::Constant => syntax.constant,
        TokenKind::Comment => syntax.comment,
        TokenKind::Anchor => syntax.label,
        TokenKind::Tag => syntax.type_name,
        TokenKind::Directive => syntax.attribute,
        TokenKind::Punctuation => syntax.punctuation,
        TokenKind::PunctuationSpecial => syntax.punctuation_special,
    }
}

fn flag_style(theme: &Theme, flags: SpanFlags) -> HighlightStyle {
    let mut style = HighlightStyle::default();
    if let Some(token) = flags.token {
        style.color = Some(rgb(token_color(theme, token)).into());
    }
    if flags.matched {
        let (color, alpha) = theme.shell.search_match_background;
        style.background_color = Some(rgb(color).alpha(alpha * 0.6).into());
    }
    if flags.selected {
        let (color, alpha) = theme.syntax.selection_background;
        style.background_color = Some(rgb(color).alpha(alpha).into());
    }
    if flags.current_match {
        let (color, alpha) = theme.shell.search_match_background;
        style.background_color = Some(rgb(color).alpha(alpha).into());
    }
    if let Some(severity) = flags.diagnostic {
        let color = match severity {
            DiagnosticSeverity::Error => theme.shell.error,
            DiagnosticSeverity::Warning => theme.shell.warning,
        };
        style.underline = Some(UnderlineStyle {
            thickness: px(1.0),
            color: Some(rgb(color).into()),
            wavy: true,
        });
    }
    if flags.caret {
        style.background_color = Some(rgb(theme.shell.cursor).into());
        style.color = Some(rgb(theme.shell.editor_background).into());
    }
    style
}

// Every decoration the visible rows carry, resolved in one pass. The shipped
// render path is what a frame budget applies to, so it asks the tree once for
// the whole viewport instead of once per row, and walks the sorted disjoint
// decorations forward with the rows instead of rescanning every one of them per
// row -- at a megabyte the search set alone is thousands of ranges. Diagnostics
// are capped at a couple of hundred by the validator, so those are scanned
// whole and the bound is the reason.
pub fn viewport_layers(
    rope: &Rope,
    tokens: &[(Range<usize>, TokenKind)],
    selections: &[Selection],
    matches: &[Range<usize>],
    current_match: Option<&Range<usize>>,
    diagnostics: &[Diagnostic],
    rows: Range<usize>,
) -> Vec<LineLayers> {
    let mut all = Vec::with_capacity(rows.end.saturating_sub(rows.start));
    let mut token_at = 0usize;
    let mut match_at = 0usize;
    let mut selection_at = 0usize;
    for row in rows {
        let start = rope.line_start(row);
        let end = start + rope.line_len(row);
        // A caret sitting past the last glyph paints on the padding column, so
        // the clip reaches one byte beyond the line.
        let clip = |range: &Range<usize>| -> Option<Range<usize>> {
            if range.end <= start || range.start > end {
                return None;
            }
            Some(range.start.max(start) - start..range.end.min(end + 1) - start)
        };
        let mut layers = LineLayers::default();

        while token_at < tokens.len() && tokens[token_at].0.end <= start {
            token_at += 1;
        }
        for (range, token) in &tokens[token_at..] {
            if range.start > end {
                break;
            }
            if let Some(local) = clip(range) {
                layers.tokens.push((local, *token));
            }
        }

        while selection_at < selections.len() && selections[selection_at].end() < start {
            selection_at += 1;
        }
        for selection in &selections[selection_at..] {
            if selection.start() > end {
                break;
            }
            if !selection.is_caret()
                && let Some(local) = clip(&(selection.start()..selection.end()))
            {
                layers.selections.push(local);
            }
            let head = selection.head;
            if head >= start && head <= end {
                let caret_end = if head < end {
                    // A CRLF is one cluster but the line keeps its CR, so the
                    // step can land on the next row; the caret stays on this one.
                    (rope.next_grapheme_offset(head) - start).min(end - start + 1)
                } else {
                    head - start + 1
                };
                layers.carets.push(head - start..caret_end);
            }
        }

        while match_at < matches.len() && matches[match_at].end <= start {
            match_at += 1;
        }
        for range in &matches[match_at..] {
            if range.start > end {
                break;
            }
            if let Some(local) = clip(range) {
                if current_match == Some(range) {
                    layers.current_match = Some(local);
                } else {
                    layers.matches.push(local);
                }
            }
        }

        for diagnostic in diagnostics {
            if let Some(local) = clip(&diagnostic.range) {
                layers.diagnostics.push((local, diagnostic.severity));
            }
        }
        all.push(layers);
    }
    all
}

impl EditorView {
    fn viewport_layers(&self, rows: Range<usize>) -> Vec<LineLayers> {
        let rope = self.buffer.rope();
        if rows.start >= rows.end || rows.start >= rope.len_lines() {
            return Vec::new();
        }
        let first = rope.line_start(rows.start);
        let last = rows.end.saturating_sub(1).max(rows.start);
        let bytes = first..rope.line_start(last) + rope.line_len(last);
        let tokens = self.syntax.highlights(rope, bytes);
        let (matches, current) = match &self.search {
            Some(search) => (search.state.matches(), search.state.current()),
            None => (&[][..], None),
        };
        viewport_layers(
            rope,
            &tokens,
            self.buffer.selections(),
            matches,
            current.as_ref(),
            &self.diagnostics,
            rows,
        )
    }
}

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
                if search.typing_replace {
                    if let Some(replace) = &mut search.replace {
                        replace.pop();
                    }
                    cx.notify();
                } else {
                    search.input.pop();
                    this.search_changed(cx);
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
                        if search.replace.is_some() {
                            search.typing_replace = !search.typing_replace;
                            cx.notify();
                        }
                        return;
                    }
                    if search.typing_replace {
                        if let Some(replace) = &mut search.replace {
                            replace.push_str(&key_char);
                        }
                        cx.notify();
                    } else {
                        search.input.push_str(&key_char);
                        this.search_changed(cx);
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

impl crate::item::Item for EditorView {
    fn title(&self) -> SharedString {
        EditorView::title(self)
    }

    fn focus_handle(&self) -> FocusHandle {
        EditorView::focus_handle(self)
    }

    fn is_dirty(&self) -> bool {
        EditorView::is_dirty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers() -> LineLayers {
        LineLayers::default()
    }

    #[test]
    fn compose_splits_a_token_around_a_caret() {
        let mut line = layers();
        line.tokens.push((0..10, TokenKind::Property));
        line.carets.push(4..5);
        let spans = compose_line(11, &line);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].0, 0..4);
        assert!(spans[0].1.token.is_some() && !spans[0].1.caret);
        assert_eq!(spans[1].0, 4..5);
        assert!(spans[1].1.caret && spans[1].1.token.is_some());
        assert_eq!(spans[2].0, 5..10);
        assert!(!spans[2].1.caret);
    }

    #[test]
    fn compose_layers_selection_over_tokens_without_overlap() {
        let mut line = layers();
        line.tokens.push((0..4, TokenKind::Property));
        line.tokens.push((6..11, TokenKind::Str));
        line.selections.push(2..8);
        let spans = compose_line(12, &line);
        for pair in spans.windows(2) {
            assert!(pair[0].0.end <= pair[1].0.start, "{spans:?}");
        }
        let selected: Vec<_> = spans.iter().filter(|(_, flags)| flags.selected).collect();
        assert_eq!(selected.first().expect("selection renders").0.start, 2);
        assert_eq!(selected.last().expect("selection renders").0.end, 8);
    }

    #[test]
    fn compose_keeps_diagnostics_and_matches_together() {
        let mut line = layers();
        line.matches.push(0..4);
        line.current_match = Some(5..9);
        line.diagnostics.push((2..9, DiagnosticSeverity::Warning));
        let spans = compose_line(10, &line);
        let both = spans
            .iter()
            .find(|(range, _)| range.start == 2)
            .expect("overlap segment exists");
        assert!(both.1.matched && both.1.diagnostic.is_some());
        let current = spans
            .iter()
            .find(|(range, _)| range.start == 5)
            .expect("current match segment exists");
        assert!(current.1.current_match);
    }

    #[test]
    fn an_unflagged_segment_is_omitted_for_the_base_style() {
        let mut line = layers();
        line.tokens.push((5..8, TokenKind::Number));
        let spans = compose_line(20, &line);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, 5..8);
    }

    #[test]
    fn a_saved_buffer_is_clean_until_it_is_edited_again() {
        let mut dirty = DirtyState::default();
        assert!(dirty.is_dirty(0), "a buffer with no clean point is dirty");
        dirty.mark_clean(7, Some(100));
        assert!(!dirty.is_dirty(7));
        assert!(dirty.is_dirty(8), "one more edit and it is dirty again");
        // Cleanliness here is a version identity, not a comparison of bytes,
        // and `Buffer::restore` counts an undo as a revision of its own. So a
        // buffer undone back to the saved *text* arrives at a version the clean
        // point never saw and keeps its dot: the tab over-reports rather than
        // under-reports, which is the direction that cannot lose work.
        assert!(
            dirty.is_dirty(9),
            "an undo is a new version, not a return to an old one"
        );
        assert!(
            !dirty.is_dirty(7),
            "only the clean version itself reads clean"
        );
    }

    // The counter a review used to be keyed by restarts at zero whenever a
    // buffer is replaced, so the same number of keystrokes after a reload lands
    // on the number the review was made at -- and the review authorised the
    // apply of a document that no longer existed.
    #[test]
    fn a_replaced_buffer_is_a_different_document_at_the_same_version() {
        assert_ne!(BufferStamp::of(1, 3), BufferStamp::of(2, 3));
        assert_eq!(BufferStamp::of(2, 3), BufferStamp::of(2, 3));
        assert_ne!(BufferStamp::of(2, 3), BufferStamp::of(2, 4));
    }

    #[test]
    fn an_external_change_costs_a_second_press_before_it_is_overwritten() {
        let mut dirty = DirtyState::default();
        dirty.mark_clean(1, Some(100));
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk),
            "the disk moved under us"
        );
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Write,
            "the second press is the confirmation"
        );
        dirty.mark_clean(2, Some(200));
        assert_eq!(dirty.save_step(Some(200)), SaveStep::Write, "in step again");
    }

    #[test]
    fn typing_disarms_a_pending_overwrite_or_close_confirmation() {
        let mut dirty = DirtyState::default();
        dirty.mark_clean(1, Some(100));
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk)
        );
        dirty.edited();
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk),
            "an edit after the warning re-asks rather than silently overwriting"
        );

        let mut dirty = DirtyState::default();
        assert_eq!(dirty.close_step(5), CloseStep::Warn);
        dirty.edited();
        assert_eq!(
            dirty.close_step(5),
            CloseStep::Warn,
            "typing after the warning re-arms the guard"
        );
        assert_eq!(dirty.close_step(5), CloseStep::Close);
    }

    #[test]
    fn a_never_written_or_deleted_file_saves_without_a_prompt() {
        let mut dirty = DirtyState::default();
        assert_eq!(
            dirty.save_step(None),
            SaveStep::Write,
            "a new file has nothing to conflict with"
        );
        dirty.mark_clean(1, Some(100));
        assert_eq!(
            dirty.save_step(None),
            SaveStep::Write,
            "a file deleted underneath is recreated, not queried"
        );
    }

    #[test]
    fn a_clean_buffer_closes_on_the_first_press() {
        let mut dirty = DirtyState::default();
        dirty.mark_clean(3, Some(1));
        assert_eq!(dirty.close_step(3), CloseStep::Close);
    }

    #[test]
    fn adopting_a_path_forgets_the_previous_files_stamp() {
        let mut dirty = DirtyState::default();
        dirty.mark_clean(1, Some(100));
        dirty.forget_disk();
        assert_eq!(
            dirty.save_step(None),
            SaveStep::Write,
            "save-as onto a new file must not inherit the old file's conflict"
        );
    }

    #[test]
    fn saving_onto_a_file_this_buffer_never_read_asks_first() {
        let mut dirty = DirtyState::default();
        assert_eq!(
            dirty.save_step(Some(42)),
            SaveStep::Confirm(Overwrite::AlreadyExists),
            "save-as onto somebody else's file must not be silent"
        );
        assert_eq!(dirty.save_step(Some(42)), SaveStep::Write);
    }

    #[test]
    fn reloading_over_unsaved_work_asks_the_same_way_closing_does() {
        let mut dirty = DirtyState::default();
        assert_eq!(dirty.reload_step(3), CloseStep::Warn);
        assert_eq!(dirty.reload_step(3), CloseStep::Close);
        dirty.mark_clean(4, Some(1));
        assert_eq!(
            dirty.reload_step(4),
            CloseStep::Close,
            "a clean buffer reloads without a question"
        );
    }

    #[test]
    fn a_buffer_that_has_never_been_loaded_is_dirty_which_is_why_opening_cannot_ask() {
        // A fresh view has no clean point, so it is dirty by definition. The
        // constructors therefore have to load unguarded: routing them through
        // the reload action left every file open, empty, behind a warning about
        // unsaved changes it did not have.
        let mut dirty = DirtyState::default();
        assert!(dirty.is_dirty(0), "no clean point yet");
        assert_eq!(
            dirty.reload_step(0),
            CloseStep::Warn,
            "which is exactly what the guard would have answered on open"
        );
    }

    #[test]
    fn reloading_and_closing_do_not_answer_each_others_questions() {
        // One shared bit could not tell them apart, so the warning about a
        // reload was answered by the next close and the buffer went away on a
        // single press.
        let mut dirty = DirtyState::default();
        assert_eq!(dirty.reload_step(3), CloseStep::Warn);
        assert_eq!(
            dirty.close_step(3),
            CloseStep::Warn,
            "a close does not inherit the reload's armed confirmation"
        );
        assert_eq!(dirty.close_step(3), CloseStep::Close);

        let mut dirty = DirtyState::default();
        dirty.mark_clean(1, Some(100));
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk)
        );
        assert_eq!(
            dirty.close_step(2),
            CloseStep::Warn,
            "and neither does a close inherit an armed overwrite"
        );
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk),
            "the close re-armed, so the overwrite asks again"
        );
    }

    fn save(version: u64, text: &str) -> PendingSave {
        PendingSave {
            path: PathBuf::from("/work/a.yaml"),
            text: text.to_string(),
            version,
            generation: 1,
            ticket: 0,
        }
    }

    fn same_save(left: &PendingSave, right: &PendingSave) -> bool {
        (&left.path, &left.text, left.version) == (&right.path, &right.text, right.version)
    }

    #[test]
    fn an_abandoned_write_cannot_hand_the_queue_to_a_second_one() {
        // The write a reload abandons is still running: it cannot be recalled.
        // If it were still allowed to advance the queue when it finishes, it
        // would start a save beside the one already in flight -- two writes
        // racing, which is what the queue exists to prevent.
        let mut queue = SaveQueue::default();
        let first = queue.request(save(1, "one")).expect("starts");
        queue.abandon();
        let second = queue
            .request(save(2, "two"))
            .expect("starts, nothing owns the flight");
        assert_ne!(first.ticket, second.ticket);
        assert!(
            queue.finished(first.ticket).is_none(),
            "the abandoned write hands on nothing"
        );
        assert!(
            queue.request(save(3, "three")).is_none(),
            "and the flight is still owned, so a third press queues rather than racing"
        );
        let next = queue
            .finished(second.ticket)
            .expect("the owner hands on the queued text");
        assert!(same_save(&next, &save(3, "three")));
    }

    #[test]
    fn saves_of_one_buffer_run_one_at_a_time_and_the_last_text_wins() {
        // Spawning a write per press left the order to the executor: an older
        // write could rename last while the newest version was marked clean.
        let mut queue = SaveQueue::default();
        let first = queue
            .request(save(1, "one"))
            .expect("the first press starts");
        assert!(same_save(&first, &save(1, "one")));
        assert!(
            queue.request(save(2, "two")).is_none(),
            "the second waits for it rather than racing it"
        );
        assert!(
            queue.request(save(3, "three")).is_none(),
            "and so does the third"
        );
        let next = queue
            .finished(first.ticket)
            .expect("the queued text follows");
        assert!(
            same_save(&next, &save(3, "three")),
            "only the newest text is worth writing"
        );
        assert!(
            queue.finished(next.ticket).is_none(),
            "then the queue is empty"
        );
        let later = queue
            .request(save(4, "four"))
            .expect("a later press starts");
        assert!(same_save(&later, &save(4, "four")));
    }

    #[test]
    fn a_conflict_empties_the_queue_so_the_overwrite_is_a_deliberate_press() {
        let mut queue = SaveQueue::default();
        let first = queue.request(save(1, "one")).expect("starts");
        queue.request(save(2, "two"));
        queue.abandon();
        assert!(
            queue.finished(first.ticket).is_none(),
            "nothing follows a save that asked a question instead of writing"
        );
        let resumed = queue
            .request(save(3, "three"))
            .expect("a deliberate press starts");
        assert!(same_save(&resumed, &save(3, "three")));
    }

    #[test]
    fn the_viewport_only_looks_at_the_rows_it_paints() {
        let rope = Rope::from(
            "alpha
beta
gamma
delta
",
        );
        let tokens = [(0..5, TokenKind::Property), (11..16, TokenKind::Str)];
        let matches = [0..2, 6..8, 11..13, 17..19];
        let selections = [Selection::caret(12)];
        let diagnostics = [Diagnostic {
            range: 17..22,
            severity: DiagnosticSeverity::Error,
            message: String::new(),
        }];
        let rows = viewport_layers(
            &rope,
            &tokens,
            &selections,
            &matches,
            Some(&(11..13)),
            &diagnostics,
            1..3,
        );
        assert_eq!(rows.len(), 2, "one layer set per painted row");
        assert!(
            rows[0].tokens.is_empty() && rows[0].current_match.is_none(),
            "row one carries only its own match: {:?}",
            rows[0]
        );
        assert_eq!(
            rows[0].matches.first(),
            Some(&(0..2)),
            "beta's match, in line coordinates"
        );
        assert_eq!(rows[0].matches.len(), 1, "and only that one");
        assert_eq!(rows[1].tokens, [(0..5, TokenKind::Str)]);
        assert_eq!(
            rows[1].current_match,
            Some(0..2),
            "the current match is the one the search is on"
        );
        assert_eq!(
            rows[1].carets.first(),
            Some(&(1..2)),
            "and the caret is local too"
        );
        assert!(
            rows[0].diagnostics.is_empty() && rows[1].diagnostics.is_empty(),
            "the diagnostic is on a row nobody painted"
        );
    }

    #[test]
    fn a_span_reaching_across_rows_is_clipped_into_each_of_them() {
        let rope = Rope::from(
            "one
two
three
",
        );
        let tokens = [(0..13, TokenKind::Comment)];
        let rows = viewport_layers(&rope, &tokens, &[], &[], None, &[], 0..3);
        assert_eq!(rows[0].tokens, [(0..4, TokenKind::Comment)]);
        assert_eq!(rows[1].tokens, [(0..4, TokenKind::Comment)]);
        assert_eq!(rows[2].tokens, [(0..5, TokenKind::Comment)]);
    }

    #[test]
    fn the_schema_store_notes_deduplicate() {
        let mut store = SchemaStore::new();
        store.note("schema catalog: access denied for this account".to_string());
        store.note("schema catalog: access denied for this account".to_string());
        assert_eq!(store.notes.len(), 1);
        assert!(store.first_note().expect("noted").contains("denied"));
    }
}
