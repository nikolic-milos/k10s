mod bench;
mod colors;
mod hex;
mod lod;

use std::cell::RefCell;
use std::rc::Rc;

use crossbeam_channel::Sender;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{
    App, Bounds, Context, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, PathBuilder, Pixels, Point, Render, ScrollWheelEvent, SharedString,
    TextAlign, TextRun, TransformationMatrix, Window, canvas, div, fill, point, prelude::*, px,
    quad, rgb, size,
};
use k10s_atlas::curves::{bow_jitter, curve_ctrl, dash_quadratic};
use k10s_atlas::{FramePacer, FrameSpans, FrameStats, StageMachine};
use k10s_core::layout::CARD_HEADER;
use k10s_core::{Health, Rect, SatKind, SceneSnapshot, SharedScene, Tool, WorkloadKind, WorldCtrl};

pub use bench::{BenchMeta, BenchReport};
pub use k10s_atlas::{Camera, CullStats, StageBlend};
pub use lod::{cull, stage_for_zoom};

use bench::{Bench, BenchFrame};
use colors::*;
use lod::lod;

const NS_LABEL_PX: f32 = 13.0;
const WL_LABEL_PX: f32 = 11.0;
const POD_LABEL_PX: f32 = 10.0;
const SAT_NAME_PX: f32 = 9.5;
const SAT_DETAIL_PX: f32 = 8.5;

const WL_ICON_PX: f32 = 12.0;
const SAT_ICON_PX: f32 = 15.0;

const CURVE_DASH_ON: f32 = 6.0;
const CURVE_DASH_OFF: f32 = 5.0;
const CURVE_TOL: f32 = 0.35;
const CURVE_CORE_W: f32 = 1.5;
const CURVE_GLOW_W: f32 = 5.0;

fn kind_icon(kind: WorkloadKind) -> (SharedString, &'static [u8]) {
    match kind {
        WorkloadKind::Deployment => (
            SharedString::new_static("icons/deploy.svg"),
            include_bytes!("../assets/icons/deploy.svg"),
        ),
        WorkloadKind::StatefulSet => (
            SharedString::new_static("icons/sts.svg"),
            include_bytes!("../assets/icons/sts.svg"),
        ),
        WorkloadKind::DaemonSet => (
            SharedString::new_static("icons/ds.svg"),
            include_bytes!("../assets/icons/ds.svg"),
        ),
        WorkloadKind::Job => (
            SharedString::new_static("icons/job.svg"),
            include_bytes!("../assets/icons/job.svg"),
        ),
    }
}

macro_rules! tool_icons {
    ($($tool:ident => $file:literal),+ $(,)?) => {
        fn tool_icon(tool: Tool) -> (SharedString, &'static [u8]) {
            match tool {
                $(Tool::$tool => (
                    SharedString::new_static(concat!("icons/tools/", $file)),
                    include_bytes!(concat!("../assets/icons/tools/", $file)),
                ),)+
                Tool::None => unreachable!("generic workloads use the kind badge"),
            }
        }
    };
}

tool_icons! {
    Airflow => "apacheairflow.svg",
    ArgoCd => "argo.svg",
    Cassandra => "apachecassandra.svg",
    ClickHouse => "clickhouse.svg",
    Consul => "consul.svg",
    Elasticsearch => "elasticsearch.svg",
    Envoy => "envoyproxy.svg",
    Etcd => "etcd.svg",
    FluentBit => "fluentbit.svg",
    Fluentd => "fluentd.svg",
    Flux => "flux.svg",
    Grafana => "grafana.svg",
    Harbor => "harbor.svg",
    Istio => "istio.svg",
    Jaeger => "jaeger.svg",
    Jenkins => "jenkins.svg",
    Kafka => "apachekafka.svg",
    Keycloak => "keycloak.svg",
    Kibana => "kibana.svg",
    Kubernetes => "kubernetes.svg",
    MariaDb => "mariadb.svg",
    Minio => "minio.svg",
    MongoDb => "mongodb.svg",
    MySql => "mysql.svg",
    Nats => "natsdotio.svg",
    Nginx => "nginx.svg",
    OpenTelemetry => "opentelemetry.svg",
    Postgres => "postgresql.svg",
    Prometheus => "prometheus.svg",
    RabbitMq => "rabbitmq.svg",
    Redis => "redis.svg",
    Temporal => "temporal.svg",
    Traefik => "traefikproxy.svg",
    Vault => "vault.svg",
}

