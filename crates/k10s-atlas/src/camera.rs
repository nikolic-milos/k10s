use crate::scene::Rect;

pub const MIN_ZOOM: f32 = 0.004;
pub const MAX_ZOOM: f32 = 40.0;

/// How far past the window edge the cull rect reaches, in screen pixels.
///
/// A thing whose centre is off screen can still have a stroke, a halo or a
/// label that is on it, so the rect the cull walks is the window plus a margin
/// rather than the window. Screen pixels rather than world units, because what
/// bleeds across the edge is drawn in screen space and does not grow when the
/// camera pulls back.
const VISIBLE_PAD_PX: f32 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq)]
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
        Rect::new(x0, y0, vw / self.zoom, vh / self.zoom).inflate(VISIBLE_PAD_PX / self.zoom)
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

    /// The fraction of the shorter viewport dimension a revealed object is
    /// allowed to occupy.
    ///
    /// Framing a pod the way `fit` frames a scene puts one twenty-unit cell
    /// across sixteen hundred pixels, which is a screen with a single pod on it
    /// and no answer to "where". A person who searched for a pod wants to see
    /// the pod *and* what it is part of, so the target takes a third and the rest
    /// of the screen stays context.
    pub const REVEAL_FRACTION: f32 = 1.0 / 3.0;

    /// A camera that shows `target` in context: centred, and zoomed so it spans
    /// [`Camera::REVEAL_FRACTION`] of the shorter viewport dimension.
    ///
    /// Sized against the target's *longer* side and the viewport's *shorter*
    /// one, so a wide namespace and a tall one are both wholly on screen rather
    /// than one of them being cropped by the axis nobody checked.
    ///
    /// A target with extent on only one axis is still revealed: a zero-width
    /// namespace has a place and a height, and the degenerate scenes this engine
    /// is swept against contain exactly that. What is refused is a target with no
    /// extent at all, or one whose coordinates came back non-finite -- there is
    /// no zoom that shows a point, and flying to one would take a person away
    /// from where they were to look at nothing.
    pub fn reveal(&self, target: Rect, vw: f32, vh: f32) -> Camera {
        let span = target.w.max(target.h);
        let viewport = vw.min(vh);
        let (cx, cy) = target.center();
        if !(span.is_finite() && span > 0.0)
            || !(viewport.is_finite() && viewport > 0.0)
            || !cx.is_finite()
            || !cy.is_finite()
        {
            return *self;
        }
        Camera {
            cx,
            cy,
            zoom: (viewport * Camera::REVEAL_FRACTION / span).clamp(MIN_ZOOM, MAX_ZOOM),
        }
    }

    /// Frame the whole of `bounds`, or leave the camera where it is.
    ///
    /// Refused for the same reasons [`Camera::reveal`] refuses: a scene with no
    /// extent, a viewport nobody has laid out yet, or a coordinate that came
    /// back non-finite. Clamping a NaN yields a NaN, so a fit that accepted one
    /// would not leave a bad camera behind -- it would leave every later
    /// `visible_world` an empty rect and the screen blank with nothing to show
    /// which frame it went wrong on.
    pub fn fit(&mut self, bounds: Rect, vw: f32, vh: f32) {
        let (cx, cy) = bounds.center();
        if fit_refused(bounds, vw, vh, cx, cy) {
            return;
        }
        self.zoom = ((vw / bounds.w).min(vh / bounds.h) * 0.94).clamp(MIN_ZOOM, MAX_ZOOM);
        self.cx = cx;
        self.cy = cy;
    }

    /// The same camera with a zoom that can be divided by.
    ///
    /// Every mutator here already keeps zoom inside the range, but the field is
    /// public and a camera can also arrive from outside this crate -- a flight
    /// segment, a saved view, a planner. Anything that interpolates or divides
    /// by a zoom takes it through here first, because a zero or a NaN does not
    /// stop at the frame that produced it: it travels into `visible_world` and
    /// silently culls the whole scene.
    #[inline]
    pub fn clamped(self) -> Camera {
        Camera {
            zoom: if self.zoom.is_finite() {
                self.zoom.clamp(MIN_ZOOM, MAX_ZOOM)
            } else {
                MIN_ZOOM
            },
            ..self
        }
    }
}

