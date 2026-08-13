//! The workspace entity: what the window is, and what it is holding.
//!
//! This module owns the state and the three general-purpose overlays. The rest
//! of the workspace lives beside it, split by what it is about rather than by
//! what it is made of: [`crate::hosting`] puts items in panes and takes them
//! out, [`crate::cluster`] chooses and adopts a connection, [`crate::chrome`]
//! draws the furniture and [`crate::render`] assembles it. All four are inherent
//! impls on this one type, because a window with a title bar and a set of tabs
//! is one object however many files describe it.
//!
//! Every read goes through [`ProviderSlot`], and every view holds a clone of the
//! same slot rather than a provider it was built with, so adopting a connection
//! re-points all of them at once.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, FocusHandle, Subscription, Window};

use k10s_core::Level;
use k10s_map::{MapView, Picked};

use crate::dock::Dock;
use crate::editor::{self, EditorView};
use crate::finder::{FileFinderView, FinderEvent, PathPickerView, PickerEvent, PickerMode};
use crate::fs;
use crate::hosting::Tab;
use crate::launch::LaunchView;
use crate::modal::ModalSlot;
use crate::palette::{PaletteEvent, PaletteView};
use crate::pane::Pane;
use crate::provider::{
    Detail, LaunchProvider, LogStop, NullLaunchProvider, ProviderSlot, ReadProvider, UsageOutcome,
};
use crate::selection::Selection;
use crate::tag::ItemTag;
use crate::ui::{DockSizes, Viewport};

/// Where the user's config files live; the app resolves the platform paths and
/// the workspace only opens what it is handed.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub settings: PathBuf,
    pub keymap: PathBuf,
}

/// What the path picker was opened for. One value rather than a flag per caller:
/// a second boolean beside the save target is exactly how two of these end up
/// true at once.
pub(crate) enum PickerPurpose {
    Open,
    Save(gpui::WeakEntity<EditorView>),
    Kubeconfig,
}

pub struct Workspace {
    pub(crate) map: Entity<MapView>,
    pub(crate) palette: ModalSlot<PaletteView>,
    pub(crate) launch: ModalSlot<LaunchView>,
    pub(crate) launch_provider: Rc<dyn LaunchProvider>,
    pub(crate) center: Pane<Tab>,
    pub(crate) left: Dock<Tab>,
    pub(crate) bottom: Dock<Tab>,
    pub(crate) selection: Option<Selection>,
    pub(crate) inspector_open: bool,
    pub(crate) app_menu_open: bool,
    // Zed's title bar drag state machine: armed on mouse down, fires the
    // compositor move on the first movement, disarmed everywhere else.
    pub(crate) should_move: bool,
    pub(crate) bench: bool,
    // Everything that reads the cluster holds a clone of the slot, so adopting
    // a connection after the window is open re-points all of them at once.
    pub(crate) slot: Rc<ProviderSlot>,
    pub(crate) schema: Rc<RefCell<editor::SchemaStore>>,
    pub(crate) fs: Arc<dyn fs::Fs>,
    pub(crate) config: Option<ConfigPaths>,
    pub(crate) files_root: Option<PathBuf>,
    pub(crate) picker: ModalSlot<PathPickerView>,
    pub(crate) picker_purpose: PickerPurpose,
    pub(crate) finder: ModalSlot<FileFinderView>,
    pub(crate) scratch_counter: usize,
    pub(crate) status_note: Option<String>,
    pub(crate) connected: bool,
    // Which cluster, for the label the state dot describes. `None` while
    // connected is an in-cluster service account, which has no context name.
    pub(crate) context: Option<String>,
    // Whether anything has been put in the world yet -- a cluster, or the
    // generator. Only used to decide whether dismissing the chooser needs to
    // say how to get back to it, which is a question that only has a wrong
    // answer when the map behind it is empty.
    pub(crate) scene_chosen: bool,
    pub(crate) events: Option<Detail>,
    pub(crate) log: Option<Detail>,
    // Live usage for the inspected pod or workload. The value is the last
    // labelled outcome the poll delivered; the guard is the poll -- dropping
    // it ends the fetching, so usage stops costing anything the moment the
    // inspector stops showing it.
    pub(crate) usage: Option<UsageOutcome>,
    pub(crate) usage_stop: Option<LogStop>,
    pub(crate) fetch_generation: u64,
    pub(crate) viewport: Viewport,
    pub(crate) dock_size_override: Option<DockSizes>,
    _pick_subscription: Subscription,
}

