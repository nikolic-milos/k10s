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
mod frame;
mod hex;
mod lod;
#[cfg(test)]
mod oracle_test;
mod pick;
mod text;

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;

use crossbeam_channel::Sender;
use futures::StreamExt as _;
use futures::channel::mpsc::Receiver;
use gpui::{
    App, Bounds, Context, FocusHandle, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, Pixels, Point, Render, ScrollWheelEvent, SharedString, TextAlign, TextRun,
    TransformationMatrix, Window, canvas, div, fill, point, prelude::*, px, quad, rgb, size,
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
}

impl gpui::EventEmitter<Picked> for MapView {}

gpui::actions!(k10s_map, [ToggleChurn, ToggleEdges, ToggleHud, FitView]);

// The map's own commands, bound in its own context so the shell's letters
// never collide with them and a user keymap can rebind them by name.
pub fn keybindings() -> Vec<gpui::KeyBinding> {
    let map = Some("Map");
    vec![
        gpui::KeyBinding::new("c", ToggleChurn, map),
        gpui::KeyBinding::new("e", ToggleEdges, map),
        gpui::KeyBinding::new("h", ToggleHud, map),
        gpui::KeyBinding::new("f", FitView, map),
    ]
}
pub use frame::FrameOpts;
pub use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
pub use lod::{cull, stage_for_zoom};
pub use pick::{PickPath, pick};

#[cfg(feature = "testing")]
pub mod testing {
    pub use crate::frame::{FramePaths, FrameSink, IconJob, LabelJob, PaintSink, walk};
}

use bench::{Bench, BenchOp};
use frame::{IconJob, LabelJob, PaintSink};
use k10s_theme::scale_alpha;
use lod::lod;
use text::TextCache;

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
    fitted: bool,

    interacted: bool,
    last_vp: (f32, f32),
    focus_handle: FocusHandle,
    stats: Rc<RefCell<FrameStats>>,
    bg_buf: Rc<RefCell<Vec<PaintQuad>>>,
    fg_buf: Rc<RefCell<Vec<PaintQuad>>>,
    label_buf: Rc<RefCell<Vec<LabelJob>>>,
    icon_buf: Rc<RefCell<Vec<IconJob>>>,
    text_cache: Rc<RefCell<TextCache>>,

    pacer: FramePacer,
    stage: StageMachine,
    last_stage_tick: Option<std::time::Instant>,
    bench: Option<Bench>,
    // The flight in progress, if any. `None` is the whole idle case: no flight
    // means no frame requested on its account, which is what keeps the measured
    // zero paints at idle true through an animation rather than despite one.
    fly: Option<FlyTo>,
    motion: Motion,
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
    pub fn new(
        scene: SharedScene,
        ctrl: Sender<WorldCtrl>,
        bench: Option<BenchMeta>,
        damage: Receiver<()>,
        cx: &mut Context<Self>,
    ) -> Self {
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
            edges_on: true,
            hud_on: true,
            fitted: false,
            interacted: false,
            last_vp: (0.0, 0.0),
            focus_handle: cx.focus_handle(),
            stats: Rc::new(RefCell::new(FrameStats::default())),
            bg_buf: Rc::new(RefCell::new(Vec::new())),
            fg_buf: Rc::new(RefCell::new(Vec::new())),
            label_buf: Rc::new(RefCell::new(Vec::new())),
            icon_buf: Rc::new(RefCell::new(Vec::new())),
            text_cache: Rc::new(RefCell::new(TextCache::default())),
            pacer: FramePacer::default(),
            fly: None,
            motion: Motion::Animate,
            stage: StageMachine::new(lod::STAGE_FADE_SECS),
            last_stage_tick: None,
            bench: bench.map(Bench::new),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
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
            self.fly_to(target);
        }
        cx.notify();
    }

    /// Send the camera somewhere, from wherever it is now.
    ///
    /// Retargets an existing flight instead of replacing it, so a second
    /// destination while the first is still being flown to continues the
    /// movement rather than snapping back to where the first one began. Marks the
    /// camera as touched, because a flight is a camera the user chose and the
    /// automatic fit must not overrule it mid-air.
    pub fn fly_to(&mut self, target: Camera) {
        match self.fly.as_mut() {
            Some(flight) => flight.retarget(target, self.motion),
            None => self.fly = Some(FlyTo::new(self.camera, target, self.motion)),
        }
        self.fitted = true;
        self.interacted = true;
    }

    /// Whether this window animates. Reduced still arrives -- on the next frame,
    /// which is still painted -- so no caller has to branch on it.
    pub fn set_motion(&mut self, motion: Motion) {
        self.motion = motion;
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
        self.camera = camera;
        self.fitted = true;
        self.interacted = true;
        self.stage = StageMachine::new(0.0);
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

    fn emit_pick(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let rect = self.map_bounds.get();
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }
        let snapshot = self.scene.load_full();
        if snapshot.rev == 0 {
            return;
        }
        // A click resolves at the stage the zoom is settling toward; a fade
        // lasts 180 ms and picking mid-fade should answer for where the user
        // is going, not where the crossfade happens to be.
        let policy = lod();
        let blend = StageBlend::settled(policy.stage_for_zoom(self.camera.zoom));
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
            return;
        };
        cx.emit(Picked { snapshot, path });
    }

    fn viewport(window: &Window) -> (f32, f32) {
        let vp = window.viewport_size();
        (f32::from(vp.width), f32::from(vp.height))
    }
}