fn sat_icon(kind: SatKind) -> (SharedString, &'static [u8]) {
    match kind {
        SatKind::Volume => (
            SharedString::new_static("icons/pvc.svg"),
            include_bytes!("../assets/icons/pvc.svg"),
        ),
        SatKind::Service => (
            SharedString::new_static("icons/svc.svg"),
            include_bytes!("../assets/icons/svc.svg"),
        ),
        SatKind::ConfigMap => (
            SharedString::new_static("icons/cm.svg"),
            include_bytes!("../assets/icons/cm.svg"),
        ),
        SatKind::Secret => (
            SharedString::new_static("icons/secret.svg"),
            include_bytes!("../assets/icons/secret.svg"),
        ),
    }
}

pub struct MapView {
    scene: SharedScene,
    ctrl: Sender<WorldCtrl>,
    camera: Camera,
    drag: Option<Point<Pixels>>,
    churn_on: bool,
    edges_on: bool,
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

        div()
            .size_full()
            .bg(rgb(BG))
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

struct LabelJob {
    text: SharedString,
    x: f32,
    y: f32,
    size_px: f32,
    color: gpui::Rgba,
}

enum IconJob {
    Wl(WorkloadKind, Bounds<Pixels>),
    Tool(Tool, Bounds<Pixels>),
    Sat(SatKind, Bounds<Pixels>),
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
    was_continuous: bool,
    animating: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let frame_start = std::time::Instant::now();
    stats.borrow_mut().begin_frame(frame_start, was_continuous);
    let mut bg = bg_buf.borrow_mut();
    bg.clear();
    let mut fg = fg_buf.borrow_mut();
    fg.clear();

    let vw = f32::from(bounds.size.width);
    let vh = f32::from(bounds.size.height);
    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let zoom = camera.zoom;
    let visible = camera.visible_world(vw, vh);

    let w2b = |r: &Rect| -> Bounds<Pixels> {
        let (sx, sy) = camera.w2s(r.x, r.y, vw, vh);
        Bounds {
            origin: point(px(ox + sx), px(oy + sy)),
            size: size(px(r.w * zoom), px(r.h * zoom)),
        }
    };

    let lod = lod();
    let stage = blend.walk_stage();
    let skip_wl = skip_workloads();
    let stress_any = lod.stress || lod.stress_curves;

    let block_alpha = blend.stage_alpha(1);
    let cell_alpha = blend.stage_alpha(2);
    let cell_label_alpha = blend.stage_alpha(3);

    let z01_t = if stage == 0 {
        0.0
    } else if blend.from.min(blend.to) >= 1 {
        1.0
    } else {
        blend.fade_alpha()
    };

    let ns_border_hsla: gpui::Hsla = rgb(NS_BORDER).into();
    let ns_fill_bg: gpui::Background = rgb(NS_FILL).into();
    let ns_fill_rgba = rgb(NS_FILL);
    let ns_border_rgba = rgb(NS_BORDER);
    let header_fill_bg: gpui::Background = scale_alpha(rgb(CARD_HEADER_FILL), block_alpha).into();
    const HEALTHS: [Health; 4] = [Health::Ok, Health::Warn, Health::Err, Health::Unknown];
    let health_ix = |h: Health| -> usize {
        match h {
            Health::Ok => 0,
            Health::Warn => 1,
            Health::Err => 2,
            Health::Unknown => 3,
        }
    };
    let wl_paint: [(gpui::Background, gpui::Hsla); 4] = HEALTHS.map(|h| {
        let (fill_c, border_c) = workload_colors(h);
        (
            scale_alpha(fill_c, block_alpha).into(),
            scale_alpha(border_c, block_alpha).into(),
        )
    });
    let pod_paint: [gpui::Background; 4] =
        HEALTHS.map(|h| scale_alpha(pod_color(h), cell_alpha).into());
    let strip_paint: [gpui::Background; 4] =
        HEALTHS.map(|h| scale_alpha(pod_color(h), block_alpha).into());

