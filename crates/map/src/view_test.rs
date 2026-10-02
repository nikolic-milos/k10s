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
