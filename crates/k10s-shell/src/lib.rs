//! The app shell: the workspace that hosts the Starmap and everything docked
//! around it.
//!
//! The shell owns selection, actions, items, and panels; the map stays a view
//! that paints snapshots and emits picks. State crosses this boundary as
//! values -- a `Picked` carries the exact snapshot the user clicked on, and a
//! `Selection` is derived from it by a pure function, so a panel can never
//! disagree with the frame that was on screen. The center is a row of items
//! -- the map, the kind browser, the node capacity table, describe documents,
//! live log follows -- switched by tabs and keyed actions; anything hosted
//! implements [`Item`] and the workspace holds it as a boxed [`ItemHandle`],
//! so a new panel kind never touches workspace internals. Every read goes
//! through the [`ReadProvider`] seam, so the shell never sees kube, and every
//! denial arrives as a labelled state; the local terminal is the same
//! [`TerminalView`] on a PTY transport instead of an exec. The chrome is
//! Zed's: a title bar with the application menu, drag-to-move, and
//! client-side window controls when the compositor asks for them, and a
//! status bar whose panel buttons dispatch the same actions the keys do.
//! Keybindings are scoped by context (`Workspace`, `Browse`, `Doc`,
//! `Typing`), with text-typing keystrokes -- plain and shifted -- suppressed
//! while an input mode is capturing. Panels and items render on notify only:
//! zero paints at idle is a gated invariant and the shell must never be the
//! reason it fails.

pub mod browse;
pub mod config_schema;
pub mod diff;
pub mod dock;
pub mod editor;
pub mod files;
pub mod finder;
pub mod forwards;
pub mod fs;
pub mod item;
pub mod keymap;
pub mod launch;
pub mod palette;
pub mod provider;
pub mod pty;
pub mod settings;
pub mod table;
pub mod term;
pub mod text;
pub mod ui;

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, ClickEvent, Context, Decorations, DragMoveEvent, Entity, FocusHandle, IntoElement,
    KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NoAction, ParentElement,
    Render, Role, SharedString, Styled, Subscription, Window, actions, canvas, div, img,
    prelude::*, px, rgb, svg,
};

use k10s_core::{KindId, Level, SceneSnapshot, kind_short};
use k10s_map::{MapView, PickPath, Picked};
use k10s_theme::{Theme, Typography};

use browse::{BrowseEvent, BrowseView};
use dock::Dock;
use editor::{EditorEvent, EditorView};
use files::{FilesEvent, FilesView};
use finder::{FileFinderView, FinderEvent, PathPickerView, PickerEvent, PickerMode};
use forwards::ForwardsView;
pub use item::{Item, ItemHandle};
use launch::{LaunchEvent, LaunchView};
use palette::{PaletteEvent, PaletteView};
pub use provider::{
    ApplyOutcome, ApplyRequest, ConfigSource, Conflicted, ConnectOutcome, ConnectRequest,
    Connection, ContainersOutcome, ContextRow, DemoOutcome, DescribeRequest, Detail, DocOutcome,
    EventRow, ExecEvent, ExecRequest, ExecSession, ForwardOutcome, ForwardRequest, ForwardRow,
    ForwardState, KindRow, LaunchProvider, LogChunk, LogRequest, LogStop, ManifestOutcome,
    NullExecSession, NullLaunchProvider, NullProvider, ProviderFactory, ProviderSlot, ReadProvider,
    Reply, ScanOutcome, ScanRequest, SchemaCatalogOutcome, SchemaSource, SchemaTextOutcome,
    TableColumn, TableOutcome, TablePage, TableRow, WorkloadLogRequest,
};
use term::TerminalView;
use text::TextView;
use ui::{
    DockSizes, MAX_DOCK_SIZE, MIN_DOCK_SIZE, MODAL_TOP, RESIZE_HANDLE_SIZE, STATUS_BAR_HEIGHT,
    TAB_HEIGHT, TITLE_MARK_SIZE, Viewport, brand_mark, icon_button, key_hint, panel_header,
    title_bar_height,
};

actions!(
    k10s_shell,
    [
        ToggleInspector,
        ClearSelection,
        OpenPalette,
        ChooseCluster,
        OpenBrowser,
        OpenNodes,
        OpenForwards,
        OpenReleases,
        ToggleTerminal,
        Quit,
        DescribeSelection,
        LogsSelection,
        ExecSelection,
        NextItem,
        PrevItem,
        CloseItem,
        ToggleLeftDock,
        ToggleRightDock,
        ToggleBottomDock,
        RowUp,
        RowDown,
        RowPageUp,
        RowPageDown,
        RowHome,
        RowEnd,
        OpenRow,
        LogsRow,
        ExecRow,
        Refresh,
        LoadMore,
        StartForward,
        StopForward,
        EnterFilter,
        Back,
        DocScrollUp,
        DocScrollDown,
        DocPageUp,
        DocPageDown,
        DocHome,
        DocEnd,
        EnterSearch,
        NextMatch,
        PrevMatch,
        ToggleFollow,
        ToggleTimestamps,
        CycleContainer,
        TogglePrevious,
        Reload,
        CancelDoc,
        CommitInput,
        CancelInput,
        DeleteInputChar,
        EditSelection,
        EditRow,
        EditorUp,
        EditorDown,
        EditorLeft,
        EditorRight,
        EditorWordLeft,
        EditorWordRight,
        EditorHome,
        EditorEnd,
        EditorPageUp,
        EditorPageDown,
        EditorDocStart,
        EditorDocEnd,
        EditorSelectUp,
        EditorSelectDown,
        EditorSelectLeft,
        EditorSelectRight,
        EditorSelectWordLeft,
        EditorSelectWordRight,
        EditorSelectHome,
        EditorSelectEnd,
        EditorSelectAll,
        EditorBackspace,
        EditorDelete,
        EditorNewline,
        EditorTab,
        EditorShiftTab,
        EditorUndo,
        EditorRedo,
        EditorDeleteLine,
        EditorToggleComment,
        EditorCursorAbove,
        EditorCursorBelow,
        EditorSelectNext,
        EditorComplete,
        EditorFind,
        EditorReplace,
        EditorReplaceAll,
        EditorToggleRegex,
        EditorCancel,
        EditorSave,
        EditorSaveAs,
        OpenFile,
        OpenFolder,
        NewFile,
        FindFile,
        OpenSettings,
        OpenKeymap,
        PickParent,
        DiffAgainstLive,
        ApplyDryRun,
        ApplyToCluster,
        ForceApply,
        NextChange,
        PrevChange,
        ToggleFolded,
        KeepTheirs,
    ]
);

/// Every key context the shell sets. One list: the binding-scope invariant test
/// reads it, the keymap file's schema offers it as an enum, and the template a
/// person is handed names it, so a new context cannot be added to one without
/// the others. A slice rather than an array, because a hand-written length is a
/// second thing to keep in step for no benefit.
pub const KEY_CONTEXTS: &[&str] = &[
    "Workspace",
    "Browse",
    "Doc",
    "Diff",
    "Editor",
    "Typing",
    "Terminal",
    "Palette",
    "Map",
];

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

/// Produce the deeper-context guards that keep an ancestor workspace command
/// from stealing keystrokes that type text (plain and shifted alike). This is
/// derived rather than listed by hand so newly added default and user
/// bindings are safe automatically. Explicit bindings in an input context win
/// and are never replaced.
pub fn input_suppressors<'a>(
    bindings: impl IntoIterator<Item = &'a KeyBinding>,
) -> Vec<KeyBinding> {
    let bindings: Vec<&KeyBinding> = bindings.into_iter().collect();
    let input_contexts = ["Typing", "Palette", "Terminal", "Editor"];

    // A binding whose single keystroke would otherwise type text: no chording
    // modifier, but shift stays -- shift-f is how an F is typed, so a
    // shift-letter workspace command must be guarded exactly like a plain one.
    let typing_key = |binding: &KeyBinding| {
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
    };
    let context_of =
        |binding: &KeyBinding| binding.predicate().map(|predicate| predicate.to_string());

    let protected: std::collections::BTreeSet<(String, String)> = bindings
        .iter()
        .filter_map(|binding| {
            let context = context_of(binding)?;
            if !input_contexts.contains(&context.as_str()) {
                return None;
            }
            Some((context, typing_key(binding)?))
        })
        .collect();
    let captured: std::collections::BTreeSet<String> = bindings
        .iter()
        .filter(|binding| matches!(context_of(binding).as_deref(), None | Some("Workspace")))
        .filter_map(|binding| typing_key(binding))
        .collect();

    captured
        .into_iter()
        .flat_map(|key| {
            input_contexts.into_iter().filter_map({
                let protected = &protected;
                move |context| {
                    (!protected.contains(&(context.to_string(), key.clone())))
                        .then(|| KeyBinding::new(&key, NoAction, Some(context)))
                }
            })
        })
        .collect()
}

// What the user has selected, named well enough to ask the cluster about it:
// the uid keys identity across publishes, the kind id names the API resource
// a describe needs, the labels are for people, and the ancestry is what a
// data-plane request needs (a pod's logs want its namespace's name, not its
// slot).
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub level: Level,
    pub kind: &'static str,
    pub kind_id: KindId,
    pub uid: Arc<str>,
    pub name: Arc<str>,
    pub namespace: Option<Arc<str>>,
    pub owner: Option<Arc<str>>,
}

impl Selection {
    pub fn from_pick(snapshot: &SceneSnapshot, path: PickPath) -> Option<Selection> {
        let id_of = |ids: &[Arc<str>], slot: u32| {
            ids.get(slot as usize)
                .cloned()
                .unwrap_or_else(|| Arc::from(""))
        };
        let region = snapshot.regions.get(path.region as usize)?;
        let namespace = region.label.clone();
        let owner = path
            .block
            .and_then(|slot| snapshot.blocks.get(slot as usize))
            .map(|block| block.label.clone());

        let selection = match path.level() {
            Level::Region => Selection {
                level: Level::Region,
                kind: "namespace",
                kind_id: KindId::NAMESPACE,
                uid: id_of(&snapshot.ids.regions, path.region),
                name: namespace,
                namespace: None,
                owner: None,
            },
            Level::Block => {
                let slot = path.block?;
                let block = snapshot.blocks.get(slot as usize)?;
                Selection {
                    level: Level::Block,
                    kind: kind_short(block.ext.kind),
                    kind_id: block.ext.kind,
                    uid: id_of(&snapshot.ids.blocks, slot),
                    name: block.label.clone(),
                    namespace: Some(namespace),
                    owner: None,
                }
            }
            Level::Cell => {
                let slot = path.cell?;
                let cell = snapshot.cells.get(slot as usize)?;
                Selection {
                    level: Level::Cell,
                    kind: "pod",
                    kind_id: KindId::POD,
                    uid: id_of(&snapshot.ids.cells, slot),
                    name: cell.label.clone(),
                    namespace: Some(namespace),
                    owner,
                }
            }
            Level::Sat => {
                let slot = path.sat?;
                let satellite = snapshot.sats.get(slot as usize)?;
                Selection {
                    level: Level::Sat,
                    kind: kind_short(satellite.ext.kind),
                    kind_id: satellite.ext.kind,
                    uid: id_of(&snapshot.ids.sats, slot),
                    name: satellite.label.clone(),
                    namespace: Some(namespace),
                    owner,
                }
            }
        };
        Some(selection)
    }

