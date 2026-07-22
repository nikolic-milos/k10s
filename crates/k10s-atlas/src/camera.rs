use crate::scene::Rect;

pub const MIN_ZOOM: f32 = 0.004;
pub const MAX_ZOOM: f32 = 40.0;

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    pub cx: f32,
    pub cy: f32,
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            cx: 0.0,
            cy: 0.0,
            zoom: 0.1,
        }
    }
}

impl Camera {
    pub fn w2s(&self, x: f32, y: f32, vw: f32, vh: f32) -> (f32, f32) {
        (
            (x - self.cx) * self.zoom + vw * 0.5,
            (y - self.cy) * self.zoom + vh * 0.5,
        )
    }

    pub fn s2w(&self, sx: f32, sy: f32, vw: f32, vh: f32) -> (f32, f32) {
        (
            (sx - vw * 0.5) / self.zoom + self.cx,
            (sy - vh * 0.5) / self.zoom + self.cy,
        )
    }

    pub fn visible_world(&self, vw: f32, vh: f32) -> Rect {
        let (x0, y0) = self.s2w(0.0, 0.0, vw, vh);
        Rect::new(x0, y0, vw / self.zoom, vh / self.zoom).inflate(8.0 / self.zoom)
    }

    pub fn pan_px(&mut self, dx: f32, dy: f32) {
        self.cx -= dx / self.zoom;
        self.cy -= dy / self.zoom;
    }

    pub fn zoom_around(&mut self, factor: f32, sx: f32, sy: f32, vw: f32, vh: f32) {
        let (wx, wy) = self.s2w(sx, sy, vw, vh);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.cx = wx - (sx - vw * 0.5) / self.zoom;
        self.cy = wy - (sy - vh * 0.5) / self.zoom;
    }

    pub fn fit(&mut self, bounds: Rect, vw: f32, vh: f32) {
        if bounds.w <= 0.0 || bounds.h <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return;
        }
        self.zoom = ((vw / bounds.w).min(vh / bounds.h) * 0.94).clamp(MIN_ZOOM, MAX_ZOOM);
        let (cx, cy) = bounds.center();
        self.cx = cx;
        self.cy = cy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_keeps_cursor_point_fixed() {
        let mut cam = Camera {
            cx: 500.0,
            cy: 300.0,
            zoom: 0.5,
        };
        let (sx, sy) = (321.0, 456.0);
        let before = cam.s2w(sx, sy, 1600.0, 1000.0);
        cam.zoom_around(1.7, sx, sy, 1600.0, 1000.0);
        let after = cam.s2w(sx, sy, 1600.0, 1000.0);
        assert!((before.0 - after.0).abs() < 1e-3);
        assert!((before.1 - after.1).abs() < 1e-3);
    }

    #[test]
    fn roundtrip() {
        let cam = Camera {
            cx: 10.0,
            cy: -20.0,
            zoom: 2.0,
        };
        let (sx, sy) = cam.w2s(123.0, 456.0, 800.0, 600.0);
        let (wx, wy) = cam.s2w(sx, sy, 800.0, 600.0);
        assert!((wx - 123.0).abs() < 1e-3);
        assert!((wy - 456.0).abs() < 1e-3);
    }
}
