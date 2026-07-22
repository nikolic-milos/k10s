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
    debug_assert!(on > 0.0 && off > 0.0);
    let mut prev = start;
    let mut drawing = true;
    let mut remain = on;
    let mut pen_down = false;
    for &p in points {
        let (dx, dy) = (p.0 - prev.0, p.1 - prev.1);
        let full = (dx * dx + dy * dy).sqrt();
        if full <= f32::EPSILON {
            prev = p;
            continue;
        }
        let (ux, uy) = (dx / full, dy / full);
        let mut pos = prev;
        let mut seg = full;
        while seg > 0.0 {
            let step = seg.min(remain);
            let next = (pos.0 + ux * step, pos.1 + uy * step);
            if drawing {
                if !pen_down {
                    emit(true, pos);
                    pen_down = true;
                }
                emit(false, next);
            }
            pos = next;
            seg -= step;
            remain -= step;
            if remain <= 0.0 {
                drawing = !drawing;
                remain = if drawing { on } else { off };
                pen_down = false;
            }
        }
        prev = p;
    }
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
    fn ctrl_is_deterministic_and_bounded() {
        let a = (0.0, 0.0);
        let b = (100.0, 0.0);
        assert_eq!(curve_ctrl(a, b, 0.5), curve_ctrl(a, b, 0.5));
        let (cx, cy) = curve_ctrl(a, b, 1.0);
        assert!((cx - 50.0).abs() < 1e-6);
        assert!((cy - 22.0).abs() < 1e-6, "bow 100% of unit = 22% of chord");
    }
}
