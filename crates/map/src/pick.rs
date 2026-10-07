use gpui::{point, px};
use k10s_atlas::{Camera, Level, LodPolicy, Rect, Scene, StageBlend, WorkloadPresentation};

use crate::primitive::{Projection, workload_presentation};

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
/// A block answers with its layout card, excluding orbit spacing. Screen-space
/// targets and selection rings resolve the bounded primitive separately.
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
    pick_typed(
        scene,
        camera,
        policy,
        blend,
        vw,
        vh,
        sx,
        sy,
        k10s_theme::MapType::default(),
    )
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn pick_typed<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    camera: &Camera,
    policy: &LodPolicy,
    blend: StageBlend,
    vw: f32,
    vh: f32,
    sx: f32,
    sy: f32,
    type_: k10s_theme::MapType,
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
    let mut heading_hit = false;
    // The halo rect indexed for a block encloses its card and satellites, so a
    // point probe can narrow both hit classes together. Large fan-out regions
    // stay O(log n + leaf) under pointer motion; small regions deliberately
    // retain the cheaper direct scan selected by the scene index.
    scene.for_each_region_block_candidate(region as usize, &probe, |block_index, block| {
        let presentation = workload_presentation(policy, block.inner, zoom, stage, type_);
        if presentation == WorkloadPresentation::Hidden {
            return;
        }
        let geometry = presentation
            .card_shown()
            .then(|| projection.workload(scene, block_index, stage, type_));
        let block_primitive = geometry.map_or_else(
            || projection.medallion(block.rect, block.inner),
            |geometry| geometry.body,
        );
        if path.block.is_none() && block_primitive.contains(screen_point) {
            path.block = Some(block_index as u32);
            let heading = geometry.and_then(|geometry| {
                crate::primitive::workload_label_shown(policy, block.inner.w, zoom)
                    .then_some(geometry.heading)
                    .flatten()
            });
            heading_hit = heading.is_some_and(|heading| heading.contains(screen_point));

            if stage >= 2 && presentation.cells_shown() && !heading_hit {
                let cells = block.children.len();
                let aggregated = cells > policy.max_cells_per_block
                    && policy.cells_aggregated(cells, block.inner.intersection_fraction(&visible));
                if !aggregated {
                    scene.for_each_block_cell_candidate(block_index, &probe, |cell_index, cell| {
                        if path.cell.is_none()
                            && projection
                                .cell(cell.rect, stage, type_, heading)
                                .is_some_and(|cell| cell.contains(screen_point))
                        {
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

    if path.cell.is_none() && !heading_hit {
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
        let type_ = k10s_theme::MapType::default();
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
        let mut heading_hit = false;
        if stage == 0 {
            return Some(path);
        }

        for block_index in scene.region_block_indices(region as usize) {
            let block = &scene.blocks[block_index];
            let presentation = workload_presentation(policy, block.inner, zoom, stage, type_);
            if presentation == WorkloadPresentation::Hidden {
                continue;
            }
            let geometry = presentation
                .card_shown()
                .then(|| projection.workload(scene, block_index, stage, type_));
            let block_primitive = geometry.map_or_else(
                || projection.medallion(block.rect, block.inner),
                |geometry| geometry.body,
            );
            if path.block.is_none() && block_primitive.contains(screen_point) {
                path.block = Some(block_index as u32);
                let heading = geometry.and_then(|geometry| {
                    crate::primitive::workload_label_shown(policy, block.inner.w, zoom)
                        .then_some(geometry.heading)
                        .flatten()
                });
                heading_hit = heading.is_some_and(|heading| heading.contains(screen_point));
                if stage >= 2 && presentation.cells_shown() && !heading_hit {
                    let cells = block.children.len();
                    let aggregated = cells > policy.max_cells_per_block
                        && policy
                            .cells_aggregated(cells, block.inner.intersection_fraction(&visible));
                    if !aggregated {
                        for cell_index in scene.block_cell_indices(block_index) {
                            if projection
                                .cell(scene.cells[cell_index].rect, stage, type_, heading)
                                .is_some_and(|cell| cell.contains(screen_point))
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
            && !heading_hit
            && let Some((block, sat)) = sat_hit
        {
            path.block = Some(block);
            path.sat = Some(sat);
        }
        Some(path)
    }

    #[test]
    fn indexed_and_flat_picking_agree_on_compact_targets_and_abandoned_padding() {
        let (vw, vh) = (1600.0, 1000.0);
        let policy = crate::lod::policy(Default::default());
        let camera = Camera {
            cx: 55.0,
            cy: 81.0,
            zoom: crate::lod::STAGE_POD_LABEL,
        };
        let blend = StageBlend::settled(3);
        let type_ = k10s_theme::MapType::default();
        for sparse in [false, true] {
            for pods in [0, 1] {
                let mut s = scene(SceneSpec::uniform(1, 128));
                s.regions[0].rect = Rect::new(-10000.0, -10000.0, 20000.0, 20000.0);
                s.blocks[0].inner = Rect::new(40.0, 40.0, 30.0, 56.0);
                s.blocks[0].rect = s.blocks[0].inner.inflate(20.0);
                s.blocks[0].sats = 0..0;
                s.cells[0].rect = Rect::new(50.0, 76.0, 10.0, 10.0);
                s.blocks[0].children = 0..pods;
                let cell_index = if sparse {
                    let actual = s.cells[0].clone();
                    s.cells[0].rect = Rect::default();
                    let index = s.cells.len();
                    s.cells.push(actual);
                    let mut members = Vec::new();
                    for (block_index, block) in s.blocks.iter_mut().enumerate() {
                        let start = members.len() as u32;
                        if block_index == 0 {
                            if pods == 1 {
                                members.push(index as u32);
                            }
                        } else {
                            members.extend(block.children.clone());
                        }
                        block.children = start..members.len() as u32;
                    }
                    s.block_cells = members;
                    index
                } else {
                    0
                };
                s.rebuild_spatial_index();
                let projection = Projection::new(camera, (vw, vh), (0.0, 0.0));
                let geometry = projection.workload(&s, 0, 3, type_);
                let original = projection.card(s.blocks[0].inner);
                let padding = point(
                    original.bounds.origin.x + px(20.0),
                    original.bounds.center().y,
                );
                assert!(!geometry.body.contains(padding));
                let heading = geometry.heading.unwrap().bounds.center();
                let hub = projection.bounds(Rect::new(55.0, 68.0, 0.0, 0.0)).origin;
                let mut probes = vec![
                    (padding, Level::Region, None),
                    (heading, Level::Block, None),
                    (hub, Level::Block, None),
                ];
                if pods == 1 {
                    let pod = projection
                        .cell(s.cells[cell_index].rect, 3, type_, geometry.heading)
                        .unwrap();
                    probes.push((pod.bounds.center(), Level::Cell, Some(cell_index as u32)));
                }
                for (point, level, cell) in probes {
                    let (sx, sy) = (f32::from(point.x), f32::from(point.y));
                    let (wx, wy) = camera.s2w(sx, sy, vw, vh);
                    assert!(s.region_block_index_is_selective(0, &Rect::new(wx, wy, 1e-3, 1e-3)));
                    let indexed = pick(&s, &camera, &policy, blend, vw, vh, sx, sy).unwrap();
                    assert_eq!(
                        Some(indexed),
                        reference(&s, &camera, &policy, blend, vw, vh, sx, sy)
                    );
                    assert_eq!((indexed.level(), indexed.cell), (level, cell));
                    assert_eq!(indexed.block, (level != Level::Region).then_some(0));
                }
            }
        }
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
        let type_ = k10s_theme::MapType::default();
        let card_zoom = policy
            .stage_cell
            .max((policy.block_chrome_min_px + 1.0) / block.inner.w);
        for (zoom, expected) in [
            (1.0, WorkloadPresentation::Medallion),
            (card_zoom, WorkloadPresentation::Detailed),
        ] {
            let (cx, cy) = block.inner.center();
            let camera = Camera { cx, cy, zoom };
            let stage = policy.stage_for_zoom(zoom);
            let presentation = workload_presentation(&policy, block.inner, zoom, stage, type_);
            assert_eq!(presentation, expected);
            let projection = Projection::new(camera, (vw, vh), (0.0, 0.0));
            let primitive = if presentation.card_shown() {
                projection.card(block.inner)
            } else {
                projection.medallion(block.rect, block.inner)
            };
            let halo = projection.bounds(block.rect);
            let probe = point(
                px((f32::from(halo.origin.x) + f32::from(primitive.bounds.origin.x)) * 0.5),
                px((f32::from(halo.origin.y) + f32::from(primitive.bounds.origin.y)) * 0.5),
            );
            let (sx, sy) = (f32::from(probe.x), f32::from(probe.y));
            let (wx, wy) = camera.s2w(sx, sy, vw, vh);
            assert!(inside(&block.rect, wx, wy));
            assert!(!primitive.contains(probe));
            assert!(projection.region(s.regions[0].rect).contains(probe));
            if stage >= 2 {
                for sat in s.block_sat_indices(0) {
                    assert!(!projection.satellite(s.sats[sat].rect).contains(probe));
                }
            }
            let path = pick(
                &s,
                &camera,
                &policy,
                StageBlend::settled(stage),
                vw,
                vh,
                sx,
                sy,
            )
            .expect("the spacing point lies inside the painted region");
            assert_eq!(
                (path.level(), path.block),
                (Level::Region, None),
                "the unused halo around {presentation:?} is spacing, not a click target"
            );
        }
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
