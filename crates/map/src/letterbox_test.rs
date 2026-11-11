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

#[test]
fn device_px_ignores_subpixel_noise_and_a_bad_scale() {
    let a = k10s_core::Rect::new(10.0, 20.0, 1600.0, 1000.0);
    let b = k10s_core::Rect::new(10.2, 20.4, 1600.1, 1000.3);
    assert_eq!(device_px(a, 1.0), device_px(b, 1.0));
    assert_ne!(
        device_px(a, 1.0),
        device_px(k10s_core::Rect::new(11.0, 20.0, 1600.0, 1000.0), 1.0)
    );
    assert_eq!(device_px(a, 0.0), device_px(a, 1.0));
    assert_eq!(device_px(a, f32::NAN), device_px(a, 1.0));
}

#[test]
fn snap_to_device_keeps_a_ladder_size_across_subpixel_origins() {
    let scale = 1.5;
    let size_px = 16.0;
    let left = Bounds {
        origin: point(px(10.4), px(20.2)),
        size: size(px(size_px), px(size_px)),
    };
    let right = Bounds {
        origin: point(px(40.7), px(80.9)),
        size: size(px(size_px), px(size_px)),
    };
    let a = snap_to_device(left, scale);
    let b = snap_to_device(right, scale);
    assert_eq!(f32::from(a.size.width), f32::from(b.size.width));
    assert_eq!(f32::from(a.size.height), f32::from(b.size.height));
    let device_w = (f32::from(a.size.width) * scale).round();
    let device_h = (f32::from(a.size.height) * scale).round();
    assert_eq!(device_w, (size_px * scale).round());
    assert_eq!(device_h, (size_px * scale).round());
    assert_eq!(snap_to_device(left, 0.0), left);
    assert_eq!(snap_to_device(left, f32::NAN), left);
}
