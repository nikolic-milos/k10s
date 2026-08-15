//! The GPUI painter for the Starmap.
//!
//! `frame::walk` is the one traversal, shared by the window painter and every
//! headless sink through `FrameSink`; in debug builds each painted frame is
//! re-derived by the `k10s-atlas` cull oracle and must agree counter for
//! counter. Painting is damage-driven -- zero paints at idle under
//! `--churn 0` is a gated invariant -- and the paint path must not allocate
//! per frame at steady state: labels come from a bounded shaped-text cache
//! that only serves settled frames, and the `bench-alloc` ratchet holds the
//! walk at zero allocations.

mod bench;
mod chrome;
mod frame;
mod hex;
mod lod;
#[cfg(test)]
mod oracle_test;
mod overlay;
mod pick;
mod primitive;
mod text;

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::Instant;

use crossbeam_channel::Sender;
use futures::StreamExt as _;
use futures::channel::mpsc::Receiver;
use gpui::{
    App, Bounds, Context, FocusHandle, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, Render, ScrollWheelEvent, SharedString, TextAlign,
    TextRun, TransformationMatrix, Window, canvas, div, fill, point, prelude::*, px, quad, rgb,
    size,
};
use k10s_atlas::{
    DrawnCounts, FLIGHT_VIEWPORT, FlyTo, FramePacer, FrameSpans, FrameStats, Motion, StageMachine,
};
use k10s_core::{KindId, SceneSnapshot, SharedScene, ToolId, WorldCtrl};

pub use bench::{BenchMeta, BenchReport};

// A click resolved against the snapshot that was on screen when it landed.
// The snapshot rides along so the consumer names slots from the exact scene
// the user saw, immune to a publish racing the mouse.
pub struct Picked {
    pub snapshot: std::sync::Arc<SceneSnapshot>,
    pub path: PickPath,
    mark: Mark,
}

impl gpui::EventEmitter<Picked> for MapView {}

gpui::actions!(
    k10s_map,
    [
        ToggleChurn,
        ToggleEdges,
        ToggleHud,
        ToggleLegend,
        CycleOverlay,
        FitView,
        ZoomIn,
        ZoomOut,
    ]
);

// The map's own commands, bound in its own context so the shell's letters
// never collide with them and a user keymap can rebind them by name.
pub fn keybindings() -> Vec<gpui::KeyBinding> {
    let map = Some("Map");
    vec![
        gpui::KeyBinding::new("c", ToggleChurn, map),
        gpui::KeyBinding::new("e", ToggleEdges, map),
        gpui::KeyBinding::new("g", ToggleLegend, map),
        gpui::KeyBinding::new("h", ToggleHud, map),
        gpui::KeyBinding::new("o", CycleOverlay, map),
        gpui::KeyBinding::new("f", FitView, map),
        gpui::KeyBinding::new("=", ZoomIn, map),
        gpui::KeyBinding::new("-", ZoomOut, map),
    ]
}
pub use frame::FrameOpts;
pub use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
pub use lod::{cull, stage_for_zoom};
pub use overlay::{OverlayFrame, OverlayKind, OverlayMark};
pub use pick::{PickPath, path_rect, pick};
pub use primitive::{MarkPrimitive, mark_primitive};

type PresentCallback = Box<dyn FnOnce(Instant, &mut App)>;
type SceneReady = Box<dyn Fn(&SceneSnapshot) -> bool>;

/// One-shot observations of frames that crossed GPUI's presentation boundary.
///
/// GPUI does not expose the platform renderer's submit callback. A callback
/// armed during a draw therefore runs at the beginning of the next platform
/// frame: after the frame that armed it was submitted, and before any later
/// frame is built. Keeping this seam here lets the application measure startup
/// without putting clocks, serialization, or process policy in the painter.
#[derive(Default)]
pub struct PresentProbe {
    first: Option<PresentCallback>,
    scene: Option<PresentCallback>,
    scene_ready: Option<SceneReady>,
}

impl PresentProbe {
    pub fn first(callback: impl FnOnce(Instant, &mut App) + 'static) -> Self {
        Self {
            first: Some(Box::new(callback)),
            scene: None,
            scene_ready: None,
        }
    }

    pub fn on_scene(mut self, callback: impl FnOnce(Instant, &mut App) + 'static) -> Self {
        self.scene_ready = Some(Box::new(|scene| scene.rev != 0));
        self.scene = Some(Box::new(callback));
        self
    }

    pub fn on_scene_when(
        mut self,
        ready: impl Fn(&SceneSnapshot) -> bool + 'static,
        callback: impl FnOnce(Instant, &mut App) + 'static,
    ) -> Self {
        self.scene_ready = Some(Box::new(ready));
        self.scene = Some(Box::new(callback));
        self
    }

    fn take(&mut self, scene_ready: bool) -> ArmedPresent {
        if scene_ready {
            self.scene_ready = None;
        }
        ArmedPresent {
            first: self.first.take(),
            scene: scene_ready.then(|| self.scene.take()).flatten(),
        }
    }

    fn arm(&mut self, scene: &SceneSnapshot, window: &Window) {
        let scene_ready = self.scene_ready.as_ref().is_some_and(|ready| ready(scene));
        let callbacks = self.take(scene_ready);
        if callbacks.is_empty() {
            return;
        }
        window.on_next_frame(move |_, cx| callbacks.fire(Instant::now(), cx));
    }
}

struct ArmedPresent {
    first: Option<PresentCallback>,
    scene: Option<PresentCallback>,
}

impl ArmedPresent {
    fn is_empty(&self) -> bool {
        self.first.is_none() && self.scene.is_none()
    }

    fn fire(self, presented_at: Instant, cx: &mut App) {
        if let Some(callback) = self.first {
            callback(presented_at, cx);
        }
        if let Some(callback) = self.scene {
            callback(presented_at, cx);
        }
    }
}

#[cfg(test)]
#[path = "present_test.rs"]
mod present_tests;

#[cfg(feature = "testing")]
pub mod testing {
    pub use crate::frame::{FramePaths, FrameSink, IconJob, LabelFace, LabelJob, PaintSink, walk};
}

use bench::{Bench, BenchOp};
use frame::{IconJob, LabelFace, LabelJob, PaintSink};
use k10s_theme::scale_alpha;
use lod::lod;
use text::TextCache;

// A highlight is an index path plus the identities which made that path true.
// World slots are intentionally reused; validating the non-empty ids before a
// paint prevents a deleted pod's ring from jumping to its replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    path: PickPath,
    rev: u64,
    ids: [Option<std::sync::Arc<str>>; 4],
}

impl Mark {
    fn new(scene: &SceneSnapshot, path: PickPath) -> Option<Mark> {
        path_rect(scene, &path)?;
        let id = |ids: &k10s_core::SlotIds, slot: Option<u32>| {
            slot.and_then(|slot| ids.get(slot as usize))
                .filter(|id| !id.is_empty())
                .cloned()
        };
        Some(Mark {
            path,
            rev: scene.rev,
            ids: [
                id(&scene.ids.regions, Some(path.region)),
                id(&scene.ids.blocks, path.block),
                id(&scene.ids.cells, path.cell),
                id(&scene.ids.sats, path.sat),
            ],
        })
    }

