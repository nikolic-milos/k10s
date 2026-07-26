use crate::camera::Camera;
use crate::lod::{LodPolicy, StageBlend};
use crate::scene::{Endpoint, Level, Rect, Scene};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CullStats {
    pub stage: u8,
    pub quads: usize,
    pub drawn_regions: usize,
    pub drawn_blocks: usize,
    pub drawn_cells: usize,
    pub drawn_sats: usize,
    pub edges: usize,
    pub curves: usize,
    pub curves_dropped: usize,
    pub labels: usize,
    pub labels_dropped: usize,
    pub icons: usize,
    pub icons_dropped: usize,

    pub bg_cells: usize,
}

#[expect(clippy::too_many_arguments)]
pub fn cull<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    camera: &Camera,
    policy: &LodPolicy,
    blend: StageBlend,
    vw: f32,
    vh: f32,
    edges_on: bool,
    skip_blocks: bool,
) -> CullStats {
    let zoom = camera.zoom;
    let visible = camera.visible_world(vw, vh);
    let stage = blend.walk_stage();

    let mut st = CullStats {
        stage,
        quads: 1,
        ..CullStats::default()
    };

    for region in &scene.regions {
        if !region.rect.intersects(&visible) {
            continue;
        }
        st.drawn_regions += 1;
        st.quads += 1;

        if policy.region_label_shown(region.rect.w, zoom) {
            push_label(&mut st, policy);
        }

        if stage == 0 {
            continue;
        }

        let region_inside = visible.contains(&region.rect);
        let blocks = &scene.blocks[region.children.start as usize..region.children.end as usize];
        for block in blocks {
            if !(region_inside || block.rect.intersects(&visible)) {
                continue;
            }

            let painted = policy.block_painted(block.inner.w, zoom) && !skip_blocks;
            if painted {
                st.drawn_blocks += 1;
                st.quads += 1;

                if policy.block_chrome_shown(block.inner.w, zoom) {
                    st.quads += 2;
                }

                if policy.block_icon_shown(block.inner.w, zoom) {
                    push_icon(&mut st, policy);
                }

                if policy.block_label_shown(block.inner.w, zoom) {
                    push_label(&mut st, policy);
                }
            }

            if stage < 2 {
                continue;
            }
            let block_inside = region_inside || visible.contains(&block.rect);

            if painted || policy.stress_curves {
                let sats = &scene.sats[block.sats.start as usize..block.sats.end as usize];
                for sat in sats {
                    if !(block_inside || sat.rect.intersects(&visible)) {
                        continue;
                    }
                    if !policy.sat_painted(sat.rect.w, zoom) {
                        continue;
                    }
                    st.drawn_sats += 1;
                    if policy.sat_icon_shown() {
                        push_icon(&mut st, policy);
                    }
                    if policy.sat_label_shown(sat.rect.w, zoom) {
                        push_label(&mut st, policy);
                        push_label(&mut st, policy);
                    }
                    if policy.sat_curves {
                        if st.curves >= policy.curve_budget() {
                            st.curves_dropped += 1;
                        } else {
                            st.curves += 1;
                        }
                    }
                }
            }

            if !painted {
                continue;
            }
            let cells = &scene.cells[block.children.start as usize..block.children.end as usize];
            for cell in cells {
                if !(block_inside || cell.rect.intersects(&visible)) {
                    continue;
                }
                st.drawn_cells += 1;
                st.quads += 1;

                if stage >= 3 && policy.cell_label_shown(cell.rect.w, zoom) {
                    push_label(&mut st, policy);
                }
            }
        }
    }

    if edges_on && stage >= 2 && !policy.stress && !policy.stress_curves {
        st.edges = walk_edges(scene, &visible, policy.max_edges, |_, _| {});
    }

    st
}

fn edge_visible(a: &Rect, b: &Rect, visible: &Rect) -> bool {
    let seg = Rect::new(
        a.center().0.min(b.center().0),
        a.center().1.min(b.center().1),
        (a.center().0 - b.center().0).abs().max(1.0),
        (a.center().1 - b.center().1).abs().max(1.0),
    );
    seg.intersects(visible)
}

/// The rect an endpoint denotes. Blocks resolve to `inner` (the card) rather than
/// the halo, which is what edges attached to before endpoints were tagged.
fn endpoint_rect<R, B, C, S>(scene: &Scene<R, B, C, S>, e: Endpoint) -> Option<&Rect> {
    let i = e.index() as usize;
    match e.level() {
        Level::Region => scene.regions.get(i).map(|n| &n.rect),
        Level::Block => scene.blocks.get(i).map(|n| &n.inner),
        Level::Cell => scene.cells.get(i).map(|n| &n.rect),
        Level::Sat => scene.sats.get(i).map(|n| &n.rect),
    }
}

