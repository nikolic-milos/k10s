use k10s_atlas::Rect;

const BASE_R: f32 = 48.0;
const MIN_PX: f32 = 72.0;

/// One stroked ring tessellates to 14 vertices and the whole backdrop is a single path, so the
/// `u16` index buffer gpui builds a path into is a hard ceiling on rings per frame: at 4,682
/// `PathBuilder::build` returns `Err` and the backdrop is gone. Measured against the fork rather
/// than derived, and pinned by `ring_cap_is_what_one_path_holds`.
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

/// The columns and rows whose hexes can reach `visible`, one hex wide on every side so a centre
/// just outside still paints the edge that crosses in.
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

/// Emit the centre of every hex covering `visible`, growing the hex radius by as little as it takes
/// to bring the ring count inside [`MAX_RINGS`].
///
/// [`level`] holds hex size fixed in pixels, so the ring count is a function of window size alone:
/// 627 rings at 1600x1000, 2,847 at 4K, 4,753 at 5K, 10,585 at 8K. A fixed cap is therefore a
/// display-size cliff, and a backdrop that vanishes when the window grows reads as a broken feature
/// rather than as a budget.
///
/// The count falls as 1/r^2, so the radius that just fits is one square root away and the grid gives
/// up only what it has to: across the whole zoom range on all six of those displays it never returns
/// less than 0.994 of the budget. Coarsening by whole levels instead -- the doublings [`level`]
/// walks -- answers a 1.5% overshoot by quartering the count, which at 5K is 26 of 851 zoom steps
/// where the backdrop thins out and comes back.
///
/// The caller strokes at [`level`]'s radius, which cannot see the viewport, so wherever this grows
/// the radius the rings sit on a lattice coarser than they are: 2% at 5K, 53% at the 8K peak, and
/// nothing at all at 4K and below. Closing that needs the level decision to take the viewport,
/// which is a change to the caller.
pub fn for_each_center(visible: &Rect, r: f32, mut emit: impl FnMut(f32, f32)) -> usize {
    let mut r = r;
    let mut band = ring_band(visible, r);
    while ring_count(band) > MAX_RINGS {
        // The square root alone can ask for a radius the band rounds straight back to the one that
        // did not fit, so every pass moves it at least 1%. Three passes is the most the sweep sees.
        let overshoot = ring_count(band) as f32 / MAX_RINGS as f32;
        r *= overshoot.sqrt().max(1.01);
        band = ring_band(visible, r);
    }
    let (c0, c1, r0, r1) = band;
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

/// Oracle-side hex count. `suppressed` folds in both `K10S_NO_HEX` and the stress modes, so this
/// stays a pure function of its arguments (see `crate::frame::FrameOpts::hex_shown`).
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
    fn count_is_viewport_bounded_and_matches_enumeration() {
        for zoom in [0.004f32, 0.1, 1.0, 10.0] {
            let (r, _) = level(zoom);
            let visible = Rect::new(-800.0 / zoom, -500.0 / zoom, 1600.0 / zoom, 1000.0 / zoom);
            let mut seen = 0usize;
            let n = for_each_center(&visible, r, |_, _| seen += 1);
            assert_eq!(n, seen);
            assert!(n > 0, "zoom {zoom}: grid empty");
            assert!(n <= MAX_RINGS, "zoom {zoom}: {n} hexes");
            // The oracle baselines are pinned at this viewport, so the clamp has no business here:
            // the grid is the whole band, ring for ring.
            assert_eq!(
                n,
                ring_count(ring_band(&visible, r)),
                "zoom {zoom}: the clamp engaged at 1600x1000"
            );
            assert_eq!(n, visible_count(&visible, zoom, false));
        }
    }

    /// Every display the app can be opened on, swept across the whole zoom range, because the ring
    /// count is a function of window size alone. A fixed cap of 1,500 rings blanked the backdrop
    /// outright -- 448 of these steps at 4K, all 851 at 8K -- and coarsening by a whole level
    /// replaced the blank with a pop: 0.28 of the budget on the 26 steps where it engaged at 5K,
    /// full density again a step later. So the sweep asks for all three at once -- a grid, inside
    /// the budget, spending what it is given. The measured worst case for the last is 0.994 of the
    /// budget, and the quarter of slack is for the band's own rounding, not for a level.
    ///
    /// What no clamp can flatten is [`level`]'s own sawtooth: at 4K, where nothing here engages, the
    /// count still steps 3.5x across a level boundary, and that jump is the alpha ramp's to hide.
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

    /// [`MAX_RINGS`] is a measurement of the gpui fork, so it has to be checked from both sides: too
    /// high and `build` drops the layer the cap was meant to protect, too low and the grid coarsens
    /// where it would have fit. Same geometry the painter emits (`frame::PaintSink::hex_ring`).
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
