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

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn edge_stays_eight_bytes() {
        assert_eq!(size_of::<Endpoint>(), 4);
        assert_eq!(size_of::<Edge>(), 8);
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
