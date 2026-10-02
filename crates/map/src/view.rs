//! Window lifecycle, input, selection and damage scheduling for the Starmap.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crossbeam_channel::Sender;
use futures::StreamExt as _;
use futures::channel::mpsc::Receiver;
use gpui::{
    Context, FocusHandle, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, Render, ScrollWheelEvent, Window, canvas, div,
    prelude::*, px,
};
use k10s_atlas::{Camera, FlyTo, FramePacer, FrameStats, Motion, StageBlend, StageMachine};
use k10s_core::{SceneSnapshot, SharedScene, WorldCtrl};

use crate::bench::{Bench, BenchOp};
use crate::frame::{IconJob, LabelJob};
use crate::lod::{self, lod};
use crate::paint::{Marks, device_px, paint_map};
use crate::present::PresentProbe;
use crate::selection::Mark;
use crate::text::TextCache;
use crate::{BenchMeta, OverlayFrame, PickPath, chrome, overlay, path_rect, pick};

// A click resolved against the snapshot that was on screen when it landed.
// The snapshot rides along so the consumer names slots from the exact scene
// the user saw, immune to a publish racing the mouse.
pub struct Picked {
    pub snapshot: std::sync::Arc<SceneSnapshot>,
    pub path: PickPath,
    mark: Mark,
}

impl gpui::EventEmitter<Picked> for MapView {}

#[derive(Debug, Clone, Copy, PartialEq)]
struct HoverBasis {
    rev: u64,
    camera: Camera,
    viewport: (f32, f32),
    stage: u8,
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
    // Behind an Rc so the canvas paint closure shares the frame instead of
    // deep-cloning up to 512 marks' strings and series every painted frame.
    overlay: Rc<OverlayFrame>,
    overlay_cache: Rc<RefCell<overlay::StampCache>>,
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
            overlay: Rc::new(OverlayFrame::default()),
            overlay_cache: Rc::new(RefCell::new(overlay::StampCache::default())),
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
        if *self.overlay == overlay {
            return;
        }
        self.overlay = Rc::new(overlay);
        // New marks may resolve to different slots; the paint path trusts the
        // stamp cache until told otherwise.
        self.overlay_cache.borrow_mut().invalidate();
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
        let overlay = Rc::clone(&self.overlay);
        let overlay_cache = self.overlay_cache.clone();

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
                            &overlay_cache,
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

fn repaint_always() -> bool {
    static ALWAYS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALWAYS.get_or_init(|| std::env::var_os("K10S_REPAINT_ALWAYS").is_some_and(|v| v != "0"))
}

#[cfg(test)]
#[path = "view_test.rs"]
mod tests;

#[cfg(test)]
#[path = "view_flight_test.rs"]
mod flight_tests;
