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
//! Schemas arrive lazily through a per-connection [`SchemaStore`] -- catalog
//! once, CRDs once, one document per group-version on demand -- and every
//! fetch outcome that is not `Ok` becomes a labelled note, never a silent
//! absence. The store is retired when the workspace adopts another cluster,
//! because a schema describes the server it came from and nothing else.
//! Typing reaches the buffer through `key_char` exactly like the terminal;
//! named keys and chords arrive as `Editor`-context actions.
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

use gpui::{Context, FocusHandle, SharedString, WeakEntity, Window, px};

use k10s_edit::complete::{
    Completion, DiagnosticSeverity, complete, complete_with_root, doc_meta, validate,
    validate_with_root,
};
use k10s_edit::schema::SchemaNode;
use k10s_edit::{
    Buffer, CursorPosition, Diagnostic, DocMeta, EditGroup, LanguageKind, Motion, Rope,
    SchemaIndex, SearchState, Selection, SelectionIntent, Syntax, completion_edit,
};

use k10s_theme::Typography;

use crate::dirty::DirtyState;
use crate::fs::Fs;
use crate::provider::{DescribeRequest, ReadProvider, SchemaSource};
use crate::saves::SaveQueue;
use crate::ui::{CONTENT_PADDING, STATUS_BAR_HEIGHT, Viewport};

pub use crate::spans::{LineLayers, SpanFlags, compose_line, viewport_layers};

pub(crate) const MAX_VISIBLE_COMPLETIONS: usize = 8;
pub(crate) const COMPLETION_WIDTH: f32 = 460.0;
pub(crate) const MAX_DOC_CHARS: usize = 280;

// One schema index per connection: catalog and CRDs fetched once, one OpenAPI
// document per group-version fetched the first time an editor needs it, and
// every non-Ok outcome a labelled note. Shared by `Rc<RefCell>` because the
// provider handle itself never leaves the UI thread, and retired when the
// window adopts another cluster, because none of it describes that one.
pub struct SchemaStore {
    pub index: SchemaIndex,
    pub(crate) catalog: Vec<SchemaSource>,
    pub(crate) requested_catalog: bool,
    pub(crate) requested_crds: bool,
    pub(crate) requested_documents: HashSet<String>,
    pub(crate) notes: Vec<String>,
    epoch: u64,
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
            epoch: 0,
        }
    }

    /// Give up everything one API server said, because the window has left it.
    ///
    /// A catalog, a CRD list and an OpenAPI document describe the shape of one
    /// cluster's objects, and the flags beside them say those were asked for
    /// already. Carried across a connection switch they answer for a cluster
    /// nothing on screen belongs to any more: completions and diagnostics that
    /// look exactly like right ones. The epoch is what the answers still in
    /// flight are checked against, so the previous cluster cannot fill the store
    /// that replaced it.
    pub(crate) fn retire(&mut self) {
        let epoch = self.epoch + 1;
        *self = SchemaStore::new();
        self.epoch = epoch;
    }

    /// Which connection this store is filled from. Taken when a fetch is asked
    /// for and compared when it answers.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    pub(crate) fn first_note(&self) -> Option<&str> {
        self.notes.first().map(String::as_str)
    }
}

pub(crate) struct CompletionMenu {
    pub(crate) items: Vec<Completion>,
    pub(crate) selected: usize,
    pub(crate) anchor: usize,
}

pub(crate) struct SearchBar {
    pub(crate) input: String,
    pub(crate) replace: Option<String>,
    pub(crate) regex: bool,
    pub(crate) state: SearchState,
    pub(crate) typing_replace: bool,
}

impl SearchBar {
    pub(crate) fn new() -> SearchBar {
        SearchBar {
            input: String::new(),
            replace: None,
            regex: false,
            state: SearchState::new("", false),
            typing_replace: false,
        }
    }

    pub(crate) fn recompile(&mut self) {
        self.state = SearchState::new(&self.input, self.regex);
    }

    // The one way into the replace field: tab flips only when a replace
    // exists, so a plain search never types into a field nobody can see.
    // Returns whether it flipped, which is the caller's cue to repaint.
    pub(crate) fn toggle_field(&mut self) -> bool {
        if self.replace.is_none() {
            return false;
        }
        self.typing_replace = !self.typing_replace;
        true
    }

