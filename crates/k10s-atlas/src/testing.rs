use std::sync::Arc;

use crate::lod::LodPolicy;
use crate::scene::{BlockNode, CellNode, Edge, Rect, RegionNode, Scene, Totals};

const CELL_PITCH: f32 = 14.0;
const CELL_SIZE: f32 = 10.0;
const BLOCK_PAD: f32 = 10.0;
const BLOCK_HEADER: f32 = 16.0;
const BLOCK_GAP: f32 = 26.0;
const BLOCK_HALO: f32 = 66.0;
const SAT_SIZE: f32 = 18.0;
const SAT_RING_GAP: f32 = 30.0;
const REGION_PAD: f32 = 36.0;
const REGION_HEADER: f32 = 44.0;
const REGION_GAP: f32 = 120.0;

#[derive(Debug, Clone, Copy)]
pub struct SceneSpec {
    pub regions: usize,
    pub blocks_per_region: usize,
    pub cells_per_block: usize,
    pub sats_per_block: usize,
    pub edges_per_region: usize,
}

impl SceneSpec {
    pub fn uniform(regions: usize, blocks_per_region: usize) -> Self {
        SceneSpec {
            regions,
            blocks_per_region,
            cells_per_block: 5,
            sats_per_block: 2,
            edges_per_region: 4,
        }
    }

    pub fn fan_out(blocks_in_one_region: usize) -> Self {
        SceneSpec::uniform(1, blocks_in_one_region)
    }

    pub fn total_blocks(&self) -> usize {
        self.regions * self.blocks_per_region
    }

    pub fn total_cells(&self) -> usize {
        self.total_blocks() * self.cells_per_block
    }

    pub fn total_sats(&self) -> usize {
        self.total_blocks() * self.sats_per_block
    }

    pub fn total_objects(&self) -> usize {
        self.regions + self.total_blocks() + self.total_cells() + self.total_sats()
    }
}

fn grid_cols(n: usize) -> usize {
    (n as f64).sqrt().ceil().max(1.0) as usize
}

pub fn lod_policy() -> LodPolicy {
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
        max_icons: 1024,
        max_edges: 3000,
        max_curves: 1500,
        sat_curves: true,
        stress: false,
        stress_curves: false,
    }
}

