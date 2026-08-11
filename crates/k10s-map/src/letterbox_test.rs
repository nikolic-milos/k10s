//! That letterboxing keeps the logical size it was asked for and centers it,
//! including the smaller-window case where the origin goes negative.

use super::*;

#[test]
fn letterbox_keeps_logical_size_and_centers() {
    let canvas = Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(1920.0), px(1200.0)),
    };
    let box_ = letterbox_bounds(canvas);
    assert_eq!(f32::from(box_.size.width), FLIGHT_VIEWPORT[0]);
    assert_eq!(f32::from(box_.size.height), FLIGHT_VIEWPORT[1]);
    assert!((f32::from(box_.origin.x) - 160.0).abs() < 1e-3);
    assert!((f32::from(box_.origin.y) - 100.0).abs() < 1e-3);
}

#[test]
fn letterbox_origin_may_go_negative_on_a_smaller_window() {
    let canvas = Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(1512.0), px(837.0)),
    };
    let box_ = letterbox_bounds(canvas);
    assert_eq!(f32::from(box_.size.width), FLIGHT_VIEWPORT[0]);
    assert_eq!(f32::from(box_.size.height), FLIGHT_VIEWPORT[1]);
    assert!(f32::from(box_.origin.x) < 0.0);
    assert!(f32::from(box_.origin.y) < 0.0);
}