impl Render for MapView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scene = self.scene.load_full();
        let (vw, vh) = Self::viewport(window);

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
        let blend = self.stage.update(lod(), self.camera.zoom, dt);
        if self.stage.animating() {
            self.pacer.request_frame();
        }

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

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, _| {
                    this.drag = Some(ev.position);
                    this.drag_total = 0.0;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                    let clicked = this.drag.take().is_some() && this.drag_total < 4.0;
                    if clicked {
                        this.emit_pick(ev.position, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let Some(last) = this.drag {
                    let dx = f32::from(ev.position.x - last.x);
                    let dy = f32::from(ev.position.y - last.y);
                    this.drag_total += dx.abs() + dy.abs();
                    this.camera.pan_px(dx, dy);
                    this.drag = Some(ev.position);
                    this.interacted = true;
                    cx.notify();
                }
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
                this.interacted = true;
                cx.notify();
            }))
            .key_context("Map")
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        map_bounds.set(k10s_core::Rect::new(
                            f32::from(bounds.origin.x),
                            f32::from(bounds.origin.y),
                            f32::from(bounds.size.width),
                            f32::from(bounds.size.height),
                        ));
                        paint_map(
                            bounds,
                            &scene,
                            camera,
                            blend,
                            &stats,
                            &bg_buf,
                            &fg_buf,
                            &label_buf,
                            &icon_buf,
                            &text_cache,
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
mod letterbox_tests {
    use super::*;

    #[test]
    fn letterbox_keeps_logical_size_and_centers() {
        let canvas = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(1920.0), px(1200.0)),
        };
        let box_ = letterbox_bounds(canvas);
        assert_eq!(f32::from(box_.size.width), FLIGHT_VIEWPORT[0]);
        assert_eq!(f32::from(box_.size.height), FLIGHT_VIEWPORT[1]);
        assert!((f32::from(box_.origin.x) - 160.0).abs() < 1e-3);
        assert!((f32::from(box_.origin.y) - 100.0).abs() < 1e-3);
    }

    #[test]
    fn letterbox_origin_may_go_negative_on_a_smaller_window() {
        let canvas = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(1512.0), px(837.0)),
        };
        let box_ = letterbox_bounds(canvas);
        assert_eq!(f32::from(box_.size.width), FLIGHT_VIEWPORT[0]);
        assert_eq!(f32::from(box_.size.height), FLIGHT_VIEWPORT[1]);
        assert!(f32::from(box_.origin.x) < 0.0);
        assert!(f32::from(box_.origin.y) < 0.0);
    }
}

fn repaint_always() -> bool {
    static ALWAYS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALWAYS.get_or_init(|| std::env::var_os("K10S_REPAINT_ALWAYS").is_some_and(|v| v != "0"))
}

