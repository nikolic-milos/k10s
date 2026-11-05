use super::*;

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
        hud_on: false,
        empty: false,
        overlay_kind: None,
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
            workload_label: k10s_core::NsExt::workload_label(1),
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
    assert_eq!(cell.status, "CrashLoopBackOff");

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
            workload_label: k10s_core::NsExt::workload_label(0),
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
        let anchor = hover_anchor(&scene, path, camera, 500.0, 300.0, HOVER_HEIGHT).unwrap();
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
            workload_label: k10s_core::NsExt::workload_label(0),
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
        hover_anchor(
            &scene,
            path,
            Camera::default(),
            HOVER_WIDTH,
            HOVER_HEIGHT,
            HOVER_HEIGHT,
        ),
        None
    );
}

fn overlay_ids(scene: &mut SceneSnapshot) {
    scene.ids = std::sync::Arc::new(k10s_core::SceneIds {
        regions: vec![std::sync::Arc::from("ns-payments")].into(),
        blocks: vec![std::sync::Arc::from("wl-checkout")].into(),
        cells: vec![std::sync::Arc::from("pod-checkout")].into(),
        sats: vec![std::sync::Arc::from("svc-checkout")].into(),
    });
}

fn chrome_overlay<'a>(
    scene: &'a SceneSnapshot,
    hovered: Option<PickPath>,
    map_overlay: &'a OverlayFrame,
) -> Overlay<'a> {
    Overlay {
        scene,
        camera: Camera {
            cx: 100.0,
            cy: 60.0,
            zoom: 1.0,
        },
        displayed_stage: 1,
        hud_on: false,
        hovered,
        summary: "1 namespace  ·  1 workload  ·  1 pod".into(),
        edges_on: false,
        legend_on: true,
        viewport: (1280.0, 720.0),
        map_overlay,
    }
}

#[test]
fn overlay_kind_is_named_apart_from_the_lod_band() {
    let mut scene = hover_scene();
    overlay_ids(&mut scene);
    let empty = OverlayFrame::default();
    for (displayed_stage, band, label) in [
        (0, DetailBand::Orbit, "Namespaces"),
        (1, DetailBand::Orbit, "Namespaces"),
        (2, DetailBand::System, "Pods"),
        (3, DetailBand::Instance, "Pod details"),
    ] {
        let mut overlay = chrome_overlay(&scene, None, &empty);
        overlay.displayed_stage = displayed_stage;
        let none = State::resolve(overlay);
        assert_eq!(none.overlay_kind, None);
        assert_eq!(none.band, band);
        assert_eq!(none.band.label(), label);

        for kind in OverlayKind::ALL {
            let frame = OverlayFrame {
                kind: Some(kind),
                marks: Vec::new(),
            };
            let mut overlay = chrome_overlay(&scene, None, &frame);
            overlay.displayed_stage = displayed_stage;
            let state = State::resolve(overlay);
            assert_eq!(state.overlay_kind, Some(kind));
            assert_eq!(state.band, band);
            assert_eq!(state.band.label(), label);
            assert_eq!(state.summary, none.summary);
            assert_ne!(kind.badge(), state.band.label());
        }
    }
    assert_ne!(
        OverlayKind::MeshDeclared.blurb(),
        OverlayKind::MeshObserved.blurb()
    );
    assert_ne!(OverlayKind::Sync.badge(), OverlayKind::Metrics.badge());
    assert_ne!(
        OverlayKind::Policy.badge(),
        OverlayKind::MeshDeclared.badge()
    );
}

#[test]
fn hover_overlay_label_is_not_cluster_health() {
    let mut scene = hover_scene();
    overlay_ids(&mut scene);
    let frame = OverlayFrame {
        kind: Some(OverlayKind::Sync),
        marks: vec![crate::overlay::OverlayMark {
            uid: "wl-checkout".into(),
            tint: Some(Severity::Warn),
            sparkline: Some(k10s_theme::Series {
                name: "cpu".into(),
                samples: vec![
                    k10s_theme::Sample {
                        t_ms: 1,
                        value: 1.0,
                    },
                    k10s_theme::Sample {
                        t_ms: 2,
                        value: 2.0,
                    },
                ],
            }),
            label: Some("OutOfSync".into()),
        }],
    };
    let path = PickPath {
        region: 0,
        block: Some(0),
        cell: None,
        sat: None,
    };
    let state = State::resolve(chrome_overlay(&scene, Some(path), &frame));
    let (info, _) = state.hover.expect("hover card");
    assert_eq!(info.status, "Critical");
    assert_eq!(info.overlay_kind, Some(OverlayKind::Sync));
    assert_eq!(info.overlay_label.as_deref(), Some("OutOfSync"));
    assert_eq!(info.overlay_tint, Some(Severity::Warn));
    assert_eq!(info.overlay_spark.len(), 2);
    assert_ne!(info.overlay_label.as_deref(), Some(info.status));
}