    let mut quads = 0usize;
    let mut drawn_ns = 0usize;
    let mut drawn_wl = 0usize;
    let mut drawn_pods = 0usize;
    let mut drawn_sats = 0usize;
    let mut curves = 0usize;
    let mut curves_dropped = 0usize;
    let mut labels = label_buf.borrow_mut();
    labels.clear();
    let mut labels_dropped = 0usize;
    let mut icons = icon_buf.borrow_mut();
    icons.clear();
    let mut icons_dropped = 0usize;

    let push_label = |labels: &mut Vec<LabelJob>,
                      dropped: &mut usize,
                      text: &std::sync::Arc<str>,
                      x: f32,
                      y: f32,
                      size_px: f32,
                      color: gpui::Rgba| {
        if labels.len() >= lod.max_labels {
            *dropped += 1;
        } else {
            labels.push(LabelJob {
                text: SharedString::new(&**text),
                x,
                y,
                size_px,
                color,
            });
        }
    };

    let glow = glow_on();
    let mut curve_core = PathBuilder::stroke(px(CURVE_CORE_W));
    let mut curve_glow = PathBuilder::stroke(px(CURVE_GLOW_W));
    let mut dash_scratch: Vec<(f32, f32)> = Vec::new();

    let walk_start = std::time::Instant::now();
    bg.push(fill(bounds, rgb(BG)));
    quads += 1;

