use gpui::{
    Background, Bounds, Corners, PaintQuad, PathBuilder, Pixels, SharedString, fill,
    linear_color_stop, linear_gradient, point, px, quad, rgb, size,
};
use k10s_atlas::curves::{bow_jitter, curve_ctrl, dash_quadratic};
use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend, WorkloadPresentation};
use k10s_core::layout::{NS_HEADER, NS_PAD};
use k10s_core::{
    KindId, NsNode, PodNode, ReasonId, Rect, SatNode, SceneSnapshot, Severity, ToolId, WorkloadNode,
};

use crate::hex;
use crate::primitive::{ISLAND_DETAIL_MIN_PX, Projection, card, pod, region as region_primitive};
#[cfg(test)]
use crate::primitive::{SAT_ICON_MAX_PX, WL_ICON_MAX_PX, WL_MEDALLION_MIN_PX, island_radii};
use k10s_theme::{HeatRamp, MapTheme, MapType, mix, quantize, scale_alpha};

const CURVE_DASH_ON: f32 = 6.0;
const CURVE_DASH_OFF: f32 = 5.0;
const CURVE_TOL: f32 = 0.35;
const CURVE_CORE_W: f32 = 1.5;
const CURVE_GLOW_W: f32 = 5.0;

// The label's box, as a multiple of the satellite it belongs to: a satellite is
// eighteen world units and its name is not, so the name is centred in a box wide
// enough to be worth centring in.
const SAT_LABEL_BOX: f32 = 5.0;

// Fill gradients. Both are the flat colour at the top mixed toward the canvas at
// the bottom, so a theme that changes one colour changes the shading with it and
// no second token has to be kept in step.
//
// An island is shaded only once it is big enough for shading to be a thing you
// can see. This is a measured threshold, not a taste one: a gradient stop is
// `Rgba -> Hsla`, the conversion is branchy, and the island's fill is the heat
// colour so it cannot be hoisted out of the region loop the way the card's four
// can. Shading every island cost **2.1x on `walk_count` at the Z0 fit camera**
// with four hundred regions -- the one walk SS6.1 budgets at O(regions) -- for a
// gradient across forty pixels that nobody can see. Above the threshold the
// region count is bounded by the viewport, which is the whole doctrine.
const ISLAND_SHADE: f32 = 0.42;
const CARD_SHADE: f32 = 0.30;
const GRADIENT_ANGLE: f32 = 168.0;

#[derive(Debug, Clone, Copy)]
pub struct FrameOpts<'a> {
    pub policy: &'a LodPolicy,
    pub theme: &'a MapTheme,
    /// The map's type ladder, resolved from the user's typography once per
    /// frame. A `Copy` struct of scalars for the same reason `MapTheme` is.
    pub type_: MapType,
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

/// Which typeface a label is set in. Namespace names are display type -- they
/// are read at a glance from across the map, not scanned -- and everything else
/// is the interface face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelFace {
    Ui,
    Display,
}

pub struct LabelJob {
    pub text: SharedString,
    /// The left edge of the box the line is placed in, already carrying the
    /// frame origin.
    pub x: f32,
    pub y: f32,
    pub size_px: f32,
    pub color: gpui::Rgba,
    pub face: LabelFace,
    /// The width of that box, in screen pixels. Zero means "set it left at `x`
    /// and let it run", which is what a pod cell wants; anything else centres
    /// the line in the box and clips it to it, so a forty-character workload
    /// name stops at the edge of its own card instead of crossing the next one.
    pub width: f32,
}