impl Workspace {
    pub fn new(
        map: Entity<MapView>,
        bench: bool,
        scene_chosen: bool,
        provider: Option<Rc<dyn ReadProvider>>,
        launch_provider: Option<Rc<dyn LaunchProvider>>,
        config: Option<ConfigPaths>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pick_subscription = cx.subscribe(&map, |this: &mut Self, map, picked: &Picked, cx| {
            this.selection = Selection::from_pick(&picked.snapshot, picked.path);
            this.inspector_open = this.selection.is_some();
            // The map does not remember its own clicks: the workspace decides
            // what is selected -- a finder result and a palette jump select
            // things the pointer never touched -- and hands the answer back so
            // the ring on the map and the inspector beside it are one fact.
            let marked = this.selection.as_ref().map(|_| picked);
            map.update(cx, |map, cx| map.set_selection(marked, cx));
            this.refresh_detail(cx);
            cx.notify();
        });
        let connected = provider.is_some();
        let slot = Rc::new(match provider {
            Some(provider) => ProviderSlot::new(provider),
            None => ProviderSlot::empty(),
        });
        let mut center = Pane::default();
        center.push(Tab::new(ItemTag::Map, map.clone()));
        Workspace {
            map,
            palette: ModalSlot::default(),
            launch: ModalSlot::default(),
            launch_provider: launch_provider.unwrap_or_else(|| Rc::new(NullLaunchProvider)),
            center,
            left: Dock::default(),
            bottom: Dock::default(),
            selection: None,
            inspector_open: false,
            app_menu_open: false,
            should_move: false,
            bench,
            slot,
            schema: Rc::new(RefCell::new(editor::SchemaStore::new())),
            fs: Arc::new(fs::RealFs),
            config,
            files_root: None,
            picker: ModalSlot::default(),
            picker_purpose: PickerPurpose::Open,
            finder: ModalSlot::default(),
            scratch_counter: 0,
            status_note: None,
            connected,
            context: None,
            // The caller also knows about a command-line scene while its
            // generator or cluster connection is still in flight. A provider
            // cannot express that state.
            scene_chosen: scene_chosen || connected || bench,
            events: None,
            log: None,
            usage: None,
            usage_stop: None,
            fetch_generation: 0,
            viewport: Viewport {
                width: 1600.0,
                height: 1000.0,
            },
            dock_size_override: None,
            _pick_subscription: pick_subscription,
        }
    }

    /// The seam every view reads the cluster through. One object behind two
    /// names was one name too many: this is the slot itself, handed out
    /// type-erased, so a view built before a connect and one built after are
    /// pointed at the same place.
    pub(crate) fn provider(&self) -> Rc<dyn ReadProvider> {
        self.slot.clone()
    }

    // Every selection change invalidates whatever was in flight: replies race
    // clicks, and a stale answer must never land under a newer question.
    pub(crate) fn refresh_detail(&mut self, cx: &mut Context<Self>) {
        self.fetch_generation += 1;
        self.events = None;
        self.log = None;
        self.sync_usage_poll(cx);
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
        let wants_log = selection.level == Level::Cell;

        let (tx, rx) = futures::channel::oneshot::channel();
        self.slot.fetch_events(
            namespace,
            &selection.name,
            Box::new(move |detail| {
                let _ = tx.send(detail);
            }),
        );
        self.land_detail(
            rx,
            generation,
            |this, detail| this.events = Some(detail),
            cx,
        );

        if wants_log {
            let (tx, rx) = futures::channel::oneshot::channel();
            self.slot.fetch_log_tail(
                namespace,
                &selection.name,
                Box::new(move |detail| {
                    let _ = tx.send(detail);
                }),
            );
            self.land_detail(rx, generation, |this, detail| this.log = Some(detail), cx);
        }
    }

