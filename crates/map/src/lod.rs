use std::sync::OnceLock;

use gpui::{Bounds, point, px, size};
use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
use k10s_core::{Endpoint, Level, Rect, SceneSnapshot};

use crate::frame::FrameOpts;

pub const STAGE_WL: f32 = 0.09;
pub const STAGE_POD: f32 = crate::primitive::POD_MARK_MIN / k10s_core::layout::POD_SIZE;
pub const STAGE_POD_LABEL: f32 =
    crate::primitive::POD_DETAIL_MIN_WIDTH / k10s_core::layout::POD_SIZE;
pub(crate) const MAX_USEFUL_ZOOM: f32 =
    crate::primitive::POD_DETAIL_MAX_WIDTH / k10s_core::layout::POD_SIZE;

pub const STAGE_EXIT: f32 = 0.85;
pub const STAGE_FADE_SECS: f32 = 0.18;

pub const WL_MIN_PX: f32 = 4.0;
// The same threshold as `WL_MIN_PX`, deliberately: if a workload is drawn at
// all, it wears the mark that says what it is. A five-pixel rectangle in a
// severity colour is not an identity, and a namespace full of them is the state
// this redesign existed to fix. How big the glyph then gets is decided by the
// space the workload owns, not here.
pub const WL_ICON_MIN_PX: f32 = 4.0;
pub const WL_CHROME_MIN_PX: f32 = crate::primitive::NAME_MIN_WIDTH;
pub const NS_LABEL_MIN_PX: f32 = 70.0;
pub const WL_LABEL_MIN_PX: f32 = crate::primitive::NAME_MIN_WIDTH;
pub const WL_LABEL_MIN_ZOOM: f32 = 0.0;
pub const POD_LABEL_MIN_PX: f32 = crate::primitive::POD_DETAIL_MIN_WIDTH;
pub const SAT_MIN_PX: f32 = 5.0;
pub const SAT_LABEL_MIN_PX: f32 = 30.0;

pub const MAX_LABELS: usize = 400;
pub const MAX_ICONS: usize = 1024;
pub const MAX_EDGES: usize = 3000;
pub const MAX_CURVES: usize = 1500;
pub const MAX_CELLS_PER_BLOCK: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Knobs {
    pub stress_quads: bool,
    pub stress_curves: bool,
    pub no_curves: bool,
    pub no_icons: bool,
}

impl Knobs {
    fn from_env() -> Self {
        let on = |name: &str| std::env::var_os(name).is_some_and(|v| v != "0");
        Knobs {
            stress_quads: on("K10S_STRESS_QUADS"),
            stress_curves: on("K10S_STRESS_CURVES"),
            no_curves: on("K10S_NO_CURVES"),
            no_icons: on("K10S_NO_ICONS"),
        }
    }
}

pub(crate) fn policy(knobs: Knobs) -> LodPolicy {
    LodPolicy {
        stage_block: STAGE_WL,
        stage_cell: STAGE_POD,
        stage_cell_label: STAGE_POD_LABEL,
        block_min_px: WL_MIN_PX,
        block_icon_min_px: if knobs.no_icons {
            f32::INFINITY
        } else {
            WL_ICON_MIN_PX
        },
        region_label_min_px: NS_LABEL_MIN_PX,
        block_label_min_px: WL_LABEL_MIN_PX,
        block_label_min_zoom: WL_LABEL_MIN_ZOOM,
        cell_label_min_px: POD_LABEL_MIN_PX,
        block_chrome_min_px: WL_CHROME_MIN_PX,
        stage_exit: STAGE_EXIT,
        sat_min_px: SAT_MIN_PX,
        sat_label_min_px: SAT_LABEL_MIN_PX,
        max_labels: MAX_LABELS,
        max_icons: MAX_ICONS,
        max_edges: MAX_EDGES,
        max_curves: MAX_CURVES,
        max_cells_per_block: MAX_CELLS_PER_BLOCK,
        sat_curves: !knobs.no_curves,
        stress: knobs.stress_quads,
        stress_curves: knobs.stress_curves && !knobs.stress_quads,
    }
}

pub fn lod() -> &'static LodPolicy {
    static LOD: OnceLock<LodPolicy> = OnceLock::new();
    LOD.get_or_init(|| policy(Knobs::from_env()))
}

pub fn stage_for_zoom(zoom: f32) -> u8 {
    lod().stage_for_zoom(zoom)
}