pub fn walk_edges<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    visible: &Rect,
    max_edges: usize,
    mut emit: impl FnMut(&Rect, &Rect),
) -> usize {
    let mut drawn = 0usize;
    let mut scan = |range: std::ops::Range<usize>, drawn: &mut usize| {
        for e in &scene.edges[range] {
            if *drawn >= max_edges {
                return false;
            }
            // Resolved through `get`, not indexing: endpoints will come from live
            // cluster data, where a dangling reference is a stale watch event
            // rather than a bug worth a panic in the frame path. An edge we
            // cannot resolve is skipped.
            //
            // Block-to-block is every edge the generator emits and the shape all
            // edges had before endpoints were tagged, so it gets a straight-line
            // path: one compare on the packed tags instead of two level matches.
            // Measured worth it, since this is the largest traversal term at high
            // fan-out.
            let (a, b) = if e.is_block_pair() {
                let (Some(a), Some(b)) = (
                    scene.blocks.get(e.a.index() as usize),
                    scene.blocks.get(e.b.index() as usize),
                ) else {
                    continue;
                };
                (&a.inner, &b.inner)
            } else {
                let (Some(a), Some(b)) = (endpoint_rect(scene, e.a), endpoint_rect(scene, e.b))
                else {
                    continue;
                };
                (a, b)
            };
            if edge_visible(a, b, visible) {
                emit(a, b);
                *drawn += 1;
            }
        }
        true
    };

    if scene.region_edges.len() == scene.regions.len() && !scene.region_edges.is_empty() {
        for (region, range) in scene.regions.iter().zip(&scene.region_edges) {
            if range.is_empty() || !region.rect.intersects(visible) {
                continue;
            }
            if !scan(range.start as usize..range.end as usize, &mut drawn) {
                return drawn;
            }
        }
        scan(
            scene.cross_edges.start as usize..scene.cross_edges.end as usize,
            &mut drawn,
        );
    } else {
        scan(0..scene.edges.len(), &mut drawn);
    }
    drawn
}

fn push_label(st: &mut CullStats, policy: &LodPolicy) {
    if st.labels >= policy.max_labels {
        st.labels_dropped += 1;
    } else {
        st.labels += 1;
    }
}