    fn resolve(&self, scene: &SceneSnapshot) -> Option<PickPath> {
        path_rect(scene, &self.path)?;
        let current = [
            scene.ids.regions.get(self.path.region as usize),
            self.path
                .block
                .and_then(|slot| scene.ids.blocks.get(slot as usize)),
            self.path
                .cell
                .and_then(|slot| scene.ids.cells.get(slot as usize)),
            self.path
                .sat
                .and_then(|slot| scene.ids.sats.get(slot as usize)),
        ];
        let mut identified = false;
        for (expected, current) in self.ids.iter().zip(current) {
            if let Some(expected) = expected {
                identified = true;
                if current != Some(expected) {
                    return None;
                }
            }
        }
        (identified || scene.rev == self.rev).then_some(self.path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct HoverBasis {
    rev: u64,
    camera: Camera,
    viewport: (f32, f32),
    stage: u8,
}

type Glyph = (&'static str, &'static [u8]);

macro_rules! glyph {
    ($file:literal) => {
        (
            concat!("icons/", $file),
            include_bytes!(concat!("../assets/icons/", $file)) as &'static [u8],
        )
    };
}

macro_rules! tool_glyph {
    ($file:literal) => {
        (
            concat!("icons/tools/", $file),
            include_bytes!(concat!("../assets/icons/tools/", $file)) as &'static [u8],
        )
    };
}

const UNKNOWN_GLYPH: Glyph = glyph!("unknown.svg");

static KIND_GLYPHS: &[Glyph] = &[
    glyph!("deploy.svg"),
    glyph!("sts.svg"),
    glyph!("ds.svg"),
    glyph!("job.svg"),
    glyph!("job.svg"),
    glyph!("unknown.svg"),
    glyph!("unknown.svg"),
    glyph!("pvc.svg"),
    glyph!("svc.svg"),
    glyph!("cm.svg"),
    glyph!("secret.svg"),
    glyph!("svc.svg"),
    glyph!("unknown.svg"),
];

const _: () = assert!(
    KIND_GLYPHS.len() == k10s_core::BUILTIN_KIND_COUNT as usize,
    "every built-in kind needs a glyph"
);

static TOOL_GLYPHS: &[Glyph] = &[
    UNKNOWN_GLYPH,
    tool_glyph!("apacheairflow.svg"),
    tool_glyph!("argo.svg"),
    tool_glyph!("apachecassandra.svg"),
    tool_glyph!("clickhouse.svg"),
    tool_glyph!("consul.svg"),
    tool_glyph!("elasticsearch.svg"),
    tool_glyph!("envoyproxy.svg"),
    tool_glyph!("etcd.svg"),
    tool_glyph!("fluentbit.svg"),
    tool_glyph!("fluentd.svg"),
    tool_glyph!("flux.svg"),
    tool_glyph!("grafana.svg"),
    tool_glyph!("harbor.svg"),
    tool_glyph!("istio.svg"),
    tool_glyph!("jaeger.svg"),
    tool_glyph!("jenkins.svg"),
    tool_glyph!("apachekafka.svg"),
    tool_glyph!("keycloak.svg"),
    tool_glyph!("kibana.svg"),
    tool_glyph!("kubernetes.svg"),
    tool_glyph!("mariadb.svg"),
    tool_glyph!("minio.svg"),
    tool_glyph!("mongodb.svg"),
    tool_glyph!("mysql.svg"),
    tool_glyph!("natsdotio.svg"),
    tool_glyph!("nginx.svg"),
    tool_glyph!("opentelemetry.svg"),
    tool_glyph!("postgresql.svg"),
    tool_glyph!("prometheus.svg"),
    tool_glyph!("rabbitmq.svg"),
    tool_glyph!("redis.svg"),
    tool_glyph!("temporal.svg"),
    tool_glyph!("traefikproxy.svg"),
    tool_glyph!("vault.svg"),
];

const _: () = assert!(
    TOOL_GLYPHS.len() == k10s_core::BUILTIN_TOOL_COUNT as usize,
    "every built-in vendor needs a glyph"
);

fn glyph_of(table: &'static [Glyph], idx: usize) -> (SharedString, &'static [u8]) {
    let (key, data) = table.get(idx).copied().unwrap_or(UNKNOWN_GLYPH);
    (SharedString::new_static(key), data)
}

fn kind_icon(kind: KindId) -> (SharedString, &'static [u8]) {
    glyph_of(KIND_GLYPHS, kind.0 as usize)
}

fn tool_icon(tool: ToolId) -> (SharedString, &'static [u8]) {
    glyph_of(TOOL_GLYPHS, tool.0 as usize)
}

pub struct MapView {
    scene: SharedScene,
    ctrl: Sender<WorldCtrl>,
    camera: Camera,
    drag: Option<Point<Pixels>>,
    drag_total: f32,
    map_bounds: Rc<Cell<k10s_core::Rect>>,
    churn_on: bool,
    edges_on: bool,
    hud_on: bool,
    legend_on: bool,
    fitted: bool,

    interacted: bool,
    // What the pointer is over and what the shell has selected. Both are drawn
    // OUTSIDE `frame::walk`, from the path and the camera alone, so neither
    // appears in `CullStats`, neither has to be mirrored in the cull oracle,
    // and neither can move a benchmark's structural counters. An affordance
    // that costs the engine nothing is an affordance that can be as generous as
    // it likes.
    hovered: Option<Mark>,
    selected: Option<Mark>,
    hover_position: Option<Point<Pixels>>,
    hover_basis: Option<HoverBasis>,
    last_vp: (f32, f32),
    focus_handle: FocusHandle,
    stats: Rc<RefCell<FrameStats>>,
    bg_buf: Rc<RefCell<Vec<PaintQuad>>>,
    fg_buf: Rc<RefCell<Vec<PaintQuad>>>,
    label_buf: Rc<RefCell<Vec<LabelJob>>>,
    icon_buf: Rc<RefCell<Vec<IconJob>>>,
    text_cache: Rc<RefCell<TextCache>>,
    summary: chrome::SummaryCache,
    chrome: gpui::Entity<chrome::Chrome>,
    chrome_state: Option<chrome::State>,
    overlay: OverlayFrame,
    present_probe: PresentProbe,

    pacer: FramePacer,
    stage: StageMachine,
    displayed_stage: u8,
    last_stage_tick: Option<std::time::Instant>,
    bench: Option<Bench>,
    // The flight in progress, if any. `None` is the whole idle case: no flight
    // means no frame requested on its account, which is what keeps the measured
    // zero paints at idle true through an animation rather than despite one.
    fly: Option<FlyTo>,
    // Set when a bench flight gave up, read by the binary after the app loop
    // returns. A view cannot choose a process's exit status and should not try.
    bench_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Advance a flight by one frame: where that leaves the camera, and whether the
/// caller must ask to be painted again.
///
/// Free rather than a method, and holding no gpui type, because it is the only
/// part of flying that can be wrong. A view that steps its own animation inside
/// a render body can only be tested by painting, and the property worth testing
/// -- that a finished flight stops asking for frames -- is exactly the one a
/// paint-based test is worst at noticing.
fn advance_flight(fly: &mut Option<FlyTo>, camera: &mut Camera, dt: f32) -> bool {
    let Some(flight) = fly.as_mut() else {
        return false;
    };
    let step = flight.step(dt);
    *camera = step.camera();
    if step.owes_a_frame() {
        return true;
    }
    // Dropped on arrival rather than left finished. A flight nobody is holding
    // cannot be stepped again by a later change to this function, and `None` is
    // the same idle state the view starts in.
    *fly = None;
    false
}

impl MapView {
    /// `bench_failed` is set if a bench flight gives up, and read by the binary
    /// after the app loop returns: a view is the wrong place to decide a
    /// process's exit status, and the wrong place to leave from.
    pub fn new(
        scene: SharedScene,
        ctrl: Sender<WorldCtrl>,
        bench: Option<BenchMeta>,
        bench_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        damage: Receiver<()>,
        cx: &mut Context<Self>,
    ) -> Self {
        let measuring = bench.is_some();
        let map_chrome = cx.new(|_| chrome::Chrome::default());
        cx.spawn(async move |this, cx| {
            let mut damage = damage;
            while damage.next().await.is_some() {
                while damage.try_recv().is_ok() {}
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            scene,
            ctrl,
            camera: Camera::default(),
            drag: None,
            drag_total: 0.0,
            map_bounds: Rc::new(Cell::new(k10s_core::Rect::ZERO)),
            churn_on: true,
            // Dense topology is opt-in for a person: it is useful during an
            // investigation and visual noise at rest. Scripted flights retain
            // the historical surface their baselines measure.
            edges_on: measuring,
            hud_on: measuring,
            legend_on: !measuring,
            fitted: false,
            interacted: false,
            hovered: None,
            selected: None,
            hover_position: None,
            hover_basis: None,
            last_vp: (0.0, 0.0),
            focus_handle: cx.focus_handle(),
            stats: Rc::new(RefCell::new(FrameStats::default())),
            bg_buf: Rc::new(RefCell::new(Vec::new())),
            fg_buf: Rc::new(RefCell::new(Vec::new())),
            label_buf: Rc::new(RefCell::new(Vec::new())),
            icon_buf: Rc::new(RefCell::new(Vec::new())),
            text_cache: Rc::new(RefCell::new(TextCache::default())),
            summary: chrome::SummaryCache::default(),
            chrome: map_chrome,
            chrome_state: None,
            overlay: OverlayFrame::default(),
            present_probe: PresentProbe::default(),
            pacer: FramePacer::default(),
            fly: None,
            bench_failed,
            stage: StageMachine::new(lod::STAGE_FADE_SECS),
            displayed_stage: lod().stage_for_zoom(Camera::default().zoom),
            last_stage_tick: None,
            bench: bench.map(Bench::new),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// The published scene the map is drawing. Search builds an index from
    /// this, never from a paint walk.
    pub fn snapshot(&self) -> std::sync::Arc<SceneSnapshot> {
        self.scene.load_full()
    }

    pub fn with_present_probe(mut self, probe: PresentProbe) -> Self {
        self.present_probe = probe;
        self
    }

    /// Fixed map furniture is hosted beside the canvas by the workspace. That
    /// gives both halves their own reactive boundary: camera frames rebuild the
    /// map, while a semantic-band or hover change rebuilds this view alone.
    pub fn chrome_view(&self) -> gpui::AnyView {
        self.chrome.clone().into()
    }

    /// Stamp overlay marks onto the map. Empty is the first-paint case: no
    /// overlay, not a hole. Replacing the frame notifies; identical frames do
    /// not.
    pub fn set_overlay(&mut self, overlay: OverlayFrame, cx: &mut Context<Self>) {
        if self.overlay == overlay {
            return;
        }
        self.overlay = overlay;
        cx.notify();
    }

    /// The stamps the finder can index. Search reads this table, not the
    /// paint walk, so an overlay query does not wait on a frame.
    pub fn overlay(&self) -> &OverlayFrame {
        &self.overlay
    }

    // The map's commands, invoked by the workspace's action handlers: the
    // handlers live on the workspace element so the map's per-paint element
    // build stays lean enough for the allocation ratchet to mean something.
    pub fn toggle_churn(&mut self, cx: &mut Context<Self>) {
        self.churn_on = !self.churn_on;
        let _ = self.ctrl.send(WorldCtrl::SetChurn(self.churn_on));
        cx.notify();
    }

    pub fn toggle_edges(&mut self, cx: &mut Context<Self>) {
        self.edges_on = !self.edges_on;
        cx.notify();
    }

    pub fn toggle_hud(&mut self, cx: &mut Context<Self>) {
        self.hud_on = !self.hud_on;
        cx.notify();
    }

    pub fn toggle_legend(&mut self, cx: &mut Context<Self>) {
        self.legend_on = !self.legend_on;
        cx.notify();
    }

    pub fn zoom_in(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.zoom_by(1.35, window, cx);
    }

    pub fn zoom_out(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.zoom_by(1.0 / 1.35, window, cx);
    }

    fn zoom_by(&mut self, factor: f32, window: &Window, cx: &mut Context<Self>) {
        let (_, vw, vh) = self.map_viewport(window);
        self.camera.zoom_around(factor, vw * 0.5, vh * 0.5, vw, vh);
        self.fly = None;
        self.suppress_hover();
        self.interacted = true;
        cx.notify();
    }

    /// Stop a camera flight before Escape proceeds to selection dismissal.
    pub fn cancel_flight(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self.fly.take().is_some();
        if cancelled {
            cx.notify();
        }
        cancelled
    }

    /// Frame the whole scene, flying there rather than arriving there.
    ///
    /// A camera that jumps leaves a person to work out for themselves that what
    /// they were looking at is now somewhere else, and at Z0 on a large cluster
    /// there is nothing in the new frame to recognise. Under a bench it is still
    /// a jump: a recording's camera path is the recording, and a flight would
    /// make the frames after a fit depend on when the fit happened.
    pub fn fit(&mut self, window: &Window, cx: &mut Context<Self>) {
        let scene = self.scene.load();
        let (_, vw, vh) = self.map_viewport(window);
        let mut target = self.camera;
        target.fit(scene.bounds, vw, vh);
        if self.bench.is_some() {
            self.camera = target;
            self.fly = None;
        } else {
            // Asked of gpui rather than kept here, so the answer a person gave
            // once reaches every animation in the application instead of
            // whichever ones remembered to look at a copy of it.
            self.fly_to(target, Motion::reduced_when(cx.reduce_motion()));
        }
        self.suppress_hover();
        cx.notify();
    }

    /// Fly to the object with this uid, if the published scene has one.
    ///
    /// Answers whether it went, because "that object is not on the map" and "the
    /// map is now going there" are different sentences and the caller is the one
    /// with somewhere to say them. A uid the snapshot does not carry is the
    /// ordinary case rather than an error: a search can outlive the object it
    /// matched, and a cluster the window has since left has none of them.
    pub fn reveal(&mut self, uid: &str, window: &Window, cx: &mut Context<Self>) -> bool {
        let scene = self.scene.load();
        let Some(found) = scene.locate(uid) else {
            return false;
        };
        let (_, vw, vh) = self.map_viewport(window);
        let target = self.camera.reveal(found.rect, vw, vh);
        if self.bench.is_some() {
            self.camera = target;
            self.fly = None;
        } else {
            self.fly_to(target, Motion::reduced_when(cx.reduce_motion()));
        }
        self.suppress_hover();
        cx.notify();
        true
    }

    /// Send the camera somewhere, from wherever it is now.
    ///
    /// Retargets an existing flight instead of replacing it, so a second
    /// destination while the first is still being flown to continues the
    /// movement rather than snapping back to where the first one began. Marks the
    /// camera as touched, because a flight is a camera the user chose and the
    /// automatic fit must not overrule it mid-air.
    pub fn fly_to(&mut self, target: Camera, motion: Motion) {
        match self.fly.as_mut() {
            Some(flight) => flight.retarget(target, motion),
            None => self.fly = Some(FlyTo::new(self.camera, target, motion)),
        }
        self.fitted = true;
        self.interacted = true;
    }

    /// What the shell says is selected. The map does not decide this -- a click
    /// leaves as a `Picked` event and comes back through here -- so selection
    /// stays one fact held in one place even when it was set by the finder or
    /// by a panel rather than by the pointer.
    pub fn set_selection(&mut self, selected: Option<&Picked>, cx: &mut Context<Self>) {
        let selected = selected.map(|picked| picked.mark.clone());
        if self.selected != selected {
            self.selected = selected;
            cx.notify();
        }
    }

    // Resolve what is under the pointer, and ask for a frame only if the answer
    // changed. Notifying per move event would repaint at pointer rate and turn
    // "zero paints at idle" into "zero paints unless the mouse is in the
    // window"; notifying on change is one paint per object crossed.
    fn hover_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.hover_position = Some(position);
        let snapshot = self.scene.load_full();
        let hovered = self.pick_mark_at(&snapshot, position);
        self.hover_basis = Some(self.hover_basis(&snapshot));
        if self.hovered != hovered {
            self.hovered = hovered;
            cx.notify();
        }
    }

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        let had_hover = self.hovered.is_some();
        self.suppress_hover();
        if had_hover {
            cx.notify();
        }
    }

    fn suppress_hover(&mut self) {
        self.hover_position = None;
        self.hover_basis = None;
        self.hovered = None;
    }

    fn hover_basis(&self, scene: &SceneSnapshot) -> HoverBasis {
        let rect = self.map_bounds.get();
        HoverBasis {
            rev: scene.rev,
            camera: self.camera,
            viewport: (rect.w, rect.h),
            stage: self.displayed_stage,
        }
    }

    fn refresh_hover(&mut self, scene: &SceneSnapshot) {
        let basis = self.hover_basis(scene);
        if self.hover_basis == Some(basis) {
            return;
        }
        self.hover_basis = Some(basis);
        self.hovered = self
            .hover_position
            .and_then(|position| self.pick_mark_at(scene, position));
    }

    fn pick_mark_at(&self, scene: &SceneSnapshot, position: Point<Pixels>) -> Option<Mark> {
        let rect = self.map_bounds.get();
        if rect.w <= 0.0 || rect.h <= 0.0 || scene.rev == 0 {
            return None;
        }
        let policy = lod();
        let blend = StageBlend::settled(self.displayed_stage);
        let path = pick(
            scene,
            &self.camera,
            policy,
            blend,
            rect.w,
            rect.h,
            f32::from(position.x) - rect.x,
            f32::from(position.y) - rect.y,
        )?;
        Mark::new(scene, path)
    }

    /// Forget that the camera was ever framed, so the next scene with anything in
    /// it is framed again.
    ///
    /// A scene chosen on the launch screen is a new subject, not a moved camera:
    /// the zoom that framed a 200-namespace starmap says nothing about the cluster
    /// somebody switched to. Deliberately not a fit: the scene it is about has not
    /// arrived yet.
    pub fn refit(&mut self, cx: &mut Context<Self>) {
        self.fitted = false;
        self.interacted = false;
        cx.notify();
    }

    /// Whether this frame frames the whole scene by itself.
    ///
    /// The automatic fit happens once, and again on a resize the user has not
    /// overridden by touching the camera. What it must not do is spend that one
    /// fit on an *empty* scene: a published revision used to imply content,
    /// because the world was always built from a stream before the window opened.
    /// The launch screen made an empty world reachable, and the starmap chosen a
    /// moment later opened off-camera -- fitted to bounds that had held nothing.
    fn should_fit_scene(regions: u32, fitted: bool, interacted: bool, resized: bool) -> bool {
        regions > 0 && (!fitted || (!interacted && resized))
    }

    #[cfg(feature = "testing")]
    pub fn testing_set_camera(&mut self, camera: Camera) {
        self.camera = camera.clamped();
        self.fitted = true;
        self.interacted = true;
        self.stage = StageMachine::new(0.0);
        self.displayed_stage = lod().stage_for_zoom(self.camera.zoom);
        self.last_stage_tick = None;
    }

    #[cfg(feature = "testing")]
    pub fn testing_text_cache(&self) -> k10s_atlas::TextCacheCounts {
        self.stats.borrow().text_cache
    }

    #[cfg(feature = "testing")]
    pub fn testing_enable_text_cache(&mut self, enabled: bool) {
        self.text_cache.borrow_mut().set_enabled(enabled);
    }

    // The painted element's rect, falling back to the window before the
    // first paint. Camera math and picking are element-relative; the window
    // is only the map when nothing else is docked beside it.
    fn map_viewport(&self, window: &Window) -> ((f32, f32), f32, f32) {
        let rect = self.map_bounds.get();
        if rect.w > 0.0 && rect.h > 0.0 {
            ((rect.x, rect.y), rect.w, rect.h)
        } else {
            let (vw, vh) = Self::viewport(window);
            ((0.0, 0.0), vw, vh)
        }
    }

    fn emit_pick(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<(std::sync::Arc<SceneSnapshot>, PickPath)> {
        let rect = self.map_bounds.get();
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return None;
        }
        let snapshot = self.scene.load_full();
        if snapshot.rev == 0 {
            return None;
        }
        let policy = lod();
        let blend = StageBlend::settled(self.displayed_stage);
        let Some(path) = pick(
            &snapshot,
            &self.camera,
            policy,
            blend,
            rect.w,
            rect.h,
            f32::from(position.x) - rect.x,
            f32::from(position.y) - rect.y,
        ) else {
            return None;
        };
        let mark = Mark::new(&snapshot, path)?;
        cx.emit(Picked {
            snapshot: snapshot.clone(),
            path,
            mark,
        });
        Some((snapshot, path))
    }

    fn focus_path(
        &mut self,
        scene: &SceneSnapshot,
        path: PickPath,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rect) = path_rect(scene, &path) else {
            return;
        };
        let (_, vw, vh) = self.map_viewport(window);
        let target = self.camera.reveal(rect, vw, vh);
        self.fly_to(target, Motion::reduced_when(cx.reduce_motion()));
        self.suppress_hover();
        cx.notify();
    }

    fn viewport(window: &Window) -> (f32, f32) {
        let vp = window.viewport_size();
        (f32::from(vp.width), f32::from(vp.height))
    }
}

impl Render for MapView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scene = self.scene.load_full();
        self.present_probe.arm(&scene, window);
        let window_viewport = Self::viewport(window);
        let (_, map_width, map_height) = self.map_viewport(window);
        let (vw, vh) = if self.bench.is_some() {
            window_viewport
        } else {
            (map_width, map_height)
        };

        let was_continuous = self.pacer.begin_frame();
        if repaint_always() {
            self.pacer.request_frame();
        }

        let now = std::time::Instant::now();
        if let Some(bench) = self.bench.as_mut() {
            let active = window.is_window_active();
            let op = bench.drive(
                now,
                vw,
                vh,
                active,
                &scene,
                &mut self.stats.borrow_mut(),
                &mut self.camera,
                &mut self.pacer,
            );
            match op {
                BenchOp::Continue => {}
                BenchOp::ArmTimer(delay) => {
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(delay).await;
                        this.update(cx, |_, cx| cx.notify()).ok();
                    })
                    .detach();
                }
                BenchOp::Quit => cx.quit(),
                // Quit the same way a finished flight does, having first said the
                // run failed. Exiting from inside a render callback -- which this
                // used to do -- skips the world thread's shutdown and the data
                // plane's retirement, and the order those happen in is what lets
                // a watch parked on a full sink see a disconnect instead of a
                // deadlock.
                BenchOp::Abort => {
                    self.bench_failed
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    cx.quit();
                }
            }
        } else if Self::should_fit_scene(
            scene.totals.regions,
            self.fitted,
            self.interacted,
            (vw, vh) != self.last_vp,
        ) {
            self.camera.fit(scene.bounds, vw, vh);
            self.fitted = true;
            self.last_vp = (vw, vh);
        }

        let dt = self
            .last_stage_tick
            .map_or(0.0, |t| (now - t).as_secs_f32());
        self.last_stage_tick = Some(now);
        if advance_flight(&mut self.fly, &mut self.camera, dt) {
            self.pacer.request_frame();
        }
        // Once per frame, not inside `w2s`: that call is per entity on the
        // walk the budgets measure, and a public zoom of zero or NaN has to
        // be repaired before LOD and paint divide by it, not a million times
        // during them.
        self.camera = self.camera.clamped();
        let blend = if cx.reduce_motion() {
            self.stage.settle(lod(), self.camera.zoom)
        } else {
            self.stage.update(lod(), self.camera.zoom, dt)
        };
        self.displayed_stage = blend.walk_stage();
        if self.stage.animating() {
            self.pacer.request_frame();
        }
        self.refresh_hover(&scene);

        let animating = self.pacer.frame_requested();
        let camera = self.camera;
        let stats = self.stats.clone();
        let bg_buf = self.bg_buf.clone();
        let fg_buf = self.fg_buf.clone();
        let label_buf = self.label_buf.clone();
        let icon_buf = self.icon_buf.clone();
        let text_cache = self.text_cache.clone();
        let edges_on = self.edges_on;
        let churn_on = self.churn_on;
        let hud_on = self.hud_on;
        let bench_letterbox = self.bench.is_some();
        let map_bounds = self.map_bounds.clone();
        let marks = Marks {
            hovered: self.hovered.as_ref().and_then(|mark| mark.resolve(&scene)),
            selected: self.selected.as_ref().and_then(|mark| mark.resolve(&scene)),
        };
        if !bench_letterbox {
            let summary = self.summary.line(scene.totals);
            let state = chrome::State::resolve(chrome::Overlay {
                scene: &scene,
                camera,
                policy: lod(),
                hovered: marks.hovered,
                summary,
                edges_on,
                legend_on: self.legend_on,
                viewport: (map_width, map_height),
                map_overlay: &self.overlay,
            });
            if self.chrome_state.as_ref() != Some(&state) {
                self.chrome_state = Some(state.clone());
                // Keep entity mutation out of the render that discovered it.
                // The deferred notification refreshes the workspace-hosted
                // chrome sibling without invalidating this canvas-owning view.
                let chrome = self.chrome.clone();
                cx.defer(move |cx| {
                    chrome.update(cx, |chrome, cx| chrome.sync(state, cx));
                });
            }
        }

        let pointer = if marks.hovered.is_some() {
            gpui::CursorStyle::PointingHand
        } else {
            gpui::CursorStyle::Arrow
        };
        let paint_scene = scene.clone();
        let overlay = self.overlay.clone();

        div()
            .id("starmap-view")
            .size_full()
            .relative()
            .cursor(pointer)
            .role(gpui::Role::Application)
            .aria_label("Interactive Kubernetes Starmap")
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, _| {
                    this.drag = Some(ev.position);
                    this.drag_total = 0.0;
                    this.fly = None;
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseDownEvent, _, _| {
                    this.drag = Some(ev.position);
                    this.drag_total = 4.0;
                    this.fly = None;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                    let clicked = this.drag.take().is_some() && this.drag_total < 4.0;
                    let mut focused = false;
                    if clicked
                        && let Some((scene, path)) = this.emit_pick(ev.position, cx)
                        && ev.click_count >= 2
                    {
                        this.focus_path(&scene, path, window, cx);
                        focused = true;
                    }
                    if !focused {
                        this.hover_at(ev.position, cx);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                    this.drag = None;
                    this.hover_at(ev.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let Some(last) = this.drag {
                    let dx = f32::from(ev.position.x - last.x);
                    let dy = f32::from(ev.position.y - last.y);
                    this.drag_total += dx.abs() + dy.abs();
                    this.camera.pan_px(dx, dy);
                    this.drag = Some(ev.position);
                    this.suppress_hover();
                    this.interacted = true;
                    cx.notify();
                } else {
                    this.hover_at(ev.position, cx);
                }
            }))
            .on_mouse_exit(cx.listener(|this, _: &MouseExitEvent, _, cx| {
                this.drag = None;
                this.clear_hover(cx);
            }))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, window, cx| {
                let dy = f32::from(ev.delta.pixel_delta(px(24.0)).y);
                let factor = (dy * 0.0035).exp();
                let (origin, vw, vh) = this.map_viewport(window);
                this.camera.zoom_around(
                    factor,
                    f32::from(ev.position.x) - origin.0,
                    f32::from(ev.position.y) - origin.1,
                    vw,
                    vh,
                );
                this.fly = None;
                this.suppress_hover();
                this.interacted = true;
                cx.notify();
            }))
            .key_context("Map")
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        let next_bounds = k10s_core::Rect::new(
                            f32::from(bounds.origin.x),
                            f32::from(bounds.origin.y),
                            f32::from(bounds.size.width),
                            f32::from(bounds.size.height),
                        );
                        // Store the precise rect for picking and fit; ask for
                        // another frame only when the device-pixel grid moved.
                        // Hyprland fractional scale can rescale logical bounds
                        // by a fraction every configure, and comparing f32s
                        // would re-arm RAF forever while nothing visible
                        // changed -- which is how the idle claim dies.
                        let scale = window.scale_factor();
                        let prev = map_bounds.replace(next_bounds);
                        if device_px(prev, scale) != device_px(next_bounds, scale) {
                            window.request_animation_frame();
                        }
                        paint_map(
                            bounds,
                            &paint_scene,
                            camera,
                            blend,
                            &stats,
                            &bg_buf,
                            &fg_buf,
                            &label_buf,
                            &icon_buf,
                            &text_cache,
                            marks,
                            &overlay,
                            edges_on,
                            churn_on,
                            hud_on,
                            was_continuous,
                            animating,
                            bench_letterbox,
                            window,
                            cx,
                        );
                        if animating {
                            window.request_animation_frame();
                        }
                    },
                )
                .size_full(),
            )
    }
}