pub fn cull(
    scene: &SceneSnapshot,
    camera: &Camera,
    blend: StageBlend,
    vw: f32,
    vh: f32,
    opts: FrameOpts<'_>,
) -> CullStats {
    // The independent endpoint walk below supplies the edge count.
    let mut st = k10s_atlas::cull(
        scene,
        camera,
        opts.policy,
        blend,
        vw,
        vh,
        false,
        opts.skip_blocks,
    );
    let visible = camera.visible_world(vw, vh);
    reference_content(scene, *camera, blend.walk_stage(), (vw, vh), opts, &mut st);
    st.bg_cells = crate::hex::visible_count(&visible, camera.zoom, !opts.hex_shown());
    st.edges = if opts.edges_on && blend.walk_stage() >= 2 && !opts.stress_any() {
        reference_edges(scene, &visible, opts.policy.max_edges)
    } else {
        0
    };
    st
}

fn reference_content(
    scene: &SceneSnapshot,
    camera: Camera,
    stage: u8,
    viewport: (f32, f32),
    opts: FrameOpts<'_>,
    st: &mut CullStats,
) {
    use crate::primitive::{
        POD_DETAIL_MIN_WIDTH, POD_MARK_MIN, Projection, TEXT_PAD, name_band_height,
    };
    use k10s_atlas::WorkloadPresentation;
    use k10s_core::layout::{NS_HEADER, NS_PAD};

    let policy = opts.policy;
    let type_ = opts.type_;
    let zoom = camera.zoom;
    let visible = camera.visible_world(viewport.0, viewport.1);
    let projection = Projection::new(camera, viewport, (0.0, 0.0));
    st.quads = 1 + st.drawn_regions;
    st.drawn_blocks = 0;
    st.drawn_cells = 0;
    st.aggregated_blocks = 0;
    st.aggregated_cells = 0;
    st.labels = 0;
    st.labels_dropped = 0;
    st.icons = 0;
    st.icons_dropped = 0;

    // This independently counts admitted primitives. It never calls the frame
    // traversal or reads its jobs; the atlas oracle still supplies satellites,
    // curves and the unchanged region cardinality.
    scene.for_each_region_candidate(&visible, |region_index, region| {
        if !region.rect.intersects(&visible) {
            return;
        }
        if policy.region_label_shown(region.rect.w, zoom) {
            let band = (NS_HEADER + NS_PAD) * zoom;
            let width = region.rect.w * zoom;
            let font = k10s_theme::quantize(
                (width * 0.055)
                    .min(band * 0.8)
                    .clamp(type_.region_min, type_.region_max),
            );
            let line = font * type_.line_height;
            let bounds = projection.bounds(region.rect);
            let row = Bounds {
                origin: point(
                    bounds.origin.x,
                    bounds.origin.y + px((band - line).max(0.0) * 0.5),
                ),
                size: size(px(width), px(line)),
            };
            st.labels += usize::from(band >= line && projection.label_visible(row, type_));
        }
        if stage == 0 {
            return;
        }
        let region_inside = visible.contains(&region.rect);
        scene.for_each_region_block_candidate(region_index, &visible, |block_index, block| {
            if !(region_inside || block.rect.intersects(&visible)) {
                return;
            }
            let effective_stage = if stage == 1
                && block.inner.w * zoom >= crate::primitive::name_min_width(type_)
                && block.inner.h * zoom >= name_band_height(type_)
            {
                2
            } else {
                stage
            };
            let presentation = if opts.skip_blocks {
                WorkloadPresentation::Hidden
            } else {
                policy.workload_presentation(block.inner.w, zoom, effective_stage)
            };
            let named = presentation.card_shown()
                && block.inner.w * zoom >= policy.block_label_min_px
                && zoom > policy.block_label_min_zoom;
            let heading = named
                .then(|| projection.name_band(block.inner, type_))
                .flatten();
            let chrome = presentation == WorkloadPresentation::Detailed
                && policy.block_chrome_shown(block.inner.w, zoom);
            if presentation != WorkloadPresentation::Hidden {
                st.drawn_blocks += 1;
            }
            if presentation.card_shown() {
                st.quads += 1;
                if chrome {
                    st.quads += 1 + usize::from(heading.is_some());
                }
            }
            if policy.block_icon_shown(block.inner.w, zoom) {
                st.icons += usize::from(
                    presentation == WorkloadPresentation::Medallion
                        || (presentation == WorkloadPresentation::Detailed && heading.is_some()),
                );
            }
            if heading.is_some() {
                st.labels += 2;
                st.icons += usize::from(policy.sat_icon_shown());
            }
            if stage < 2 {
                return;
            }
            let inside = region_inside || visible.contains(&block.rect);
            if presentation != WorkloadPresentation::Hidden || policy.stress_curves {
                scene.for_each_block_sat(block_index, |_, sat| {
                    if (inside || sat.rect.intersects(&visible))
                        && policy.sat_painted(sat.rect.w, zoom)
                    {
                        st.icons += usize::from(policy.sat_icon_shown());
                        st.labels += 2 * usize::from(policy.sat_label_shown(sat.rect.w, zoom));
                    }
                });
            }
            if !presentation.cells_shown() {
                return;
            }
            let cells = block.children.len();
            if policy.cells_aggregated(cells, block.inner.intersection_fraction(&visible)) {
                let inset = (2.0 / zoom).clamp(0.5, 6.0);
                let header = scene.card_header.min(block.inner.h * 0.5);
                let aggregate = projection.bounds(Rect::new(
                    block.inner.x + inset,
                    block.inner.y + header + inset,
                    (block.inner.w - inset * 2.0).max(1.0),
                    (block.inner.h - header - inset * 2.0).max(1.0),
                ));
                let top = heading.map_or(aggregate.origin.y, |heading| {
                    aggregate
                        .origin
                        .y
                        .max(heading.bounds.bottom() + px(crate::primitive::HEADING_CLEARANCE))
                });
                st.quads += usize::from(aggregate.bottom() > top);
                st.aggregated_blocks += 1;
                st.aggregated_cells += cells;
                return;
            }
            scene.for_each_block_cell_candidate(block_index, &visible, |_, cell| {
                if !(inside || cell.rect.intersects(&visible)) {
                    return;
                }
                let Some(primitive) = projection.cell(cell.rect, stage, type_, heading) else {
                    return;
                };
                st.drawn_cells += 1;
                st.quads += 1;
                let width = f32::from(primitive.bounds.size.width);
                let height = f32::from(primitive.bounds.size.height);
                if policy.sat_icon_shown() && width >= POD_MARK_MIN && height >= POD_MARK_MIN {
                    st.icons += 1;
                }
                if stage >= 3
                    && width >= POD_DETAIL_MIN_WIDTH
                    && policy.cell_label_shown(cell.rect.w, zoom)
                {
                    let line = type_.pod * type_.line_height;
                    let rows = Bounds {
                        origin: point(
                            primitive.bounds.origin.x + px(TEXT_PAD),
                            primitive.bounds.origin.y + px(TEXT_PAD),
                        ),
                        size: size(px(width - TEXT_PAD * 2.0), px(line * 2.0)),
                    };
                    st.labels += 2 * usize::from(projection.label_visible(rows, type_));
                }
            });
        });
    });
    st.labels_dropped = st.labels.saturating_sub(policy.max_labels);
    st.labels = st.labels.min(policy.max_labels);
    st.icons_dropped = st.icons.saturating_sub(policy.max_icons);
    st.icons = st.icons.min(policy.max_icons);
}

