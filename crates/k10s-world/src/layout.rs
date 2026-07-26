use std::collections::HashMap;

use crate::input::ClusterInput;
use k10s_core::Rect;
use k10s_core::layout::*;

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

fn orbit_rings(card_w: f32, card_h: f32, n: usize) -> (Vec<(f32, usize)>, f32) {
    let mut rings = Vec::new();
    let mut r = 0.5 * (card_w * card_w + card_h * card_h).sqrt() + SAT_RING0_GAP;
    let mut remaining = n;
    while remaining > 0 {
        let cap = ((std::f32::consts::TAU * r / SAT_ARC_PITCH) as usize).max(6);
        let take = cap.min(remaining);
        rings.push((r, take));
        remaining -= take;
        if remaining > 0 {
            r += SAT_RING_GAP;
        }
    }
    (rings, r)
}

fn scatter_pack(
    sizes: &[(f32, f32)],
    margin: &dyn Fn(usize) -> f32,
    order: &mut Vec<usize>,
    out_origins: &mut Vec<(f32, f32)>,
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
    let cell = mean_edge.max(1.0);

    let mut hash: HashMap<(i32, i32), Vec<Rect>> = HashMap::new();
    let cells_of = |r: &Rect| {
        let x0 = (r.x / cell).floor() as i32;
        let y0 = (r.y / cell).floor() as i32;
        let x1 = (r.max_x() / cell).floor() as i32;
        let y1 = (r.max_y() / cell).floor() as i32;
        (x0, y0, x1, y1)
    };

    let mut bounds: Option<Rect> = None;
    let mut k = 0u64;
    for &i in &*order {
        let (w, h) = sizes[i];
        let m = margin(i);
        let (fw, fh) = (w + 2.0 * m, h + 2.0 * m);
        loop {
            let r = step * (k as f32).sqrt();
            let theta = k as f32 * GOLDEN_ANGLE;
            let (cx, cy) = (r * theta.cos(), r * theta.sin());
            let inflated = Rect::new(cx - fw * 0.5, cy - fh * 0.5, fw, fh);
            let (x0, y0, x1, y1) = cells_of(&inflated);
            let mut free = true;
            'probe: for gx in x0..=x1 {
                for gy in y0..=y1 {
                    if let Some(v) = hash.get(&(gx, gy)) {
                        for other in v {
                            if inflated.intersects(other) {
                                free = false;
                                break 'probe;
                            }
                        }
                    }
                }
            }
            if free {
                let placed = Rect::new(cx - w * 0.5, cy - h * 0.5, w, h);
                out_origins[base + i] = (placed.x, placed.y);
                for gx in x0..=x1 {
                    for gy in y0..=y1 {
                        hash.entry((gx, gy)).or_default().push(inflated);
                    }
                }
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
                let (_, outer_r) = orbit_rings(cw, ch, wl.sats.len());
                let half = outer_r + SAT_SIZE * 0.5 + SAT_MARGIN;
                wl_halo.push((2.0 * half, 2.0 * half));
            }
        }
        let content = scatter_pack(
            &wl_halo[start..],
            &|_| HUB_GAP * 0.5,
            &mut order_scratch,
            &mut wl_offsets,
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
                let (rings, _) = orbit_rings(cw, ch, wl.sats.len());
                let (hub_x, hub_y) = card.center();
                let wl_seed = splitmix64(wl_i as u64);
                let mut sat_i = 0usize;
                for (ring_i, &(ring_r, count)) in rings.iter().enumerate() {
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
mod tests {
    use super::*;
    use crate::input::fold;
    use k10s_clustergen::{GenConfig, Scenario, generate, stream};

    fn gap(a: &Rect, b: &Rect) -> f32 {
        let dx = (a.x - b.max_x()).max(b.x - a.max_x()).max(0.0);
        let dy = (a.y - b.max_y()).max(b.y - a.max_y()).max(0.0);
        dx.max(dy)
    }

    fn platform(seed: u64, target_objects: u32) -> ClusterInput {
        gen_input(seed, target_objects, Scenario::Platform)
    }

    /// Generator to stream to fold, which is now the only way in. If the layout
    /// fingerprints still match after this, the fold is provably equivalent to the
    /// old direct spec walk.
    fn gen_input(seed: u64, target_objects: u32, scenario: Scenario) -> ClusterInput {
        let spec = generate(&GenConfig {
            seed,
            target_objects,
            scenario,
        });
        fold(&stream::snapshot(&spec, true)).0
    }

    fn all_rects(out: &LayoutOut) -> [&Vec<Rect>; 5] {
        [
            &out.ns_rects,
            &out.wl_rects,
            &out.card_rects,
            &out.pod_rects,
            &out.sat_rects,
        ]
    }

    fn escape(outer: &Rect, inner: &Rect) -> f32 {
        (outer.x - inner.x)
            .max(outer.y - inner.y)
            .max(inner.max_x() - outer.max_x())
            .max(inner.max_y() - outer.max_y())
            .max(0.0)
    }

    struct Fnv(u64);

    impl Fnv {
        fn new() -> Self {
            Fnv(0xCBF2_9CE4_8422_2325)
        }

        fn write(&mut self, v: u64) {
            self.0 ^= v;
            self.0 = self.0.wrapping_mul(0x100_0000_01B3);
        }
    }

    fn transcendental_free(mode: LayoutMode) -> bool {
        match mode {
            LayoutMode::Dense => true,
            LayoutMode::Spread => false,
        }
    }

    fn fingerprint(out: &LayoutOut, bit_exact: bool) -> u64 {
        let mut h = Fnv::new();
        let feed = |h: &mut Fnv, r: &Rect| {
            for v in [r.x, r.y, r.w, r.h] {
                if bit_exact {
                    h.write(v.to_bits() as u64);
                } else {
                    h.write(((v * 64.0).round() as i64) as u64);
                }
            }
        };
        for arr in all_rects(out) {
            h.write(arr.len() as u64);
            for r in arr {
                feed(&mut h, r);
            }
        }
        feed(&mut h, &out.bounds);
        h.0
    }

    #[test]
    fn namespaces_do_not_overlap_either_mode() {
        let spec = gen_input(42, 20_000, Scenario::Platform);
        for mode in [LayoutMode::Dense, LayoutMode::Spread] {
            let out = layout(&spec, mode);
            for i in 0..out.ns_rects.len() {
                for j in (i + 1)..out.ns_rects.len() {
                    assert!(
                        !out.ns_rects[i].intersects(&out.ns_rects[j]),
                        "{mode:?}: ns {i} and {j} overlap: {:?} vs {:?}",
                        out.ns_rects[i],
                        out.ns_rects[j]
                    );
                }
            }
        }
    }

    #[test]
    fn spread_islands_keep_map_distance() {
        let spec = gen_input(42, 20_000, Scenario::Platform);
        let out = layout(&spec, LayoutMode::Spread);
        for i in 0..out.ns_rects.len() {
            for j in (i + 1)..out.ns_rects.len() {
                let g = gap(&out.ns_rects[i], &out.ns_rects[j]);
                assert!(
                    g >= ISLAND_GAP_MIN - 1.0,
                    "islands {i}/{j} too close: gap {g}"
                );
            }
        }
        let island_area: f32 = out.ns_rects.iter().map(|r| r.w * r.h).sum();
        let world_area = out.bounds.w * out.bounds.h;
        let coverage = island_area / world_area;
        assert!(
            coverage < 0.40,
            "spread world coverage {coverage:.2} - not map-like"
        );

        let dense = layout(&spec, LayoutMode::Dense);
        let dense_cov: f32 = dense.ns_rects.iter().map(|r| r.w * r.h).sum::<f32>()
            / (dense.bounds.w * dense.bounds.h);
        assert!(
            coverage < dense_cov * 0.75,
            "spread ({coverage:.2}) must be materially sparser than dense ({dense_cov:.2})"
        );
    }

    #[test]
    fn workload_halos_do_not_overlap_within_namespace() {
        let spec = gen_input(7, 8_000, Scenario::Platform);
        let out = layout(&spec, LayoutMode::Spread);
        let mut wl = 0usize;
        for ns in &spec.namespaces {
            let n = ns.workloads.len();
            for i in 0..n {
                for j in (i + 1)..n {
                    assert!(
                        !out.wl_rects[wl + i].intersects(&out.wl_rects[wl + j]),
                        "halos {}/{} overlap",
                        wl + i,
                        wl + j
                    );
                }
            }
            wl += n;
        }
    }

    #[test]
    fn hierarchy_containment_spread() {
        let spec = gen_input(42, 6_000, Scenario::Platform);
        let out = layout(&spec, LayoutMode::Spread);
        let (mut wl, mut pod, mut sat) = (0usize, 0usize, 0usize);
        let eps = 0.01f32;
        let inside = |inner: &Rect, outer: &Rect| {
            inner.x >= outer.x - eps
                && inner.y >= outer.y - eps
                && inner.max_x() <= outer.max_x() + eps
                && inner.max_y() <= outer.max_y() + eps
        };
        for (ni, ns) in spec.namespaces.iter().enumerate() {
            let nr = out.ns_rects[ni];
            for w in &ns.workloads {
                let halo = out.wl_rects[wl];
                let card = out.card_rects[wl];
                assert!(inside(&halo, &nr), "halo {wl} escapes island {ni}");
                assert!(inside(&card, &halo), "card {wl} escapes halo");
                for _ in 0..w.pods.len() {
                    assert!(
                        inside(&out.pod_rects[pod], &card),
                        "pod {pod} escapes card {wl}"
                    );
                    pod += 1;
                }
                for _ in 0..w.sats.len() {
                    let sr = out.sat_rects[sat];
                    assert!(inside(&sr, &halo), "sat {sat} escapes halo {wl}");
                    assert!(!sr.intersects(&card), "sat {sat} collides with card {wl}");
                    sat += 1;
                }
                wl += 1;
            }
        }
        assert_eq!(sat, spec.total_sats as usize);
    }

    #[test]
    fn pods_stay_inside_their_workload_dense() {
        let spec = gen_input(42, 5_000, Scenario::Platform);
        let out = layout(&spec, LayoutMode::Dense);
        let mut pod = 0usize;
        let mut wl = 0usize;
        for ns in &spec.namespaces {
            for w in &ns.workloads {
                let wr = out.wl_rects[wl];
                for _ in 0..w.pods.len() {
                    let pr = out.pod_rects[pod];
                    assert!(
                        pr.x >= wr.x && pr.max_x() <= wr.max_x() + 0.01,
                        "pod {pod} escapes workload {wl}"
                    );
                    assert!(pr.y >= wr.y && pr.max_y() <= wr.max_y() + 0.01);
                    pod += 1;
                }
                wl += 1;
            }
        }
    }

    #[test]
    fn bounds_contain_every_rect() {
        const AUDITED_SPREAD_MARGIN_PX: f32 = 1.0 / 128.0;
        const SPREAD_TOLERANCE_PX: f32 = 0.01;
        for seed in [2u64, 42] {
            for objects in [25_000u32, 50_000] {
                let spec = platform(seed, objects);
                for mode in [LayoutMode::Dense, LayoutMode::Spread] {
                    let out = layout(&spec, mode);
                    let tolerance = match mode {
                        LayoutMode::Dense => 0.0,
                        LayoutMode::Spread => SPREAD_TOLERANCE_PX,
                    };
                    let mut worst = 0.0f32;
                    let mut escapes = 0usize;
                    let mut total = 0usize;
                    for arr in all_rects(&out) {
                        for r in arr {
                            total += 1;
                            let e = escape(&out.bounds, r);
                            if e > 0.0 {
                                escapes += 1;
                            }
                            worst = worst.max(e);
                        }
                    }
                    assert!(
                        worst <= tolerance,
                        "{mode:?} seed {seed} objects {objects}: {escapes} of {total} rects leave \
                         bounds {:?}, worst overhang {worst} px, tolerance {tolerance} px, \
                         audited f32 shift rounding overhang {AUDITED_SPREAD_MARGIN_PX} px",
                        out.bounds
                    );
                }
            }
        }
    }

    /// Skewed fan-out is the shape real clusters have and the themed scenarios do not: one
    /// namespace holding thousands of workloads, or one workload holding thousands of pods and
    /// an equal number of PVCs. Every invariant the themed scenarios are held to must survive it,
    /// per mode: containment and halo separation are spread-mode properties, because dense mode
    /// omits attachments and packs halos flush.
    #[test]
    fn fan_out_layout_holds_every_invariant() {
        const EPS: f32 = 0.01;
        let inside = |inner: &Rect, outer: &Rect| {
            inner.x >= outer.x - EPS
                && inner.y >= outer.y - EPS
                && inner.max_x() <= outer.max_x() + EPS
                && inner.max_y() <= outer.max_y() + EPS
        };
        for scenario in [Scenario::NsFanOut, Scenario::WlFanOut] {
            for objects in [12_000u32, 50_000] {
                let spec = gen_input(55, objects, scenario);
                for mode in [LayoutMode::Dense, LayoutMode::Spread] {
                    let out = layout(&spec, mode);
                    let label = format!("{} {objects} {mode:?}", scenario.as_str());

                    for i in 0..out.ns_rects.len() {
                        for j in (i + 1)..out.ns_rects.len() {
                            assert!(
                                !out.ns_rects[i].intersects(&out.ns_rects[j]),
                                "{label}: islands {i}/{j} overlap"
                            );
                        }
                    }

                    for arr in all_rects(&out) {
                        for r in arr {
                            let e = escape(&out.bounds, r);
                            assert!(e <= EPS, "{label}: rect {r:?} leaves bounds by {e} px");
                        }
                    }

                    let (mut wl, mut pod, mut sat) = (0usize, 0usize, 0usize);
                    for (ni, ns) in spec.namespaces.iter().enumerate() {
                        let nr = out.ns_rects[ni];
                        let first_wl = wl;
                        for w in &ns.workloads {
                            let halo = out.wl_rects[wl];
                            assert!(inside(&halo, &nr), "{label}: halo {wl} escapes island {ni}");
                            let parent = match mode {
                                LayoutMode::Dense => halo,
                                LayoutMode::Spread => {
                                    let card = out.card_rects[wl];
                                    assert!(
                                        inside(&card, &halo),
                                        "{label}: card {wl} escapes halo"
                                    );
                                    card
                                }
                            };
                            for _ in 0..w.pods.len() {
                                assert!(
                                    inside(&out.pod_rects[pod], &parent),
                                    "{label}: pod {pod} escapes workload {wl}"
                                );
                                pod += 1;
                            }
                            if mode == LayoutMode::Spread {
                                for _ in 0..w.sats.len() {
                                    let sr = out.sat_rects[sat];
                                    assert!(
                                        inside(&sr, &halo),
                                        "{label}: sat {sat} escapes halo {wl}"
                                    );
                                    sat += 1;
                                }
                            }
                            wl += 1;
                        }
                        if mode == LayoutMode::Spread {
                            for i in first_wl..wl {
                                for j in (i + 1)..wl {
                                    assert!(
                                        !out.wl_rects[i].intersects(&out.wl_rects[j]),
                                        "{label}: halos {i}/{j} overlap in ns {ni}"
                                    );
                                }
                            }
                        }
                    }
                    if mode == LayoutMode::Spread {
                        assert_eq!(sat, spec.total_sats as usize, "{label}: sat count");
                    }
                }
            }
        }
    }

    #[test]
    fn satellites_keep_clearance_within_their_hub() {
        const MAX_OVERLAP_DEPTH_PX: f32 = 0.2 * SAT_SIZE;
        const OVERLAP_PAIRS_PER_MILLION: usize = 50;
        for seed in [7u64, 42] {
            let spec = platform(seed, 25_000);
            let out = layout(&spec, LayoutMode::Spread);
            let mut sat = 0usize;
            let mut pairs = 0usize;
            let mut overlaps = 0usize;
            let mut closest_centers = f32::MAX;
            let mut deepest = 0.0f32;
            let mut deepest_pair = (0usize, 0usize);
            for ns in &spec.namespaces {
                for wl in &ns.workloads {
                    let n = wl.sats.len();
                    for i in 0..n {
                        for j in (i + 1)..n {
                            pairs += 1;
                            let (a, b) = (out.sat_rects[sat + i], out.sat_rects[sat + j]);
                            let (ac, bc) = (a.center(), b.center());
                            let (dx, dy) = ((ac.0 - bc.0).abs(), (ac.1 - bc.1).abs());
                            closest_centers = closest_centers.min((dx * dx + dy * dy).sqrt());
                            if a.intersects(&b) {
                                overlaps += 1;
                                let depth = (SAT_SIZE - dx).min(SAT_SIZE - dy);
                                if depth > deepest {
                                    deepest = depth;
                                    deepest_pair = (sat + i, sat + j);
                                }
                            }
                        }
                    }
                    sat += n;
                }
            }
            assert_eq!(sat, out.sat_rects.len());
            assert!(
                pairs > 50_000,
                "seed {seed}: only {pairs} sat pairs checked"
            );
            assert!(
                closest_centers >= SAT_SIZE,
                "seed {seed}: closest sat centers are {closest_centers} px apart, a sat is \
                 {SAT_SIZE} px wide, ring gap {SAT_RING_GAP} px against radial jitter \
                 {SAT_JITTER_MAX} px"
            );
            assert!(
                overlaps * 1_000_000 <= OVERLAP_PAIRS_PER_MILLION * pairs,
                "seed {seed}: {overlaps} of {pairs} sat rect pairs overlap, deepest \
                 {deepest} px at {deepest_pair:?}"
            );
            assert!(
                deepest <= MAX_OVERLAP_DEPTH_PX,
                "seed {seed}: sats {deepest_pair:?} overlap by {deepest} px of {SAT_SIZE} px"
            );
        }
    }

    #[test]
    fn only_attachment_modes_emit_satellite_rects() {
        let spec = platform(42, 8_000);
        assert!(spec.total_sats > 0);
        for mode in [LayoutMode::Dense, LayoutMode::Spread] {
            let out = layout(&spec, mode);
            assert_eq!(
                out.sat_rects.is_empty(),
                !mode.emits_attachments(),
                "{mode:?} disagrees with emits_attachments"
            );
            if mode.emits_attachments() {
                assert_eq!(out.sat_rects.len(), spec.total_sats as usize, "{mode:?}");
            }
        }
    }

    #[test]
    fn layout_fingerprints_are_committed() {
        let cases: &[(LayoutMode, u64, u32, u64)] = &[
            (LayoutMode::Dense, 1234, 10_000, 0x20d2_6824_fecf_31a4),
            (LayoutMode::Dense, 7, 25_000, 0xe9aa_f079_556b_16d5),
            (LayoutMode::Spread, 1234, 10_000, 0xedac_a8d8_afec_b255),
            (LayoutMode::Spread, 7, 25_000, 0x1c65_a882_421c_cd1a),
        ];
        for &(mode, seed, objects, expected) in cases {
            let spec = platform(seed, objects);
            let out = layout(&spec, mode);
            let got = fingerprint(&out, transcendental_free(mode));
            assert_eq!(
                got, expected,
                "{mode:?} seed {seed} objects {objects}: got {got:#018x}, want {expected:#018x}"
            );
        }
    }

    #[test]
    fn deterministic_both_modes() {
        let spec = gen_input(1234, 10_000, Scenario::Platform);
        for mode in [LayoutMode::Dense, LayoutMode::Spread] {
            let a = layout(&spec, mode);
            let b = layout(&spec, mode);
            assert_eq!(a.bounds, b.bounds, "{mode:?}");
            assert_eq!(a.sat_rects.len(), b.sat_rects.len());
            for (x, y) in a.sat_rects.iter().zip(&b.sat_rects) {
                assert_eq!(x, y);
            }
            for (x, y) in a.wl_rects.iter().zip(&b.wl_rects) {
                assert_eq!(x, y);
            }
        }
    }
}
