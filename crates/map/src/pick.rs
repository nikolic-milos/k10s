use gpui::{point, px};
use k10s_atlas::{Camera, Level, LodPolicy, Rect, Scene, StageBlend, WorkloadPresentation};

use crate::primitive::Projection;

#[cfg(test)]
fn inside(rect: &Rect, x: f32, y: f32) -> bool {
    rect.x <= x && x < rect.max_x() && rect.y <= y && y < rect.max_y()
}

// What sits under a screen point, resolved by the same LOD policy the painter
// draws with: what is visible is what is clickable, and nothing else. The
// path carries the whole ancestry because a consumer naming a cell needs its
// block and region too.
//
// Tiebreak, deepest first: cell, then satellite, then block card, then
// region. Cells live inside a card and satellites outside it, so the first
// two cannot collide; a satellite overhanging a neighbouring card is resolved
// as the satellite, which is what a pointer over its icon expects.
//
// The bench-only knobs (`stress`, `stress_curves`, skip_blocks) are ignored:
// picking answers for the real scene, not a stressed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickPath {
    pub region: u32,
    pub block: Option<u32>,
    pub cell: Option<u32>,
    pub sat: Option<u32>,
}

impl PickPath {
    pub fn level(&self) -> Level {
        if self.cell.is_some() {
            Level::Cell
        } else if self.sat.is_some() {
            Level::Sat
        } else if self.block.is_some() {
            Level::Block
        } else {
            Level::Region
        }
    }

    pub fn index(&self) -> u32 {
        self.cell.or(self.sat).or(self.block).unwrap_or(self.region)
    }
}

/// Where a picked path is on the map, in world units.
///
/// A block answers with its CARD, not its halo: the halo is spacing that the
/// painter never draws and `pick` never hit-tests, so a ring drawn round it
/// would sit in empty space well outside the thing it is pointing at.
pub fn path_rect<R, B, C, S>(scene: &Scene<R, B, C, S>, path: &PickPath) -> Option<Rect> {
    if let Some(cell) = path.cell {
        return scene.cells.get(cell as usize).map(|node| node.rect);
    }
    if let Some(sat) = path.sat {
        return scene.sats.get(sat as usize).map(|node| node.rect);
    }
    if let Some(block) = path.block {
        return scene.blocks.get(block as usize).map(|node| node.inner);
    }
    scene
        .regions
        .get(path.region as usize)
        .map(|node| node.rect)
}