#[test]
fn a_hovered_object_without_a_mark_does_not_inherit_a_default_overlay() {
    let mut scene = hover_scene();
    overlay_ids(&mut scene);
    let frame = OverlayFrame {
        kind: Some(OverlayKind::Policy),
        marks: vec![crate::overlay::OverlayMark {
            uid: "someone-else".into(),
            tint: Some(Severity::Err),
            sparkline: None,
            label: Some("Violation".into()),
        }],
    };
    let path = PickPath {
        region: 0,
        block: Some(0),
        cell: None,
        sat: None,
    };
    let state = State::resolve(chrome_overlay(&scene, Some(path), &frame));
    assert_eq!(state.overlay_kind, Some(OverlayKind::Policy));
    let (info, _) = state.hover.expect("hover card");
    assert_eq!(info.status, "Critical");
    assert_eq!(info.overlay_kind, None);
    assert_eq!(info.overlay_label, None);
    assert!(info.overlay_spark.is_empty());
    assert_eq!(info.overlay_tint, None);
}

#[test]
fn mesh_declared_and_observed_stay_apart_on_the_hover_card() {
    let mut scene = hover_scene();
    overlay_ids(&mut scene);
    let path = PickPath {
        region: 0,
        block: Some(0),
        cell: None,
        sat: None,
    };
    let declared = OverlayFrame {
        kind: Some(OverlayKind::MeshDeclared),
        marks: vec![crate::overlay::OverlayMark {
            uid: "wl-checkout".into(),
            tint: Some(Severity::Ok),
            sparkline: None,
            label: Some("can reach".into()),
        }],
    };
    let observed = OverlayFrame {
        kind: Some(OverlayKind::MeshObserved),
        marks: vec![crate::overlay::OverlayMark {
            uid: "wl-checkout".into(),
            tint: Some(Severity::Ok),
            sparkline: None,
            label: Some("did reach".into()),
        }],
    };
    let d = State::resolve(chrome_overlay(&scene, Some(path), &declared));
    let o = State::resolve(chrome_overlay(&scene, Some(path), &observed));
    assert_eq!(d.overlay_kind, Some(OverlayKind::MeshDeclared));
    assert_eq!(o.overlay_kind, Some(OverlayKind::MeshObserved));
    assert_eq!(
        d.hover.as_ref().unwrap().0.overlay_label.as_deref(),
        Some("can reach")
    );
    assert_eq!(
        o.hover.as_ref().unwrap().0.overlay_label.as_deref(),
        Some("did reach")
    );
    assert_ne!(
        OverlayKind::MeshDeclared.blurb(),
        OverlayKind::MeshObserved.blurb()
    );
}