fn skip_workloads() -> bool {
    static SKIP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SKIP.get_or_init(|| std::env::var_os("K10S_SKIP_WL").is_some_and(|v| v != "0"))
}

/// The device-pixel grid a logical rect occupies at `scale`.
///
/// Used to decide whether a bounds change is worth another frame: comparing
/// the f32s themselves treats a 0.01 px Hyprland rescale as damage.
fn device_px(rect: k10s_core::Rect, scale: f32) -> [i32; 4] {
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    [
        (rect.x * s).round() as i32,
        (rect.y * s).round() as i32,
        (rect.w * s).round() as i32,
        (rect.h * s).round() as i32,
    ]
}

/// Round a paint rect onto the device pixel grid before `paint_svg`.
///
/// gpui keys its SVG atlas on the size of `snap_bounds`, which rounds each
/// edge independently. Two icons of the same ladder size at different
/// subpixel origins then become two tiles, and a fractional scale (1.25 /
/// 1.5) makes that the common case. Rounding origin and size here keeps the
/// atlas key equal to the ladder size the walk already chose. The walk is
/// untouched: only the paint quad moves.
fn snap_to_device(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    if !(scale.is_finite() && scale > 0.0) {
        return bounds;
    }
    let snap = |v: f32| px((v * scale).round() / scale);
    Bounds {
        origin: point(
            snap(f32::from(bounds.origin.x)),
            snap(f32::from(bounds.origin.y)),
        ),
        size: size(
            snap(f32::from(bounds.size.width)),
            snap(f32::from(bounds.size.height)),
        ),
    }
}

