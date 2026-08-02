use std::ops::Range;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn max_x(&self) -> f32 {
        self.x + self.w
    }

    pub fn max_y(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.max_x() && o.x < self.max_x() && self.y < o.max_y() && o.y < self.max_y()
    }

    pub fn contains(&self, o: &Rect) -> bool {
        self.x <= o.x && self.y <= o.y && o.max_x() <= self.max_x() && o.max_y() <= self.max_y()
    }

    pub fn intersection_fraction(&self, other: &Rect) -> f32 {
        if self.w <= 0.0 || self.h <= 0.0 {
            return 0.0;
        }
        let width = self.max_x().min(other.max_x()) - self.x.max(other.x);
        let height = self.max_y().min(other.max_y()) - self.y.max(other.y);
        width.max(0.0) * height.max(0.0) / (self.w * self.h)
    }

    pub fn inflate(&self, m: f32) -> Rect {
        Rect::new(self.x - m, self.y - m, self.w + 2.0 * m, self.h + 2.0 * m)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegionNode<X = ()> {
    pub rect: Rect,
    pub label: Arc<str>,
    pub weight: u32,
    pub children: Range<u32>,
    pub ext: X,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockNode<X = ()> {
    pub rect: Rect,

    pub inner: Rect,
    pub label: Arc<str>,
    pub children: Range<u32>,
    pub sats: Range<u32>,
    pub ext: X,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellNode<X = ()> {
    pub rect: Rect,
    pub label: Arc<str>,
    pub ext: X,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Level {
    Region = 0,
    Block = 1,
    Cell = 2,
    Sat = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Endpoint(u32);

impl Endpoint {
    pub const MAX_INDEX: u32 = (1 << 30) - 1;

    pub const fn new(level: Level, idx: u32) -> Endpoint {
        debug_assert!(idx <= Endpoint::MAX_INDEX);
        Endpoint(((level as u32) << 30) | (idx & Endpoint::MAX_INDEX))
    }

    pub const fn block(idx: u32) -> Endpoint {
        Endpoint::new(Level::Block, idx)
    }

    pub const fn region(idx: u32) -> Endpoint {
        Endpoint::new(Level::Region, idx)
    }

    pub const fn cell(idx: u32) -> Endpoint {
        Endpoint::new(Level::Cell, idx)
    }

    pub const fn sat(idx: u32) -> Endpoint {
        Endpoint::new(Level::Sat, idx)
    }

    pub const fn level(self) -> Level {
        match self.0 >> 30 {
            0 => Level::Region,
            1 => Level::Block,
            2 => Level::Cell,
            _ => Level::Sat,
        }
    }

    pub const fn index(self) -> u32 {
        self.0 & Endpoint::MAX_INDEX
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Edge {
    pub a: Endpoint,
    pub b: Endpoint,
}

impl Edge {
    pub const fn blocks(a: u32, b: u32) -> Edge {
        Edge {
            a: Endpoint::block(a),
            b: Endpoint::block(b),
        }
    }

    #[inline(always)]
    pub const fn is_block_pair(&self) -> bool {
        const BLOCK: u32 = (Level::Block as u32) << 30;
        (self.a.0 & !Endpoint::MAX_INDEX) == BLOCK && (self.b.0 & !Endpoint::MAX_INDEX) == BLOCK
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeSegment {
    pub a: (f32, f32),
    pub b: (f32, f32),
}

impl EdgeSegment {
    pub fn bounds(self) -> Rect {
        Rect::new(
            self.a.0.min(self.b.0),
            self.a.1.min(self.b.1),
            (self.a.0 - self.b.0).abs().max(1.0),
            (self.a.1 - self.b.1).abs().max(1.0),
        )
    }
}

const EDGE_INDEX_LEAF_LEN: usize = 16;
const EDGE_INDEX_CANDIDATE_LIMIT: usize = EDGE_INDEX_LEAF_LEN * 2;
const EDGE_INDEX_MIN_LEN: usize = EDGE_INDEX_LEAF_LEN * 4;
const EDGE_INDEX_NODE_LIMIT: usize = 32;
const NO_EDGE_INDEX_NODE: u32 = u32::MAX;
const NO_SPATIAL_NODE: u32 = u32::MAX;
const REGION_SPATIAL_INDEX_MIN_LEN: usize = 64;
const BLOCK_SPATIAL_INDEX_MIN_LEN: usize = 128;
const CELL_SPATIAL_INDEX_MIN_LEN: usize = 128;
const LARGE_SCENE_CELL_INDEX_MIN_LEN: usize = 8_192;
const SMALL_SCENE_BLOCK_LIMIT: usize = 64;
const SPATIAL_LEAF_LEN: usize = 8;

#[derive(Debug, Clone, Copy)]
struct EdgeIndexNode {
    bounds: Rect,
    start: u32,
    end: u32,
    left: u32,
    right: u32,
}

impl Default for EdgeIndexNode {
    fn default() -> Self {
        EdgeIndexNode {
            bounds: Rect::ZERO,
            start: 0,
            end: 0,
            left: NO_EDGE_INDEX_NODE,
            right: NO_EDGE_INDEX_NODE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EdgeIndex {
    range: Range<u32>,
    nodes: Vec<EdgeIndexNode>,
    root: u32,
}

impl Default for EdgeIndex {
    fn default() -> Self {
        EdgeIndex {
            range: 0..0,
            nodes: Vec::new(),
            root: NO_EDGE_INDEX_NODE,
        }
    }
}

impl EdgeIndex {
    fn build<R, B, C, S>(scene: &Scene<R, B, C, S>, range: Range<u32>) -> Self {
        if range.len() <= EDGE_INDEX_MIN_LEN || range.end as usize > scene.edges.len() {
            return EdgeIndex::default();
        }

        let mut nodes =
            Vec::with_capacity((range.len().div_ceil(EDGE_INDEX_LEAF_LEN) * 2).saturating_sub(1));
        let root = build_edge_index_node(scene, range.start, range.end, &mut nodes)
            .unwrap_or(NO_EDGE_INDEX_NODE);
        EdgeIndex { range, nodes, root }
    }

    pub(crate) fn covers(&self, range: &Range<u32>) -> bool {
        self.root != NO_EDGE_INDEX_NODE && self.range == *range
    }

    pub(crate) fn is_selective(&self, visible: &Rect) -> bool {
        if self.root == NO_EDGE_INDEX_NODE {
            return false;
        }
        let mut candidates = 0usize;
        let mut visited = 0usize;
        self.count_candidates(
            self.root,
            visible,
            EDGE_INDEX_CANDIDATE_LIMIT,
            &mut candidates,
            &mut visited,
        )
    }

    pub(crate) fn for_each_candidate(
        &self,
        visible: &Rect,
        mut emit: impl FnMut(Range<usize>) -> bool,
    ) -> bool {
        if self.root == NO_EDGE_INDEX_NODE {
            return true;
        }
        self.visit(self.root, visible, &mut emit)
    }

    fn count_candidates(
        &self,
        node_index: u32,
        visible: &Rect,
        limit: usize,
        candidates: &mut usize,
        visited: &mut usize,
    ) -> bool {
        *visited += 1;
        if *visited > EDGE_INDEX_NODE_LIMIT {
            return false;
        }
        let node = &self.nodes[node_index as usize];
        if !node.bounds.intersects(visible) {
            return true;
        }
        if node.left == NO_EDGE_INDEX_NODE && node.right == NO_EDGE_INDEX_NODE {
            *candidates += (node.end - node.start) as usize;
            return *candidates < limit;
        }
        if node.left != NO_EDGE_INDEX_NODE
            && !self.count_candidates(node.left, visible, limit, candidates, visited)
        {
            return false;
        }
        node.right == NO_EDGE_INDEX_NODE
            || self.count_candidates(node.right, visible, limit, candidates, visited)
    }

    fn visit(
        &self,
        node_index: u32,
        visible: &Rect,
        emit: &mut impl FnMut(Range<usize>) -> bool,
    ) -> bool {
        let node = &self.nodes[node_index as usize];
        if !node.bounds.intersects(visible) {
            return true;
        }
        if node.left == NO_EDGE_INDEX_NODE && node.right == NO_EDGE_INDEX_NODE {
            return emit(node.start as usize..node.end as usize);
        }
        if node.left != NO_EDGE_INDEX_NODE && !self.visit(node.left, visible, emit) {
            return false;
        }
        node.right == NO_EDGE_INDEX_NODE || self.visit(node.right, visible, emit)
    }
}

fn build_edge_index_node<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    start: u32,
    end: u32,
    nodes: &mut Vec<EdgeIndexNode>,
) -> Option<u32> {
    let slot = nodes.len() as u32;
    nodes.push(EdgeIndexNode::default());

    if end - start <= EDGE_INDEX_LEAF_LEN as u32 {
        let bounds = (start..end)
            .filter_map(|i| scene.edge_segments[i as usize].map(EdgeSegment::bounds))
            .reduce(rect_union)?;
        nodes[slot as usize] = EdgeIndexNode {
            bounds,
            start,
            end,
            left: NO_EDGE_INDEX_NODE,
            right: NO_EDGE_INDEX_NODE,
        };
        return Some(slot);
    }

    let mid = start + (end - start) / 2;
    let left = build_edge_index_node(scene, start, mid, nodes);
    let right = build_edge_index_node(scene, mid, end, nodes);
    let bounds = match (left, right) {
        (Some(left), Some(right)) => {
            rect_union(nodes[left as usize].bounds, nodes[right as usize].bounds)
        }
        (Some(left), None) => nodes[left as usize].bounds,
        (None, Some(right)) => nodes[right as usize].bounds,
        (None, None) => return None,
    };
    nodes[slot as usize] = EdgeIndexNode {
        bounds,
        start,
        end,
        left: left.unwrap_or(NO_EDGE_INDEX_NODE),
        right: right.unwrap_or(NO_EDGE_INDEX_NODE),
    };
    Some(slot)
}

fn rect_union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect::new(
        x,
        y,
        a.max_x().max(b.max_x()) - x,
        a.max_y().max(b.max_y()) - y,
    )
}

#[cfg(test)]
pub(crate) fn edge_bounds<R, B, C, S>(scene: &Scene<R, B, C, S>, edge: Edge) -> Option<Rect> {
    resolve_edge(scene, edge).map(EdgeSegment::bounds)
}

pub(crate) fn resolve_edge<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    edge: Edge,
) -> Option<EdgeSegment> {
    if edge.is_block_pair() {
        return Some(EdgeSegment {
            a: scene.blocks.get(edge.a.index() as usize)?.inner.center(),
            b: scene.blocks.get(edge.b.index() as usize)?.inner.center(),
        });
    }
    let endpoint = |endpoint: Endpoint| {
        let i = endpoint.index() as usize;
        match endpoint.level() {
            Level::Region => scene.regions.get(i).map(|node| &node.rect),
            Level::Block => scene.blocks.get(i).map(|node| &node.inner),
            Level::Cell => scene.cells.get(i).map(|node| &node.rect),
            Level::Sat => scene.sats.get(i).map(|node| &node.rect),
        }
    };
    Some(EdgeSegment {
        a: endpoint(edge.a)?.center(),
        b: endpoint(edge.b)?.center(),
    })
}

#[derive(Debug, Clone, Copy)]
struct SpatialNode {
    bounds: Rect,
    start: u32,
    end: u32,
    left: u32,
    right: u32,
}

#[derive(Debug, Clone, Default)]
struct SpatialForest {
    items: Vec<u32>,
    nodes: Vec<SpatialNode>,
    roots: Vec<u32>,
}

impl SpatialForest {
    fn with_roots(roots: usize) -> Self {
        SpatialForest {
            roots: vec![NO_SPATIAL_NODE; roots],
            ..SpatialForest::default()
        }
    }

    fn build_root(
        &mut self,
        root: usize,
        minimum_len: usize,
        indices: impl ExactSizeIterator<Item = u32>,
        rect: impl Fn(u32) -> Rect,
    ) {
        if indices.len() < minimum_len {
            return;
        }
        let indices: Vec<u32> = indices
            .filter(|&index| {
                let rect = rect(index);
                rect.w > 0.0 && rect.h > 0.0
            })
            .collect();
        if indices.len() < minimum_len {
            return;
        }
        let start = self.items.len();
        self.items.extend(indices);
        let end = self.items.len();
        self.roots[root] = build_spatial_node(&mut self.items, &mut self.nodes, start, end, &rect);
    }

    fn is_selective(&self, root: usize, visible: &Rect) -> bool {
        let Some(&root) = self.roots.get(root) else {
            return false;
        };
        root != NO_SPATIAL_NODE && !visible.contains(&self.nodes[root as usize].bounds)
    }

    #[inline]
    fn for_each_candidate(
        &self,
        root: usize,
        visible: &Rect,
        mut visit: impl FnMut(usize),
    ) -> bool {
        let Some(&root) = self.roots.get(root) else {
            return false;
        };
        if root == NO_SPATIAL_NODE || visible.contains(&self.nodes[root as usize].bounds) {
            return false;
        }
        self.visit(root, visible, &mut visit);
        true
    }

    #[inline]
    fn visit(&self, node: u32, visible: &Rect, visit: &mut impl FnMut(usize)) {
        let node = self.nodes[node as usize];
        if !node.bounds.intersects(visible) {
            return;
        }
        if node.left == NO_SPATIAL_NODE {
            for &item in &self.items[node.start as usize..node.end as usize] {
                visit(item as usize);
            }
            return;
        }
        self.visit(node.left, visible, visit);
        self.visit(node.right, visible, visit);
    }

    #[cfg(any(test, feature = "testing"))]
    fn candidate_stats(&self, root: usize, visible: &Rect) -> Option<(usize, usize)> {
        let &root = self.roots.get(root)?;
        if root == NO_SPATIAL_NODE || visible.contains(&self.nodes[root as usize].bounds) {
            return None;
        }
        let mut nodes = 0usize;
        let mut candidates = 0usize;
        self.visit_stats(root, visible, &mut nodes, &mut candidates);
        Some((nodes, candidates))
    }

    #[cfg(any(test, feature = "testing"))]
    fn visit_stats(&self, node: u32, visible: &Rect, nodes: &mut usize, candidates: &mut usize) {
        *nodes += 1;
        let node = self.nodes[node as usize];
        if !node.bounds.intersects(visible) {
            return;
        }
        if node.left == NO_SPATIAL_NODE {
            *candidates += (node.end - node.start) as usize;
            return;
        }
        self.visit_stats(node.left, visible, nodes, candidates);
        self.visit_stats(node.right, visible, nodes, candidates);
    }
}

#[derive(Debug, Clone, Default)]
pub struct SceneIndex {
    regions: SpatialForest,
    blocks: SpatialForest,
    cells: SpatialForest,
    max_blocks_per_region: usize,
}

impl SceneIndex {
    pub fn is_empty(&self) -> bool {
        self.regions.nodes.is_empty() && self.blocks.nodes.is_empty() && self.cells.nodes.is_empty()
    }

    pub(crate) fn max_blocks_per_region(&self) -> usize {
        self.max_blocks_per_region
    }

    pub(crate) fn region_is_selective(&self, visible: &Rect) -> bool {
        self.regions.is_selective(0, visible)
    }
}

fn build_spatial_node(
    items: &mut [u32],
    nodes: &mut Vec<SpatialNode>,
    start: usize,
    end: usize,
    rect: &impl Fn(u32) -> Rect,
) -> u32 {
    let bounds = items[start..end]
        .iter()
        .map(|&index| rect(index))
        .reduce(rect_union)
        .expect("a spatial node is never empty");
    let slot = nodes.len() as u32;
    nodes.push(SpatialNode {
        bounds,
        start: start as u32,
        end: end as u32,
        left: NO_SPATIAL_NODE,
        right: NO_SPATIAL_NODE,
    });
    if end - start <= SPATIAL_LEAF_LEN {
        return slot;
    }

    let split_x = bounds.w >= bounds.h;
    items[start..end].sort_unstable_by(|&a, &b| {
        let (ax, ay) = rect(a).center();
        let (bx, by) = rect(b).center();
        let order = if split_x {
            ax.total_cmp(&bx)
        } else {
            ay.total_cmp(&by)
        };
        order.then_with(|| a.cmp(&b))
    });
    let middle = start + (end - start) / 2;
    let left = build_spatial_node(items, nodes, start, middle, rect);
    let right = build_spatial_node(items, nodes, middle, end, rect);
    nodes[slot as usize].left = left;
    nodes[slot as usize].right = right;
    slot
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub regions: u32,
    pub blocks: u32,
    pub cells: u32,
    pub sats: u32,
    pub edges: u32,
}

#[derive(Debug, Clone)]
pub struct Scene<R = (), B = (), C = (), S = ()> {
    pub rev: u64,
    pub bounds: Rect,
    pub regions: Vec<RegionNode<R>>,
    pub blocks: Vec<BlockNode<B>>,
    pub cells: Vec<CellNode<C>>,
    pub sats: Vec<CellNode<S>>,
    pub region_blocks: Vec<u32>,
    pub block_cells: Vec<u32>,
    pub block_sats: Vec<u32>,
    pub spatial_index: SceneIndex,
    pub edges: Vec<Edge>,
    pub edge_segments: Vec<Option<EdgeSegment>>,

    pub region_edges: Vec<Range<u32>>,
    pub region_edge_indexes: Vec<EdgeIndex>,
    pub cross_edges: Range<u32>,
    pub cross_edge_index: EdgeIndex,
    pub totals: Totals,
}

impl<R, B, C, S> Scene<R, B, C, S> {
    pub fn child_ranges_are_direct(&self) -> bool {
        self.region_blocks.is_empty() && self.block_cells.is_empty() && self.block_sats.is_empty()
    }

    pub fn region_index_is_selective(&self, visible: &Rect) -> bool {
        self.spatial_index.region_is_selective(visible)
    }

    pub fn region_block_index_is_selective(&self, region: usize, visible: &Rect) -> bool {
        self.spatial_index.blocks.is_selective(region, visible)
    }

    pub fn block_cell_index_is_selective(&self, block: usize, visible: &Rect) -> bool {
        self.spatial_index.cells.is_selective(block, visible)
    }

    pub(crate) fn visible_region_has_selective_block_index(&self, visible: &Rect) -> bool {
        if self.spatial_index.blocks.nodes.is_empty() {
            return false;
        }
        let mut selective = false;
        self.for_each_region_candidate(visible, |region_index, region| {
            selective |= region.rect.intersects(visible)
                && self
                    .spatial_index
                    .blocks
                    .is_selective(region_index, visible);
        });
        selective
    }

    pub fn rebuild_spatial_index(&mut self) {
        let mut regions = SpatialForest::with_roots(1);
        regions.build_root(
            0,
            REGION_SPATIAL_INDEX_MIN_LEN,
            0..self.regions.len() as u32,
            |index| self.regions[index as usize].rect,
        );

        let mut blocks = SpatialForest::with_roots(self.regions.len());
        for region in 0..self.regions.len() {
            blocks.build_root(
                region,
                BLOCK_SPATIAL_INDEX_MIN_LEN,
                self.region_block_indices(region).map(|index| index as u32),
                |index| self.blocks[index as usize].rect,
            );
        }

        let cell_index_min_len = if self.blocks.len() <= SMALL_SCENE_BLOCK_LIMIT {
            CELL_SPATIAL_INDEX_MIN_LEN
        } else {
            LARGE_SCENE_CELL_INDEX_MIN_LEN
        };
        let mut cells = SpatialForest::with_roots(self.blocks.len());
        for block in 0..self.blocks.len() {
            cells.build_root(
                block,
                cell_index_min_len,
                self.block_cell_indices(block).map(|index| index as u32),
                |index| self.cells[index as usize].rect,
            );
        }
        self.spatial_index = SceneIndex {
            regions,
            blocks,
            cells,
            max_blocks_per_region: self
                .regions
                .iter()
                .map(|region| region.children.len())
                .max()
                .unwrap_or(0),
        };
    }

    #[inline]
    pub fn for_each_region_candidate(
        &self,
        visible: &Rect,
        visit: impl FnMut(usize, &RegionNode<R>),
    ) {
        self.for_each_region_candidate_mode::<true>(visible, visit);
    }

    #[inline]
    pub(crate) fn for_each_region_candidate_mode<const INDEXED: bool>(
        &self,
        visible: &Rect,
        mut visit: impl FnMut(usize, &RegionNode<R>),
    ) {
        if INDEXED
            && self
                .spatial_index
                .regions
                .for_each_candidate(0, visible, |index| visit(index, &self.regions[index]))
        {
            return;
        }
        for (index, region) in self.regions.iter().enumerate() {
            visit(index, region);
        }
    }

    #[inline]
    pub fn for_each_region_block_candidate(
        &self,
        region: usize,
        visible: &Rect,
        visit: impl FnMut(usize, &BlockNode<B>),
    ) {
        self.for_each_region_block_candidate_mode::<true>(region, visible, visit);
    }

    #[inline]
    pub(crate) fn for_each_region_block_candidate_mode<const INDEXED: bool>(
        &self,
        region: usize,
        visible: &Rect,
        mut visit: impl FnMut(usize, &BlockNode<B>),
    ) {
        if INDEXED
            && self
                .spatial_index
                .blocks
                .for_each_candidate(region, visible, |index| visit(index, &self.blocks[index]))
        {
            return;
        }
        self.for_each_region_block(region, visit);
    }

    #[inline]
    pub fn for_each_block_cell_candidate(
        &self,
        block: usize,
        visible: &Rect,
        visit: impl FnMut(usize, &CellNode<C>),
    ) {
        self.for_each_block_cell_candidate_mode::<true>(block, visible, visit);
    }

    #[inline]
    pub(crate) fn for_each_block_cell_candidate_mode<const INDEXED: bool>(
        &self,
        block: usize,
        visible: &Rect,
        mut visit: impl FnMut(usize, &CellNode<C>),
    ) {
        if INDEXED
            && self
                .spatial_index
                .cells
                .for_each_candidate(block, visible, |index| visit(index, &self.cells[index]))
        {
            return;
        }
        self.for_each_block_cell(block, visit);
    }

    pub fn region_block_indices(&self, region: usize) -> ChildIndices<'_> {
        let range = self
            .regions
            .get(region)
            .map(|node| node.children.clone())
            .unwrap_or(0..0);
        ChildIndices::new(range, &self.region_blocks)
    }

    pub fn block_cell_indices(&self, block: usize) -> ChildIndices<'_> {
        let range = self
            .blocks
            .get(block)
            .map(|node| node.children.clone())
            .unwrap_or(0..0);
        ChildIndices::new(range, &self.block_cells)
    }

    pub fn block_sat_indices(&self, block: usize) -> ChildIndices<'_> {
        let range = self
            .blocks
            .get(block)
            .map(|node| node.sats.clone())
            .unwrap_or(0..0);
        ChildIndices::new(range, &self.block_sats)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn indexed_region_candidate_stats(&self, visible: &Rect) -> Option<(usize, usize)> {
        self.spatial_index.regions.candidate_stats(0, visible)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn indexed_block_cell_candidate_stats(
        &self,
        block: usize,
        visible: &Rect,
    ) -> Option<(usize, usize)> {
        self.spatial_index.cells.candidate_stats(block, visible)
    }

    #[inline]
    pub fn for_each_region_block(
        &self,
        region: usize,
        mut visit: impl FnMut(usize, &BlockNode<B>),
    ) {
        let Some(node) = self.regions.get(region) else {
            return;
        };
        for_each_child(
            &node.children,
            &self.region_blocks,
            &self.blocks,
            &mut visit,
        );
    }

    #[inline]
    pub fn for_each_block_cell(&self, block: usize, mut visit: impl FnMut(usize, &CellNode<C>)) {
        let Some(node) = self.blocks.get(block) else {
            return;
        };
        for_each_child(&node.children, &self.block_cells, &self.cells, &mut visit);
    }

    #[inline]
    pub fn for_each_block_sat(&self, block: usize, mut visit: impl FnMut(usize, &CellNode<S>)) {
        let Some(node) = self.blocks.get(block) else {
            return;
        };
        for_each_child(&node.sats, &self.block_sats, &self.sats, &mut visit);
    }

    pub fn rebuild_edge_indexes(&mut self) {
        self.rebuild_edge_segments();
        self.region_edge_indexes = self
            .region_edges
            .iter()
            .cloned()
            .map(|range| EdgeIndex::build(self, range))
            .collect();
        self.cross_edge_index = EdgeIndex::build(self, self.cross_edges.clone());
    }

    pub fn rebuild_cross_edge_index(&mut self) {
        self.rebuild_edge_segments();
        self.cross_edge_index = EdgeIndex::build(self, self.cross_edges.clone());
    }

    fn rebuild_edge_segments(&mut self) {
        self.edge_segments = self
            .edges
            .iter()
            .copied()
            .map(|edge| resolve_edge(self, edge))
            .collect();
    }
}

#[inline]
fn for_each_child<'a, T>(
    range: &Range<u32>,
    adjacency: &[u32],
    nodes: &'a [T],
    visit: &mut impl FnMut(usize, &'a T),
) {
    if adjacency.is_empty() {
        let start = range.start as usize;
        let Some(children) = nodes.get(start..range.end as usize) else {
            return;
        };
        for (offset, node) in children.iter().enumerate() {
            visit(start + offset, node);
        }
        return;
    }
    let Some(indices) = adjacency.get(range.start as usize..range.end as usize) else {
        return;
    };
    for &index in indices {
        let index = index as usize;
        if let Some(node) = nodes.get(index) {
            visit(index, node);
        }
    }
}

impl<R, B, C, S> Default for Scene<R, B, C, S> {
    fn default() -> Self {
        Scene {
            rev: 0,
            bounds: Rect::ZERO,
            regions: Vec::new(),
            blocks: Vec::new(),
            cells: Vec::new(),
            sats: Vec::new(),
            region_blocks: Vec::new(),
            block_cells: Vec::new(),
            block_sats: Vec::new(),
            spatial_index: SceneIndex::default(),
            edges: Vec::new(),
            edge_segments: Vec::new(),
            region_edges: Vec::new(),
            region_edge_indexes: Vec::new(),
            cross_edges: 0..0,
            cross_edge_index: EdgeIndex::default(),
            totals: Totals::default(),
        }
    }
}

#[derive(Clone)]
pub struct ChildIndices<'a> {
    indirect: Option<&'a [u32]>,
    next: u32,
    end: u32,
}

impl<'a> ChildIndices<'a> {
    fn new(range: Range<u32>, adjacency: &'a [u32]) -> Self {
        let indirect = if adjacency.is_empty() {
            None
        } else {
            Some(
                adjacency
                    .get(range.start as usize..range.end as usize)
                    .expect("child adjacency covers every declared range"),
            )
        };
        ChildIndices {
            indirect,
            next: range.start,
            end: range.end,
        }
    }

    pub fn len(&self) -> usize {
        self.indirect
            .map(|indices| indices.len())
            .unwrap_or((self.end - self.next) as usize)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Iterator for ChildIndices<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self.indirect {
            Some([]) => None,
            Some([first, rest @ ..]) => {
                self.indirect = Some(rest);
                Some(*first as usize)
            }
            None if self.next < self.end => {
                let next = self.next;
                self.next += 1;
                Some(next as usize)
            }
            None => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl ExactSizeIterator for ChildIndices<'_> {}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn edge_stays_eight_bytes() {
        assert_eq!(size_of::<Endpoint>(), 4);
        assert_eq!(size_of::<Edge>(), 8);
    }

    #[test]
    fn intersection_fraction_is_relative_to_the_receiver() {
        let rect = Rect::new(0.0, 0.0, 10.0, 8.0);
        assert_eq!(rect.intersection_fraction(&rect), 1.0);
        assert_eq!(
            rect.intersection_fraction(&Rect::new(5.0, 0.0, 10.0, 8.0)),
            0.5
        );
        assert_eq!(
            rect.intersection_fraction(&Rect::new(20.0, 0.0, 1.0, 1.0)),
            0.0
        );
        assert_eq!(Rect::ZERO.intersection_fraction(&rect), 0.0);
    }

    #[test]
    fn level_and_index_round_trip() {
        for level in [Level::Region, Level::Block, Level::Cell, Level::Sat] {
            for idx in [0, 1, 2, 41, 1_000, 1_000_000, Endpoint::MAX_INDEX] {
                let e = Endpoint::new(level, idx);
                assert_eq!(e.level(), level, "level for {idx}");
                assert_eq!(e.index(), idx, "index for {level:?}");
            }
        }
    }

    #[test]
    fn constructors_agree_with_new() {
        assert_eq!(Endpoint::region(7), Endpoint::new(Level::Region, 7));
        assert_eq!(Endpoint::block(7), Endpoint::new(Level::Block, 7));
        assert_eq!(Endpoint::cell(7), Endpoint::new(Level::Cell, 7));
        assert_eq!(Endpoint::sat(7), Endpoint::new(Level::Sat, 7));
        let zeros = [
            Endpoint::region(0),
            Endpoint::block(0),
            Endpoint::cell(0),
            Endpoint::sat(0),
        ];
        for (i, a) in zeros.iter().enumerate() {
            for b in &zeros[i + 1..] {
                assert_ne!(a, b, "distinct levels collided at index 0");
            }
        }
    }

    #[test]
    fn is_block_pair_agrees_with_matching_on_levels() {
        let levels = [Level::Region, Level::Block, Level::Cell, Level::Sat];
        for la in levels {
            for lb in levels {
                let e = Edge {
                    a: Endpoint::new(la, 5),
                    b: Endpoint::new(lb, 6),
                };
                let expected = la == Level::Block && lb == Level::Block;
                assert_eq!(e.is_block_pair(), expected, "{la:?}/{lb:?}");
            }
        }
        assert!(Edge::blocks(Endpoint::MAX_INDEX, 0).is_block_pair());
        assert!(Edge::blocks(0, Endpoint::MAX_INDEX).is_block_pair());
    }

    #[test]
    fn blocks_helper_matches_the_old_untagged_shape() {
        let e = Edge::blocks(3, 9);
        assert_eq!(e.a.level(), Level::Block);
        assert_eq!(e.b.level(), Level::Block);
        assert_eq!((e.a.index(), e.b.index()), (3, 9));
    }

    #[test]
    fn a_pod_can_link_to_a_service() {
        let e = Edge {
            a: Endpoint::cell(12),
            b: Endpoint::sat(4),
        };
        assert_eq!(e.a.level(), Level::Cell);
        assert_eq!(e.b.level(), Level::Sat);
        assert_eq!(size_of_val(&e), 8);
    }
}
