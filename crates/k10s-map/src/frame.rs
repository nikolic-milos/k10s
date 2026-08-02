use gpui::{
    Bounds, PaintQuad, PathBuilder, Pixels, SharedString, fill, point, px, quad, rgb, size,
};
use k10s_atlas::curves::{bow_jitter, curve_ctrl, dash_quadratic};
use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
use k10s_core::layout::CARD_HEADER;
use k10s_core::{
    KindId, NsNode, PodNode, Rect, SatNode, SceneSnapshot, Severity, ToolId, WorkloadNode,
};

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

#[derive(Debug, Clone, Copy)]
pub struct FrameOpts<'a> {
    pub policy: &'a LodPolicy,
    pub edges_on: bool,
    pub skip_blocks: bool,
    pub hex: bool,
}

impl FrameOpts<'_> {
    pub(crate) fn stress_any(&self) -> bool {
        self.policy.stress || self.policy.stress_curves
    }

    pub(crate) fn hex_shown(&self) -> bool {
        self.hex && !self.stress_any()
    }
}

pub struct LabelJob {
    pub text: SharedString,
    pub x: f32,
    pub y: f32,
    pub size_px: f32,
    pub color: gpui::Rgba,
}

pub enum IconJob {
    Wl(KindId, Bounds<Pixels>),
    ToolId(ToolId, Bounds<Pixels>),
    Sat(KindId, Bounds<Pixels>),
}

pub trait FrameSink {
    fn bg_quad(&mut self, quad: PaintQuad);
    fn fg_quad(&mut self, quad: PaintQuad);
    fn label(&mut self, label: LabelJob);
    fn icon(&mut self, icon: IconJob);
    fn hex_ring(&mut self, ring: &[(f32, f32); 6]);
    fn curve(&mut self, hub: (f32, f32), ctrl: (f32, f32), sat: (f32, f32));
    fn edge(&mut self, a: (f32, f32), ctrl: (f32, f32), b: (f32, f32));
}

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

struct FrameWalk<'a, S> {
    camera: Camera,
    visible: Rect,
    viewport: (f32, f32),
    origin: (f32, f32),
    zoom: f32,
    stage: u8,
    z01_t: f32,
    block_alpha: f32,
    cell_alpha: f32,
    cell_label_alpha: f32,
    policy: &'a LodPolicy,
    skip_blocks: bool,
    hex_shown: bool,
    edges_on: bool,
    ns_border: gpui::Hsla,
    ns_fill: gpui::Background,
    ns_fill_rgba: gpui::Rgba,
    ns_border_rgba: gpui::Rgba,
    header_fill: gpui::Background,
    workload_paint: [(gpui::Background, gpui::Hsla); 4],
    pod_paint: [gpui::Background; 4],
    strip_paint: [gpui::Background; 4],
    sink: &'a mut S,
    stats: CullStats,
}

