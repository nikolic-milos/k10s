//! Camera movement that plays out over time instead of teleporting.
//!
//! Time is handed in rather than read. This crate owns no clock, which is what
//! lets a scripted flight replay a fly-to exactly and a test assert where the
//! camera is forty milliseconds in without waiting forty milliseconds for it.
//!
//! The two facts a caller needs from a step are where the camera is and whether
//! another frame is owed, and they arrive as one value rather than as a position
//! plus a flag. That is what keeps `--churn 0`'s measured zero paints at idle
//! true through an animation: the frame after the last one is only requested
//! while [`Step::Moving`] says so, and arriving is a state rather than a
//! threshold somebody has to test for.

use crate::camera::Camera;

/// Whether this window may animate at all.
///
/// Not a `bool`, because the two answers are read at the point where a person
/// asked for something and a shape that carries the reason is the one a settings
/// surface can explain. Reduced does not mean "do not move" -- the camera still
/// has to arrive, and the frame that shows it arriving still has to be painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Animate,
    /// The platform or the user asked for no motion (WCAG 2.3.3, and every
    /// desktop's own switch). Every flight completes on its first step.
    Reduced,
}

impl Motion {
    /// From the host's own switch.
    ///
    /// There is no platform query anywhere in this stack -- gpui carries a
    /// `reduce_motion` flag because an application *sets* it, and Zed sets it
    /// from a settings file rather than from GNOME, macOS or Windows. So "the
    /// system asked for less motion" is a sentence this product can only say by
    /// being told, and the constructor takes the answer rather than looking for
    /// it.
    pub fn reduced_when(reduced: bool) -> Motion {
        if reduced {
            Motion::Reduced
        } else {
            Motion::Animate
        }
    }
}

/// Where a flight is after a step, and whether it owes another frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Mid-flight. The caller paints this camera and must request another frame.
    Moving(Camera),
    /// The target, exactly, and nothing further is owed. Distinguishing this
    /// from `Moving` at the target is the whole point: a repaint requested one
    /// frame past the end is an idle paint, and this crate's headline
    /// measurement is that there are none.
    Arrived(Camera),
}

impl Step {
    pub fn camera(self) -> Camera {
        match self {
            Step::Moving(camera) | Step::Arrived(camera) => camera,
        }
    }

    /// Whether the caller must ask for another frame.
    ///
    /// Asking the step rather than comparing the camera to the target is the
    /// point of the enum. Two cameras a millionth of a unit apart compare
    /// unequal forever, so an animation that decides it has arrived by measuring
    /// how close it got is an animation that paints for the rest of the session
    /// -- and this crate's headline measurement is that idle paints are zero.
    pub fn owes_a_frame(self) -> bool {
        matches!(self, Step::Moving(_))
    }
}

/// How long a fly-to takes when motion is allowed, in seconds.
///
/// Long enough to read as movement -- which is the point, since a camera that
/// jumps leaves a person to work out for themselves that the thing they are
/// looking at is somewhere else -- and short enough that it never becomes the
/// thing being waited on.
pub const FLY_SECONDS: f32 = 0.35;

/// A camera travelling from where it was to where it was asked to go.
#[derive(Debug, Clone, Copy)]
pub struct FlyTo {
    from: Camera,
    to: Camera,
    elapsed: f32,
    duration: f32,
}

impl FlyTo {
    /// Under [`Motion::Reduced`] the duration is zero, so the first step arrives.
    /// Zero rather than absent, because the caller's loop must not have to know
    /// which it is holding.
    ///
    /// Both ends are brought inside the zoom range, not just the destination.
    /// The source is whatever camera the caller was holding, and the zoom lerp
    /// is logarithmic: a zero or negative source zoom makes `ln` infinite and
    /// every sample between here and arrival NaN, which is a flight that paints
    /// nothing and only recovers on the frame it lands.
    pub fn new(from: Camera, to: Camera, motion: Motion) -> FlyTo {
        FlyTo {
            from: from.clamped(),
            to: to.clamped(),
            elapsed: 0.0,
            duration: match motion {
                Motion::Animate => FLY_SECONDS,
                Motion::Reduced => 0.0,
            },
        }
    }