fn say_once(message: &'static str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        eprintln!("k10s: {message}");
    }
}

fn letterbox_bounds(canvas: Bounds<Pixels>) -> Bounds<Pixels> {
    let [lw, lh] = FLIGHT_VIEWPORT;
    let cw = f32::from(canvas.size.width);
    let ch = f32::from(canvas.size.height);
    let ox = f32::from(canvas.origin.x) + (cw - lw) * 0.5;
    let oy = f32::from(canvas.origin.y) + (ch - lh) * 0.5;
    Bounds {
        origin: point(px(ox), px(oy)),
        size: size(px(lw), px(lh)),
    }
}

#[cfg(test)]
#[path = "letterbox_test.rs"]
mod letterbox_tests;

fn repaint_always() -> bool {
    static ALWAYS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALWAYS.get_or_init(|| std::env::var_os("K10S_REPAINT_ALWAYS").is_some_and(|v| v != "0"))
}

fn glow_on() -> bool {
    static GLOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GLOW.get_or_init(|| std::env::var_os("K10S_NO_GLOW").is_none_or(|v| v == "0"))
}

/// The pointer and selection marks, carried into the paint closure as one
/// `Copy` value so the closure captures a fact rather than the view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Marks {
    hovered: Option<PickPath>,
    selected: Option<PickPath>,
}