impl<S: FrameSink> FrameWalk<'_, S> {
    #[inline]
    fn screen_bounds(&self, rect: &Rect) -> Bounds<Pixels> {
        let (x, y) = self
            .camera
            .w2s(rect.x, rect.y, self.viewport.0, self.viewport.1);
        Bounds {
            origin: point(px(self.origin.0 + x), px(self.origin.1 + y)),
            size: size(px(rect.w * self.zoom), px(rect.h * self.zoom)),
        }
    }

    #[inline]
    fn paint_region(&mut self, region: &NsNode) {
        self.stats.drawn_regions += 1;
        let bounds = self.screen_bounds(&region.rect);

        if self.z01_t <= 0.0 {
            self.sink.bg_quad(quad(
                bounds,
                px(6.0),
                heat_color(region.ext.unhealthy_frac),
                px(1.0),
                self.ns_border,
                Default::default(),
            ));
        } else if self.z01_t >= 1.0 {
            self.sink.bg_quad(quad(
                bounds,
                px(8.0),
                self.ns_fill,
                px(1.0),
                heat_border(region.ext.unhealthy_frac),
                Default::default(),
            ));
        } else {
            self.sink.bg_quad(quad(
                bounds,
                px(6.0 + 2.0 * self.z01_t),
                mix(
                    heat_color(region.ext.unhealthy_frac),
                    self.ns_fill_rgba,
                    self.z01_t,
                ),
                px(1.0),
                mix(
                    self.ns_border_rgba,
                    heat_border(region.ext.unhealthy_frac),
                    self.z01_t,
                ),
                Default::default(),
            ));
        }
        self.stats.quads += 1;

        if self.policy.region_label_shown(region.rect.w, self.zoom) {
            let (x, y) = self.camera.w2s(
                region.rect.x,
                region.rect.y,
                self.viewport.0,
                self.viewport.1,
            );
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&region.label),
                x: self.origin.0 + x + 10.0,
                y: self.origin.1 + y + 6.0,
                size_px: NS_LABEL_PX,
                color: gpui::Rgba {
                    r: 0.62,
                    g: 0.58,
                    b: 0.75,
                    a: 1.0,
                },
            });
        }
    }

    fn walk_region_children<const DIRECT: bool>(
        &mut self,
        scene: &SceneSnapshot,
        region_index: usize,
        region: &NsNode,
    ) {
        let region_inside = self.visible.contains(&region.rect);
        if DIRECT {
            let visible = self.visible;
            if scene.region_block_index_is_selective(region_index, &visible) {
                scene.for_each_region_block_candidate(region_index, &visible, |index, block| {
                    self.block::<true>(scene, index, block, region_inside);
                });
            } else {
                let start = region.children.start as usize;
                for (offset, block) in scene.blocks[start..region.children.end as usize]
                    .iter()
                    .enumerate()
                {
                    self.block::<true>(scene, start + offset, block, region_inside);
                }
            }
        } else {
            let visible = self.visible;
            scene.for_each_region_block_candidate(region_index, &visible, |index, block| {
                self.block::<false>(scene, index, block, region_inside);
            });
        }
    }

    #[inline(always)]
    fn block<const DIRECT: bool>(
        &mut self,
        scene: &SceneSnapshot,
        block_index: usize,
        block: &WorkloadNode,
        region_inside: bool,
    ) {
        if !(region_inside || block.rect.intersects(&self.visible)) {
            return;
        }
        let painted = self.policy.block_painted(block.inner.w, self.zoom) && !self.skip_blocks;
        if painted {
            self.stats.drawn_blocks += 1;
            let severity = severity_index(block.ext.rollup);
            let (fill_color, border) = self.workload_paint[severity];
            self.sink.fg_quad(quad(
                self.screen_bounds(&block.inner),
                px(4.0),
                fill_color,
                px(1.0),
                border,
                Default::default(),
            ));
            self.stats.quads += 1;

            if self.policy.block_chrome_shown(block.inner.w, self.zoom) {
                let header_height = CARD_HEADER.min(block.inner.h * 0.32);
                let header = Rect::new(block.inner.x, block.inner.y, block.inner.w, header_height);
                self.sink.fg_quad(quad(
                    self.screen_bounds(&header),
                    px(4.0),
                    self.header_fill,
                    px(0.0),
                    gpui::transparent_black(),
                    Default::default(),
                ));
                let strip = Rect::new(
                    block.inner.x + block.inner.w * 0.06,
                    block.inner.y + header_height * 0.72,
                    block.inner.w * 0.88,
                    header_height * 0.14,
                );
                self.sink
                    .fg_quad(fill(self.screen_bounds(&strip), self.strip_paint[severity]));
                self.stats.quads += 2;
            }

            if self.policy.block_icon_shown(block.inner.w, self.zoom) {
                let (x, y) = self.camera.w2s(
                    block.inner.max_x(),
                    block.inner.y,
                    self.viewport.0,
                    self.viewport.1,
                );
                let bounds = Bounds {
                    origin: point(
                        px(self.origin.0 + x - WL_ICON_PX - 3.0),
                        px(self.origin.1 + y + 3.0),
                    ),
                    size: size(px(WL_ICON_PX), px(WL_ICON_PX)),
                };
                push_icon(&mut self.stats, self.policy, self.sink, || {
                    if block.ext.tool != ToolId::NONE {
                        IconJob::ToolId(block.ext.tool, bounds)
                    } else {
                        IconJob::Wl(block.ext.kind, bounds)
                    }
                });
            }

            if self.policy.block_label_shown(block.inner.w, self.zoom) {
                let (x, y) = self.camera.w2s(
                    block.inner.x,
                    block.inner.y,
                    self.viewport.0,
                    self.viewport.1,
                );
                push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                    text: SharedString::from(&block.label),
                    x: self.origin.0 + x + 4.0,
                    y: self.origin.1 + y + 1.0,
                    size_px: WL_LABEL_PX,
                    color: gpui::Rgba {
                        r: 0.72,
                        g: 0.68,
                        b: 0.85,
                        a: self.block_alpha,
                    },
                });
            }
        }

        if self.stage < 2 {
            return;
        }
        let block_inside = region_inside || self.visible.contains(&block.rect);
        if painted || self.policy.stress_curves {
            let (hub_x, hub_y) = block.inner.center();
            let (x, y) = self
                .camera
                .w2s(hub_x, hub_y, self.viewport.0, self.viewport.1);
            let hub = (self.origin.0 + x, self.origin.1 + y);
            if DIRECT {
                let start = block.sats.start as usize;
                for (offset, satellite) in scene.sats[start..block.sats.end as usize]
                    .iter()
                    .enumerate()
                {
                    self.satellite(start + offset, satellite, block_inside, hub);
                }
            } else {
                scene.for_each_block_sat(block_index, |index, satellite| {
                    self.satellite(index, satellite, block_inside, hub);
                });
            }
        }

        if !painted {
            return;
        }
        let cells = block.children.len();
        if cells > self.policy.max_cells_per_block
            && self
                .policy
                .cells_aggregated(cells, block.inner.intersection_fraction(&self.visible))
        {
            let inset = (2.0 / self.zoom).clamp(0.5, 6.0);
            let header = CARD_HEADER.min(block.inner.h * 0.32);
            let aggregate = Rect::new(
                block.inner.x + inset,
                block.inner.y + header + inset,
                (block.inner.w - inset * 2.0).max(1.0),
                (block.inner.h - header - inset * 2.0).max(1.0),
            );
            self.sink.fg_quad(fill(
                self.screen_bounds(&aggregate),
                self.pod_paint[severity_index(block.ext.rollup)],
            ));
            self.stats.aggregated_blocks += 1;
            self.stats.aggregated_cells += cells;
            self.stats.quads += 1;
            return;
        }

        if DIRECT {
            let visible = self.visible;
            if scene.block_cell_index_is_selective(block_index, &visible) {
                scene.for_each_block_cell_candidate(block_index, &visible, |_, cell| {
                    self.cell(cell, block_inside);
                });
            } else {
                for cell in &scene.cells[block.children.start as usize..block.children.end as usize]
                {
                    self.cell(cell, block_inside);
                }
            }
        } else {
            let visible = self.visible;
            scene.for_each_block_cell_candidate(block_index, &visible, |_, cell| {
                self.cell(cell, block_inside);
            });
        }
    }

    fn satellite(
        &mut self,
        satellite_index: usize,
        satellite: &SatNode,
        block_inside: bool,
        hub: (f32, f32),
    ) {
        if !(block_inside || satellite.rect.intersects(&self.visible))
            || !self.policy.sat_painted(satellite.rect.w, self.zoom)
        {
            return;
        }
        self.stats.drawn_sats += 1;
        let (world_x, world_y) = satellite.rect.center();
        let (x, y) = self
            .camera
            .w2s(world_x, world_y, self.viewport.0, self.viewport.1);
        let point = (self.origin.0 + x, self.origin.1 + y);

        if self.policy.sat_icon_shown() {
            push_icon(&mut self.stats, self.policy, self.sink, || {
                IconJob::Sat(
                    satellite.ext.kind,
                    Bounds {
                        origin: gpui::point(
                            px(point.0 - SAT_ICON_PX * 0.5),
                            px(point.1 - SAT_ICON_PX * 0.5),
                        ),
                        size: size(px(SAT_ICON_PX), px(SAT_ICON_PX)),
                    },
                )
            });
        }

        if self.policy.sat_label_shown(satellite.rect.w, self.zoom) {
            let (x, y) = self.camera.w2s(
                satellite.rect.x,
                satellite.rect.max_y(),
                self.viewport.0,
                self.viewport.1,
            );
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&satellite.label),
                x: self.origin.0 + x - 8.0,
                y: self.origin.1 + y + 2.0,
                size_px: SAT_NAME_PX,
                color: gpui::Rgba {
                    r: 0.80,
                    g: 0.75,
                    b: 0.90,
                    a: self.cell_alpha,
                },
            });
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&satellite.ext.detail),
                x: self.origin.0 + x - 8.0,
                y: self.origin.1 + y + 2.0 + SAT_NAME_PX * 1.25,
                size_px: SAT_DETAIL_PX,
                color: gpui::Rgba {
                    r: 0.62,
                    g: 0.57,
                    b: 0.74,
                    a: self.cell_alpha,
                },
            });
        }

        if self.policy.sat_curves {
            if self.stats.curves >= self.policy.curve_budget() {
                self.stats.curves_dropped += 1;
            } else {
                self.stats.curves += 1;
                let control = curve_ctrl(hub, point, bow_jitter(satellite_index as u64));
                self.sink.curve(hub, control, point);
            }
        }
    }

    #[inline(always)]
    fn cell(&mut self, cell: &PodNode, block_inside: bool) {
        if !(block_inside || cell.rect.intersects(&self.visible)) {
            return;
        }
        self.stats.drawn_cells += 1;
        self.sink.fg_quad(fill(
            self.screen_bounds(&cell.rect),
            self.pod_paint[severity_index(cell.ext.state.severity)],
        ));
        self.stats.quads += 1;

        if self.stage >= 3 && self.policy.cell_label_shown(cell.rect.w, self.zoom) {
            let (x, y) = self.camera.w2s(
                cell.rect.x,
                cell.rect.y + cell.rect.h,
                self.viewport.0,
                self.viewport.1,
            );
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&cell.label),
                x: self.origin.0 + x,
                y: self.origin.1 + y + 2.0,
                size_px: POD_LABEL_PX,
                color: gpui::Rgba {
                    r: 0.55,
                    g: 0.51,
                    b: 0.66,
                    a: self.cell_label_alpha,
                },
            });
        }
    }

    fn hierarchy<const DIRECT: bool>(&mut self, scene: &SceneSnapshot) {
        if DIRECT && !scene.region_index_is_selective(&self.visible) {
            for (index, region) in scene.regions.iter().enumerate() {
                if region.rect.intersects(&self.visible) {
                    self.paint_region(region);
                    if self.stage != 0 {
                        self.walk_region_children::<true>(scene, index, region);
                    }
                }
            }
        } else {
            let visible = self.visible;
            scene.for_each_region_candidate(&visible, |index, region| {
                if region.rect.intersects(&visible) {
                    self.paint_region(region);
                    if self.stage != 0 {
                        self.walk_region_children::<DIRECT>(scene, index, region);
                    }
                }
            });
        }
    }

    fn finish(mut self, scene: &SceneSnapshot) -> CullStats {
        if self.hex_shown {
            // The clamp can grow the grid pitch; ring vertices must use the
            // same radius as the centers or clamped frames draw gapped hexes.
            let radius = hex::effective_radius(&self.visible, hex::level(self.zoom).0);
            self.stats.bg_cells =
                hex::for_each_center(&self.visible, radius, |center_x, center_y| {
                    let mut ring = [(0.0f32, 0.0f32); 6];
                    for (index, vertex) in ring.iter_mut().enumerate() {
                        let angle = index as f32 * std::f32::consts::FRAC_PI_3;
                        let world = (
                            center_x + radius * angle.cos(),
                            center_y + radius * angle.sin(),
                        );
                        let screen =
                            self.camera
                                .w2s(world.0, world.1, self.viewport.0, self.viewport.1);
                        *vertex = (self.origin.0 + screen.0, self.origin.1 + screen.1);
                    }
                    self.sink.hex_ring(&ring);
                });
        }

        if self.edges_on && self.stage >= 2 && !self.policy.stress && !self.policy.stress_curves {
            self.stats.edges =
                k10s_atlas::walk_edges(scene, &self.visible, self.policy.max_edges, |a, b| {
                    let hash = ((a.0.to_bits() as u64) << 32 ^ a.1.to_bits() as u64)
                        ^ ((b.0.to_bits() as u64) << 16 ^ b.1.to_bits() as u64);
                    let a = self.camera.w2s(a.0, a.1, self.viewport.0, self.viewport.1);
                    let b = self.camera.w2s(b.0, b.1, self.viewport.0, self.viewport.1);
                    let start = (self.origin.0 + a.0, self.origin.1 + a.1);
                    let end = (self.origin.0 + b.0, self.origin.1 + b.1);
                    let control = curve_ctrl(start, end, bow_jitter(hash) * 0.6);
                    self.sink.edge(start, control, end);
                });
        }
        self.stats
    }
}

