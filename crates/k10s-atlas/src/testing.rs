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

const NAMESPACES: [&str; 6] = [
    "platform-observability",
    "team-checkout",
    "ingress-system",
    "data-pipelines",
    "kube-system",
    "team-identity",
];

const WORKLOADS: [&str; 8] = [
    "checkout-api",
    "payments-worker",
    "search-indexer",
    "otel-collector",
    "postgres-primary",
    "session-cache",
    "notification-relay",
    "inventory-sync",
];

const SAT_KINDS: [&str; 4] = ["svc", "pvc", "cm", "secret"];

fn tag(n: usize, width: usize) -> String {
    const DIGITS: [u8; 36] = *b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut x = (n as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    (0..width)
        .map(|_| {
            let d = DIGITS[(x % 36) as usize] as char;
            x /= 36;
            d
        })
        .collect()
}

/// The policy every bench and oracle test culls with.
///
/// Every threshold and budget here is the shipping one from `k10s-map`'s `lod`.
/// A bench that measured a policy the product does not use would be measuring
/// nothing, so when a shipping threshold moves this moves with it -- and if one
/// is ever deliberately different, this is where that has to be said.
pub fn lod_policy() -> LodPolicy {
    LodPolicy {
        stage_block: 0.09,
        stage_cell: 0.55,
        stage_cell_label: 3.0,
        block_min_px: 4.0,
        block_icon_min_px: 4.0,
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
        max_cells_per_block: 1024,
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

            let wl = format!("{}-{b}", WORKLOADS[(r + b) % WORKLOADS.len()]);
            let rs = tag(blocks.len(), 9);

            let cell_base = cells.len() as u32;
            for c in 0..spec.cells_per_block {
                let cx = inner.x + BLOCK_PAD + (c % cell_cols) as f32 * CELL_PITCH;
                let cy = inner.y + BLOCK_HEADER + (c / cell_cols) as f32 * CELL_PITCH;
                cells.push(CellNode {
                    rect: Rect::new(cx, cy, CELL_SIZE, CELL_SIZE),
                    label: Arc::from(format!("{wl}-{rs}-{}", tag(c, 5)).as_str()),
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
                    label: Arc::from(
                        format!("{}/{wl}-{s}", SAT_KINDS[s % SAT_KINDS.len()]).as_str(),
                    ),
                    ext: (),
                });
            }

            blocks.push(BlockNode {
                rect: halo,
                inner,
                label: Arc::from(wl.as_str()),
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
                edges.push(Edge::blocks(a, b));
            }
        }
        region_edges.push(edge_start..edges.len() as u32);

        regions.push(RegionNode {
            rect: Rect::new(rx, ry, region_w, region_h),
            label: Arc::from(format!("{}-{r}", NAMESPACES[r % NAMESPACES.len()]).as_str()),
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

    let mut scene = Scene {
        rev: 1,
        bounds,
        // The bench fixtures are the Spread layout, which is the shipping one.
        card_header: 26.0,
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
        region_blocks: vec![],
        block_cells: vec![],
        block_sats: vec![],
        spatial_index: Default::default(),
        edges,
        edge_segments: vec![],
        region_edges,
        region_edge_indexes: vec![],
        cross_edges: edge_count..edge_count,
        cross_edge_index: Default::default(),
    };
    scene.rebuild_spatial_index();
    scene.rebuild_edge_indexes();
    scene
}

pub fn cross_scene(spec: SceneSpec, count: usize) -> Scene {
    let mut s = scene(spec);
    if spec.regions < 2 || spec.blocks_per_region == 0 {
        return s;
    }
    let start = s.edges.len() as u32;
    for e in 0..count {
        let from = e % (spec.regions - 1);
        s.edges.push(Edge::blocks(
            s.regions[from].children.start,
            s.regions[from + 1].children.start,
        ));
    }
    s.cross_edges = start..s.edges.len() as u32;
    s.totals.edges = s.edges.len() as u32;
    s.rebuild_cross_edge_index();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const INLINE_CAP: usize = 23;

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
            assert!((e.a.index() as usize) < s.blocks.len());
            assert!((e.b.index() as usize) < s.blocks.len());
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
                let sats = &s.sats[block.sats.start as usize..block.sats.end as usize];
                for sat in sats {
                    assert!(
                        block.rect.contains(&sat.rect),
                        "satellite {:?} escapes the halo {:?} that the cull takes as its \
                         bound when the block is wholly on screen",
                        sat.rect,
                        block.rect
                    );
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
    fn cell_labels_are_too_long_to_live_inline() {
        let s = scene(SceneSpec::uniform(3, 4));
        for c in &s.cells {
            assert!(
                c.label.len() > INLINE_CAP,
                "{:?} is {} bytes, inside the inline capacity an allocation counter cannot see",
                c.label,
                c.label.len(),
            );
        }
        assert!(
            s.cells
                .iter()
                .any(|c| s.blocks.iter().any(|b| c.label.starts_with(&*b.label))),
            "a cell name must hang off its block's, the way a pod name hangs off its workload's"
        );
    }

    #[test]
    fn labels_stay_unique_within_their_level() {
        let s = scene(SceneSpec {
            regions: 4,
            blocks_per_region: 6,
            cells_per_block: 64,
            sats_per_block: 3,
            edges_per_region: 0,
        });
        for labels in [
            s.regions.iter().map(|n| &n.label).collect::<Vec<_>>(),
            s.cells.iter().map(|n| &n.label).collect(),
            s.sats.iter().map(|n| &n.label).collect(),
        ] {
            let mut sorted = labels.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), labels.len(), "duplicate label");
        }
    }

    #[test]
    fn cross_edges_land_in_the_tail_and_leave_their_region() {
        let spec = SceneSpec::uniform(4, 3);
        let s = cross_scene(spec, 6);
        assert_eq!(s.cross_edges.len(), 6);
        assert_eq!(s.totals.edges as usize, s.edges.len());
        assert_eq!(s.region_edges.len(), s.regions.len());
        for range in &s.region_edges {
            assert!(
                range.end <= s.cross_edges.start,
                "a cross edge was grouped under a region"
            );
        }
        for e in &s.edges[s.cross_edges.start as usize..] {
            let a = &s.blocks[e.a.index() as usize].inner;
            let b = &s.blocks[e.b.index() as usize].inner;
            assert!(
                !s.regions
                    .iter()
                    .any(|r| r.rect.contains(a) && r.rect.contains(b)),
                "a cross edge that stays inside one region is groupable, and this one is grouped"
            );
        }
    }

    #[test]
    fn walk_edges_scans_the_cross_tail_with_no_region_visible() {
        let spec = SceneSpec {
            regions: 2,
            blocks_per_region: 4,
            cells_per_block: 5,
            sats_per_block: 2,
            edges_per_region: 0,
        };
        let s = cross_scene(spec, 1);
        let (_, ay) = s.blocks[s.regions[0].children.start as usize]
            .inner
            .center();
        let visible = Rect::new(s.regions[0].rect.max_x() + 10.0, ay - 5.0, 100.0, 10.0);
        assert!(
            !s.regions.iter().any(|r| r.rect.intersects(&visible)),
            "the window must miss every region for this to prove anything"
        );
        assert_eq!(crate::cull::walk_edges(&s, &visible, 100, |_, _| {}), 1);
    }

    #[test]
    fn fan_out_concentrates_blocks_in_one_region() {
        let s = scene(SceneSpec::fan_out(2000));
        assert_eq!(s.regions.len(), 1);
        assert_eq!(s.blocks.len(), 2000);
        assert_eq!(s.regions[0].children, 0..2000);
    }
}
