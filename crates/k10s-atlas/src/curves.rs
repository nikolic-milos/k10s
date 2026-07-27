const MIN_DASH: f32 = 1e-3;

/// The most on/off periods one polyline may spend. The floor this puts under `on` and `off` is what
/// makes [`dash_polyline`] terminate, and terminating is all it is for: a step below one ULP of the
/// distance already walked leaves the walk where it was, so `on` at [`MIN_DASH`] over a 40,000 px
/// segment never returns, and 6 px over 1e8 spends 18 million vertices getting there. At 2^-16 of
/// the line the floor clears one ULP -- 2^-23 of it -- by 128x, and is still slack enough never to
/// recut a pitch a caller asked for: the curve layer's 6/5 px pattern survives to a 327,680 px
/// curve, which is 8,192 world px at maximum zoom. It is no kind of vertex budget -- one path holds
/// nowhere near 65,536 periods -- and how many curves a frame draws is the caller's to bound.
const MAX_PERIODS: f32 = 65_536.0;

pub fn flatten_quadratic(
    p0: (f32, f32),
    ctrl: (f32, f32),
    p1: (f32, f32),
    tol: f32,
    mut emit: impl FnMut((f32, f32)),
) {
    let ax = p0.0 - 2.0 * ctrl.0 + p1.0;
    let ay = p0.1 - 2.0 * ctrl.1 + p1.1;
    let dev = (ax * ax + ay * ay).sqrt();
    let k = ((dev / (4.0 * tol.max(1e-3))).sqrt().ceil() as usize).clamp(1, 64);
    let inv = 1.0 / k as f32;
    for i in 1..=k {
        let t = i as f32 * inv;
        let mt = 1.0 - t;
        emit((
            mt * mt * p0.0 + 2.0 * mt * t * ctrl.0 + t * t * p1.0,
            mt * mt * p0.1 + 2.0 * mt * t * ctrl.1 + t * t * p1.1,
        ));
    }
}

pub fn dash_polyline(
    start: (f32, f32),
    points: &[(f32, f32)],
    on: f32,
    off: f32,
    mut emit: impl FnMut(bool, (f32, f32)),
) {
    // The floor under the pattern is a fraction of the whole line, so the length comes first. Two
    // passes over 64 flattened points is nothing next to a walk that does not terminate.
    let mut total = 0.0;
    let mut prev = start;
    for &p in points {
        total += seg_len(prev, p).unwrap_or(0.0);
        prev = p;
    }
    let floor = (total / MAX_PERIODS).max(MIN_DASH);
    let on = on.max(floor);
    let off = off.max(floor);

    let mut prev = start;
    let mut drawing = true;
    let mut remain = on;
    let mut pen_down = false;
    for &p in points {
        let Some(full) = seg_len(prev, p) else {
            prev = p;
            continue;
        };
        let (ux, uy) = ((p.0 - prev.0) / full, (p.1 - prev.1) / full);
        // `walked` is the distance from the segment start, and both points come straight off it, so
        // the walk neither drifts nor stalls: the step is the rest of the dash or the rest of the
        // segment, and two distinct floats differ by at least one ULP of the smaller.
        let mut walked = 0.0f32;
        while walked < full {
            let step = remain.min(full - walked);
            let end = walked + step;
            if drawing {
                if !pen_down {
                    emit(true, (prev.0 + ux * walked, prev.1 + uy * walked));
                    pen_down = true;
                }
                emit(false, (prev.0 + ux * end, prev.1 + uy * end));
            }
            remain -= step;
            if remain <= 0.0 {
                drawing = !drawing;
                remain = if drawing { on } else { off };
                pen_down = false;
            }
            walked = end;
        }
        prev = p;
    }
}

/// `None` for a segment the walk has to skip: no length to dash, or an endpoint that arithmetic
/// upstream turned into a NaN or an infinity, which no pattern can subdivide.
fn seg_len(a: (f32, f32), b: (f32, f32)) -> Option<f32> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    (len.is_finite() && len > f32::EPSILON).then_some(len)
}