    /// Aim somewhere else without stopping first.
    ///
    /// The new flight starts from where the camera *is*, not from where the
    /// previous one started, so a second search result while the first is still
    /// flying continues the movement rather than snapping back and setting off
    /// again.
    pub fn retarget(&mut self, to: Camera, motion: Motion) {
        *self = FlyTo::new(self.at(progress(self.elapsed, self.duration)), to, motion);
    }

    /// Advance by `dt` seconds and say where that leaves the camera.
    ///
    /// A `dt` that is negative, NaN, or infinite advances nothing: a frame delta
    /// is measured, and a measurement that came back nonsense must not be able to
    /// move the camera anywhere, least of all to a coordinate no later frame can
    /// recover from. A very large one is *not* clamped, and that is a decision
    /// rather than an oversight -- after the process was stopped for a second,
    /// the honest place for the camera is where it was going.
    pub fn step(&mut self, dt: f32) -> Step {
        if dt.is_finite() && dt > 0.0 {
            self.elapsed += dt;
        }
        let t = progress(self.elapsed, self.duration);
        if t >= 1.0 {
            // The target itself, never an interpolation that landed on it. A lerp
            // at t == 1.0 is `to` in exact arithmetic and need not be in this
            // one, and a camera a millionth of a unit short of a saved view is a
            // saved view that does not round-trip.
            Step::Arrived(self.to)
        } else {
            Step::Moving(self.at(t))
        }
    }

    /// Where the flight is at a normalised time, eased.
    fn at(&self, t: f32) -> Camera {
        lerp_camera(self.from, self.to, ease_in_out(t))
    }
}

/// A camera part way from `a` to `b`.
///
/// Zoom is a scale factor, so it travels geometrically. Interpolated linearly,
/// a flight from 0.05 to 24 spends its first half crossing a hundredth of the
/// zoom range and its second half crossing the rest, which reads as a lurch at
/// the end rather than as approach. Halfway through a flight should be halfway
/// through the zoom in the sense zoom is actually used, which is multiplicative.
///
/// Both the interactive fly-to and the scripted flight sample cameras this way,
/// and they share this function rather than each keeping the rule, because a
/// recording that moved differently from the product would be measuring
/// something nobody ever sees. Endpoints must already be inside the zoom range
/// -- see [`Camera::clamped`], which is what makes the logarithm defined.
#[inline]
pub(crate) fn lerp_camera(a: Camera, b: Camera, t: f32) -> Camera {
    Camera {
        cx: lerp(a.cx, b.cx, t),
        cy: lerp(a.cy, b.cy, t),
        zoom: (a.zoom.ln() + (b.zoom.ln() - a.zoom.ln()) * t).exp(),
    }
}