#[test]
fn semantic_band_matches_paint_pick_and_overlay_during_handoff_and_hysteresis() {
    use crate::frame::tests::{Collect, VH, VW, viewport};
    use crate::frame::{FrameOpts, walk};
    use crate::overlay::{OverlayKind, OverlayMark, StampCache};
    use k10s_atlas::{StageBlend, StageMachine};

    let mut scene = hover_scene();
    scene.card_header = crate::frame::tests::CARD_HEADER_FIXTURE;
    scene.blocks[0].rect = k10s_core::Rect::new(10.0, 10.0, 130.0, 100.0);
    scene.blocks[0].inner = k10s_core::Rect::new(10.0, 10.0, 100.0, 100.0);
    scene.bounds = scene.regions[0].rect;
    scene.cells[0].rect = k10s_core::Rect::new(40.0, 58.0, 10.0, 10.0);
    overlay_ids(&mut scene);
    scene.rebuild_spatial_index();
    let policy = crate::lod::policy(Default::default());
    let frame = OverlayFrame {
        kind: Some(OverlayKind::Policy),
        marks: vec![OverlayMark {
            uid: "wl-checkout".into(),
            tint: Some(Severity::Warn),
            sparkline: None,
            label: None,
        }],
    };
    let check = |zoom, blend: StageBlend, expected| {
        let camera = Camera {
            cx: 45.0,
            cy: 63.0,
            zoom,
        };
        let mut overlay = chrome_overlay(&scene, None, &frame);
        overlay.camera = camera;
        overlay.viewport = (VW, VH);
        overlay.displayed_stage = blend.walk_stage();
        let chrome = State::resolve(overlay);
        assert_eq!(chrome.band.label(), expected);
        let opts = FrameOpts {
            theme: &k10s_theme::K10S_DARK.map,
            policy: &policy,
            type_: k10s_theme::MapType::default(),
            edges_on: false,
            skip_blocks: false,
            hex: false,
        };
        let mut sink = Collect::default();
        let counts = walk(viewport(), &scene, camera, blend, opts, &mut sink);
        assert_eq!(
            counts,
            crate::lod::cull(&scene, &camera, blend, VW, VH, opts)
        );
        let children_shown = expected != "Namespaces";
        assert_eq!(counts.drawn_blocks > 0, children_shown);
        assert_eq!(counts.drawn_cells > 0, children_shown);
        let (x, y) = camera.w2s(45.0, 63.0, VW, VH);
        let picked = crate::pick::pick(&scene, &camera, &policy, blend, VW, VH, x, y)
            .expect("the displayed namespace or its child remains pickable");
        assert_eq!(picked.block.is_some(), children_shown);
        assert_eq!(picked.cell.is_some(), children_shown);
        assert_eq!(
            frame
                .visible_stamps_typed(
                    &mut StampCache::default(),
                    &scene,
                    camera,
                    &policy,
                    blend,
                    VW,
                    VH,
                    k10s_theme::MapType::default(),
                )
                .is_empty(),
            !children_shown
        );
    };
    for (stage, zoom, label) in [
        (0, policy.stage_block * 0.5, "Namespaces"),
        (1, policy.stage_block, "Namespaces"),
        (2, policy.stage_cell, "Pods"),
        (3, policy.stage_cell_label, "Pod details"),
    ] {
        check(zoom, StageBlend::settled(stage), label);
    }
    for (from, to) in [(1, 2), (2, 1)] {
        for t in [0.0, 0.49, 0.5, 0.51, 1.0] {
            let blend = StageBlend { from, to, t };
            let label = if blend.walk_stage() == 1 {
                "Namespaces"
            } else {
                "Pods"
            };
            check(policy.stage_cell, blend, label);
        }
    }
    let mut machine = StageMachine::new(crate::lod::STAGE_FADE_SECS);
    machine.settle(&policy, policy.stage_cell);
    let held_zoom = policy.stage_cell * 0.95;
    assert_eq!(policy.stage_for_zoom(held_zoom), 1);
    let held = machine.settle(&policy, held_zoom);
    assert_eq!(held.walk_stage(), 2);
    check(held_zoom, held, "Pods");
    let exit_zoom = policy.stage_cell * policy.stage_exit - 0.001;
    let exited = machine.settle(&policy, exit_zoom);
    assert_eq!(exited.walk_stage(), 1);
    check(exit_zoom, exited, "Namespaces");
}

#[test]
fn explicit_diagnostic_hud_hides_only_the_normal_summary() {
    let scene = hover_scene();
    let frame = OverlayFrame::default();
    for viewport in [(1280.0, 720.0), (700.0, 480.0), (320.0, 200.0)] {
        let mut overlay = chrome_overlay(&scene, None, &frame);
        overlay.viewport = viewport;
        let normal = State::resolve(overlay);
        assert_eq!(normal.summary_shown(), normal.density != Density::Minimal);
        let mut overlay = chrome_overlay(&scene, None, &frame);
        overlay.viewport = viewport;
        overlay.hud_on = true;
        let diagnostic = State::resolve(overlay);
        assert!(!diagnostic.summary_shown());
        assert_eq!(diagnostic.summary, normal.summary);
        assert_eq!(diagnostic.band, normal.band);
        assert_eq!(diagnostic.legend_on, normal.legend_on);
        assert_eq!(diagnostic.edges_on, normal.edges_on);
        assert_eq!(diagnostic.hover, normal.hover);
        let mut chrome = Chrome::default();
        chrome.replace(normal.clone());
        assert!(chrome.replace(diagnostic.clone()));
        assert!(!chrome.replace(diagnostic));
        assert!(chrome.replace(normal));
    }
}
