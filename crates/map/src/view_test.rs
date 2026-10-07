//! View fit and selection identity transitions.

use std::sync::Arc;

use k10s_core::{NsExt, NsNode, SceneIds, SceneSnapshot, Severity};

use super::{MapView, Mark, PickPath};

#[test]
fn an_empty_scene_does_not_spend_the_one_automatic_fit() {
    // The incident: a window can now open on an empty world and be filled
    // from the launch screen a moment later. Fitting to nothing marked the
    // view fitted, and the starmap that arrived opened off-camera.
    assert!(!MapView::should_fit_scene(0, false, false, false));
    assert!(!MapView::should_fit_scene(0, false, false, true));
    assert!(MapView::should_fit_scene(197, false, false, false));

    // And the rules that were already true stay true.
    assert!(!MapView::should_fit_scene(197, true, false, false));
    assert!(
        MapView::should_fit_scene(197, true, false, true),
        "a resize re-frames a camera nobody has touched"
    );
    assert!(
        !MapView::should_fit_scene(197, true, true, true),
        "and never one they have"
    );
}

fn identity_scene(rev: u64, uid: &str) -> SceneSnapshot {
    let mut snapshot = SceneSnapshot::default();
    snapshot.scene.rev = rev;
    snapshot.scene.regions.push(NsNode {
        rect: k10s_core::Rect::new(0.0, 0.0, 100.0, 100.0),
        label: Arc::from("payments"),
        weight: 1,
        children: 0..0,
        ext: NsExt {
            unhealthy_frac: 0.0,
            rollup: Severity::Ok,
        },
    });
    snapshot.ids = Arc::new(SceneIds {
        regions: vec![Arc::from(uid)].into(),
        ..SceneIds::default()
    });
    snapshot
}

#[test]
fn a_mark_follows_the_same_identity_across_publishes() {
    let path = PickPath {
        region: 0,
        block: None,
        cell: None,
        sat: None,
    };
    let mark = Mark::new(&identity_scene(7, "uid-a"), path).expect("region exists");

    assert_eq!(mark.resolve(&identity_scene(8, "uid-a")), Some(path));
}

#[test]
fn a_mark_does_not_jump_when_a_world_slot_is_reused() {
    let path = PickPath {
        region: 0,
        block: None,
        cell: None,
        sat: None,
    };
    let mark = Mark::new(&identity_scene(7, "uid-a"), path).expect("region exists");

    assert_eq!(mark.resolve(&identity_scene(8, "uid-b")), None);
}

#[test]
fn an_identityless_benchmark_mark_is_revision_scoped() {
    let path = PickPath {
        region: 0,
        block: None,
        cell: None,
        sat: None,
    };
    let mark = Mark::new(&identity_scene(7, ""), path).expect("region exists");

    assert_eq!(mark.resolve(&identity_scene(7, "")), Some(path));
    assert_eq!(mark.resolve(&identity_scene(8, "")), None);
}

#[test]
fn zoom_at_the_useful_ceiling_keeps_the_camera_and_requires_no_frame() {
    let mut camera = k10s_atlas::Camera {
        cx: 73.0,
        cy: 91.0,
        zoom: super::lod::MAX_USEFUL_ZOOM,
    };
    let before = camera;
    for _ in 0..100 {
        assert!(!super::zoom_camera(
            &mut camera,
            1.35,
            200.0,
            140.0,
            600.0,
            420.0
        ));
        assert_eq!(camera, before);
    }
    let anchor = camera.s2w(200.0, 140.0, 600.0, 420.0);
    assert!(super::zoom_camera(
        &mut camera,
        0.8,
        200.0,
        140.0,
        600.0,
        420.0
    ));
    let after = camera.s2w(200.0, 140.0, 600.0, 420.0);
    assert!((anchor.0 - after.0).abs() < 1e-4);
    assert!((anchor.1 - after.1).abs() < 1e-4);
}

#[test]
fn a_revealed_pod_clears_the_pinned_name_at_large_type_in_a_narrow_view() {
    let viewport = (600.0, 420.0);
    let inner = k10s_core::Rect::new(40.0, 40.0, 120.0, 130.0);
    let cell = k10s_core::Rect::new(50.0, 76.0, 10.0, 10.0);
    for ui_size in [8.0, 14.0, 24.0, 32.0] {
        let type_ = k10s_theme::Typography {
            ui_size,
            ..Default::default()
        }
        .map();
        let camera =
            super::reveal_camera(k10s_atlas::Camera::default(), cell, viewport, type_, true);
        assert!(camera.zoom <= super::lod::MAX_USEFUL_ZOOM);
        let projection = crate::primitive::Projection::new(camera, viewport, (0.0, 0.0));
        let heading = projection
            .name_band(inner, type_)
            .expect("the workload name fits");
        let body = projection
            .cell(cell, 3, type_, Some(heading))
            .expect("the revealed pod clears its heading");
        assert!(f32::from(body.bounds.size.width) >= crate::primitive::POD_DETAIL_MIN_WIDTH);
        assert!(body.bounds.origin.y > heading.bounds.bottom());
        assert!(
            f32::from(body.bounds.bottom())
                <= viewport.1 - crate::primitive::bottom_clearance(type_)
        );
    }
}