pub fn scene(spec: SceneSpec) -> Scene {
    let cell_cols = grid_cols(spec.cells_per_block);
    let cell_rows = spec.cells_per_block.div_ceil(cell_cols).max(1);
    let inner_w = cell_cols as f32 * CELL_PITCH + BLOCK_PAD * 2.0;
    let inner_h = cell_rows as f32 * CELL_PITCH + BLOCK_PAD + BLOCK_HEADER;
    let block_w = inner_w + BLOCK_HALO * 2.0;
    let block_h = inner_h + BLOCK_HALO * 2.0;

    let block_cols = grid_cols(spec.blocks_per_region);
    let block_rows = spec.blocks_per_region.div_ceil(block_cols).max(1);
    let region_w = block_cols as f32 * (block_w + BLOCK_GAP) + REGION_PAD * 2.0;
    let region_h =
        block_rows as f32 * (block_h + BLOCK_GAP) + REGION_PAD + REGION_HEADER + BLOCK_PAD;

    let region_cols = grid_cols(spec.regions);

    let mut regions = Vec::with_capacity(spec.regions);
    let mut blocks = Vec::with_capacity(spec.total_blocks());
    let mut cells = Vec::with_capacity(spec.total_cells());
    let mut sats = Vec::with_capacity(spec.total_sats());
    let mut edges = Vec::new();
    let mut region_edges = Vec::with_capacity(spec.regions);

    for r in 0..spec.regions {
        let rx = (r % region_cols) as f32 * (region_w + REGION_GAP);
        let ry = (r / region_cols) as f32 * (region_h + REGION_GAP);
        let block_base = blocks.len() as u32;

        for b in 0..spec.blocks_per_region {
            let bx = rx + REGION_PAD + (b % block_cols) as f32 * (block_w + BLOCK_GAP);
            let by =
                ry + REGION_PAD + REGION_HEADER + (b / block_cols) as f32 * (block_h + BLOCK_GAP);
            let halo = Rect::new(bx, by, block_w, block_h);
            let inner = Rect::new(bx + BLOCK_HALO, by + BLOCK_HALO, inner_w, inner_h);

            let cell_base = cells.len() as u32;
            for c in 0..spec.cells_per_block {
                let cx = inner.x + BLOCK_PAD + (c % cell_cols) as f32 * CELL_PITCH;
                let cy = inner.y + BLOCK_HEADER + (c / cell_cols) as f32 * CELL_PITCH;
                cells.push(CellNode {
                    rect: Rect::new(cx, cy, CELL_SIZE, CELL_SIZE),
                    label: Arc::from(format!("pod-{r}-{b}-{c}").as_str()),
                    ext: (),
                });
            }

            let sat_base = sats.len() as u32;
            let (icx, icy) = inner.center();
            let radius = inner_w.max(inner_h) * 0.5 + SAT_RING_GAP;
            for s in 0..spec.sats_per_block {
                let angle = s as f32 / spec.sats_per_block.max(1) as f32 * std::f32::consts::TAU;
                sats.push(CellNode {
                    rect: Rect::new(
                        icx + radius * angle.cos() - SAT_SIZE * 0.5,
                        icy + radius * angle.sin() - SAT_SIZE * 0.5,
                        SAT_SIZE,
                        SAT_SIZE,
                    ),
                    label: Arc::from(format!("svc-{r}-{b}-{s}").as_str()),
                    ext: (),
                });
            }

            blocks.push(BlockNode {
                rect: halo,
                inner,
                label: Arc::from(format!("workload-{r}-{b}").as_str()),
                children: cell_base..cells.len() as u32,
                sats: sat_base..sats.len() as u32,
                ext: (),
            });
        }

        let edge_start = edges.len() as u32;
        let span = spec.blocks_per_region;
        if span >= 2 {
            for e in 0..spec.edges_per_region {
                let a = block_base + (e % span) as u32;
                let b = block_base + ((e + 1) % span) as u32;
                edges.push(Edge { a, b });
            }
        }
        region_edges.push(edge_start..edges.len() as u32);

        regions.push(RegionNode {
            rect: Rect::new(rx, ry, region_w, region_h),
            label: Arc::from(format!("namespace-{r}").as_str()),
            weight: spec.blocks_per_region as u32,
            children: block_base..blocks.len() as u32,
            ext: (),
        });
    }

    let region_rows = spec.regions.div_ceil(region_cols).max(1);
    let bounds = Rect::new(
        0.0,
        0.0,
        region_cols as f32 * (region_w + REGION_GAP),
        region_rows as f32 * (region_h + REGION_GAP),
    );
    let edge_count = edges.len() as u32;

    Scene {
        rev: 1,
        bounds,
        totals: Totals {
            regions: regions.len() as u32,
            blocks: blocks.len() as u32,
            cells: cells.len() as u32,
            sats: sats.len() as u32,
            edges: edge_count,
        },
        regions,
        blocks,
        cells,
        sats,
        edges,
        region_edges,
        cross_edges: edge_count..edge_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_totals_match_built_scene() {
        let spec = SceneSpec::uniform(9, 4);
        let s = scene(spec);
        assert_eq!(s.regions.len(), spec.regions);
        assert_eq!(s.blocks.len(), spec.total_blocks());
        assert_eq!(s.cells.len(), spec.total_cells());
        assert_eq!(s.sats.len(), spec.total_sats());
        assert_eq!(s.totals.regions, spec.regions as u32);
        assert_eq!(s.totals.cells, spec.total_cells() as u32);
    }

    #[test]
    fn child_ranges_partition_their_arrays() {
        let s = scene(SceneSpec::uniform(6, 3));
        assert_eq!(s.regions[0].children.start, 0);
        assert_eq!(
            s.regions.last().unwrap().children.end,
            s.blocks.len() as u32
        );
        for w in s.regions.windows(2) {
            assert_eq!(w[0].children.end, w[1].children.start);
        }
        for w in s.blocks.windows(2) {
            assert_eq!(w[0].children.end, w[1].children.start);
            assert_eq!(w[0].sats.end, w[1].sats.start);
        }
        assert_eq!(s.blocks.last().unwrap().children.end, s.cells.len() as u32);
        assert_eq!(s.blocks.last().unwrap().sats.end, s.sats.len() as u32);
    }

    #[test]
    fn region_edges_align_with_regions_and_cross_edges_stay_empty() {
        let s = scene(SceneSpec::uniform(5, 4));
        assert_eq!(s.region_edges.len(), s.regions.len());
        assert!(s.cross_edges.is_empty());
        for range in &s.region_edges {
            assert!(range.end as usize <= s.edges.len());
        }
        for e in &s.edges {
            assert!((e.a as usize) < s.blocks.len());
            assert!((e.b as usize) < s.blocks.len());
        }
    }

    #[test]
    fn nodes_stay_inside_their_parents() {
        let s = scene(SceneSpec::uniform(4, 4));
        for region in &s.regions {
            let blocks = &s.blocks[region.children.start as usize..region.children.end as usize];
            for block in blocks {
                assert!(
                    region.rect.contains(&block.rect),
                    "block {:?} escapes region {:?}",
                    block.rect,
                    region.rect
                );
                let cells = &s.cells[block.children.start as usize..block.children.end as usize];
                for cell in cells {
                    assert!(block.inner.contains(&cell.rect));
                }
            }
        }
    }

    #[test]
    fn scene_is_deterministic() {
        let a = scene(SceneSpec::uniform(7, 5));
        let b = scene(SceneSpec::uniform(7, 5));
        assert_eq!(a.regions.len(), b.regions.len());
        for (x, y) in a.regions.iter().zip(&b.regions) {
            assert_eq!(x.rect, y.rect);
            assert_eq!(x.label, y.label);
        }
        for (x, y) in a.cells.iter().zip(&b.cells) {
            assert_eq!(x.rect, y.rect);
        }
    }

    #[test]
    fn fan_out_concentrates_blocks_in_one_region() {
        let s = scene(SceneSpec::fan_out(2000));
        assert_eq!(s.regions.len(), 1);
        assert_eq!(s.blocks.len(), 2000);
        assert_eq!(s.regions[0].children, 0..2000);
    }
}
