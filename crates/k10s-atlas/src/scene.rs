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

    pub fn inflate(&self, m: f32) -> Rect {
        Rect::new(self.x - m, self.y - m, self.w + 2.0 * m, self.h + 2.0 * m)
    }
}

#[derive(Debug, Clone)]
pub struct RegionNode<X = ()> {
    pub rect: Rect,
    pub label: Arc<str>,
    pub weight: u32,
    pub children: Range<u32>,
    pub ext: X,
}

#[derive(Debug, Clone)]
pub struct BlockNode<X = ()> {
    pub rect: Rect,

    pub inner: Rect,
    pub label: Arc<str>,
    pub children: Range<u32>,
    pub sats: Range<u32>,
    pub ext: X,
}

#[derive(Debug, Clone)]
pub struct CellNode<X = ()> {
    pub rect: Rect,
    pub label: Arc<str>,
    pub ext: X,
}

#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub a: u32,
    pub b: u32,
}

#[derive(Debug, Clone, Copy, Default)]
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
    pub edges: Vec<Edge>,

    pub region_edges: Vec<Range<u32>>,
    pub cross_edges: Range<u32>,
    pub totals: Totals,
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
            edges: Vec::new(),
            region_edges: Vec::new(),
            cross_edges: 0..0,
            totals: Totals::default(),
        }
    }
}
