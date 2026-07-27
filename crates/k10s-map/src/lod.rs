use std::sync::OnceLock;

use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
use k10s_core::{Endpoint, Level, Rect, SceneSnapshot};

use crate::frame::FrameOpts;

pub const STAGE_WL: f32 = 0.09;
pub const STAGE_POD: f32 = 0.55;
pub const STAGE_POD_LABEL: f32 = 3.0;

pub const STAGE_EXIT: f32 = 0.85;
pub const STAGE_FADE_SECS: f32 = 0.18;

pub const WL_MIN_PX: f32 = 4.0;
pub const WL_ICON_MIN_PX: f32 = 14.0;
pub const WL_CHROME_MIN_PX: f32 = 34.0;
pub const NS_LABEL_MIN_PX: f32 = 70.0;
pub const WL_LABEL_MIN_PX: f32 = 60.0;
pub const WL_LABEL_MIN_ZOOM: f32 = 0.22;
pub const POD_LABEL_MIN_PX: f32 = 34.0;
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
    let mut st = k10s_atlas::cull(
        scene,
        camera,
        opts.policy,
        blend,
        vw,
        vh,
        opts.edges_on,
        opts.skip_blocks,
    );
    let visible = camera.visible_world(vw, vh);
    st.bg_cells = crate::hex::visible_count(&visible, camera.zoom, !opts.hex_shown());
    st.edges = if opts.edges_on && blend.walk_stage() >= 2 && !opts.stress_any() {
        reference_edges(scene, &visible, opts.policy.max_edges)
    } else {
        0
    };
    st
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
    use k10s_core::{
        KindId, NsExt, NsNode, PodExt, PodNode, Rect, SatExt, SatNode, Severity, State, ToolId,
        Totals, WlExt, WorkloadNode,
    };
    use std::sync::Arc;

    fn default_opts(policy: &LodPolicy) -> FrameOpts<'_> {
        FrameOpts {
            policy,
            edges_on: true,
            skip_blocks: false,
            hex: true,
        }
    }

    fn tiny_scene() -> SceneSnapshot {
        let halo = Rect::new(10.0, 20.0, 110.0, 60.0);
        let card = Rect::new(10.0, 20.0, 80.0, 60.0);
        SceneSnapshot {
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
        }
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
            zoom: 1.0,
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