    for ns in &scene.regions {
        if !ns.rect.intersects(&visible) {
            continue;
        }
        drawn_ns += 1;
        let b = w2b(&ns.rect);

        if z01_t <= 0.0 {
            bg.push(quad(
                b,
                px(6.0),
                heat_color(ns.ext.unhealthy_frac),
                px(1.0),
                ns_border_hsla,
                Default::default(),
            ));
        } else if z01_t >= 1.0 {
            bg.push(quad(
                b,
                px(8.0),
                ns_fill_bg,
                px(1.0),
                heat_border(ns.ext.unhealthy_frac),
                Default::default(),
            ));
        } else {
            bg.push(quad(
                b,
                px(6.0 + 2.0 * z01_t),
                mix(heat_color(ns.ext.unhealthy_frac), ns_fill_rgba, z01_t),
                px(1.0),
                mix(ns_border_rgba, heat_border(ns.ext.unhealthy_frac), z01_t),
                Default::default(),
            ));
        }
        quads += 1;

        if lod.region_label_shown(ns.rect.w, zoom) {
            let (sx, sy) = camera.w2s(ns.rect.x, ns.rect.y, vw, vh);
            push_label(
                &mut labels,
                &mut labels_dropped,
                &ns.label,
                ox + sx + 10.0,
                oy + sy + 6.0,
                NS_LABEL_PX,
                gpui::Rgba {
                    r: 0.62,
                    g: 0.58,
                    b: 0.75,
                    a: 1.0,
                },
            );
        }

        if stage == 0 {
            continue;
        }

        let region_inside = visible.contains(&ns.rect);
        let ns_blocks = &scene.blocks[ns.children.start as usize..ns.children.end as usize];
        for wl in ns_blocks {
            if !(region_inside || wl.rect.intersects(&visible)) {
                continue;
            }
            let painted = lod.block_painted(wl.inner.w, zoom) && !skip_wl;
            if painted {
                drawn_wl += 1;
                let (fill_bg, border_hsla) = wl_paint[health_ix(wl.ext.health)];
                fg.push(quad(
                    w2b(&wl.inner),
                    px(4.0),
                    fill_bg,
                    px(1.0),
                    border_hsla,
                    Default::default(),
                ));
                quads += 1;

                if lod.block_chrome_shown(wl.inner.w, zoom) {
                    let header_h = CARD_HEADER.min(wl.inner.h * 0.32);
                    let header = Rect::new(wl.inner.x, wl.inner.y, wl.inner.w, header_h);
                    fg.push(quad(
                        w2b(&header),
                        px(4.0),
                        header_fill_bg,
                        px(0.0),
                        gpui::transparent_black(),
                        Default::default(),
                    ));
                    let strip = Rect::new(
                        wl.inner.x + wl.inner.w * 0.06,
                        wl.inner.y + header_h * 0.72,
                        wl.inner.w * 0.88,
                        header_h * 0.14,
                    );
                    fg.push(fill(w2b(&strip), strip_paint[health_ix(wl.ext.health)]));
                    quads += 2;
                }

                if lod.block_icon_shown(wl.inner.w, zoom) {
                    if icons.len() >= lod.max_icons {
                        icons_dropped += 1;
                    } else {
                        let (sx, sy) = camera.w2s(wl.inner.max_x(), wl.inner.y, vw, vh);
                        let b = Bounds {
                            origin: point(px(ox + sx - WL_ICON_PX - 3.0), px(oy + sy + 3.0)),
                            size: size(px(WL_ICON_PX), px(WL_ICON_PX)),
                        };
                        icons.push(if wl.ext.tool != Tool::None {
                            IconJob::Tool(wl.ext.tool, b)
                        } else {
                            IconJob::Wl(wl.ext.kind, b)
                        });
                    }
                }

                if lod.block_label_shown(wl.inner.w, zoom) {
                    let (sx, sy) = camera.w2s(wl.inner.x, wl.inner.y, vw, vh);
                    push_label(
                        &mut labels,
                        &mut labels_dropped,
                        &wl.label,
                        ox + sx + 4.0,
                        oy + sy + 1.0,
                        WL_LABEL_PX,
                        gpui::Rgba {
                            r: 0.72,
                            g: 0.68,
                            b: 0.85,
                            a: block_alpha,
                        },
                    );
                }
            }

            if stage < 2 {
                continue;
            }
            let block_inside = region_inside || visible.contains(&wl.rect);

            if painted || lod.stress_curves {
                let sat_base = wl.sats.start as usize;
                let sats = &scene.sats[sat_base..wl.sats.end as usize];
                let (hub_wx, hub_wy) = wl.inner.center();
                let (hx, hy) = camera.w2s(hub_wx, hub_wy, vw, vh);
                let hub_pt = (ox + hx, oy + hy);
                for (j, sat) in sats.iter().enumerate() {
                    if !(block_inside || sat.rect.intersects(&visible)) {
                        continue;
                    }
                    if !lod.sat_painted(sat.rect.w, zoom) {
                        continue;
                    }
                    drawn_sats += 1;
                    let (sat_wx, sat_wy) = sat.rect.center();
                    let (sx, sy) = camera.w2s(sat_wx, sat_wy, vw, vh);
                    let sat_pt = (ox + sx, oy + sy);

                    if lod.sat_icon_shown() {
                        if icons.len() >= lod.max_icons {
                            icons_dropped += 1;
                        } else {
                            icons.push(IconJob::Sat(
                                sat.ext.kind,
                                Bounds {
                                    origin: point(
                                        px(sat_pt.0 - SAT_ICON_PX * 0.5),
                                        px(sat_pt.1 - SAT_ICON_PX * 0.5),
                                    ),
                                    size: size(px(SAT_ICON_PX), px(SAT_ICON_PX)),
                                },
                            ));
                        }
                    }

                    if lod.sat_label_shown(sat.rect.w, zoom) {
                        let (lx, ly) = camera.w2s(sat.rect.x, sat.rect.max_y(), vw, vh);
                        push_label(
                            &mut labels,
                            &mut labels_dropped,
                            &sat.label,
                            ox + lx - 8.0,
                            oy + ly + 2.0,
                            SAT_NAME_PX,
                            gpui::Rgba {
                                r: 0.80,
                                g: 0.75,
                                b: 0.90,
                                a: cell_alpha,
                            },
                        );
                        push_label(
                            &mut labels,
                            &mut labels_dropped,
                            &sat.ext.detail,
                            ox + lx - 8.0,
                            oy + ly + 2.0 + SAT_NAME_PX * 1.25,
                            SAT_DETAIL_PX,
                            gpui::Rgba {
                                r: 0.62,
                                g: 0.57,
                                b: 0.74,
                                a: cell_alpha,
                            },
                        );
                    }

                    if lod.sat_curves {
                        if curves >= lod.curve_budget() {
                            curves_dropped += 1;
                        } else {
                            curves += 1;
                            let bow = bow_jitter((sat_base + j) as u64);
                            let ctrl = curve_ctrl(hub_pt, sat_pt, bow);
                            if glow {
                                curve_glow.move_to(point(px(hub_pt.0), px(hub_pt.1)));
                                curve_glow.curve_to(
                                    point(px(sat_pt.0), px(sat_pt.1)),
                                    point(px(ctrl.0), px(ctrl.1)),
                                );
                            }
                            dash_quadratic(
                                hub_pt,
                                ctrl,
                                sat_pt,
                                CURVE_TOL,
                                CURVE_DASH_ON,
                                CURVE_DASH_OFF,
                                &mut dash_scratch,
                                |is_move, p| {
                                    if is_move {
                                        curve_core.move_to(point(px(p.0), px(p.1)));
                                    } else {
                                        curve_core.line_to(point(px(p.0), px(p.1)));
                                    }
                                },
                            );
                        }
                    }
                }
            }

            if !painted {
                continue;
            }
            let wl_cells = &scene.cells[wl.children.start as usize..wl.children.end as usize];
            for pod in wl_cells {
                if !(block_inside || pod.rect.intersects(&visible)) {
                    continue;
                }
                drawn_pods += 1;
                fg.push(fill(w2b(&pod.rect), pod_paint[health_ix(pod.ext.health)]));
                quads += 1;

                if stage >= 3 && lod.cell_label_shown(pod.rect.w, zoom) {
                    let (sx, sy) = camera.w2s(pod.rect.x, pod.rect.y + pod.rect.h, vw, vh);
                    push_label(
                        &mut labels,
                        &mut labels_dropped,
                        &pod.label,
                        ox + sx,
                        oy + sy + 2.0,
                        POD_LABEL_PX,
                        gpui::Rgba {
                            r: 0.55,
                            g: 0.51,
                            b: 0.66,
                            a: cell_label_alpha,
                        },
                    );
                }
            }
        }
    }

