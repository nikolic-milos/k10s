use k10s_atlas::Rect;

const BASE_R: f32 = 48.0;
const MIN_PX: f32 = 72.0;

const MAX_HEXES: usize = 1500;

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

pub fn for_each_center(visible: &Rect, r: f32, mut emit: impl FnMut(f32, f32)) -> usize {
    let col_pitch = 1.5 * r;
    let row_pitch = 3.0f32.sqrt() * r;
    let c0 = ((visible.x - r) / col_pitch).floor() as i64;
    let c1 = ((visible.max_x() + r) / col_pitch).ceil() as i64;
    let r0 = ((visible.y - row_pitch) / row_pitch).floor() as i64;
    let r1 = ((visible.max_y() + row_pitch) / row_pitch).ceil() as i64;
    let cols = (c1 - c0 + 1).max(0) as usize;
    let rows = (r1 - r0 + 1).max(0) as usize;
    if cols * rows > MAX_HEXES {
        return 0;
    }
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
    if suppressed || !hex_on() {
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
            assert!(n <= MAX_HEXES, "zoom {zoom}: {n} hexes");
            assert_eq!(n, visible_count(&visible, zoom, false));
        }
    }

    #[test]
    fn suppressed_counts_zero() {
        let visible = Rect::new(0.0, 0.0, 1600.0, 1000.0);
        assert_eq!(visible_count(&visible, 1.0, true), 0);
    }
}
