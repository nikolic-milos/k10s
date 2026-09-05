//! Visibility over a tiny snapshot: the bitsets must track the filter without
//! cloning scene nodes.

use std::sync::Arc;

use k10s_core::{
    KindId, NsExt, NsNode, PodExt, PodNode, Rect, SatExt, SatNode, SceneSnapshot, Severity, State,
    ToolId, WlExt, WorkloadNode,
};

use crate::{HealthFilter, SceneFilter, filter_scene};

fn tiny() -> SceneSnapshot {
    let mut snap = SceneSnapshot::default();
    snap.scene.regions.extend([
        NsNode {
            rect: Rect::new(0.0, 0.0, 400.0, 300.0),
            label: Arc::from("prod"),
            weight: 3,
            children: 0..2,
            ext: NsExt {
                unhealthy_frac: 0.5,
                rollup: Severity::Err,
            },
        },
        NsNode {
            rect: Rect::new(500.0, 0.0, 200.0, 200.0),
            label: Arc::from("staging"),
            weight: 1,
            children: 2..3,
            ext: NsExt {
                unhealthy_frac: 1.0,
                rollup: Severity::Warn,
            },
        },
    ]);
    snap.scene.blocks.extend([
        WorkloadNode {
            rect: Rect::new(10.0, 10.0, 120.0, 90.0),
            inner: Rect::new(14.0, 14.0, 112.0, 82.0),
            label: Arc::from("api"),
            children: 0..2,
            sats: 0..1,
            ext: WlExt {
                kind: KindId::DEPLOYMENT,
                tool: ToolId::NONE,
                rollup: Severity::Err,
                ns: 0,
            },
        },
        WorkloadNode {
            rect: Rect::new(140.0, 10.0, 80.0, 80.0),
            inner: Rect::new(144.0, 14.0, 72.0, 72.0),
            label: Arc::from("db"),
            children: 2..3,
            sats: 0..0,
            ext: WlExt {
                kind: KindId::STATEFUL_SET,
                tool: ToolId::NONE,
                rollup: Severity::Ok,
                ns: 0,
            },
        },
        WorkloadNode {
            rect: Rect::new(510.0, 10.0, 80.0, 80.0),
            inner: Rect::new(514.0, 14.0, 72.0, 72.0),
            label: Arc::from("web"),
            children: 3..4,
            sats: 0..0,
            ext: WlExt {
                kind: KindId::DEPLOYMENT,
                tool: ToolId::NONE,
                rollup: Severity::Warn,
                ns: 1,
            },
        },
    ]);
    snap.scene.cells.extend([
        PodNode {
            rect: Rect::new(20.0, 30.0, 16.0, 16.0),
            label: Arc::from("api-0"),
            ext: PodExt { state: State::OK },
        },
        PodNode {
            rect: Rect::new(40.0, 30.0, 16.0, 16.0),
            label: Arc::from("api-1"),
            ext: PodExt {
                state: State::of(k10s_core::ReasonId::CRASH_LOOP_BACK_OFF),
            },
        },
        PodNode {
            rect: Rect::new(150.0, 30.0, 16.0, 16.0),
            label: Arc::from("db-0"),
            ext: PodExt { state: State::OK },
        },
        PodNode {
            rect: Rect::new(520.0, 30.0, 16.0, 16.0),
            label: Arc::from("web-0"),
            ext: PodExt {
                state: State::of(k10s_core::ReasonId::PENDING),
            },
        },
    ]);
    snap.scene.sats.push(SatNode {
        rect: Rect::new(130.0, 40.0, 12.0, 12.0),
        label: Arc::from("svc"),
        ext: SatExt {
            kind: KindId::SERVICE,
            detail: Arc::from("ClusterIP"),
        },
    });
    snap
}

fn open() -> SceneFilter {
    SceneFilter::default()
}