impl Marks {
    fn is_empty(&self) -> bool {
        self.hovered.is_none() && self.selected.is_none()
    }
}

// How far the ring stands off the thing it marks and how thick it is, in screen
// pixels. Both are constants rather than fractions of the target: a ring is
// chrome, and chrome that scales with the camera stops reading as chrome. Its
// CORNERS are not constant -- they follow the target, or a ring round an island
// reads as a box drawn over a blob.
const MARK_INSET: f32 = 3.0;
const MARK_WIDTH: f32 = 2.0;
const MARK_HALO_ALPHA: f32 = 0.16;

// Draw whatever the pointer is on and whatever is selected, from the path and
// the camera alone.
//
// Deliberately outside `frame::walk`: the walk's counters are an exact-gated
// benchmark surface and are re-derived by an independently written cull oracle
// on every debug frame, so a ring emitted inside it would have to be reproduced
// in five cull functions and would move committed baselines. Out here it is two
// quads, O(1), and it can be as expensive to look at as it likes.
fn paint_marks(
    bounds: Bounds<Pixels>,
    scene: &SceneSnapshot,
    camera: Camera,
    stage: u8,
    marks: Marks,
    theme: &k10s_theme::MapTheme,
    window: &mut Window,
) {
    let vw = f32::from(bounds.size.width);
    let vh = f32::from(bounds.size.height);
    let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    // Selection under hover: pointing at the selected thing must still show the
    // pointer's own ring, or the map stops responding to the mouse the moment
    // the two coincide.
    for (path, color) in [
        (marks.selected, theme.selection_ring),
        (marks.hovered, theme.hover_ring),
    ] {
        let Some(path) = path else { continue };
        let Some(target) = mark_primitive(scene, path, camera, lod(), stage, (vw, vh), origin)
        else {
            continue;
        };
        let ring = target.outset(MARK_INSET);
        let stroke = rgb(color);
        window.paint_quad(quad(
            ring.bounds,
            ring.corners,
            scale_alpha(stroke, MARK_HALO_ALPHA),
            px(MARK_WIDTH),
            stroke,
            Default::default(),
        ));
    }
}