    // Typed text lands in whichever field is being typed into; the return
    // says whether the FIND side changed, i.e. whether a recompile is owed.
    pub(crate) fn push(&mut self, text: &str) -> bool {
        if self.typing_replace {
            if let Some(replace) = &mut self.replace {
                replace.push_str(text);
            }
            false
        } else {
            self.input.push_str(text);
            true
        }
    }

    pub(crate) fn pop(&mut self) -> bool {
        if self.typing_replace {
            if let Some(replace) = &mut self.replace {
                replace.pop();
            }
            false
        } else {
            self.input.pop();
            true
        }
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
    pub(crate) focus: FocusHandle,
    // This view's own handle, so `diff_sources` can name the entity a
    // comparison belongs to. A diff is opened by the workspace, which holds the
    // strong handle; the sources it is built from have to carry the identity or
    // a reused diff tab keeps pointing at the editor it was first opened from.
    pub(crate) handle: WeakEntity<EditorView>,
    pub(crate) provider: Rc<dyn ReadProvider>,
    pub(crate) fs: Arc<dyn Fs>,
    pub(crate) source: EditorSource,
    pub(crate) title: SharedString,
    pub(crate) buffer: Buffer,
    pub(crate) syntax: Syntax,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) meta: DocMeta,
    pub(crate) schema: Rc<RefCell<SchemaStore>>,
    pub(crate) schema_root: Option<Arc<SchemaNode>>,
    // The cluster's own text for this object, as fetched. The buffer diverges
    // from it the moment anything is typed, and the diff needs both.
    pub(crate) live: Option<String>,
    // The `last-applied-configuration`, rendered: a three-way diff's base.
    pub(crate) base: Option<String>,
    // Which object `live` came from. Replaced by every read, which is what keeps
    // it from ageing into a claim about an object that has since been recreated.
    pub(crate) uid: Option<String>,
    pub(crate) patchable: bool,
    pub(crate) status_subresource: bool,
    pub(crate) completion: Option<CompletionMenu>,
    pub(crate) search: Option<SearchBar>,
    pub(crate) scroll_top: usize,
    pub(crate) status: Option<String>,
    pub(crate) dirty: DirtyState,
    // The dirtiness the workspace has been told about, so a keystroke that does
    // not change it costs nothing outside this view.
    pub(crate) reported_dirty: bool,
    pub(crate) saves: SaveQueue,
    // What a file that does not exist yet opens with: the settings and keymap
    // templates. Held rather than checked up front, so opening costs no stat on
    // the UI thread.
    pub(crate) template: Option<&'static str>,
    pub(crate) generation: u64,
    /// The settle timer a large buffer validates behind, held rather than
    /// detached: a keystroke replaces it, and replacing a task is what cancels
    /// the one before it. Detaching each one left a task per keypress waiting
    /// to do a whole-document walk the version gate would then throw away.
    pub(crate) validation: Option<gpui::Task<()>>,
    pub(crate) viewport: Viewport,
    pub(crate) rows: usize,
    pub(crate) origin: (f32, f32),
    pub(crate) dragging: bool,
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
            validation: None,
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

    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty.is_dirty(self.buffer.version())
    }

    pub(crate) fn revalidate(&mut self) {
        let store = self.schema.borrow();
        self.diagnostics = match &self.schema_root {
            Some(root) => validate_with_root(&store.index, self.buffer.rope(), &self.syntax, root),
            None => validate(&store.index, self.buffer.rope(), &self.syntax),
        };
    }

    // Small buffers validate on the keystroke; large ones validate after the
    // typing pauses, because a whole-document schema walk over a megabyte is
    // measured in milliseconds and does not belong between two characters.
    pub(crate) fn schedule_validation(&mut self, cx: &mut Context<Self>) {
        const SYNCHRONOUS_LIMIT: usize = 64 << 10;
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(150);
        if self.buffer.rope().len() <= SYNCHRONOUS_LIMIT {
            self.validation = None;
            self.revalidate();
            return;
        }
        let version = self.buffer.version();
        self.validation = Some(cx.spawn(async move |this, cx| {
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
        }));
    }

    // Every mutation ends here, whatever made it. An edit that skipped this
    // left an armed overwrite confirmation alive across a real change, left the
    // document metadata and the diagnostics describing the previous text, and
    // never told the workspace its tab was dirty.
    pub(crate) fn committed(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn publish_state(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn after_edit(&mut self, splices: Vec<k10s_edit::Splice>, cx: &mut Context<Self>) {
        if splices.is_empty() {
            cx.notify();
            return;
        }
        self.syntax.edit(self.buffer.rope(), &splices);
        self.committed(cx);
    }

    // A whole-text change with no splices to follow -- undo and redo restore a
    // snapshot -- so the tree is rebuilt before the same commit runs.
    pub(crate) fn after_restore(&mut self, cx: &mut Context<Self>) {
        self.syntax.reparse(self.buffer.rope());
        self.committed(cx);
    }

    pub(crate) fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
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

    pub(crate) fn value_position_ready(&self) -> bool {
        let head = self.buffer.primary_selection().head;
        let context = self.syntax.context_at(self.buffer.rope(), head);
        context.position == CursorPosition::Value && context.prefix.is_empty()
    }

    pub(crate) fn trigger_completion(&mut self, explicit: bool, cx: &mut Context<Self>) {
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

    pub(crate) fn accept_completion(&mut self, cx: &mut Context<Self>) -> bool {
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

    pub(crate) fn move_or_navigate(
        &mut self,
        motion: Motion,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        if !extend && let Some(menu) = &mut self.completion {
            match motion {
                Motion::Up => {
                    menu.selected = menu.selected.saturating_sub(1);
                    cx.notify();
                    return;
                }
                Motion::Down => {
                    menu.selected = (menu.selected + 1).min(menu.items.len().saturating_sub(1));
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

    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn searching(&self) -> bool {
        self.search.is_some()
    }

    // A load replaces the buffer and its versions restart, so a search bar left
    // open is holding the previous document's match ranges and would not notice:
    // `refresh` short-circuits on a version it has already seen.
    pub(crate) fn resync_search(&mut self) {
        if let Some(search) = &mut self.search {
            search.recompile();
            search.state.refresh(&self.buffer);
        }
    }

    pub(crate) fn search_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.recompile();
            search.state.refresh(&self.buffer);
            search
                .state
                .jump_from(self.buffer.primary_selection().start());
        }
        cx.notify();
    }

    pub(crate) fn jump_to_match(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn ensure_visible(&mut self) {
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

    pub(crate) fn scroll_by(&mut self, delta: i64) {
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

    pub(crate) fn resize(
        &mut self,
        width: f32,
        height: f32,
        origin: (f32, f32),
        cx: &mut Context<Self>,
    ) {
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

    pub(crate) fn offset_for_mouse(
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

    pub(crate) fn toggle_comment(&mut self, cx: &mut Context<Self>) {
        let rows = self.buffer.cursor_rows();
        let edits = comment_edits(self.buffer.rope(), &rows, self.syntax.language());
        if !edits.is_empty() {
            let splices = self
                .buffer
                .edit(edits, EditGroup::Other, SelectionIntent::Preserve);
            self.after_edit(splices, cx);
        }
    }

    pub(crate) fn status_line(&self) -> String {
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
pub(crate) fn shape_plain(text: &str, fonts: &Typography, window: &mut Window) -> gpui::ShapedLine {
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

/// What a line comment starts with in this language. A JSON buffer -- the
/// settings and the keymap are both one -- has no `#` comment: writing one is
/// not a toggle, it is a parse error the next reader inherits.
pub(crate) fn comment_marker(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::Json => "//",
        LanguageKind::Yaml | LanguageKind::Plain => "#",
    }
}

/// The edits a comment toggle makes: uncomment when every touched line is
/// already commented, comment otherwise, blank lines untouched either way.
/// Pure, so the marker a language uses is checked without a window.
pub(crate) fn comment_edits(
    rope: &Rope,
    rows: &[usize],
    language: LanguageKind,
) -> Vec<(Range<usize>, String)> {
    let marker = comment_marker(language);
    let all_commented = rows.iter().all(|row| {
        let line = rope.line(*row);
        line.trim().is_empty() || line.trim_start().starts_with(marker)
    });
    let mut edits = Vec::new();
    for row in rows {
        let line = rope.line(*row);
        let start = rope.line_start(*row);
        let indent = line.len() - line.trim_start().len();
        if all_commented {
            let body = line.trim_start();
            if let Some(rest) = body.strip_prefix(marker) {
                let strip = marker.len() + usize::from(rest.starts_with(' '));
                edits.push((start + indent..start + indent + strip, String::new()));
            }
        } else if !line.trim().is_empty() {
            edits.push((start + indent..start + indent, format!("{marker} ")));
        }
    }
    edits
}

pub(crate) fn file_title(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

pub(crate) fn gutter_width(len_lines: usize, fonts: &Typography, window: &mut Window) -> f32 {
    let digits = len_lines.max(1).ilog10() as usize + 1;
    let sample = "0".repeat(digits.max(2));
    f32::from(shape_plain(&sample, fonts, window).width()) + CONTENT_PADDING * 2.0
}

impl EditorView {
    pub(crate) fn viewport_layers(&self, rows: Range<usize>) -> Vec<LineLayers> {
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
    fn the_schema_store_notes_deduplicate() {
        let mut store = SchemaStore::new();
        store.note("schema catalog: access denied for this account".to_string());
        store.note("schema catalog: access denied for this account".to_string());
        assert_eq!(store.notes.len(), 1);
        assert!(store.first_note().expect("noted").contains("denied"));
    }

    #[test]
    fn a_comment_toggle_uses_the_marker_the_language_actually_has() {
        let json = Buffer::new("{\n  \"a\": 1\n}\n");
        let edits = comment_edits(json.rope(), &[1], LanguageKind::Json);
        assert_eq!(
            edits,
            vec![(4..4, "// ".to_string())],
            "a hash in a JSON buffer is not a comment, it is a parse error"
        );

        let yaml = Buffer::new("a: 1\n");
        assert_eq!(
            comment_edits(yaml.rope(), &[0], LanguageKind::Yaml),
            vec![(0..0, "# ".to_string())]
        );
    }

    #[test]
    fn a_comment_toggle_uncomments_only_what_it_would_have_written() {
        let json = Buffer::new("  // \"a\": 1\n  //\"b\": 2\n");
        let edits = comment_edits(json.rope(), &[0, 1], LanguageKind::Json);
        assert_eq!(
            edits,
            vec![(2..5, String::new()), (14..16, String::new())],
            "the marker and the one space after it, and nothing else"
        );

        let mixed = Buffer::new("// \"a\": 1\n\"b\": 2\n");
        assert_eq!(
            comment_edits(mixed.rope(), &[0, 1], LanguageKind::Json),
            vec![(0..0, "// ".to_string()), (10..10, "// ".to_string())],
            "one uncommented line among commented ones comments the block"
        );
    }

    // "Which field is being typed into" used to be decided in three places in
    // the key router; the bar owns it now, and these pin the three rules the
    // sites shared.
    #[test]
    fn the_search_bar_owns_which_field_is_being_typed_into() {
        let mut bar = SearchBar::new();
        assert!(
            !bar.toggle_field(),
            "with no replace field, tab must not move typing into one"
        );
        assert!(!bar.typing_replace);

        assert!(bar.push("po"), "find-side typing owes a recompile");
        assert!(bar.pop());
        assert_eq!(bar.input, "p");

        bar.replace = Some(String::new());
        assert!(bar.toggle_field());
        assert!(bar.typing_replace);
        assert!(
            !bar.push("nginx"),
            "replace-side typing changes no match and owes none"
        );
        assert!(!bar.pop());
        assert_eq!(bar.replace.as_deref(), Some("ngin"));
        assert_eq!(bar.input, "p", "the find field is untouched");

        assert!(bar.toggle_field(), "tab returns to the find field");
        assert!(!bar.typing_replace);
    }
}