pub enum IconJob {
    Wl(KindId, crate::MarkPrimitive),
    ToolId(ToolId, crate::MarkPrimitive),
    Sat(KindId, crate::MarkPrimitive),
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
    policy: &'a LodPolicy,
    type_: MapType,
    heat: HeatRamp,
    skip_blocks: bool,
    hex_shown: bool,
    edges_on: bool,
    ns_border: gpui::Hsla,
    ns_fill: gpui::Background,
    ns_fill_shaded: gpui::Background,
    ns_fill_rgba: gpui::Rgba,
    ns_border_rgba: gpui::Rgba,
    // The canvas colour, hoisted because every gradient's far stop is its own
    // fill mixed toward it: one token drives the shading of every surface.
    bg_rgba: gpui::Rgba,
    // The header band THIS scene's layout reserved above a pod grid, carried on
    // the snapshot because the two layout modes reserve different ones and no
    // card's geometry reveals which. Guessing it from the card's height drew the
    // header over the first row of pods in whichever mode was not guessed for.
    card_header: f32,
    // The shading threshold stays pre-divided because the region loop already
    // has the world-space short side and should not do another conversion.
    island_detail_min: f32,
    header_fill: gpui::Background,
    // Pod fills paired with the border a terminating pod is drawn with instead.
    // A pod on its way out is hollow rather than a different colour: it is the
    // same severity it always was, and inventing a fifth severity colour for a
    // transition would put a colour on the map that means two things.
    pod_hollow: [gpui::Hsla; 4],
    // Label colours, channel-converted and alpha-applied once at walk setup.
    // A per-region loop must not touch the theme struct: that is the hoist the
    // 14-18% regression bought, and these are text colours, not exceptions.
    region_label: gpui::Rgba,
    workload_label: gpui::Rgba,
    sat_label: gpui::Rgba,
    sat_detail_label: gpui::Rgba,
    pod_label: gpui::Rgba,
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
    fn projection(&self) -> Projection {
        Projection::new(self.camera, self.viewport, self.origin)
    }