    let bg_quads_start = std::time::Instant::now();
    window.paint_quads(&bg);

    let paths_start = std::time::Instant::now();
    let mut bg_hexes = 0usize;
    if !stress_any && hex::hex_on() {
        let (hex_r, hex_alpha) = hex::level(zoom);
        let mut builder = PathBuilder::stroke(px(1.0));
        bg_hexes = hex::for_each_center(&visible, hex_r, |cx_, cy_| {
            for i in 0..6 {
                let ang = i as f32 * std::f32::consts::FRAC_PI_3;
                let (wx, wy) = (cx_ + hex_r * ang.cos(), cy_ + hex_r * ang.sin());
                let (sx, sy) = camera.w2s(wx, wy, vw, vh);
                let p = point(px(ox + sx), px(oy + sy));
                if i == 0 {
                    builder.move_to(p);
                } else {
                    builder.line_to(p);
                }
            }
            builder.close();
        });
        if bg_hexes > 0
            && let Ok(path) = builder.build()
        {
            window.paint_path(path, rgb(HEX_LINE).alpha(hex_alpha));
        }
    }

    let mut drawn_edges = 0usize;
    if edges_on && stage >= 2 && !stress_any {
        let mut builder = PathBuilder::stroke(px(1.0));
        drawn_edges = k10s_atlas::walk_edges(scene, &visible, lod.max_edges, |a, b| {
            let (ax, ay) = camera.w2s(a.center().0, a.center().1, vw, vh);
            let (bx, by) = camera.w2s(b.center().0, b.center().1, vw, vh);
            let pa = (ox + ax, oy + ay);
            let pb = (ox + bx, oy + by);

            let h = ((a.x.to_bits() as u64) << 32 ^ a.y.to_bits() as u64)
                ^ ((b.x.to_bits() as u64) << 16 ^ b.y.to_bits() as u64);
            let ctrl = curve_ctrl(pa, pb, bow_jitter(h) * 0.6);
            builder.move_to(point(px(pa.0), px(pa.1)));
            builder.curve_to(point(px(pb.0), px(pb.1)), point(px(ctrl.0), px(ctrl.1)));
        });
        if drawn_edges > 0
            && let Ok(path) = builder.build()
        {
            window.paint_path(path, rgb(EDGE).alpha(0.30 * cell_alpha));
        }
    }

