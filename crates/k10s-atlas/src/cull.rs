use crate::camera::Camera;
use crate::lod::{LodPolicy, StageBlend};
use crate::scene::{BlockNode, Rect, Scene};
#[cfg(test)]
use crate::scene::{Endpoint, Level};

const DIRECT_REGION_SCAN_LIMIT: usize = 2_048;
const DIRECT_SINGLE_REGION_FRACTION: f32 = 0.125;
const DIRECT_STAGE_ONE_CHILD_LIMIT: usize = 64;
const DIRECT_MULTI_REGION_CHILD_LIMIT: usize = 4_096;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CullStats {
    pub stage: u8,
    pub quads: usize,
    pub drawn_regions: usize,
    pub drawn_blocks: usize,
    pub drawn_cells: usize,
    pub aggregated_blocks: usize,
    pub aggregated_cells: usize,
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
    let visible = camera.visible_world(vw, vh);
    let stage = blend.walk_stage();
    if stage == 0 {
        return cull_stage_zero(scene, camera.zoom, &visible, policy);
    }
    let contiguous = scene.child_ranges_are_direct();
    let single_region_inside = scene.regions.len() == 1 && visible.contains(&scene.regions[0].rect);
    if contiguous && stage != 1 {
        if stage >= 2 && scene.visible_region_has_selective_block_index(&visible) {
            return cull_inner::<true, _, _, _, _>(
                scene,
                camera,
                policy,
                blend,
                vw,
                vh,
                edges_on,
                skip_blocks,
            );
        }
        if scene.region_index_is_selective(&visible) {
            return cull_contiguous::<true, _, _, _, _>(
                scene,
                camera.zoom,
                &visible,
                policy,
                stage,
                edges_on,
                skip_blocks,
            );
        }
        return cull_contiguous::<false, _, _, _, _>(
            scene,
            camera.zoom,
            &visible,
            policy,
            stage,
            edges_on,
            skip_blocks,
        );
    }
    if contiguous
        && stage == 1
        && (scene.spatial_index.is_empty()
            || visible.contains(&scene.bounds)
            || single_region_inside)
    {
        return cull_stage_one_contiguous(scene, camera.zoom, &visible, policy, skip_blocks);
    }
    if contiguous
        && stage == 1
        && match scene.regions.as_slice() {
            [region] => {
                region.rect.intersection_fraction(&visible) >= DIRECT_SINGLE_REGION_FRACTION
            }
            regions => {
                regions.len() <= DIRECT_REGION_SCAN_LIMIT
                    && (scene.spatial_index.max_blocks_per_region() <= DIRECT_STAGE_ONE_CHILD_LIMIT
                        || regions.iter().any(|region| {
                            region.rect.intersects(&visible)
                                && region.children.len() < DIRECT_MULTI_REGION_CHILD_LIMIT
                                && region.rect.intersection_fraction(&visible)
                                    >= DIRECT_SINGLE_REGION_FRACTION
                        }))
            }
        }
    {
        return cull_stage_one_contiguous(scene, camera.zoom, &visible, policy, skip_blocks);
    }
    if stage == 1 {
        return cull_stage_one_indexed(scene, camera.zoom, &visible, policy, skip_blocks);
    }
    if scene.spatial_index.is_empty() {
        cull_inner::<false, _, _, _, _>(scene, camera, policy, blend, vw, vh, edges_on, skip_blocks)
    } else {
        cull_inner::<true, _, _, _, _>(scene, camera, policy, blend, vw, vh, edges_on, skip_blocks)
    }
}

fn cull_stage_zero<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    zoom: f32,
    visible: &Rect,
    policy: &LodPolicy,
) -> CullStats {
    let budget = policy.max_labels;
    let mut drawn_regions = 0usize;
    let mut labels = 0usize;
    let mut labels_dropped = 0usize;
    if scene.region_index_is_selective(visible) {
        scene.for_each_region_candidate(visible, |_, region| {
            if !region.rect.intersects(visible) {
                return;
            }
            drawn_regions += 1;
            if policy.region_label_shown(region.rect.w, zoom) {
                if labels >= budget {
                    labels_dropped += 1;
                } else {
                    labels += 1;
                }
            }
        });
    } else {
        for region in &scene.regions {
            if !region.rect.intersects(visible) {
                continue;
            }
            drawn_regions += 1;
            if policy.region_label_shown(region.rect.w, zoom) {
                if labels >= budget {
                    labels_dropped += 1;
                } else {
                    labels += 1;
                }
            }
        }
    }
    CullStats {
        stage: 0,
        quads: 1 + drawn_regions,
        drawn_regions,
        labels,
        labels_dropped,
        ..CullStats::default()
    }
}