#[derive(Default, Clone, Copy)]
struct LabelCounts {
    lines: usize,
    glyphs: usize,
}

impl LabelCounts {
    fn count(&mut self, text: &str) {
        self.lines += 1;
        self.glyphs += text.chars().count();
    }
}

#[expect(clippy::too_many_arguments)]
fn paint_map(
    canvas_bounds: Bounds<Pixels>,
    scene: &SceneSnapshot,
    camera: Camera,
    blend: StageBlend,
    stats: &Rc<RefCell<FrameStats>>,
    bg_buf: &Rc<RefCell<Vec<PaintQuad>>>,
    fg_buf: &Rc<RefCell<Vec<PaintQuad>>>,
    label_buf: &Rc<RefCell<Vec<LabelJob>>>,
    icon_buf: &Rc<RefCell<Vec<IconJob>>>,
    text_cache: &Rc<RefCell<TextCache>>,
    marks: Marks,
    overlay: &OverlayFrame,
    edges_on: bool,
    churn_on: bool,
    hud_on: bool,
    was_continuous: bool,
    animating: bool,
    bench_letterbox: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let frame_start = std::time::Instant::now();
    stats.borrow_mut().begin_frame(frame_start, was_continuous);
    // One refcount bump per frame, at walk setup and never inside it: the
    // walk itself still reads plain `MapTheme` fields, which is the whole
    // reason that struct is `Copy` scalars.
    let theme = k10s_theme::active(cx).clone();
    let typography = k10s_theme::typography(cx).clone();
    let mut bg = bg_buf.borrow_mut();
    let mut fg = fg_buf.borrow_mut();
    let mut labels = label_buf.borrow_mut();
    let mut icons = icon_buf.borrow_mut();

    let bounds = if bench_letterbox {
        window.paint_quad(fill(canvas_bounds, rgb(theme.map.bg)));
        letterbox_bounds(canvas_bounds)
    } else {
        canvas_bounds
    };

    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let zoom = camera.zoom;
    let block_alpha = blend.stage_alpha(1);
    let cell_alpha = blend.stage_alpha(2);

    let opts = FrameOpts {
        policy: lod(),
        theme: &theme.map,
        type_: typography.map(),
        edges_on,
        skip_blocks: skip_workloads(),
        hex: hex::hex_on(),
    };

    let walk_start = std::time::Instant::now();
    let mut sink = PaintSink::new(&mut bg, &mut fg, &mut labels, &mut icons, glow_on());
    let counts = frame::walk(bounds, scene, camera, blend, opts, &mut sink);
    let paths = sink.into_paths();

    #[cfg(debug_assertions)]
    {
        let vw = f32::from(bounds.size.width);
        let vh = f32::from(bounds.size.height);
        debug_assert_eq!(
            lod::cull(scene, &camera, blend, vw, vh, opts),
            counts,
            "cull oracle diverged from painter"
        );
        debug_assert_eq!(
            counts.quads,
            bg.len() + fg.len(),
            "quad counter drifted from the quads actually emitted"
        );
        debug_assert_eq!(counts.labels, labels.len(), "label counter drifted");
        debug_assert_eq!(counts.icons, icons.len(), "icon counter drifted");
    }

    let bg_quads_start = std::time::Instant::now();
    window.paint_quads(&bg);

    let paths_start = std::time::Instant::now();
    if counts.bg_cells > 0 {
        let hex = paths.hex.build();
        debug_assert!(hex.is_ok(), "hex layer failed to tessellate");
        if let Ok(path) = hex {
            window.paint_path(path, rgb(theme.map.hex_line).alpha(hex::level(zoom).1));
        }
    }

    if counts.edges > 0 {
        let edges = paths.edges.build();
        debug_assert!(edges.is_ok(), "edge layer failed to tessellate");
        if let Ok(path) = edges {
            window.paint_path(path, rgb(theme.map.edge).alpha(0.30 * cell_alpha));
        }
    }

    if counts.curves > 0 {
        if paths.glow {
            let glow = paths.curve_glow.build();
            debug_assert!(glow.is_ok(), "curve glow layer failed to tessellate");
            if let Ok(path) = glow {
                window.paint_path(
                    path,
                    rgb(theme.map.curve_glow).alpha(theme.map.curve_glow_alpha * cell_alpha),
                );
            }
        }
        let core = paths.curve_core.build();
        debug_assert!(core.is_ok(), "curve core layer failed to tessellate");
        if let Ok(path) = core {
            window.paint_path(
                path,
                rgb(theme.map.curve_core).alpha(theme.map.curve_core_alpha * cell_alpha),
            );
        }
    }

    let fg_quads_start = std::time::Instant::now();
    window.paint_quads(&fg);
    // Overlay stamps sit outside `frame::walk`, the same way hover rings do:
    // a post-pass over the bounded mark table, keyed by uid, only for objects
    // the camera already has on screen. CullStats does not grow a field.
    if !overlay.marks.is_empty() {
        let vw = f32::from(bounds.size.width);
        let vh = f32::from(bounds.size.height);
        let stamps = overlay.visible_stamps(scene, camera, lod(), blend, vw, vh);
        overlay::paint_stamps(
            &stamps,
            (f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
            &theme.map,
            window,
        );
    }
    if !marks.is_empty() {
        paint_marks(
            bounds,
            scene,
            camera,
            blend.walk_stage(),
            marks,
            &theme.map,
            window,
        );
    }

    let icons_start = std::time::Instant::now();
    if !icons.is_empty() {
        let wl_icon_color: gpui::Hsla =
            scale_alpha(rgb(theme.map.wl_icon), 0.95 * block_alpha).into();
        let scale = window.scale_factor();
        window.paint_layer(bounds, |window| {
            for job in icons.iter() {
                let (key, data, icon_bounds, color) = match job {
                    IconJob::Wl(kind, primitive) => {
                        let (key, data) = kind_icon(*kind);
                        (
                            key,
                            data,
                            snap_to_device(primitive.bounds, scale),
                            wl_icon_color,
                        )
                    }
                    IconJob::ToolId(tool, primitive) => {
                        let (key, data) = tool_icon(*tool);
                        (
                            key,
                            data,
                            snap_to_device(primitive.bounds, scale),
                            scale_alpha(theme.map.tool_color(*tool), 0.95 * block_alpha).into(),
                        )
                    }
                    IconJob::Sat(kind, primitive) => {
                        let (key, data) = kind_icon(*kind);
                        (
                            key,
                            data,
                            snap_to_device(primitive.bounds, scale),
                            scale_alpha(theme.map.kind_color(*kind), cell_alpha).into(),
                        )
                    }
                };
                if window
                    .paint_svg(
                        icon_bounds,
                        key,
                        Some(data),
                        TransformationMatrix::unit(),
                        color,
                        cx,
                    )
                    .is_err()
                {
                    say_once(
                        "a map icon failed to paint; further failures this process are silent",
                    );
                }
            }
        });
    }

    let text_start = std::time::Instant::now();
    let ui_font = gpui::font(typography.ui_family.clone());
    let display_font = gpui::font(typography.display_family.clone());
    let line_factor = typography.map().line_height;
    let mut label_counts = LabelCounts::default();
    let mut cache = text_cache.borrow_mut();
    let cache_before = cache.stats();
    for job in labels.iter() {
        let font = match job.face {
            LabelFace::Ui => &ui_font,
            LabelFace::Display => &display_font,
        };
        let line = cache.shape_label(
            job.text.clone(),
            font,
            job.size_px,
            job.color.into(),
            blend.is_settled(),
            window.text_system(),
        );
        let origin = point(px(job.x), px(job.y));
        let line_height = px(job.size_px * line_factor);
        // A label with a box is centred in it and clipped to it; one without is
        // set left and runs, which is what a pod cell's name wants. Clipping is
        // a content mask rather than an ellipsis because building the elided
        // string would allocate once per label per frame, and the walk's
        // zero-allocation ratchet is worth more than three dots.
        let painted = if job.width > 0.0 {
            let mask = gpui::ContentMask {
                bounds: Bounds {
                    origin: point(origin.x, origin.y - line_height),
                    size: size(px(job.width), line_height * 3.0),
                },
            };
            // Centred while it fits, set left the moment it does not. Centring
            // an overlong name clips it at BOTH ends, and a Kubernetes name is
            // informative from the front: `payments-redis-primary` cut to
            // `ments-redis-pri` names nothing. The line is already shaped, so
            // this costs a comparison and no second pass.
            let align = if line.width() > px(job.width) {
                TextAlign::Left
            } else {
                TextAlign::Center
            };
            window.with_content_mask(Some(mask), |window| {
                line.paint(origin, line_height, align, Some(px(job.width)), window, cx)
            })
        } else {
            line.paint(origin, line_height, TextAlign::Left, None, window, cx)
        };
        if painted.is_ok() {
            label_counts.count(&job.text);
        }
    }
    let cache_delta = cache.stats().since(cache_before);
    drop(cache);
    let text_end = std::time::Instant::now();

    {
        let mut st = stats.borrow_mut();
        st.quads = counts.quads;
        st.lines = label_counts.lines;
        st.glyphs = label_counts.glyphs;
        st.edges = counts.edges;
        st.icons = counts.icons;
        st.sats = counts.drawn_sats;
        st.curves = counts.curves;
        st.curves_dropped = counts.curves_dropped;
        st.bg_cells = counts.bg_cells;
        st.drawn = DrawnCounts {
            regions: counts.drawn_regions,
            blocks: counts.drawn_blocks,
            cells: counts.drawn_cells,
        };
        st.labels_dropped = counts.labels_dropped;
        st.icons_dropped = counts.icons_dropped;
        st.text_cache.hits = cache_delta.hits;
        st.text_cache.misses = cache_delta.misses;
        st.text_cache.evictions = cache_delta.evictions;
        st.end_cpu(frame_start);
        st.commit_counters();
    }

    let hud_start = std::time::Instant::now();
    paint_hud(
        scene,
        &theme,
        &typography,
        stats,
        camera.zoom,
        blend,
        edges_on,
        churn_on,
        hud_on,
        animating,
        ox,
        oy,
        text_cache,
        window,
        cx,
    );
    let hud_end = std::time::Instant::now();

    stats.borrow_mut().push_spans(FrameSpans {
        walk_us: span_us(walk_start, bg_quads_start),
        quads_us: span_us(bg_quads_start, paths_start) + span_us(fg_quads_start, icons_start),
        paths_us: span_us(paths_start, fg_quads_start),
        icons_us: span_us(icons_start, text_start),
        text_us: span_us(text_start, text_end),
        hud_us: span_us(hud_start, hud_end),
    });
}

fn span_us(from: std::time::Instant, to: std::time::Instant) -> f32 {
    (to - from).as_secs_f32() * 1_000_000.0
}

#[expect(clippy::too_many_arguments)]
fn paint_hud(
    scene: &SceneSnapshot,
    theme: &k10s_theme::Theme,
    typography: &k10s_theme::Typography,
    stats: &Rc<RefCell<FrameStats>>,
    zoom: f32,
    blend: StageBlend,
    edges_on: bool,
    churn_on: bool,
    hud_on: bool,
    animating: bool,
    ox: f32,
    oy: f32,
    text_cache: &Rc<RefCell<TextCache>>,
    window: &mut Window,
    cx: &mut App,
) {
    if !hud_on {
        return;
    }

    let st = stats.borrow();
    let (fp50, fp95, fp99) = st.frame_percentiles();
    let (cp50, cp99) = st.cpu_percentiles();
    let t = scene.totals;
    let mut cache = text_cache.borrow_mut();
    let lines = cache.hud_lines_mut();
    for line in lines.iter_mut() {
        line.clear();
    }
    write!(
        lines[0],
        "k10s starmap [rev {}]  {} ns / {} wl / {} pods / {} sats / {} edges",
        scene.rev,
        Grouped(t.regions),
        Grouped(t.blocks),
        Grouped(t.cells),
        Grouped(t.sats),
        Grouped(t.edges),
    )
    .expect("writing to a String is infallible");
    write!(
        lines[1],
        "frame  p50 {fp50:.1}  p95 {fp95:.1}  p99 {fp99:.1} ms   (~{:.0} {})",
        if fp50 > 0.0 { 1000.0 / fp50 } else { 0.0 },
        if animating { "fps" } else { "paints/s" },
    )
    .expect("writing to a String is infallible");
    write!(
        lines[2],
        "paint cpu  p50 {cp50:.2}  p99 {cp99:.2} ms  |  text cache {}H/{}M",
        st.text_cache.hits, st.text_cache.misses,
    )
    .expect("writing to a String is infallible");
    if blend.is_settled() {
        write!(lines[3], "zoom {zoom:.3}  stage Z{}", blend.to)
    } else {
        write!(
            lines[3],
            "zoom {zoom:.3}  stage Z{}>Z{}",
            blend.from, blend.to
        )
    }
    .expect("writing to a String is infallible");
    write!(
        lines[3],
        "  |  quads {}  lines {}  glyphs {}  icons {}  edges {}  dropped {}L/{}I",
        Grouped(st.quads as u32),
        st.lines,
        Grouped(st.glyphs as u32),
        st.icons,
        st.edges,
        st.labels_dropped,
        st.icons_dropped,
    )
    .expect("writing to a String is infallible");
    write!(lines[4], "sats {}  curves {}", st.sats, st.curves)
        .expect("writing to a String is infallible");
    if st.curves_dropped > 0 {
        write!(lines[4], " (-{})", st.curves_dropped).expect("writing to a String is infallible");
    }
    write!(
        lines[4],
        "  hex {}  |  drawn ns {} wl {} pods {}",
        st.bg_cells,
        st.drawn.regions,
        st.drawn.blocks,
        Grouped(st.drawn.cells as u32),
    )
    .expect("writing to a String is infallible");
    write!(
        lines[5],
        "[c]hurn {}  [e]dges {}  [f]it  [h]ide",
        if churn_on { "on" } else { "off" },
        if edges_on { "on" } else { "off" },
    )
    .expect("writing to a String is infallible");
    drop(st);

    let pad = 10.0;
    let line_h = 16.0;
    let hud_bounds = Bounds {
        origin: point(px(ox + 12.0), px(oy + 12.0)),
        size: size(px(600.0), px(2.0 * pad + line_h * lines.len() as f32)),
    };
    window.paint_quad(quad(
        hud_bounds,
        px(6.0),
        rgb(theme.map.hud_bg).alpha(0.88),
        px(1.0),
        rgb(theme.map.ns_border),
        Default::default(),
    ));

    let font = gpui::font(typography.buffer_family.clone());
    for (i, text) in lines.iter().enumerate() {
        let hash = text::content_hash(text);
        let run = TextRun {
            len: text.len(),
            font: font.clone(),
            color: rgb(theme.map.hud_text).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window.text_system().shape_line_by_hash(
            hash,
            text.len(),
            px(11.0),
            &[run],
            None,
            || SharedString::from(text.as_str()),
        );
        let _ = line.paint(
            point(px(ox + 12.0 + pad), px(oy + 12.0 + pad + i as f32 * line_h)),
            px(line_h),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

struct Grouped(u32);

impl std::fmt::Display for Grouped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn write_grouped(value: u32, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if value < 1000 {
                write!(f, "{value}")
            } else {
                write_grouped(value / 1000, f)?;
                write!(f, ",{:03}", value % 1000)
            }
        }
        write_grouped(self.0, f)
    }
}

#[cfg(test)]
#[path = "view_test.rs"]
mod tests;

#[cfg(test)]
#[path = "view_flight_test.rs"]
mod flight_tests;
