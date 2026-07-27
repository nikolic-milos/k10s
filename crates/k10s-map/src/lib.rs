mod bench;
mod colors;
mod frame;
mod hex;
mod lod;
#[cfg(test)]
mod oracle_test;

use std::cell::RefCell;
use std::rc::Rc;

use crossbeam_channel::Sender;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{
    App, Bounds, Context, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, Render, ScrollWheelEvent, SharedString, TextAlign,
    TextRun, TransformationMatrix, Window, canvas, div, point, prelude::*, px, quad, rgb, size,
};
use k10s_atlas::{DrawnCounts, FramePacer, FrameSpans, FrameStats, StageMachine};
use k10s_core::{KindId, SceneSnapshot, SharedScene, ToolId, WorldCtrl};

pub use bench::{BenchMeta, BenchReport};
pub use frame::FrameOpts;
pub use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
pub use lod::{cull, stage_for_zoom};

#[cfg(feature = "testing")]
pub mod testing {
    pub use crate::frame::{FramePaths, FrameSink, IconJob, LabelJob, PaintSink, walk};
}

use bench::{Bench, BenchFrame};
use colors::*;
use frame::{IconJob, LabelJob, PaintSink};
use lod::lod;

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

    pacer: FramePacer,
    stage: StageMachine,
    last_stage_tick: Option<std::time::Instant>,
    bench: Option<Bench>,
}

impl MapView {
    pub fn new(
        scene: SharedScene,
        ctrl: Sender<WorldCtrl>,
        bench: Option<BenchMeta>,
        damage: UnboundedReceiver<()>,
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
            pacer: FramePacer::default(),
            stage: StageMachine::new(lod::STAGE_FADE_SECS),
            last_stage_tick: None,
            bench: bench.map(Bench::new),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
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
            let frame = bench.frame(now, vw, vh, active, &scene, &mut self.stats.borrow_mut());
            if frame.needs_frame() {
                self.pacer.request_frame();
            }
            match frame {
                BenchFrame::Camera(cam) => self.camera = cam,
                BenchFrame::Waiting => {}
                BenchFrame::Idle { camera, arm_timer } => {
                    self.camera = camera;
                    if let Some(delay) = arm_timer {
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(delay).await;
                            this.update(cx, |_, cx| cx.notify()).ok();
                        })
                        .detach();
                    }
                }
                BenchFrame::Done => cx.quit(),
            }
        } else if scene.rev > 0 && (!self.fitted || (!self.interacted && (vw, vh) != self.last_vp))
        {
            self.camera.fit(scene.bounds, vw, vh);
            self.fitted = true;
            self.last_vp = (vw, vh);
        }

        let dt = self
            .last_stage_tick
            .map_or(0.0, |t| (now - t).as_secs_f32());
        self.last_stage_tick = Some(now);
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
        let edges_on = self.edges_on;
        let churn_on = self.churn_on;
        let hud_on = self.hud_on;

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, _| {
                    this.drag = Some(ev.position);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.drag = None;
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let Some(last) = this.drag {
                    let dx = f32::from(ev.position.x - last.x);
                    let dy = f32::from(ev.position.y - last.y);
                    this.camera.pan_px(dx, dy);
                    this.drag = Some(ev.position);
                    this.interacted = true;
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, window, cx| {
                let dy = f32::from(ev.delta.pixel_delta(px(24.0)).y);
                let factor = (dy * 0.0035).exp();
                let (vw, vh) = Self::viewport(window);
                this.camera.zoom_around(
                    factor,
                    f32::from(ev.position.x),
                    f32::from(ev.position.y),
                    vw,
                    vh,
                );
                this.interacted = true;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "c" => {
                        this.churn_on = !this.churn_on;
                        let _ = this.ctrl.send(WorldCtrl::SetChurn(this.churn_on));
                    }
                    "e" => this.edges_on = !this.edges_on,
                    "h" => this.hud_on = !this.hud_on,
                    "f" => {
                        let scene = this.scene.load();
                        let (vw, vh) = Self::viewport(window);
                        this.camera.fit(scene.bounds, vw, vh);
                    }
                    _ => return,
                }
                cx.notify();
            }))
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
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
                            edges_on,
                            churn_on,
                            hud_on,
                            was_continuous,
                            animating,
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
    *SKIP.get_or_init(|| std::env::var_os("K10S_SKIP_WL").is_some())
}