#[inline(always)]
const fn severity_index(severity: Severity) -> usize {
    match severity {
        Severity::Ok => 0,
        Severity::Warn => 1,
        Severity::Err => 2,
        Severity::Unknown => 3,
    }
}

pub fn walk<S: FrameSink>(
    bounds: Bounds<Pixels>,
    scene: &SceneSnapshot,
    camera: Camera,
    blend: StageBlend,
    opts: FrameOpts<'_>,
    sink: &mut S,
) -> CullStats {
    const SEVERITIES: [Severity; 4] = [
        Severity::Ok,
        Severity::Warn,
        Severity::Err,
        Severity::Unknown,
    ];
    let viewport = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let stage = blend.walk_stage();
    let block_alpha = blend.stage_alpha(1);
    let cell_alpha = blend.stage_alpha(2);
    let workload_paint = SEVERITIES.map(|severity| {
        let (fill, border) = workload_colors(severity);
        (
            scale_alpha(fill, block_alpha).into(),
            scale_alpha(border, block_alpha).into(),
        )
    });

    sink.bg_quad(fill(bounds, rgb(BG)));
    let mut walk = FrameWalk {
        camera,
        visible: camera.visible_world(viewport.0, viewport.1),
        viewport,
        origin,
        zoom: camera.zoom,
        stage,
        z01_t: if stage == 0 {
            0.0
        } else if blend.from.min(blend.to) >= 1 {
            1.0
        } else {
            blend.fade_alpha()
        },
        block_alpha,
        cell_alpha,
        cell_label_alpha: blend.stage_alpha(3),
        policy: opts.policy,
        skip_blocks: opts.skip_blocks,
        hex_shown: opts.hex_shown(),
        edges_on: opts.edges_on,
        ns_border: rgb(NS_BORDER).into(),
        ns_fill: rgb(NS_FILL).into(),
        ns_fill_rgba: rgb(NS_FILL),
        ns_border_rgba: rgb(NS_BORDER),
        header_fill: scale_alpha(rgb(CARD_HEADER_FILL), block_alpha).into(),
        workload_paint,
        pod_paint: SEVERITIES.map(|severity| scale_alpha(pod_color(severity), cell_alpha).into()),
        strip_paint: SEVERITIES
            .map(|severity| scale_alpha(pod_color(severity), block_alpha).into()),
        sink,
        stats: CullStats {
            stage,
            quads: 1,
            ..CullStats::default()
        },
    };
    if scene.child_ranges_are_direct() {
        walk.hierarchy::<true>(scene);
    } else {
        walk.hierarchy::<false>(scene);
    }
    walk.finish(scene)
}

