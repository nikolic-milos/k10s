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

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, SharedString, Subscription, Window,
};

use k10s_core::Level;
use k10s_map::{MapView, OverlayFrame, OverlayKind, Picked};

use crate::dock::Dock;
use crate::editor::{self, EditorView};
use crate::finder::{
    ClusterFinderEvent, ClusterFinderView, FileFinderView, FinderEvent, PathPickerView,
    PickerEvent, PickerMode,
};
use crate::fs;
use crate::hosting::Tab;
use crate::launch::LaunchView;
use crate::modal::ModalSlot;
use crate::overlay::{self, next_overlay};
use crate::palette::{PaletteEvent, PaletteView};
use crate::pane::Pane;
use crate::selection::Selection;
use crate::tag::ItemTag;
use crate::ui::Viewport;
use k10s_provider::{
    ClusterProvider, Detail, LaunchProvider, LogStop, NullLaunchProvider, OverlayOutcome,
    PostureOutcome, ProviderSlot, RequestStop, ToolPresence, UsageOutcome,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ObservabilityNavigation {
    pub(crate) observe: Option<ToolPresence>,
    pub(crate) traces: Option<ToolPresence>,
    pub(crate) collectors: Option<ToolPresence>,
}

impl ObservabilityNavigation {
    pub(crate) fn set(&mut self, tag: &ItemTag, presence: ToolPresence) {
        match tag {
            ItemTag::Observe => self.observe = Some(presence),
            ItemTag::Traces => self.traces = Some(presence),
            ItemTag::Collectors => self.collectors = Some(presence),
            _ => {}
        }
    }

    pub(crate) fn available(&self, tag: &ItemTag) -> bool {
        let presence = match tag {
            ItemTag::Observe => self.observe,
            ItemTag::Traces => self.traces,
            ItemTag::Collectors => self.collectors,
            _ => None,
        };
        presence != Some(ToolPresence::Missing)
    }
}

#[derive(Default)]
struct InspectorRequests {
    events: Option<RequestStop>,
    logs: Option<RequestStop>,
}

impl InspectorRequests {
    fn reset(&mut self, connected: bool, visible: bool) -> bool {
        self.events = None;
        self.logs = None;
        connected && visible
    }
}

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
    SavedView,
}

#[derive(Default)]
pub(crate) struct UsageState {
    generation: u64,
    outcome: Option<UsageOutcome>,
}

impl UsageState {
    fn restart(&mut self) -> u64 {
        self.generation += 1;
        self.outcome = None;
        self.generation
    }

    fn current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn publish(&mut self, generation: u64, outcome: UsageOutcome) -> bool {
        if !self.current(generation) || self.outcome.as_ref() == Some(&outcome) {
            return false;
        }
        self.outcome = Some(outcome);
        true
    }

    pub(crate) fn outcome(&self) -> Option<&UsageOutcome> {
        self.outcome.as_ref()
    }
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
    pub(crate) observe_sparkline: Option<Entity<crate::chart::ChartView>>,
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
    pub(crate) cluster_finder: ModalSlot<ClusterFinderView>,
    pub(crate) scratch_counter: usize,
    pub(crate) status_note: Option<String>,
    // The stored-manifest apply that is out, if one is. Kept here rather than in
    // the Releases view, so a tab closed and reopened meanwhile refuses a second
    // one and still shows the answer.
    pub(crate) helm_in_flight: Option<crate::hosting::HelmInFlight>,
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
    pub(crate) observability_navigation: ObservabilityNavigation,
    inspector_requests: InspectorRequests,
    pub(crate) retained_loki: Option<ToolPresence>,
    pub(crate) retained_loki_notice: Option<SharedString>,
    retained_loki_generation: u64,
    retained_loki_stop: Option<RequestStop>,
    // Live usage for the inspected pod or workload. The value is the last
    // labelled outcome the poll delivered; the guard is the poll -- dropping
    // it ends the fetching, so usage stops costing anything the moment the
    // inspector stops showing it.
    pub(crate) usage: UsageState,
    pub(crate) usage_stop: Option<LogStop>,
    pub(crate) posture: Option<PostureOutcome>,
    pub(crate) overlay_kind: Option<OverlayKind>,
    pub(crate) overlay_generation: u64,
    pub(crate) fetch_generation: u64,
    pub(crate) viewport: Viewport,
    _pick_subscription: Subscription,
}