/// The refusal half of [`Camera::fit`], kept out of line so the guard's eight
/// comparisons do not sit in the crate's hot text between the walks the frame
/// budget measures; `fit` runs at most once per publish or flight, never per
/// frame, and a call is nothing there.
#[cold]
#[inline(never)]
fn fit_refused(bounds: Rect, vw: f32, vh: f32, cx: f32, cy: f32) -> bool {
    !(bounds.w.is_finite() && bounds.w > 0.0)
        || !(bounds.h.is_finite() && bounds.h > 0.0)
        || !(vw.is_finite() && vw > 0.0)
        || !(vh.is_finite() && vh > 0.0)
        || !cx.is_finite()
        || !cy.is_finite()
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

#[cfg(test)]
mod reveal_tests {
    use super::*;

    const VW: f32 = 1600.0;
    const VH: f32 = 1000.0;

    fn at(zoom: f32) -> Camera {
        Camera {
            cx: -9_999.0,
            cy: 9_999.0,
            zoom,
        }
    }

    fn on_screen(camera: &Camera, target: Rect) -> bool {
        camera.visible_world(VW, VH).contains(&target)
    }

    #[test]
    fn revealing_a_pod_shows_what_it_is_part_of_rather_than_only_the_pod() {
        let pod = Rect::new(1_000.0, 500.0, 20.0, 20.0);
        let camera = at(0.05).reveal(pod, VW, VH);
        assert_eq!((camera.cx, camera.cy), pod.center());
        let across = pod.w * camera.zoom;
        assert!(
            across < VH * 0.5,
            "the pod took {across} px of a {VH} px viewport, which is a screen with \
             one pod on it"
        );
        assert!(
            across > 40.0,
            "the pod is {across} px across, which is not revealed"
        );
        assert!(on_screen(&camera, pod));
    }

    #[test]
    fn revealing_a_namespace_puts_the_whole_of_it_on_screen_on_both_axes() {
        // Wider than the viewport's aspect and taller than it, in turn: whichever
        // axis is unlucky is the one a sloppy fit crops.
        for target in [
            Rect::new(0.0, 0.0, 4_000.0, 300.0),
            Rect::new(0.0, 0.0, 300.0, 4_000.0),
            Rect::new(-2_000.0, -2_000.0, 4_000.0, 4_000.0),
        ] {
            let camera = at(20.0).reveal(target, VW, VH);
            assert!(
                on_screen(&camera, target),
                "{target:?} was cropped by the reveal at zoom {}",
                camera.zoom
            );
        }
    }

    #[test]
    fn a_reveal_stays_inside_the_zoom_range_at_both_extremes() {
        let speck = at(1.0).reveal(Rect::new(0.0, 0.0, 1e-6, 1e-6), VW, VH);
        assert_eq!(speck.zoom, MAX_ZOOM);
        let everything = at(1.0).reveal(Rect::new(0.0, 0.0, 1e9, 1e9), VW, VH);
        assert_eq!(everything.zoom, MIN_ZOOM);
    }

    #[test]
    fn a_target_with_no_size_leaves_the_camera_alone_instead_of_dividing_by_it() {
        let before = at(2.5);
        for degenerate in [
            Rect::new(10.0, 10.0, 0.0, 0.0),
            Rect::new(f32::NAN, 0.0, 10.0, 10.0),
            Rect::new(0.0, 0.0, f32::INFINITY, 10.0),
        ] {
            let after = before.reveal(degenerate, VW, VH);
            assert_eq!(after, before, "{degenerate:?} moved the camera");
        }
        // And a viewport that has not been laid out yet is the same case.
        assert_eq!(
            before.reveal(Rect::new(0.0, 0.0, 10.0, 10.0), 0.0, 0.0),
            before
        );
    }

    #[test]
    fn revealing_the_same_thing_twice_lands_in_the_same_place() {
        let target = Rect::new(123.0, -45.0, 60.0, 90.0);
        let once = at(0.1).reveal(target, VW, VH);
        assert_eq!(
            once.reveal(target, VW, VH),
            once,
            "a reveal drifted on repeat"
        );
    }

    #[test]
    fn a_fit_of_nonsense_bounds_leaves_the_camera_where_it_was() {
        let before = Camera {
            cx: 12.0,
            cy: -34.0,
            zoom: 0.5,
        };
        for (bounds, vw, vh) in [
            (Rect::new(0.0, 0.0, f32::NAN, 100.0), 1600.0, 1000.0),
            (Rect::new(0.0, 0.0, 100.0, f32::NAN), 1600.0, 1000.0),
            (Rect::new(f32::NAN, 0.0, 100.0, 100.0), 1600.0, 1000.0),
            (Rect::new(0.0, f32::INFINITY, 100.0, 100.0), 1600.0, 1000.0),
            (Rect::new(0.0, 0.0, f32::INFINITY, 100.0), 1600.0, 1000.0),
            (Rect::new(0.0, 0.0, 100.0, 100.0), f32::NAN, 1000.0),
            (Rect::new(0.0, 0.0, 100.0, 100.0), 1600.0, f32::INFINITY),
        ] {
            let mut cam = before;
            cam.fit(bounds, vw, vh);
            assert_eq!(cam, before, "{bounds:?} at {vw}x{vh} moved the camera");
        }
    }

    #[test]
    fn a_clamped_camera_can_always_be_divided_by() {
        for zoom in [0.0, -1.0, f32::NAN, f32::INFINITY, 1e9, MIN_ZOOM * 0.5] {
            let clamped = Camera {
                cx: 1.0,
                cy: 2.0,
                zoom,
            }
            .clamped();
            assert!(
                (MIN_ZOOM..=MAX_ZOOM).contains(&clamped.zoom),
                "a zoom of {zoom} stayed outside the range"
            );
            assert_eq!((clamped.cx, clamped.cy), (1.0, 2.0));
        }
        let sane = Camera {
            cx: 0.0,
            cy: 0.0,
            zoom: 1.5,
        };
        assert_eq!(sane.clamped(), sane, "a usable camera was changed");
    }
}