    pub fn describe_request(&self) -> DescribeRequest {
        DescribeRequest {
            kind: self.kind_id,
            namespace: self.namespace.as_deref().map(str::to_string),
            name: self.name.to_string(),
            uid: self.uid.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ItemTag {
    Map,
    Browse,
    Nodes,
    Forwards,
    Files,
    Releases,
    Doc(String),
    Edit(String),
    Diff(String),
    Logs(String),
    Term(String),
    LocalTerm,
}

// What a cluster switch does to one open item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnAdopt {
    // Everything in it came out of the cluster and none of it is the user's, so
    // the previous cluster's answers leave with the previous cluster.
    Retire,
    // Cluster-derived, but holding text a person may have typed. Discarding
    // unsaved work to keep a provider tidy is the wrong trade, and the slot means
    // its next apply reaches the cluster this window is actually on.
    KeepUnsavedWork,
    // Nothing in it belongs to any cluster: the map's own scene, the file tree, a
    // local shell.
    NotTheClusters,
}

impl ItemTag {
    // The answer lives on the kind, next to the kind, and is a `match` with no
    // wildcard arm.
    //
    // It used to be a `matches!` beside `adopt`, twelve hundred lines from here.
    // An unlisted variant fell through to "not cluster bound", which is the
    // dangerous default: a cluster-backed tab that survives the switch goes on
    // painting the cluster the window has left, under a title that names no
    // cluster at all -- the one failure nothing on screen would admit to. The
    // hand-written guard test could not catch it either, because a kind nobody
    // added to the list is a kind neither of its loops ever constructs, so both
    // passed. Here a kind that has not answered does not compile, and that is the
    // whole reason for the shape.
    fn on_adopt(&self) -> OnAdopt {
        match self {
            ItemTag::Browse
            | ItemTag::Nodes
            | ItemTag::Forwards
            | ItemTag::Releases
            | ItemTag::Doc(_)
            | ItemTag::Diff(_)
            | ItemTag::Logs(_)
            | ItemTag::Term(_) => OnAdopt::Retire,
            ItemTag::Edit(_) => OnAdopt::KeepUnsavedWork,
            ItemTag::Map | ItemTag::Files | ItemTag::LocalTerm => OnAdopt::NotTheClusters,
        }
    }

    fn retires_on_adopt(&self) -> bool {
        self.on_adopt() == OnAdopt::Retire
    }
}

// Where the user's config files live; the app resolves the platform paths
// and the workspace only opens what it is handed.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub settings: std::path::PathBuf,
    pub keymap: std::path::PathBuf,
}

// The map is an item like any other hosted view: erasing it behind the same
// handle is what lets the center row treat "the starmap" and "a describe
// document" identically.
impl Item for MapView {
    fn title(&self) -> SharedString {
        "map".into()
    }

    fn focus_handle(&self) -> FocusHandle {
        MapView::focus_handle(self)
    }
}

// One hosted view: the dedup tag the workspace finds it by, the type-erased
// handle it renders and focuses through, and whatever subscription keeps its
// events flowing. New panel kinds cost an `Item` impl and nothing here.
struct Tab {
    tag: ItemTag,
    view: Box<dyn ItemHandle>,
    _subscription: Option<Subscription>,
}

impl Tab {
    fn new(tag: ItemTag, view: impl ItemHandle + 'static) -> Tab {
        Tab {
            tag,
            view: Box::new(view),
            _subscription: None,
        }
    }