fn progress(elapsed: f32, duration: f32) -> f32 {
    if duration <= 0.0 {
        1.0
    } else {
        (elapsed / duration).clamp(0.0, 1.0)
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Symmetric ease, zero slope at both ends, exactly 0.5 at the midpoint.
fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        2.0 * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(2) * 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{MAX_ZOOM, MIN_ZOOM};

    fn camera(cx: f32, cy: f32, zoom: f32) -> Camera {
        Camera { cx, cy, zoom }
    }

    #[test]
    fn a_flight_ends_on_its_target_exactly_rather_than_near_it() {
        let to = camera(1234.5, -678.25, 3.75);
        let mut fly = FlyTo::new(camera(0.0, 0.0, 0.1), to, Motion::Animate);
        let mut step = fly.step(FLY_SECONDS * 0.5);
        assert!(step.owes_a_frame());
        step = fly.step(FLY_SECONDS);
        assert_eq!(step, Step::Arrived(to));
        let arrived = step.camera();
        assert_eq!(
            (arrived.cx, arrived.cy, arrived.zoom),
            (to.cx, to.cy, to.zoom)
        );
    }

    #[test]
    fn nothing_is_owed_after_arrival_which_is_what_keeps_idle_at_zero_paints() {
        let mut fly = FlyTo::new(
            camera(0.0, 0.0, 1.0),
            camera(10.0, 10.0, 2.0),
            Motion::Animate,
        );
        // Bounded, because a flight that never arrives has to fail rather than
        // hang: an unbounded loop here turns "this animation paints forever"
        // into "the test runner timed out", which is a worse sentence and
        // arrives much later. Found by mutating `owes_a_frame` to always say
        // yes, which spun this test for ten minutes instead of failing it.
        let mut frames = 0;
        while fly.step(0.016).owes_a_frame() {
            frames += 1;
            assert!(frames < 1_000, "a {FLY_SECONDS}s flight never arrived");
        }
        for _ in 0..600 {
            assert!(
                !fly.step(0.016).owes_a_frame(),
                "a finished flight went on asking to be painted"
            );
        }
    }

    #[test]
    fn reduced_motion_arrives_on_the_first_step_and_still_paints_that_frame() {
        let to = camera(500.0, 500.0, 8.0);
        let mut fly = FlyTo::new(camera(0.0, 0.0, 0.1), to, Motion::Reduced);
        // The first step arrives rather than the constructor doing so: the caller
        // still has to paint the frame that shows the camera somewhere new.
        let step = fly.step(0.016);
        assert_eq!(step, Step::Arrived(to));
        assert!(!step.owes_a_frame());
    }

    #[test]
    fn zoom_travels_geometrically_so_the_middle_of_a_flight_is_the_middle_of_the_zoom() {
        let (from, to) = (0.05_f32, 20.0_f32);
        let mut fly = FlyTo::new(
            camera(0.0, 0.0, from),
            camera(0.0, 0.0, to),
            Motion::Animate,
        );
        let half = fly.step(FLY_SECONDS * 0.5).camera().zoom;
        let geometric = (from * to).sqrt();
        assert!(
            (half / geometric - 1.0).abs() < 1e-3,
            "halfway through the flight the zoom was {half}, not the geometric mean {geometric}"
        );
        // Which is nowhere near where a linear interpolation would have left it,
        // or the assertion above would be satisfied by the bug it is written for.
        let arithmetic = (from + to) * 0.5;
        assert!((half / arithmetic - 1.0).abs() > 0.5);
    }

    #[test]
    fn a_stalled_frame_arrives_instead_of_overshooting() {
        let to = camera(1.0, 2.0, 4.0);
        let mut fly = FlyTo::new(camera(0.0, 0.0, 0.5), to, Motion::Animate);
        assert_eq!(fly.step(90.0), Step::Arrived(to));
    }

    #[test]
    fn a_delta_that_came_back_nonsense_moves_nothing() {
        let start = camera(0.0, 0.0, 1.0);
        let mut fly = FlyTo::new(start, camera(100.0, 100.0, 4.0), Motion::Animate);
        for bad in [-1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.0] {
            let at = fly.step(bad).camera();
            assert_eq!(
                (at.cx, at.cy, at.zoom),
                (start.cx, start.cy, start.zoom),
                "a dt of {bad} moved the camera"
            );
        }
        // Not moving is only half of it. A bad delta that quietly accumulated
        // would leave the flight owing time it never spent -- the camera would
        // sit still and then take a second longer than it was asked to, which is
        // invisible in a position check and obvious to a person. So the clock
        // has to be unpoisoned too: one full duration of real time from here must
        // still arrive.
        assert!(fly.step(FLY_SECONDS * 0.5).owes_a_frame());
        assert!(
            !fly.step(FLY_SECONDS * 0.5).owes_a_frame(),
            "the flight owed time that a nonsense delta had added to its clock"
        );
    }

    #[test]
    fn retargeting_mid_flight_carries_on_from_where_the_camera_is() {
        let mut fly = FlyTo::new(
            camera(0.0, 0.0, 1.0),
            camera(100.0, 0.0, 1.0),
            Motion::Animate,
        );
        let midway = fly.step(FLY_SECONDS * 0.5).camera();
        assert!(midway.cx > 0.0 && midway.cx < 100.0);

        fly.retarget(camera(-100.0, 0.0, 1.0), Motion::Animate);
        let after = fly.step(0.0).camera();
        assert_eq!(
            after.cx, midway.cx,
            "retargeting snapped back instead of continuing from here"
        );
        // And it goes the other way now rather than finishing the old flight.
        assert!(fly.step(FLY_SECONDS * 0.25).camera().cx < midway.cx);
    }

    #[test]
    fn a_target_outside_the_zoom_range_is_brought_inside_it_before_the_flight() {
        let mut fly = FlyTo::new(
            camera(0.0, 0.0, 1.0),
            camera(0.0, 0.0, 1e9),
            Motion::Animate,
        );
        let arrived = fly.step(FLY_SECONDS).camera();
        assert_eq!(arrived.zoom, MAX_ZOOM);
        let mut under = FlyTo::new(
            camera(0.0, 0.0, 1.0),
            camera(0.0, 0.0, 0.0),
            Motion::Animate,
        );
        assert_eq!(under.step(FLY_SECONDS).camera().zoom, MIN_ZOOM);
    }

    #[test]
    fn a_flight_that_starts_from_an_impossible_zoom_still_flies() {
        let to = camera(100.0, 100.0, 4.0);
        for bad in [0.0, -2.0, f32::NAN, f32::INFINITY, 1e12] {
            let mut fly = FlyTo::new(camera(0.0, 0.0, bad), to, Motion::Animate);
            for frame in 0..200 {
                let step = fly.step(0.016);
                let at = step.camera();
                assert!(
                    at.cx.is_finite() && at.cy.is_finite() && at.zoom.is_finite(),
                    "a flight from a zoom of {bad} reached {at:?} on frame {frame}"
                );
                assert!(
                    (MIN_ZOOM..=MAX_ZOOM).contains(&at.zoom),
                    "a flight from a zoom of {bad} left the range at {}",
                    at.zoom
                );
                if !step.owes_a_frame() {
                    break;
                }
            }
            assert_eq!(fly.step(FLY_SECONDS), Step::Arrived(to));
        }
    }

    #[test]
    fn the_ease_is_symmetric_and_pins_both_ends() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
        for i in 0..=50 {
            let t = i as f32 / 50.0;
            assert!(
                (ease_in_out(t) + ease_in_out(1.0 - t) - 1.0).abs() < 1e-5,
                "the ease is not symmetric at {t}"
            );
        }
        // Monotone, or the camera would visibly back up mid-flight.
        let mut previous = -1.0;
        for i in 0..=100 {
            let e = ease_in_out(i as f32 / 100.0);
            assert!(e >= previous, "the ease went backwards at {i}");
            previous = e;
        }
    }

    #[test]
    fn every_frame_of_a_flight_is_finite_and_inside_the_zoom_range() {
        let mut fly = FlyTo::new(
            camera(-9_000.0, 12_345.0, MIN_ZOOM),
            camera(50_000.0, -3.0, MAX_ZOOM),
            Motion::Animate,
        );
        for frame in 0..10_000 {
            let step = fly.step(0.004);
            let at = step.camera();
            assert!(at.cx.is_finite() && at.cy.is_finite() && at.zoom.is_finite());
            assert!(
                (MIN_ZOOM..=MAX_ZOOM).contains(&at.zoom),
                "zoom left its range"
            );
            if !step.owes_a_frame() {
                return;
            }
            assert!(frame < 9_999, "a {FLY_SECONDS}s flight never arrived");
        }
    }
}
