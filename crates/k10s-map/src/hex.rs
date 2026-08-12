use k10s_atlas::Rect;

const BASE_R: f32 = 48.0;
const MIN_PX: f32 = 72.0;

const MAX_RINGS: usize = u16::MAX as usize / 14;

pub fn hex_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("K10S_NO_HEX").is_none_or(|v| v == "0"))
}

pub fn level(zoom: f32) -> (f32, f32) {
    let n = (MIN_PX / (2.0 * BASE_R * zoom)).log2().ceil();
    let r = BASE_R * n.exp2();
    let px = 2.0 * r * zoom;
    let t = ((px - MIN_PX) / MIN_PX).clamp(0.0, 1.0);
    let alpha = 0.10 + 0.16 * (1.0 - (2.0 * t - 1.0).abs());
    (r, alpha)
}

fn ring_band(visible: &Rect, r: f32) -> (i64, i64, i64, i64) {
    let col_pitch = 1.5 * r;
    let row_pitch = 3.0f32.sqrt() * r;
    (
        ((visible.x - r) / col_pitch).floor() as i64,
        ((visible.max_x() + r) / col_pitch).ceil() as i64,
        ((visible.y - row_pitch) / row_pitch).floor() as i64,
        ((visible.max_y() + row_pitch) / row_pitch).ceil() as i64,
    )
}

fn ring_count((c0, c1, r0, r1): (i64, i64, i64, i64)) -> usize {
    (c1 - c0 + 1).max(0) as usize * (r1 - r0 + 1).max(0) as usize
}

pub fn effective_radius(visible: &Rect, r: f32) -> f32 {
    let mut r = r;
    let mut band = ring_band(visible, r);
    while ring_count(band) > MAX_RINGS {
        let overshoot = ring_count(band) as f32 / MAX_RINGS as f32;
        r *= overshoot.sqrt().max(1.01);
        band = ring_band(visible, r);
    }
    r
}

pub fn for_each_center(visible: &Rect, r: f32, mut emit: impl FnMut(f32, f32)) -> usize {
    let r = effective_radius(visible, r);
    let (c0, c1, r0, r1) = ring_band(visible, r);
    let col_pitch = 1.5 * r;
    let row_pitch = 3.0f32.sqrt() * r;
    let mut n = 0usize;
    for c in c0..=c1 {
        let x = c as f32 * col_pitch;
        let y_off = if c.rem_euclid(2) == 1 {
            row_pitch * 0.5
        } else {
            0.0
        };
        for row in r0..=r1 {
            emit(x, row as f32 * row_pitch + y_off);
            n += 1;
        }
    }
    n
}

pub fn visible_count(visible: &Rect, zoom: f32, suppressed: bool) -> usize {
    if suppressed {
        return 0;
    }
    let (r, _) = level(zoom);
    for_each_center(visible, r, |_, _| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_keeps_hexes_in_screen_band() {
        for zoom in [0.004f32, 0.01, 0.09, 0.55, 1.0, 4.5, 40.0] {
            let (r, alpha) = level(zoom);
            let px = 2.0 * r * zoom;
            assert!(
                (MIN_PX..2.0 * MIN_PX + 0.1).contains(&px),
                "zoom {zoom}: hex {px} px outside band"
            );
            assert!(alpha > 0.0 && alpha < 0.3);
        }
    }

    #[test]
    fn a_clamped_grid_is_stable_under_its_own_effective_radius() {
        let visible = Rect::new(0.0, 0.0, 1.0e6, 1.0e6);
        let r = 100.0;
        let clamped = effective_radius(&visible, r);
        assert!(clamped > r, "this viewport must engage the clamp");
        assert_eq!(
            clamped,
            effective_radius(&visible, clamped),
            "effective_radius must be idempotent so centers and vertices agree"
        );
        let mut original = Vec::new();
        let mut reclamped = Vec::new();
        for_each_center(&visible, r, |x, y| original.push((x, y)));
        for_each_center(&visible, clamped, |x, y| reclamped.push((x, y)));
        assert_eq!(original, reclamped, "pre-clamping must not move any center");
    }

    #[test]
    fn count_is_viewport_bounded_and_matches_enumeration() {
        for zoom in [0.004f32, 0.1, 1.0, 10.0] {
            let (r, _) = level(zoom);
            let visible = Rect::new(-800.0 / zoom, -500.0 / zoom, 1600.0 / zoom, 1000.0 / zoom);
            let mut seen = 0usize;
            let n = for_each_center(&visible, r, |_, _| seen += 1);
            assert_eq!(n, seen);
            assert!(n > 0, "zoom {zoom}: grid empty");
            assert!(n <= MAX_RINGS, "zoom {zoom}: {n} hexes");
            assert_eq!(
                n,
                ring_count(ring_band(&visible, r)),
                "zoom {zoom}: the clamp engaged at 1600x1000"
            );
            assert_eq!(n, visible_count(&visible, zoom, false));
        }
    }

    #[test]
    fn grid_holds_on_every_viewport() {
        for (name, vw, vh) in [
            ("1600x1000", 1600.0f32, 1000.0f32),
            ("1251x1350", 1251.0, 1350.0),
            ("2560x1440", 2560.0, 1440.0),
            ("3840x2160", 3840.0, 2160.0),
            ("5120x2880", 5120.0, 2880.0),
            ("7680x4320", 7680.0, 4320.0),
        ] {
            let mut zoom = k10s_atlas::MIN_ZOOM;
            while zoom <= k10s_atlas::MAX_ZOOM {
                let (r, _) = level(zoom);
                let visible = Rect::new(-0.5 * vw / zoom, -0.5 * vh / zoom, vw / zoom, vh / zoom);
                let n = for_each_center(&visible, r, |_, _| {});
                assert!(n > 0, "{name} at zoom {zoom}: no backdrop");
                assert!(n <= MAX_RINGS, "{name} at zoom {zoom}: {n} rings");
                let afford = ring_count(ring_band(&visible, r)).min(MAX_RINGS);
                assert!(
                    4 * n >= 3 * afford,
                    "{name} at zoom {zoom}: {n} rings where {afford} fit"
                );
                zoom *= 2.0f32.powf(1.0 / 64.0);
            }
        }
    }

    #[test]
    fn ring_cap_is_what_one_path_holds() {
        let builds = |n: usize| {
            let mut b = gpui::PathBuilder::stroke(gpui::px(1.0));
            for i in 0..n {
                let (cx, cy) = ((i % 100) as f32 * 80.0, (i / 100) as f32 * 70.0);
                for k in 0..6 {
                    let ang = k as f32 * std::f32::consts::FRAC_PI_3;
                    let p = gpui::point(
                        gpui::px(cx + 36.0 * ang.cos()),
                        gpui::px(cy + 36.0 * ang.sin()),
                    );
                    if k == 0 {
                        b.move_to(p);
                    } else {
                        b.line_to(p);
                    }
                }
                b.close();
            }
            b.build().is_ok()
        };
        assert!(builds(MAX_RINGS), "{MAX_RINGS} rings must tessellate");
        assert!(
            !builds(MAX_RINGS + 1),
            "the cap is stale: {} rings still tessellate",
            MAX_RINGS + 1
        );
    }

    #[test]
    fn suppressed_counts_zero() {
        let visible = Rect::new(0.0, 0.0, 1600.0, 1000.0);
        assert_eq!(visible_count(&visible, 1.0, true), 0);
    }
}
