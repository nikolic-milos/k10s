//! Putting an item in a pane, finding it again, and taking it out.
//!
//! Every hosted view arrives the same way: a tag says which item it is, a place
//! says where its shape belongs, and [`Workspace::open_item`] does the rest. The
//! two rules that used to be the first line of fifteen separate methods live
//! there once -- a bench window hosts nothing but the map, and asking for a
//! document twice activates the tab already open rather than stacking another
//! copy of the same question. Four kinds of item do not fit that shape, because
//! reopening them means telling the view that exists something new: a diff
//! re-compares, the forwards registry starts another forward, the file panel
//! re-roots, and the terminal toggles. Those stay written out.

use gpui::{AppContext as _, Context, Entity, FocusHandle, SharedString, Subscription, Window};

use k10s_core::kind_short;
use k10s_map::MapView;

use crate::browse::{BrowseEvent, BrowseView};
use crate::config_schema;
use crate::diff;
use crate::editor::{self, EditorEvent, EditorView};
use crate::files::{FilesEvent, FilesView};
use crate::finder::PickerMode;
use crate::forwards::ForwardsView;
use crate::item::{Item, ItemHandle};
use crate::provider::{
    DescribeRequest, ExecRequest, ForwardRequest, LogRequest, WorkloadLogRequest,
};
use crate::tag::ItemTag;
use crate::term::TerminalView;
use crate::text::TextView;
use crate::workspace::{PickerPurpose, Workspace};

// The map is an item like any other hosted view: erasing it behind the same
// handle is what lets the center row treat "the starmap" and "a describe
// document" identically.
impl Item for MapView {
    fn title(&self) -> SharedString {
        "Starmap".into()
    }

    fn focus_handle(&self) -> FocusHandle {
        MapView::focus_handle(self)
    }
}

// One hosted view: the dedup tag the workspace finds it by, the type-erased
// handle it renders and focuses through, and whatever subscription keeps its
// events flowing. New panel kinds cost an `Item` impl and nothing here.
pub(crate) struct Tab {
    pub(crate) tag: ItemTag,
    pub(crate) view: Box<dyn ItemHandle>,
    _subscription: Option<Subscription>,
}

impl Tab {
    pub(crate) fn new(tag: ItemTag, view: impl ItemHandle + 'static) -> Tab {
        Tab {
            tag,
            view: Box::new(view),
            _subscription: None,
        }
    }

    pub(crate) fn with_subscription(
        tag: ItemTag,
        view: impl ItemHandle + 'static,
        subscription: Subscription,
    ) -> Tab {
        Tab {
            tag,
            view: Box::new(view),
            _subscription: Some(subscription),
        }
    }
}

/// An item lives where its shape belongs: documents in the centre row,
/// navigation on the left, panel-shaped feeds and sessions on the bottom.
#[derive(Clone, Copy)]
pub(crate) enum Place {
    Center,
    Left,
    Bottom,
}

/// Which of the two config files a command is asking for. An enum rather than
/// the boolean this used to take, because `open_config(true, ..)` at a call site
/// says nothing about which file it opens.
#[derive(Clone, Copy)]
pub(crate) enum ConfigFile {
    Settings,
    Keymap,
}