fn repaint_always() -> bool {
    static ALWAYS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALWAYS.get_or_init(|| std::env::var_os("K10S_REPAINT_ALWAYS").is_some())
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
    bounds: Bounds<Pixels>,
    scene: &SceneSnapshot,
    camera: Camera,
    blend: StageBlend,
    stats: &Rc<RefCell<FrameStats>>,
    bg_buf: &Rc<RefCell<Vec<PaintQuad>>>,
    fg_buf: &Rc<RefCell<Vec<PaintQuad>>>,
    label_buf: &Rc<RefCell<Vec<LabelJob>>>,
    icon_buf: &Rc<RefCell<Vec<IconJob>>>,
    edges_on: bool,
    churn_on: bool,
    hud_on: bool,
    was_continuous: bool,
    animating: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let frame_start = std::time::Instant::now();
    stats.borrow_mut().begin_frame(frame_start, was_continuous);
    let mut bg = bg_buf.borrow_mut();
    let mut fg = fg_buf.borrow_mut();
    let mut labels = label_buf.borrow_mut();
    let mut icons = icon_buf.borrow_mut();

    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let zoom = camera.zoom;
    let block_alpha = blend.stage_alpha(1);
    let cell_alpha = blend.stage_alpha(2);

    let opts = FrameOpts {
        policy: lod(),
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
            window.paint_path(path, rgb(HEX_LINE).alpha(hex::level(zoom).1));
        }
    }

    if counts.edges > 0 {
        let edges = paths.edges.build();
        debug_assert!(edges.is_ok(), "edge layer failed to tessellate");
        if let Ok(path) = edges {
            window.paint_path(path, rgb(EDGE).alpha(0.30 * cell_alpha));
        }
    }

    if counts.curves > 0 {
        if paths.glow {
            let glow = paths.curve_glow.build();
            debug_assert!(glow.is_ok(), "curve glow layer failed to tessellate");
            if let Ok(path) = glow {
                window.paint_path(path, rgb(CURVE_GLOW).alpha(CURVE_GLOW_ALPHA * cell_alpha));
            }
        }
        let core = paths.curve_core.build();
        debug_assert!(core.is_ok(), "curve core layer failed to tessellate");
        if let Ok(path) = core {
            window.paint_path(path, rgb(CURVE_CORE).alpha(CURVE_CORE_ALPHA * cell_alpha));
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
                            scale_alpha(tool_color(*tool), 0.95 * block_alpha).into(),
                        )
                    }
                    IconJob::Sat(kind, b) => {
                        let (key, data) = kind_icon(*kind);
                        (
                            key,
                            data,
                            *b,
                            scale_alpha(kind_color(*kind), cell_alpha).into(),
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
    let font = gpui::font("Noto Sans");
    let mut label_counts = LabelCounts::default();
    for job in labels.iter() {
        let run = TextRun {
            len: job.text.len(),
            font: font.clone(),
            color: job.color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(job.text.clone(), px(job.size_px), &[run], None);
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
        st.end_cpu(frame_start);
    }

    let hud_start = std::time::Instant::now();
    paint_hud(
        scene,
        stats,
        camera.zoom,
        blend,
        edges_on,
        churn_on,
        hud_on,
        animating,
        ox,
        oy,
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
    stats: &Rc<RefCell<FrameStats>>,
    zoom: f32,
    blend: StageBlend,
    edges_on: bool,
    churn_on: bool,
    hud_on: bool,
    animating: bool,
    ox: f32,
    oy: f32,
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
    let lines = [
        format!(
            "k10s starmap [rev {}]  {} ns / {} wl / {} pods / {} sats / {} edges",
            scene.rev,
            group(t.regions),
            group(t.blocks),
            group(t.cells),
            group(t.sats),
            group(t.edges),
        ),
        format!(
            "frame  p50 {fp50:.1}  p95 {fp95:.1}  p99 {fp99:.1} ms   (~{:.0} {})",
            if fp50 > 0.0 { 1000.0 / fp50 } else { 0.0 },
            if animating { "fps" } else { "paints/s" },
        ),
        format!("paint cpu  p50 {cp50:.2}  p99 {cp99:.2} ms"),
        format!(
            "zoom {zoom:.3}  stage {}  |  quads {}  lines {}  glyphs {}  icons {}  edges {}  dropped {}L/{}I",
            if blend.is_settled() {
                format!("Z{}", blend.to)
            } else {
                format!("Z{}>Z{}", blend.from, blend.to)
            },
            group(st.quads as u32),
            st.lines,
            group(st.glyphs as u32),
            st.icons,
            st.edges,
            st.labels_dropped,
            st.icons_dropped,
        ),
        format!(
            "sats {}  curves {}{}  hex {}  |  drawn ns {} wl {} pods {}",
            st.sats,
            st.curves,
            if st.curves_dropped > 0 {
                format!(" (-{})", st.curves_dropped)
            } else {
                String::new()
            },
            st.bg_cells,
            st.drawn.regions,
            st.drawn.blocks,
            group(st.drawn.cells as u32),
        ),
        format!(
            "[c]hurn {}  [e]dges {}  [f]it  [h]ide",
            if churn_on { "on" } else { "off" },
            if edges_on { "on" } else { "off" },
        ),
    ];
    drop(st);

    let pad = 10.0;
    let line_h = 16.0;
    let hud_bounds = Bounds {
        origin: point(px(ox + 12.0), px(oy + 12.0)),
        size: size(px(560.0), px(2.0 * pad + line_h * lines.len() as f32)),
    };
    window.paint_quad(quad(
        hud_bounds,
        px(6.0),
        rgb(HUD_BG).alpha(0.88),
        px(1.0),
        rgb(NS_BORDER),
        Default::default(),
    ));

    let font = gpui::font("JetBrains Mono");
    for (i, text) in lines.iter().enumerate() {
        let s = SharedString::from(text.as_str());
        let run = TextRun {
            len: s.len(),
            font: font.clone(),
            color: rgb(HUD_TEXT).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window.text_system().shape_line(s, px(11.0), &[run], None);
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

fn group(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::LabelCounts;

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
}