#[expect(clippy::too_many_arguments)]
pub fn pick<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    camera: &Camera,
    policy: &LodPolicy,
    blend: StageBlend,
    vw: f32,
    vh: f32,
    sx: f32,
    sy: f32,
) -> Option<PickPath> {
    if sx < 0.0 || sy < 0.0 || sx >= vw || sy >= vh {
        return None;
    }
    let (wx, wy) = camera.s2w(sx, sy, vw, vh);
    let stage = blend.walk_stage();
    let zoom = camera.zoom;
    let visible = camera.visible_world(vw, vh);
    let projection = Projection::new(*camera, (vw, vh), (0.0, 0.0));
    let screen_point = point(px(sx), px(sy));

    // Layout guarantees regions do not overlap, so the first containing
    // region is the only one. The probe rect exists because the spatial
    // index speaks intersection, not containment.
    let probe = Rect::new(wx, wy, 1e-3, 1e-3);
    let mut region_hit: Option<u32> = None;
    if scene.region_index_is_selective(&probe) {
        scene.for_each_region_candidate(&probe, |index, region| {
            if region_hit.is_none() && projection.region(region.rect).contains(screen_point) {
                region_hit = Some(index as u32);
            }
        });
    } else {
        region_hit = scene
            .regions
            .iter()
            .position(|region| projection.region(region.rect).contains(screen_point))
            .map(|index| index as u32);
    }
    let region = region_hit?;
    let mut path = PickPath {
        region,
        block: None,
        cell: None,
        sat: None,
    };
    if stage == 0 {
        return Some(path);
    }

    // The halo is only a spatial-index envelope. Resolving the displayed
    // primitive here keeps a mid-zoom medallion clickable without turning its
    // unused card or orbit spacing into a target.
    let mut sat_hit: Option<(u32, u32)> = None;
    // The halo rect indexed for a block encloses its card and satellites, so a
    // point probe can narrow both hit classes together. Large fan-out regions
    // stay O(log n + leaf) under pointer motion; small regions deliberately
    // retain the cheaper direct scan selected by the scene index.
    scene.for_each_region_block_candidate(region as usize, &probe, |block_index, block| {
        let presentation = policy.workload_presentation(block.inner.w, zoom, stage);
        if presentation == WorkloadPresentation::Hidden {
            return;
        }
        let block_primitive = match presentation {
            WorkloadPresentation::Medallion => projection.medallion(block.rect, block.inner),
            WorkloadPresentation::Card | WorkloadPresentation::Detailed => {
                projection.card(block.inner)
            }
            WorkloadPresentation::Hidden => unreachable!(),
        };
        if path.block.is_none() && block_primitive.contains(screen_point) {
            path.block = Some(block_index as u32);

            if presentation.cells_shown() {
                let cells = block.children.len();
                let aggregated = cells > policy.max_cells_per_block
                    && policy.cells_aggregated(cells, block.inner.intersection_fraction(&visible));
                if !aggregated {
                    scene.for_each_block_cell_candidate(block_index, &probe, |cell_index, cell| {
                        if path.cell.is_none() && projection.pod(cell.rect).contains(screen_point) {
                            path.cell = Some(cell_index as u32);
                        }
                    });
                }
            }
        }
        if stage >= 2 && sat_hit.is_none() {
            scene.for_each_block_sat(block_index, |sat_index, satellite| {
                if sat_hit.is_none()
                    && policy.sat_painted(satellite.rect.w, zoom)
                    && projection.satellite(satellite.rect).contains(screen_point)
                {
                    sat_hit = Some((block_index as u32, sat_index as u32));
                }
            });
        }
    });

    if path.cell.is_none() {
        if let Some((block, sat)) = sat_hit {
            path.block = Some(block);
            path.sat = Some(sat);
        }
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_atlas::testing::{SceneSpec, lod_policy, scene};

    fn cameras(bounds: Rect, vw: f32, vh: f32) -> Vec<Camera> {
        let mut fit = Camera::default();
        fit.fit(bounds, vw, vh);
        let (cx, cy) = bounds.center();
        [fit.zoom, 0.05, 0.12, 0.7, 1.4, 4.5]
            .into_iter()
            .map(|zoom| Camera { cx, cy, zoom })
            .collect()
    }

    // The reference resolves the same rules with flat scans and no hierarchy,
    // so agreement means the traversal, the index, and the aggregation gate
    // did not change the answer.
    fn reference<R, B, C, S>(
        scene: &Scene<R, B, C, S>,
        camera: &Camera,
        policy: &LodPolicy,
        blend: StageBlend,
        vw: f32,
        vh: f32,
        sx: f32,
        sy: f32,
    ) -> Option<PickPath> {
        if sx < 0.0 || sy < 0.0 || sx >= vw || sy >= vh {
            return None;
        }
        let stage = blend.walk_stage();
        let zoom = camera.zoom;
        let visible = camera.visible_world(vw, vh);
        let projection = Projection::new(*camera, (vw, vh), (0.0, 0.0));
        let screen_point = point(px(sx), px(sy));

        let region = scene
            .regions
            .iter()
            .position(|region| projection.region(region.rect).contains(screen_point))?
            as u32;
        let mut path = PickPath {
            region,
            block: None,
            cell: None,
            sat: None,
        };
        let mut sat_hit: Option<(u32, u32)> = None;
        if stage == 0 {
            return Some(path);
        }

        for block_index in scene.region_block_indices(region as usize) {
            let block = &scene.blocks[block_index];
            let presentation = policy.workload_presentation(block.inner.w, zoom, stage);
            if presentation == WorkloadPresentation::Hidden {
                continue;
            }
            let block_primitive = match presentation {
                WorkloadPresentation::Medallion => projection.medallion(block.rect, block.inner),
                WorkloadPresentation::Card | WorkloadPresentation::Detailed => {
                    projection.card(block.inner)
                }
                WorkloadPresentation::Hidden => unreachable!(),
            };
            if path.block.is_none() && block_primitive.contains(screen_point) {
                path.block = Some(block_index as u32);
                if presentation.cells_shown() {
                    let cells = block.children.len();
                    let aggregated = cells > policy.max_cells_per_block
                        && policy
                            .cells_aggregated(cells, block.inner.intersection_fraction(&visible));
                    if !aggregated {
                        for cell_index in scene.block_cell_indices(block_index) {
                            if projection
                                .pod(scene.cells[cell_index].rect)
                                .contains(screen_point)
                            {
                                path.cell = Some(cell_index as u32);
                                break;
                            }
                        }
                    }
                }
            }
            if stage >= 2 && sat_hit.is_none() {
                for sat_index in scene.block_sat_indices(block_index) {
                    let satellite = &scene.sats[sat_index];
                    if policy.sat_painted(satellite.rect.w, zoom)
                        && projection.satellite(satellite.rect).contains(screen_point)
                    {
                        sat_hit = Some((block_index as u32, sat_index as u32));
                        break;
                    }
                }
            }
        }
        if path.cell.is_none()
            && let Some((block, sat)) = sat_hit
        {
            path.block = Some(block);
            path.sat = Some(sat);
        }
        Some(path)
    }

    #[test]
    fn pick_agrees_with_a_flat_reference_everywhere() {
        let (vw, vh) = (1600.0, 1000.0);
        let policy = lod_policy();
        for spec in [
            SceneSpec::uniform(6, 8),
            SceneSpec::uniform(80, 15),
            SceneSpec::fan_out(600),
        ] {
            let s = scene(spec);
            for camera in cameras(s.bounds, vw, vh) {
                let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
                for gx in 0..24 {
                    for gy in 0..15 {
                        let sx = (gx as f32 + 0.5) * vw / 24.0;
                        let sy = (gy as f32 + 0.5) * vh / 15.0;
                        assert_eq!(
                            pick(&s, &camera, &policy, blend, vw, vh, sx, sy),
                            reference(&s, &camera, &policy, blend, vw, vh, sx, sy),
                            "pick diverged at zoom {} screen ({sx},{sy})",
                            camera.zoom
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn stage_zero_resolves_regions_and_nothing_deeper() {
        let (vw, vh) = (1600.0, 1000.0);
        let policy = lod_policy();
        let s = scene(SceneSpec::uniform(400, 15));
        let mut camera = Camera::default();
        camera.fit(s.bounds, vw, vh);
        assert_eq!(policy.stage_for_zoom(camera.zoom), 0, "fit must be Z0");
        let blend = StageBlend::settled(0);

        let (bx, by) = s.blocks[0].inner.center();
        let (sx, sy) = camera.w2s(bx, by, vw, vh);
        let path = pick(&s, &camera, &policy, blend, vw, vh, sx, sy)
            .expect("a block center lies inside its region");
        assert_eq!(path.level(), Level::Region);
        assert_eq!(
            path.block, None,
            "Z0 draws regions only, so it picks them only"
        );
    }

    #[test]
    fn a_card_gap_click_is_the_region_not_the_halo() {
        let (vw, vh) = (1600.0, 1000.0);
        let policy = lod_policy();
        let s = scene(SceneSpec::uniform(4, 4));
        let block = &s.blocks[0];
        // A point inside the halo rect but outside the drawn card.
        let (wx, wy) = (block.inner.x - 2.0, block.inner.y - 2.0);
        assert!(inside(&block.rect, wx, wy));
        let camera = Camera {
            cx: wx,
            cy: wy,
            zoom: 1.0,
        };
        let blend = StageBlend::settled(policy.stage_for_zoom(1.0));
        let path = pick(&s, &camera, &policy, blend, vw, vh, vw * 0.5, vh * 0.5)
            .expect("the halo lies inside the region");
        assert_eq!(
            (path.level(), path.block),
            (Level::Region, None),
            "the halo is spacing, not a click target"
        );
    }

    #[test]
    fn a_mid_zoom_medallion_owns_pixels_outside_its_hidden_card() {
        let (vw, vh) = (1251.5, 733.25);
        let policy = lod_policy();
        let s = scene(SceneSpec::uniform(4, 4));
        let block = &s.blocks[0];
        let camera = Camera {
            cx: block.inner.center().0,
            cy: block.inner.center().1,
            zoom: 0.20,
        };
        let projection = Projection::new(camera, (vw, vh), (0.0, 0.0));
        let medallion = projection.medallion(block.rect, block.inner);
        let card = projection.card(block.inner);
        assert!(
            medallion.bounds.size.width > card.bounds.size.width,
            "the probe must cover the pixels the old card hit box missed"
        );

        let probe = point(
            medallion.bounds.origin.x + px(1.0),
            medallion.bounds.center().y,
        );
        assert!(medallion.contains(probe));
        assert!(!card.contains(probe));
        let path = pick(
            &s,
            &camera,
            &policy,
            StageBlend::settled(1),
            vw,
            vh,
            f32::from(probe.x),
            f32::from(probe.y),
        )
        .expect("the displayed medallion lies inside its namespace");
        assert_eq!(path.block, Some(0));
    }

    #[test]
    fn transition_pick_switches_hierarchy_only_at_the_display_handoff() {
        let (vw, vh) = (1251.5, 733.25);
        let policy = lod_policy();
        let s = scene(SceneSpec::uniform(1, 8));
        let cell = &s.cells[0];
        let camera = Camera {
            cx: cell.rect.center().0,
            cy: cell.rect.center().1,
            zoom: 4.5,
        };
        let (sx, sy) = camera.w2s(cell.rect.center().0, cell.rect.center().1, vw, vh);

        let before = pick(
            &s,
            &camera,
            &policy,
            StageBlend {
                from: 1,
                to: 2,
                t: 0.49,
            },
            vw,
            vh,
            sx,
            sy,
        )
        .expect("the workload remains displayed before the handoff");
        let after = pick(
            &s,
            &camera,
            &policy,
            StageBlend {
                from: 1,
                to: 2,
                t: 0.5,
            },
            vw,
            vh,
            sx,
            sy,
        )
        .expect("the detailed card is displayed at the handoff");

        assert_eq!(before.cell, None);
        assert_eq!(after.cell, Some(0));
    }

    #[test]
    fn a_satellite_pick_keeps_its_parent_ancestry() {
        let (vw, vh) = (1251.5, 733.25);
        let policy = lod_policy();
        let s = scene(SceneSpec::uniform(1, 4));
        let sat_index = 0usize;
        let block_index = s
            .blocks
            .iter()
            .position(|block| block.sats.contains(&(sat_index as u32)))
            .expect("the fixture satellite has a workload parent");
        let sat = &s.sats[sat_index];
        let camera = Camera {
            cx: sat.rect.center().0,
            cy: sat.rect.center().1,
            zoom: 4.5,
        };
        let path = pick(
            &s,
            &camera,
            &policy,
            StageBlend::settled(2),
            vw,
            vh,
            vw * 0.5,
            vh * 0.5,
        )
        .expect("the satellite glyph is displayed at the viewport center");

        assert_eq!(path.sat, Some(sat_index as u32));
        assert_eq!(path.block, Some(block_index as u32));
    }

    #[test]
    fn an_aggregated_block_swallows_its_cells() {
        let (vw, vh) = (1600.0, 1000.0);
        let policy = lod_policy();
        let mut spec = SceneSpec::fan_out(1);
        spec.cells_per_block = policy.max_cells_per_block + 1;
        let s = scene(spec);
        let block = &s.blocks[0];
        let cell = &s.cells[0];
        let (wx, wy) = cell.rect.center();
        let camera = Camera {
            cx: wx,
            cy: wy,
            // Zoomed far enough out that the whole card is visible, so the
            // aggregation fraction gate holds, but still stage 2.
            zoom: policy.stage_cell * 1.2,
        };
        let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
        assert!(blend.walk_stage() >= 2);
        assert!(
            block
                .inner
                .intersection_fraction(&camera.visible_world(vw, vh))
                >= 0.125
        );
        let path = pick(&s, &camera, &policy, blend, vw, vh, vw * 0.5, vh * 0.5)
            .expect("the cell center lies inside the scene");
        assert_eq!(
            (path.level(), path.cell),
            (Level::Block, None),
            "an aggregated card is one click target, not a thousand"
        );
    }

    #[test]
    fn a_point_pick_in_a_50k_workload_namespace_visits_one_index_leaf() {
        let (vw, vh) = (1600.0, 1000.0);
        let policy = lod_policy();
        let s = scene(SceneSpec::fan_out(50_000));
        let target = 25_000usize;
        let (wx, wy) = s.blocks[target].inner.center();
        let probe = Rect::new(wx, wy, 1e-3, 1e-3);
        assert!(s.region_block_index_is_selective(0, &probe));

        let mut candidates = 0usize;
        s.for_each_region_block_candidate(0, &probe, |_, _| candidates += 1);
        assert!(
            candidates <= 64,
            "a point probe visited {candidates} of 50,000 workloads"
        );

        let camera = Camera {
            cx: wx,
            cy: wy,
            zoom: 4.5,
        };
        let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
        let path = pick(&s, &camera, &policy, blend, vw, vh, vw * 0.5, vh * 0.5)
            .expect("a workload center is pickable");
        assert_eq!(path.block, Some(target as u32));
    }

    #[test]
    fn a_point_pick_in_a_50k_pod_workload_visits_one_index_leaf() {
        let (vw, vh) = (1600.0, 1000.0);
        let policy = lod_policy();
        let mut spec = SceneSpec::fan_out(1);
        spec.cells_per_block = 50_000;
        let s = scene(spec);
        let target = 25_000usize;
        let (wx, wy) = s.cells[target].rect.center();
        let probe = Rect::new(wx, wy, 1e-3, 1e-3);
        assert!(s.block_cell_index_is_selective(0, &probe));

        let mut candidates = 0usize;
        s.for_each_block_cell_candidate(0, &probe, |_, _| candidates += 1);
        assert!(
            candidates <= 64,
            "a point probe visited {candidates} of 50,000 pods"
        );

        let camera = Camera {
            cx: wx,
            cy: wy,
            zoom: 24.0,
        };
        let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
        let path = pick(&s, &camera, &policy, blend, vw, vh, vw * 0.5, vh * 0.5)
            .expect("a pod center is pickable");
        assert_eq!(path.cell, Some(target as u32));
    }
}