    fn with_subscription(
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

#[derive(Clone, Copy)]
enum DockEdge {
    Left,
    Right,
    Bottom,
}

// What the path picker was opened for. One value rather than a flag per caller:
// a second boolean beside the save target is exactly how two of these end up
// true at once.
enum PickerPurpose {
    Open,
    Save(gpui::WeakEntity<EditorView>),
    Kubeconfig,
}

#[derive(Clone)]
struct DraggedDockResize(DockEdge);

impl Render for DraggedDockResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

pub struct Workspace {
    map: Entity<MapView>,
    palette: Option<(Entity<PaletteView>, Subscription)>,
    palette_previous_focus: Option<FocusHandle>,
    launch: Option<(Entity<LaunchView>, Subscription)>,
    launch_previous_focus: Option<FocusHandle>,
    launch_provider: Rc<dyn LaunchProvider>,
    center: Vec<Tab>,
    center_active: usize,
    left: Dock<Tab>,
    bottom: Dock<Tab>,
    selection: Option<Selection>,
    inspector_open: bool,
    app_menu_open: bool,
    // Zed's title bar drag state machine: armed on mouse down, fires the
    // compositor move on the first movement, disarmed everywhere else.
    should_move: bool,
    bench: bool,
    // Everything that reads the cluster holds a clone of the slot, so adopting
    // a connection after the window is open re-points all of them at once.
    slot: Rc<ProviderSlot>,
    provider: Rc<dyn ReadProvider>,
    schema: Rc<std::cell::RefCell<editor::SchemaStore>>,
    fs: std::sync::Arc<dyn fs::Fs>,
    config: Option<ConfigPaths>,
    files_root: Option<std::path::PathBuf>,
    picker: Option<(Entity<PathPickerView>, Subscription)>,
    picker_purpose: PickerPurpose,
    picker_previous_focus: Option<FocusHandle>,
    finder: Option<(Entity<FileFinderView>, Subscription)>,
    scratch_counter: usize,
    status_note: Option<String>,
    connected: bool,
    // Which cluster, for the label the state dot describes. `None` while
    // connected is an in-cluster service account, which has no context name.
    context: Option<String>,
    // Whether anything has been put in the world yet -- a cluster, or the
    // generator. Only used to decide whether dismissing the chooser needs to
    // say how to get back to it, which is a question that only has a wrong
    // answer when the map behind it is empty.
    scene_chosen: bool,
    events: Option<Detail>,
    log: Option<Detail>,
    fetch_generation: u64,
    viewport: Viewport,
    dock_size_override: Option<DockSizes>,
    _pick_subscription: Subscription,
}

impl Workspace {
    pub fn new(
        map: Entity<MapView>,
        bench: bool,
        provider: Option<Rc<dyn ReadProvider>>,
        launch_provider: Option<Rc<dyn LaunchProvider>>,
        config: Option<ConfigPaths>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pick_subscription = cx.subscribe(&map, |this: &mut Self, _, picked: &Picked, cx| {
            this.selection = Selection::from_pick(&picked.snapshot, picked.path);
            this.inspector_open = this.selection.is_some();
            this.refresh_detail(cx);
            cx.notify();
        });
        let connected = provider.is_some();
        let slot = Rc::new(match provider {
            Some(provider) => ProviderSlot::new(provider),
            None => ProviderSlot::empty(),
        });
        Workspace {
            map: map.clone(),
            palette: None,
            palette_previous_focus: None,
            launch: None,
            launch_previous_focus: None,
            launch_provider: launch_provider.unwrap_or_else(|| Rc::new(NullLaunchProvider)),
            center: vec![Tab::new(ItemTag::Map, map)],
            center_active: 0,
            left: Dock::default(),
            bottom: Dock::default(),
            selection: None,
            inspector_open: false,
            app_menu_open: false,
            should_move: false,
            bench,
            provider: slot.clone(),
            slot,
            schema: Rc::new(std::cell::RefCell::new(editor::SchemaStore::new())),
            fs: std::sync::Arc::new(fs::RealFs),
            config,
            files_root: None,
            picker: None,
            picker_purpose: PickerPurpose::Open,
            picker_previous_focus: None,
            finder: None,
            scratch_counter: 0,
            status_note: None,
            connected,
            context: None,
            // A bench flight is handed its scene at spawn and a command line
            // that named a cluster has already connected; only the path that
            // opens the chooser starts with an empty world.
            scene_chosen: connected || bench,
            events: None,
            log: None,
            fetch_generation: 0,
            viewport: Viewport {
                width: 1600.0,
                height: 1000.0,
            },
            dock_size_override: None,
            _pick_subscription: pick_subscription,
        }
    }

    // Every selection change invalidates whatever was in flight: replies race
    // clicks, and a stale answer must never land under a newer question.
    fn refresh_detail(&mut self, cx: &mut Context<Self>) {
        self.fetch_generation += 1;
        self.events = None;
        self.log = None;
        if !self.connected {
            return;
        }
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let Some(namespace) = selection.namespace.as_deref() else {
            return;
        };
        let generation = self.fetch_generation;

        let (tx, rx) = futures::channel::oneshot::channel();
        self.provider.fetch_events(
            namespace,
            &selection.name,
            Box::new(move |detail| {
                let _ = tx.send(detail);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(detail) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.fetch_generation == generation {
                        this.events = Some(detail);
                        cx.notify();
                    }
                });
            }
        })
        .detach();

        if selection.level == Level::Cell {
            let (tx, rx) = futures::channel::oneshot::channel();
            self.provider.fetch_log_tail(
                namespace,
                &selection.name,
                Box::new(move |detail| {
                    let _ = tx.send(detail);
                }),
            );
            cx.spawn(async move |this, cx| {
                if let Ok(detail) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        if this.fetch_generation == generation {
                            this.log = Some(detail);
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
    }

    pub fn map_focus_handle(&self, cx: &App) -> FocusHandle {
        self.map.read(cx).focus_handle()
    }

    fn focus_item(&self, tab: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&tab.view.focus_handle(cx), cx);
    }

    fn activate_center(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.center.len() {
            return;
        }
        self.center_active = index;
        if let Some(tab) = self.center.get(index) {
            window.focus(&tab.view.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn activate_left(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.left.activate(index);
        if let Some(tab) = self.left.active() {
            window.focus(&tab.view.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn activate_bottom(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.bottom.activate(index);
        if let Some(tab) = self.bottom.active() {
            window.focus(&tab.view.focus_handle(cx), cx);
        }
        cx.notify();
    }

    // An item lives where its shape belongs: documents in the center pane,
    // navigation on the left, panel-shaped feeds and sessions on the bottom.
    fn activate_existing(
        &mut self,
        tag: &ItemTag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(index) = self.center.iter().position(|tab| &tab.tag == tag) {
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
        self.center.push(tab);
        self.activate_center(self.center.len() - 1, window, cx);
    }

    fn open_left(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.left.push(tab);
        self.activate_left(index, window, cx);
    }

    fn open_bottom(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.bottom.push(tab);
        self.activate_bottom(index, window, cx);
    }

    fn subscribe_browse(
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

    fn open_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench || self.activate_existing(&ItemTag::Browse, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| BrowseView::kinds(provider, cx));
        let subscription = self.subscribe_browse(&view, window, cx);
        self.open_left(
            Tab::with_subscription(ItemTag::Browse, view, subscription),
            window,
            cx,
        );
    }

    fn open_nodes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench || self.activate_existing(&ItemTag::Nodes, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| BrowseView::nodes(provider, cx));
        let subscription = self.subscribe_browse(&view, window, cx);
        self.open_left(
            Tab::with_subscription(ItemTag::Nodes, view, subscription),
            window,
            cx,
        );
    }

    // One tab, reused: the inventory is a whole-cluster answer, so a second
    // press activates the one that is open rather than fetching it again.
    fn open_releases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench || self.activate_existing(&ItemTag::Releases, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| TextView::releases(provider, cx));
        self.open_center(Tab::new(ItemTag::Releases, view), window, cx);
    }

    fn open_doc(&mut self, request: DescribeRequest, window: &mut Window, cx: &mut Context<Self>) {
        let tag = ItemTag::Doc(format!("{}/{}", request.uid, request.name));
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| TextView::doc(provider, request, cx));
        self.open_center(Tab::new(tag, view), window, cx);
    }

    // Every editor tab routes its events the same way: a save-as request
    // opens the picker aimed back at this editor, and a state change (dirty,
    // saved, renamed) repaints the tab strip so the dot is honest.
    fn subscribe_editor(
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
    fn open_diff(
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
        if let Some(index) = self.center.iter().position(|tab| tab.tag == tag) {
            let existing = self.center[index]
                .view
                .to_any()
                .downcast::<diff::DiffView>()
                .ok();
            if let Some(existing) = existing {
                existing.update(cx, |view, cx| view.refresh(sources, dry_run, cx));
                self.activate_center(index, window, cx);
                return;
            }
        }
        let provider = self.provider.clone();
        let weak = editor.downgrade();
        let view = cx.new(|cx| diff::DiffView::new(provider, weak, sources, dry_run, cx));
        self.open_center(Tab::new(tag, view), window, cx);
    }

    // One tag per document identity, so a scratch buffer saved to disk stops
    // being a scratch buffer and cannot be opened again beside itself.
    fn editor_tag(source: &editor::EditorSource) -> ItemTag {
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

    fn picker_seed(&self, source: &editor::EditorSource) -> std::path::PathBuf {
        match source {
            editor::EditorSource::File(path) => path
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::path::PathBuf::from("/")),
            _ => self
                .files_root
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| std::path::PathBuf::from("/")),
        }
    }

    fn open_editor(
        &mut self,
        request: DescribeRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = Self::editor_tag(&editor::EditorSource::Cluster(request.clone()));
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let schema = self.schema.clone();
        let fs = self.fs.clone();
        let view = cx.new(|cx| EditorView::cluster(provider, fs, schema, request, cx));
        let subscription = self.subscribe_editor(&view, window, cx);
        self.open_center(Tab::with_subscription(tag, view, subscription), window, cx);
    }

    // A path is whatever it is on disk: a folder opens the files panel, a
    // file opens an editor. One entry point so the picker, the finder, the
    // panel, and the command line all agree.
    fn open_path(&mut self, path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
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
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let schema = self.schema.clone();
        let fs = self.fs.clone();
        let view = cx.new(|cx| EditorView::file(provider, fs, schema, path, cx));
        let subscription = self.subscribe_editor(&view, window, cx);
        self.open_center(Tab::with_subscription(tag, view, subscription), window, cx);
    }

    fn open_folder(
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

    fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        self.scratch_counter += 1;
        let title = format!("untitled-{}.yaml", self.scratch_counter);
        let tag = ItemTag::Edit(format!("scratch:{title}"));
        let provider = self.provider.clone();
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
    fn open_config(&mut self, keymap: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config.clone() else {
            if !self.bench {
                self.status_note = Some(
                    "no config directory on this platform, so there is no file to edit".to_string(),
                );
                cx.notify();
            }
            return;
        };
        let (path, template, root) = if keymap {
            (
                config.keymap,
                config_schema::KEYMAP_TEMPLATE,
                config_schema::keymap_root(cx),
            )
        } else {
            (
                config.settings,
                config_schema::SETTINGS_TEMPLATE,
                config_schema::settings_root(
                    k10s_theme::registry(cx),
                    &cx.text_system().all_font_names(),
                ),
            )
        };
        let tag = Self::editor_tag(&editor::EditorSource::File(path.clone()));
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let schema = self.schema.clone();
        let fs = self.fs.clone();
        let view = cx.new(|cx| {
            EditorView::file_or_template(provider, fs, schema, path, template, cx)
                .with_schema_root(root)
        });
        let subscription = self.subscribe_editor(&view, window, cx);
        self.open_center(Tab::with_subscription(tag, view, subscription), window, cx);
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            self.picker_purpose = PickerPurpose::Open;
            if let Some(previous) = self.picker_previous_focus.take() {
                window.focus(&previous, cx);
            }
            cx.notify();
        }
    }

    fn open_picker(
        &mut self,
        seed: std::path::PathBuf,
        mode: PickerMode,
        purpose: PickerPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.bench {
            return;
        }
        self.close_palette(window, cx);
        self.close_finder(window, cx);
        if self.picker.is_some() {
            self.close_picker(window, cx);
        }
        self.picker_purpose = purpose;
        self.picker_previous_focus = window.focused(cx);
        let fs = self.fs.clone();
        let view = cx.new(|cx| PathPickerView::new(fs, seed, mode, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &PickerEvent, window, cx| match event {
                PickerEvent::Dismissed => this.close_picker(window, cx),
                PickerEvent::Confirmed(path) => {
                    let path = path.clone();
                    let purpose = std::mem::replace(&mut this.picker_purpose, PickerPurpose::Open);
                    this.close_picker(window, cx);
                    match purpose {
                        PickerPurpose::Save(editor) => match editor.upgrade() {
                            Some(editor) => editor.update(cx, |editor, cx| {
                                editor.assign_path_and_save(path, cx);
                            }),
                            None => {
                                this.status_note = Some(
                                    "that editor closed before the save; nothing was written"
                                        .to_string(),
                                );
                                cx.notify();
                            }
                        },
                        PickerPurpose::Kubeconfig => this.scan_kubeconfig(path, cx),
                        PickerPurpose::Open => this.open_path(path, window, cx),
                    }
                }
            },
        );
        let focus = view.read(cx).focus_handle();
        self.picker = Some((view, subscription));
        window.focus(&focus, cx);
        cx.notify();
    }

    fn close_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.finder.take().is_some() {
            if let Some(previous) = self.picker_previous_focus.take() {
                window.focus(&previous, cx);
            }
            cx.notify();
        }
    }

    fn open_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        let Some(root) = self.files_root.clone() else {
            // No folder open, so there is nothing to search: say which key
            // opens one rather than showing an empty list.
            self.status_note = Some("no folder open; ctrl-shift-o opens one".to_string());
            cx.notify();
            return;
        };
        self.close_palette(window, cx);
        if self.picker.is_some() {
            self.close_picker(window, cx);
        }
        if self.finder.is_some() {
            self.close_finder(window, cx);
        }
        self.picker_previous_focus = window.focused(cx);
        let fs = self.fs.clone();
        let view = cx.new(|cx| FileFinderView::new(fs, root, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &FinderEvent, window, cx| match event {
                FinderEvent::Dismissed => this.close_finder(window, cx),
                FinderEvent::Confirmed(path) => {
                    let path = path.clone();
                    this.close_finder(window, cx);
                    this.open_path(path, window, cx);
                }
            },
        );
        let focus = view.read(cx).focus_handle();
        self.finder = Some((view, subscription));
        window.focus(&focus, cx);
        cx.notify();
    }

    fn open_logs(
        &mut self,
        namespace: String,
        pod: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = ItemTag::Logs(format!("{namespace}/{pod}"));
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let request = LogRequest {
            namespace,
            pod,
            container: None,
            previous: false,
        };
        let view = cx.new(|cx| TextView::logs(provider, request, cx));
        self.open_bottom(Tab::new(tag, view), window, cx);
    }

    // One forwards item; a start request lands on the existing view rather
    // than opening a second registry window.
    fn open_forwards(
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
                    .panels()
                    .nth(index)
                    .and_then(|(_, tab)| tab.view.to_any().downcast::<ForwardsView>().ok())
            {
                view.update(cx, |view, cx| view.start(request, cx));
            }
            self.activate_bottom(index, window, cx);
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| ForwardsView::new(provider, start, cx));
        self.open_bottom(Tab::new(ItemTag::Forwards, view), window, cx);
    }

    // The shell we ask a container for: bash when the image has one, else
    // sh. A container with neither answers inside the terminal itself.
    fn shell_command() -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "command -v bash >/dev/null 2>&1 && exec bash || exec sh".to_string(),
        ]
    }

    fn open_terminal(
        &mut self,
        namespace: String,
        pod: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tag = ItemTag::Term(format!("{namespace}/{pod}"));
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let request = ExecRequest {
            namespace,
            pod,
            container: None,
            command: Self::shell_command(),
        };
        let view = cx.new(|cx| TerminalView::exec(provider, request, cx));
        self.open_bottom(Tab::new(tag, view), window, cx);
    }

    // Zed's terminal toggle semantics: create lazily on first use, focus it
    // when it is visible but unfocused, hide the dock when it already holds
    // focus. Lazy on purpose -- a shell nobody asked for yet must not spend
    // a process, an fd, or a paint at startup.
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        if let Some(index) = self.bottom.find(|tab| tab.tag == ItemTag::LocalTerm) {
            let focused = self.bottom.is_open()
                && self.bottom.active_index() == index
                && self
                    .bottom
                    .active()
                    .is_some_and(|tab| tab.view.focus_handle(cx).contains_focused(window, cx));
            if focused {
                self.bottom.set_open(false);
                self.activate_center(self.center_active, window, cx);
            } else {
                self.activate_bottom(index, window, cx);
            }
            return;
        }
        let view = cx.new(TerminalView::local);
        self.open_bottom(Tab::new(ItemTag::LocalTerm, view), window, cx);
    }

    fn open_workload_logs(
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
        if self.bench || self.activate_existing(&tag, window, cx) {
            return;
        }
        let provider = self.provider.clone();
        let view = cx.new(|cx| TextView::workload_logs(provider, request, cx));
        self.open_bottom(Tab::new(tag, view), window, cx);
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            if let Some(previous) = self.palette_previous_focus.take() {
                window.focus(&previous, cx);
            }
            cx.notify();
        }
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        self.palette_previous_focus = window.focused(cx);
        let view = cx.new(PaletteView::new);
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &PaletteEvent, window, cx| match event {
                PaletteEvent::Dismissed => this.close_palette(window, cx),
                PaletteEvent::Confirmed(name) => {
                    let name = *name;
                    // Zed's order: whoever had focus gets it back, then the
                    // command lands exactly where the keystroke would have.
                    this.close_palette(window, cx);
                    match cx.build_action(name, None) {
                        Ok(action) => window.dispatch_action(action, cx),
                        Err(error) => {
                            eprintln!("k10s: the palette cannot build {name:?}: {error}")
                        }
                    }
                }
            },
        );
        let focus = view.read(cx).focus_handle();
        self.palette = Some((view, subscription));
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Show the chooser: the contexts this process can see, a way to reach a
    /// kubeconfig it cannot, and the generated starmap.
    ///
    /// Opened at startup when the command line named no cluster, and reopenable
    /// from the palette or its chord at any time after. It is an overlay rather
    /// than a separate window because the workspace behind it is already the
    /// thing being filled in.
    pub fn open_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench || self.launch.is_some() {
            return;
        }
        self.close_palette(window, cx);
        self.launch_previous_focus = window.focused(cx);
        let view = cx.new(LaunchView::new);
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, view, event: &LaunchEvent, window, cx| match event {
                LaunchEvent::Dismissed => this.dismiss_launch(window, cx),
                LaunchEvent::Chose(choice) => {
                    this.chose_launch(view.clone(), choice.clone(), window, cx)
                }
            },
        );
        let focus = view.read(cx).focus_handle();
        self.launch = Some((view.clone(), subscription));
        window.focus(&focus, cx);
        cx.notify();
        self.scan_launch(&view, ScanRequest::Detected, cx);
    }

