use crate::input::ClusterInput;
use k10s_core::Rect;
use k10s_core::layout::*;
use rustc_hash::FxHashMap as HashMap;

/// Spread: island scatter with satellite rings. Dense: shelf pack, no sats.
///
/// Node and Zone packing are not variants. `Payload::Instance` and
/// `PreparedPod` carry only `state`; `mapping::stage_pod` keeps labels and
/// volume refs, not `spec.nodeName`. Adding a layout that packed by node
/// would either invent positions or churn every exhaustive match that the
/// committed fingerprints pin. Filter a published snapshot instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Spread,
    Dense,
}

impl LayoutMode {
    pub fn parse(s: &str) -> Option<LayoutMode> {
        match s {
            "spread" => Some(LayoutMode::Spread),
            "dense" => Some(LayoutMode::Dense),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LayoutMode::Spread => "spread",
            LayoutMode::Dense => "dense",
        }
    }

    pub fn emits_attachments(self) -> bool {
        match self {
            LayoutMode::Spread => true,
            LayoutMode::Dense => false,
        }
    }
}

pub struct LayoutOut {
    pub ns_rects: Vec<Rect>,
    pub wl_rects: Vec<Rect>,
    pub card_rects: Vec<Rect>,
    pub pod_rects: Vec<Rect>,
    pub sat_rects: Vec<Rect>,
    pub bounds: Rect,
}

fn shelf_pack_into(
    sizes: &[(f32, f32)],
    gap: f32,
    aspect: f32,
    order: &mut Vec<usize>,
    pos: &mut Vec<(f32, f32)>,
) -> (f32, f32) {
    if sizes.is_empty() {
        return (0.0, 0.0);
    }
    let base = pos.len();
    pos.resize(base + sizes.len(), (0.0, 0.0));
    let total_area: f32 = sizes.iter().map(|(w, h)| w * h).sum();
    let widest = sizes.iter().map(|s| s.0).fold(0.0, f32::max);
    let target_w = (total_area * aspect).sqrt().max(widest);

    order.clear();
    order.extend(0..sizes.len());
    order.sort_by(|&a, &b| sizes[b].1.total_cmp(&sizes[a].1));

    let (mut x, mut y, mut shelf_h, mut max_w) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for &i in &*order {
        let (w, h) = sizes[i];
        if x > 0.0 && x + w > target_w {
            y += shelf_h + gap;
            x = 0.0;
            shelf_h = 0.0;
        }
        pos[base + i] = (x, y);
        shelf_h = shelf_h.max(h);
        x += w + gap;
        max_w = max_w.max(x - gap);
    }
    (max_w, y + shelf_h)
}

pub fn pod_grid(n: usize) -> (usize, usize) {
    let cols = (n as f32).sqrt().ceil().max(1.0) as usize;
    let rows = n.div_ceil(cols);
    (cols, rows)
}

fn grid_size(cols: usize, rows: usize) -> (f32, f32) {
    let inner_w = cols as f32 * POD_PITCH - POD_GAP;
    let inner_h = rows as f32 * POD_PITCH - POD_GAP;
    (inner_w + 2.0 * WL_PAD, inner_h + 2.0 * WL_PAD + WL_HEADER)
}