#[test]
fn an_empty_filter_marks_every_slot() {
    let snap = tiny();
    let vis = filter_scene(&snap, &open());
    assert!(vis.regions.contains(0) && vis.regions.contains(1));
    assert!(vis.blocks.contains(0) && vis.blocks.contains(2));
    assert!(vis.cells.contains(0) && vis.cells.contains(3));
    assert!(vis.sats.contains(0));
}

#[test]
fn a_namespace_filter_hides_the_other_island() {
    let snap = tiny();
    let vis = filter_scene(
        &snap,
        &SceneFilter {
            namespaces: vec!["prod".into()],
            ..open()
        },
    );
    assert!(vis.regions.contains(0));
    assert!(!vis.regions.contains(1));
    assert!(vis.blocks.contains(0) && vis.blocks.contains(1));
    assert!(!vis.blocks.contains(2));
    assert!(vis.cells.contains(0) && vis.cells.contains(2));
    assert!(!vis.cells.contains(3));
}

#[test]
fn unhealthy_keeps_the_crashing_pod_and_its_ancestors() {
    let snap = tiny();
    let vis = filter_scene(
        &snap,
        &SceneFilter {
            health: Some(HealthFilter::Unhealthy),
            ..open()
        },
    );
    assert!(vis.cells.contains(1), "api-1 is CrashLoopBackOff");
    assert!(vis.cells.contains(3), "web-0 is Pending");
    assert!(!vis.cells.contains(0));
    assert!(!vis.cells.contains(2));
    assert!(vis.blocks.contains(0) && vis.regions.contains(0));
    assert!(vis.blocks.contains(2) && vis.regions.contains(1));
}

#[test]
fn a_kind_filter_matches_slug_or_short_and_keeps_ancestors() {
    let snap = tiny();
    let vis = filter_scene(
        &snap,
        &SceneFilter {
            kinds: vec!["svc".into()],
            ..open()
        },
    );
    assert!(vis.sats.contains(0));
    assert!(vis.blocks.contains(0) && vis.regions.contains(0));
    assert!(!vis.cells.contains(0));
    assert!(!vis.blocks.contains(1));

    let pods = filter_scene(
        &snap,
        &SceneFilter {
            kinds: vec!["pod".into()],
            ..open()
        },
    );
    assert!(pods.cells.contains(0) && pods.cells.contains(3));
    assert!(pods.blocks.contains(0) && pods.regions.contains(1));
}

#[test]
fn a_deployment_kind_still_shows_the_pods_inside_the_card() {
    let snap = tiny();
    let vis = filter_scene(
        &snap,
        &SceneFilter {
            kinds: vec!["deployment".into()],
            ..open()
        },
    );
    assert!(vis.blocks.contains(0) && vis.blocks.contains(2));
    assert!(!vis.blocks.contains(1), "db is a StatefulSet");
    assert!(vis.cells.contains(0) && vis.cells.contains(1));
    assert!(!vis.cells.contains(2));
    assert!(vis.cells.contains(3));
}

#[test]
fn kubernetes_labels_are_not_on_the_snapshot_so_a_selector_does_not_hide() {
    let snap = tiny();
    let open_vis = filter_scene(&snap, &open());
    let labeled = filter_scene(
        &snap,
        &SceneFilter {
            labels: vec![("app".into(), "api".into())],
            ..open()
        },
    );
    assert_eq!(
        labeled, open_vis,
        "SceneSnapshot has no pod labels; the selector cannot be evaluated"
    );
}

#[test]
fn filter_scene_does_not_clone_the_snapshot_nodes() {
    let snap = tiny();
    let before = snap.cells.len();
    let vis = filter_scene(
        &snap,
        &SceneFilter {
            namespaces: vec!["prod".into()],
            kinds: vec!["pod".into()],
            health: Some(HealthFilter::Healthy),
            ..open()
        },
    );
    assert_eq!(snap.cells.len(), before, "the snapshot is borrowed");
    assert!(vis.cells.contains(0));
    assert!(!vis.cells.contains(1), "api-1 is unhealthy");
    assert!(vis.cells.contains(2));
    assert!(!vis.cells.contains(3), "staging is filtered out");
}