    #[inline]
    fn paint_region(&mut self, region: &NsNode) {
        self.stats.drawn_regions += 1;
        // The island's short side, in WORLD units, compared against thresholds
        // already divided into world units at walk setup. This loop runs once
        // per namespace in the cluster at the fit camera, so it asks both of its
        // questions with one comparison each and no conversion at all.
        let short = if region.rect.w < region.rect.h {
            region.rect.w
        } else {
            region.rect.h
        };
        let detailed = short >= self.island_detail_min;
        let primitive = region_primitive(
            region.rect,
            self.screen_bounds(&region.rect),
            short * self.zoom,
            detailed,
        );
        let bounds = primitive.bounds;

        // One quad, whatever the stage. The stage decides its colours, the LOD
        // cross-fade interpolates between them, and the silhouette is the same
        // island throughout, so zooming in on a namespace never changes its
        // shape under the pointer.
        //
        // Three arms rather than one and a pair of variables, and the reason is
        // measured: folding them into `(fill, border)` unifies the border's type
        // across the arms, which turned the settled `Hsla` border into an
        // `Hsla -> Rgba -> Hsla` round trip for every region at the fit camera.
        // That conversion is branchy and this loop runs once per namespace in
        // the cluster; it cost more than everything else in the redesign put
        // together.
        // A hairline around an island that fills a third of the screen does not
        // read as a coastline; around forty pixels of island it is all there is.
        let edge = px(if detailed { 1.6 } else { 1.0 });
        let radii = primitive.corners;
        if self.z01_t <= 0.0 {
            let heat = self.heat.color(region.ext.unhealthy_frac);
            self.sink.bg_quad(quad(
                bounds,
                radii,
                if detailed {
                    shade(heat, self.bg_rgba, ISLAND_SHADE)
                } else {
                    heat.into()
                },
                edge,
                self.ns_border,
                Default::default(),
            ));
        } else if self.z01_t >= 1.0 {
            self.sink.bg_quad(quad(
                bounds,
                radii,
                if detailed {
                    self.ns_fill_shaded
                } else {
                    self.ns_fill
                },
                edge,
                self.heat.border(region.ext.unhealthy_frac),
                Default::default(),
            ));
        } else {
            let fill = mix(
                self.heat.color(region.ext.unhealthy_frac),
                self.ns_fill_rgba,
                self.z01_t,
            );
            self.sink.bg_quad(quad(
                bounds,
                radii,
                if detailed {
                    shade(fill, self.bg_rgba, ISLAND_SHADE)
                } else {
                    fill.into()
                },
                edge,
                mix(
                    self.ns_border_rgba,
                    self.heat.border(region.ext.unhealthy_frac),
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
            let width = f32::from(bounds.size.width);
            // Display type, sized to the island rather than to a constant: a
            // namespace filling the screen says its name at the size it
            // deserves, and one the size of a postage stamp does not shout.
            //
            // The second bound is the title band both layout engines leave above
            // the first card. Without it a wide island at low zoom sets its name
            // at forty pixels straight through the workloads underneath, which
            // is the failure a semi-transparent watermark hides and a legible
            // label cannot.
            let band = (NS_HEADER + NS_PAD) * self.zoom;
            let size_px = quantize(
                (width * 0.055)
                    .min(band * 0.8)
                    .clamp(self.type_.region_min, self.type_.region_max),
            );
            let line = size_px * self.type_.line_height;
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&region.label),
                x: self.origin.0 + x,
                y: self.origin.1 + y + (band - line).max(0.0) * 0.5,
                size_px,
                color: self.region_label,
                face: LabelFace::Display,
                width,
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
        let presentation = if self.skip_blocks {
            WorkloadPresentation::Hidden
        } else {
            self.policy
                .workload_presentation(block.inner.w, self.zoom, self.stage)
        };
        let header_height = self.card_header.min(block.inner.h * 0.5);
        let chrome = presentation == WorkloadPresentation::Detailed
            && self.policy.block_chrome_shown(block.inner.w, self.zoom);
        if presentation != WorkloadPresentation::Hidden {
            self.stats.drawn_blocks += 1;
        }
        if presentation.card_shown() {
            let severity = severity_index(block.ext.rollup);
            let (fill_color, border) = self.workload_paint[severity];
            let primitive = card(self.screen_bounds(&block.inner));
            let radius = primitive.corners.top_left;
            self.sink.fg_quad(quad(
                primitive.bounds,
                primitive.corners,
                fill_color,
                px(1.0),
                border,
                Default::default(),
            ));
            self.stats.quads += 1;

            if chrome {
                // The header keeps the card's top corners and squares off where
                // the pod grid begins, so it reads as part of the card rather
                // than a rectangle laid over one.
                let header = Rect::new(block.inner.x, block.inner.y, block.inner.w, header_height);
                self.sink.fg_quad(quad(
                    self.screen_bounds(&header),
                    Corners {
                        top_left: radius,
                        top_right: radius,
                        bottom_right: px(0.0),
                        bottom_left: px(0.0),
                    },
                    self.header_fill,
                    px(0.0),
                    gpui::transparent_black(),
                    Default::default(),
                ));
                // The severity reading moved from a floating bar inside the
                // header to the card's own bottom edge, where it underlines the
                // whole workload, cannot collide with the glyph or the grid, and
                // is still legible on a card whose header is two pixels tall.
                let strip_h = (block.inner.h * 0.05).clamp(1.5, 4.0);
                let strip = Rect::new(
                    block.inner.x,
                    block.inner.max_y() - strip_h,
                    block.inner.w,
                    strip_h,
                );
                self.sink.fg_quad(quad(
                    self.screen_bounds(&strip),
                    Corners {
                        top_left: px(0.0),
                        top_right: px(0.0),
                        bottom_right: radius,
                        bottom_left: radius,
                    },
                    self.strip_paint[severity],
                    px(0.0),
                    gpui::transparent_black(),
                    Default::default(),
                ));
                self.stats.quads += 2;
            }
        }

        if self.policy.block_icon_shown(block.inner.w, self.zoom) {
            let primitive = match presentation {
                WorkloadPresentation::Medallion => {
                    Some(self.projection().medallion(block.rect, block.inner))
                }
                WorkloadPresentation::Detailed => {
                    Some(self.projection().header_icon(block.inner, header_height))
                }
                WorkloadPresentation::Hidden | WorkloadPresentation::Card => None,
            };
            if let Some(primitive) = primitive {
                push_icon(&mut self.stats, self.policy, self.sink, || {
                    if block.ext.tool != ToolId::NONE {
                        IconJob::ToolId(block.ext.tool, primitive)
                    } else {
                        IconJob::Wl(block.ext.kind, primitive)
                    }
                });
            }
        }

        if presentation.card_shown() && self.policy.block_label_shown(block.inner.w, self.zoom) {
            let card_w = block.inner.w * self.zoom;
            let header_px = header_height * self.zoom;
            let (x, y) = self.camera.w2s(
                block.inner.x,
                block.inner.y,
                self.viewport.0,
                self.viewport.1,
            );
            // The name takes the header minus the square the glyph occupies
            // on its left, and is centred and clipped inside what is left,
            // so a forty-character name stops at its own card.
            let inset = if chrome { header_px } else { 0.0 };
            let width = (card_w - inset - header_px * 0.25).max(1.0);
            let size_px = self.type_.workload;
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&block.label),
                x: self.origin.0 + x + inset,
                y: self.origin.1
                    + y
                    + (header_px - size_px * self.type_.line_height).max(0.0) * 0.5,
                size_px,
                color: self.workload_label,
                face: LabelFace::Ui,
                width,
            });
        }

        if self.stage < 2 {
            return;
        }
        let block_inside = region_inside || self.visible.contains(&block.rect);
        if presentation != WorkloadPresentation::Hidden || self.policy.stress_curves {
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

        if !presentation.cells_shown() {
            return;
        }
        let cells = block.children.len();
        if cells > self.policy.max_cells_per_block
            && self
                .policy
                .cells_aggregated(cells, block.inner.intersection_fraction(&self.visible))
        {
            let inset = (2.0 / self.zoom).clamp(0.5, 6.0);
            let header = self.card_header.min(block.inner.h * 0.5);
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

        let cell_px = satellite.rect.w * self.zoom;
        if self.policy.sat_icon_shown() {
            // Sized to the satellite instead of pinned at fifteen pixels. There
            // is no quad behind a satellite -- the glyph IS the satellite -- so
            // a constant size meant it was the same size whether it was the
            // thing you were looking at or one of ninety in the background.
            let primitive = self.projection().satellite(satellite.rect);
            push_icon(&mut self.stats, self.policy, self.sink, || {
                IconJob::Sat(satellite.ext.kind, primitive)
            });
        }

        if self.policy.sat_label_shown(satellite.rect.w, self.zoom) {
            let (_, y) = self.camera.w2s(
                satellite.rect.x,
                satellite.rect.max_y(),
                self.viewport.0,
                self.viewport.1,
            );
            // Two lines centred under the glyph in a box wide enough to hold a
            // PVC name, clipped to it. They used to hang off a fixed eight-pixel
            // offset from the satellite's left edge, which is why a claim called
            // `pvc/data-payments-redis-49` ran through its neighbour.
            let width = (cell_px * SAT_LABEL_BOX).max(self.type_.sat * 6.0);
            // `point` is the satellite's centre and already carries the frame
            // origin; adding it again is what `every_placement_carries_the_frame_origin`
            // caught here on the first run.
            let left = point.0 - width * 0.5;
            let name = self.type_.sat;
            let detail = self.type_.sat_detail;
            let top = self.origin.1 + y + name * 0.35;
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&satellite.label),
                x: left,
                y: top,
                size_px: name,
                color: self.sat_label,
                face: LabelFace::Ui,
                width,
            });
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&satellite.ext.detail),
                x: left,
                y: top + name * self.type_.line_height,
                size_px: detail,
                color: self.sat_detail_label,
                face: LabelFace::Ui,
                width,
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
        let severity = severity_index(cell.ext.state.severity);
        let primitive = pod(self.screen_bounds(&cell.rect));
        if cell.ext.state.reason == ReasonId::TERMINATING {
            // A pod inside its termination grace period is still there and still
            // counted, and it is on its way out. Drawn hollow it says both, in
            // the colour it already had -- which matters most on a scale-down,
            // where the grace window is the whole of the delay between asking
            // for eight replicas and the card coming down to eight.
            self.sink.fg_quad(quad(
                primitive.bounds,
                primitive.corners,
                gpui::transparent_black(),
                px(1.0),
                self.pod_hollow[severity],
                Default::default(),
            ));
        } else {
            self.sink.fg_quad(quad(
                primitive.bounds,
                primitive.corners,
                self.pod_paint[severity],
                px(0.0),
                gpui::transparent_black(),
                Default::default(),
            ));
        }
        self.stats.quads += 1;

