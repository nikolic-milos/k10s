//! How the view asks for frames: no flight asks for none, a flight stops
//! asking on the frame it arrives, and reduced motion costs one frame instead
//! of a flight.

use super::*;
use k10s_atlas::motion::FLY_SECONDS;

fn camera(cx: f32, cy: f32, zoom: f32) -> Camera {
    Camera { cx, cy, zoom }
}

#[test]
fn no_flight_asks_for_no_frames() {
    let mut fly = None;
    let mut at = camera(1.0, 2.0, 3.0);
    assert!(!advance_flight(&mut fly, &mut at, 0.016));
    assert_eq!(
        at,
        camera(1.0, 2.0, 3.0),
        "an absent flight moved the camera"
    );
}

#[test]
fn a_flight_stops_asking_the_frame_it_arrives_on() {
    let start = camera(0.0, 0.0, 0.1);
    let target = camera(400.0, 300.0, 2.0);
    let mut at = start;
    let mut fly = Some(FlyTo::new(start, target, Motion::Animate));

    let mut frames = 0;
    while advance_flight(&mut fly, &mut at, 0.016) {
        frames += 1;
        assert!(frames < 1_000, "a {FLY_SECONDS}s flight never arrived");
        assert!(
            fly.is_some(),
            "the flight was dropped while it was still owed"
        );
    }
    assert_eq!(at, target, "the flight stopped short of where it was sent");
    assert!(
        fly.is_none(),
        "an arrived flight is still holding a frame's worth of state"
    );

    // The frames after arrival are the ones the whole shape is for: a single
    // extra request here is one idle paint per fit, and `--churn 0` measures
    // exactly zero.
    for _ in 0..600 {
        assert!(
            !advance_flight(&mut fly, &mut at, 0.016),
            "an arrived flight went on asking to be painted"
        );
    }
}

#[test]
fn reduced_motion_costs_one_frame_and_not_a_flight() {
    let target = camera(400.0, 300.0, 2.0);
    let mut at = camera(0.0, 0.0, 0.1);
    let mut fly = Some(FlyTo::new(at, target, Motion::Reduced));
    // One step, which the caller has already been asked to paint by whatever
    // started the flight, and no frame owed after it.
    assert!(!advance_flight(&mut fly, &mut at, 0.016));
    assert_eq!(at, target);
    assert!(fly.is_none());
}