fn reference_edges(scene: &SceneSnapshot, visible: &Rect, max_edges: usize) -> usize {
    let rect = |e: Endpoint| {
        let i = e.index() as usize;
        match e.level() {
            Level::Region => scene.regions.get(i).map(|n| n.rect),
            Level::Block => scene.blocks.get(i).map(|n| n.inner),
            Level::Cell => scene.cells.get(i).map(|n| n.rect),
            Level::Sat => scene.sats.get(i).map(|n| n.rect),
        }
    };

    let mut drawn = 0;
    for e in &scene.edges {
        if drawn >= max_edges {
            break;
        }
        let (Some(a), Some(b)) = (rect(e.a), rect(e.b)) else {
            continue;
        };
        let (ax, ay) = a.center();
        let (bx, by) = b.center();
        let span = Rect::new(
            ax.min(bx),
            ay.min(by),
            (ax - bx).abs().max(1.0),
            (ay - by).abs().max(1.0),
        );
        if span.intersects(visible) {
            drawn += 1;
        }
    }
    drawn
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::SceneData;
    use k10s_core::{
        KindId, NsExt, NsNode, PodExt, PodNode, Rect, SatExt, SatNode, Severity, State, ToolId,
        Totals, WlExt, WorkloadNode,
    };
    use std::sync::Arc;

    fn default_opts(policy: &LodPolicy) -> FrameOpts<'_> {
        FrameOpts {
            theme: &k10s_theme::K10S_DARK.map,
            policy,
            type_: k10s_theme::Typography::default().map(),
            edges_on: true,
            skip_blocks: false,
            hex: true,
        }
    }

    fn tiny_scene() -> SceneSnapshot {
        let halo = Rect::new(10.0, 20.0, 110.0, 60.0);
        let card = Rect::new(10.0, 20.0, 80.0, 60.0);
        SceneSnapshot {
            identity_rev: 1,
            ids: Default::default(),
            scene: SceneData {
                card_header: 26.0,
                rev: 1,
                bounds: Rect::new(0.0, 0.0, 400.0, 200.0),
                regions: vec![NsNode {
                    rect: Rect::new(0.0, 0.0, 200.0, 100.0),
                    label: Arc::from("ns"),
                    weight: 1,
                    children: 0..1,
                    ext: NsExt {
                        unhealthy_frac: 0.0,
                        rollup: Severity::Ok,
                    },
                }],
                blocks: vec![WorkloadNode {
                    rect: halo,
                    inner: card,
                    label: Arc::from("wl"),
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
                    rect: Rect::new(20.0, 40.0, 12.0, 12.0),
                    label: Arc::from("pod"),
                    ext: PodExt { state: State::OK },
                }],
                sats: vec![SatNode {
                    rect: Rect::new(94.0, 30.0, 18.0, 18.0),
                    label: Arc::from("pvc/data-wl-0"),
                    ext: SatExt {
                        kind: KindId::VOLUME,
                        detail: Arc::from("16Gi"),
                    },
                }],
                region_blocks: vec![],
                block_cells: vec![],
                block_sats: vec![],
                spatial_index: Default::default(),
                edges: vec![],
                edge_segments: vec![],
                region_edges: vec![],
                region_edge_indexes: vec![],
                cross_edges: 0..0,
                cross_edge_index: Default::default(),
                totals: Totals {
                    regions: 1,
                    blocks: 1,
                    cells: 1,
                    sats: 1,
                    edges: 0,
                },
            },
        }
    }

    #[test]
    fn the_bench_fixture_policy_is_the_shipping_policy() {
        assert_eq!(
            policy(Knobs::default()),
            k10s_atlas::testing::lod_policy(),
            "k10s_atlas::testing::lod_policy() is a hand-maintained copy of \
             lod::policy(Knobs::default()); four gated bench suites measure the \
             copy, so a drift here silently benchmarks a non-shipping policy"
        );
    }

    #[test]
    fn cull_fit_draws_namespaces_at_z0() {
        let mut snap = tiny_scene();
        snap.bounds = Rect::new(0.0, 0.0, 50_000.0, 30_000.0);
        snap.regions[0].rect = Rect::new(100.0, 100.0, 800.0, 400.0);
        let mut cam = Camera::default();
        cam.fit(snap.bounds, 1600.0, 1000.0);
        assert!(cam.zoom < STAGE_WL, "fit zoom {} should be Z0", cam.zoom);
        let pol = policy(Knobs::default());
        let blend = StageBlend::settled(pol.stage_for_zoom(cam.zoom));
        let st = cull(&snap, &cam, blend, 1600.0, 1000.0, default_opts(&pol));
        assert_eq!(st.drawn_regions, 1);
        assert_eq!(st.stage, 0);
        assert_eq!(st.drawn_blocks, 0);
        assert!(st.quads >= 2);
    }

    #[test]
    fn cull_z2_draws_pods_sats_and_hexes() {
        let snap = tiny_scene();
        let cam = Camera {
            cx: 100.0,
            cy: 50.0,
            zoom: 3.0,
        };
        let pol = policy(Knobs::default());
        let blend = StageBlend::settled(pol.stage_for_zoom(cam.zoom));
        let st = cull(&snap, &cam, blend, 1600.0, 1000.0, default_opts(&pol));
        assert!(st.stage >= 2);
        assert_eq!(st.drawn_cells, 1);
        assert_eq!(st.drawn_sats, 1);
        assert_eq!(st.curves, 1);
        assert!(st.bg_cells > 0, "hex grid must be counted by the oracle");
    }
}