    if curves > 0 {
        if glow && let Ok(path) = curve_glow.build() {
            window.paint_path(path, rgb(CURVE_GLOW).alpha(CURVE_GLOW_ALPHA * cell_alpha));
        }
        if let Ok(path) = curve_core.build() {
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
                    IconJob::Tool(tool, b) => {
                        let (key, data) = tool_icon(*tool);
                        (
                            key,
                            data,
                            *b,
                            scale_alpha(tool_color(*tool), 0.95 * block_alpha).into(),
                        )
                    }
                    IconJob::Sat(kind, b) => {
                        let (key, data) = sat_icon(*kind);
                        (
                            key,
                            data,
                            *b,
                            scale_alpha(sat_color(*kind), cell_alpha).into(),
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

    #[cfg(debug_assertions)]
    {
        let oracle = lod::cull(scene, &camera, blend, vw, vh, edges_on, skip_workloads());
        debug_assert_eq!(
            oracle.quads, quads,
            "cull oracle: quads diverged from painter"
        );
        debug_assert_eq!(oracle.edges, drawn_edges, "cull oracle: edges diverged");
        debug_assert_eq!(
            (oracle.drawn_sats, oracle.curves, oracle.curves_dropped),
            (drawn_sats, curves, curves_dropped),
            "cull oracle: satellites/curves diverged"
        );
        debug_assert_eq!(oracle.bg_cells, bg_hexes, "cull oracle: hex count diverged");
        debug_assert_eq!(
            (oracle.labels, oracle.labels_dropped),
            (labels.len(), labels_dropped),
            "cull oracle: labels diverged"
        );
        debug_assert_eq!(
            (oracle.icons, oracle.icons_dropped),
            (icons.len(), icons_dropped),
            "cull oracle: icons diverged"
        );
        debug_assert_eq!(
            (
                oracle.drawn_regions,
                oracle.drawn_blocks,
                oracle.drawn_cells
            ),
            (drawn_ns, drawn_wl, drawn_pods),
            "cull oracle: drawn counts diverged"
        );
    }

    {
        let mut st = stats.borrow_mut();
        st.quads = quads;
        st.lines = label_counts.lines;
        st.glyphs = label_counts.glyphs;
        st.edges = drawn_edges;
        st.icons = icons.len();
        st.sats = drawn_sats;
        st.curves = curves;
        st.curves_dropped = curves_dropped;
        st.bg_cells = bg_hexes;
        st.drawn = (drawn_ns, drawn_wl, drawn_pods);
        st.labels_dropped = labels_dropped;
        st.icons_dropped = icons_dropped;
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
    animating: bool,
    ox: f32,
    oy: f32,
    window: &mut Window,
    cx: &mut App,
) {
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
            st.drawn.0,
            st.drawn.1,
            group(st.drawn.2 as u32),
        ),
        format!(
            "[c]hurn {}  [e]dges {}  [f]it",
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
        let s = SharedString::from(text.clone());
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