fn glow_on() -> bool {
    static GLOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GLOW.get_or_init(|| std::env::var_os("K10S_NO_GLOW").is_none_or(|v| v == "0"))
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

    let icons_start = std::time::Instant::now();
    if !icons.is_empty() {
        let wl_icon_color: gpui::Hsla = gpui::Rgba {
            r: 0.62,
            g: 0.58,
            b: 0.78,
            a: 0.9 * block_alpha,
        }
        .into();
        window.paint_layer(bounds, |window| {
            for job in icons.iter() {
                let (key, data, icon_bounds, color) = match job {
                    IconJob::Wl(kind, b) => {
                        let (key, data) = kind_icon(*kind);
                        (key, data, *b, wl_icon_color)
                    }
                    IconJob::ToolId(tool, b) => {
                        let (key, data) = tool_icon(*tool);
                        (
                            key,
                            data,
                            *b,
                            scale_alpha(theme.map.tool_color(*tool), 0.95 * block_alpha).into(),
                        )
                    }
                    IconJob::Sat(kind, b) => {
                        let (key, data) = kind_icon(*kind);
                        (
                            key,
                            data,
                            *b,
                            scale_alpha(theme.map.kind_color(*kind), cell_alpha).into(),
                        )
                    }
                };
                let _ = window.paint_svg(
                    icon_bounds,
                    key,
                    Some(data),
                    TransformationMatrix::unit(),
                    color,
                    cx,
                );
            }
        });
    }

    let text_start = std::time::Instant::now();
    let font = gpui::font(typography.ui_family.clone());
    let mut label_counts = LabelCounts::default();
    let mut cache = text_cache.borrow_mut();
    let cache_before = cache.stats();
    for job in labels.iter() {
        let line = cache.shape_label(
            job.text.clone(),
            &font,
            job.size_px,
            job.color.into(),
            blend.is_settled(),
            window.text_system(),
        );
        if line
            .paint(
                point(px(job.x), px(job.y)),
                px(job.size_px * 1.4),
                TextAlign::Left,
                None,
                window,
                cx,
            )
            .is_ok()
        {
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
mod tests {
    use super::{LabelCounts, MapView};

    const POD_LABELS: [&str; 3] = [
        "checkout-api-7f9c8d6b5-tzq4x",
        "postgres-primary-0",
        "otel-collector-agent-vv2mn",
    ];

    #[test]
    fn label_counts_keep_lines_and_glyphs_apart() {
        let mut counts = LabelCounts::default();
        for text in POD_LABELS {
            counts.count(text);
        }
        assert_eq!(counts.lines, POD_LABELS.len());
        assert_eq!(
            counts.glyphs,
            POD_LABELS.iter().map(|t| t.chars().count()).sum::<usize>()
        );
        assert!(
            counts.glyphs >= counts.lines,
            "glyphs {} lines {}",
            counts.glyphs,
            counts.lines
        );
        assert_ne!(
            counts.glyphs, counts.lines,
            "a line counter must not be reported as a glyph counter"
        );
    }

    #[test]
    fn label_counts_measure_characters_not_bytes() {
        let text = "naive-wörker-0";
        let mut counts = LabelCounts::default();
        counts.count(text);
        assert_eq!(counts.lines, 1);
        assert_eq!(counts.glyphs, 14);
        assert!(counts.glyphs < text.len());
    }

    #[test]
    fn an_empty_scene_does_not_spend_the_one_automatic_fit() {
        // The incident: a window can now open on an empty world and be filled
        // from the launch screen a moment later. Fitting to nothing marked the
        // view fitted, and the starmap that arrived opened off-camera.
        assert!(!MapView::should_fit_scene(0, false, false, false));
        assert!(!MapView::should_fit_scene(0, false, false, true));
        assert!(MapView::should_fit_scene(197, false, false, false));

        // And the rules that were already true stay true.
        assert!(!MapView::should_fit_scene(197, true, false, false));
        assert!(
            MapView::should_fit_scene(197, true, false, true),
            "a resize re-frames a camera nobody has touched"
        );
        assert!(
            !MapView::should_fit_scene(197, true, true, true),
            "and never one they have"
        );
    }
}

#[cfg(test)]
mod flight_tests {
    use super::*;
    use k10s_atlas::motion::FLY_SECONDS;

    fn camera(cx: f32, cy: f32, zoom: f32) -> Camera {
        Camera { cx, cy, zoom }
    }

    #[test]
    fn no_flight_asks_for_no_frames() {
        let mut fly = None;
        let mut at = camera(1.0, 2.0, 3.0);
        assert!(!advance_flight(&mut fly, &mut at, 0.016));
        assert_eq!(
            at,
            camera(1.0, 2.0, 3.0),
            "an absent flight moved the camera"
        );
    }

    #[test]
    fn a_flight_stops_asking_the_frame_it_arrives_on() {
        let start = camera(0.0, 0.0, 0.1);
        let target = camera(400.0, 300.0, 2.0);
        let mut at = start;
        let mut fly = Some(FlyTo::new(start, target, Motion::Animate));

        let mut frames = 0;
        while advance_flight(&mut fly, &mut at, 0.016) {
            frames += 1;
            assert!(frames < 1_000, "a {FLY_SECONDS}s flight never arrived");
            assert!(
                fly.is_some(),
                "the flight was dropped while it was still owed"
            );
        }
        assert_eq!(at, target, "the flight stopped short of where it was sent");
        assert!(
            fly.is_none(),
            "an arrived flight is still holding a frame's worth of state"
        );

        // The frames after arrival are the ones the whole shape is for: a single
        // extra request here is one idle paint per fit, and `--churn 0` measures
        // exactly zero.
        for _ in 0..600 {
            assert!(
                !advance_flight(&mut fly, &mut at, 0.016),
                "an arrived flight went on asking to be painted"
            );
        }
    }

    #[test]
    fn reduced_motion_costs_one_frame_and_not_a_flight() {
        let target = camera(400.0, 300.0, 2.0);
        let mut at = camera(0.0, 0.0, 0.1);
        let mut fly = Some(FlyTo::new(at, target, Motion::Reduced));
        // One step, which the caller has already been asked to paint by whatever
        // started the flight, and no frame owed after it.
        assert!(!advance_flight(&mut fly, &mut at, 0.016));
        assert_eq!(at, target);
        assert!(fly.is_none());
    }
}