impl Workspace {
    pub fn new(
        map: Entity<MapView>,
        bench: bool,
        scene_chosen: bool,
        provider: Option<Rc<dyn ClusterProvider>>,
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
            observe_sparkline: None,
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
            cluster_finder: ModalSlot::default(),
            scratch_counter: 0,
            status_note: None,
            helm_in_flight: None,
            connected,
            context: None,
            // The caller also knows about a command-line scene while its
            // generator or cluster connection is still in flight. A provider
            // cannot express that state.
            scene_chosen: scene_chosen || connected || bench,
            events: None,
            log: None,
            observability_navigation: ObservabilityNavigation::default(),
            inspector_requests: InspectorRequests::default(),
            retained_loki: None,
            retained_loki_notice: None,
            retained_loki_generation: 0,
            retained_loki_stop: None,
            usage: UsageState::default(),
            usage_stop: None,
            posture: None,
            overlay_kind: None,
            overlay_generation: 0,
            fetch_generation: 0,
            viewport: Viewport {
                width: 1600.0,
                height: 1000.0,
            },
            _pick_subscription: pick_subscription,
        }
    }

    /// The seam every view reads the cluster through. One object behind two
    /// names was one name too many: this is the slot itself, handed out
    /// type-erased, so a view built before a connect and one built after are
    /// pointed at the same place.
    pub(crate) fn provider(&self) -> Rc<dyn ClusterProvider> {
        self.slot.clone()
    }

    // Every selection change invalidates whatever was in flight: replies race
    // clicks, and a stale answer must never land under a newer question.
    pub(crate) fn refresh_detail(&mut self, cx: &mut Context<Self>) {
        let mut metrics = None;
        if let Some(index) = self.center.find(|tab| tab.tag == ItemTag::Observe) {
            if let Ok(view) = self
                .center
                .get(index)
                .expect("found tab")
                .view
                .to_any()
                .downcast::<crate::observe::ObserveView>()
            {
                metrics = view.update(cx, |view, cx| {
                    view.set_selection(self.selection.as_ref(), cx);
                    view.metrics(cx)
                });
            }
        }
        self.set_observe_samples(metrics, cx);
        if let Some(index) = self.center.find(|tab| tab.tag == ItemTag::Traces) {
            if let Ok(view) = self
                .center
                .get(index)
                .expect("found tab")
                .view
                .to_any()
                .downcast::<crate::traces::TracesView>()
            {
                view.update(cx, |view, cx| {
                    view.set_selection(self.selection.as_ref(), cx)
                });
            }
        }
        self.fetch_generation += 1;
        let fetch = self
            .inspector_requests
            .reset(self.connected, self.inspector_open);
        self.events = None;
        self.log = None;
        self.retained_loki = None;
        self.retained_loki_notice = None;
        self.retained_loki_stop = None;
        self.posture = None;
        self.sync_usage_poll(cx);
        if !fetch {
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
        self.inspector_requests.events = Some(self.slot.fetch_events(
            namespace,
            &selection.uid,
            Box::new(move |detail| {
                let _ = tx.send(detail);
            }),
        ));
        self.land_detail(
            rx,
            generation,
            |this, detail, _| {
                this.inspector_requests.events = None;
                this.events = Some(detail);
            },
            cx,
        );

        if wants_log {
            let (tx, rx) = futures::channel::oneshot::channel();
            self.inspector_requests.logs = Some(self.slot.fetch_log_tail(
                namespace,
                &selection.name,
                &selection.uid,
                Box::new(move |detail| {
                    let _ = tx.send(detail);
                }),
            ));
            self.land_detail(
                rx,
                generation,
                |this, detail, cx| {
                    this.inspector_requests.logs = None;
                    this.log = Some(detail);
                    this.probe_retained_loki(cx);
                },
                cx,
            );
        }

        if wants_log {
            let namespace = namespace.to_string();
            let name = selection.name.to_string();
            let (tx, rx) = futures::channel::oneshot::channel();
            self.slot.fetch_pod_posture(
                &namespace,
                &name,
                Box::new(move |outcome| {
                    let _ = tx.send(outcome);
                }),
            );
            cx.spawn(async move |this, cx| {
                if let Ok(outcome) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        if this.fetch_generation == generation {
                            this.posture = Some(outcome);
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
    }

    // The usage poll exists exactly while the inspector is looking at
    // something that has usage; anything else drops the guard, which is what
    // ends the poll -- there is no other stop signal, exactly like a log
    // follow. Called on every selection change and on every inspector toggle,
    // because toggling the panel closed must stop the polling that only
    // existed to fill it.
    pub(crate) fn sync_usage_poll(&mut self, cx: &mut Context<Self>) {
        if self.inspector_open {
            self.probe_retained_loki(cx);
        } else {
            self.retained_loki_stop = None;
            self.retained_loki_generation += 1;
        }
        // Dropping the previous guard first means two polls never overlap.
        self.usage_stop = None;
        let generation = self.usage.restart();
        if !self.connected || !self.inspector_open {
            if let Some(chart) = &self.observe_sparkline {
                chart.update(cx, |chart, _| chart.pause_preparation());
            }
            return;
        }
        let Some(request) = self.selection.as_ref().and_then(Selection::usage_target) else {
            return;
        };
        let source = self.slot.current();
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
                    if !this.usage.current(generation)
                        || !this.inspector_open
                        || !std::rc::Rc::ptr_eq(&source, &this.slot.current())
                    {
                        return false;
                    }
                    if this.usage.publish(generation, outcome) {
                        cx.notify();
                    }
                    true
                });
                if !matches!(live, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    pub(crate) fn set_observe_samples(
        &mut self,
        data: Option<Arc<crate::chart::ChartData>>,
        cx: &mut Context<Self>,
    ) -> bool {
        match (self.observe_sparkline.as_ref(), data.as_ref()) {
            (None, None) => return false,
            (Some(chart), Some(data))
                if chart
                    .read(cx)
                    .data()
                    .is_some_and(|current| Arc::ptr_eq(current, data)) =>
            {
                return false;
            }
            _ => {}
        }
        if let Some(data) = data {
            let chart = self
                .observe_sparkline
                .get_or_insert_with(|| cx.new(|_| crate::chart::ChartView::new(true)));
            chart.update(cx, |chart, cx| chart.set_data(Some(data), cx));
        } else {
            self.observe_sparkline = None;
        }
        true
    }

    /// Park one detail reply and put it where it goes, unless a newer question
    /// has been asked in the meantime. The generation check is the whole point:
    /// two of these are in flight at once and a click can outrun either.
    fn land_detail(
        &self,
        reply: futures::channel::oneshot::Receiver<Detail>,
        generation: u64,
        place: fn(&mut Self, Detail, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            if let Ok(detail) = reply.await {
                let _ = this.update(cx, |this, cx| {
                    if this.fetch_generation == generation && this.inspector_open {
                        place(this, detail, cx);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn probe_retained_loki(&mut self, cx: &mut Context<Self>) {
        if !self.connected
            || !self.inspector_open
            || self.retained_loki.is_some()
            || self.retained_loki_stop.is_some()
            || !matches!(
                self.log,
                Some(Detail::Log {
                    previous_unavailable: true,
                    ..
                })
            )
        {
            return;
        }
        let generation = self.fetch_generation;
        self.retained_loki_generation += 1;
        let probe_generation = self.retained_loki_generation;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.retained_loki_stop = Some(self.slot.probe_loki(Box::new(move |presence| {
            let _ = tx.send(presence);
        })));
        cx.spawn(async move |this, cx| {
            if let Ok(presence) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.fetch_generation != generation
                        || this.retained_loki_generation != probe_generation
                        || !this.inspector_open
                    {
                        return;
                    }
                    this.retained_loki_stop = None;
                    match presence {
                        Ok(presence) => this.retained_loki = Some(presence),
                        Err(why) => {
                            this.retained_loki = Some(ToolPresence::Blocked);
                            this.retained_loki_notice =
                                Some(crate::chart::display_message(&why).into());
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn open_retained_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::chrome::retained_logs_available(
            self.connected,
            self.selection.as_ref(),
            self.log.as_ref(),
            self.retained_loki,
        ) {
            return;
        }
        let request = self
            .selection
            .as_ref()
            .expect("eligible Pod")
            .describe_request();
        self.open_observe_entry(
            crate::observe::ObserveEntry::RetainedLogs(request),
            window,
            cx,
        );
    }

    pub(crate) fn cycle_overlay(&mut self, cx: &mut Context<Self>) {
        self.set_overlay_kind(next_overlay(self.overlay_kind), cx);
    }

    pub(crate) fn set_overlay_kind(&mut self, kind: Option<OverlayKind>, cx: &mut Context<Self>) {
        self.overlay_kind = kind;
        self.refresh_overlay(cx);
    }

    /// Overlay through [`SavedView::overlay_kind`], camera through the same
    /// fly-to a click uses. Parse already refused secrets; this is the apply
    /// half, and it must not teleport.
    /// `s` and `a` need a pod, and a map pick is often a workload or a
    /// namespace. Saying which is missing beats a key that appears to do
    /// nothing: every other refusal in this shell is a labelled state, and a
    /// silent no-op reads as a broken binding.
    pub(crate) fn note_needs_a_pod(&mut self, verb: &str, cx: &mut Context<Self>) {
        let what = match self.selection.as_ref() {
            Some(selection) => format!("a {} is selected", selection.kind),
            None => "nothing is selected".to_string(),
        };
        self.status_note = Some(format!(
            "{verb} runs in a container, so it needs a pod -- {what}. Pick a pod, or press l for \
             this workload's merged logs."
        ));
        cx.notify();
    }

    pub(crate) fn apply_saved_view(
        &mut self,
        view: crate::saved_views::SavedView,
        cx: &mut Context<Self>,
    ) {
        self.set_overlay_kind(view.overlay_kind(), cx);
        let target = view.camera_target();
        self.map.update(cx, |map, cx| {
            map.fly_to(target, k10s_atlas::Motion::reduced_when(cx.reduce_motion()));
            cx.notify();
        });
        self.status_note = Some(view.load_status());
        cx.notify();
    }

    pub(crate) fn load_saved_view(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match self.fs.read_to_string(&path) {
            Ok(text) => match crate::saved_views::parse_view(&text) {
                Ok(view) => self.apply_saved_view(view, cx),
                Err(error) => {
                    self.status_note = Some(error.to_string());
                    cx.notify();
                }
            },
            Err(error) => {
                self.status_note = Some(format!("saved view could not be read: {error}"));
                cx.notify();
            }
        }
    }

    pub(crate) fn open_saved_view_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status_note = None;
        let seed = self
            .config
            .as_ref()
            .and_then(|paths| paths.settings.parent().map(PathBuf::from))
            .unwrap_or_else(|| seed_dir(self.files_root.as_deref()));
        self.open_picker(
            seed,
            PickerMode::OpenFile,
            PickerPurpose::SavedView,
            window,
            cx,
        );
    }

    pub(crate) fn refresh_overlay(&mut self, cx: &mut Context<Self>) {
        self.overlay_generation += 1;
        let Some(kind) = self.overlay_kind else {
            self.map
                .update(cx, |map, cx| map.set_overlay(OverlayFrame::default(), cx));
            cx.notify();
            return;
        };
        if !self.connected {
            self.map.update(cx, |map, cx| {
                map.set_overlay(
                    OverlayFrame {
                        kind: Some(kind),
                        marks: Vec::new(),
                    },
                    cx,
                );
            });
            self.status_note = Some(format!(
                "{} overlay needs a cluster",
                kind.badge().to_ascii_lowercase()
            ));
            cx.notify();
            return;
        }
        let generation = self.overlay_generation;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.slot.fetch_overlay(
            kind,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        );
        cx.spawn(async move |this, cx| {
            if let Ok(outcome) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.overlay_generation == generation {
                        this.apply_overlay(kind, outcome, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn apply_overlay(
        &mut self,
        kind: OverlayKind,
        outcome: OverlayOutcome,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.map.read(cx).snapshot();
        let (frame, note) = match outcome {
            OverlayOutcome::Ready {
                stamps,
                note,
                truncated,
            } => {
                let frame = overlay::resolve_frame(kind, &stamps, &snapshot);
                let note = if truncated && note.is_none() {
                    Some("overlay truncated; not every object was stamped".to_string())
                } else {
                    note
                };
                (frame, note)
            }
            OverlayOutcome::Denied(what) => (
                OverlayFrame {
                    kind: Some(kind),
                    marks: Vec::new(),
                },
                Some(format!("{what}: access denied for this account")),
            ),
            OverlayOutcome::Failed(why) => (
                OverlayFrame {
                    kind: Some(kind),
                    marks: Vec::new(),
                },
                Some(why),
            ),
        };
        self.map.update(cx, |map, cx| map.set_overlay(frame, cx));
        if let Some(note) = note {
            self.status_note = Some(note);
        }
        cx.notify();
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
        self.close_cluster_finder(window, cx);
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
                        PickerPurpose::SavedView => this.load_saved_view(path, cx),
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
        self.close_cluster_finder(window, cx);
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

    pub(crate) fn close_cluster_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cluster_finder.close(window, cx) {
            cx.notify();
        }
    }

    pub(crate) fn open_cluster_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        self.close_palette(window, cx);
        self.close_picker(window, cx);
        self.close_finder(window, cx);
        self.close_cluster_finder(window, cx);
        let snapshot = self.map.read(cx).snapshot();
        let map = self.map.clone();
        let view = cx.new(|cx| ClusterFinderView::new(map, snapshot, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &ClusterFinderEvent, window, cx| match event {
                ClusterFinderEvent::Dismissed => this.close_cluster_finder(window, cx),
                ClusterFinderEvent::Confirmed(uid) => {
                    let uid = uid.clone();
                    this.close_cluster_finder(window, cx);
                    let found = this
                        .map
                        .update(cx, |map, cx| map.select_and_reveal(&uid, window, cx));
                    if !found {
                        this.status_note = Some("that object is not on the map".to_string());
                        cx.notify();
                    } else {
                        this.status_note = None;
                    }
                }
            },
        );
        self.cluster_finder.open(view, subscription, window, cx);
        cx.notify();
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.close(window, cx) {
            cx.notify();
        }
    }

    pub(crate) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_palette_view(false, window, cx);
    }

    pub(crate) fn toggle_surface_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_palette_view(true, window, cx);
    }

    pub(crate) fn set_observability_presence(
        &mut self,
        tag: &ItemTag,
        presence: ToolPresence,
        cx: &mut Context<Self>,
    ) {
        self.observability_navigation.set(tag, presence);
        self.refresh_surface_picker(cx);
    }

    pub(crate) fn refresh_surface_picker(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.palette.view() {
            view.update(cx, |view, cx| {
                view.update_surfaces(self.observability_navigation, cx)
            });
        }
    }

    pub(crate) fn has_modal(&self) -> bool {
        self.palette.is_open()
            || self.launch.is_open()
            || self.picker.is_open()
            || self.finder.is_open()
            || self.cluster_finder.is_open()
    }

    pub(crate) fn rebase_modal_focus(&mut self, focus: &FocusHandle, cx: &App) {
        self.launch.rebase_focus(focus);
        let previous = self
            .launch
            .view()
            .map(|view| view.read(cx).focus_handle())
            .unwrap_or_else(|| focus.clone());
        self.palette.rebase_focus(&previous);
        self.picker.rebase_focus(&previous);
        self.finder.rebase_focus(&previous);
        self.cluster_finder.rebase_focus(&previous);
    }

    fn toggle_palette_view(&mut self, surfaces: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench {
            return;
        }
        if self
            .palette
            .view()
            .is_some_and(|view| view.read(cx).is_surface_picker() == surfaces)
        {
            self.close_palette(window, cx);
            return;
        }
        self.close_palette(window, cx);
        self.close_picker(window, cx);
        self.close_finder(window, cx);
        self.close_cluster_finder(window, cx);
        self.close_launch(window, cx);
        self.app_menu_open = false;
        let state = if surfaces {
            let mut state = self.rail_state();
            if self.left.is_open()
                && self
                    .left
                    .active()
                    .is_some_and(|tab| tab.view.focus_handle(cx).contains_focused(window, cx))
            {
                state = crate::activity::RailState {
                    left: state.left,
                    ..Default::default()
                };
            } else if self.bottom.is_open()
                && self
                    .bottom
                    .active()
                    .is_some_and(|tab| tab.view.focus_handle(cx).contains_focused(window, cx))
            {
                state = crate::activity::RailState {
                    bottom: state.bottom,
                    ..Default::default()
                };
            } else {
                state.left = None;
                state.bottom = None;
                state.inspector_open = false;
            }
            state
        } else {
            Default::default()
        };
        let view = cx.new(|cx| {
            if surfaces {
                PaletteView::for_surfaces(self.observability_navigation, state, cx)
            } else {
                PaletteView::new(cx)
            }
        });
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &PaletteEvent, window, cx| match event {
                PaletteEvent::Dismissed => this.close_palette(window, cx),
                PaletteEvent::SurfaceConfirmed(id) => {
                    this.close_palette(window, cx);
                    this.open_surface(*id, window, cx);
                }
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

#[cfg(test)]
mod inspector_tests {
    use super::InspectorRequests;
    use k10s_provider::RequestStop;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn hiding_replacing_and_disconnect_cancel_inspector_reads_and_reopening_admits_fresh_reads() {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let guard = || {
            let cancelled = cancelled.clone();
            RequestStop::new(move || {
                cancelled.fetch_add(1, Ordering::Relaxed);
            })
        };
        let mut requests = InspectorRequests::default();
        assert!(requests.reset(true, true));
        requests.events = Some(guard());
        requests.logs = Some(guard());
        assert!(!requests.reset(true, false));
        assert_eq!(cancelled.load(Ordering::Relaxed), 2);
        assert!(!requests.reset(true, false));
        assert_eq!(cancelled.load(Ordering::Relaxed), 2);

        assert!(requests.reset(true, true), "reopening admits fresh reads");
        requests.events = Some(guard());
        requests.logs = Some(guard());
        assert!(
            requests.reset(true, true),
            "replacement cancels before admission"
        );
        assert_eq!(cancelled.load(Ordering::Relaxed), 4);
        requests.events = Some(guard());
        requests.logs = Some(guard());
        assert!(!requests.reset(false, true));
        assert_eq!(cancelled.load(Ordering::Relaxed), 6);
    }
}

#[cfg(test)]
mod observability_navigation_tests {
    use super::ObservabilityNavigation;
    use crate::tag::ItemTag;
    use k10s_provider::ToolPresence;

    #[test]
    fn absence_is_local_to_one_tool_and_a_context_reset_discards_it() {
        let mut navigation = ObservabilityNavigation::default();
        for tag in [ItemTag::Observe, ItemTag::Traces, ItemTag::Collectors] {
            assert!(navigation.available(&tag));
            navigation.set(&tag, ToolPresence::Missing);
            assert!(!navigation.available(&tag));
            assert!(navigation.available(&ItemTag::Browse));
            for presence in [ToolPresence::Blocked, ToolPresence::Ready] {
                navigation.set(&tag, presence);
                assert!(navigation.available(&tag));
            }
            navigation.set(&tag, ToolPresence::Missing);
        }
        navigation = ObservabilityNavigation::default();
        for tag in [ItemTag::Observe, ItemTag::Traces, ItemTag::Collectors] {
            assert!(navigation.available(&tag));
        }
    }
}

#[cfg(test)]
mod usage_tests {
    use super::UsageState;
    use k10s_provider::{Bytes, Millicores, UsageOutcome, UsageSample, UsageSource};

    fn sample() -> UsageSample {
        UsageSample {
            cpu: Some(Millicores(10)),
            memory: Some(Bytes(1024)),
            cpu_request: Some(Millicores(100)),
            cpu_limit: Some(Millicores(200)),
            memory_request: Some(Bytes(2048)),
            memory_limit: Some(Bytes(4096)),
            source: UsageSource::MetricsServer,
            pods_measured: 1,
            pods_total: 1,
            truncated: false,
        }
    }

    #[test]
    fn identical_poll_replies_do_not_publish_but_real_usage_changes_do() {
        let mut state = UsageState::default();
        let generation = state.restart();
        let first = UsageOutcome::Usage(sample());
        assert!(state.publish(generation, first.clone()));
        assert!(!state.publish(generation, first.clone()));
        let changed = UsageOutcome::Usage(UsageSample {
            cpu: Some(Millicores(11)),
            ..sample()
        });
        assert!(state.publish(generation, changed.clone()));
        assert!(!state.publish(generation, changed));
        assert!(state.publish(generation, UsageOutcome::Denied("pod metrics")));
        assert!(!state.publish(generation, UsageOutcome::Denied("pod metrics")));
        assert!(state.publish(generation, first));
    }

    #[test]
    fn a_hidden_or_replaced_poll_cannot_land_after_a_new_poll_starts() {
        let mut state = UsageState::default();
        let old = state.restart();
        assert!(state.publish(old, UsageOutcome::Usage(sample())));
        let hidden = state.restart();
        assert!(state.outcome().is_none());
        assert!(!state.publish(old, UsageOutcome::Failed("late old reply".into())));
        let visible = state.restart();
        assert!(!state.publish(hidden, UsageOutcome::Usage(sample())));
        assert!(state.publish(visible, UsageOutcome::Usage(sample())));
    }

    #[test]
    fn source_coverage_requests_limits_and_failure_sentences_are_visible_changes() {
        let mut state = UsageState::default();
        let generation = state.restart();
        assert!(state.publish(generation, UsageOutcome::Usage(sample())));
        for changed in [
            UsageSample {
                memory: Some(Bytes(1025)),
                ..sample()
            },
            UsageSample {
                cpu: None,
                ..sample()
            },
            UsageSample {
                source: UsageSource::Kubelet,
                ..sample()
            },
            UsageSample {
                cpu_request: None,
                ..sample()
            },
            UsageSample {
                cpu_limit: None,
                ..sample()
            },
            UsageSample {
                memory_request: None,
                ..sample()
            },
            UsageSample {
                memory_limit: None,
                ..sample()
            },
            UsageSample {
                pods_measured: 0,
                ..sample()
            },
            UsageSample {
                pods_total: 2,
                ..sample()
            },
            UsageSample {
                truncated: true,
                ..sample()
            },
        ] {
            assert!(state.publish(generation, UsageOutcome::Usage(changed)));
            assert!(state.publish(generation, UsageOutcome::Usage(sample())));
        }
        for outcome in [
            UsageOutcome::Absent("metrics API unavailable".into()),
            UsageOutcome::Denied("kubelet summary"),
            UsageOutcome::Failed("server connection failed".into()),
            UsageOutcome::Failed("server response timed out".into()),
        ] {
            assert!(state.publish(generation, outcome.clone()));
            assert!(!state.publish(generation, outcome));
        }
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
