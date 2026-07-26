//! The traversal-and-emit seam between a `SceneSnapshot` and the painter.
//!
//! [`walk`] is the single implementation of "what is visible, and where". It never touches a
//! `Window`: every primitive it produces leaves through [`FrameSink`], so the real painter
//! ([`PaintSink`], which owns the gpui quad buffers and path builders) and a headless test sink
//! drive the exact same traversal. The [`CullStats`] it returns is the painter's side of the cull
//! oracle invariant; [`crate::lod::cull`] re-derives the same struct from `k10s-atlas` and the two
//! must be equal for every camera, blend and policy.
//!
//! Nothing in here reads a process global. Every `K10S_*` knob that can move a counter arrives as
//! [`FrameOpts`] plus the `LodPolicy` it borrows, which is what lets a test sweep policies in
//! parallel threads. `K10S_NO_GLOW` stops at [`PaintSink`] and `K10S_REPAINT_ALWAYS` never gets
//! this far.

use gpui::{
    Bounds, PaintQuad, PathBuilder, Pixels, SharedString, fill, point, px, quad, rgb, size,
};
use k10s_atlas::curves::{bow_jitter, curve_ctrl, dash_quadratic};
use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
use k10s_core::layout::CARD_HEADER;
use k10s_core::{Health, Rect, SatKind, SceneSnapshot, Tool, WorkloadKind};

use crate::colors::*;
use crate::hex;

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

/// Everything outside the camera that changes what a frame emits.
///
/// `policy` carries the four LOD knobs (`K10S_STRESS_QUADS`, `K10S_STRESS_CURVES`,
/// `K10S_NO_CURVES`, `K10S_NO_ICONS`) and the budgets; the three fields here carry the rest.
/// `K10S_NO_GLOW` belongs to the sink, not the walk, because it cannot change a counter.
#[derive(Debug, Clone, Copy)]
pub struct FrameOpts<'a> {
    pub policy: &'a LodPolicy,
    /// The `[e]` toggle.
    pub edges_on: bool,
    /// `K10S_SKIP_WL`.
    pub skip_blocks: bool,
    /// `K10S_NO_HEX` inverted.
    pub hex: bool,
}

impl FrameOpts<'_> {
    pub(crate) fn stress_any(&self) -> bool {
        self.policy.stress || self.policy.stress_curves
    }

    /// The hex grid is a calm-state backdrop: any stress mode suppresses it.
    pub(crate) fn hex_shown(&self) -> bool {
        self.hex && !self.stress_any()
    }
}

pub(crate) struct LabelJob {
    pub(crate) text: SharedString,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) size_px: f32,
    pub(crate) color: gpui::Rgba,
}

pub(crate) enum IconJob {
    Wl(WorkloadKind, Bounds<Pixels>),
    Tool(Tool, Bounds<Pixels>),
    Sat(SatKind, Bounds<Pixels>),
}

/// Where a frame's primitives go. Implemented once for real painting ([`PaintSink`]) and once in
/// the oracle test, which counts and drops.
///
/// The walk never reads anything back from a sink, so a sink cannot influence the counters: that
/// is what makes the headless comparison a test of the painter rather than of the test's own copy.
pub(crate) trait FrameSink {
    fn bg_quad(&mut self, quad: PaintQuad);
    fn fg_quad(&mut self, quad: PaintQuad);
    fn label(&mut self, label: LabelJob);
    fn icon(&mut self, icon: IconJob);
    /// Six screen-space vertices of one background hex, in ring order.
    fn hex_ring(&mut self, ring: &[(f32, f32); 6]);
    /// A hub-to-satellite quadratic in screen space.
    fn curve(&mut self, hub: (f32, f32), ctrl: (f32, f32), sat: (f32, f32));
    /// A block-to-block quadratic in screen space.
    fn edge(&mut self, a: (f32, f32), ctrl: (f32, f32), b: (f32, f32));
}

/// Mirrors `k10s_atlas::cull::push_label`. The job is built lazily so a dropped label costs
/// nothing; change one of the two and the oracle test fails.
fn push_label<S: FrameSink>(
    st: &mut CullStats,
    policy: &LodPolicy,
    sink: &mut S,
    job: impl FnOnce() -> LabelJob,
) {
    if st.labels >= policy.max_labels {
        st.labels_dropped += 1;
    } else {
        st.labels += 1;
        sink.label(job());
    }
}