fn cull_stage_one_indexed<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    zoom: f32,
    visible: &Rect,
    policy: &LodPolicy,
    skip_blocks: bool,
) -> CullStats {
    let mut st = CullStats {
        stage: 1,
        quads: 1,
        ..CullStats::default()
    };
    scene.for_each_region_candidate(visible, |region_index, region| {
        if !region.rect.intersects(visible) {
            return;
        }
        st.drawn_regions += 1;
        st.quads += 1;
        if policy.region_label_shown(region.rect.w, zoom) {
            push_label(&mut st, policy);
        }

        let region_inside = visible.contains(&region.rect);
        scene.for_each_region_block_candidate(region_index, visible, |_, block| {
            if !(region_inside || block.rect.intersects(visible)) {
                return;
            }
            if policy.block_painted(block.inner.w, zoom) && !skip_blocks {
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
        });
    });
    st
}

fn cull_stage_one_contiguous<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    zoom: f32,
    visible: &Rect,
    policy: &LodPolicy,
    skip_blocks: bool,
) -> CullStats {
    let mut st = CullStats {
        stage: 1,
        quads: 1,
        ..CullStats::default()
    };
    for region in &scene.regions {
        if !region.rect.intersects(visible) {
            continue;
        }
        st.drawn_regions += 1;
        st.quads += 1;
        if policy.region_label_shown(region.rect.w, zoom) {
            push_label(&mut st, policy);
        }

        let region_inside = visible.contains(&region.rect);
        for block in &scene.blocks[region.children.start as usize..region.children.end as usize] {
            if !(region_inside || block.rect.intersects(visible)) {
                continue;
            }
            if policy.block_painted(block.inner.w, zoom) && !skip_blocks {
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
        }
    }
    st
}

fn cull_contiguous<const INDEX_REGIONS: bool, R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    zoom: f32,
    visible: &Rect,
    policy: &LodPolicy,
    stage: u8,
    edges_on: bool,
    skip_blocks: bool,
) -> CullStats {
    let mut st = CullStats {
        stage,
        quads: 1,
        ..CullStats::default()
    };

    let mut visit_region = |region_index: usize, region: &crate::scene::RegionNode<R>| {
        if !region.rect.intersects(visible) {
            return;
        }
        st.drawn_regions += 1;
        st.quads += 1;

        if policy.region_label_shown(region.rect.w, zoom) {
            push_label(&mut st, policy);
        }
        if stage == 0 {
            return;
        }

        let region_inside = visible.contains(&region.rect);
        let mut visit_block = |block_index: usize, block: &BlockNode<B>| {
            if !(region_inside || block.rect.intersects(visible)) {
                return;
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
                return;
            }

            let block_inside = region_inside || visible.contains(&block.rect);
            if painted || policy.stress_curves {
                for satellite in &scene.sats[block.sats.start as usize..block.sats.end as usize] {
                    if !(block_inside || satellite.rect.intersects(visible))
                        || !policy.sat_painted(satellite.rect.w, zoom)
                    {
                        continue;
                    }
                    st.drawn_sats += 1;
                    if policy.sat_icon_shown() {
                        push_icon(&mut st, policy);
                    }
                    if policy.sat_label_shown(satellite.rect.w, zoom) {
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
                return;
            }

            let cell_count = block.children.len();
            if cell_count > policy.max_cells_per_block
                && policy.cells_aggregated(cell_count, block.inner.intersection_fraction(visible))
            {
                st.aggregated_blocks += 1;
                st.aggregated_cells += cell_count;
                st.quads += 1;
                return;
            }

            let cells = &scene.cells[block.children.start as usize..block.children.end as usize];
            if block_inside
                || (block.inner.w > 0.0 && block.inner.h > 0.0 && visible.contains(&block.inner))
            {
                cull_cell_slice(cells, zoom, stage, policy, &mut st);
            } else if scene.block_cell_index_is_selective(block_index, visible) {
                scene.for_each_block_cell_candidate(block_index, visible, |_, cell| {
                    if cell.rect.intersects(visible) {
                        cull_cell(cell, zoom, stage, policy, &mut st);
                    }
                });
            } else {
                for cell in cells {
                    if cell.rect.intersects(visible) {
                        cull_cell(cell, zoom, stage, policy, &mut st);
                    }
                }
            }
        };
        if scene.region_block_index_is_selective(region_index, visible) {
            scene.for_each_region_block_candidate(region_index, visible, &mut visit_block);
        } else {
            let start = region.children.start as usize;
            for (offset, block) in scene.blocks[start..region.children.end as usize]
                .iter()
                .enumerate()
            {
                visit_block(start + offset, block);
            }
        }
    };
    if INDEX_REGIONS {
        scene.for_each_region_candidate(visible, &mut visit_region);
    } else {
        for (index, region) in scene.regions.iter().enumerate() {
            visit_region(index, region);
        }
    }

    if edges_on && stage >= 2 && !policy.stress && !policy.stress_curves {
        st.edges = walk_edges(scene, visible, policy.max_edges, |_, _| {});
    }
    st
}

#[expect(clippy::too_many_arguments)]
#[inline(always)]
fn cull_inner<const INDEXED: bool, R, B, C, S>(
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

    scene.for_each_region_candidate_mode::<INDEXED>(&visible, |region_index, region| {
        if !region.rect.intersects(&visible) {
            return;
        }
        st.drawn_regions += 1;
        st.quads += 1;

        if policy.region_label_shown(region.rect.w, zoom) {
            push_label(&mut st, policy);
        }

        if stage == 0 {
            return;
        }

        let region_inside = visible.contains(&region.rect);
        scene.for_each_region_block_candidate_mode::<INDEXED>(
            region_index,
            &visible,
            |block_index, block| {
                cull_block::<INDEXED, _, _, _, _>(
                    scene,
                    block_index,
                    block,
                    &visible,
                    region_inside,
                    zoom,
                    stage,
                    policy,
                    skip_blocks,
                    &mut st,
                );
            },
        );
    });

    if edges_on && stage >= 2 && !policy.stress && !policy.stress_curves {
        st.edges = walk_edges(scene, &visible, policy.max_edges, |_, _| {});
    }

    st
}

#[inline]
fn cull_block<const INDEXED: bool, R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    block_index: usize,
    block: &BlockNode<B>,
    visible: &Rect,
    region_inside: bool,
    zoom: f32,
    stage: u8,
    policy: &LodPolicy,
    skip_blocks: bool,
    st: &mut CullStats,
) {
    if !(region_inside || block.rect.intersects(visible)) {
        return;
    }

    let painted = policy.block_painted(block.inner.w, zoom) && !skip_blocks;
    if painted {
        st.drawn_blocks += 1;
        st.quads += 1;

        if policy.block_chrome_shown(block.inner.w, zoom) {
            st.quads += 2;
        }
        if policy.block_icon_shown(block.inner.w, zoom) {
            push_icon(st, policy);
        }
        if policy.block_label_shown(block.inner.w, zoom) {
            push_label(st, policy);
        }
    }

    if stage < 2 {
        return;
    }
    let block_inside = region_inside || visible.contains(&block.rect);

    if painted || policy.stress_curves {
        scene.for_each_block_sat(block_index, |_, sat| {
            if !(block_inside || sat.rect.intersects(visible))
                || !policy.sat_painted(sat.rect.w, zoom)
            {
                return;
            }
            st.drawn_sats += 1;
            if policy.sat_icon_shown() {
                push_icon(st, policy);
            }
            if policy.sat_label_shown(sat.rect.w, zoom) {
                push_label(st, policy);
                push_label(st, policy);
            }
            if policy.sat_curves {
                if st.curves >= policy.curve_budget() {
                    st.curves_dropped += 1;
                } else {
                    st.curves += 1;
                }
            }
        });
    }

    if !painted {
        return;
    }
    let cells = block.children.len();
    if cells > policy.max_cells_per_block
        && policy.cells_aggregated(cells, block.inner.intersection_fraction(visible))
    {
        st.aggregated_blocks += 1;
        st.aggregated_cells += cells;
        st.quads += 1;
        return;
    }
    if block_inside
        || (block.inner.w > 0.0 && block.inner.h > 0.0 && visible.contains(&block.inner))
    {
        scene.for_each_block_cell(block_index, |_, cell| {
            cull_cell(cell, zoom, stage, policy, st);
        });
    } else {
        scene.for_each_block_cell_candidate_mode::<INDEXED>(block_index, visible, |_, cell| {
            if cell.rect.intersects(visible) {
                cull_cell(cell, zoom, stage, policy, st);
            }
        });
    }
}

#[inline]
fn cull_cell_slice<C>(
    cells: &[crate::scene::CellNode<C>],
    zoom: f32,
    stage: u8,
    policy: &LodPolicy,
    st: &mut CullStats,
) {
    st.drawn_cells += cells.len();
    st.quads += cells.len();
    if stage < 3 {
        return;
    }

    let labels = cells
        .iter()
        .filter(|cell| policy.cell_label_shown(cell.rect.w, zoom))
        .count();
    let accepted = labels.min(policy.max_labels.saturating_sub(st.labels));
    st.labels += accepted;
    st.labels_dropped += labels - accepted;
}

#[inline(always)]
fn cull_cell<C>(
    cell: &crate::scene::CellNode<C>,
    zoom: f32,
    stage: u8,
    policy: &LodPolicy,
    st: &mut CullStats,
) {
    st.drawn_cells += 1;
    st.quads += 1;
    if stage >= 3 && policy.cell_label_shown(cell.rect.w, zoom) {
        push_label(st, policy);
    }
}

fn edge_visible(a: (f32, f32), b: (f32, f32), visible: &Rect) -> bool {
    let (ax, ay) = a;
    let (bx, by) = b;
    Rect::new(
        ax.min(bx),
        ay.min(by),
        (ax - bx).abs().max(1.0),
        (ay - by).abs().max(1.0),
    )
    .intersects(visible)
}

#[cfg(test)]
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
    mut emit: impl FnMut((f32, f32), (f32, f32)),
) -> usize {
    if max_edges == 0 || scene.edges.is_empty() {
        return 0;
    }

    let mut drawn = 0usize;

    if scene.region_edges.len() == scene.regions.len() && !scene.region_edges.is_empty() {
        for (region_index, (region, range)) in
            scene.regions.iter().zip(&scene.region_edges).enumerate()
        {
            if range.is_empty() || !region.rect.intersects(visible) {
                continue;
            }
            let used_index = scene
                .region_edge_indexes
                .get(region_index)
                .is_some_and(|index| {
                    index.covers(range)
                        && scan_indexed_edges(
                            index, scene, visible, max_edges, &mut drawn, &mut emit,
                        )
                });
            if !used_index
                && !scan_edges(
                    scene,
                    range.start as usize..range.end as usize,
                    visible,
                    max_edges,
                    &mut drawn,
                    &mut emit,
                )
            {
                return drawn;
            }
        }
        let used_index = scene.cross_edge_index.covers(&scene.cross_edges)
            && scan_indexed_edges(
                &scene.cross_edge_index,
                scene,
                visible,
                max_edges,
                &mut drawn,
                &mut emit,
            );
        if !used_index {
            scan_edges(
                scene,
                scene.cross_edges.start as usize..scene.cross_edges.end as usize,
                visible,
                max_edges,
                &mut drawn,
                &mut emit,
            );
        }
    } else {
        scan_edges(
            scene,
            0..scene.edges.len(),
            visible,
            max_edges,
            &mut drawn,
            &mut emit,
        );
    }
    drawn
}