fn push_icon(st: &mut CullStats, policy: &LodPolicy) {
    if st.icons >= policy.max_icons {
        st.icons_dropped += 1;
    } else {
        st.icons += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{BlockNode, CellNode, Edge, RegionNode, Totals};

    fn policy() -> LodPolicy {
        LodPolicy {
            stage_block: 0.09,
            stage_cell: 0.55,
            stage_cell_label: 3.0,
            block_min_px: 4.0,
            block_icon_min_px: 14.0,
            region_label_min_px: 70.0,
            block_label_min_px: 60.0,
            block_label_min_zoom: 0.22,
            cell_label_min_px: 34.0,
            block_chrome_min_px: 34.0,
            stage_exit: 0.85,
            sat_min_px: 5.0,
            sat_label_min_px: 30.0,
            max_labels: 400,
            max_icons: 512,
            max_edges: 3000,
            max_curves: 1500,
            sat_curves: true,
            stress: false,
            stress_curves: false,
        }
    }

    fn settled(pol: &LodPolicy, cam: &Camera) -> StageBlend {
        StageBlend::settled(pol.stage_for_zoom(cam.zoom))
    }

    fn tiny_scene() -> Scene {
        let block_rect = Rect::new(10.0, 20.0, 80.0, 60.0);
        Scene {
            rev: 1,
            bounds: Rect::new(0.0, 0.0, 400.0, 200.0),
            regions: vec![RegionNode {
                rect: Rect::new(0.0, 0.0, 200.0, 100.0),
                label: "region".into(),
                weight: 1,
                children: 0..1,
                ext: (),
            }],
            blocks: vec![BlockNode {
                rect: block_rect,
                inner: block_rect,
                label: "block".into(),
                children: 0..1,
                sats: 0..0,
                ext: (),
            }],
            cells: vec![CellNode {
                rect: Rect::new(20.0, 40.0, 12.0, 12.0),
                label: "cell".into(),
                ext: (),
            }],
            sats: vec![],
            edges: vec![],
            region_edges: vec![],
            cross_edges: 0..0,
            totals: Totals {
                regions: 1,
                blocks: 1,
                cells: 1,
                sats: 0,
                edges: 0,
            },
        }
    }

    fn hub_scene() -> Scene {
        let halo = Rect::new(100.0, 100.0, 200.0, 200.0);
        let card = Rect::new(180.0, 180.0, 40.0, 40.0);
        let sat = |x, y| CellNode {
            rect: Rect::new(x, y, 18.0, 18.0),
            label: "sat".into(),
            ext: (),
        };
        Scene {
            rev: 1,
            bounds: Rect::new(0.0, 0.0, 400.0, 400.0),
            regions: vec![RegionNode {
                rect: Rect::new(80.0, 80.0, 240.0, 240.0),
                label: "region".into(),
                weight: 1,
                children: 0..1,
                ext: (),
            }],
            blocks: vec![BlockNode {
                rect: halo,
                inner: card,
                label: "hub".into(),
                children: 0..1,
                sats: 0..3,
                ext: (),
            }],
            cells: vec![CellNode {
                rect: Rect::new(190.0, 190.0, 10.0, 10.0),
                label: "cell".into(),
                ext: (),
            }],
            sats: vec![sat(120.0, 120.0), sat(260.0, 140.0), sat(200.0, 260.0)],
            edges: vec![],
            region_edges: vec![],
            cross_edges: 0..0,
            totals: Totals {
                regions: 1,
                blocks: 1,
                cells: 1,
                sats: 3,
                edges: 0,
            },
        }
    }

    #[test]
    fn walk_edges_grouped_matches_flat() {
        let region = |x, i: u32| RegionNode {
            rect: Rect::new(x, 0.0, 100.0, 100.0),
            label: "r".into(),
            weight: 1,
            children: i * 2..i * 2 + 2,
            ext: (),
        };
        let block = |x, y| {
            let rect = Rect::new(x + 10.0, y, 20.0, 20.0);
            BlockNode {
                rect,
                inner: rect,
                label: "b".into(),
                children: 0..0,
                sats: 0..0,
                ext: (),
            }
        };
        let mut scene: Scene = Scene {
            rev: 1,
            bounds: Rect::new(0.0, 0.0, 500.0, 100.0),
            regions: (0..3).map(|i| region(i as f32 * 200.0, i)).collect(),
            blocks: (0..3)
                .flat_map(|i| {
                    let x = i as f32 * 200.0;
                    [block(x, 10.0), block(x, 60.0)]
                })
                .collect(),
            cells: vec![],
            sats: vec![],
            edges: vec![
                Edge::blocks(0, 1),
                Edge::blocks(2, 3),
                Edge::blocks(4, 5),
                Edge::blocks(0, 5),
            ],
            region_edges: vec![0..1, 1..2, 2..3],
            cross_edges: 3..4,
            totals: Totals {
                regions: 3,
                blocks: 6,
                cells: 0,
                sats: 0,
                edges: 4,
            },
        };

        let visible = Rect::new(0.0, 0.0, 350.0, 100.0);
        let mut grouped = Vec::new();
        let n = walk_edges(&scene, &visible, 100, |a, b| grouped.push((*a, *b)));
        assert_eq!(n, 3);

        scene.region_edges.clear();
        let mut flat = Vec::new();
        let n = walk_edges(&scene, &visible, 100, |a, b| flat.push((*a, *b)));
        assert_eq!(n, 3);
        assert_eq!(grouped, flat);

        scene.region_edges = vec![0..1, 1..2, 2..3];
        assert_eq!(walk_edges(&scene, &visible, 2, |_, _| {}), 2);
    }

    #[test]
    fn cull_fit_draws_regions_at_z0() {
        let pol = policy();
        let mut snap = tiny_scene();
        snap.bounds = Rect::new(0.0, 0.0, 50_000.0, 30_000.0);
        snap.regions[0].rect = Rect::new(100.0, 100.0, 800.0, 400.0);
        let mut cam = Camera::default();
        cam.fit(snap.bounds, 1600.0, 1000.0);
        assert!(
            cam.zoom < pol.stage_block,
            "fit zoom {} should be Z0",
            cam.zoom
        );
        let st = cull(
            &snap,
            &cam,
            &pol,
            settled(&pol, &cam),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!(st.drawn_regions, 1);
        assert_eq!(st.stage, 0);
        assert_eq!(st.drawn_blocks, 0);
        assert!(st.quads >= 2);
    }

    #[test]
    fn cull_z2_draws_cells() {
        let snap = tiny_scene();
        let cam = Camera {
            cx: 100.0,
            cy: 50.0,
            zoom: 1.0,
        };
        let pol = policy();
        let st = cull(
            &snap,
            &cam,
            &pol,
            settled(&pol, &cam),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert!(st.stage >= 2);
        assert_eq!(st.drawn_cells, 1);
    }

    #[test]
    fn stress_forces_stage_2_and_paints_tiny_blocks() {
        let mut pol = policy();
        pol.stress = true;
        assert_eq!(pol.stage_for_zoom(0.01), 2);
        assert!(pol.block_painted(1.0, 0.01));
        let snap = tiny_scene();
        let cam = Camera {
            cx: 100.0,
            cy: 50.0,
            zoom: 1.0,
        };
        let st = cull(
            &snap,
            &cam,
            &pol,
            settled(&pol, &cam),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!(st.edges, 0);
    }

    #[test]
    fn sats_counted_with_curve_budget() {
        let snap = hub_scene();
        let cam = Camera {
            cx: 200.0,
            cy: 200.0,
            zoom: 2.0,
        };
        let pol = policy();
        let st = cull(
            &snap,
            &cam,
            &pol,
            settled(&pol, &cam),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert!(st.stage >= 2);
        assert_eq!(st.drawn_sats, 3);
        assert_eq!(st.curves, 3);
        assert_eq!(st.curves_dropped, 0);
        assert_eq!(st.quads, 6);
        assert_eq!(st.icons, 4);
        assert_eq!(st.labels, 1 + 1 + 6);

        let mut tight = policy();
        tight.max_curves = 2;
        let st = cull(
            &snap,
            &cam,
            &tight,
            settled(&tight, &cam),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!((st.curves, st.curves_dropped), (2, 1));

        let mut off = policy();
        off.sat_curves = false;
        let st = cull(
            &snap,
            &cam,
            &off,
            settled(&off, &cam),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!((st.curves, st.curves_dropped), (0, 0));
        assert_eq!(st.drawn_sats, 3, "curves off must not hide satellites");
    }

    #[test]
    fn stress_curves_probes_every_visible_sat() {
        let snap = hub_scene();
        let cam = Camera {
            cx: 200.0,
            cy: 200.0,
            zoom: 0.1,
        };
        let pol = policy();
        let st = cull(
            &snap,
            &cam,
            &pol,
            settled(&pol, &cam),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!((st.drawn_sats, st.curves), (0, 0));

        let mut probe = policy();
        probe.stress_curves = true;
        assert_eq!(probe.stage_for_zoom(0.1), 2, "probe forces the sat stage");
        let st = cull(
            &snap,
            &cam,
            &probe,
            settled(&probe, &cam),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!(st.drawn_sats, 3);
        assert_eq!(st.curves, 3);
        assert_eq!(st.icons, 0, "probe measures curves alone");
        assert_eq!(st.labels, 0);
        assert_eq!(st.edges, 0);

        let far = Camera {
            cx: 200.0,
            cy: 200.0,
            zoom: 0.02,
        };
        let st = cull(
            &snap,
            &far,
            &probe,
            settled(&probe, &far),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!(st.drawn_blocks, 0, "card 0.8 px stays unpainted");
        assert_eq!(st.drawn_sats, 3, "probe walks sats of unpainted hubs");
    }

    #[test]
    fn fade_counts_union_of_stages() {
        let snap = hub_scene();
        let cam = Camera {
            cx: 200.0,
            cy: 200.0,
            zoom: 2.0,
        };
        let pol = policy();

        let at_1 = cull(
            &snap,
            &cam,
            &pol,
            StageBlend::settled(1),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!((at_1.drawn_sats, at_1.curves, at_1.drawn_cells), (0, 0, 0));

        let at_2 = cull(
            &snap,
            &cam,
            &pol,
            StageBlend::settled(2),
            1600.0,
            1000.0,
            true,
            false,
        );
        assert_eq!(at_2.drawn_sats, 3);

        for blend in [
            StageBlend {
                from: 1,
                to: 2,
                t: 0.0,
            },
            StageBlend {
                from: 1,
                to: 2,
                t: 0.5,
            },
            StageBlend {
                from: 2,
                to: 1,
                t: 0.5,
            },
        ] {
            let fading = cull(&snap, &cam, &pol, blend, 1600.0, 1000.0, true, false);
            assert_eq!(fading, at_2, "fade {blend:?} must count the union");
        }
    }
}