#[expect(clippy::too_many_arguments)]
pub fn dash_quadratic(
    p0: (f32, f32),
    ctrl: (f32, f32),
    p1: (f32, f32),
    tol: f32,
    on: f32,
    off: f32,
    scratch: &mut Vec<(f32, f32)>,
    emit: impl FnMut(bool, (f32, f32)),
) {
    scratch.clear();
    flatten_quadratic(p0, ctrl, p1, tol, |p| scratch.push(p));
    dash_polyline(p0, scratch, on, off, emit);
}

pub fn curve_ctrl(a: (f32, f32), b: (f32, f32), bow: f32) -> (f32, f32) {
    let (mx, my) = ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    (mx - dy * 0.22 * bow, my + dx * 0.22 * bow)
}

pub fn bow_jitter(i: u64) -> f32 {
    let mut x = i.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^= x >> 31;
    ((x >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad_point(p0: (f32, f32), c: (f32, f32), p1: (f32, f32), t: f32) -> (f32, f32) {
        let mt = 1.0 - t;
        (
            mt * mt * p0.0 + 2.0 * mt * t * c.0 + t * t * p1.0,
            mt * mt * p0.1 + 2.0 * mt * t * c.1 + t * t * p1.1,
        )
    }

    #[test]
    fn flatten_stays_within_tolerance() {
        let p0 = (10.0, 20.0);
        let c = (200.0, 350.0);
        let p1 = (420.0, 40.0);
        let tol = 0.35;
        let mut pts = vec![p0];
        flatten_quadratic(p0, c, p1, tol, |p| pts.push(p));
        assert_eq!(*pts.last().unwrap(), p1);
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let q = quad_point(p0, c, p1, t);
            let mut best = f32::MAX;
            for w in pts.windows(2) {
                let (a, b) = (w[0], w[1]);
                let (abx, aby) = (b.0 - a.0, b.1 - a.1);
                let len2 = (abx * abx + aby * aby).max(1e-9);
                let s = (((q.0 - a.0) * abx + (q.1 - a.1) * aby) / len2).clamp(0.0, 1.0);
                let (px_, py_) = (a.0 + abx * s, a.1 + aby * s);
                let d2 = (q.0 - px_) * (q.0 - px_) + (q.1 - py_) * (q.1 - py_);
                best = best.min(d2);
            }
            assert!(
                best.sqrt() <= tol * 1.05,
                "t={t}: deviation {} > tol {tol}",
                best.sqrt()
            );
        }
    }

    #[test]
    fn dash_pattern_covers_expected_length() {
        let start = (0.0, 0.0);
        let pts = [(110.0, 0.0)];
        let mut subpaths = 0usize;
        let mut drawn = 0.0f32;
        let mut last = (0.0, 0.0);
        dash_polyline(start, &pts, 6.0, 5.0, |is_move, p| {
            if is_move {
                subpaths += 1;
            } else {
                drawn += ((p.0 - last.0).powi(2) + (p.1 - last.1).powi(2)).sqrt();
            }
            last = p;
        });
        assert_eq!(subpaths, 10);
        assert!((drawn - 60.0).abs() < 1e-3, "drawn {drawn}");
    }

    #[test]
    fn dash_phase_carries_across_vertices() {
        let mut straight = 0usize;
        dash_polyline((0.0, 0.0), &[(110.0, 0.0)], 6.0, 5.0, |m, _| {
            straight += m as usize
        });
        let mut bent = 0usize;
        dash_polyline(
            (0.0, 0.0),
            &[(55.0, 0.0), (55.0, 55.0)],
            6.0,
            5.0,
            |m, _| bent += m as usize,
        );
        assert_eq!(straight, bent);
    }

    #[test]
    fn degenerate_dash_lengths_terminate() {
        for (on, off) in [
            (0.0, 0.0),
            (0.0, 5.0),
            (6.0, 0.0),
            (-1.0, -1.0),
            (-3.0, 4.0),
            (f32::MIN_POSITIVE, f32::MIN_POSITIVE),
        ] {
            let mut emitted = 0usize;
            dash_polyline((0.0, 0.0), &[(40.0, 0.0)], on, off, |_, _| emitted += 1);
            // 40 px is far too short for [`MAX_PERIODS`] to bind, so [`MIN_DASH`] is the floor here
            // and the ceiling is a vertex pair per period of it: 40,012 for the patterns that clamp
            // to it, where the 12 is the walk's own rounding. The 1% is for that rounding and no
            // more -- the bound this test used to carry was 4x the arithmetic maximum.
            let bound = 2 * (40.0 / (2.0 * MIN_DASH)) as usize;
            assert!(
                emitted <= bound + bound / 100,
                "on {on} off {off} emitted {emitted}"
            );
        }
    }

    /// The lengths the curve layer actually hands [`dash_polyline`]: it dashes in screen space at
    /// 6/5 px and zooms to 40x, so a few thousand pixels is an ordinary curve and two of them in one
    /// frame have to come out with the same pitch. A floor at 512 periods gave all three of these
    /// exactly 256 dashes whatever their length -- 4,000 px gave up 364 for 256, 40,000 px gave up
    /// 3,637 -- which is a pitch set by the curve instead of by the caller.
    #[test]
    fn long_lines_keep_the_pitch_they_asked_for() {
        for len in [4_000.0f32, 8_000.0, 40_000.0] {
            let mut starts = Vec::new();
            dash_polyline((0.0, 0.0), &[(len, 0.0)], 6.0, 5.0, |is_move, p| {
                if is_move {
                    starts.push(p.0);
                }
            });
            assert_eq!(
                starts.len(),
                (len / 11.0).ceil() as usize,
                "{len} px: {} dashes",
                starts.len()
            );
            let worst = starts
                .windows(2)
                .map(|w| (w[1] - w[0] - 11.0).abs())
                .fold(0.0f32, f32::max);
            assert!(worst < 0.01, "{len} px: pitch off by {worst}");
        }
    }

    #[test]
    fn long_lines_bound_the_dash_count() {
        let mut subpaths = 0usize;
        let mut emitted = 0usize;
        let mut last = (0.0f32, 0.0f32);
        dash_polyline((0.0, 0.0), &[(1e8, 0.0)], 6.0, 5.0, |is_move, p| {
            subpaths += is_move as usize;
            emitted += 1;
            last = p;
        });
        assert!(emitted <= 2 * MAX_PERIODS as usize, "emitted {emitted}");
        assert!(subpaths > 1, "a coarser pattern is still a pattern");
        assert!(last.0 >= 0.99 * 1e8, "pattern ran out at {last:?}");
    }

    #[test]
    fn dash_walk_cannot_stall() {
        // Both of these spin forever against an advance that counts the dash down from the distance
        // left: 1e-3 disappears under one ULP of 40,000, and nothing subtracts from an infinity. The
        // test is that they return; the bound says they returned for the right reason.
        for (case, points, on, off) in [
            (
                "pattern below the arithmetic",
                vec![(4e4f32, 0.0f32)],
                0.0f32,
                0.0f32,
            ),
            (
                "non-finite vertex",
                vec![(f32::INFINITY, 0.0), (40.0, 0.0)],
                6.0,
                5.0,
            ),
        ] {
            let mut emitted = 0usize;
            dash_polyline((0.0, 0.0), &points, on, off, |_, _| emitted += 1);
            assert!(
                emitted <= 2 * MAX_PERIODS as usize,
                "{case}: emitted {emitted}"
            );
        }
    }

    #[test]
    fn ctrl_is_deterministic_and_bounded() {
        let a = (0.0, 0.0);
        let b = (100.0, 0.0);
        assert_eq!(curve_ctrl(a, b, 0.5), curve_ctrl(a, b, 0.5));
        let (cx, cy) = curve_ctrl(a, b, 1.0);
        assert!((cx - 50.0).abs() < 1e-6);
        assert!((cy - 22.0).abs() < 1e-6, "bow 100% of unit = 22% of chord");
    }
}
