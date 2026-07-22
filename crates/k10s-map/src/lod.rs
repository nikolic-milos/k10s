use std::sync::OnceLock;

use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
use k10s_core::SceneSnapshot;

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

pub fn lod() -> &'static LodPolicy {
    static LOD: OnceLock<LodPolicy> = OnceLock::new();
    LOD.get_or_init(|| {
        let on = |name: &str| std::env::var_os(name).is_some_and(|v| v != "0");
        let stress = on("K10S_STRESS_QUADS");
        LodPolicy {
            stage_block: STAGE_WL,
            stage_cell: STAGE_POD,
            stage_cell_label: STAGE_POD_LABEL,
            block_min_px: WL_MIN_PX,
            block_icon_min_px: if on("K10S_NO_ICONS") {
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
            sat_curves: !on("K10S_NO_CURVES"),
            stress,
            stress_curves: on("K10S_STRESS_CURVES") && !stress,
        }
    })
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
    edges_on: bool,
    skip_workloads: bool,
) -> CullStats {
    let pol = lod();
    let mut st = k10s_atlas::cull(scene, camera, pol, blend, vw, vh, edges_on, skip_workloads);
    let visible = camera.visible_world(vw, vh);
    st.bg_cells = crate::hex::visible_count(&visible, camera.zoom, pol.stress || pol.stress_curves);
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{
        Health, NsExt, NsNode, PodExt, PodNode, Rect, SatExt, SatKind, SatNode, Tool, Totals,
        WlExt, WorkloadKind, WorkloadNode,
    };
    use std::sync::Arc;

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
                },
            }],
            blocks: vec![WorkloadNode {
                rect: halo,
                inner: card,
                label: Arc::from("wl"),
                children: 0..1,
                sats: 0..1,
                ext: WlExt {
                    kind: WorkloadKind::Deployment,
                    tool: Tool::None,
                    health: Health::Ok,
                    ns: 0,
                },
            }],
            cells: vec![PodNode {
                rect: Rect::new(20.0, 40.0, 12.0, 12.0),
                label: Arc::from("pod"),
                ext: PodExt { health: Health::Ok },
            }],
            sats: vec![SatNode {
                rect: Rect::new(94.0, 30.0, 18.0, 18.0),
                label: Arc::from("pvc/data-wl-0"),
                ext: SatExt {
                    kind: SatKind::Volume,
                    detail: Arc::from("16Gi"),
                },
            }],
            edges: vec![],
            region_edges: vec![],
            cross_edges: 0..0,
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
        let blend = StageBlend::settled(stage_for_zoom(cam.zoom));
        let st = cull(&snap, &cam, blend, 1600.0, 1000.0, true, false);
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
        let blend = StageBlend::settled(stage_for_zoom(cam.zoom));
        let st = cull(&snap, &cam, blend, 1600.0, 1000.0, true, false);
        assert!(st.stage >= 2);
        assert_eq!(st.drawn_cells, 1);
        assert_eq!(st.drawn_sats, 1);
        assert_eq!(st.curves, 1);
        assert!(st.bg_cells > 0, "hex grid must be counted by the oracle");
    }
}