        if self.stage >= 3 && self.policy.cell_label_shown(cell.rect.w, self.zoom) {
            let (x, y) = self.camera.w2s(
                cell.rect.x,
                cell.rect.y + cell.rect.h,
                self.viewport.0,
                self.viewport.1,
            );
            let size_px = self.type_.pod;
            push_label(&mut self.stats, self.policy, self.sink, || LabelJob {
                text: SharedString::from(&cell.label),
                x: self.origin.0 + x,
                y: self.origin.1 + y + 2.0,
                size_px,
                color: self.pod_label,
                face: LabelFace::Ui,
                width: 0.0,
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

/// A fill mixed toward the canvas along the map's light direction.
///
/// One call, no second theme token: every surface shades from its own colour
/// toward `bg`, so recolouring the canvas recolours every gradient with it and
/// there is no pair of values that can drift apart.
#[inline]
fn shade(fill: gpui::Rgba, bg: gpui::Rgba, amount: f32) -> Background {
    linear_gradient(
        GRADIENT_ANGLE,
        linear_color_stop(fill, 0.0),
        linear_color_stop(mix(fill, bg, amount), 1.0),
    )
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
    let bg_for_shade = rgb(opts.theme.bg);
    let workload_paint = SEVERITIES.map(|severity| {
        let (fill, border) = opts.theme.workload_colors(severity);
        (
            shade(scale_alpha(fill, block_alpha), bg_for_shade, CARD_SHADE),
            scale_alpha(border, block_alpha).into(),
        )
    });

    let bg_rgba = rgb(opts.theme.bg);
    // The canvas is one quad and always has been; it is now shaded rather than
    // flat, which costs the same quad and gives the hex field somewhere to sit.
    sink.bg_quad(fill(bounds, shade(bg_rgba, rgb(opts.theme.hex_line), 0.35)));
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
        policy: opts.policy,
        type_: opts.type_,
        heat: opts.theme.heat_ramp(),
        skip_blocks: opts.skip_blocks,
        hex_shown: opts.hex_shown(),
        edges_on: opts.edges_on,
        ns_border: rgb(opts.theme.ns_border).into(),
        ns_fill: rgb(opts.theme.ns_fill).into(),
        ns_fill_shaded: shade(rgb(opts.theme.ns_fill), bg_rgba, ISLAND_SHADE),
        ns_fill_rgba: rgb(opts.theme.ns_fill),
        ns_border_rgba: rgb(opts.theme.ns_border),
        bg_rgba,
        card_header: scene.card_header,
        island_detail_min: ISLAND_DETAIL_MIN_PX / camera.zoom,
        header_fill: shade(
            scale_alpha(rgb(opts.theme.card_header_fill), block_alpha),
            bg_rgba,
            CARD_SHADE,
        ),
        pod_hollow: SEVERITIES
            .map(|severity| scale_alpha(opts.theme.pod_color(severity), cell_alpha).into()),
        region_label: rgb(opts.theme.region_label),
        workload_label: scale_alpha(rgb(opts.theme.workload_label), block_alpha),
        sat_label: scale_alpha(rgb(opts.theme.sat_label), cell_alpha),
        sat_detail_label: scale_alpha(rgb(opts.theme.sat_detail_label), cell_alpha),
        pod_label: scale_alpha(rgb(opts.theme.pod_label), blend.stage_alpha(3)),
        workload_paint,
        pod_paint: SEVERITIES
            .map(|severity| scale_alpha(opts.theme.pod_color(severity), cell_alpha).into()),
        strip_paint: SEVERITIES
            .map(|severity| scale_alpha(opts.theme.pod_color(severity), block_alpha).into()),
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
#[path = "frame_test.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "frame_visual_test.rs"]
mod visual_tests;
