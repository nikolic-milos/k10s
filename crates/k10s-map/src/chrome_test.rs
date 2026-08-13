use super::*;

#[test]
fn semantic_bands_follow_the_shipping_lod_thresholds() {
    let policy = crate::lod::policy(Default::default());
    assert_eq!(
        detail_band(&policy, policy.stage_block * 0.5),
        DetailBand::Orbit
    );
    assert_eq!(detail_band(&policy, policy.stage_block), DetailBand::Region);
    assert_eq!(detail_band(&policy, policy.stage_cell), DetailBand::System);
    assert_eq!(
        detail_band(&policy, policy.stage_cell_label),
        DetailBand::Instance
    );
}

#[test]
fn overlay_density_never_allows_full_chrome_to_collide_in_a_tiny_view() {
    assert_eq!(density(320.0, 200.0), Density::Minimal);
    assert_eq!(density(700.0, 480.0), Density::Compact);
    assert_eq!(density(1280.0, 720.0), Density::Full);
}

#[test]
fn summary_text_is_reused_until_counts_change() {
    let mut cache = SummaryCache::default();
    let totals = Totals {
        regions: 52,
        blocks: 656,
        cells: 2_778,
        sats: 1_517,
        edges: 419,
    };
    let first = cache.line(totals);
    let second = cache.line(totals);
    assert_eq!(first, second);
    assert_eq!(
        first.as_ref(),
        "52 namespaces  ·  656 workloads  ·  2,778 pods"
    );

    let changed = cache.line(Totals {
        cells: 3_000,
        ..totals
    });
    assert!(changed.ends_with("3,000 pods"));
}

#[test]
fn identical_chrome_state_does_not_dirty_its_reactive_boundary() {
    let mut chrome = Chrome::default();
    let state = State {
        summary: "1 namespace  ·  2 workloads  ·  3 pods".into(),
        band: DetailBand::System,
        density: Density::Full,
        hover: None,
        edges_on: false,
        legend_on: true,
        empty: false,
    };

    assert!(chrome.replace(state.clone()));
    assert!(!chrome.replace(state));
}

fn hover_scene() -> SceneSnapshot {
    let mut scene = SceneSnapshot::default();
    scene.regions.push(k10s_core::NsNode {
        rect: k10s_core::Rect::new(0.0, 0.0, 200.0, 120.0),
        label: "payments".into(),
        weight: 1,
        children: 0..1,
        ext: k10s_core::NsExt {
            unhealthy_frac: 0.5,
            rollup: Severity::Warn,
        },
    });
    scene.blocks.push(k10s_core::WorkloadNode {
        rect: k10s_core::Rect::new(10.0, 10.0, 80.0, 80.0),
        inner: k10s_core::Rect::new(10.0, 10.0, 80.0, 80.0),
        label: "checkout-api".into(),
        children: 0..1,
        sats: 0..1,
        ext: k10s_core::WlExt {
            kind: k10s_core::KindId::DEPLOYMENT,
            tool: k10s_core::ToolId::NONE,
            rollup: Severity::Err,
            ns: 0,
        },
    });
    scene.cells.push(k10s_core::PodNode {
        rect: k10s_core::Rect::new(12.0, 12.0, 20.0, 20.0),
        label: "checkout-api-7f9c8".into(),
        ext: k10s_core::PodExt {
            state: k10s_core::State::of(k10s_core::ReasonId::CRASH_LOOP_BACK_OFF),
        },
    });
    scene.sats.push(k10s_core::SatNode {
        rect: k10s_core::Rect::new(120.0, 20.0, 10.0, 10.0),
        label: "checkout-api-svc".into(),
        ext: k10s_core::SatExt {
            kind: k10s_core::KindId::SERVICE,
            detail: "ClusterIP".into(),
        },
    });
    scene
}

#[test]
fn every_hover_level_names_its_object_and_carries_its_ancestry() {
    let scene = hover_scene();
    let path = |block, cell, sat| PickPath {
        region: 0,
        block,
        cell,
        sat,
    };

    let region = HoverInfo::resolve(&scene, path(None, None, None)).expect("region hover");
    assert_eq!(region.kind, "Namespace");
    assert_eq!(region.name.as_ref(), "payments");
    assert_eq!(region.namespace, None);
    assert_eq!(region.owner, None);
    assert_eq!(region.status, "Warning");

    let block = HoverInfo::resolve(&scene, path(Some(0), None, None)).expect("block hover");
    assert_eq!(block.kind, "Deployment");
    assert_eq!(block.name.as_ref(), "checkout-api");
    assert_eq!(block.namespace.as_deref(), Some("payments"));
    assert_eq!(block.owner, None);
    assert_eq!(block.status, "Critical");

    let cell = HoverInfo::resolve(&scene, path(Some(0), Some(0), None)).expect("cell hover");
    assert_eq!(cell.kind, "Pod");
    assert_eq!(cell.name.as_ref(), "checkout-api-7f9c8");
    assert_eq!(cell.namespace.as_deref(), Some("payments"));
    assert_eq!(cell.owner.as_deref(), Some("checkout-api"));
    assert_eq!(cell.status, "Critical");

    let sat = HoverInfo::resolve(&scene, path(Some(0), None, Some(0))).expect("sat hover");
    assert_eq!(sat.kind, "Service");
    assert_eq!(sat.name.as_ref(), "checkout-api-svc");
    assert_eq!(sat.namespace.as_deref(), Some("payments"));
    assert_eq!(sat.owner.as_deref(), Some("checkout-api"));
    assert_eq!(sat.status, "Attached");
}

#[test]
fn hover_cards_stay_inside_the_viewport() {
    let mut scene = SceneSnapshot::default();
    scene.regions.push(k10s_core::NsNode {
        rect: k10s_core::Rect::new(0.0, 0.0, 200.0, 120.0),
        label: "edge".into(),
        weight: 0,
        children: 0..0,
        ext: k10s_core::NsExt {
            unhealthy_frac: 0.0,
            rollup: Severity::Ok,
        },
    });
    let path = PickPath {
        region: 0,
        block: None,
        cell: None,
        sat: None,
    };
    for camera in [
        Camera {
            cx: 0.0,
            cy: 0.0,
            zoom: 1.0,
        },
        Camera {
            cx: 200.0,
            cy: 120.0,
            zoom: 4.0,
        },
    ] {
        let anchor = hover_anchor(&scene, path, camera, 500.0, 300.0).unwrap();
        assert!(anchor.left >= EDGE_MARGIN);
        assert!(anchor.top >= EDGE_MARGIN);
        assert!(anchor.left + HOVER_WIDTH <= 500.0 - EDGE_MARGIN + 0.01);
        assert!(anchor.top + HOVER_HEIGHT <= 300.0 - EDGE_MARGIN + 0.01);
    }
}

#[test]
fn hover_cards_stand_down_when_the_viewport_cannot_contain_them() {
    let mut scene = SceneSnapshot::default();
    scene.regions.push(k10s_core::NsNode {
        rect: k10s_core::Rect::new(0.0, 0.0, 20.0, 20.0),
        label: "tiny".into(),
        weight: 0,
        children: 0..0,
        ext: k10s_core::NsExt {
            unhealthy_frac: 0.0,
            rollup: Severity::Ok,
        },
    });
    let path = PickPath {
        region: 0,
        block: None,
        cell: None,
        sat: None,
    };

    assert_eq!(
        hover_anchor(&scene, path, Camera::default(), HOVER_WIDTH, HOVER_HEIGHT),
        None
    );
}