/// Mirrors `k10s_atlas::cull::push_icon`.
fn push_icon<S: FrameSink>(
    st: &mut CullStats,
    policy: &LodPolicy,
    sink: &mut S,
    job: impl FnOnce() -> IconJob,
) {
    if st.icons >= policy.max_icons {
        st.icons_dropped += 1;
    } else {
        st.icons += 1;
        sink.icon(job());
    }
}

/// Walk the scene once and emit a frame into `sink`, returning what was emitted.
///
/// This is the painter's traversal. `crate::paint_map` calls it with a [`PaintSink`] and then
/// submits the buffers to gpui; nothing else about a frame decides visibility.
pub(crate) fn walk<S: FrameSink>(
    bounds: Bounds<Pixels>,
    scene: &SceneSnapshot,
    camera: Camera,
    blend: StageBlend,
    opts: FrameOpts<'_>,
    sink: &mut S,
) -> CullStats {
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

    let lod = opts.policy;
    let stage = blend.walk_stage();
    let skip_wl = opts.skip_blocks;

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

    let mut st = CullStats {
        stage,
        ..CullStats::default()
    };

    sink.bg_quad(fill(bounds, rgb(BG)));
    st.quads += 1;

    for ns in &scene.regions {
        if !ns.rect.intersects(&visible) {
            continue;
        }
        st.drawn_regions += 1;
        let b = w2b(&ns.rect);

        if z01_t <= 0.0 {
            sink.bg_quad(quad(
                b,
                px(6.0),
                heat_color(ns.ext.unhealthy_frac),
                px(1.0),
                ns_border_hsla,
                Default::default(),
            ));
        } else if z01_t >= 1.0 {
            sink.bg_quad(quad(
                b,
                px(8.0),
                ns_fill_bg,
                px(1.0),
                heat_border(ns.ext.unhealthy_frac),
                Default::default(),
            ));
        } else {
            sink.bg_quad(quad(
                b,
                px(6.0 + 2.0 * z01_t),
                mix(heat_color(ns.ext.unhealthy_frac), ns_fill_rgba, z01_t),
                px(1.0),
                mix(ns_border_rgba, heat_border(ns.ext.unhealthy_frac), z01_t),
                Default::default(),
            ));
        }
        st.quads += 1;

        if lod.region_label_shown(ns.rect.w, zoom) {
            push_label(&mut st, lod, sink, || {
                let (sx, sy) = camera.w2s(ns.rect.x, ns.rect.y, vw, vh);
                LabelJob {
                    text: SharedString::new(&*ns.label),
                    x: ox + sx + 10.0,
                    y: oy + sy + 6.0,
                    size_px: NS_LABEL_PX,
                    color: gpui::Rgba {
                        r: 0.62,
                        g: 0.58,
                        b: 0.75,
                        a: 1.0,
                    },
                }
            });
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
                st.drawn_blocks += 1;
                let (fill_bg, border_hsla) = wl_paint[health_ix(wl.ext.health)];
                sink.fg_quad(quad(
                    w2b(&wl.inner),
                    px(4.0),
                    fill_bg,
                    px(1.0),
                    border_hsla,
                    Default::default(),
                ));
                st.quads += 1;

                if lod.block_chrome_shown(wl.inner.w, zoom) {
                    let header_h = CARD_HEADER.min(wl.inner.h * 0.32);
                    let header = Rect::new(wl.inner.x, wl.inner.y, wl.inner.w, header_h);
                    sink.fg_quad(quad(
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
                    sink.fg_quad(fill(w2b(&strip), strip_paint[health_ix(wl.ext.health)]));
                    st.quads += 2;
                }

                if lod.block_icon_shown(wl.inner.w, zoom) {
                    push_icon(&mut st, lod, sink, || {
                        let (sx, sy) = camera.w2s(wl.inner.max_x(), wl.inner.y, vw, vh);
                        let b = Bounds {
                            origin: point(px(ox + sx - WL_ICON_PX - 3.0), px(oy + sy + 3.0)),
                            size: size(px(WL_ICON_PX), px(WL_ICON_PX)),
                        };
                        if wl.ext.tool != Tool::None {
                            IconJob::Tool(wl.ext.tool, b)
                        } else {
                            IconJob::Wl(wl.ext.kind, b)
                        }
                    });
                }

                if lod.block_label_shown(wl.inner.w, zoom) {
                    push_label(&mut st, lod, sink, || {
                        let (sx, sy) = camera.w2s(wl.inner.x, wl.inner.y, vw, vh);
                        LabelJob {
                            text: SharedString::new(&*wl.label),
                            x: ox + sx + 4.0,
                            y: oy + sy + 1.0,
                            size_px: WL_LABEL_PX,
                            color: gpui::Rgba {
                                r: 0.72,
                                g: 0.68,
                                b: 0.85,
                                a: block_alpha,
                            },
                        }
                    });
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
                    st.drawn_sats += 1;
                    let (sat_wx, sat_wy) = sat.rect.center();
                    let (sx, sy) = camera.w2s(sat_wx, sat_wy, vw, vh);
                    let sat_pt = (ox + sx, oy + sy);

                    if lod.sat_icon_shown() {
                        push_icon(&mut st, lod, sink, || {
                            IconJob::Sat(
                                sat.ext.kind,
                                Bounds {
                                    origin: point(
                                        px(sat_pt.0 - SAT_ICON_PX * 0.5),
                                        px(sat_pt.1 - SAT_ICON_PX * 0.5),
                                    ),
                                    size: size(px(SAT_ICON_PX), px(SAT_ICON_PX)),
                                },
                            )
                        });
                    }

                    if lod.sat_label_shown(sat.rect.w, zoom) {
                        let (lx, ly) = camera.w2s(sat.rect.x, sat.rect.max_y(), vw, vh);
                        push_label(&mut st, lod, sink, || LabelJob {
                            text: SharedString::new(&*sat.label),
                            x: ox + lx - 8.0,
                            y: oy + ly + 2.0,
                            size_px: SAT_NAME_PX,
                            color: gpui::Rgba {
                                r: 0.80,
                                g: 0.75,
                                b: 0.90,
                                a: cell_alpha,
                            },
                        });
                        push_label(&mut st, lod, sink, || LabelJob {
                            text: SharedString::new(&*sat.ext.detail),
                            x: ox + lx - 8.0,
                            y: oy + ly + 2.0 + SAT_NAME_PX * 1.25,
                            size_px: SAT_DETAIL_PX,
                            color: gpui::Rgba {
                                r: 0.62,
                                g: 0.57,
                                b: 0.74,
                                a: cell_alpha,
                            },
                        });
                    }

                    if lod.sat_curves {
                        if st.curves >= lod.curve_budget() {
                            st.curves_dropped += 1;
                        } else {
                            st.curves += 1;
                            let bow = bow_jitter((sat_base + j) as u64);
                            let ctrl = curve_ctrl(hub_pt, sat_pt, bow);
                            sink.curve(hub_pt, ctrl, sat_pt);
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
                st.drawn_cells += 1;
                sink.fg_quad(fill(w2b(&pod.rect), pod_paint[health_ix(pod.ext.health)]));
                st.quads += 1;

                if stage >= 3 && lod.cell_label_shown(pod.rect.w, zoom) {
                    push_label(&mut st, lod, sink, || {
                        let (sx, sy) = camera.w2s(pod.rect.x, pod.rect.y + pod.rect.h, vw, vh);
                        LabelJob {
                            text: SharedString::new(&*pod.label),
                            x: ox + sx,
                            y: oy + sy + 2.0,
                            size_px: POD_LABEL_PX,
                            color: gpui::Rgba {
                                r: 0.55,
                                g: 0.51,
                                b: 0.66,
                                a: cell_label_alpha,
                            },
                        }
                    });
                }
            }
        }
    }

    if opts.hex_shown() {
        let (hex_r, _) = hex::level(zoom);
        st.bg_cells = hex::for_each_center(&visible, hex_r, |cx_, cy_| {
            let mut ring = [(0.0f32, 0.0f32); 6];
            for (i, vertex) in ring.iter_mut().enumerate() {
                let ang = i as f32 * std::f32::consts::FRAC_PI_3;
                let (wx, wy) = (cx_ + hex_r * ang.cos(), cy_ + hex_r * ang.sin());
                let (sx, sy) = camera.w2s(wx, wy, vw, vh);
                *vertex = (ox + sx, oy + sy);
            }
            sink.hex_ring(&ring);
        });
    }

    if opts.edges_on && stage >= 2 && !opts.stress_any() {
        st.edges = k10s_atlas::walk_edges(scene, &visible, lod.max_edges, |a, b| {
            let (ax, ay) = camera.w2s(a.center().0, a.center().1, vw, vh);
            let (bx, by) = camera.w2s(b.center().0, b.center().1, vw, vh);
            let pa = (ox + ax, oy + ay);
            let pb = (ox + bx, oy + by);

            let h = ((a.x.to_bits() as u64) << 32 ^ a.y.to_bits() as u64)
                ^ ((b.x.to_bits() as u64) << 16 ^ b.y.to_bits() as u64);
            let ctrl = curve_ctrl(pa, pb, bow_jitter(h) * 0.6);
            sink.edge(pa, ctrl, pb);
        });
    }

    st
}

/// The painting sink: gpui quad buffers, label and icon job lists, and four path builders.
///
/// It owns the geometry decisions that cannot change a counter (dash flattening, whether the glow
/// pass is built), which is why `K10S_NO_GLOW` lives here and not in [`FrameOpts`].
pub(crate) struct PaintSink<'a> {
    bg: &'a mut Vec<PaintQuad>,
    fg: &'a mut Vec<PaintQuad>,
    labels: &'a mut Vec<LabelJob>,
    icons: &'a mut Vec<IconJob>,
    hex_path: PathBuilder,
    edge_path: PathBuilder,
    curve_core: PathBuilder,
    curve_glow: PathBuilder,
    glow: bool,
    dash: Vec<(f32, f32)>,
}

/// The four stroked paths a frame builds, handed back once the walk is done so the painter can
/// tessellate and submit them. `glow` is false when `K10S_NO_GLOW` suppressed the glow pass, in
/// which case `curve_glow` is empty.
pub(crate) struct FramePaths {
    pub(crate) hex: PathBuilder,
    pub(crate) edges: PathBuilder,
    pub(crate) curve_core: PathBuilder,
    pub(crate) curve_glow: PathBuilder,
    pub(crate) glow: bool,
}

impl<'a> PaintSink<'a> {
    pub(crate) fn new(
        bg: &'a mut Vec<PaintQuad>,
        fg: &'a mut Vec<PaintQuad>,
        labels: &'a mut Vec<LabelJob>,
        icons: &'a mut Vec<IconJob>,
        glow: bool,
    ) -> Self {
        bg.clear();
        fg.clear();
        labels.clear();
        icons.clear();
        PaintSink {
            bg,
            fg,
            labels,
            icons,
            hex_path: PathBuilder::stroke(px(1.0)),
            edge_path: PathBuilder::stroke(px(1.0)),
            curve_core: PathBuilder::stroke(px(CURVE_CORE_W)),
            curve_glow: PathBuilder::stroke(px(CURVE_GLOW_W)),
            glow,
            dash: Vec::new(),
        }
    }

    /// Release the borrowed buffers and hand the built paths to the painter.
    pub(crate) fn into_paths(self) -> FramePaths {
        FramePaths {
            hex: self.hex_path,
            edges: self.edge_path,
            curve_core: self.curve_core,
            curve_glow: self.curve_glow,
            glow: self.glow,
        }
    }
}

impl FrameSink for PaintSink<'_> {
    fn bg_quad(&mut self, quad: PaintQuad) {
        self.bg.push(quad);
    }

    fn fg_quad(&mut self, quad: PaintQuad) {
        self.fg.push(quad);
    }

    fn label(&mut self, label: LabelJob) {
        self.labels.push(label);
    }

    fn icon(&mut self, icon: IconJob) {
        self.icons.push(icon);
    }

    fn hex_ring(&mut self, ring: &[(f32, f32); 6]) {
        for (i, (x, y)) in ring.iter().enumerate() {
            let p = point(px(*x), px(*y));
            if i == 0 {
                self.hex_path.move_to(p);
            } else {
                self.hex_path.line_to(p);
            }
        }
        self.hex_path.close();
    }

    fn curve(&mut self, hub: (f32, f32), ctrl: (f32, f32), sat: (f32, f32)) {
        if self.glow {
            self.curve_glow.move_to(point(px(hub.0), px(hub.1)));
            self.curve_glow
                .curve_to(point(px(sat.0), px(sat.1)), point(px(ctrl.0), px(ctrl.1)));
        }
        let dash = &mut self.dash;
        let core = &mut self.curve_core;
        dash_quadratic(
            hub,
            ctrl,
            sat,
            CURVE_TOL,
            CURVE_DASH_ON,
            CURVE_DASH_OFF,
            dash,
            |is_move, p| {
                if is_move {
                    core.move_to(point(px(p.0), px(p.1)));
                } else {
                    core.line_to(point(px(p.0), px(p.1)));
                }
            },
        );
    }

    fn edge(&mut self, a: (f32, f32), ctrl: (f32, f32), b: (f32, f32)) {
        self.edge_path.move_to(point(px(a.0), px(a.1)));
        self.edge_path
            .curve_to(point(px(b.0), px(b.1)), point(px(ctrl.0), px(ctrl.1)));
    }
}