fn card_size(cols: usize, rows: usize) -> (f32, f32) {
    let inner_w = cols as f32 * POD_PITCH - POD_GAP;
    let inner_h = rows as f32 * POD_PITCH - POD_GAP;
    (
        inner_w + 2.0 * CARD_PAD,
        inner_h + 2.0 * CARD_PAD + CARD_HEADER,
    )
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

fn jitter_unit(h: u64) -> f32 {
    ((h >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0
}

const GOLDEN_ANGLE: f32 = 2.399_963_2;

struct OrbitRings {
    radius: f32,
    remaining: usize,
}

impl Iterator for OrbitRings {
    type Item = (f32, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }

        let radius = self.radius;
        let capacity = ((std::f32::consts::TAU * radius / SAT_ARC_PITCH) as usize).max(6);
        let count = capacity.min(self.remaining);
        self.remaining -= count;
        if self.remaining > 0 {
            self.radius += SAT_RING_GAP;
        }
        Some((radius, count))
    }
}

fn orbit_rings(card_w: f32, card_h: f32, n: usize) -> OrbitRings {
    OrbitRings {
        radius: 0.5 * (card_w * card_w + card_h * card_h).sqrt() + SAT_RING0_GAP,
        remaining: n,
    }
}

trait CollisionIndex {
    type Probe;

    fn probe(&self, rect: &Rect) -> Option<Self::Probe>;
    fn insert(&mut self, rect: Rect, probe: Self::Probe);
}

#[derive(Clone, Copy)]
struct SpiralPoint {
    sqrt_index: f32,
    cos: f32,
    sin: f32,
}

fn spiral_point(points: &mut Vec<SpiralPoint>, index: usize) -> SpiralPoint {
    while points.len() <= index {
        let index = points.len();
        let theta = index as f32 * GOLDEN_ANGLE;
        points.push(SpiralPoint {
            sqrt_index: (index as f32).sqrt(),
            cos: theta.cos(),
            sin: theta.sin(),
        });
    }
    points[index]
}

#[derive(Clone, Copy)]
struct CollisionRect {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl CollisionRect {
    fn from_rect(rect: &Rect) -> Self {
        Self {
            min_x: rect.x,
            min_y: rect.y,
            max_x: rect.max_x(),
            max_y: rect.max_y(),
        }
    }

    fn intersects(self, other: Self) -> bool {
        self.min_x < other.max_x
            && other.min_x < self.max_x
            && self.min_y < other.max_y
            && other.min_y < self.max_y
    }
}

struct LinearCollision<'a> {
    placed: &'a mut Vec<CollisionRect>,
}

impl CollisionIndex for LinearCollision<'_> {
    type Probe = CollisionRect;

    fn probe(&self, rect: &Rect) -> Option<Self::Probe> {
        let candidate = CollisionRect::from_rect(rect);
        self.placed
            .iter()
            .all(|&other| !candidate.intersects(other))
            .then_some(candidate)
    }

    fn insert(&mut self, _rect: Rect, candidate: Self::Probe) {
        self.placed.push(candidate);
    }
}

struct GridCollision {
    cell: f32,
    placed: HashMap<(i32, i32), Vec<Rect>>,
}

impl GridCollision {
    fn cells_of(&self, rect: &Rect) -> (i32, i32, i32, i32) {
        let x0 = (rect.x / self.cell).floor() as i32;
        let y0 = (rect.y / self.cell).floor() as i32;
        let x1 = (rect.max_x() / self.cell).floor() as i32;
        let y1 = (rect.max_y() / self.cell).floor() as i32;
        (x0, y0, x1, y1)
    }
}

impl CollisionIndex for GridCollision {
    type Probe = (i32, i32, i32, i32);

    fn probe(&self, rect: &Rect) -> Option<Self::Probe> {
        let cells = self.cells_of(rect);
        for gx in cells.0..=cells.2 {
            for gy in cells.1..=cells.3 {
                if self
                    .placed
                    .get(&(gx, gy))
                    .is_some_and(|placed| placed.iter().any(|other| rect.intersects(other)))
                {
                    return None;
                }
            }
        }
        Some(cells)
    }

    fn insert(&mut self, rect: Rect, cells: Self::Probe) {
        for gx in cells.0..=cells.2 {
            for gy in cells.1..=cells.3 {
                self.placed.entry((gx, gy)).or_default().push(rect);
            }
        }
    }
}

fn scatter_place(
    sizes: &[(f32, f32)],
    margin: &dyn Fn(usize) -> f32,
    order: &[usize],
    out_origins: &mut Vec<(f32, f32)>,
    base: usize,
    step: f32,
    spiral: &mut Vec<SpiralPoint>,
    mut collision: impl CollisionIndex,
) -> Rect {
    let mut bounds: Option<Rect> = None;
    let mut k = 0usize;
    for &i in order {
        let (w, h) = sizes[i];
        let m = margin(i);
        let (fw, fh) = (w + 2.0 * m, h + 2.0 * m);
        loop {
            let point = spiral_point(spiral, k);
            let r = step * point.sqrt_index;
            let (cx, cy) = (r * point.cos, r * point.sin);
            let inflated = Rect::new(cx - fw * 0.5, cy - fh * 0.5, fw, fh);
            if let Some(probe) = collision.probe(&inflated) {
                let placed = Rect::new(cx - w * 0.5, cy - h * 0.5, w, h);
                out_origins[base + i] = (placed.x, placed.y);
                collision.insert(inflated, probe);
                bounds = Some(match bounds {
                    None => placed,
                    Some(b) => {
                        let x = b.x.min(placed.x);
                        let y = b.y.min(placed.y);
                        let mx = b.max_x().max(placed.max_x());
                        let my = b.max_y().max(placed.max_y());
                        Rect::new(x, y, mx - x, my - y)
                    }
                });
                break;
            }
            k += 1;
        }
    }
    bounds.unwrap_or(Rect::ZERO)
}

// Namespace packs are small and numerous: allocating a hash table plus one
// Vec per occupied cell cost more than testing their already-contiguous
// rectangles. The measured crossover is 128; the multi-thousand-namespace
// world pack stays on the grid so the fallback remains sub-quadratic.
const LINEAR_SCATTER_LIMIT: usize = 128;

fn scatter_pack(
    sizes: &[(f32, f32)],
    margin: &dyn Fn(usize) -> f32,
    order: &mut Vec<usize>,
    out_origins: &mut Vec<(f32, f32)>,
    linear_scratch: &mut Vec<CollisionRect>,
    spiral_scratch: &mut Vec<SpiralPoint>,
) -> Rect {
    if sizes.is_empty() {
        return Rect::ZERO;
    }
    let base = out_origins.len();
    out_origins.resize(base + sizes.len(), (0.0, 0.0));

    order.clear();
    order.extend(0..sizes.len());
    order.sort_by(|&a, &b| (sizes[b].0 * sizes[b].1).total_cmp(&(sizes[a].0 * sizes[a].1)));

    let mean_edge = sizes
        .iter()
        .enumerate()
        .map(|(i, s)| (s.0 + 2.0 * margin(i)).max(s.1 + 2.0 * margin(i)))
        .sum::<f32>()
        / sizes.len() as f32;
    let step = (mean_edge * 0.62).max(1.0);
    if sizes.len() <= LINEAR_SCATTER_LIMIT {
        linear_scratch.clear();
        linear_scratch.reserve(sizes.len());
        return scatter_place(
            sizes,
            margin,
            order,
            out_origins,
            base,
            step,
            spiral_scratch,
            LinearCollision {
                placed: linear_scratch,
            },
        );
    }

    scatter_place(
        sizes,
        margin,
        order,
        out_origins,
        base,
        step,
        spiral_scratch,
        GridCollision {
            cell: mean_edge.max(1.0),
            placed: HashMap::with_capacity_and_hasher(
                sizes.len().saturating_mul(4),
                Default::default(),
            ),
        },
    )
}

pub fn layout(spec: &ClusterInput, mode: LayoutMode) -> LayoutOut {
    match mode {
        LayoutMode::Dense => layout_dense(spec),
        LayoutMode::Spread => layout_spread(spec),
    }
}

fn layout_spread(spec: &ClusterInput) -> LayoutOut {
    let total_wl = spec.total_workloads as usize;
    let mut out = LayoutOut {
        ns_rects: Vec::with_capacity(spec.namespaces.len()),
        wl_rects: Vec::with_capacity(total_wl),
        card_rects: Vec::with_capacity(total_wl),
        pod_rects: Vec::with_capacity(spec.total_pods as usize),
        sat_rects: Vec::with_capacity(spec.total_sats as usize),
        bounds: Rect::ZERO,
    };

    let mut wl_halo: Vec<(f32, f32)> = Vec::with_capacity(total_wl);
    let mut wl_card: Vec<(f32, f32)> = Vec::with_capacity(total_wl);
    let mut wl_cols: Vec<u32> = Vec::with_capacity(total_wl);
    let mut wl_offsets: Vec<(f32, f32)> = Vec::with_capacity(total_wl);
    let mut ns_content: Vec<Rect> = Vec::with_capacity(spec.namespaces.len());
    let mut ns_sizes: Vec<(f32, f32)> = Vec::with_capacity(spec.namespaces.len());
    let mut order_scratch: Vec<usize> = Vec::new();
    let mut linear_scratch: Vec<CollisionRect> = Vec::new();
    let mut spiral_scratch: Vec<SpiralPoint> = Vec::new();

    for ns in &spec.namespaces {
        let start = wl_halo.len();
        for wl in &ns.workloads {
            let (cols, rows) = pod_grid(wl.pods.len());
            wl_cols.push(cols as u32);
            let (cw, ch) = card_size(cols, rows);
            wl_card.push((cw, ch));
            if wl.sats.is_empty() {
                wl_halo.push((cw, ch));
            } else {
                let outer_r = orbit_rings(cw, ch, wl.sats.len())
                    .last()
                    .expect("non-empty attachment list has at least one orbit ring")
                    .0;
                let half = outer_r + SAT_SIZE * 0.5 + SAT_MARGIN;
                wl_halo.push((2.0 * half, 2.0 * half));
            }
        }
        let content = scatter_pack(
            &wl_halo[start..],
            &|_| HUB_GAP * 0.5,
            &mut order_scratch,
            &mut wl_offsets,
            &mut linear_scratch,
            &mut spiral_scratch,
        );
        ns_content.push(content);
        ns_sizes.push((
            content.w + 2.0 * NS_PAD,
            content.h + 2.0 * NS_PAD + NS_HEADER,
        ));
    }

    let mut ns_origins: Vec<(f32, f32)> = Vec::with_capacity(spec.namespaces.len());
    let island_margin = |i: usize| -> f32 {
        let (w, h) = ns_sizes[i];
        0.5 * (ISLAND_GAP_FACTOR * w.max(h)).max(ISLAND_GAP_MIN)
    };
    let world = scatter_pack(
        &ns_sizes,
        &island_margin,
        &mut order_scratch,
        &mut ns_origins,
        &mut linear_scratch,
        &mut spiral_scratch,
    );
    let (shift_x, shift_y) = (-world.x, -world.y);
    out.bounds = Rect::new(0.0, 0.0, world.w, world.h);

    let mut wl_i = 0usize;
    for (ni, ns) in spec.namespaces.iter().enumerate() {
        let (nx, ny) = (ns_origins[ni].0 + shift_x, ns_origins[ni].1 + shift_y);
        let (nw, nh) = ns_sizes[ni];
        out.ns_rects.push(Rect::new(nx, ny, nw, nh));
        let content = ns_content[ni];
        let (cx0, cy0) = (nx + NS_PAD - content.x, ny + NS_PAD + NS_HEADER - content.y);

        for wl in &ns.workloads {
            let (ox, oy) = wl_offsets[wl_i];
            let (hw, hh) = wl_halo[wl_i];
            let (cw, ch) = wl_card[wl_i];
            let cols = wl_cols[wl_i] as usize;
            let halo = Rect::new(cx0 + ox, cy0 + oy, hw, hh);
            let card = Rect::new(halo.x + (hw - cw) * 0.5, halo.y + (hh - ch) * 0.5, cw, ch);
            out.wl_rects.push(halo);
            out.card_rects.push(card);

            let n = wl.pods.len();
            let base_x = card.x + CARD_PAD;
            let base_y = card.y + CARD_HEADER + CARD_PAD;
            let (mut c, mut r) = (0usize, 0usize);
            out.pod_rects.extend((0..n).map(|_| {
                let rect = Rect::new(
                    base_x + c as f32 * POD_PITCH,
                    base_y + r as f32 * POD_PITCH,
                    POD_SIZE,
                    POD_SIZE,
                );
                c += 1;
                if c == cols {
                    c = 0;
                    r += 1;
                }
                rect
            }));

            if !wl.sats.is_empty() {
                let (hub_x, hub_y) = card.center();
                let wl_seed = splitmix64(wl_i as u64);
                let mut sat_i = 0usize;
                for (ring_i, (ring_r, count)) in orbit_rings(cw, ch, wl.sats.len()).enumerate() {
                    let phase = jitter_unit(splitmix64(wl_seed ^ (ring_i as u64) << 8))
                        * std::f32::consts::PI;
                    let slot_angle = std::f32::consts::TAU / count as f32;
                    for j in 0..count {
                        let h = splitmix64(wl_seed ^ ((ring_i as u64) << 32 | j as u64));
                        let ang =
                            phase + j as f32 * slot_angle + jitter_unit(h) * slot_angle * 0.22;
                        let rad = ring_r + jitter_unit(splitmix64(h)) * SAT_JITTER_MAX;
                        let sx = hub_x + rad * ang.cos() - SAT_SIZE * 0.5;
                        let sy = hub_y + rad * ang.sin() - SAT_SIZE * 0.5;
                        out.sat_rects.push(Rect::new(sx, sy, SAT_SIZE, SAT_SIZE));
                        sat_i += 1;
                    }
                }
                debug_assert_eq!(sat_i, wl.sats.len());
            }
            wl_i += 1;
        }
    }
    debug_assert_eq!(wl_i, total_wl);

    out
}

fn layout_dense(spec: &ClusterInput) -> LayoutOut {
    let total_wl = spec.total_workloads as usize;
    let mut wl_sizes: Vec<(f32, f32)> = Vec::with_capacity(total_wl);
    let mut wl_cols: Vec<u32> = Vec::with_capacity(total_wl);
    let mut wl_offsets: Vec<(f32, f32)> = Vec::with_capacity(total_wl);
    let mut ns_sizes = Vec::with_capacity(spec.namespaces.len());
    let mut order_scratch: Vec<usize> = Vec::new();

    for ns in &spec.namespaces {
        let start = wl_sizes.len();
        for wl in &ns.workloads {
            let (cols, rows) = pod_grid(wl.pods.len());
            wl_cols.push(cols as u32);
            wl_sizes.push(grid_size(cols, rows));
        }
        let extent = shelf_pack_into(
            &wl_sizes[start..],
            WL_GAP,
            1.7,
            &mut order_scratch,
            &mut wl_offsets,
        );
        ns_sizes.push((extent.0 + 2.0 * NS_PAD, extent.1 + 2.0 * NS_PAD + NS_HEADER));
    }

    let mut ns_origins: Vec<(f32, f32)> = Vec::with_capacity(spec.namespaces.len());
    let world_extent = shelf_pack_into(
        &ns_sizes,
        NS_GAP,
        16.0 / 9.0,
        &mut order_scratch,
        &mut ns_origins,
    );

    let mut out = LayoutOut {
        ns_rects: Vec::with_capacity(spec.namespaces.len()),
        wl_rects: Vec::with_capacity(total_wl),
        card_rects: Vec::with_capacity(total_wl),
        pod_rects: Vec::with_capacity(spec.total_pods as usize),
        sat_rects: Vec::new(),
        bounds: Rect::new(0.0, 0.0, world_extent.0, world_extent.1),
    };

    let mut wl_i = 0usize;
    for (ni, ns) in spec.namespaces.iter().enumerate() {
        let (nx, ny) = ns_origins[ni];
        let (nw, nh) = ns_sizes[ni];
        out.ns_rects.push(Rect::new(nx, ny, nw, nh));

        for wl in &ns.workloads {
            let (ox, oy) = wl_offsets[wl_i];
            let (ww, wh) = wl_sizes[wl_i];
            let cols = wl_cols[wl_i] as usize;
            wl_i += 1;
            let wx = nx + NS_PAD + ox;
            let wy = ny + NS_PAD + NS_HEADER + oy;
            let rect = Rect::new(wx, wy, ww, wh);
            out.wl_rects.push(rect);
            out.card_rects.push(rect);

            let n = wl.pods.len();
            let base_x = wx + WL_PAD;
            let base_y = wy + WL_HEADER + WL_PAD;
            let (mut c, mut r) = (0usize, 0usize);
            out.pod_rects.extend((0..n).map(|_| {
                let rect = Rect::new(
                    base_x + c as f32 * POD_PITCH,
                    base_y + r as f32 * POD_PITCH,
                    POD_SIZE,
                    POD_SIZE,
                );
                c += 1;
                if c == cols {
                    c = 0;
                    r += 1;
                }
                rect
            }));
        }
    }
    debug_assert_eq!(wl_i, wl_sizes.len());

    out
}

#[cfg(test)]
#[path = "layout_test.rs"]
mod tests;