    // The usage poll exists exactly while the inspector is looking at
    // something that has usage; anything else drops the guard, which is what
    // ends the poll -- there is no other stop signal, exactly like a log
    // follow. Called on every selection change and on every inspector toggle,
    // because toggling the panel closed must stop the polling that only
    // existed to fill it.
    pub(crate) fn sync_usage_poll(&mut self, cx: &mut Context<Self>) {
        // Dropping the previous guard first means two polls never overlap.
        self.usage_stop = None;
        self.usage = None;
        if !self.connected || !self.inspector_open {
            return;
        }
        let Some(request) = self.selection.as_ref().and_then(Selection::usage_target) else {
            return;
        };
        let generation = self.fetch_generation;
        // A slow UI drops ticks rather than queueing them: the next tick
        // supersedes whatever a full lane would have carried anyway.
        let (tx, mut rx) = futures::channel::mpsc::channel::<UsageOutcome>(8);
        let on_update: Box<dyn Fn(UsageOutcome) + Send + Sync> = Box::new(move |outcome| {
            let _ = tx.clone().try_send(outcome);
        });
        self.usage_stop = Some(self.slot.poll_usage(&request, on_update));
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(outcome) = rx.next().await {
                let live = this.update(cx, |this, cx| {
                    if this.fetch_generation != generation {
                        return false;
                    }
                    this.usage = Some(outcome);
                    cx.notify();
                    true
                });
                if !matches!(live, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    /// Park one detail reply and put it where it goes, unless a newer question
    /// has been asked in the meantime. The generation check is the whole point:
    /// two of these are in flight at once and a click can outrun either.
    fn land_detail(
        &self,
        reply: futures::channel::oneshot::Receiver<Detail>,
        generation: u64,
        place: fn(&mut Self, Detail),
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            if let Ok(detail) = reply.await {
                let _ = this.update(cx, |this, cx| {
                    if this.fetch_generation == generation {
                        place(this, detail);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    pub fn map_focus_handle(&self, cx: &App) -> FocusHandle {
        self.map.read(cx).focus_handle()
    }

    pub(crate) fn picker_seed(&self, source: &editor::EditorSource) -> PathBuf {
        match source {
            editor::EditorSource::File(path) => path
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/")),
            _ => seed_dir(self.files_root.as_deref()),
        }
    }

    pub(crate) fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.close(window, cx) {
            self.picker_purpose = PickerPurpose::Open;
            cx.notify();
        }
    }

    pub(crate) fn open_picker(
        &mut self,
        seed: PathBuf,
        mode: PickerMode,
        purpose: PickerPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.bench {
            return;
        }
        // Three sheets, one place on screen: whichever is up yields before this
        // one goes down, and each hands its focus back on the way out so the
        // handle this records is the one from before any of them opened.
        self.close_palette(window, cx);
        self.close_finder(window, cx);
        self.close_picker(window, cx);
        self.picker_purpose = purpose;
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
        self.picker.open(view, subscription, window, cx);
        cx.notify();
    }

    pub(crate) fn close_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.finder.close(window, cx) {
            cx.notify();
        }
    }

    pub(crate) fn open_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        self.close_picker(window, cx);
        self.close_finder(window, cx);
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
        self.finder.open(view, subscription, window, cx);
        cx.notify();
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.close(window, cx) {
            cx.notify();
        }
    }

    pub(crate) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        if self.palette.is_open() {
            self.close_palette(window, cx);
            return;
        }
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
                            eprintln!("k10s: the palette cannot build {name:?}: {error}");
                            this.status_note = Some(palette_note(name));
                            cx.notify();
                        }
                    }
                }
            },
        );
        self.palette.open(view, subscription, window, cx);
        cx.notify();
    }
}

// Where a picker opens when nothing better is known: an explicit root wins,
// else wherever the process is, else the filesystem root. One function rather
// than the same chain in four closures; only the root half is assertable,
// because `current_dir` cannot be controlled under `forbid(unsafe_code)`.
pub(crate) fn seed_dir(root: Option<&std::path::Path>) -> PathBuf {
    root.map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// What the window says when a command the palette listed cannot be built. The
/// reason itself goes to stderr, where the rest of them go, but somebody who
/// pressed enter on a command watched nothing happen and is owed a sentence
/// about it. A value rather than a method so the sentence can be checked
/// without a window.
pub(crate) fn palette_note(name: &str) -> String {
    format!("{name} did not run; the reason is on stderr")
}

#[cfg(test)]
mod tests {
    use super::palette_note;

    #[test]
    fn a_command_that_cannot_be_built_says_so_on_screen() {
        assert_eq!(
            palette_note("k10s::OpenSettings"),
            "k10s::OpenSettings did not run; the reason is on stderr"
        );
    }
}
