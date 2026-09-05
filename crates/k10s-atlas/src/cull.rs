use crate::camera::Camera;
use crate::lod::{LodPolicy, StageBlend, WorkloadPresentation};
use crate::scene::{BlockNode, Rect, Scene};

/// Regions a Z0 painter will scan without aggregation. The fit-camera corpus
/// tops out at 1600, which still fits. Two of those on one Starmap would not,
/// so a window still shows one cluster: launch replaces the connection, it
/// does not merge contexts into one scene.
pub const MAX_Z0_REGIONS: usize = 2_048;
const DIRECT_REGION_SCAN_LIMIT: usize = MAX_Z0_REGIONS;
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

#[inline]
fn count_workload_mark(
    st: &mut CullStats,
    policy: &LodPolicy,
    inner_w: f32,
    zoom: f32,
    stage: u8,
    skip_blocks: bool,
) -> WorkloadPresentation {
    let presentation = if skip_blocks {
        WorkloadPresentation::Hidden
    } else {
        policy.workload_presentation(inner_w, zoom, stage)
    };
    if presentation == WorkloadPresentation::Hidden {
        return presentation;
    }

    st.drawn_blocks += 1;
    if presentation.card_shown() {
        st.quads += 1;
        if presentation == WorkloadPresentation::Detailed
            && policy.block_chrome_shown(inner_w, zoom)
        {
            st.quads += 2;
        }
    }
    if matches!(
        presentation,
        WorkloadPresentation::Medallion | WorkloadPresentation::Detailed
    ) && policy.block_icon_shown(inner_w, zoom)
    {
        push_icon(st, policy);
    }
    if presentation.card_shown() && policy.block_label_shown(inner_w, zoom) {
        push_label(st, policy);
    }
    presentation
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
            count_workload_mark(&mut st, policy, block.inner.w, zoom, 1, skip_blocks);
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
            count_workload_mark(&mut st, policy, block.inner.w, zoom, 1, skip_blocks);
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

            let presentation =
                count_workload_mark(&mut st, policy, block.inner.w, zoom, stage, skip_blocks);
            if stage < 2 {
                return;
            }

            let block_inside = region_inside || visible.contains(&block.rect);
            if presentation != WorkloadPresentation::Hidden || policy.stress_curves {
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
            if !presentation.cells_shown() {
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

    let presentation = count_workload_mark(st, policy, block.inner.w, zoom, stage, skip_blocks);

    if stage < 2 {
        return;
    }
    let block_inside = region_inside || visible.contains(&block.rect);

    if presentation != WorkloadPresentation::Hidden || policy.stress_curves {
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

    if !presentation.cells_shown() {
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
