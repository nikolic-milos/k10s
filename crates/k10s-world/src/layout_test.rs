//! The invariants a layout must hold in both modes: nothing overlaps, every
//! child is contained by its parent, satellites keep their clearance, and the
//! same input lays out identically twice. The fingerprints are committed, so a
//! change of arrangement has to be an intended one.

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
    let dense_cov: f32 =
        dense.ns_rects.iter().map(|r| r.w * r.h).sum::<f32>() / (dense.bounds.w * dense.bounds.h);
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
                                assert!(inside(&card, &halo), "{label}: card {wl} escapes halo");
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
                                assert!(inside(&sr, &halo), "{label}: sat {sat} escapes halo {wl}");
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