impl Workspace {
    /// Activate the item this tag names if one is already open; otherwise build
    /// one and put it where its shape belongs.
    ///
    /// The builder hands back a view and whatever subscription keeps it alive,
    /// and the tag is applied here rather than by the caller: an item filed
    /// under a tag other than the one it was looked up by would never be found
    /// again, and this makes that unrepresentable instead of a thing to notice.
    fn open_item<V: ItemHandle + 'static>(
        &mut self,
        tag: ItemTag,
        place: Place,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> (V, Option<Subscription>),
    ) {
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let (view, subscription) = build(self, window, cx);
        let tab = match subscription {
            Some(subscription) => Tab::with_subscription(tag, view, subscription),
            None => Tab::new(tag, view),
        };
        match place {
            Place::Center => self.open_center(tab, window, cx),
            Place::Left => self.open_left(tab, window, cx),
            Place::Bottom => self.open_bottom(tab, window, cx),
        }
    }

    pub(crate) fn focus_item(&self, tab: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&tab.view.focus_handle(cx), cx);
    }

    pub(crate) fn activate_center(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.center.activate(index) {
            return;
        }
        if let Some(tab) = self.center.active() {
            window.focus(&tab.view.focus_handle(cx), cx);
        }
        cx.notify();
    }

    pub(crate) fn activate_left(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.left.activate(index);
        if let Some(tab) = self.left.active() {
            window.focus(&tab.view.focus_handle(cx), cx);
        }
        cx.notify();
    }

    pub(crate) fn activate_bottom(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bottom.activate(index);
        if let Some(tab) = self.bottom.active() {
            window.focus(&tab.view.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn activate_existing(
        &mut self,
        tag: &ItemTag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(index) = self.center.find(|tab| &tab.tag == tag) {
            self.activate_center(index, window, cx);
            return true;
        }
        if let Some(index) = self.left.find(|tab| &tab.tag == tag) {
            self.activate_left(index, window, cx);
            return true;
        }
        if let Some(index) = self.bottom.find(|tab| &tab.tag == tag) {
            self.activate_bottom(index, window, cx);
            return true;
        }
        false
    }

    fn open_center(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.center.push(tab);
        self.activate_center(index, window, cx);
    }

    pub(crate) fn open_left(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.left.push(tab);
        self.activate_left(index, window, cx);
    }

    pub(crate) fn open_bottom(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.bottom.push(tab);
        self.activate_bottom(index, window, cx);
    }

    pub(crate) fn subscribe_browse(
        &mut self,
        view: &Entity<BrowseView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            view,
            window,
            |this, _, event: &BrowseEvent, window, cx| match event {
                BrowseEvent::OpenDoc(request) => this.open_doc(request.clone(), window, cx),
                BrowseEvent::OpenEdit(request) => this.open_editor(request.clone(), window, cx),
                BrowseEvent::OpenLogs { namespace, pod } => {
                    this.open_logs(namespace.clone(), pod.clone(), window, cx)
                }
                BrowseEvent::OpenWorkloadLogs {
                    namespace,
                    kind,
                    name,
                } => this.open_workload_logs(
                    WorkloadLogRequest {
                        namespace: namespace.clone(),
                        kind: *kind,
                        name: name.clone(),
                    },
                    window,
                    cx,
                ),
                BrowseEvent::StartForward(request) => {
                    this.open_forwards(Some(request.clone()), window, cx)
                }
                BrowseEvent::OpenExec { namespace, pod } => {
                    this.open_terminal(namespace.clone(), pod.clone(), window, cx)
                }
            },
        )
    }

    pub(crate) fn open_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_item(
            ItemTag::Browse,
            Place::Left,
            window,
            cx,
            |this, window, cx| {
                let provider = this.provider();
                let view = cx.new(|cx| BrowseView::kinds(provider, cx));
                let subscription = this.subscribe_browse(&view, window, cx);
                (view, Some(subscription))
            },
        );
    }

    pub(crate) fn open_nodes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_item(
            ItemTag::Nodes,
            Place::Left,
            window,
            cx,
            |this, window, cx| {
                let provider = this.provider();
                let view = cx.new(|cx| BrowseView::nodes(provider, cx));
                let subscription = this.subscribe_browse(&view, window, cx);
                (view, Some(subscription))
            },
        );
    }

    // One tab, reused: the inventory is a whole-cluster answer, so a second
    // press activates the one that is open rather than fetching it again.
    pub(crate) fn open_releases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_item(
            ItemTag::Releases,
            Place::Center,
            window,
            cx,
            |this, _, cx| {
                let provider = this.provider();
                (cx.new(|cx| TextView::releases(provider, cx)), None)
            },
        );
    }

    pub(crate) fn open_doc(
        &mut self,
        request: DescribeRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = ItemTag::Doc(format!("{}/{}", request.uid, request.name));
        self.open_item(tag, Place::Center, window, cx, |this, _, cx| {
            let provider = this.provider();
            (cx.new(|cx| TextView::doc(provider, request, cx)), None)
        });
    }

    // Every editor tab routes its events the same way: a save-as request
    // opens the picker aimed back at this editor, and a state change (dirty,
    // saved, renamed) repaints the tab strip so the dot is honest.
    pub(crate) fn subscribe_editor(
        &mut self,
        view: &Entity<EditorView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            view,
            window,
            |this, editor, event: &EditorEvent, window, cx| match event {
                EditorEvent::SaveAsRequested => {
                    let seed = this.picker_seed(editor.read(cx).source());
                    let purpose = PickerPurpose::Save(editor.downgrade());
                    this.open_picker(seed, PickerMode::Save, purpose, window, cx);
                }
                EditorEvent::DiffRequested { dry_run } => {
                    this.open_diff(editor, *dry_run, window, cx);
                }
                EditorEvent::StateChanged => {
                    let tag = Self::editor_tag(editor.read(cx).source());
                    let tag = if tag == ItemTag::Edit(String::new()) {
                        None
                    } else {
                        Some(tag)
                    };
                    let id = editor.entity_id();
                    if let Some(tag) = tag
                        && let Some(tab) =
                            this.center.iter_mut().find(|tab| tab.view.item_id() == id)
                    {
                        tab.tag = tag;
                    }
                    cx.notify();
                }
            },
        )
    }

    // A diff belongs to the buffer that asked for it: asking twice re-compares
    // in the tab that is already open rather than stacking copies of the same
    // question, and re-asking with a dry run upgrades that comparison in place.
    pub(crate) fn open_diff(
        &mut self,
        editor: &Entity<EditorView>,
        dry_run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sources) = editor.read(cx).diff_sources() else {
            self.status_note = Some("this buffer has no live version to compare with".to_string());
            cx.notify();
            return;
        };
        if self.bench {
            return;
        }
        let tag = ItemTag::Diff(format!("{}/{}", sources.request.uid, sources.request.name));
        if let Some(index) = self.center.find(|tab| tab.tag == tag) {
            let existing = self
                .center
                .get(index)
                .and_then(|tab| tab.view.to_any().downcast::<diff::DiffView>().ok());
            if let Some(existing) = existing {
                existing.update(cx, |view, cx| view.refresh(sources, dry_run, cx));
                self.activate_center(index, window, cx);
                return;
            }
        }
        let provider = self.provider();
        let weak = editor.downgrade();
        let view = cx.new(|cx| diff::DiffView::new(provider, weak, sources, dry_run, cx));
        self.open_center(Tab::new(tag, view), window, cx);
    }

    // One tag per document identity, so a scratch buffer saved to disk stops
    // being a scratch buffer and cannot be opened again beside itself.
    pub(crate) fn editor_tag(source: &editor::EditorSource) -> ItemTag {
        match source {
            editor::EditorSource::Cluster(request) => {
                ItemTag::Edit(format!("cluster:{}/{}", request.uid, request.name))
            }
            editor::EditorSource::File(path) => ItemTag::Edit(format!("file:{}", path.display())),
            // A scratch buffer has no identity to collide with, so it keeps
            // the tag it opened with until a save gives it a path.
            editor::EditorSource::Scratch => ItemTag::Edit(String::new()),
        }
    }

    pub(crate) fn open_editor(
        &mut self,
        request: DescribeRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = Self::editor_tag(&editor::EditorSource::Cluster(request.clone()));
        self.open_item(tag, Place::Center, window, cx, |this, window, cx| {
            let provider = this.provider();
            let schema = this.schema.clone();
            let fs = this.fs.clone();
            let view = cx.new(|cx| EditorView::cluster(provider, fs, schema, request, cx));
            let subscription = this.subscribe_editor(&view, window, cx);
            (view, Some(subscription))
        });
    }

    // A path is whatever it is on disk: a folder opens the files panel, a
    // file opens an editor. One entry point so the picker, the finder, the
    // panel, and the command line all agree.
    pub(crate) fn open_path(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Asking what the path is costs a stat, and a stat on a dead network
        // mount costs seconds, so the question is asked off this thread and the
        // answer opens the item one turn later.
        let fs = self.fs.clone();
        let probe = path.clone();
        let this = cx.weak_entity();
        window
            .spawn(cx, async move |cx| {
                let folder = cx
                    .background_executor()
                    .spawn(async move { fs.is_dir(&probe) })
                    .await;
                let _ = this.update_in(cx, |this, window, cx| {
                    if folder {
                        this.open_folder(path, window, cx);
                    } else {
                        this.open_file(path, window, cx);
                    }
                });
            })
            .detach();
    }

    fn open_file(&mut self, path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let tag = Self::editor_tag(&editor::EditorSource::File(path.clone()));
        self.open_item(tag, Place::Center, window, cx, |this, window, cx| {
            let provider = this.provider();
            let schema = this.schema.clone();
            let fs = this.fs.clone();
            let view = cx.new(|cx| EditorView::file(provider, fs, schema, path, cx));
            let subscription = this.subscribe_editor(&view, window, cx);
            (view, Some(subscription))
        });
    }

    pub(crate) fn open_folder(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.bench {
            return;
        }
        self.files_root = Some(path.clone());
        let existing = self
            .left
            .panels()
            .find(|(_, tab)| tab.tag == ItemTag::Files)
            .and_then(|(_, tab)| tab.view.to_any().downcast::<FilesView>().ok());
        if let Some(existing) = existing {
            existing.update(cx, |view, cx| view.set_root(path, cx));
            self.activate_existing(&ItemTag::Files, window, cx);
            cx.notify();
            return;
        }
        let fs = self.fs.clone();
        let view = cx.new(|cx| FilesView::new(fs, path, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &FilesEvent, window, cx| match event {
                FilesEvent::OpenFile(path) => this.open_file(path.clone(), window, cx),
            },
        );
        self.open_left(
            Tab::with_subscription(ItemTag::Files, view, subscription),
            window,
            cx,
        );
    }

    // The one opener that does not go through `open_item`: a scratch buffer has
    // no identity to collide with, so there is nothing to look up and every
    // press is meant to produce another one.
    pub(crate) fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        self.scratch_counter += 1;
        let title = format!("untitled-{}.yaml", self.scratch_counter);
        let tag = ItemTag::Edit(format!("scratch:{title}"));
        let provider = self.provider();
        let schema = self.schema.clone();
        let fs = self.fs.clone();
        let view = cx.new(|cx| EditorView::scratch(provider, fs, schema, title, cx));
        let subscription = self.subscribe_editor(&view, window, cx);
        self.open_center(Tab::with_subscription(tag, view, subscription), window, cx);
    }

    // Settings and keymap are ordinary editor tabs over the real files, with
    // the schema this binary defines. Saving them is what applies them: the
    // config poller notices the write and reloads, so there is no separate
    // settings UI to drift from the file.
    pub(crate) fn open_config(
        &mut self,
        which: ConfigFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.config.clone() else {
            if !self.bench {
                self.status_note = Some(
                    "no config directory on this platform, so there is no file to edit".to_string(),
                );
                cx.notify();
            }
            return;
        };
        let (path, template, root) = match which {
            ConfigFile::Keymap => (
                config.keymap,
                config_schema::KEYMAP_TEMPLATE,
                config_schema::keymap_root(cx),
            ),
            ConfigFile::Settings => (
                config.settings,
                config_schema::SETTINGS_TEMPLATE,
                config_schema::settings_root(
                    k10s_theme::registry(cx),
                    &cx.text_system().all_font_names(),
                ),
            ),
        };
        let tag = Self::editor_tag(&editor::EditorSource::File(path.clone()));
        self.open_item(tag, Place::Center, window, cx, |this, window, cx| {
            let provider = this.provider();
            let schema = this.schema.clone();
            let fs = this.fs.clone();
            let view = cx.new(|cx| {
                EditorView::file_or_template(provider, fs, schema, path, template, cx)
                    .with_schema_root(root)
            });
            let subscription = this.subscribe_editor(&view, window, cx);
            (view, Some(subscription))
        });
    }

    pub(crate) fn open_logs(
        &mut self,
        namespace: String,
        pod: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = ItemTag::Logs(format!("{namespace}/{pod}"));
        self.open_item(tag, Place::Bottom, window, cx, |this, _, cx| {
            let provider = this.provider();
            let request = LogRequest {
                namespace,
                pod,
                container: None,
                previous: false,
            };
            (cx.new(|cx| TextView::logs(provider, request, cx)), None)
        });
    }

    // One forwards item; a start request lands on the existing view rather
    // than opening a second registry window.
    pub(crate) fn open_forwards(
        &mut self,
        start: Option<ForwardRequest>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.bench {
            return;
        }
        if let Some(index) = self.bottom.find(|tab| tab.tag == ItemTag::Forwards) {
            if let Some(request) = start
                && let Some(view) = self
                    .bottom
                    .get(index)
                    .and_then(|tab| tab.view.to_any().downcast::<ForwardsView>().ok())
            {
                view.update(cx, |view, cx| view.start(request, cx));
            }
            self.activate_bottom(index, window, cx);
            return;
        }
        let provider = self.provider();
        let view = cx.new(|cx| ForwardsView::new(provider, start, cx));
        self.open_bottom(Tab::new(ItemTag::Forwards, view), window, cx);
    }

    // The shell we ask a container for: bash when the image has one, else
    // sh. A container with neither answers inside the terminal itself.
    pub(crate) fn shell_command() -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "command -v bash >/dev/null 2>&1 && exec bash || exec sh".to_string(),
        ]
    }

    pub(crate) fn open_terminal(
        &mut self,
        namespace: String,
        pod: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = ItemTag::Term(format!("{namespace}/{pod}"));
        self.open_item(tag, Place::Bottom, window, cx, |this, _, cx| {
            let provider = this.provider();
            let request = ExecRequest {
                namespace,
                pod,
                container: None,
                command: Self::shell_command(),
            };
            (cx.new(|cx| TerminalView::exec(provider, request, cx)), None)
        });
    }

    // Zed's terminal toggle semantics: create lazily on first use, focus it
    // when it is visible but unfocused, hide the dock when it already holds
    // focus. Lazy on purpose -- a shell nobody asked for yet must not spend
    // a process, an fd, or a paint at startup.
    pub(crate) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        let found = self.bottom.find(|tab| tab.tag == ItemTag::LocalTerm);
        let holds_focus = self
            .bottom
            .active()
            .is_some_and(|tab| tab.view.focus_handle(cx).contains_focused(window, cx));
        match terminal_toggle(
            found,
            self.bottom.is_open(),
            self.bottom.active_index(),
            holds_focus,
        ) {
            TerminalToggle::Hide => {
                self.bottom.set_open(false);
                self.activate_center(self.center.active_index(), window, cx);
            }
            TerminalToggle::Show(index) => self.activate_bottom(index, window, cx),
            TerminalToggle::Spawn => {
                let view = cx.new(TerminalView::local);
                self.open_bottom(Tab::new(ItemTag::LocalTerm, view), window, cx);
            }
        }
    }

    pub(crate) fn open_workload_logs(
        &mut self,
        request: WorkloadLogRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = ItemTag::Logs(format!(
            "{}/{}/{}",
            request.namespace,
            kind_short(request.kind),
            request.name
        ));
        self.open_item(tag, Place::Bottom, window, cx, |this, _, cx| {
            let provider = this.provider();
            (
                cx.new(|cx| TextView::workload_logs(provider, request, cx)),
                None,
            )
        });
    }

    // ctrl-w closes whatever has focus, if it is closable: a bottom panel, a
    // left panel, or a center tab that is not the map. Dropping the item
    // drops its entity: a log follow's stop guard or an exec session goes
    // with it.
    pub(crate) fn close_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused_in =
            |tab: &Tab, cx: &Context<Self>| tab.view.focus_handle(cx).contains_focused(window, cx);
        let center_active = self.center.active_index();
        let (active_dirty, active_focused) = self
            .center
            .active()
            .map(|tab| (tab.view.is_dirty(cx), focused_in(tab, cx)))
            .unwrap_or((false, false));
        let target = close_target(
            self.bottom.active().is_some_and(|tab| focused_in(tab, cx)),
            self.left.active().is_some_and(|tab| focused_in(tab, cx)),
            center_active,
            self.center.len(),
            active_dirty,
            active_focused,
        );
        match target {
            CloseTarget::Bottom => {
                self.bottom.remove(self.bottom.active_index());
                match self.bottom.active() {
                    Some(next) => self.focus_item(next, window, cx),
                    None => self.activate_center(center_active, window, cx),
                }
                cx.notify();
            }
            CloseTarget::Left => {
                self.left.remove(self.left.active_index());
                match self.left.active() {
                    Some(next) => self.focus_item(next, window, cx),
                    None => self.activate_center(center_active, window, cx),
                }
                cx.notify();
            }
            CloseTarget::WarnDirty => {
                if let Some(tab) = self.center.active() {
                    let handle = tab.view.focus_handle(cx);
                    window.focus(&handle, cx);
                }
                self.status_note =
                    Some("unsaved changes; ctrl-w again to discard them".to_string());
                cx.notify();
            }
            CloseTarget::Center(land_on) => {
                self.center.remove(center_active);
                self.activate_center(land_on, window, cx);
            }
            CloseTarget::Nothing => {}
        }
    }
}