#[inline]
fn scan_edges<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    range: std::ops::Range<usize>,
    visible: &Rect,
    max_edges: usize,
    drawn: &mut usize,
    emit: &mut impl FnMut((f32, f32), (f32, f32)),
) -> bool {
    if let Some(segments) = scene.edge_segments.get(range.clone()) {
        for &segment in segments {
            let Some(segment) = segment else {
                continue;
            };
            if *drawn >= max_edges {
                return false;
            }
            if edge_visible(segment.a, segment.b, visible) {
                emit(segment.a, segment.b);
                *drawn += 1;
            }
        }
        return true;
    }

    for &edge in &scene.edges[range] {
        if *drawn >= max_edges {
            return false;
        }
        let Some(segment) = crate::scene::resolve_edge(scene, edge) else {
            continue;
        };
        if edge_visible(segment.a, segment.b, visible) {
            emit(segment.a, segment.b);
            *drawn += 1;
        }
    }
    true
}

fn scan_indexed_edges<R, B, C, S>(
    index: &crate::scene::EdgeIndex,
    scene: &Scene<R, B, C, S>,
    visible: &Rect,
    max_edges: usize,
    drawn: &mut usize,
    emit: &mut impl FnMut((f32, f32), (f32, f32)),
) -> bool {
    if !index.is_selective(visible) {
        return false;
    }
    index.for_each_candidate(visible, |range| {
        scan_edges(scene, range, visible, max_edges, drawn, emit)
    });
    true
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
            max_cells_per_block: 1024,
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
            region_blocks: vec![],
            block_cells: vec![],
            block_sats: vec![],
            spatial_index: Default::default(),
            edges: vec![
                Edge::blocks(0, 1),
                Edge::blocks(2, 3),
                Edge::blocks(4, 5),
                Edge::blocks(0, 5),
            ],
            edge_segments: vec![],
            region_edges: vec![0..1, 1..2, 2..3],
            region_edge_indexes: vec![],
            cross_edges: 3..4,
            cross_edge_index: Default::default(),
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
        let n = walk_edges(&scene, &visible, 100, |a, b| grouped.push((a, b)));
        assert_eq!(n, 3);

        scene.region_edges.clear();
        let mut flat = Vec::new();
        let n = walk_edges(&scene, &visible, 100, |a, b| flat.push((a, b)));
        assert_eq!(n, 3);
        assert_eq!(grouped, flat);

        scene.region_edges = vec![0..1, 1..2, 2..3];
        assert_eq!(walk_edges(&scene, &visible, 2, |_, _| {}), 2);
    }

    #[test]
    fn region_edges_are_a_spatial_index() {
        let specs = [
            SceneSpec::uniform(200, 15),
            SceneSpec::fan_out(500),
            SceneSpec::fan_out(2000),
            SceneSpec::fan_out(8000),
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
                        if endpoint_rect(&scene, e.a).is_none()
                            || endpoint_rect(&scene, e.b).is_none()
                        {
                            panic!("{built} {spec:?}: region {i} groups a dangling endpoint");
                        }
                        assert!(
                            region.rect.contains(
                                &crate::scene::edge_bounds(&scene, *e).expect("resolved above")
                            ),
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
            Edge {
                a: Endpoint::cell(0),
                b: Endpoint::sat(9),
            },
        ];

        let visible = Rect::new(0.0, 0.0, 400.0, 400.0);
        let mut seen = Vec::new();
        let n = walk_edges(&scene, &visible, 100, |a, b| seen.push((a, b)));

        assert_eq!(n, 2, "the dangling endpoint must be skipped, not counted");
        assert_eq!(
            seen,
            vec![
                (scene.cells[0].rect.center(), scene.sats[1].rect.center()),
                (
                    scene.blocks[0].inner.center(),
                    scene.regions[0].rect.center()
                ),
            ]
        );
        assert_ne!(
            scene.blocks[0].inner, scene.blocks[0].rect,
            "the card and the halo must differ or the assertion above is empty"
        );
    }

    // A scene built to make the edge index's *tree* matter, which `cross_scene`
    // structurally cannot.
    //
    // Two numbers govern this. `EDGE_INDEX_CANDIDATE_LIMIT` is two leaves' worth,
    // and `is_selective` counts whole leaves rather than intersecting segments --
    // so an index over a range that splits into full sixteens declines the moment
    // a query needs a second leaf, and every query it accepts is answered by one.
    // A range of 68 splits to leaves of eight and nine instead, and three of those
    // still fit under the limit: a query can be selective *and* need more than one
    // subtree.
    //
    // The other half is that the segments must be short and their array order must
    // not follow their positions, or a small viewport picks up a contiguous run
    // that lives in a single leaf anyway. Blocks are laid in a row, each edge joins
    // two neighbours, and the edges are emitted under a stride that walks the row
    // coprime to its length -- so segments that sit beside each other on screen are
    // as far apart in the array as the permutation can put them.
    const ROW_EDGES: usize = 68;
    const ROW_PITCH: f32 = 30.0;

    fn scattered_edge_scene() -> Scene {
        let blocks: Vec<BlockNode> = (0..=ROW_EDGES)
            .map(|i| {
                let rect = Rect::new(i as f32 * ROW_PITCH, 100.0, 20.0, 20.0);
                BlockNode {
                    rect,
                    inner: rect,
                    label: "b".into(),
                    children: 0..0,
                    sats: 0..0,
                    ext: (),
                }
            })
            .collect();
        // 37 is coprime with 68, so this visits every position exactly once and
        // neighbouring positions land far apart in the array.
        let edges: Vec<Edge> = (0..ROW_EDGES)
            .map(|k| {
                let at = (k * 37) % ROW_EDGES;
                Edge::blocks(at as u32, at as u32 + 1)
            })
            .collect();
        let width = (ROW_EDGES + 1) as f32 * ROW_PITCH;
        // Collected rather than written as a literal: one region means one range,
        // and a single-element vector of ranges reads to the linter as a range
        // being used as a collection, which this is not.
        let region_edges: Vec<std::ops::Range<u32>> =
            std::iter::once(0..ROW_EDGES as u32).collect();
        let mut scene = Scene {
            rev: 1,
            bounds: Rect::new(0.0, 0.0, width, 240.0),
            regions: vec![RegionNode {
                rect: Rect::new(0.0, 0.0, width, 240.0),
                label: "row".into(),
                weight: 1,
                children: 0..blocks.len() as u32,
                ext: (),
            }],
            totals: Totals {
                regions: 1,
                blocks: blocks.len() as u32,
                cells: 0,
                sats: 0,
                edges: edges.len() as u32,
            },
            blocks,
            cells: vec![],
            sats: vec![],
            region_blocks: vec![],
            block_cells: vec![],
            block_sats: vec![],
            spatial_index: Default::default(),
            region_edges,
            region_edge_indexes: vec![],
            cross_edges: ROW_EDGES as u32..ROW_EDGES as u32,
            cross_edge_index: Default::default(),
            edge_segments: vec![],
            edges,
        };
        scene.rebuild_edge_indexes();
        scene
    }

    #[test]
    fn the_edge_index_answers_what_a_flat_scan_answers_when_its_tree_is_needed() {
        let indexed = scattered_edge_scene();
        assert!(
            indexed.region_edge_indexes[0].covers(&indexed.region_edges[0]),
            "the fixture is under the build threshold, so there is no tree to test"
        );
        let mut flat = indexed.clone();
        flat.region_edge_indexes.clear();
        flat.cross_edge_index = Default::default();

        let mut engaged = 0usize;
        let mut drew = 0usize;
        for step in 0..ROW_EDGES {
            // A window a few blocks wide: enough segments to need more than one
            // leaf, few enough to stay under the candidate limit.
            let x = step as f32 * ROW_PITCH;
            for width in [ROW_PITCH * 2.5, ROW_PITCH * 3.5] {
                let visible = Rect::new(x, 90.0, width, 40.0);
                if indexed.region_edge_indexes[0].is_selective(&visible) {
                    engaged += 1;
                }
                for budget in [1, 7, usize::MAX] {
                    let mut with = Vec::new();
                    let a = walk_edges(&indexed, &visible, budget, |p, q| with.push((p, q)));
                    let mut without = Vec::new();
                    let b = walk_edges(&flat, &visible, budget, |p, q| without.push((p, q)));
                    assert_eq!(a, b, "{visible:?}, budget {budget}");
                    assert_eq!(with, without, "{visible:?}, budget {budget}");
                    drew += a;
                }
            }
        }
        assert!(
            engaged > 0,
            "the index declined every viewport, so the flat path answered them all"
        );
        assert!(
            drew > 0,
            "no viewport drew an edge, so nothing was compared"
        );

        // And the same grazing pass the spatial forests get, for the same reason:
        // a window seventy-five units wide cannot notice a node whose bounds is
        // short by one, because it overlaps by far more than the mistake. These
        // sit on the corners of the segments themselves, which is where a leaf's
        // own bounds ends.
        let mut grazed = 0usize;
        for k in 0..ROW_EDGES {
            let at = (k * 37) % ROW_EDGES;
            let x0 = at as f32 * ROW_PITCH + 10.0;
            let x1 = (at + 1) as f32 * ROW_PITCH + 10.0;
            for x in [x0, x1] {
                for offset in [-0.003f32, -0.001] {
                    let visible = Rect::new(x + offset, 109.0, 0.004, 3.0);
                    let mut with = Vec::new();
                    let a = walk_edges(&indexed, &visible, usize::MAX, |p, q| with.push((p, q)));
                    let mut without = Vec::new();
                    let b = walk_edges(&flat, &visible, usize::MAX, |p, q| without.push((p, q)));
                    assert_eq!(a, b, "grazing {visible:?}");
                    assert_eq!(with, without, "grazing {visible:?}");
                    grazed += a;
                }
            }
        }
        assert!(
            grazed > 0,
            "no grazing viewport reached a segment, so none of them proves anything"
        );
    }

    // What this covers, and what covers the rest, because the name claims more
    // than it delivers and the difference was measured rather than guessed.
    //
    // It covers the *decision*: that a scene with edge indexes and the same scene
    // without them draw the same edges in the same order under every budget. That
    // is the property a fallback path most easily breaks, and it is real.
    //
    // It does not cover the tree, and cannot. An unconditional `panic!` in the
    // right-subtree recursion of `EdgeIndex::visit` is reached by none of this
    // crate's tests while this fixture is the only one -- deleting that descent
    // outright left all 114 tests across this crate and `k10s-map` green. The
    // mechanism is arithmetic rather than oversight: `is_selective` counts whole
    // leaves against a limit of two leaves' worth, so an index whose range splits
    // into full sixteens declines the moment a query needs a second leaf, and
    // every query it accepts is answered inside one. `cross_scene`'s long
    // diagonals cannot escape that -- a viewport intersecting several of them
    // intersects far too many to stay selective -- and eight hundred thin strips
    // swept across this scene found no viewport that tells the two apart.
    //
    // `the_edge_index_answers_what_a_flat_scan_answers_when_its_tree_is_needed`
    // is the fixture that can: 68 edges split into leaves of eight and nine, so
    // three of them still fit under the candidate limit, over short segments whose
    // array order is a coprime stride away from their positions. Dropping either
    // subtree, dropping one segment from a leaf, or shrinking a node's bounds by a
    // single unit all fail it -- the last only because it grazes the segments'
    // own corners, since a window seventy-five units wide overlaps by far more
    // than that mistake and would never notice.
    #[test]
    fn edge_indexes_preserve_flat_order_and_budgets() {
        let indexed = crate::testing::cross_scene(
            SceneSpec {
                cells_per_block: 2,
                sats_per_block: 1,
                edges_per_region: 97,
                ..SceneSpec::uniform(8, 6)
            },
            97,
        );
        assert!(
            indexed
                .region_edge_indexes
                .iter()
                .zip(&indexed.region_edges)
                .all(|(index, range)| index.covers(range))
        );
        assert!(indexed.cross_edge_index.covers(&indexed.cross_edges));

        let mut flat = indexed.clone();
        flat.region_edge_indexes.clear();
        flat.cross_edge_index = Default::default();
        let viewports = [
            indexed.regions[0].rect,
            indexed.regions[3].rect,
            Rect::new(
                indexed.bounds.w * 0.25,
                indexed.bounds.h * 0.25,
                indexed.bounds.w * 0.5,
                indexed.bounds.h * 0.5,
            ),
            indexed.bounds.inflate(100.0),
            Rect::new(-10_000.0, -10_000.0, 100.0, 100.0),
        ];

        for visible in viewports {
            for budget in [0, 1, 7, 31, usize::MAX] {
                let mut indexed_edges = Vec::new();
                let indexed_count = walk_edges(&indexed, &visible, budget, |a, b| {
                    indexed_edges.push((a, b));
                });
                let mut flat_edges = Vec::new();
                let flat_count = walk_edges(&flat, &visible, budget, |a, b| {
                    flat_edges.push((a, b));
                });
                assert_eq!(indexed_count, flat_count, "{visible:?}, budget {budget}");
                assert_eq!(indexed_edges, flat_edges, "{visible:?}, budget {budget}");
            }
        }
    }

    // Which forests a sweep actually reached. A comparison between an indexed
    // scene and a flat one is worth exactly nothing if the index was never built,
    // or was built and never consulted, or was consulted and never declined --
    // and each of those is a silent pass, so each is a field here.
    #[derive(Default, Debug)]
    struct ReachedIndex {
        region_used: bool,
        region_declined: bool,
        block_used: bool,
        block_declined: bool,
        cell_used: bool,
        cell_declined: bool,
        stages: [bool; 4],
        drew_regions: bool,
        drew_blocks: bool,
        drew_cells: bool,
        drew_edges: bool,
    }

    // The cull oracle's independence is independence of *code*: two traversals,
    // two crates, written apart. It is not independence of *data*. Both reach the
    // scene through the same `for_each_*_candidate` helpers, so a `SpatialForest`
    // that dropped a candidate would drop it for the painter and for the oracle
    // alike, and the two would agree -- exactly, counter for counter -- about a
    // scene with a region missing from it. Agreement can only mean something
    // while something else says the index is telling the truth.
    //
    // The edge indexes have had that something since they were written, one test
    // above. The region, block and cell forests have not: clearing
    // `spatial_index` appears once in this repository, in a bench, where it
    // measures how much faster the index is rather than whether it is right.
    //
    // Two fixtures, because the build thresholds cannot be satisfied by one. A
    // region forest wants 64 regions and a block forest wants 128 blocks under a
    // single region; a cell forest only drops to 128 cells per block while the
    // whole scene has at most 64 blocks in it, and above that wants 8,192, which
    // no unit test should build. So one scene is wide and one is deep, and each
    // asserts the forest it exists for was really built.
    fn assert_index_matches_flat(indexed: &Scene, label: &str, reached: &mut ReachedIndex) {
        const VW: f32 = 1600.0;
        const VH: f32 = 1000.0;
        assert!(
            !indexed.spatial_index.is_empty(),
            "{label}: no forest was built, so this comparison proves nothing"
        );
        let mut flat = indexed.clone();
        flat.spatial_index = Default::default();
        assert!(flat.spatial_index.is_empty());

        let pol = policy();
        let (rx, ry) = indexed.regions[0].rect.center();
        let (bx, by) = indexed.blocks[0].inner.center();
        let mut fit = Camera::default();
        fit.fit(indexed.bounds, VW, VH);
        let cameras = [
            fit,
            Camera {
                cx: rx,
                cy: ry,
                zoom: 0.05,
            },
            Camera {
                cx: rx,
                cy: ry,
                zoom: 0.2,
            },
            Camera {
                cx: rx,
                cy: ry,
                zoom: 1.0,
            },
            Camera {
                cx: bx,
                cy: by,
                zoom: 1.0,
            },
            Camera {
                cx: bx,
                cy: by,
                zoom: 4.5,
            },
            Camera {
                cx: bx,
                cy: by,
                zoom: 18.0,
            },
            // Off the scene entirely: the empty answer has to match too, and it
            // is the case where an index is most tempted to answer early.
            Camera {
                cx: indexed.bounds.x - indexed.bounds.w,
                cy: indexed.bounds.y - indexed.bounds.h,
                zoom: 2.0,
            },
        ];
        // Settled at each stage, plus one crossfade in each direction, because
        // `walk_stage` takes the max and the dispatcher branches on it.
        let blends = [
            StageBlend::settled(0),
            StageBlend::settled(1),
            StageBlend::settled(2),
            StageBlend::settled(3),
            StageBlend {
                from: 1,
                to: 2,
                t: 0.5,
            },
            StageBlend {
                from: 3,
                to: 1,
                t: 0.5,
            },
        ];

        for cam in cameras {
            let visible = cam.visible_world(VW, VH);
            if indexed.region_index_is_selective(&visible) {
                reached.region_used = true;
            } else {
                reached.region_declined = true;
            }
            for region in 0..indexed.regions.len() {
                if indexed.region_block_index_is_selective(region, &visible) {
                    reached.block_used = true;
                } else {
                    reached.block_declined = true;
                }
            }
            for block in 0..indexed.blocks.len() {
                if indexed.block_cell_index_is_selective(block, &visible) {
                    reached.cell_used = true;
                } else {
                    reached.cell_declined = true;
                }
            }
            for blend in blends {
                reached.stages[blend.walk_stage() as usize] = true;
                for edges_on in [false, true] {
                    for skip_blocks in [false, true] {
                        let with = cull(indexed, &cam, &pol, blend, VW, VH, edges_on, skip_blocks);
                        let without = cull(&flat, &cam, &pol, blend, VW, VH, edges_on, skip_blocks);
                        assert_eq!(
                            with, without,
                            "{label}: the index and a flat scan disagree at {cam:?} \
                             {blend:?} edges={edges_on} skip_blocks={skip_blocks}"
                        );
                        reached.drew_regions |= with.drawn_regions > 0;
                        reached.drew_blocks |= with.drawn_blocks > 0;
                        reached.drew_cells |= with.drawn_cells > 0;
                        reached.drew_edges |= with.edges > 0;
                    }
                }
            }
        }
    }

    // Queries small enough that a rect overlaps one by less than any plausible
    // epsilon, placed around a point at several distances from it. The overlap
    // has to be smaller than the mistake for the mistake to show: a probe that
    // overlaps by half a unit cannot detect a query shaved by a quarter of one.
    fn graze_at(x: f32, y: f32, out: &mut Vec<Rect>) {
        const GRAZE: [f32; 4] = [-0.3, -0.003, 0.003, 0.3];
        const TINY: f32 = 0.004;
        for dx in GRAZE {
            for dy in GRAZE {
                out.push(Rect::new(x + dx, y + dy, TINY, TINY));
            }
        }
    }

    // Probe rects built out of the scene's own geometry, so their edges land
    // exactly on the edges the forest split on, and small enough that a rect
    // overlaps one by less than any plausible epsilon. Both halves matter: a
    // grid of arbitrary rects tests the middle of things, which is where nothing
    // goes wrong, and a probe that overlaps by half a unit cannot detect a query
    // shaved by a quarter of one. The overlap has to be smaller than the mistake
    // for the mistake to show.
    fn boundary_probes(scene: &Scene) -> Vec<Rect> {
        let mut probes = Vec::new();
        let mut at = |x: f32, y: f32| graze_at(x, y, &mut probes);
        let mut around = |r: Rect| {
            at(r.x, r.y);
            at(r.x + r.w, r.y + r.h);
            at(r.x + r.w, r.y + r.h * 0.5);
            at(r.x + r.w * 0.5, r.y);
            // The centre too: an edge segment ends at a node's centre, so this is
            // where a probe grazes the corner of a *segment's* bounds rather than
            // a node's, which is what the edge forest splits on.
            at(r.x + r.w * 0.5, r.y + r.h * 0.5);
        };
        // The scene's own corner first: the outermost leaf's bounds ends exactly
        // where the scene does, so a probe there is the one guaranteed to sit in
        // the outer band of every node above it. The rest are a handful from each
        // level rather than a sweep of all of them -- the probes are the sharp
        // part of this instrument, but each one costs a pass over the scene, and
        // eight of a kind find what eight hundred would.
        around(scene.bounds);
        let some = |len: usize| (0..len).step_by((len / 8).max(1)).take(8);
        for index in some(scene.regions.len()) {
            around(scene.regions[index].rect);
        }
        for index in some(scene.blocks.len()) {
            around(scene.blocks[index].rect);
        }
        for index in some(scene.cells.len()) {
            around(scene.cells[index].rect);
        }
        probes
    }

    // One reusable membership set. A fresh `vec![false; n]` per parent per probe
    // is what made the first draft of this too slow to afford enough probes to
    // be sharp, which is a real trade and not a micro-optimisation: the probes
    // are the whole instrument.
    struct Offered {
        seen: Vec<bool>,
        touched: Vec<usize>,
    }

    impl Offered {
        fn new(len: usize) -> Offered {
            Offered {
                seen: vec![false; len],
                touched: Vec::new(),
            }
        }

        fn mark(&mut self, index: usize) {
            if !self.seen[index] {
                self.seen[index] = true;
                self.touched.push(index);
            }
        }

        fn clear(&mut self) {
            for index in self.touched.drain(..) {
                self.seen[index] = false;
            }
        }
    }

    // What the counters above compare is what the cull *concluded*; this compares
    // what the index *offered*. The difference is not academic: shrinking every
    // node's bounds by a whole unit, or the query rect by a quarter of one,
    // passes the counter comparison at every camera in that sweep and fails here.
    // A camera sweep is a coarse way to go looking for a candidate that grazes,
    // because a camera frames what is in front of it and the bug lives at the
    // edge of what is not.
    //
    // The contract is a superset rather than an equality. A candidate list may
    // contain rects that miss, because every caller re-tests with `intersects`;
    // what may never happen is that something which really does intersect is
    // absent from it.
    fn assert_index_offers_every_intersecting_child(scene: &Scene, label: &str) {
        let mut regions = Offered::new(scene.regions.len());
        let mut blocks = Offered::new(scene.blocks.len());
        let mut cells = Offered::new(scene.cells.len());
        let mut wanted: Vec<usize> = Vec::new();

        for probe in boundary_probes(scene) {
            regions.clear();
            scene.for_each_region_candidate(&probe, |index, _| regions.mark(index));
            for (index, region) in scene.regions.iter().enumerate() {
                assert!(
                    !region.rect.intersects(&probe) || regions.seen[index],
                    "{label}: region {index} at {:?} intersects {probe:?}, \
                     and the index did not offer it",
                    region.rect
                );
            }

            for region in 0..scene.regions.len() {
                wanted.clear();
                scene.for_each_region_block(region, |index, block| {
                    if block.rect.intersects(&probe) {
                        wanted.push(index);
                    }
                });
                if wanted.is_empty() {
                    continue;
                }
                blocks.clear();
                scene
                    .for_each_region_block_candidate(region, &probe, |index, _| blocks.mark(index));
                for &index in &wanted {
                    assert!(
                        blocks.seen[index],
                        "{label}: block {index} of region {region} at {:?} intersects \
                         {probe:?}, and the index did not offer it",
                        scene.blocks[index].rect
                    );
                }
            }

            for block in 0..scene.blocks.len() {
                wanted.clear();
                scene.for_each_block_cell(block, |index, cell| {
                    if cell.rect.intersects(&probe) {
                        wanted.push(index);
                    }
                });
                if wanted.is_empty() {
                    continue;
                }
                cells.clear();
                scene.for_each_block_cell_candidate(block, &probe, |index, _| cells.mark(index));
                for &index in &wanted {
                    assert!(
                        cells.seen[index],
                        "{label}: cell {index} of block {block} at {:?} intersects \
                         {probe:?}, and the index did not offer it",
                        scene.cells[index].rect
                    );
                }
            }
        }
    }

    #[test]
    fn the_spatial_index_answers_what_a_flat_scan_answers() {
        let mut reached = ReachedIndex::default();

        // Wide: enough regions for a region forest, and enough blocks under each
        // for a per-region block forest.
        let wide = crate::testing::scene(SceneSpec {
            cells_per_block: 2,
            sats_per_block: 1,
            edges_per_region: 3,
            ..SceneSpec::uniform(72, 136)
        });
        assert_index_matches_flat(&wide, "wide", &mut reached);
        assert_index_offers_every_intersecting_child(&wide, "wide");

        // Deep: few enough blocks that the cell threshold stays at 128, and
        // enough cells under each to cross it.
        let deep = crate::testing::scene(SceneSpec {
            cells_per_block: 160,
            sats_per_block: 2,
            edges_per_region: 3,
            ..SceneSpec::uniform(8, 8)
        });
        assert_index_matches_flat(&deep, "deep", &mut reached);
        assert_index_offers_every_intersecting_child(&deep, "deep");

        assert!(
            reached.region_used && reached.region_declined,
            "the region forest was never both consulted and declined: {reached:?}"
        );
        assert!(
            reached.block_used && reached.block_declined,
            "the block forests were never both consulted and declined: {reached:?}"
        );
        assert!(
            reached.cell_used && reached.cell_declined,
            "the cell forests were never both consulted and declined: {reached:?}"
        );
        assert!(
            reached.stages.iter().all(|hit| *hit),
            "a walk stage was never reached: {reached:?}"
        );
        assert!(
            reached.drew_regions && reached.drew_blocks && reached.drew_cells && reached.drew_edges,
            "the sweep never drew one of the four things it compares: {reached:?}"
        );
    }

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

    // What the fit camera costs, since it is the one camera
    // `visible_work_is_independent_of_scene_size` deliberately excludes.
    //
    // That exclusion is honest -- at Z0 with the whole scene framed, every region
    // is visible, so visible work *cannot* be independent of how many there are.
    // What has been prose rather than a test is the shape of the growth, and
    // "unmeasured" and "linear" look identical in a roadmap. This pins it: the
    // region term is exactly one quad each and grows exactly with the count, the
    // label term is held by its budget rather than by the count, and nothing below
    // the region level is touched at all.
    //
    // So a change that made Z0 quadratic, or that started drawing blocks there, or
    // that let labels grow with the cluster, fails here -- and the aggregation §I
    // needs before multi-cluster has a number to beat rather than an adjective.
    #[test]
    fn the_fit_camera_costs_one_quad_a_region_and_nothing_deeper() {
        const VW: f32 = 1600.0;
        const VH: f32 = 1000.0;
        const SIZES: [usize; 4] = [200, 400, 800, 1600];
        let pol = policy();

        let mut costs: Vec<(usize, usize)> = Vec::new();
        for regions in SIZES {
            let scene = crate::testing::scene(SceneSpec::uniform(regions, 15));
            let mut cam = Camera::default();
            cam.fit(scene.bounds, VW, VH);
            assert!(
                cam.zoom < pol.stage_block,
                "{regions} regions: fit is not Z0 at zoom {}",
                cam.zoom
            );
            let st = cull(&scene, &cam, &pol, settled(&pol, &cam), VW, VH, true, false);

            // Named explicitly, because at this zoom the deeper traversals draw
            // no blocks either -- so every other assertion here would hold while
            // the dispatcher quietly took the O(children) path.
            assert_eq!(st.stage, 0, "{regions} regions: the fit camera left Z0");
            assert_eq!(
                st.drawn_regions, regions,
                "{regions} regions: the fit camera must see all of them or this \
                 measures something else"
            );
            // One backdrop quad plus one per region, and that is the whole quad
            // cost at this stage. A second quad per region would double the term
            // this test exists to bound.
            assert_eq!(
                st.quads,
                1 + regions,
                "{regions} regions: the per-region quad cost moved"
            );
            // Nothing below the region level is reached at Z0, which is the other
            // half of the bound: the cost is O(regions), not O(objects).
            assert_eq!(
                (st.drawn_blocks, st.drawn_cells, st.drawn_sats, st.curves),
                (0, 0, 0, 0),
                "{regions} regions: Z0 descended past the region level"
            );
            // No text at all, and that is measured rather than hoped: a region
            // framed inside a two-hundred-namespace scene is narrower on screen
            // than `region_label_min_px`, so the pixel threshold refuses every one
            // of them before the label budget is ever consulted. It is part of why
            // Z0 is survivable at fleet scale, and equally why §I wants
            // aggregation here -- what a person sees at fit is sixteen hundred
            // unnamed rectangles.
            assert_eq!(
                (st.labels, st.labels_dropped),
                (0, 0),
                "{regions} regions: Z0 fit drew text, so the pixel threshold moved \
                 and the budget is now the only thing standing between this camera \
                 and text work shaped like the cluster"
            );
            costs.push((regions, st.quads));
        }

        // Linear, and exactly linear. Counts eight times apart must differ by
        // exactly the ratio of their regions, or the per-region term has picked up
        // a second factor -- which at this camera is the one growth the engine
        // cannot absorb, since every region is visible here by definition.
        let (small_n, small_q) = costs[0];
        let (large_n, large_q) = costs[costs.len() - 1];
        assert_eq!(
            (large_q - 1) * small_n,
            (small_q - 1) * large_n,
            "the Z0 quad cost is not linear in regions: {costs:?}"
        );
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