pub struct PaintSink<'a> {
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

pub struct FramePaths {
    pub(crate) hex: PathBuilder,
    pub(crate) edges: PathBuilder,
    pub(crate) curve_core: PathBuilder,
    pub(crate) curve_glow: PathBuilder,
    pub(crate) glow: bool,
}

impl<'a> PaintSink<'a> {
    pub fn new(
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

    pub fn into_paths(self) -> FramePaths {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use k10s_core::{
        NsExt, NsNode, PodExt, PodNode, ReasonId, SatExt, SatNode, State, WlExt, WorkloadNode,
    };

    use super::*;
    use crate::lod::{Knobs, policy};
    use k10s_core::SceneData;

    const INLINE_CAP: usize = 23;

    const VW: f32 = 1600.0;
    const VH: f32 = 1000.0;

    const CAMERA: Camera = Camera {
        cx: 50.0,
        cy: 50.0,
        zoom: 4.0,
    };

    fn viewport() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(VW), px(VH)),
        }
    }

    fn scene(cell_label: &str) -> SceneSnapshot {
        SceneSnapshot {
            ids: Default::default(),
            scene: SceneData {
                rev: 1,
                bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                regions: vec![NsNode {
                    rect: Rect::new(0.0, 0.0, 100.0, 100.0),
                    label: Arc::from("payments-production-eu-west"),
                    weight: 1,
                    children: 0..1,
                    ext: NsExt {
                        unhealthy_frac: 0.25,
                        rollup: Severity::Warn,
                    },
                }],
                blocks: vec![WorkloadNode {
                    rect: Rect::new(10.0, 10.0, 60.0, 60.0),
                    inner: Rect::new(10.0, 10.0, 60.0, 60.0),
                    label: Arc::from("checkout-api-canary-rollout"),
                    children: 0..1,
                    sats: 0..1,
                    ext: WlExt {
                        kind: KindId::DEPLOYMENT,
                        tool: ToolId::NONE,
                        rollup: Severity::Ok,
                        ns: 0,
                    },
                }],
                cells: vec![PodNode {
                    rect: Rect::new(12.0, 12.0, 20.0, 20.0),
                    label: Arc::from(cell_label),
                    ext: PodExt {
                        state: State::of(ReasonId::RUNNING),
                    },
                }],
                sats: vec![SatNode {
                    rect: Rect::new(75.0, 20.0, 10.0, 10.0),
                    label: Arc::from("checkout-api-primary-service"),
                    ext: SatExt {
                        kind: KindId::SERVICE,
                        detail: Arc::from("ClusterIP 10.96.0.1:8443/tcp"),
                    },
                }],
                ..SceneData::default()
            },
        }
    }

    #[derive(Default)]
    struct Collect {
        labels: Vec<LabelJob>,
    }

    impl FrameSink for Collect {
        fn bg_quad(&mut self, _: PaintQuad) {}
        fn fg_quad(&mut self, _: PaintQuad) {}
        fn label(&mut self, label: LabelJob) {
            self.labels.push(label);
        }
        fn icon(&mut self, _: IconJob) {}
        fn hex_ring(&mut self, _: &[(f32, f32); 6]) {}
        fn curve(&mut self, _: (f32, f32), _: (f32, f32), _: (f32, f32)) {}
        fn edge(&mut self, _: (f32, f32), _: (f32, f32), _: (f32, f32)) {}
    }

    fn walk_labels(scene: &SceneSnapshot) -> Collect {
        let pol = policy(Knobs::default());
        let opts = FrameOpts {
            policy: &pol,
            edges_on: false,
            skip_blocks: false,
            hex: false,
        };
        let blend = StageBlend::settled(pol.stage_for_zoom(CAMERA.zoom));
        let mut sink = Collect::default();
        let st = walk(viewport(), scene, CAMERA, blend, opts, &mut sink);
        assert_eq!(
            st.labels,
            sink.labels.len(),
            "the sink and the counter disagree"
        );
        sink
    }

    #[test]
    fn every_label_site_shares_the_scenes_arc() {
        let scene = scene("checkout-api-7f9c8d6b5-tzq4x");
        let sink = walk_labels(&scene);
        assert_eq!(sink.labels.len(), 5, "the fixture must fire all five sites");

        for (site, label) in [
            ("region", &scene.regions[0].label),
            ("block", &scene.blocks[0].label),
            ("cell", &scene.cells[0].label),
            ("satellite", &scene.sats[0].label),
            ("satellite detail", &scene.sats[0].ext.detail),
        ] {
            assert!(
                label.len() > INLINE_CAP,
                "{site}: a {}-byte fixture label inlines, so it proves nothing",
                label.len()
            );
            assert_eq!(
                Arc::strong_count(label),
                2,
                "{site}: the label was copied instead of shared"
            );
        }
        drop(sink);
    }

    #[test]
    fn a_label_shares_the_arc_only_past_the_inline_cap() {
        for (len, strong) in [(INLINE_CAP, 1), (INLINE_CAP + 1, 2)] {
            let scene = scene(&"p".repeat(len));
            let sink = walk_labels(&scene);
            assert_eq!(
                Arc::strong_count(&scene.cells[0].label),
                strong,
                "{len}-byte cell label"
            );
            drop(sink);
        }
    }
}
