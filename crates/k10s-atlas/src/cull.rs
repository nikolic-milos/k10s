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

/// The rect an edge occupies, centre to centre. Widened to a unit because `intersects` is strict on
/// both sides, so a perfectly horizontal or vertical run would otherwise be invisible everywhere.
fn edge_span(a: &Rect, b: &Rect) -> Rect {
    let (ax, ay) = a.center();
    let (bx, by) = b.center();
    Rect::new(
        ax.min(bx),
        ay.min(by),
        (ax - bx).abs().max(1.0),
        (ay - by).abs().max(1.0),
    )
}

fn edge_visible(a: &Rect, b: &Rect, visible: &Rect) -> bool {
    edge_span(a, b).intersects(visible)
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

/// Emit every visible edge, region by region where the scene carries the grouping.
///
/// `region_edges[i]` is a spatial index and not merely a partition by owner: the span of every edge
/// in it must lie inside `regions[i].rect`, which is what lets one intersection test skip the whole
/// range. An edge that reaches outside its region belongs in the `cross_edges` tail, scanned
/// unconditionally -- that is where `k10s_world` puts a dependency that leaves its namespace, and
/// `k10s_atlas::testing` keeps the generated scene's edges inside their regions. Group an escaping
/// edge under a region anyway and this drops it whenever the region misses the viewport; the flat
/// rescan behind `k10s_map`'s cull oracle is what notices.
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
    use crate::testing::SceneSpec;

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

        // The three regions hold their own blocks and the one edge that spans two of them is in the
        // cross tail, so the grouped walk is entitled to skip a range on one intersection test and
        // the two orders must agree. Break that and this comparison is what fails.
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

    /// The precondition the per-region skip rides on, checked against every scene shape
    /// `benches/cull.rs` and `k10s-map`'s oracle sweep actually feed the generator. Block
    /// containment alone is not quite enough: `edge_span` widens a straight run of blocks to a
    /// unit, so a region needs that unit of slack as well.
    ///
    /// `testing::scene` leaves `cross_edges` empty, so every shape is built a second time by
    /// `cross_scene`, which fills the tail wherever the shape has two regions for an edge to run
    /// between. Without that second build the tail scan is never entered and the walk assertion
    /// below holds for a reason that says nothing about it.
    #[test]
    fn region_edges_are_a_spatial_index() {
        // One region count stands for the bench's uniform axis: a region's rect and the placement
        // of its blocks within it do not depend on how many regions there are. The three fan-out
        // sizes do not collapse the same way -- `blocks_per_region` is what sets the block grid and
        // so the region's size.
        let specs = [
            SceneSpec::uniform(200, 15),
            SceneSpec::fan_out(500),
            SceneSpec::fan_out(2000),
            SceneSpec::fan_out(8000),
            // The oracle sweep's three: uniform, fan-out, dense.
            SceneSpec {
                cells_per_block: 8,
                sats_per_block: 3,
                edges_per_region: 6,
                ..SceneSpec::uniform(16, 9)
            },
            SceneSpec {
                cells_per_block: 6,
                sats_per_block: 3,
                edges_per_region: 40,
                ..SceneSpec::uniform(1, 400)
            },
            SceneSpec {
                cells_per_block: 16,
                sats_per_block: 14,
                edges_per_region: 30,
                ..SceneSpec::uniform(4, 32)
            },
        ];

        let mut tailed = 0;
        for spec in specs {
            for (built, scene) in [
                ("scene", crate::testing::scene(spec)),
                ("cross_scene", crate::testing::cross_scene(spec, 12)),
            ] {
                assert_eq!(scene.region_edges.len(), scene.regions.len());
                tailed += scene.cross_edges.len();

                let mut grouped = 0;
                for (i, (region, range)) in
                    scene.regions.iter().zip(&scene.region_edges).enumerate()
                {
                    for e in &scene.edges[range.start as usize..range.end as usize] {
                        let (Some(a), Some(b)) =
                            (endpoint_rect(&scene, e.a), endpoint_rect(&scene, e.b))
                        else {
                            panic!("{built} {spec:?}: region {i} groups a dangling endpoint");
                        };
                        assert!(
                            region.rect.contains(&edge_span(a, b)),
                            "{built} {spec:?}: edge {e:?} reaches outside region {i}, which the \
                             walk may skip"
                        );
                        grouped += 1;
                    }
                }
                assert!(grouped > 0, "{built} {spec:?}: nothing was grouped");
                assert_eq!(
                    grouped + scene.cross_edges.len(),
                    scene.edges.len(),
                    "{built} {spec:?}: an edge in neither a region range nor the cross tail is one \
                     the walk never reaches"
                );

                // The partition above is a property of the fixture; this is the property of the
                // walk that the fixture exists to enable. With everything visible and no budget,
                // reachable and reached must be the same number, which is what fails if the tail
                // scan is dropped.
                let all = Rect::new(-1e6, -1e6, 2e6, 2e6);
                assert_eq!(
                    walk_edges(&scene, &all, usize::MAX, |_, _| {}),
                    scene.edges.len(),
                    "{built} {spec:?}: the walk reached fewer edges than the ranges partition"
                );
            }
        }
        assert!(
            tailed > 0,
            "no scene here has a cross tail, so every walk assertion above ran against an empty \
             tail and none of them exercised the cross scan"
        );
    }

    /// Every edge the generator and `k10s-world` build today is a block pair, so the tagged branch
    /// and `endpoint_rect` run for the first time when a Phase D or F overlay links a pod to a
    /// service. Pin what they resolve to while the answer is still obvious: a block endpoint means
    /// the card and not the halo, the same rect the block-pair fast path picks, and an endpoint that
    /// resolves to nothing is skipped rather than counted or panicked on.
    #[test]
    fn walk_edges_resolves_tagged_endpoints() {
        let mut scene = hub_scene();
        scene.edges = vec![
            Edge {
                a: Endpoint::cell(0),
                b: Endpoint::sat(1),
            },
            Edge {
                a: Endpoint::block(0),
                b: Endpoint::region(0),
            },
            // A stale watch event: the satellite this refers to is gone.
            Edge {
                a: Endpoint::cell(0),
                b: Endpoint::sat(9),
            },
        ];

        let visible = Rect::new(0.0, 0.0, 400.0, 400.0);
        let mut seen = Vec::new();
        let n = walk_edges(&scene, &visible, 100, |a, b| seen.push((*a, *b)));

        assert_eq!(n, 2, "the dangling endpoint must be skipped, not counted");
        assert_eq!(
            seen,
            vec![
                (scene.cells[0].rect, scene.sats[1].rect),
                (scene.blocks[0].inner, scene.regions[0].rect),
            ]
        );
        assert_ne!(
            scene.blocks[0].inner, scene.blocks[0].rect,
            "the card and the halo must differ or the assertion above is empty"
        );
    }

    /// ROADMAP §6.1's crown jewel as an assertion rather than a bench table: at a fixed camera the
    /// counters must be *identical* between object counts (§6.7), not merely similar. The oracle
    /// sweep in `k10s-map` checks the four budgeted counters as ceilings, but `quads`,
    /// `drawn_blocks`, `drawn_cells` and `drawn_sats` have no budget and nothing else pins them, so
    /// work proportional to total object count -- a child loop that forgets to slice by its parent,
    /// an overlay walked per scene node -- lands here or nowhere.
    ///
    /// The axis is `benches/cull.rs`'s: 200 to 1600 regions at 15 blocks each. Region 0 sits at the
    /// origin and its size depends only on `blocks_per_region` and `cells_per_block`, so one camera
    /// frames the same objects at every scene size -- as long as the frame stays inside region 0,
    /// which each camera is checked for. A frame reaching a neighbouring region would turn this
    /// into a test of the layout grid. The Z0 fit camera is left out because it is O(regions) *by
    /// design*, which is why §4-I makes Z0 aggregation a prerequisite for multi-cluster.
    #[test]
    fn visible_work_is_independent_of_scene_size() {
        const VW: f32 = 1600.0;
        const VH: f32 = 1000.0;
        const SIZES: [usize; 4] = [200, 400, 800, 1600];
        const BLOCKS: usize = 15;

        let scenes: Vec<Scene> = SIZES
            .iter()
            .map(|&regions| crate::testing::scene(SceneSpec::uniform(regions, BLOCKS)))
            .collect();
        let (rx, ry) = scenes[0].regions[0].rect.center();
        // The bench centres its Z2, Z3 and Z4 on `blocks[0]`, region 0's corner block, and the Z2
        // frame from there runs off the region's left edge and its top. It still counts one region,
        // because nothing sits above or left of the origin -- but only because region 0 is the
        // grid's first cell, which is the layout accident this test must not rest on. So the
        // cameras below sit on the block nearest the region's middle, where every frame stays
        // inside the rect.
        let region0 = scenes[0].regions[0].rect;
        let (cx, cy) = scenes[0].blocks[0].inner.center();
        let corner = Camera { cx, cy, zoom: 2.2 }.visible_world(VW, VH);
        assert!(
            corner.x < region0.x && corner.y < region0.y,
            "the bench's Z2 on blocks[0] frames {corner:?}, which no longer escapes region 0 \
             {region0:?} -- the reason for recentring is gone"
        );

        let from_centre = |(x, y): (f32, f32)| (x - rx).powi(2) + (y - ry).powi(2);
        let hub = (0..BLOCKS)
            .min_by(|&a, &b| {
                from_centre(scenes[0].blocks[a].inner.center())
                    .total_cmp(&from_centre(scenes[0].blocks[b].inner.center()))
            })
            .expect("a region has blocks");

        for (regions, s) in SIZES.iter().zip(&scenes) {
            assert_eq!(
                (s.regions[0].rect, s.blocks[hub].inner),
                (region0, scenes[0].blocks[hub].inner),
                "{regions} regions: region 0 is not where the camera expects it"
            );
        }

        let (bx, by) = scenes[0].blocks[hub].inner.center();
        let pol = policy();
        // The bench's Z2, Z3 and Z4 zooms, plus a region-centred zoom 2.0. Only that fourth one
        // reaches every block of region 0; the bench's three each see part of it, so on their own
        // this would be invariance over a corner.
        let cams = [
            (
                "region",
                Camera {
                    cx: rx,
                    cy: ry,
                    zoom: 2.0,
                },
            ),
            (
                "hub",
                Camera {
                    cx: bx,
                    cy: by,
                    zoom: 2.2,
                },
            ),
            (
                "pod",
                Camera {
                    cx: bx,
                    cy: by,
                    zoom: 4.5,
                },
            ),
            (
                "extreme",
                Camera {
                    cx: bx,
                    cy: by,
                    zoom: 24.0,
                },
            ),
        ];

        for (name, cam) in cams {
            let frame = cam.visible_world(VW, VH);
            assert!(
                region0.contains(&frame),
                "{name}: frame {frame:?} is not inside region 0 {region0:?}"
            );
            let mut baseline: Option<CullStats> = None;
            for (regions, s) in SIZES.iter().zip(&scenes) {
                let st = cull(s, &cam, &pol, settled(&pol, &cam), VW, VH, true, false);
                assert_eq!(
                    st.drawn_regions, 1,
                    "{name} at {regions} regions: a second region reaches the frame"
                );
                assert!(st.drawn_cells > 0, "{name}: nothing to compare");
                assert_eq!(
                    st.drawn_blocks == BLOCKS,
                    name == "region",
                    "{name} at {regions} regions: drew {} of region 0's {BLOCKS} blocks",
                    st.drawn_blocks
                );
                match baseline {
                    None => baseline = Some(st),
                    Some(first) => assert_eq!(
                        st, first,
                        "{name}: visible work changed between {} and {regions} regions",
                        SIZES[0]
                    ),
                }
            }
        }
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