    fn toggle_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launch.is_some() {
            self.dismiss_launch(window, cx);
        } else {
            self.open_launch(window, cx);
        }
    }

    fn close_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launch.take().is_some() {
            if let Some(previous) = self.launch_previous_focus.take() {
                window.focus(&previous, cx);
            }
            cx.notify();
        }
    }

    // Escape, or a click outside. Leaving with nothing chosen is allowed -- an
    // empty map is a legitimate place to stand -- but it has to say how to come
    // back, because the alternative is an empty window and a guess.
    fn dismiss_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let nothing_chosen = !self.scene_chosen;
        self.close_launch(window, cx);
        if nothing_chosen {
            self.status_note =
                Some("nothing chosen; ctrl-k ctrl-c picks a cluster or the starmap".to_string());
            cx.notify();
        }
    }

    // Reading and merging kubeconfigs is file I/O on paths that can be stalled
    // network mounts, so it happens behind the seam and lands here one turn
    // later. The request travels with the answer: two scans can be in flight and
    // each has to land under its own header.
    fn scan_launch(
        &mut self,
        view: &Entity<LaunchView>,
        request: ScanRequest,
        cx: &mut Context<Self>,
    ) {
        view.update(cx, |view, cx| view.rescanning(cx));
        let (tx, rx) = futures::channel::oneshot::channel();
        self.launch_provider.scan(
            request.clone(),
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        let view = view.downgrade();
        cx.spawn(async move |_, cx| {
            if let Ok(outcome) = rx.await {
                let _ = view.update(cx, |view, cx| view.scanned(&request, outcome, cx));
            }
        })
        .detach();
    }

    fn scan_kubeconfig(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        let Some((view, _)) = self.launch.as_ref() else {
            return;
        };
        let view = view.clone();
        self.scan_launch(&view, ScanRequest::File(path), cx);
    }

    fn chose_launch(
        &mut self,
        view: Entity<LaunchView>,
        choice: launch::Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match choice {
            launch::Choice::OpenKubeconfig => {
                // The picker opens over the chooser rather than instead of it,
                // so dismissing it returns to the list already on screen.
                let seed = self.kubeconfig_seed();
                self.open_picker(
                    seed,
                    PickerMode::OpenFile,
                    PickerPurpose::Kubeconfig,
                    window,
                    cx,
                );
            }
            launch::Choice::Demo => self.start_demo(view, window, cx),
            launch::Choice::Context { request, .. } => self.connect(view, request, window, cx),
        }
    }

    // Where a kubeconfig usually is, which is the only useful guess: the picker
    // lists whatever is there and a typed path overrides it.
    fn kubeconfig_seed(&self) -> std::path::PathBuf {
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| std::path::PathBuf::from(home).join(".kube"))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("/"))
    }

    fn connect(
        &mut self,
        view: Entity<LaunchView>,
        request: ConnectRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Nothing is given up until the new connection exists. A refused attempt
        // has to leave the window exactly as it was -- somebody who mistypes a
        // context while looking at production must not lose production for it --
        // and the seam is written the same way: it retires the previous cluster
        // only once the next one has synced.
        let (tx, rx) = futures::channel::oneshot::channel();
        self.launch_provider.connect(
            request,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        let this = cx.weak_entity();
        let view = view.downgrade();
        window
            .spawn(cx, async move |cx| {
                let Ok(outcome) = rx.await else {
                    let _ = view.update(cx, |view, cx| {
                        view.refused("the connection attempt was dropped".to_string(), cx)
                    });
                    return;
                };
                match outcome {
                    ConnectOutcome::Connected(connection) => {
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.adopt(connection, cx);
                            this.close_launch(window, cx);
                        });
                    }
                    // The screen stays open and usable, which is the whole point
                    // of it being a screen: an unreachable cluster is where a
                    // dead end would cost the most.
                    ConnectOutcome::Failed(why) => {
                        let _ = view.update(cx, |view, cx| view.refused(why, cx));
                    }
                }
            })
            .detach();
    }

    // Adopting is a provider swap and a notify, because every view reads through
    // the one slot rather than through a clone it was built with.
    fn adopt(&mut self, connection: Connection, cx: &mut Context<Self>) {
        // Every cluster-shaped view open right now belongs to the connection this
        // one replaces, and a table that keeps painting a cluster the window has
        // left is the one failure nothing on screen would admit to.
        let retired = self.retire_cluster_views(cx);
        self.slot.set((connection.provider)());
        self.connected = true;
        self.chose_scene(cx);
        self.context = connection.context;
        let mut note = connection.summary;
        // The notes themselves go to stderr, where they always went. Their count
        // goes here, because somebody who launched from a desktop entry has no
        // stderr to look at and must at least know there is something to look
        // for.
        match connection.notes.len() {
            0 => {}
            1 => note.push_str("  ·  1 degradation note on stderr"),
            many => note.push_str(&format!("  ·  {many} degradation notes on stderr")),
        }
        if retired > 0 {
            note.push_str(&format!(
                "  ·  closed {retired} view{} belonging to the previous cluster",
                if retired == 1 { "" } else { "s" }
            ));
        }
        self.status_note = Some(note);
        self.refresh_detail(cx);
        cx.notify();
    }

    // A scene has been chosen, whatever it is. The map forgets its framing rather
    // than being told to fit: the scene this is about is still on its way, and the
    // camera that framed the last one says nothing about it.
    fn chose_scene(&mut self, cx: &mut Context<Self>) {
        self.scene_chosen = true;
        self.map.update(cx, |map, cx| map.refit(cx));
    }

    fn start_demo(
        &mut self,
        view: Entity<LaunchView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.launch_provider.generate(Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }));
        let this = cx.weak_entity();
        let weak_view = view.downgrade();
        window
            .spawn(cx, async move |cx| {
                let Ok(outcome) = rx.await else {
                    let _ = weak_view.update(cx, |view, cx| {
                        view.refused("the generator was dropped".to_string(), cx)
                    });
                    return;
                };
                match outcome {
                    DemoOutcome::Started(summary) => {
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.chose_scene(cx);
                            this.status_note = Some(summary);
                            this.close_launch(window, cx);
                        });
                    }
                    DemoOutcome::Failed(why) => {
                        let _ = weak_view.update(cx, |view, cx| view.refused(why, cx));
                    }
                }
            })
            .detach();
    }

    // Which tabs a cluster switch invalidates is [`ItemTag::on_adopt`], asked
    // once per collection here. A fourth collection is a fourth line, and that is
    // the remaining hand-written thing in this function.
    fn retire_cluster_views(&mut self, cx: &mut Context<Self>) -> usize {
        let held = self
            .center
            .get(self.center_active)
            .map(|tab| tab.tag.clone());
        let before = self.center.len() + self.left.len() + self.bottom.len();
        // The map never retires, so the center can never empty here.
        self.center.retain(|tab| !tab.tag.retires_on_adopt());
        self.left.retain(|tab| !tab.tag.retires_on_adopt());
        self.bottom.retain(|tab| !tab.tag.retires_on_adopt());
        // The tab strip is corrected without `activate_center`, because that
        // focuses what it activates -- and the chooser is still on screen and
        // still the thing the keyboard belongs to. Taking focus here left a
        // refused connection with a list the arrow keys could no longer move.
        self.center_active = held
            .and_then(|tag| self.center.iter().position(|tab| tab.tag == tag))
            .unwrap_or(0);
        cx.notify();
        before - (self.center.len() + self.left.len() + self.bottom.len())
    }

    // ctrl-w closes whatever has focus, if it is closable: a bottom panel, a
    // left panel, or a center tab that is not the map. Dropping the item
    // drops its entity: a log follow's stop guard or an exec session goes
    // with it.
    fn close_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused_in =
            |tab: &Tab, cx: &Context<Self>| tab.view.focus_handle(cx).contains_focused(window, cx);
        if let Some(tab) = self.bottom.active()
            && focused_in(tab, cx)
        {
            self.bottom.remove(self.bottom.active_index());
            match self.bottom.active() {
                Some(next) => self.focus_item(next, window, cx),
                None => self.activate_center(self.center_active, window, cx),
            }
            cx.notify();
            return;
        }
        if let Some(tab) = self.left.active()
            && focused_in(tab, cx)
        {
            self.left.remove(self.left.active_index());
            match self.left.active() {
                Some(next) => self.focus_item(next, window, cx),
                None => self.activate_center(self.center_active, window, cx),
            }
            cx.notify();
            return;
        }
        if self.center_active > 0 && self.center.len() > 1 {
            // An item holding unsaved work is never closed by a keystroke
            // aimed somewhere else: focus it instead, so the next ctrl-w
            // reaches its own guard and the user sees what they are discarding.
            if self.center[self.center_active].view.is_dirty(cx) {
                let tab = &self.center[self.center_active];
                let focused = tab.view.focus_handle(cx).contains_focused(window, cx);
                if !focused {
                    let handle = tab.view.focus_handle(cx);
                    window.focus(&handle, cx);
                    self.status_note =
                        Some("unsaved changes; ctrl-w again to discard them".to_string());
                    cx.notify();
                    return;
                }
            }
            self.center.remove(self.center_active);
            let index = self.center_active.min(self.center.len() - 1);
            self.activate_center(index, window, cx);
        }
    }

    fn row(
        theme: &Theme,
        fonts: &Typography,
        label: &'static str,
        value: impl Into<SharedString>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(label),
            )
            .child(
                div()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(fonts.ui_size))
                    .text_color(rgb(theme.shell.text))
                    .child(value.into()),
            )
    }

    fn inspector(
        &self,
        theme: &Theme,
        fonts: &Typography,
        width: f32,
        selection: Option<Selection>,
    ) -> impl IntoElement {
        let body = match selection {
            None => div()
                .flex_1()
                .min_h(px(0.0))
                .p(px(12.0))
                .text_size(px(fonts.ui_size))
                .text_color(rgb(theme.shell.text_muted))
                .child("Nothing selected. Select a resource on the map.")
                .into_any_element(),
            Some(selection) => {
                let mut body = div()
                    .id("inspector-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .p(px(12.0))
                    .child(
                        div()
                            .text_size(px(fonts.ui_size))
                            .text_color(rgb(theme.shell.text))
                            .child(format!("{} {}", selection.kind, selection.name)),
                    )
                    .child(Self::row(theme, &fonts, "Name", selection.name.to_string()))
                    .child(Self::row(theme, &fonts, "Kind", selection.kind));
                if let Some(namespace) = selection.namespace.as_deref() {
                    body = body.child(Self::row(theme, &fonts, "Namespace", namespace.to_string()));
                }
                if let Some(owner) = selection.owner.as_deref() {
                    body = body.child(Self::row(theme, &fonts, "Owner", owner.to_string()));
                }
                if !selection.uid.is_empty() {
                    body = body.child(Self::row(theme, &fonts, "UID", selection.uid.to_string()));
                }
                body = body.child(Self::detail_section(
                    theme,
                    &fonts,
                    "Events",
                    self.events.as_ref(),
                    |detail| {
                        let Detail::Events(rows) = detail else {
                            return Vec::new();
                        };
                        rows.iter()
                            .map(|row| {
                                format!(
                                    "{} x{} {} — {}",
                                    row.kind, row.count, row.reason, row.message
                                )
                            })
                            .collect()
                    },
                ));
                if selection.level == Level::Cell {
                    body = body.child(Self::detail_section(
                        theme,
                        &fonts,
                        "Log tail",
                        self.log.as_ref(),
                        |detail| {
                            let Detail::Log(lines) = detail else {
                                return Vec::new();
                            };
                            lines.iter().rev().take(12).rev().cloned().collect()
                        },
                    ));
                }
                body.child(
                    div()
                        .text_size(px(fonts.small()))
                        .text_color(rgb(theme.shell.text_muted))
                        .child(if self.connected {
                            "d describe · l logs · s shell"
                        } else {
                            "Events and logs require a cluster connection."
                        }),
                )
                .into_any_element()
            }
        };

        div()
            .id("inspector")
            .w(px(width))
            .h_full()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme.shell.panel_background))
            .border_l_1()
            .border_color(rgb(theme.shell.border))
            .role(Role::Complementary)
            .aria_label("Inspector")
            .child(panel_header(theme, fonts, "Inspector"))
            .child(body)
            .child(Self::resize_handle(DockEdge::Right))
    }

    fn detail_section(
        theme: &Theme,
        fonts: &Typography,
        title: &'static str,
        detail: Option<&Detail>,
        rows: impl Fn(&Detail) -> Vec<String>,
    ) -> impl IntoElement {
        let mut section = div().flex().flex_col().gap(px(4.0)).child(
            div()
                .text_size(px(fonts.small()))
                .text_color(rgb(theme.shell.text_muted))
                .child(title),
        );
        section = match detail {
            None => section.child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child("Loading…"),
            ),
            Some(Detail::Denied(what)) => section.child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text))
                    .child(format!("{what}: access denied for this account")),
            ),
            Some(Detail::Failed(why)) => section.child(
                div()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text))
                    .child(why.clone()),
            ),
            Some(detail) => {
                let lines = rows(detail);
                if lines.is_empty() {
                    section.child(
                        div()
                            .text_size(px(fonts.small()))
                            .text_color(rgb(theme.shell.text_muted))
                            .child("None"),
                    )
                } else {
                    section.children(lines.into_iter().map(|line| {
                        div()
                            .text_size(px(fonts.small()))
                            .line_height(px(18.0))
                            .text_color(rgb(theme.shell.text))
                            .child(line)
                    }))
                }
            }
        };
        section
    }

    fn status_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(match (self.connected, self.context.as_deref()) {
            (true, Some(context)) => format!("connected to {context}"),
            (true, None) => "connected in-cluster".to_string(),
            (false, _) => "no cluster".to_string(),
        });
        if let Some(selection) = &self.selection {
            parts.push(format!("{} {}", selection.kind, selection.name));
        }
        if let Some(root) = &self.files_root {
            parts.push(format!("folder {}", root.display()));
        }
        let open = self.bottom.len();
        if open > 0 {
            parts.push(format!(
                "{open} panel{} below",
                if open == 1 { "" } else { "s" }
            ));
        }
        if let Some(note) = &self.status_note {
            parts.push(note.clone());
        }
        parts.join("  ·  ")
    }

    fn item_view(tab: &Tab) -> gpui::AnyElement {
        tab.view.to_any().into_any_element()
    }

    // A modal's scrim: click-outside dismisses, click-inside does not, and
    // the sheet lands where every other modal lands.
    fn modal_scrim(
        view: gpui::AnyElement,
        dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, dismiss)
            .child(
                div()
                    .absolute()
                    .top(px(MODAL_TOP))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(view),
            )
    }

    // Zed's dock toggle button: the panel's own icon, lit while its dock is
    // showing it, tooltip carrying the action's live keybinding, and the
    // click dispatching the same action the key would.
    fn panel_button<A: gpui::Action + Clone>(
        id: &'static str,
        icon: &'static str,
        label: &'static str,
        active: bool,
        action: A,
        theme: &Theme,
    ) -> impl IntoElement {
        let tooltip_action = action.clone();
        icon_button(id, icon, label, active, theme)
            .on_click(move |_, window, cx| {
                window.dispatch_action(Box::new(action.clone()), cx);
            })
            .tooltip(move |window, cx| {
                let tooltip = ui::Tooltip::with_binding(label, &tooltip_action, window);
                cx.new(move |_| tooltip).into()
            })
    }

    /// The sentence the title bar's state dot is about.
    ///
    /// Which cluster, not merely that there is one: somebody holding a prod and a
    /// staging context has to be able to answer "which of these am I about to
    /// apply to" by looking rather than by remembering, and a cluster is chosen on
    /// screen now, so the command line they started with no longer says.
    fn connection_label(connected: bool, context: Option<&str>) -> SharedString {
        match (connected, context) {
            (true, Some(context)) => context.to_string().into(),
            // A service account has no context name, and saying "connected" twice
            // over -- once as a dot, once as a word -- says nothing.
            (true, None) => "in-cluster".into(),
            (false, _) => "local starmap".into(),
        }
    }

    fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if self.viewport.update(width, height) {
            cx.notify();
        }
    }

    fn requested_dock_sizes(&self, cx: &App) -> DockSizes {
        self.dock_size_override.unwrap_or_else(|| {
            let settings = settings::active(cx);
            DockSizes {
                left: settings.left_dock_width,
                right: settings.right_dock_width,
                bottom: settings.bottom_dock_height,
            }
        })
    }

    fn resize_dock(&mut self, event: &DragMoveEvent<DraggedDockResize>, cx: &mut Context<Self>) {
        let mut sizes = self.requested_dock_sizes(cx);
        let position_x = f32::from(event.event.position.x);
        let position_y = f32::from(event.event.position.y);
        match event.drag(cx).0 {
            DockEdge::Left => sizes.left = position_x.clamp(MIN_DOCK_SIZE, MAX_DOCK_SIZE),
            DockEdge::Right => {
                sizes.right =
                    (self.viewport.width - position_x).clamp(MIN_DOCK_SIZE, MAX_DOCK_SIZE);
            }
            DockEdge::Bottom => {
                let body_bottom = self.viewport.height - STATUS_BAR_HEIGHT;
                sizes.bottom = (body_bottom - position_y).clamp(MIN_DOCK_SIZE, MAX_DOCK_SIZE);
            }
        }
        if self.dock_size_override != Some(sizes) {
            self.dock_size_override = Some(sizes);
            cx.notify();
        }
    }

    fn resize_handle(edge: DockEdge) -> gpui::AnyElement {
        let (id, handle) = match edge {
            DockEdge::Left => (
                "left-dock-resize",
                div()
                    .right_0()
                    .top_0()
                    .h_full()
                    .w(px(RESIZE_HANDLE_SIZE))
                    .cursor_col_resize(),
            ),
            DockEdge::Right => (
                "right-dock-resize",
                div()
                    .left_0()
                    .top_0()
                    .h_full()
                    .w(px(RESIZE_HANDLE_SIZE))
                    .cursor_col_resize(),
            ),
            DockEdge::Bottom => (
                "bottom-dock-resize",
                div()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h(px(RESIZE_HANDLE_SIZE))
                    .cursor_row_resize(),
            ),
        };
        handle
            .id(id)
            .absolute()
            .on_drag(DraggedDockResize(edge), |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .occlude()
            .into_any_element()
    }

    // The Linux window control strip, shown exactly when the compositor
    // hands us client-side decorations -- Zed's own bail-out rule. GNOME
    // style: 20 px circular hovers around 16 px glyphs, Zed's shipped icons.
    fn window_controls(theme: &Theme, window: &Window) -> Option<impl IntoElement> {
        if !matches!(window.window_decorations(), Decorations::Client { .. }) {
            return None;
        }
        let supported = window.window_controls();
        let maximize_icon = if window.is_maximized() {
            "icons/generic_restore.svg"
        } else {
            "icons/generic_maximize.svg"
        };
        let control = |id: &'static str, icon: &'static str, act: fn(&mut Window)| {
            div()
                .id(id)
                .size(px(20.0))
                .rounded_full()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .role(Role::Button)
                .aria_label(id)
                .hover(|button| button.bg(rgb(theme.shell.element_hover)))
                .child(
                    svg()
                        .path(icon)
                        .size(px(16.0))
                        .text_color(rgb(theme.shell.text_muted)),
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    act(window);
                })
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .pl(px(12.0))
                // A press on a control must not arm the window drag.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(supported.minimize.then(|| {
                    control("minimize", "icons/generic_minimize.svg", |window| {
                        window.minimize_window()
                    })
                }))
                .children(
                    supported
                        .maximize
                        .then(|| control("maximize", maximize_icon, |window| window.zoom_window())),
                )
                .child(control("close", "icons/generic_close.svg", |window| {
                    window.remove_window()
                })),
        )
    }

    fn title_bar(
        &self,
        theme: &Theme,
        fonts: &Typography,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active_title = self
            .center
            .get(self.center_active)
            .map(|tab| tab.view.title(cx))
            .unwrap_or_else(|| "map".into());
        let connection = Self::connection_label(self.connected, self.context.as_deref());

        div()
            .id("title-bar")
            .h(px(title_bar_height(window)))
            .w_full()
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .px(px(6.0))
            .bg(rgb(theme.shell.background))
            .role(Role::Toolbar)
            .aria_label("Title bar")
            // Zed's drag-to-move state machine: arm on mouse down, hand the
            // window to the compositor on the first movement, disarm on
            // every other outcome. Interactive children stop propagation on
            // mouse down so a button press never starts a move.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, _| this.should_move = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.should_move = false),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.should_move = false))
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, window, _| {
                if this.should_move {
                    this.should_move = false;
                    window.start_window_move();
                }
            }))
            .on_click(|event: &ClickEvent, window, _| {
                if event.click_count() == 2 {
                    window.zoom_window();
                }
            })
            .when(window.window_controls().window_menu, |bar| {
                bar.on_mouse_down(MouseButton::Right, |event: &MouseDownEvent, window, _| {
                    window.show_window_menu(event.position);
                })
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        icon_button(
                            "app-menu",
                            "icons/menu.svg",
                            "Application menu",
                            self.app_menu_open,
                            theme,
                        )
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.app_menu_open = !this.app_menu_open;
                            cx.notify();
                        }))
                        .tooltip(|_, cx| {
                            cx.new(|_| ui::Tooltip {
                                label: "Application Menu".into(),
                                key: None,
                            })
                            .into()
                        }),
                    )
                    // The brand lockup: the symbol, then the wordmark. The
                    // symbol rather than the helm because the wheel's spokes
                    // mush at this size, and the appearance picks the artwork
                    // rather than a tint, because a tinted brand colour is an
                    // approximation of a brand colour.
                    .child(
                        img(brand_mark(theme.appearance))
                            .size(px(TITLE_MARK_SIZE))
                            .flex_none(),
                    )
                    // The one place the product says its own name, so the
                    // one place the display face belongs: League Spartan is a
                    // headline typeface and reads as noise anywhere else.
                    .child(
                        div()
                            .font_family(k10s_theme::DISPLAY_FAMILY)
                            .text_size(px(fonts.ui_size))
                            .text_color(rgb(theme.shell.text))
                            .child("k10s"),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .text_size(px(fonts.small()))
                    .text_color(rgb(theme.shell.text_muted))
                    .child(active_title),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    // The state dot sits with the sentence it is about. It used
                    // to sit beside the mark, where a coloured dot next to a
                    // logo is decoration; next to the name of the thing it
                    // describes it is an indicator.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().size(px(8.0)).flex_none().rounded_full().bg(rgb(
                                if self.connected {
                                    theme.shell.success
                                } else {
                                    theme.shell.text_muted
                                },
                            )))
                            .child(
                                div()
                                    .text_size(px(fonts.small()))
                                    .text_color(rgb(theme.shell.text_muted))
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(connection),
                            ),
                    )
                    .children(Self::window_controls(theme, window)),
            )
    }

    // The burger's dropdown: the workspace commands with their bindings,
    // anchored under the title bar the way Zed deploys its application menu.
    // Click-out and escape dismiss; confirming dispatches and dismisses.
    fn app_menu(
        &self,
        theme: &Theme,
        fonts: &Typography,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let entry = |id: usize, label: &'static str, action: Box<dyn gpui::Action>| {
            let key = window
                .bindings_for_action(action.as_ref())
                .into_iter()
                .next()
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|keystroke| keystroke.inner().to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                });
            div()
                .id(("app-menu-item", id))
                .h(px(26.0))
                .px(px(10.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(16.0))
                .cursor_pointer()
                .hover(|item| item.bg(rgb(theme.shell.element_hover)))
                .text_size(px(fonts.small()))
                .text_color(rgb(theme.shell.text))
                .role(Role::MenuItem)
                .aria_label(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.app_menu_open = false;
                    window.dispatch_action(action.boxed_clone(), cx);
                    cx.notify();
                }))
                .child(label)
                .children(key.map(|key| {
                    div()
                        .text_size(px(fonts.xsmall()))
                        .text_color(rgb(theme.shell.text_muted))
                        .child(SharedString::from(key))
                }))
        };
        let separator = || {
            div()
                .h(px(1.0))
                .my(px(4.0))
                .flex_none()
                .bg(rgb(theme.shell.border_variant))
        };

        div()
            .id("app-menu-backdrop")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.app_menu_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("app-menu")
                    .absolute()
                    .top(px(title_bar_height(window)))
                    .left(px(6.0))
                    .w(px(280.0))
                    .flex()
                    .flex_col()
                    .py(px(4.0))
                    .bg(rgb(theme.shell.elevated_surface_background))
                    .border_1()
                    .border_color(rgb(theme.shell.border_variant))
                    .rounded(px(8.0))
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .role(Role::Menu)
                    .aria_label("Application menu")
                    .child(entry(0, "Command Palette…", Box::new(OpenPalette)))
                    .child(entry(1, "Choose Cluster…", Box::new(ChooseCluster)))
                    .child(separator())
                    .child(entry(2, "Browse Resources", Box::new(OpenBrowser)))
                    .child(entry(3, "Node Capacity", Box::new(OpenNodes)))
                    .child(entry(4, "Port Forwards", Box::new(OpenForwards)))
                    .child(entry(5, "Helm Releases", Box::new(OpenReleases)))
                    .child(entry(6, "Terminal", Box::new(ToggleTerminal)))
                    .child(separator())
                    .child(entry(7, "Toggle Left Dock", Box::new(ToggleLeftDock)))
                    .child(entry(8, "Toggle Bottom Dock", Box::new(ToggleBottomDock)))
                    .child(entry(9, "Toggle Inspector", Box::new(ToggleRightDock)))
                    .child(separator())
                    .child(entry(10, "Quit", Box::new(Quit))),
            )
    }

    // A panel with multiple items gets Zed's 32 px tab strip. Individual
    // panels still own their toolbars, just as Zed's terminal and project
    // panels do.
    fn dock_tabs(
        &self,
        dock: &Dock<Tab>,
        id: &'static str,
        activate: fn(&mut Self, usize, &mut Window, &mut Context<Self>),
        theme: &Theme,
        fonts: &Typography,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if dock.len() < 2 {
            return None;
        }
        let active = dock.active_index();
        Some(
            div()
                .id(id)
                .h(px(TAB_HEIGHT))
                .flex_none()
                .flex()
                .flex_row()
                .overflow_x_hidden()
                .bg(rgb(theme.shell.tab_bar_background))
                .border_b_1()
                .border_color(rgb(theme.shell.border))
                .role(Role::TabList)
                .aria_label("Dock tabs")
                .children(dock.panels().map(|(index, tab)| {
                    let selected = index == active;
                    div()
                        // A workspace can show left and bottom tab strips at
                        // once. Include the strip identity so GPUI never
                        // aliases interaction state between equal indices.
                        .id((id, index))
                        .px(px(12.0))
                        .h_full()
                        .flex_none()
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .bg(rgb(if selected {
                            theme.shell.tab_active_background
                        } else {
                            theme.shell.tab_inactive_background
                        }))
                        .border_r_1()
                        .when(!selected, |tab| tab.border_b_1())
                        .border_color(rgb(theme.shell.border))
                        .hover(|tab| tab.bg(rgb(theme.shell.element_hover)))
                        .text_size(px(fonts.ui_size))
                        .text_color(rgb(if selected {
                            theme.shell.text
                        } else {
                            theme.shell.text_muted
                        }))
                        .role(Role::Tab)
                        .aria_selected(selected)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut Self, _: &MouseDownEvent, window, cx| {
                                activate(this, index, window, cx);
                            }),
                        )
                        .child(tab.view.title(cx))
                }))
                .into_any_element(),
        )
    }

    fn tab_bar(&self, theme: &Theme, fonts: &Typography, cx: &Context<Self>) -> impl IntoElement {
        let active = self.center_active;
        div()
            .id("center-tabs")
            .h(px(TAB_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .flex_row()
            .overflow_x_hidden()
            .bg(rgb(theme.shell.tab_bar_background))
            .border_b_1()
            .border_color(rgb(theme.shell.border))
            .role(Role::TabList)
            .aria_label("Center tabs")
            .children(self.center.iter().enumerate().map(|(index, tab)| {
                let selected = index == active;
                div()
                    .id(("center-tab", index))
                    .px(px(12.0))
                    .h_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .bg(rgb(if selected {
                        theme.shell.tab_active_background
                    } else {
                        theme.shell.tab_inactive_background
                    }))
                    .border_r_1()
                    .when(!selected, |tab| tab.border_b_1())
                    .border_color(rgb(theme.shell.border))
                    .hover(|tab| tab.bg(rgb(theme.shell.element_hover)))
                    .text_size(px(fonts.ui_size))
                    .text_color(rgb(if selected {
                        theme.shell.text
                    } else {
                        theme.shell.text_muted
                    }))
                    .role(Role::Tab)
                    .aria_selected(selected)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut Self, _: &MouseDownEvent, window, cx| {
                            this.activate_center(index, window, cx);
                        }),
                    )
                    .child(tab.view.title(cx))
            }))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = k10s_theme::active(cx).clone();
        let fonts = k10s_theme::typography(cx).clone();
        let requested = self.requested_dock_sizes(cx);
        let workspace = cx.entity();
        let sizes = DockSizes::resolve(
            self.viewport,
            requested,
            self.left.is_open(),
            self.inspector_open,
            self.bottom.is_open(),
        );
        let content: gpui::AnyElement = self
            .center
            .get(self.center_active)
            .map(Self::item_view)
            .unwrap_or_else(|| self.map.clone().into_any_element());
        let left = (!self.bench && self.left.is_open()).then(|| {
            div()
                .id("left-dock")
                .w(px(sizes.left))
                .h_full()
                .relative()
                .flex_none()
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(rgb(theme.shell.panel_background))
                .border_r_1()
                .border_color(rgb(theme.shell.border))
                .role(Role::Complementary)
                .aria_label("Left dock")
                .children(self.dock_tabs(
                    &self.left,
                    "left-dock-tabs",
                    Self::activate_left,
                    &theme,
                    &fonts,
                    cx,
                ))
                .children(
                    self.left
                        .active()
                        .map(|tab| div().flex_1().min_h(px(0.0)).child(Self::item_view(tab))),
                )
                .child(Self::resize_handle(DockEdge::Left))
        });
        let bottom = (!self.bench && self.bottom.is_open()).then(|| {
            div()
                .id("bottom-dock")
                .h(px(sizes.bottom))
                .w_full()
                .relative()
                .flex_none()
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(rgb(theme.shell.panel_background))
                .border_t_1()
                .border_color(rgb(theme.shell.border))
                .role(Role::Complementary)
                .aria_label("Bottom dock")
                .children(self.dock_tabs(
                    &self.bottom,
                    "bottom-dock-tabs",
                    Self::activate_bottom,
                    &theme,
                    &fonts,
                    cx,
                ))
                .children(
                    self.bottom
                        .active()
                        .map(|tab| div().flex_1().min_h(px(0.0)).child(Self::item_view(tab))),
                )
                .child(Self::resize_handle(DockEdge::Bottom))
        });
        let right = (!self.bench && self.inspector_open)
            .then(|| self.inspector(&theme, &fonts, sizes.right, self.selection.clone()));
        let status = (!self.bench).then(|| {
            // Lit exactly when the dock is showing that panel, Zed's
            // is_active_button rule.
            let terminal_active = self.bottom.is_open()
                && self
                    .bottom
                    .active()
                    .is_some_and(|tab| tab.tag == ItemTag::LocalTerm);
            div()
                .id("status-bar")
                .h(px(STATUS_BAR_HEIGHT))
                .w_full()
                .flex_none()
                .px(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .bg(rgb(theme.shell.status_bar_background))
                .border_t_1()
                .border_color(rgb(theme.shell.border))
                .text_size(px(fonts.small()))
                .text_color(rgb(theme.shell.text_muted))
                .role(Role::Toolbar)
                .aria_label("Status bar")
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .child(Self::panel_button(
                            "toggle-left-dock",
                            "icons/file_tree.svg",
                            "Toggle Left Dock",
                            self.left.is_open(),
                            ToggleLeftDock,
                            &theme,
                        ))
                        .child(Self::panel_button(
                            "toggle-terminal",
                            "icons/terminal_alt.svg",
                            "Toggle Terminal",
                            terminal_active,
                            ToggleTerminal,
                            &theme,
                        ))
                        .child(
                            div()
                                .w(px(1.0))
                                .h(px(14.0))
                                .mx(px(4.0))
                                .flex_none()
                                .bg(rgb(theme.shell.border)),
                        )
                        .child(
                            div()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .child(SharedString::from(self.status_line())),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .flex_none()
                        .child(key_hint(&theme, &fonts, "Ctrl Shift P", "Commands"))
                        .child(Self::panel_button(
                            "toggle-inspector",
                            "icons/info.svg",
                            "Toggle Inspector",
                            self.inspector_open,
                            ToggleRightDock,
                            &theme,
                        )),
                )
        });
        let viewport_observer = (!self.bench).then(|| {
            canvas(
                move |bounds, _, cx| {
                    let _ = workspace.update(cx, |this, cx| {
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
            .size_full()
        });
        let center = div()
            .h_full()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .children((!self.bench).then(|| self.tab_bar(&theme, &fonts, cx)))
            .child(
                div()
                    .id("workspace-content")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .role(Role::Main)
                    .aria_label("Workspace")
                    .child(content),
            )
            .children(bottom);
        let title_bar = (!self.bench).then(|| self.title_bar(&theme, &fonts, window, cx));
        let app_menu =
            (self.app_menu_open && !self.bench).then(|| self.app_menu(&theme, &fonts, window, cx));
        let palette = self.palette.as_ref().map(|(view, _)| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.close_palette(window, cx);
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(MODAL_TOP))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(view.clone()),
                )
        });
        // The picker and the finder share the palette's scrim, dismissal,
        // and placement: three modals, one chrome.
        let picker = self.picker.as_ref().map(|(view, _)| {
            Self::modal_scrim(
                view.clone().into_any_element(),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_picker(window, cx);
                }),
            )
        });
        let finder = self.finder.as_ref().map(|(view, _)| {
            Self::modal_scrim(
                view.clone().into_any_element(),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_finder(window, cx);
                }),
            )
        });
        // The chooser stands down while it is asking for a file: two sheets at
        // the same place is one sheet with a lid on it. It is only unpainted, not
        // closed, so dismissing the picker brings back the list and the highlight
        // exactly as they were.
        let launch = self
            .launch
            .as_ref()
            .filter(|_| self.picker.is_none())
            .map(|(view, _)| {
                Self::modal_scrim(
                    view.clone().into_any_element(),
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.dismiss_launch(window, cx);
                    }),
                )
            });
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme.shell.background))
            .font_family(fonts.ui_family.clone())
            .text_size(px(fonts.ui_size))
            .text_color(rgb(theme.shell.text))
            .key_context("Workspace")
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedDockResize>, _, cx| {
                    this.resize_dock(event, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.toggle_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ChooseCluster, window, cx| {
                this.toggle_launch(window, cx);
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ToggleChurn, _, cx| {
                this.map.update(cx, |map, cx| map.toggle_churn(cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ToggleEdges, _, cx| {
                this.map.update(cx, |map, cx| map.toggle_edges(cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::ToggleHud, _, cx| {
                this.map.update(cx, |map, cx| map.toggle_hud(cx));
            }))
            .on_action(cx.listener(|this, _: &k10s_map::FitView, window, cx| {
                this.map.update(cx, |map, cx| map.fit(window, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, _, cx| {
                this.inspector_open = !this.inspector_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleLeftDock, _, cx| {
                this.left.toggle();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleRightDock, _, cx| {
                this.inspector_open = !this.inspector_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleBottomDock, _, cx| {
                this.bottom.toggle();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                if this.app_menu_open {
                    this.app_menu_open = false;
                    cx.notify();
                    return;
                }
                if this.selection.take().is_some() {
                    this.inspector_open = false;
                    this.refresh_detail(cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
            .on_action(cx.listener(|this, _: &OpenBrowser, window, cx| {
                this.open_browse(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenNodes, window, cx| {
                this.open_nodes(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenForwards, window, cx| {
                this.open_forwards(None, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenReleases, window, cx| {
                this.open_releases(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                this.toggle_terminal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DescribeSelection, window, cx| {
                if let Some(selection) = this.selection.clone() {
                    this.open_doc(selection.describe_request(), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &EditSelection, window, cx| {
                if let Some(selection) = this.selection.clone() {
                    this.open_editor(selection.describe_request(), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| {
                this.status_note = None;
                let seed = this
                    .files_root
                    .clone()
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("/"));
                this.open_picker(seed, PickerMode::OpenFile, PickerPurpose::Open, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenFolder, window, cx| {
                this.status_note = None;
                let seed = this
                    .files_root
                    .clone()
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("/"));
                this.open_picker(
                    seed,
                    PickerMode::OpenFolder,
                    PickerPurpose::Open,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &FindFile, window, cx| {
                this.open_finder(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewFile, window, cx| {
                this.status_note = None;
                this.new_file(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.open_config(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenKeymap, window, cx| {
                this.open_config(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &LogsSelection, window, cx| {
                let Some(selection) = this.selection.clone() else {
                    return;
                };
                let Some(namespace) = selection.namespace.as_deref() else {
                    return;
                };
                match selection.level {
                    Level::Cell => this.open_logs(
                        namespace.to_string(),
                        selection.name.to_string(),
                        window,
                        cx,
                    ),
                    // A workload's logs are the merged follows of its pods;
                    // a kind without a pod selector answers with a labelled
                    // failure from the data plane, not a guess here.
                    Level::Block => this.open_workload_logs(
                        WorkloadLogRequest {
                            namespace: namespace.to_string(),
                            kind: selection.kind_id,
                            name: selection.name.to_string(),
                        },
                        window,
                        cx,
                    ),
                    _ => {}
                }
            }))
            .on_action(cx.listener(|this, _: &ExecSelection, window, cx| {
                if let Some(selection) = this.selection.clone()
                    && selection.level == Level::Cell
                    && let Some(namespace) = selection.namespace.as_deref()
                {
                    this.open_terminal(
                        namespace.to_string(),
                        selection.name.to_string(),
                        window,
                        cx,
                    );
                }
            }))
            .on_action(cx.listener(|this, _: &NextItem, window, cx| {
                let next = (this.center_active + 1) % this.center.len();
                this.activate_center(next, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevItem, window, cx| {
                let previous = (this.center_active + this.center.len() - 1) % this.center.len();
                this.activate_center(previous, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseItem, window, cx| {
                this.close_focused(window, cx);
            }))
            .children(viewport_observer)
            .children(title_bar)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .children(left)
                    .child(center)
                    .children(right),
            )
            .children(status)
            .children(app_menu)
            .children(palette)
            .children(launch)
            .children(picker)
            .children(finder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{Op, State, replay};
    use k10s_world::{LayoutMode, PublishBench};

    fn slot_of(labels: impl Iterator<Item = Arc<str>>, name: &str) -> u32 {
        labels
            .enumerate()
            .find(|(_, label)| label.as_ref() == name)
            .map(|(slot, _)| slot as u32)
            .expect("the fixture names exist")
    }

    #[test]
    fn a_pick_becomes_a_selection_the_data_plane_can_act_on() {
        let initial = replay::initial_sync();
        let bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let snapshot = bench.snapshot();

        let region = slot_of(snapshot.regions.iter().map(|n| n.label.clone()), "prod");
        let block = slot_of(snapshot.blocks.iter().map(|n| n.label.clone()), "api");
        let cell = slot_of(snapshot.cells.iter().map(|n| n.label.clone()), "pod-1");

        let selection = Selection::from_pick(
            &snapshot,
            PickPath {
                region,
                block: Some(block),
                cell: Some(cell),
                sat: None,
            },
        )
        .expect("the path names live slots");
        assert_eq!(selection.level, Level::Cell);
        assert_eq!(selection.kind, "pod");
        assert_eq!(
            selection.kind_id,
            KindId::POD,
            "a describe needs the API resource, not a display string"
        );
        assert_eq!(selection.name.as_ref(), "pod-1");
        assert_eq!(
            selection.uid.as_ref(),
            "pod-1",
            "the uid comes from the identity vectors, not the label"
        );
        assert_eq!(selection.namespace.as_deref(), Some("prod"));
        assert_eq!(selection.owner.as_deref(), Some("api"));

        let request = selection.describe_request();
        assert_eq!(request.kind, KindId::POD);
        assert_eq!(request.namespace.as_deref(), Some("prod"));
        assert_eq!(request.name, "pod-1");
        assert_eq!(request.uid, "pod-1");

        let namespace_only = Selection::from_pick(
            &snapshot,
            PickPath {
                region,
                block: None,
                cell: None,
                sat: None,
            },
        )
        .expect("a region pick resolves");
        assert_eq!(namespace_only.level, Level::Region);
        assert_eq!(namespace_only.kind, "namespace");
        assert_eq!(namespace_only.kind_id, KindId::NAMESPACE);
        assert_eq!(namespace_only.uid.as_ref(), "ns-prod");
        assert_eq!(namespace_only.namespace, None);
        assert_eq!(
            namespace_only.describe_request().namespace,
            None,
            "a cluster-scoped describe carries no namespace"
        );
    }

    #[test]
    fn a_selection_keyed_by_uid_sees_slot_reuse() {
        let initial = replay::initial_sync();
        let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
        let snapshot = bench.snapshot();
        let cell = slot_of(snapshot.cells.iter().map(|n| n.label.clone()), "pod-1");
        let before = Selection::from_pick(
            &snapshot,
            PickPath {
                region: 0,
                block: Some(0),
                cell: Some(cell),
                sat: None,
            },
        )
        .expect("pod-1 resolves");
        drop(snapshot);

        bench.apply_events(&[
            replay::instance("pod-1", "prod", "wl-api", State::OK, Op::Deleted),
            replay::instance("pod-intruder", "prod", "wl-api", State::OK, Op::Added),
        ]);
        bench.run_publish();
        let snapshot = bench.snapshot();
        let after = Selection::from_pick(
            &snapshot,
            PickPath {
                region: 0,
                block: Some(0),
                cell: Some(cell),
                sat: None,
            },
        )
        .expect("the reused slot resolves");
        assert_eq!(after.name.as_ref(), "pod-intruder");
        assert_ne!(
            before.uid, after.uid,
            "the same slot now names a different pod, and only the uid says so"
        );
    }

    #[test]
    fn the_title_bar_names_the_cluster_rather_than_the_fact_of_one() {
        assert_eq!(
            Workspace::connection_label(true, Some("prod-eu-west")).as_ref(),
            "prod-eu-west"
        );
        assert_eq!(
            Workspace::connection_label(true, None).as_ref(),
            "in-cluster",
            "a service account has no context name and needs no invented one"
        );
        assert_eq!(
            Workspace::connection_label(false, Some("prod-eu-west")).as_ref(),
            "local starmap",
            "a context that is only remembered is not a connection"
        );
        assert_eq!(
            Workspace::connection_label(false, None).as_ref(),
            "local starmap"
        );
    }

    // Switching cluster invalidates every view whose content came out of the old
    // one. What it must *not* invalidate is anything holding text a person typed.
    //
    // Completeness is the compiler's half now: `on_adopt` is a match with no
    // wildcard arm, so a kind that has not answered does not build, and this test
    // no longer has to be the thing that notices. What it pins is the answers --
    // that nothing cluster-backed drifts into being kept, and that the two
    // reasons for keeping stay apart, because collapsing them is how the editor's
    // exemption would later read as an oversight and get "tidied" into a retire.
    #[test]
    fn a_cluster_switch_retires_the_views_it_invalidates_and_keeps_the_rest() {
        for (tag, expected) in [
            (ItemTag::Browse, OnAdopt::Retire),
            (ItemTag::Nodes, OnAdopt::Retire),
            (ItemTag::Forwards, OnAdopt::Retire),
            (ItemTag::Releases, OnAdopt::Retire),
            (ItemTag::Doc("uid/name".into()), OnAdopt::Retire),
            (ItemTag::Diff("uid/name".into()), OnAdopt::Retire),
            (ItemTag::Logs("prod/pod-1".into()), OnAdopt::Retire),
            (ItemTag::Term("prod/pod-1".into()), OnAdopt::Retire),
            (ItemTag::Map, OnAdopt::NotTheClusters),
            (ItemTag::Files, OnAdopt::NotTheClusters),
            (ItemTag::LocalTerm, OnAdopt::NotTheClusters),
            (
                ItemTag::Edit("cluster:uid/name".into()),
                OnAdopt::KeepUnsavedWork,
            ),
            (
                ItemTag::Edit("file:/tmp/x.yaml".into()),
                OnAdopt::KeepUnsavedWork,
            ),
            (ItemTag::Edit(String::new()), OnAdopt::KeepUnsavedWork),
        ] {
            assert_eq!(tag.on_adopt(), expected, "{tag:?}");
            assert_eq!(
                tag.retires_on_adopt(),
                expected == OnAdopt::Retire,
                "{tag:?}"
            );
        }
    }

    #[test]
    fn every_binding_names_a_context_the_shell_actually_sets() {
        let bindings = keybindings();
        assert!(!bindings.is_empty());
        for binding in &bindings {
            let predicate = binding
                .predicate()
                .map(|p| p.to_string())
                .unwrap_or_default();
            assert!(
                KEY_CONTEXTS.contains(&predicate.as_str()),
                "a binding is scoped to an unknown context: {predicate:?}"
            );
        }
    }

    #[test]
    fn the_terminal_captures_every_typing_workspace_binding_but_keeps_the_chords() {
        let bindings = keybindings();
        // The canonical spelling of a keystroke that types text; Display is
        // for people ("shift-F") and must not leak into matching.
        let typed = |b: &KeyBinding| {
            let stroke = &b.keystrokes()[0];
            if stroke.modifiers().shift {
                format!("shift-{}", stroke.key())
            } else {
                stroke.key().to_string()
            }
        };
        let workspace_typing: Vec<String> = bindings
            .iter()
            .filter(|b| {
                b.predicate().map(|p| p.to_string()).as_deref() == Some("Workspace")
                    && b.keystrokes().len() == 1
                    && {
                        let modifiers = b.keystrokes()[0].modifiers();
                        !(modifiers.control
                            || modifiers.alt
                            || modifiers.platform
                            || modifiers.function)
                    }
            })
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
                bindings.iter().any(|b| {
                    b.predicate().map(|p| p.to_string()).as_deref() == Some("Terminal")
                        && b.keystrokes().len() == 1
                        && typed(b) == *key
                        && gpui::is_no_action(b.action())
                }),
                "pressing {key:?} in a terminal would dispatch a command instead of typing"
            );
        }
        for chord in ["ctrl-tab", "ctrl-shift-tab", "ctrl-w"] {
            assert!(
                !bindings.iter().any(|b| {
                    b.predicate().map(|p| p.to_string()).as_deref() == Some("Terminal")
                        && format!("{}", b.keystrokes()[0]) == chord
                }),
                "{chord} is the way out of a terminal and must stay live"
            );
        }
    }

    #[test]
    fn the_editor_types_every_plain_workspace_key_but_keeps_its_own_escape() {
        let bindings = keybindings();
        let typed = |b: &KeyBinding| {
            let stroke = &b.keystrokes()[0];
            if stroke.modifiers().shift {
                format!("shift-{}", stroke.key())
            } else {
                stroke.key().to_string()
            }
        };
        let editor_binding = |key: &str| {
            bindings
                .iter()
                .filter(|b| {
                    b.predicate().map(|p| p.to_string()).as_deref() == Some("Editor")
                        && b.keystrokes().len() == 1
                        && {
                            let modifiers = b.keystrokes()[0].modifiers();
                            !(modifiers.control
                                || modifiers.alt
                                || modifiers.platform
                                || modifiers.function)
                        }
                })
                .find(|b| typed(b) == key)
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
                !bindings.iter().any(|b| {
                    b.predicate().map(|p| p.to_string()).as_deref() == Some("Editor")
                        && format!("{}", b.keystrokes()[0]) == chord
                }),
                "{chord} is the way out of the editor and must stay live"
            );
        }
    }

    #[test]
    fn the_default_map_focus_shadows_no_workspace_key() {
        let bindings = keybindings();
        let strokes_in = |context: &str| -> std::collections::BTreeSet<String> {
            bindings
                .iter()
                .filter(|b| b.predicate().map(|p| p.to_string()).as_deref() == Some(context))
                .filter(|b| !gpui::is_no_action(b.action()))
                .map(|b| {
                    b.keystrokes()
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
        let workspace_letters: Vec<String> = bindings
            .iter()
            .filter(|b| {
                b.predicate().map(|p| p.to_string()).as_deref() == Some("Workspace")
                    && b.keystrokes().len() == 1
                    && b.keystrokes()[0].modifiers().number_of_modifiers() == 0
                    && b.keystrokes()[0].key().chars().count() == 1
            })
            .map(|b| b.keystrokes()[0].key().to_string())
            .collect();
        assert!(!workspace_letters.is_empty());
        for letter in &workspace_letters {
            for context in ["Typing", "Palette"] {
                assert!(
                    bindings.iter().any(|b| {
                        b.predicate().map(|p| p.to_string()).as_deref() == Some(context)
                            && b.keystrokes().len() == 1
                            && b.keystrokes()[0].key() == letter
                            && gpui::is_no_action(b.action())
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
                binding.predicate().map(|p| p.to_string()).as_deref() == Some(context)
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
}