// Whether tapping the terminal key hides, reveals, or spawns: hiding demands
// all three of open AND active AND focused, so a terminal that is merely
// somewhere in the dock is brought forward rather than dismissed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalToggle {
    Hide,
    Show(usize),
    Spawn,
}

pub(crate) fn terminal_toggle(
    found: Option<usize>,
    dock_open: bool,
    active_index: usize,
    holds_focus: bool,
) -> TerminalToggle {
    match found {
        None => TerminalToggle::Spawn,
        Some(index) if dock_open && active_index == index && holds_focus => TerminalToggle::Hide,
        Some(index) => TerminalToggle::Show(index),
    }
}

// What ctrl-w closes, decided as a value before any window is touched. The
// precedence is bottom, then left, then the centre; index zero is the map,
// which never closes, and that is also what keeps the row non-empty. An item
// holding unsaved work is never closed by a keystroke aimed somewhere else:
// it is focused instead, so the next ctrl-w reaches its own guard and the
// user sees what they are discarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseTarget {
    Bottom,
    Left,
    WarnDirty,
    // Closing a centre tab lands on whichever tab slid into its place, i.e.
    // the one to its right, clamped to the end of the row. That is
    // deliberately not what the docks do -- `Pane::remove` keeps the selection
    // on the left neighbour -- so the landing index is carried here rather
    // than taken from the pane.
    Center(usize),
    Nothing,
}

pub(crate) fn close_target(
    bottom_focused: bool,
    left_focused: bool,
    center_active: usize,
    center_len: usize,
    active_dirty: bool,
    active_focused: bool,
) -> CloseTarget {
    if bottom_focused {
        return CloseTarget::Bottom;
    }
    if left_focused {
        return CloseTarget::Left;
    }
    if center_active == 0 || center_len <= 1 {
        return CloseTarget::Nothing;
    }
    if active_dirty && !active_focused {
        return CloseTarget::WarnDirty;
    }
    CloseTarget::Center(center_active.min(center_len.saturating_sub(2)))
}

#[cfg(test)]
#[path = "hosting_test.rs"]
mod tests;
