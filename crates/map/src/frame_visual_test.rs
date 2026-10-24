//! The visual grammar, asserted rather than eyeballed: glyph sizes sit on one
//! shared ladder, a label is confined to the thing it names, an island keeps
//! its shape when its namespace resizes, and a terminating pod costs the same
//! quad as a healthy one.

use std::sync::Arc;

use k10s_core::{
    NsExt, NsNode, PodExt, PodNode, ReasonId, SatExt, SatNode, SceneData, State, WlExt,
    WorkloadNode,
};

use super::tests::*;
use super::*;
use crate::lod::{Knobs, policy};

fn one_of_each(pod_state: State) -> SceneSnapshot {
    SceneSnapshot {
        identity_rev: 1,
        ids: Default::default(),
        scene: SceneData {
            rev: 1,
            card_header: CARD_HEADER_FIXTURE,
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            regions: vec![NsNode {
                rect: Rect::new(0.0, 0.0, 400.0, 300.0),
                label: Arc::from("payments-production-eu-west"),
                weight: 1,
                children: 0..1,
                ext: NsExt {
                    workload_label: k10s_core::NsExt::workload_label(1),
                    unhealthy_frac: 0.2,
                    rollup: Severity::Warn,
                },
            }],
            blocks: vec![WorkloadNode {
                rect: Rect::new(40.0, 40.0, 200.0, 200.0),
                inner: Rect::new(40.0, 40.0, 120.0, 130.0),
                label: Arc::from("payments-redis-primary-statefulset"),
                children: 0..1,
                sats: 0..1,
                ext: WlExt {
                    kind: KindId::STATEFUL_SET,
                    tool: ToolId::NONE,
                    rollup: Severity::Ok,
                    ns: 0,
                },
            }],
            cells: vec![PodNode {
                rect: Rect::new(50.0, 76.0, 10.0, 10.0),
                label: Arc::from("payments-redis-primary-0"),
                ext: PodExt { state: pod_state },
            }],
            sats: vec![SatNode {
                rect: Rect::new(210.0, 90.0, 18.0, 18.0),
                label: Arc::from("pvc/data-payments-redis-49"),
                ext: SatExt {
                    kind: KindId::VOLUME,
                    detail: Arc::from("16Gi"),
                },
            }],
            ..SceneData::default()
        },
    }
}

fn small_workload(pod_state: State, sparse: bool) -> SceneSnapshot {
    let mut scene = one_of_each(pod_state);
    scene.blocks[0].inner = Rect::new(40.0, 40.0, 30.0, 56.0);
    let mut pod_ids = vec![Arc::<str>::from("pod-current")];
    if sparse {
        let actual = scene.cells[0].clone();
        scene.cells[0].rect = Rect::default();
        scene.cells[0].label = "".into();
        scene.cells.push(PodNode {
            rect: Rect::new(300.0, 200.0, 10.0, 10.0),
            label: "another-workload-pod".into(),
            ext: PodExt { state: State::OK },
        });
        scene.cells.push(actual);
        scene.block_cells = vec![2];
        pod_ids = vec![
            Arc::from(""),
            Arc::from("other-pod"),
            Arc::from("pod-current"),
        ];
    }
    scene.ids = Arc::new(k10s_core::SceneIds {
        regions: [Arc::<str>::from("namespace")].into_iter().collect(),
        blocks: [Arc::<str>::from("workload")].into_iter().collect(),
        cells: pod_ids.into_iter().collect(),
        sats: [Arc::<str>::from("satellite")].into_iter().collect(),
    });
    scene.rebuild_spatial_index();
    scene
}

fn live_attachment_snapshots() -> (Arc<SceneSnapshot>, Arc<SceneSnapshot>, usize) {
    use k10s_core::{IngestEvent, Op, Payload, ResourceEvent, replay};
    use k10s_world::{LayoutMode, PublishBench};

    let mut events = vec![replay::scope("ns-prod", "prod", Op::Added)];
    for index in 0..128 {
        let owner = format!("wl-{index:03}");
        events.push(replay::owner(
            &owner,
            "prod",
            &owner,
            KindId::DEPLOYMENT,
            Op::Added,
        ));
        events.push(replay::instance(
            &format!("pod-{index:03}"),
            "prod",
            &owner,
            State::OK,
            Op::Added,
        ));
    }
    let mut bench = PublishBench::new(&events, LayoutMode::Spread);
    let bare = bench.snapshot();
    bench.apply_events(&[IngestEvent::Resource(ResourceEvent {
        kind: KindId::SERVICE,
        uid: "svc-api".into(),
        namespace: "prod".into(),
        name: "svc-api".into(),
        resource_version: 0,
        parent: Some("wl-000".into()),
        op: Op::Added,
        payload: Payload::Attached {
            kind: KindId::SERVICE,
            detail: "ClusterIP".into(),
        },
    })]);
    bench.run_publish();
    let attached = bench.snapshot();
    let index = attached
        .ids
        .blocks
        .iter()
        .position(|uid| uid.as_ref() == "wl-000")
        .expect("the live workload is published");
    assert_eq!(attached.blocks[index].inner, bare.blocks[index].inner);
    let halo = attached.blocks[index].rect;
    let inner = attached.blocks[index].inner;
    assert_eq!((inner.w, inner.h), (30.0, 56.0));
    assert_eq!((halo.x, halo.y, halo.h), (inner.x, inner.y, inner.h));
    assert!(halo.w > inner.w);
    assert!(halo.contains(&attached.sats[0].rect));
    (bare, attached, index)
}

#[test]
fn live_asymmetric_halos_keep_identity_paint_pick_marks_overlays_and_culling_bounded() {
    use crate::overlay::{OverlayFrame, OverlayKind, OverlayMark, StampCache};

    let (bare, scene, index) = live_attachment_snapshots();
    let pol = policy(Knobs::default());
    let (vw, vh) = (64.0, 48.0);
    let type_ = MapType::default();
    let block = &scene.blocks[index];
    let path = crate::PickPath {
        region: block.ext.ns,
        block: Some(index as u32),
        cell: None,
        sat: None,
    };
    let overlay = OverlayFrame {
        kind: Some(OverlayKind::Policy),
        marks: vec![OverlayMark {
            uid: "wl-000".into(),
            tint: Some(Severity::Warn),
            sparkline: None,
            label: None,
        }],
    };
    for zoom in [0.14, 0.20, 0.25, 0.30, 0.50, 1.0] {
        let camera = Camera {
            cx: block.inner.center().0,
            cy: block.inner.center().1,
            zoom,
        };
        let blend = StageBlend::settled(2);
        let projection = Projection::new(camera, (vw, vh), (0.0, 0.0));
        let presentation = workload_presentation(&pol, block.inner, zoom, 2, type_);
        let target = crate::primitive::mark_primitive_typed(
            &scene,
            path,
            camera,
            &pol,
            2,
            (vw, vh),
            (0.0, 0.0),
            type_,
        )
        .expect("the workload is drawn");
        assert_eq!(
            Some(target),
            crate::primitive::mark_primitive_typed(
                &bare,
                path,
                camera,
                &pol,
                2,
                (vw, vh),
                (0.0, 0.0),
                type_,
            ),
            "an attachment cannot enlarge the workload target at zoom {zoom}",
        );
        let envelope = projection.bounds(block.inner);
        assert!(
            target.bounds.origin.x >= envelope.origin.x
                && target.bounds.origin.y >= envelope.origin.y
                && target.bounds.right() <= envelope.right()
                && target.bounds.bottom() <= envelope.bottom(),
            "zoom {zoom}: {target:?} extends outside {envelope:?}",
        );
        assert!(
            scene.region_block_index_is_selective(
                path.region as usize,
                &camera.visible_world(vw, vh),
            )
        );
        if presentation == k10s_atlas::WorkloadPresentation::Medallion {
            assert!(k10s_theme::SIZE_STEPS.contains(&f32::from(target.bounds.size.width)));
        }
        let stamps = overlay.visible_stamps_typed(
            &mut StampCache::default(),
            &scene,
            camera,
            &pol,
            blend,
            vw,
            vh,
            type_,
        );
        assert_eq!(stamps.len(), 1);
        assert_eq!(
            stamps[0].screen,
            Rect::new(
                f32::from(target.bounds.origin.x),
                f32::from(target.bounds.origin.y),
                f32::from(target.bounds.size.width),
                f32::from(target.bounds.size.height),
            )
        );
        for theme in [&k10s_theme::K10S_DARK.map, &k10s_theme::K10S_LIGHT.map] {
            let opts = FrameOpts {
                policy: &pol,
                theme,
                type_,
                edges_on: false,
                skip_blocks: false,
                hex: false,
            };
            let mut painted = Collect::default();
            let stats = walk(
                Bounds {
                    origin: point(px(0.0), px(0.0)),
                    size: size(px(vw), px(vh)),
                },
                &scene,
                camera,
                blend,
                opts,
                &mut painted,
            );
            assert_eq!(
                stats,
                crate::lod::cull(&scene, &camera, blend, vw, vh, opts)
            );
            assert_eq!(stats.drawn_cells, 0);
            let glyphs = painted
                .icons
                .iter()
                .filter(|job| {
                    matches!(job, IconJob::Wl(_, primitive) if primitive.bounds == target.bounds)
                })
                .count();
            assert_eq!(
                glyphs,
                usize::from(presentation == k10s_atlas::WorkloadPresentation::Medallion),
            );
            if presentation == k10s_atlas::WorkloadPresentation::Card {
                assert!(painted.fg.iter().any(|quad| quad.bounds == target.bounds));
            }
            for offset in [-0.4, 0.0, 0.4] {
                let probe = point(
                    target.bounds.center().x + target.bounds.size.width * offset,
                    target.bounds.center().y,
                );
                assert!(target.contains(probe));
                let (wx, wy) = camera.s2w(f32::from(probe.x), f32::from(probe.y), vw, vh);
                assert!(scene.region_block_index_is_selective(
                    path.region as usize,
                    &Rect::new(wx, wy, 1e-3, 1e-3),
                ));
                assert_eq!(
                    crate::pick::pick(
                        &scene.scene,
                        &camera,
                        &pol,
                        blend,
                        vw,
                        vh,
                        f32::from(probe.x),
                        f32::from(probe.y)
                    ),
                    Some(path),
                );
            }
        }
    }
}

#[test]
fn a_live_attachment_keeps_workload_glyph_scale_and_monotonic_targets() {
    let (bare, attached, index) = live_attachment_snapshots();
    let pol = policy(Knobs::default());
    let inner = bare.blocks[index].inner;
    let path = crate::PickPath {
        region: bare.blocks[index].ext.ns,
        block: Some(index as u32),
        cell: None,
        sat: None,
    };
    for font in [8.0, 14.0, 24.0, 32.0] {
        let type_ = MapType {
            workload: font,
            pod: font,
            ..Default::default()
        };
        let mut previous = [None; 2];
        let mut medallions = 0;
        let mut headers = 0;
        for step in 1..=2400 {
            let zoom = step as f32 * 0.01;
            let camera = Camera {
                cx: inner.center().0,
                cy: inner.center().1,
                zoom,
            };
            let stage = pol.stage_for_zoom(zoom);
            let presentation = workload_presentation(&pol, inner, zoom, stage, type_);
            let mut targets = [None; 2];
            for (scene_index, scene) in [&bare, &attached].into_iter().enumerate() {
                let target = crate::primitive::mark_primitive_typed(
                    scene,
                    path,
                    camera,
                    &pol,
                    stage,
                    (1600.0, 1000.0),
                    (0.0, 0.0),
                    type_,
                );
                let Some(target) = target else { continue };
                let width = f32::from(target.bounds.size.width);
                let height = f32::from(target.bounds.size.height);
                if let Some((old_width, old_height)) = previous[scene_index] {
                    assert!(
                        width + 0.01 >= old_width && height + 0.01 >= old_height,
                        "font {font}, zoom {zoom}, stage {stage}: {target:?} shrank"
                    );
                }
                previous[scene_index] = Some((width, height));
                targets[scene_index] = Some(target);
            }
            if stage >= 2
                && matches!(
                    presentation,
                    k10s_atlas::WorkloadPresentation::Medallion
                        | k10s_atlas::WorkloadPresentation::Detailed
                )
            {
                assert!(targets[0].is_some() && targets[1].is_some());
                assert_eq!(targets[0], targets[1], "attachments cannot resize identity");
                if presentation == k10s_atlas::WorkloadPresentation::Medallion {
                    medallions += 1;
                } else {
                    headers += 1;
                }
            }
        }
        assert!(medallions > 10 && headers > 10);
    }
}

#[test]
fn compact_workload_geometry_agrees_across_paint_pick_marks_overlays_and_counters() {
    use crate::overlay::{OverlayFrame, OverlayKind, OverlayMark, StampCache};

    let pol = policy(Knobs::default());
    let blend = StageBlend::settled(3);
    for sparse in [false, true] {
        let scene = small_workload(State::of(ReasonId::FAILED), sparse);
        let cell_index = if sparse { 2 } else { 0 };
        for theme in [&k10s_theme::K10S_DARK.map, &k10s_theme::K10S_LIGHT.map] {
            for font in [8.0, 14.0, 32.0] {
                let type_ = MapType {
                    workload: font,
                    pod: font,
                    ..Default::default()
                };
                let zoom = crate::lod::STAGE_POD_LABEL
                    .max(crate::primitive::detail_min_width(type_) / 10.0);
                for (vw, vh, pan) in [(VW, VH, 0.0), (600.0, 420.0, 0.0), (384.0, 300.0, 5.0)] {
                    let height = (row_pitch(font, type_.line_height) * 2.0 + TEXT_PAD * 2.0)
                        .ceil()
                        .clamp(64.0, 104.0);
                    let pod_y = vh - crate::primitive::bottom_clearance(type_) - height * 0.5 - 4.0;
                    let camera = Camera {
                        cx: 55.0 + pan,
                        cy: 81.0 - (pod_y - vh * 0.5) / zoom,
                        zoom,
                    };
                    let projection = Projection::new(camera, (vw, vh), (0.0, 0.0));
                    let geometry = projection.workload_card(&scene, 0, 3, type_);
                    let pod =
                        projection.cell(scene.cells[cell_index].rect, 3, type_, geometry.heading);
                    let bounds = Bounds {
                        origin: point(px(0.0), px(0.0)),
                        size: size(px(vw), px(vh)),
                    };
                    let opts = FrameOpts {
                        policy: &pol,
                        theme,
                        type_,
                        edges_on: false,
                        skip_blocks: false,
                        hex: false,
                    };
                    let mut sink = Collect::default();
                    let stats = walk(bounds, &scene, camera, blend, opts, &mut sink);
                    assert_eq!(
                        stats,
                        crate::lod::cull(&scene, &camera, blend, vw, vh, opts)
                    );
                    assert!(
                        sink.fg
                            .iter()
                            .any(|quad| quad.bounds == geometry.body.bounds)
                    );
                    let block_path = crate::PickPath {
                        region: 0,
                        block: Some(0),
                        cell: None,
                        sat: None,
                    };
                    assert_eq!(
                        crate::primitive::mark_primitive_typed(
                            &scene,
                            block_path,
                            camera,
                            &pol,
                            3,
                            (vw, vh),
                            (0.0, 0.0),
                            type_
                        ),
                        Some(geometry.body)
                    );
                    let block_overlay = OverlayFrame {
                        kind: Some(OverlayKind::Policy),
                        marks: vec![OverlayMark {
                            uid: "workload".into(),
                            tint: Some(Severity::Warn),
                            sparkline: None,
                            label: None,
                        }],
                    };
                    let shown = block_overlay.visible_stamps_typed(
                        &mut StampCache::default(),
                        &scene,
                        camera,
                        &pol,
                        blend,
                        vw,
                        vh,
                        type_,
                    );
                    assert_eq!(shown.len(), 1);
                    assert_eq!(
                        shown[0].screen,
                        Rect::new(
                            f32::from(geometry.body.bounds.origin.x),
                            f32::from(geometry.body.bounds.origin.y),
                            f32::from(geometry.body.bounds.size.width),
                            f32::from(geometry.body.bounds.size.height)
                        )
                    );
                    if let Some(heading) = geometry.heading {
                        assert!(sink.fg.iter().any(|quad| quad.bounds == heading.bounds));
                        assert!(heading.bounds.is_contained_within(&geometry.body.bounds));
                        let point = heading.bounds.center();
                        let picked = crate::pick::pick_typed(
                            &scene,
                            &camera,
                            &pol,
                            blend,
                            vw,
                            vh,
                            f32::from(point.x),
                            f32::from(point.y),
                            type_,
                        )
                        .unwrap();
                        assert_eq!(picked, block_path);
                        let first = projection.band_icon(heading, 0, type_);
                        let second = projection.band_icon(heading, 1, type_);
                        assert!(first.bounds.bottom() <= second.bounds.origin.y);
                        assert!(first.bounds.is_contained_within(&heading.bounds));
                        assert!(second.bounds.is_contained_within(&heading.bounds));
                        assert!(
                            sink.icons
                                .iter()
                                .any(|job| icon_bounds(job) == first.bounds)
                        );
                        assert!(
                            sink.icons
                                .iter()
                                .any(|job| icon_bounds(job) == second.bounds)
                        );
                        let name = sink
                            .labels
                            .iter()
                            .find(|job| job.text == scene.blocks[0].label.as_ref())
                            .unwrap();
                        let status = sink
                            .labels
                            .iter()
                            .find(|job| job.text == "Healthy")
                            .unwrap();
                        assert!(name.y + name.height <= f32::from(second.bounds.origin.y));
                        assert!(f32::from(first.bounds.bottom()) <= status.y);
                    }
                    let cell_path = crate::PickPath {
                        cell: Some(cell_index as u32),
                        ..block_path
                    };
                    assert_eq!(
                        crate::primitive::mark_primitive_typed(
                            &scene,
                            cell_path,
                            camera,
                            &pol,
                            3,
                            (vw, vh),
                            (0.0, 0.0),
                            type_
                        ),
                        pod
                    );
                    let pod_overlay = OverlayFrame {
                        kind: Some(OverlayKind::Policy),
                        marks: vec![OverlayMark {
                            uid: "pod-current".into(),
                            tint: Some(Severity::Err),
                            sparkline: None,
                            label: None,
                        }],
                    };
                    let shown = pod_overlay.visible_stamps_typed(
                        &mut StampCache::default(),
                        &scene,
                        camera,
                        &pol,
                        blend,
                        vw,
                        vh,
                        type_,
                    );
                    assert_eq!(shown.len(), usize::from(pod.is_some()));
                    if let Some(pod) = pod {
                        assert!(pod.bounds.is_contained_within(&geometry.body.bounds));
                        assert_eq!(
                            pod.bounds.center(),
                            projection.bounds(scene.cells[cell_index].rect).center()
                        );
                        assert!(sink.fg.iter().any(|quad| quad.bounds == pod.bounds));
                        let point = pod.bounds.center();
                        let picked = crate::pick::pick_typed(
                            &scene,
                            &camera,
                            &pol,
                            blend,
                            vw,
                            vh,
                            f32::from(point.x),
                            f32::from(point.y),
                            type_,
                        )
                        .unwrap();
                        assert_eq!(picked, cell_path);
                        assert_eq!(shown[0].screen.x, f32::from(pod.bounds.origin.x));
                        assert_eq!(shown[0].screen.y, f32::from(pod.bounds.origin.y));
                        let name = sink
                            .labels
                            .iter()
                            .find(|job| job.text == scene.cells[cell_index].label.as_ref())
                            .expect("the visible Pod has its name");
                        let icon = sink
                            .icons
                            .iter()
                            .find_map(|job| match job {
                                IconJob::Status(Severity::Err, primitive) => Some(primitive),
                                _ => None,
                            })
                            .expect("the visible Pod has its status glyph");
                        assert!(name.y + name.height <= f32::from(icon.bounds.origin.y));
                    } else {
                        assert!(
                            !sink
                                .labels
                                .iter()
                                .any(|job| job.text == scene.cells[cell_index].label.as_ref())
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn workload_headers_reserve_the_glyph_height_without_changing_pod_rows() {
    let type_ = MapType::default();
    assert_eq!(
        row_pitch(type_.workload, type_.line_height),
        type_.workload * type_.line_height
    );
    assert_eq!(
        row_pitch(type_.pod, type_.line_height),
        type_.pod * type_.line_height
    );
    assert_eq!(crate::primitive::workload_row_pitch(type_), WL_ICON_MAX_PX);
    assert_eq!(crate::primitive::name_band_height(type_), 80.0);
}

#[test]
fn removing_the_only_pod_keeps_observed_truth_and_a_bounded_workload_target() {
    let mut scene = small_workload(State::OK, true);
    scene.blocks[0].children = 0..0;
    let ids = Arc::make_mut(&mut scene.ids);
    ids.cells[2] = Arc::from("");
    scene.identity_rev += 1;
    let pol = policy(Knobs::default());
    let block_path = crate::PickPath {
        region: 0,
        block: Some(0),
        cell: None,
        sat: None,
    };
    let overlay = crate::OverlayFrame {
        kind: Some(crate::OverlayKind::Policy),
        marks: vec![crate::OverlayMark {
            uid: "pod-current".into(),
            tint: Some(Severity::Err),
            sparkline: None,
            label: None,
        }],
    };
    for (stage, zoom, inner) in [
        (0, 0.5, Rect::new(40.0, 40.0, 30.0, 56.0)),
        (1, 0.5, Rect::new(40.0, 40.0, 30.0, 56.0)),
        (1, 1.0, Rect::new(40.0, 40.0, 400.0, 400.0)),
        (2, 8.0, Rect::new(40.0, 40.0, 30.0, 42.0)),
        (2, 8.0, Rect::new(40.0, 40.0, 30.0, 56.0)),
        (
            3,
            crate::lod::STAGE_POD_LABEL,
            Rect::new(40.0, 40.0, 30.0, 56.0),
        ),
    ] {
        scene.blocks[0].inner = inner;
        scene.blocks[0].rect = inner.inflate(20.0);
        scene.blocks[0].sats = 0..0;
        scene.regions[0].rect = inner.inflate(400.0);
        scene.bounds = scene.regions[0].rect;
        scene.rebuild_spatial_index();
        let (cx, cy) = inner.center();
        let camera = Camera { cx, cy, zoom };
        let blend = StageBlend::settled(stage);
        let type_ = MapType::default();
        let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
        let presentation = workload_presentation(&pol, inner, zoom, stage, type_);
        for theme in [&k10s_theme::K10S_DARK.map, &k10s_theme::K10S_LIGHT.map] {
            let opts = FrameOpts {
                theme,
                policy: &pol,
                type_,
                edges_on: false,
                skip_blocks: false,
                hex: false,
            };
            let mut sink = Collect::default();
            let stats = walk(viewport(), &scene, camera, blend, opts, &mut sink);
            assert_eq!(
                stats,
                crate::lod::cull(&scene, &camera, blend, VW, VH, opts)
            );
            assert_eq!(stats.drawn_cells, 0);
            assert!(
                !sink
                    .labels
                    .iter()
                    .any(|job| job.text == "Healthy" || job.text == scene.cells[2].label.as_ref())
            );
            let mark = crate::primitive::mark_primitive_typed(
                &scene,
                block_path,
                camera,
                &pol,
                stage,
                (VW, VH),
                (0.0, 0.0),
                type_,
            );
            if stage < 2 || presentation == k10s_atlas::WorkloadPresentation::Hidden {
                assert!(mark.is_none());
                assert!(!sink.labels.iter().any(|job| job.text == "0 observed pods"));
            } else if matches!(
                presentation,
                k10s_atlas::WorkloadPresentation::Medallion
                    | k10s_atlas::WorkloadPresentation::Card
            ) {
                let medallion = projection.medallion(inner);
                assert_eq!(mark, Some(medallion));
                if presentation == k10s_atlas::WorkloadPresentation::Medallion {
                    assert!(
                        sink.icons
                            .iter()
                            .any(|job| icon_bounds(job) == medallion.bounds)
                    );
                } else {
                    assert!(sink.fg.iter().any(|quad| quad.bounds == medallion.bounds));
                }
                let point = medallion.bounds.center();
                let picked = crate::pick::pick(
                    &scene,
                    &camera,
                    &pol,
                    blend,
                    VW,
                    VH,
                    f32::from(point.x),
                    f32::from(point.y),
                )
                .unwrap();
                assert_eq!(picked, block_path);
            } else {
                let geometry = projection.workload_card(&scene, 0, stage, type_);
                assert_eq!(geometry.body.bounds.size, size(px(168.0), px(100.0)));
                assert_eq!(mark, Some(geometry.body));
                assert!(
                    sink.fg
                        .iter()
                        .any(|quad| quad.bounds == geometry.body.bounds)
                );
                assert!(sink.labels.iter().any(|job| job.text == "0 observed pods"));
                let heading = geometry.heading.unwrap();
                let hub = projection.bounds(Rect::new(cx, cy, 0.0, 0.0)).origin;
                for point in [heading.bounds.center(), hub] {
                    let picked = crate::pick::pick(
                        &scene,
                        &camera,
                        &pol,
                        blend,
                        VW,
                        VH,
                        f32::from(point.x),
                        f32::from(point.y),
                    )
                    .unwrap();
                    assert_eq!(picked, block_path);
                }
                let original = projection.card(inner);
                let gap = point(
                    original.bounds.center().x,
                    original.bounds.bottom() - px(1.0),
                );
                assert!(original.contains(gap));
                assert!(!geometry.body.contains(gap));
                let picked = crate::pick::pick(
                    &scene,
                    &camera,
                    &pol,
                    blend,
                    VW,
                    VH,
                    f32::from(gap.x),
                    f32::from(gap.y),
                )
                .unwrap();
                assert_eq!(
                    (picked.region, picked.block, picked.cell, picked.sat),
                    (0, None, None, None)
                );
            }
            assert!(
                overlay
                    .visible_stamps(&scene, camera, &pol, blend, VW, VH)
                    .is_empty()
            );
        }
    }
}

#[test]
fn workload_card_colors_and_status_use_observed_pods() {
    let pol = policy(Knobs::default());
    let type_ = MapType::default();
    for observed in [0usize, 1] {
        for rollup in [
            Severity::Ok,
            Severity::Warn,
            Severity::Err,
            Severity::Unknown,
        ] {
            let mut scene = small_workload(State::OK, false);
            scene.blocks[0].ext.rollup = rollup;
            if observed == 0 {
                scene.blocks[0].children = 0..0;
                scene.cells.clear();
                Arc::make_mut(&mut scene.ids).cells.clear();
                scene.rebuild_spatial_index();
            }
            let expected = if observed == 0 {
                Severity::Unknown
            } else {
                rollup
            };
            let expected_label = if observed == 0 {
                "0 observed pods"
            } else {
                crate::status::label(rollup)
            };
            let inner = scene.blocks[0].inner;
            for (stage, zoom) in [
                (2, (pol.block_chrome_min_px + TEXT_PAD * 2.0) / inner.w),
                (3, pol.stage_cell_label),
            ] {
                assert_eq!(pol.stage_for_zoom(zoom), stage);
                let camera = Camera {
                    cx: inner.center().0,
                    cy: inner.center().1,
                    zoom,
                };
                let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
                let geometry = projection.workload_card(&scene, 0, stage, type_);
                let heading = geometry
                    .heading
                    .expect("the observed workload has a heading");
                let footer = geometry.body.severity_strip();
                let status_bounds = projection.band_icon(heading, 1, type_).bounds;
                for (theme_name, theme) in [
                    ("dark", &k10s_theme::K10S_DARK.map),
                    ("light", &k10s_theme::K10S_LIGHT.map),
                ] {
                    let opts = FrameOpts {
                        theme,
                        policy: &pol,
                        type_,
                        edges_on: false,
                        skip_blocks: false,
                        hex: false,
                    };
                    let blend = StageBlend::settled(stage);
                    let mut sink = Collect::default();
                    let stats = walk(viewport(), &scene, camera, blend, opts, &mut sink);
                    assert_eq!(
                        stats,
                        crate::lod::cull(&scene, &camera, blend, VW, VH, opts)
                    );
                    let body = sink
                        .fg
                        .iter()
                        .find(|quad| quad.bounds == geometry.body.bounds)
                        .expect("the workload emits its card body");
                    let strip = sink
                        .fg
                        .iter()
                        .find(|quad| quad.bounds == footer.bounds)
                        .expect("the workload emits its severity strip");
                    let (fill, border) = theme.workload_colors(expected);
                    let border: gpui::Hsla = border.into();
                    let strip_color: Background = theme.pod_color(expected).into();
                    assert_eq!(
                        body.background,
                        shade(fill, rgb(theme.bg), CARD_SHADE),
                        "{theme_name}, stage {stage}, {observed} observed, {rollup:?} rollup: card fill",
                    );
                    assert_eq!(
                        body.border_color, border,
                        "{theme_name}, stage {stage}, {observed} observed, {rollup:?} rollup: card border",
                    );
                    assert_eq!(
                        strip.background, strip_color,
                        "{theme_name}, stage {stage}, {observed} observed, {rollup:?} rollup: severity strip",
                    );
                    assert!(
                        sink.icons.iter().any(|job| matches!(job, IconJob::Status(severity, primitive) if *severity == expected && primitive.bounds == status_bounds)),
                        "{theme_name}, stage {stage}, {observed} observed, {rollup:?} rollup: workload status icon",
                    );
                    assert!(
                        sink.labels.iter().any(|label| label.text == expected_label
                            && heading.bounds.contains(&point(px(label.x), px(label.y)))),
                        "{theme_name}, stage {stage}, {observed} observed, {rollup:?} rollup: workload status label",
                    );
                }
            }
        }
    }
}

fn cameras() -> Vec<Camera> {
    // Across every stage the map has, including both sides of the chrome
    // threshold, because that is where the glyph changes what it is doing.
    [
        0.05f32, 0.09, 0.2, 0.4, 0.55, 0.9, 1.7, 3.0, 6.0, 12.0, 24.0,
    ]
    .into_iter()
    .map(|zoom| Camera {
        cx: 150.0,
        cy: 120.0,
        zoom,
    })
    .collect()
}

// The guard against gpui's sprite atlas, and the reason every dynamic size
// on the map goes through `quantize`: `paint_svg` keys its atlas on the
// rasterized pixel size, so a glyph whose size follows the camera smoothly
// would mint a fresh tile and a fresh GPU upload on every frame of a zoom.
// A ladder of fifteen values cannot.
#[test]
fn every_glyph_size_is_on_the_shared_ladder() {
    let scene = one_of_each(State::OK);
    let mut seen = 0usize;
    for camera in cameras() {
        for job in walk_at(&scene, camera).icons.iter() {
            let bounds = icon_bounds(job);
            let side = f32::from(bounds.size.width);
            assert_eq!(
                side,
                f32::from(bounds.size.height),
                "zoom {}: a glyph box is not square",
                camera.zoom
            );
            assert!(
                k10s_theme::SIZE_STEPS.contains(&side),
                "zoom {}: {side} px is not a ladder step",
                camera.zoom
            );
            // The ceilings are what stop a satellite forty pixels across
            // from wearing a hundred-pixel glyph at extreme zoom. There is
            // no quad behind a satellite, so nothing else bounds it.
            let ceiling = match job {
                IconJob::Sat(..) => SAT_ICON_MAX_PX,
                _ => WL_ICON_MAX_PX,
            };
            assert!(
                side <= ceiling,
                "zoom {}: a {side} px glyph is over its {ceiling} px ceiling",
                camera.zoom
            );
            seen += 1;
        }
    }
    assert!(seen >= 8, "the sweep drew {seen} glyphs and proves little");
}

#[test]
fn a_workload_glyph_reaches_its_readable_size_without_outgrowing_its_halo() {
    let scene = one_of_each(State::OK);
    let halo = scene.blocks[0].rect;
    let mut biggest = 0.0f32;
    for camera in cameras() {
        let sink = walk_at(&scene, camera);
        for job in sink.icons.iter() {
            let IconJob::Wl(_, primitive) = job else {
                continue;
            };
            let bounds = primitive.bounds;
            let side = f32::from(bounds.size.width);
            biggest = biggest.max(side);
            assert!(
                side <= halo.w * camera.zoom + 1.0 && side <= halo.h * camera.zoom + 1.0,
                "zoom {}: a {side} px glyph in a {:.1}x{:.1} px halo",
                camera.zoom,
                halo.w * camera.zoom,
                halo.h * camera.zoom
            );
        }
    }
    assert!(
        biggest == WL_ICON_MAX_PX,
        "the glyph must reach the shared readable size, got {biggest} px"
    );

    // The other half, and the one a sweep of sizes cannot see: below the
    // chrome threshold there is no header and no pod grid, so the glyph
    // stops being a decoration in a corner and becomes the workload --
    // centred on the card, and legible whatever the card's own size.
    let card = scene.blocks[0].inner;
    let pol = policy(Knobs::default());
    let zoom = pol.block_chrome_min_px * 0.88 / card.w;
    assert!(
        pol.block_icon_shown(halo.w, zoom) && !pol.block_chrome_shown(card.w, zoom),
        "the probe zoom is not in the no-chrome band"
    );
    let camera = Camera {
        cx: card.center().0,
        cy: card.center().1,
        zoom,
    };
    let sink = walk_at_stage(&scene, camera, 2);
    let bounds = sink
        .icons
        .iter()
        .find_map(|job| match job {
            IconJob::Wl(_, primitive) => Some(primitive.bounds),
            _ => None,
        })
        .expect("the workload glyph was drawn without chrome");
    assert!(f32::from(bounds.size.width) <= WL_ICON_MAX_PX);
    assert!(
        f32::from(bounds.size.width) >= WL_MEDALLION_MIN_PX,
        "a {:?} glyph is under the legibility floor and is a smudge",
        bounds.size.width
    );
    let (x, y) = camera.w2s(card.center().0, card.center().1, VW, VH);
    assert!(
        (f32::from(bounds.center().x) - x).abs() <= 1.0
            && (f32::from(bounds.center().y) - y).abs() <= 1.0,
        "the glyph is not centred on the card it stands for"
    );
}

#[test]
fn workload_lod_never_stacks_medallion_card_and_pods() {
    let mut scene = one_of_each(State::OK);
    scene.cells[0].rect.x = scene.blocks[0].inner.center().0 - scene.cells[0].rect.w * 0.5;
    let block = &scene.blocks[0];
    let center = block.inner.center();
    let mid = Camera {
        cx: center.0,
        cy: center.1,
        zoom: 0.20,
    };
    let mid_frame = walk_at(&scene, mid);
    assert_eq!(
        mid_frame.fg.len(),
        0,
        "the namespace overview retained a child card"
    );
    assert_eq!(
        mid_frame
            .icons
            .iter()
            .filter(|job| matches!(job, IconJob::Wl(..) | IconJob::ToolId(..)))
            .count(),
        0
    );

    let near = Camera {
        cx: center.0,
        cy: center.1,
        zoom: 3.0,
    };
    let near_frame = walk_at(&scene, near);
    assert!(
        near_frame.fg.len() >= 4,
        "the detailed representation lost its card, chrome, or pod"
    );
    assert!(
        near_frame
            .labels
            .iter()
            .any(|label| label.text == scene.blocks[0].label.as_ref()),
        "the detailed representation lost its workload heading"
    );
    let pod = Projection::new(near, (VW, VH), (0.0, 0.0)).bounds(scene.cells[0].rect);
    assert!(
        near_frame.fg.iter().any(|quad| quad.bounds == pod),
        "the detailed representation lost its Pod"
    );

    let pol = policy(Knobs {
        no_icons: true,
        ..Knobs::default()
    });
    let opts = FrameOpts {
        theme: &k10s_theme::K10S_DARK.map,
        policy: &pol,
        type_: k10s_theme::Typography::default().map(),
        edges_on: false,
        skip_blocks: false,
        hex: false,
    };
    let mut fallback = Collect::default();
    walk(
        viewport(),
        &scene,
        mid,
        StageBlend::settled(2),
        opts,
        &mut fallback,
    );
    assert_eq!(
        fallback.fg.len(),
        1,
        "icon disable erased the card fallback"
    );
    assert!(fallback.icons.is_empty());
}

// A name is centred in a box and clipped to it, and the box belongs to the
// thing being named. Left-anchored labels are what let a long workload name
// run across the card beside it.
#[test]
fn every_boxed_label_is_confined_to_what_it_names() {
    let scene = one_of_each(State::OK);
    // Named rather than counted: a test that only counts boxed labels
    // passes when one whole class of them loses its box, which is exactly
    // what happened to the workload name the first time this was written.
    let must_be_boxed = [
        scene.regions[0].label.to_string(),
        scene.blocks[0].label.to_string(),
        scene.sats[0].label.to_string(),
        scene.sats[0].ext.detail.to_string(),
    ];
    let mut boxed = 0usize;
    let mut seen: Vec<String> = Vec::new();
    for camera in cameras() {
        let sink = walk_at(&scene, camera);
        for job in sink.labels.iter() {
            if must_be_boxed.contains(&job.text.to_string()) {
                assert!(
                    job.width > 0.0,
                    "zoom {}: {:?} was set loose instead of centred in a box",
                    camera.zoom,
                    job.text
                );
                if !seen.contains(&job.text.to_string()) {
                    seen.push(job.text.to_string());
                }
            }
            if job.width == 0.0 {
                continue;
            }
            boxed += 1;
            assert!(
                job.width.is_finite() && job.width > 0.0,
                "zoom {}: a label box of {}",
                camera.zoom,
                job.width
            );
            assert!(job.size_px >= 6.0, "a label below the legibility floor");
        }
    }
    assert!(boxed >= 4, "only {boxed} boxed labels in the sweep");
    assert_eq!(
        seen.len(),
        must_be_boxed.len(),
        "the sweep never drew one of the labels it is checking: {seen:?}"
    );
}

#[test]
fn a_namespace_title_uses_a_font_that_fits_its_reserved_header() {
    let mut scene = one_of_each(State::OK);
    scene.regions[0].rect = Rect::new(0.0, 0.0, 1500.0, 1000.0);
    scene.bounds = scene.regions[0].rect;
    let child_y = k10s_core::layout::NS_HEADER + k10s_core::layout::NS_PAD;
    scene.blocks[0].rect.y = child_y;
    scene.blocks[0].inner.y = child_y;
    scene.cells[0].rect.y = child_y + 36.0;
    scene.sats[0].rect.y = child_y + 10.0;
    scene.rebuild_spatial_index();
    let camera = Camera {
        cx: 750.0,
        cy: 500.0,
        zoom: 0.4,
    };
    let pol = policy(Knobs::default());
    let blend = StageBlend::settled(2);
    let opts = FrameOpts {
        theme: &k10s_theme::K10S_DARK.map,
        policy: &pol,
        type_: k10s_theme::Typography::default().map(),
        edges_on: false,
        skip_blocks: false,
        hex: false,
    };
    let mut painted = Collect::default();
    let counts = walk(viewport(), &scene, camera, blend, opts, &mut painted);
    let title = painted
        .labels
        .iter()
        .find(|job| job.text == scene.regions[0].label.as_ref())
        .expect("the reserved header can fit the namespace title");
    assert_eq!(title.size_px, 20.0);
    assert_eq!(title.width, 600.0);
    assert!(title.height <= 32.0);
    let (_, child_top) = camera.w2s(0.0, child_y, VW, VH);
    assert!(title.y + title.height <= child_top);
    let expected = crate::lod::cull(&scene, &camera, blend, VW, VH, opts);
    assert_eq!(counts.labels, painted.labels.len());
    assert_eq!(expected.labels, counts.labels);
}

#[test]
fn an_empty_namespace_centers_its_identity_and_exact_zero_total() {
    let mut scene = one_of_each(State::OK);
    scene.regions[0].children = 0..0;
    scene.regions[0].ext.workload_label = NsExt::workload_label(0);
    scene.blocks.clear();
    scene.cells.clear();
    scene.sats.clear();
    scene.rebuild_spatial_index();
    let (cx, cy) = scene.regions[0].rect.center();
    for zoom in [0.5, 1.0, 2.0] {
        let painted = walk_at(&scene, Camera { cx, cy, zoom });
        assert_eq!(painted.labels.len(), 2);
        let name = &painted.labels[0];
        let count = &painted.labels[1];
        assert_eq!(name.text, scene.regions[0].label.as_ref());
        assert_eq!(count.text, "0 workloads");
        assert!(name.whole_line && count.whole_line);
        assert!((name.y + count.y + count.height - VH).abs() < 0.001);
        assert!((name.x + name.width * 0.5 - VW * 0.5).abs() < 0.001);
    }
}

#[test]
fn namespace_corners_stay_uniform_across_detail_thresholds_and_relayout() {
    let scene = one_of_each(State::OK);
    let mut previous = 0.0;
    for short in [15.0, 95.99, 96.0, 96.01, 400.0, 700.0] {
        let camera = Camera {
            cx: 150.0,
            cy: 120.0,
            zoom: short / scene.regions[0].rect.h,
        };
        let painted = walk_at(&scene, camera).bg[1].corner_radii;
        let projected_short = scene.regions[0].rect.h * camera.zoom;
        assert_eq!(painted, island_radii(projected_short));
        assert_eq!(
            painted,
            Projection::new(camera, (VW, VH), (0.0, 0.0))
                .region(scene.regions[0].rect)
                .corners,
        );
        assert_eq!(painted.top_left, painted.top_right);
        assert_eq!(painted.top_left, painted.bottom_left);
        assert_eq!(painted.top_left, painted.bottom_right);
        let radius = f32::from(painted.top_left);
        assert!(radius > 0.0 && radius <= short * 0.5);
        assert!(radius >= previous && radius - previous <= short * 0.5);
        previous = radius;

        let mut moved = scene.clone();
        moved.regions[0].rect.x += 1.0;
        moved.regions[0].rect.w *= 1.5;
        assert_eq!(walk_at(&moved, camera).bg[1].corner_radii, painted);
    }
}

#[test]
fn single_pod_targets_do_not_shrink_when_card_and_pod_detail_appear() {
    let scene = small_workload(State::OK, true);
    let pol = policy(Knobs::default());
    let path = crate::PickPath {
        region: 0,
        block: Some(0),
        cell: None,
        sat: None,
    };
    let (cx, cy) = scene.blocks[0].inner.center();
    for font in [8.0, 14.0, 24.0, 32.0] {
        let type_ = k10s_theme::MapType {
            workload: font,
            pod: font,
            ..Default::default()
        };
        let mut previous = None;
        let mut cards = 0;
        for step in 1..=2400 {
            let zoom = step as f32 * 0.01;
            let camera = Camera { cx, cy, zoom };
            let stage = pol.stage_for_zoom(zoom);
            let Some(target) = crate::primitive::mark_primitive_typed(
                &scene,
                path,
                camera,
                &pol,
                stage,
                (1600.0, 1000.0),
                (0.0, 0.0),
                type_,
            ) else {
                continue;
            };
            if let Some((width, height)) = previous {
                assert!(
                    f32::from(target.bounds.size.width) + 0.01 >= width
                        && f32::from(target.bounds.size.height) + 0.01 >= height,
                    "font {font}, zoom {zoom}, stage {stage}: {target:?} shrank from {previous:?}"
                );
            }
            previous = Some((
                f32::from(target.bounds.size.width),
                f32::from(target.bounds.size.height),
            ));
            if crate::primitive::workload_presentation(
                &pol,
                scene.blocks[0].inner,
                zoom,
                stage,
                type_,
            )
            .card_shown()
            {
                cards += 1;
            }
        }
        assert!(cards > 100, "the sweep must cross real card transitions");
    }
}

#[test]
fn a_workload_identity_glyph_never_shrinks_when_it_enters_the_header() {
    let pol = policy(Knobs::default());
    for attachments in [false, true] {
        let mut scene = small_workload(State::OK, true);
        if !attachments {
            scene.blocks[0].rect = scene.blocks[0].inner;
            scene.blocks[0].sats = 0..0;
            scene.sats.clear();
        }
        scene.rebuild_spatial_index();
        let (cx, cy) = scene.blocks[0].inner.center();
        for font in [8.0, 14.0, 24.0, 32.0] {
            let type_ = MapType {
                workload: font,
                pod: font,
                ..Default::default()
            };
            let opts = FrameOpts {
                theme: &k10s_theme::K10S_DARK.map,
                policy: &pol,
                type_,
                edges_on: false,
                skip_blocks: false,
                hex: false,
            };
            let mut previous = 0.0;
            let mut medallions = 0;
            let mut headers = 0;
            for step in 1..=2400 {
                let zoom = step as f32 * 0.01;
                let camera = Camera { cx, cy, zoom };
                let stage = pol.stage_for_zoom(zoom);
                let presentation =
                    workload_presentation(&pol, scene.blocks[0].inner, zoom, stage, type_);
                let mut painted = Collect::default();
                walk(
                    viewport(),
                    &scene,
                    camera,
                    StageBlend::settled(stage),
                    opts,
                    &mut painted,
                );
                let glyphs: Vec<_> = painted
                    .icons
                    .iter()
                    .filter_map(|job| {
                        if let IconJob::Wl(_, primitive) = job {
                            Some(primitive.bounds)
                        } else {
                            None
                        }
                    })
                    .collect();
                if stage >= 2
                    && matches!(
                        presentation,
                        k10s_atlas::WorkloadPresentation::Medallion
                            | k10s_atlas::WorkloadPresentation::Detailed
                    )
                {
                    assert_eq!(
                        glyphs.len(),
                        1,
                        "zoom {zoom}, font {font}, {presentation:?}"
                    );
                    let side = f32::from(glyphs[0].size.width);
                    assert!(
                        side >= previous,
                        "glyph shrank from {previous} to {side} at zoom {zoom}"
                    );
                    assert!(side <= WL_ICON_MAX_PX);
                    previous = side;
                    if presentation == k10s_atlas::WorkloadPresentation::Medallion {
                        medallions += 1;
                    } else {
                        headers += 1;
                    }
                }
            }
            assert!(
                medallions > 10 && headers > 10,
                "both identity forms must be exercised"
            );
        }
    }
}

// A pod inside its termination grace period is drawn hollow rather than in
// a colour of its own: same cell, same severity, same one quad, and the
// whole difference is visible at a glance during a scale-down.
#[test]
fn a_terminating_pod_is_hollow_and_costs_the_same_quad() {
    let camera = Camera {
        cx: 100.0,
        cy: 100.0,
        zoom: 4.0,
    };
    let running = walk_at(&one_of_each(State::of(ReasonId::RUNNING)), camera);
    let leaving = walk_at(&one_of_each(State::of(ReasonId::TERMINATING)), camera);
    assert_eq!(
        (running.bg.len(), running.fg.len()),
        (leaving.bg.len(), leaving.fg.len()),
        "the two states cost different numbers of quads"
    );

    let pod_of = |sink: &Collect| {
        sink.fg
            .iter()
            .find(|quad| f32::from(quad.bounds.size.width) == 10.0 * camera.zoom)
            .expect("the pod cell was drawn")
            .clone()
    };
    let running = pod_of(&running);
    let leaving = pod_of(&leaving);
    assert_eq!(running.bounds, leaving.bounds, "the cell moved");
    assert!(
        f32::from(running.border_widths.top) == 0.0 && f32::from(leaving.border_widths.top) > 0.0,
        "a terminating pod must be the outlined one"
    );
    assert_ne!(
        running.background, leaving.background,
        "a terminating pod must not be filled like a running one"
    );
}

#[test]
fn severity_footers_keep_screen_height_and_stay_inside_the_card_shape() {
    for inner in [
        Rect::new(40.0, 40.0, 30.0, 56.0),
        Rect::new(40.0, 40.0, 120.0, 0.5),
    ] {
        let (cx, cy) = inner.center();
        for zoom in [
            0.05,
            1.0,
            crate::lod::STAGE_POD_LABEL,
            crate::lod::MAX_USEFUL_ZOOM,
            40.0,
        ] {
            let camera = Camera { cx, cy, zoom };
            for origin in [(0.0, 0.0), (13.25, 27.75)] {
                let projection = Projection::new(camera, (VW, VH), origin);
                let body = projection.card(inner);
                let footer = body.severity_strip();
                let height = f32::from(footer.bounds.size.height);
                assert!(height > 0.0 && height <= 4.0);
                assert!(height <= f32::from(body.bounds.size.height));
                assert!(footer.bounds.origin.x >= body.bounds.origin.x);
                assert!(footer.bounds.origin.y >= body.bounds.origin.y);
                assert!(footer.bounds.right() <= body.bounds.right());
                assert_eq!(footer.bounds.bottom(), body.bounds.bottom());
                assert_eq!(footer.corners, Corners::all(px(0.0)));
                if f32::from(body.bounds.size.height) >= 80.0 {
                    assert_eq!(height, 4.0);
                }
                for fx in [0.25, 0.5, 0.75] {
                    for fy in [0.25, 0.5, 0.75] {
                        let point = point(
                            footer.bounds.origin.x + footer.bounds.size.width * fx,
                            footer.bounds.origin.y + footer.bounds.size.height * fy,
                        );
                        assert!(footer.contains(point));
                        assert!(body.contains(point));
                    }
                }
                if f32::from(body.corners.bottom_left) > height {
                    let corner = point(
                        body.bounds.origin.x + px(0.25),
                        body.bounds.bottom() - px(0.25),
                    );
                    assert!(!body.contains(corner));
                    assert!(!footer.contains(corner));
                }
            }
        }
    }
}

#[test]
fn a_painted_severity_footer_picks_its_card_without_filling_the_rounded_gap() {
    let mut scene = one_of_each(State::OK);
    scene.blocks[0].inner = Rect::new(40.0, 40.0, 30.0, 56.0);
    let inner = scene.blocks[0].inner;
    let pol = policy(Knobs::default());
    let type_ = MapType::default();
    let card_zoom = ((crate::primitive::name_min_width(type_) + TEXT_PAD * 2.0 + 1.0) / inner.w)
        .max(
            (crate::primitive::name_band_height(type_)
                + TEXT_PAD
                + crate::primitive::HEADING_CLEARANCE
                + 1.0)
                * 2.0
                / inner.h,
        );
    for zoom in [
        card_zoom,
        crate::lod::STAGE_POD_LABEL,
        crate::lod::MAX_USEFUL_ZOOM,
        40.0,
    ] {
        let camera = Camera {
            cx: inner.center().0,
            cy: scene.cells[0].rect.center().1,
            zoom,
        };
        let blend = StageBlend::settled(pol.stage_for_zoom(zoom));
        let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
        let body = projection
            .workload_card(&scene, 0, blend.walk_stage(), MapType::default())
            .body;
        let footer = body.severity_strip();
        let opts = FrameOpts {
            policy: &pol,
            theme: &k10s_theme::K10S_DARK.map,
            type_: k10s_theme::MapType::default(),
            edges_on: false,
            skip_blocks: false,
            hex: false,
        };
        let mut sink = Collect::default();
        let stats = walk(viewport(), &scene, camera, blend, opts, &mut sink);
        assert_eq!(
            stats,
            crate::lod::cull(&scene, &camera, blend, VW, VH, opts)
        );
        let painted = sink
            .fg
            .iter()
            .find(|quad| quad.bounds == footer.bounds)
            .expect("the card emits its bounded severity footer");
        assert_eq!(painted.bounds.size.height, px(4.0));
        assert_eq!(painted.corner_radii, footer.corners);
        let probe = footer.bounds.center();
        assert!(body.contains(probe));
        assert!(viewport().contains(&probe));
        let path = crate::pick::pick(
            &scene,
            &camera,
            &pol,
            blend,
            VW,
            VH,
            f32::from(probe.x),
            f32::from(probe.y),
        )
        .expect("the footer belongs to the workload");
        assert_eq!((path.block, path.cell, path.sat), (Some(0), None, None));

        let gap = point(
            body.bounds.origin.x + px(0.25),
            body.bounds.bottom() - px(0.25),
        );
        assert!(body.bounds.contains(&gap));
        assert!(!body.contains(gap));
        assert!(!footer.contains(gap));
        assert!(projection.region(scene.regions[0].rect).contains(gap));
        let path = crate::pick::pick(
            &scene,
            &camera,
            &pol,
            blend,
            VW,
            VH,
            f32::from(gap.x),
            f32::from(gap.y),
        )
        .expect("the rounded gap belongs to the region");
        assert_eq!((path.block, path.cell, path.sat), (None, None, None));
    }
}

#[test]
fn detailed_pods_keep_identity_status_and_picking_inside_the_same_body() {
    let mut scene = one_of_each(State::of(ReasonId::FAILED));
    scene.blocks[0].label = Arc::from("payments-redis-primary");
    let cell = &scene.cells[0];
    let (cx, cy) = cell.rect.center();
    let pol = policy(Knobs::default());
    let mut details = 0;
    for ui_size in [8.0, 14.0, 24.0, 32.0] {
        let type_ = k10s_theme::Typography {
            ui_size,
            ..Default::default()
        }
        .map();
        for zoom in [
            crate::lod::STAGE_POD_LABEL,
            crate::lod::MAX_USEFUL_ZOOM,
            40.0,
        ] {
            let camera = Camera { cx, cy, zoom };
            let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
            let heading = projection.workload_card(&scene, 0, 3, type_).heading;
            let body = projection
                .cell(cell.rect, 3, type_, heading)
                .expect("the centered pod clears its heading");
            let mut sink = Collect::default();
            let opts = FrameOpts {
                policy: &pol,
                theme: &k10s_theme::K10S_DARK.map,
                type_,
                edges_on: false,
                skip_blocks: false,
                hex: false,
            };
            let stats = walk(
                viewport(),
                &scene,
                camera,
                StageBlend::settled(3),
                opts,
                &mut sink,
            );
            assert_eq!(
                stats,
                crate::lod::cull(&scene, &camera, StageBlend::settled(3), VW, VH, opts)
            );
            assert!(sink.fg.iter().any(|quad| quad.bounds == body.bounds));
            let path = crate::pick::pick_typed(
                &scene,
                &camera,
                &pol,
                StageBlend::settled(3),
                VW,
                VH,
                VW * 0.5,
                VH * 0.5,
                type_,
            )
            .expect("the painted pod is pickable");
            assert_eq!(path.cell, Some(0));
            assert_eq!(
                crate::primitive::mark_primitive_typed(
                    &scene,
                    path,
                    camera,
                    &pol,
                    3,
                    (VW, VH),
                    (0.0, 0.0),
                    type_
                ),
                Some(body),
            );
            let width = f32::from(body.bounds.size.width);
            if width < POD_DETAIL_MIN_WIDTH {
                assert!(
                    !sink
                        .labels
                        .iter()
                        .any(|job| job.text == cell.label.as_ref())
                );
                continue;
            }
            details += 1;
            assert!((144.0..=224.0).contains(&width));
            assert!((64.0..=104.0).contains(&f32::from(body.bounds.size.height)));
            let name = sink
                .labels
                .iter()
                .find(|job| job.text == cell.label.as_ref())
                .expect("actual pod name");
            assert_eq!(&name.text[name.text_start..], "0");
            assert_eq!(name.size_px, ui_size);
            let status = sink
                .labels
                .iter()
                .find(|job| job.text == "Failed")
                .expect("actual pod status");
            for job in [name, status] {
                assert_eq!(job.size_px, ui_size);
                assert!(job.width > 0.0);
                assert!(job.x >= f32::from(body.bounds.origin.x) + TEXT_PAD);
                assert!(job.y >= f32::from(body.bounds.origin.y) + TEXT_PAD);
                assert!(job.x + job.width <= f32::from(body.bounds.right()));
                assert!(job.y + job.height <= f32::from(body.bounds.bottom()) - TEXT_PAD);
            }
            let mark = sink
                .icons
                .iter()
                .find_map(|job| match job {
                    IconJob::Status(Severity::Err, primitive) => Some(primitive),
                    _ => None,
                })
                .expect("the status has its vector mark");
            assert!(mark.bounds.origin.x >= body.bounds.origin.x);
            assert!(mark.bounds.origin.y >= body.bounds.origin.y);
            assert!(mark.bounds.right() <= body.bounds.right());
            assert!(mark.bounds.bottom() <= body.bounds.bottom());
        }
    }
    assert!(
        details >= 8,
        "the font and zoom sweep did not reach enough detail cases"
    );
}

#[test]
fn a_pinned_heading_selects_the_workload_and_excludes_obscured_pods() {
    let mut scene = one_of_each(State::OK);
    let mut second = scene.cells[0].clone();
    second.rect.x += k10s_core::layout::POD_PITCH;
    scene.cells.push(second);
    scene.blocks[0].children = 0..2;
    let camera = Camera {
        cx: 55.0,
        cy: 105.0,
        zoom: 16.0,
    };
    let type_ = k10s_theme::MapType::default();
    let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
    let heading = projection
        .workload_card(&scene, 0, 3, type_)
        .heading
        .expect("the workload enters the viewport");
    assert_eq!(
        f32::from(heading.bounds.origin.y),
        crate::primitive::top_clearance(type_, (VW, VH))
    );
    assert!(
        projection
            .cell(scene.cells[0].rect, 3, type_, Some(heading))
            .is_none()
    );
    let pol = policy(Knobs::default());
    let center = heading.bounds.center();
    let path = crate::pick::pick_typed(
        &scene,
        &camera,
        &pol,
        StageBlend::settled(3),
        VW,
        VH,
        f32::from(center.x),
        f32::from(center.y),
        type_,
    )
    .expect("the pinned heading is a workload target");
    assert_eq!(path.block, Some(0));
    assert_eq!(path.cell, None);
    let cell_path = crate::PickPath {
        region: 0,
        block: Some(0),
        cell: Some(0),
        sat: None,
    };
    assert!(
        crate::primitive::mark_primitive_typed(
            &scene,
            cell_path,
            camera,
            &pol,
            3,
            (VW, VH),
            (0.0, 0.0),
            type_,
        )
        .is_none()
    );
    assert_eq!(
        walk_at(&scene, camera).fg.len(),
        3,
        "the obscured pod still emitted a body"
    );
}

#[test]
fn large_workload_names_stay_in_the_visible_band_while_panning() {
    let mut scene = one_of_each(State::OK);
    let mut second = scene.cells[0].clone();
    second.rect.x += k10s_core::layout::POD_PITCH;
    scene.cells.push(second);
    scene.blocks[0].children = 0..2;
    let type_ = k10s_theme::MapType::default();
    let mut observed = 0;
    for cx in [55.0, 95.0, 130.0] {
        for cy in [81.0, 100.0, 125.0] {
            let camera = Camera { cx, cy, zoom: 16.0 };
            let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
            let band = projection
                .workload_card(&scene, 0, 3, type_)
                .heading
                .expect("part of the workload remains visible");
            let frame = walk_at(&scene, camera);
            let name = frame
                .labels
                .iter()
                .find(|job| job.text == scene.blocks[0].label.as_ref())
                .expect("pinned workload name");
            assert!(name.x >= f32::from(band.bounds.origin.x) + TEXT_PAD);
            assert!(name.x + name.width <= f32::from(band.bounds.right()) - TEXT_PAD);
            assert!(name.y >= f32::from(band.bounds.origin.y) + TEXT_PAD);
            assert!(name.y + name.height <= f32::from(band.bounds.bottom()) - TEXT_PAD);
            observed += 1;
        }
    }
    assert_eq!(observed, 9);
}

#[test]
fn a_workload_without_observed_pods_does_not_claim_health() {
    let mut scene = one_of_each(State::OK);
    scene.blocks[0].children = 0..0;
    scene.cells.clear();
    let camera = Camera {
        cx: 100.0,
        cy: 100.0,
        zoom: 4.0,
    };
    let frame = walk_at(&scene, camera);
    assert!(frame.labels.iter().any(|job| job.text == "0 observed pods"));
    assert!(!frame.labels.iter().any(|job| job.text == "Healthy"));
    assert!(
        frame
            .icons
            .iter()
            .any(|job| matches!(job, IconJob::Status(Severity::Unknown, _)))
    );
}

#[test]
fn published_compact_cards_follow_pods_without_repositioning_either_layout() {
    use crate::overlay::{OverlayFrame, OverlayKind, OverlayMark, StampCache};
    use k10s_core::{IngestEvent, Op, Payload, ResourceEvent, replay};
    use k10s_world::{LayoutMode, PublishBench};

    let pol = policy(Knobs::default());
    let (vw, vh) = (8000.0, 6000.0);
    for mode in [LayoutMode::Spread, LayoutMode::Dense] {
        for pods in [0, 1, 4] {
            for sparse in [false, true] {
                let mut events = vec![
                    replay::scope("ns-prod", "prod", Op::Added),
                    replay::owner("wl-api", "prod", "api", KindId::DEPLOYMENT, Op::Added),
                ];
                if sparse {
                    events.push(replay::instance(
                        "retired",
                        "prod",
                        "wl-api",
                        State::OK,
                        Op::Added,
                    ));
                }
                for index in 0..pods {
                    events.push(replay::instance(
                        &format!("pod-{index}"),
                        "prod",
                        "wl-api",
                        State::OK,
                        Op::Added,
                    ));
                }
                events.push(IngestEvent::Resource(ResourceEvent {
                    kind: KindId::SERVICE,
                    uid: "svc-api".into(),
                    namespace: "prod".into(),
                    name: "svc-api".into(),
                    resource_version: 0,
                    parent: Some("wl-api".into()),
                    op: Op::Added,
                    payload: Payload::Attached {
                        kind: KindId::SERVICE,
                        detail: "ClusterIP".into(),
                    },
                }));
                let mut bench = PublishBench::new(&events, mode);
                if sparse {
                    bench.apply_events(&[replay::instance(
                        "retired",
                        "prod",
                        "wl-api",
                        State::OK,
                        Op::Deleted,
                    )]);
                    bench.run_publish();
                }
                let scene = bench.snapshot();
                let index = scene.locate("wl-api").unwrap().slot;
                let block = &scene.blocks[index];
                assert_eq!(block.children.len(), pods);
                if sparse && pods > 0 {
                    assert!(!scene.block_cells.is_empty());
                    assert!(scene.ids.cells[0].is_empty());
                }
                let cell_index = scene.block_cell_indices(index).next();
                if pods == 1 {
                    let rect = scene.cells[cell_index.unwrap()].rect;
                    let (header, height) = match mode {
                        LayoutMode::Spread => (26.0, 56.0),
                        LayoutMode::Dense => (16.0, 46.0),
                    };
                    let (width, column) = if sparse { (44.0, 24.0) } else { (30.0, 10.0) };
                    assert_eq!((block.inner.w, block.inner.h), (width, height));
                    assert!((rect.x - block.inner.x - column).abs() < 0.001);
                    assert!((rect.y - block.inner.y - header - 10.0).abs() < 0.001);
                }
                let path = crate::PickPath {
                    region: block.ext.ns,
                    block: Some(index as u32),
                    cell: None,
                    sat: None,
                };
                for (font, zoom, heading_height, empty_height, detail_height, detail_width) in [
                    (14.0, 14.4, 80.0, 100.0, 164.0, 168.0),
                    (32.0, 32.0, 103.0, 123.0, 226.0, 364.0),
                ] {
                    let type_ = MapType {
                        workload: font,
                        pod: font,
                        ..Default::default()
                    };
                    for stage in [2, 3] {
                        let center = if pods == 1 {
                            scene.cells[cell_index.unwrap()].rect.center()
                        } else {
                            block.inner.center()
                        };
                        let camera = Camera {
                            cx: center.0,
                            cy: center.1,
                            zoom,
                        };
                        let projection = Projection::new(camera, (vw, vh), (0.0, 0.0));
                        let geometry = projection.workload_card(&scene, index, stage, type_);
                        let heading = geometry.heading.expect("the complete heading fits");
                        assert_eq!(heading.bounds.size.height, px(heading_height));
                        if pods == 0 {
                            assert_eq!(
                                geometry.body.bounds.size,
                                size(px(detail_width), px(empty_height))
                            );
                        } else if pods == 1 {
                            let pod = projection
                                .cell(
                                    scene.cells[cell_index.unwrap()].rect,
                                    stage,
                                    type_,
                                    Some(heading),
                                )
                                .expect("the Pod clears its heading");
                            let height = if stage == 2 {
                                heading_height + 84.0
                            } else {
                                detail_height
                            };
                            assert_eq!(
                                geometry.body.bounds.size,
                                size(px(detail_width), px(height))
                            );
                            assert_eq!(
                                pod.bounds.center(),
                                projection
                                    .bounds(scene.cells[cell_index.unwrap()].rect)
                                    .center()
                            );
                            assert_eq!(pod.bounds.origin.y - heading.bounds.bottom(), px(4.0));
                            assert_eq!(
                                geometry.body.bounds.bottom() - pod.bounds.bottom(),
                                px(8.0)
                            );
                            let old_hub = projection
                                .bounds(Rect::new(
                                    block.inner.center().0,
                                    block.inner.center().1,
                                    0.0,
                                    0.0,
                                ))
                                .origin;
                            let hub_inside = !sparse && mode == LayoutMode::Dense && font == 14.0;
                            assert_eq!(geometry.body.contains(old_hub), hub_inside);
                            assert_eq!(
                                crate::pick::pick_typed(
                                    &scene,
                                    &camera,
                                    &pol,
                                    StageBlend::settled(stage),
                                    vw,
                                    vh,
                                    f32::from(old_hub.x),
                                    f32::from(old_hub.y),
                                    type_,
                                )
                                .unwrap()
                                .block,
                                hub_inside.then_some(index as u32)
                            );
                        } else {
                            assert_eq!(geometry.body, projection.card(block.inner));
                        }
                        let opts = FrameOpts {
                            policy: &pol,
                            theme: &k10s_theme::K10S_DARK.map,
                            type_,
                            edges_on: false,
                            skip_blocks: false,
                            hex: false,
                        };
                        for origin in [(0.0, 0.0), (17.25, 23.5)] {
                            let mut sink = Collect::default();
                            let bounds = Bounds {
                                origin: point(px(origin.0), px(origin.1)),
                                size: size(px(vw), px(vh)),
                            };
                            let stats = walk(
                                bounds,
                                &scene,
                                camera,
                                StageBlend::settled(stage),
                                opts,
                                &mut sink,
                            );
                            assert_eq!(
                                stats,
                                crate::lod::cull(
                                    &scene,
                                    &camera,
                                    StageBlend::settled(stage),
                                    vw,
                                    vh,
                                    opts
                                )
                            );
                            let translated = Projection::new(camera, (vw, vh), origin)
                                .workload_card(&scene, index, stage, type_);
                            assert!(
                                sink.fg
                                    .iter()
                                    .any(|quad| quad.bounds == translated.body.bounds)
                            );
                            assert_eq!(
                                crate::primitive::mark_primitive_typed(
                                    &scene,
                                    path,
                                    camera,
                                    &pol,
                                    stage,
                                    (vw, vh),
                                    origin,
                                    type_,
                                ),
                                Some(translated.body)
                            );
                            assert_eq!(sink.curves.len(), usize::from(mode.emits_attachments()));
                            for [hub, _, end] in sink.curves {
                                let expected = if pods == 1 {
                                    translated.body.bounds.center()
                                } else {
                                    Projection::new(camera, (vw, vh), origin)
                                        .bounds(Rect::new(
                                            block.inner.center().0,
                                            block.inner.center().1,
                                            0.0,
                                            0.0,
                                        ))
                                        .origin
                                };
                                assert_eq!(hub, (f32::from(expected.x), f32::from(expected.y)));
                                assert!(translated.body.contains(point(px(hub.0), px(hub.1))));
                                let sat = scene.block_sat_indices(index).next().unwrap();
                                let target = Projection::new(camera, (vw, vh), origin)
                                    .bounds(scene.sats[sat].rect)
                                    .center();
                                assert!((end.0 - f32::from(target.x)).abs() < 0.001);
                                assert!((end.1 - f32::from(target.y)).abs() < 0.001);
                            }
                        }
                        let overlay = OverlayFrame {
                            kind: Some(OverlayKind::Policy),
                            marks: vec![OverlayMark {
                                uid: "wl-api".into(),
                                tint: Some(Severity::Warn),
                                sparkline: None,
                                label: None,
                            }],
                        };
                        let stamps = overlay.visible_stamps_typed(
                            &mut StampCache::default(),
                            &scene,
                            camera,
                            &pol,
                            StageBlend::settled(stage),
                            vw,
                            vh,
                            type_,
                        );
                        assert_eq!(stamps.len(), 1);
                        assert_eq!(
                            stamps[0].screen,
                            Rect::new(
                                f32::from(geometry.body.bounds.origin.x),
                                f32::from(geometry.body.bounds.origin.y),
                                f32::from(geometry.body.bounds.size.width),
                                f32::from(geometry.body.bounds.size.height),
                            )
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn a_compact_heading_requires_room_above_the_visible_pod() {
    use crate::overlay::{OverlayFrame, OverlayKind, OverlayMark, StampCache};

    let scene = small_workload(State::OK, true);
    let type_ = MapType::default();
    let pol = policy(Knobs::default());
    for (cy, top) in [(100.0, 164.0), (105.0, 84.0)] {
        let camera = Camera {
            cx: 55.0,
            cy,
            zoom: 16.0,
        };
        for stage in [2, 3] {
            let blend = StageBlend::settled(stage);
            let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
            let geometry = projection.workload_card(&scene, 0, stage, type_);
            assert!(geometry.heading.is_none());
            let pod = projection
                .cell(scene.cells[2].rect, stage, type_, geometry.heading)
                .unwrap();
            assert_eq!(pod.bounds.origin.y, px(top));
            assert_eq!(pod.bounds.bottom(), px(top + 64.0));
            let mut sink = Collect::default();
            let opts = FrameOpts {
                policy: &pol,
                theme: &k10s_theme::K10S_DARK.map,
                type_,
                edges_on: false,
                skip_blocks: false,
                hex: false,
            };
            let stats = walk(viewport(), &scene, camera, blend, opts, &mut sink);
            assert_eq!(
                stats,
                crate::lod::cull(&scene, &camera, blend, VW, VH, opts)
            );
            assert!(sink.fg.iter().any(|quad| quad.bounds == pod.bounds));
            assert!(
                !sink
                    .labels
                    .iter()
                    .any(|label| label.text == scene.blocks[0].label.as_ref())
            );
            let center = pod.bounds.center();
            let picked = crate::pick::pick_typed(
                &scene,
                &camera,
                &pol,
                blend,
                VW,
                VH,
                f32::from(center.x),
                f32::from(center.y),
                type_,
            )
            .unwrap();
            assert_eq!(picked.cell, Some(2));
            assert_eq!(
                crate::primitive::mark_primitive_typed(
                    &scene,
                    picked,
                    camera,
                    &pol,
                    stage,
                    (VW, VH),
                    (0.0, 0.0),
                    type_,
                ),
                Some(pod)
            );
            let overlay = OverlayFrame {
                kind: Some(OverlayKind::Policy),
                marks: vec![OverlayMark {
                    uid: "pod-current".into(),
                    tint: Some(Severity::Warn),
                    sparkline: None,
                    label: None,
                }],
            };
            let stamps = overlay.visible_stamps_typed(
                &mut StampCache::default(),
                &scene,
                camera,
                &pol,
                blend,
                VW,
                VH,
                type_,
            );
            assert_eq!(stamps.len(), 1);
            assert_eq!(
                stamps[0].screen,
                Rect::new(
                    f32::from(pod.bounds.origin.x),
                    f32::from(pod.bounds.origin.y),
                    f32::from(pod.bounds.size.width),
                    f32::from(pod.bounds.size.height),
                )
            );
        }
    }
}

fn published_edge_workloads(
    mode: k10s_world::LayoutMode,
    pods: usize,
    sparse: bool,
    owners: usize,
) -> Arc<SceneSnapshot> {
    use k10s_core::{IngestEvent, Op, Payload, replay};
    use k10s_world::PublishBench;

    let mut events = Vec::new();
    let mut removals = Vec::new();
    for namespace in 0..2 {
        let ns = format!("ns{namespace}");
        events.push(replay::scope(&format!("ns-{ns}"), &ns, Op::Added));
        for index in 0..owners {
            let uid = format!("wl-{namespace}-{index}");
            let mut owner = replay::owner(&uid, &ns, &uid, KindId::DEPLOYMENT, Op::Added);
            let IngestEvent::Resource(resource) = &mut owner else {
                unreachable!()
            };
            let Payload::Owner { depends_on, .. } = &mut resource.payload else {
                unreachable!()
            };
            depends_on.extend([
                Arc::from(format!("wl-{namespace}-{}", index ^ 1)),
                Arc::from(format!("wl-{}-{index}", 1 - namespace)),
            ]);
            events.push(owner);
            if sparse {
                let retired = format!("retired-{namespace}-{index}");
                events.push(replay::instance(&retired, &ns, &uid, State::OK, Op::Added));
                removals.push(replay::instance(
                    &retired,
                    &ns,
                    &uid,
                    State::OK,
                    Op::Deleted,
                ));
            }
            for pod in 0..pods {
                events.push(replay::instance(
                    &format!("pod-{namespace}-{index}-{pod}"),
                    &ns,
                    &uid,
                    State::OK,
                    Op::Added,
                ));
            }
        }
    }
    let mut bench = PublishBench::new(&events, mode);
    if sparse {
        bench.apply_events(&removals);
        bench.run_publish();
    }
    bench.snapshot()
}

fn expected_published_edge_anchor(
    scene: &SceneSnapshot,
    endpoint: k10s_core::Endpoint,
    camera: Camera,
    viewport: (f32, f32),
    origin: (f32, f32),
    heading_offset: f32,
) -> (f32, f32) {
    let block = &scene.blocks[endpoint.index() as usize];
    let center = if block.children.len() == 1 {
        let cell = scene
            .block_cell_indices(endpoint.index() as usize)
            .next()
            .unwrap();
        scene.cells[cell].rect.center()
    } else {
        block.inner.center()
    };
    let (x, y) = camera.w2s(center.0, center.1, viewport.0, viewport.1);
    (
        origin.0 + x,
        origin.1 + y
            - if block.children.len() == 1 {
                heading_offset
            } else {
                0.0
            },
    )
}

#[test]
fn published_relationships_follow_compact_marks_in_both_directions() {
    use k10s_world::LayoutMode;

    for mode in [LayoutMode::Spread, LayoutMode::Dense] {
        for pods in [0, 1, 4] {
            for sparse in [false, true] {
                let scene = published_edge_workloads(mode, pods, sparse, 2);
                assert_eq!(scene.edges.len(), 8);
                assert_eq!(scene.cross_edges.len(), 4);
                assert_eq!(scene.block_cell_indices(0).len(), pods);
                if sparse && pods == 1 {
                    assert!(scene.ids.cells[0].is_empty());
                    let cell = scene.block_cell_indices(0).next().unwrap();
                    assert_eq!(scene.cells[cell].rect.x - scene.blocks[0].inner.x, 24.0);
                    assert_eq!(scene.blocks[0].inner.w, 44.0);
                }
                let (vw, vh) = (80000.0, 60000.0);
                let center = scene.bounds.center();
                for (font, zoom, offset) in [
                    (8.0, 14.4, 42.0),
                    (14.0, 14.4, 42.0),
                    (24.0, 24.0, 42.5),
                    (32.0, 32.0, 53.5),
                ] {
                    let type_ = MapType {
                        workload: font,
                        pod: font,
                        ..Default::default()
                    };
                    let camera = Camera {
                        cx: center.0,
                        cy: center.1,
                        zoom,
                    };
                    let visible = camera.visible_world(vw, vh);
                    assert!(visible.contains(&scene.bounds));
                    for stage in [2, 3] {
                        for budget in [0, 1, 7, 8, 9] {
                            let mut pol = policy(Knobs::default());
                            pol.max_edges = budget;
                            let opts = FrameOpts {
                                policy: &pol,
                                theme: &k10s_theme::K10S_DARK.map,
                                type_,
                                edges_on: true,
                                skip_blocks: false,
                                hex: false,
                            };
                            let mut original = Vec::new();
                            let count = k10s_atlas::walk_edges(&scene, &visible, budget, |a, b| {
                                original.push((a, b))
                            });
                            assert_eq!(count, budget.min(8));
                            let world_expected: Vec<_> = scene
                                .edges
                                .iter()
                                .take(budget)
                                .map(|edge| {
                                    (
                                        scene.blocks[edge.a.index() as usize].inner.center(),
                                        scene.blocks[edge.b.index() as usize].inner.center(),
                                    )
                                })
                                .collect();
                            assert_eq!(original, world_expected);
                            for origin in [(0.0, 0.0), (17.25, 23.5)] {
                                let mut sink = Collect::default();
                                let stats = walk(
                                    Bounds {
                                        origin: point(px(origin.0), px(origin.1)),
                                        size: size(px(vw), px(vh)),
                                    },
                                    &scene,
                                    camera,
                                    StageBlend::settled(stage),
                                    opts,
                                    &mut sink,
                                );
                                assert_eq!(stats.edges, budget.min(8));
                                assert_eq!(
                                    stats,
                                    crate::lod::cull(
                                        &scene,
                                        &camera,
                                        StageBlend::settled(stage),
                                        vw,
                                        vh,
                                        opts
                                    )
                                );
                                assert_eq!(sink.edges.len(), budget.min(8));
                                for (edge, [a, control, b]) in scene.edges.iter().zip(&sink.edges) {
                                    for (actual, endpoint) in [(*a, edge.a), (*b, edge.b)] {
                                        let expected = expected_published_edge_anchor(
                                            &scene,
                                            endpoint,
                                            camera,
                                            (vw, vh),
                                            origin,
                                            offset,
                                        );
                                        assert!((actual.0 - expected.0).abs() < 0.02);
                                        assert!((actual.1 - expected.1).abs() < 0.02);
                                    }
                                    assert!(control.0.is_finite() && control.1.is_finite());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn indexed_published_relationships_admit_the_displayed_segment_after_the_world_segment_leaves() {
    use k10s_atlas::EdgeSegment;
    use k10s_world::LayoutMode;

    let type_ = MapType::default();
    let (vw, vh) = (64.0, 48.0);
    for mode in [LayoutMode::Spread, LayoutMode::Dense] {
        for sparse in [false, true] {
            let indexed = published_edge_workloads(mode, 1, sparse, 68);
            assert_eq!(indexed.region_edge_indexes.len(), 2);
            assert_eq!(indexed.region_edges[0].len(), 68);
            assert_eq!(indexed.region_edges[1].len(), 68);
            assert_eq!(indexed.cross_edges.len(), 136);
            for cross in [false, true] {
                for reverse in [false, true] {
                    let (target, camera) = indexed
                        .edges
                        .iter()
                        .copied()
                        .find_map(|edge| {
                            let a = &indexed.blocks[edge.a.index() as usize];
                            let b = &indexed.blocks[edge.b.index() as usize];
                            if (a.ext.ns != b.ext.ns) != cross
                                || (edge.a.index() > edge.b.index()) != reverse
                            {
                                return None;
                            }
                            let lower = if a.inner.center().1 >= b.inner.center().1 {
                                edge.a
                            } else {
                                edge.b
                            };
                            let cell = indexed
                                .block_cell_indices(lower.index() as usize)
                                .next()
                                .unwrap();
                            let center = indexed.cells[cell].rect.center();
                            let camera = Camera {
                                cx: center.0,
                                cy: center.1 - 42.0 / 14.4,
                                zoom: 14.4,
                            };
                            let visible = camera.visible_world(vw, vh);
                            let index = if cross {
                                &indexed.cross_edge_index
                            } else {
                                &indexed.region_edge_indexes[a.ext.ns as usize]
                            };
                            index
                                .selective_candidate_count(&visible)
                                .is_some_and(|count| count > 0)
                                .then_some((edge, camera))
                        })
                        .expect(
                            "the real publication must reach a selective nonempty edge-index leaf",
                        );
                    let a = indexed.blocks[target.a.index() as usize].inner.center();
                    let b = indexed.blocks[target.b.index() as usize].inner.center();
                    let visible = camera.visible_world(vw, vh);
                    assert!(!EdgeSegment { a, b }.bounds().intersects(&visible));
                    for stage in [2, 3] {
                        let expected: Vec<_> = indexed
                            .edges
                            .iter()
                            .copied()
                            .filter(|edge| {
                                let (ax, ay) = expected_published_edge_anchor(
                                    &indexed,
                                    edge.a,
                                    camera,
                                    (vw, vh),
                                    (0.0, 0.0),
                                    42.0,
                                );
                                let (bx, by) = expected_published_edge_anchor(
                                    &indexed,
                                    edge.b,
                                    camera,
                                    (vw, vh),
                                    (0.0, 0.0),
                                    42.0,
                                );
                                ax.min(bx) < vw + 8.0
                                    && -8.0 < ax.min(bx) + (ax - bx).abs().max(14.4)
                                    && ay.min(by) < vh + 8.0
                                    && -8.0 < ay.min(by) + (ay - by).abs().max(14.4)
                            })
                            .collect();
                        assert!(expected.contains(&target));
                        for budget in [
                            0,
                            1,
                            expected.len().saturating_sub(1),
                            expected.len(),
                            expected.len() + 1,
                        ] {
                            let mut pol = policy(Knobs::default());
                            pol.max_edges = budget;
                            let opts = FrameOpts {
                                policy: &pol,
                                theme: &k10s_theme::K10S_DARK.map,
                                type_,
                                edges_on: true,
                                skip_blocks: false,
                                hex: false,
                            };
                            for use_index in [false, true] {
                                for cached in [false, true] {
                                    let mut scene = (*indexed).clone();
                                    if !use_index {
                                        scene.region_edge_indexes.clear();
                                        scene.cross_edge_index = Default::default();
                                    }
                                    if !cached {
                                        scene.edge_segments.clear();
                                    }
                                    for origin in [(0.0, 0.0), (17.25, 23.5)] {
                                        let mut sink = Collect::default();
                                        let stats = walk(
                                            Bounds {
                                                origin: point(px(origin.0), px(origin.1)),
                                                size: size(px(vw), px(vh)),
                                            },
                                            &scene,
                                            camera,
                                            StageBlend::settled(stage),
                                            opts,
                                            &mut sink,
                                        );
                                        assert_eq!(stats.edges, budget.min(expected.len()));
                                        assert_eq!(
                                            stats,
                                            crate::lod::cull(
                                                &scene,
                                                &camera,
                                                StageBlend::settled(stage),
                                                vw,
                                                vh,
                                                opts
                                            )
                                        );
                                        assert_eq!(sink.edges.len(), budget.min(expected.len()));
                                        for (edge, [a, control, b]) in
                                            expected.iter().zip(&sink.edges)
                                        {
                                            for (actual, endpoint) in [(*a, edge.a), (*b, edge.b)] {
                                                let want = expected_published_edge_anchor(
                                                    &scene,
                                                    endpoint,
                                                    camera,
                                                    (vw, vh),
                                                    origin,
                                                    42.0,
                                                );
                                                assert!((actual.0 - want.0).abs() < 0.002);
                                                assert!((actual.1 - want.1).abs() < 0.002);
                                            }
                                            assert!(control.0.is_finite() && control.1.is_finite());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn relationship_projection_preserves_other_endpoint_kinds_and_small_workload_presentations() {
    use k10s_atlas::{Edge, EdgeSegment, Endpoint};

    let scene = one_of_each(State::OK);
    let pol = policy(Knobs::default());
    let type_ = MapType::default();
    for zoom in [0.01, 0.04, 0.14, 0.30, 16.0, 40.0] {
        let camera = Camera {
            cx: 55.0,
            cy: 81.0,
            zoom,
        };
        let projection = Projection::new(camera, (VW, VH), (17.25, 23.5));
        for other in [Endpoint::region(0), Endpoint::cell(0), Endpoint::sat(0)] {
            let world = match other.level() {
                k10s_atlas::Level::Region => scene.regions[0].rect.center(),
                k10s_atlas::Level::Cell => scene.cells[0].rect.center(),
                k10s_atlas::Level::Sat => scene.sats[0].rect.center(),
                k10s_atlas::Level::Block => unreachable!(),
            };
            let hub = scene.blocks[0].inner.center();
            for reverse in [false, true] {
                let (edge, original) = if reverse {
                    (
                        Edge {
                            a: other,
                            b: Endpoint::block(0),
                        },
                        EdgeSegment { a: world, b: hub },
                    )
                } else {
                    (
                        Edge {
                            a: Endpoint::block(0),
                            b: other,
                        },
                        EdgeSegment { a: hub, b: world },
                    )
                };
                let actual = projection.edge_segment(&scene, edge, original, &pol, 3, type_);
                assert_eq!(if reverse { actual.a } else { actual.b }, world);
                let presentation =
                    workload_presentation(&pol, scene.blocks[0].inner, zoom, 3, type_);
                if presentation != k10s_atlas::WorkloadPresentation::Detailed {
                    assert_eq!(actual, original);
                }
                assert_eq!(
                    actual,
                    Projection::new(camera, (VW, VH), (0.0, 0.0))
                        .edge_segment(&scene, edge, original, &pol, 3, type_)
                );
            }
        }
    }
}

#[test]
fn hidden_workloads_keep_literal_world_relationship_anchors() {
    use k10s_atlas::Edge;

    let mut scene = one_of_each(State::OK);
    scene.edges = vec![Edge::blocks(0, 0)];
    scene.rebuild_edge_indexes();
    let camera = Camera {
        cx: 100.0,
        cy: 105.0,
        zoom: 16.0,
    };
    let pol = policy(Knobs::default());
    let opts = FrameOpts {
        policy: &pol,
        theme: &k10s_theme::K10S_DARK.map,
        type_: MapType::default(),
        edges_on: true,
        skip_blocks: true,
        hex: false,
    };
    for stage in [2, 3] {
        for (origin, expected) in [
            ((0.0, 0.0), (800.0, 500.0)),
            ((17.25, 23.5), (817.25, 523.5)),
        ] {
            let mut sink = Collect::default();
            let stats = walk(
                Bounds {
                    origin: point(px(origin.0), px(origin.1)),
                    size: size(px(VW), px(VH)),
                },
                &scene,
                camera,
                StageBlend::settled(stage),
                opts,
                &mut sink,
            );
            assert_eq!(stats.drawn_blocks, 0);
            assert_eq!(stats.drawn_cells, 0);
            assert_eq!(stats.curves, 0);
            assert_eq!(stats.edges, 1);
            assert_eq!(
                stats,
                crate::lod::cull(&scene, &camera, StageBlend::settled(stage), VW, VH, opts,)
            );
            assert!(sink.fg.is_empty());
            assert_eq!(sink.edges, vec![[expected, expected, expected]]);
        }
    }
}

fn overview_fixture(sparse: bool, indexed: bool) -> (SceneSnapshot, usize) {
    let mut scene = small_workload(State::OK, sparse);
    let block = if sparse {
        let actual = scene.blocks[0].clone();
        scene.blocks[0].rect = Rect::default();
        scene.blocks[0].inner = Rect::default();
        scene.blocks[0].children = 0..0;
        scene.blocks[0].sats = 0..0;
        let empty = scene.blocks[0].clone();
        scene.blocks.push(empty);
        scene.blocks.push(actual);
        scene.region_blocks = vec![2];
        Arc::make_mut(&mut scene.ids).blocks = ["", "", "workload"]
            .into_iter()
            .map(Arc::<str>::from)
            .collect();
        2
    } else {
        0
    };
    for index in 1..129 {
        let mut region = scene.regions[0].clone();
        region.rect = Rect::new(index as f32 * 2000.0, 0.0, 400.0, 300.0);
        region.label = format!("distant-{index}").into();
        region.children = 0..0;
        region.ext.workload_label = NsExt::workload_label(0);
        scene.regions.push(region);
        Arc::make_mut(&mut scene.ids)
            .regions
            .push(format!("namespace-{index}").into());
    }
    scene.bounds = Rect::new(0.0, 0.0, 256400.0, 300.0);
    scene.rebuild_spatial_index();
    if !indexed {
        scene.spatial_index = Default::default();
    }
    (scene, block)
}

#[test]
fn namespace_overview_has_literal_rows_and_one_shared_indexed_locator() {
    use crate::overlay::{OverlayFrame, OverlayKind, OverlayMark, StampCache};
    let pol = policy(Knobs::default());
    let camera = Camera {
        cx: 200.0,
        cy: 150.0,
        zoom: 2.0,
    };
    for sparse in [false, true] {
        for indexed in [false, true] {
            let (scene, block) = overview_fixture(sparse, indexed);
            assert_eq!(
                scene.region_index_is_selective(&camera.visible_world(VW, VH)),
                indexed
            );
            let region_path = crate::PickPath {
                region: 0,
                block: None,
                cell: None,
                sat: None,
            };
            let child_paths = [
                crate::PickPath {
                    block: Some(block as u32),
                    ..region_path
                },
                crate::PickPath {
                    block: Some(block as u32),
                    cell: Some(if sparse { 2 } else { 0 }),
                    ..region_path
                },
                crate::PickPath {
                    block: Some(block as u32),
                    sat: Some(0),
                    ..region_path
                },
            ];
            for stage in [0, 1] {
                let blend = StageBlend::settled(stage);
                for origin in [(0.0, 0.0), (47.25, 19.5)] {
                    let bounds = Bounds {
                        origin: point(px(origin.0), px(origin.1)),
                        size: size(px(VW), px(VH)),
                    };
                    let opts = FrameOpts {
                        policy: &pol,
                        theme: &k10s_theme::K10S_DARK.map,
                        type_: MapType::default(),
                        edges_on: true,
                        skip_blocks: false,
                        hex: false,
                    };
                    let mut painted = Collect::default();
                    let counts = walk(bounds, &scene, camera, blend, opts, &mut painted);
                    assert_eq!(
                        counts,
                        crate::lod::cull(&scene, &camera, blend, VW, VH, opts)
                    );
                    assert_eq!((counts.quads, counts.labels, counts.icons), (2, 2, 1));
                    assert_eq!(
                        (counts.drawn_blocks, counts.drawn_cells, counts.drawn_sats),
                        (0, 0, 0)
                    );
                    assert!(painted.fg.is_empty());
                    assert_eq!(painted.labels[0].text, "payments-production-eu-west");
                    assert_eq!(painted.labels[1].text, "1 workload");
                    let expected_body = Bounds {
                        origin: point(px(origin.0 + 400.0), px(origin.1 + 200.0)),
                        size: size(px(800.0), px(600.0)),
                    };
                    assert_eq!(painted.bg[1].bounds, expected_body);
                    assert!(painted.labels.iter().all(|job| job.whole_line
                        && job.text_start == 0
                        && job.prepared.is_none()));
                    // Default type is 14 px with a 1.35 line height and a 4 px row gap.
                    assert!((painted.labels[0].y - origin.1 - 479.1).abs() < 0.001);
                    assert!((painted.labels[1].y - origin.1 - 502.0).abs() < 0.001);
                    for job in &painted.labels {
                        let row = Bounds {
                            origin: point(px(job.x), px(job.y)),
                            size: size(px(job.width), px(job.height)),
                        };
                        assert!(row.is_contained_within(&expected_body));
                    }
                    let outline = crate::primitive::mark_primitive_typed(
                        &scene,
                        region_path,
                        camera,
                        &pol,
                        stage,
                        (VW, VH),
                        origin,
                        MapType::default(),
                    )
                    .unwrap();
                    assert_eq!(outline.bounds, expected_body);
                    assert_eq!(outline.corners, painted.bg[1].corner_radii);
                    for path in child_paths {
                        assert!(
                            crate::primitive::mark_primitive_typed(
                                &scene,
                                path,
                                camera,
                                &pol,
                                stage,
                                (VW, VH),
                                origin,
                                MapType::default(),
                            )
                            .is_none()
                        );
                    }
                    for world_point in [(55.0, 68.0), (55.0, 81.0), (219.0, 99.0)] {
                        let (x, y) = camera.w2s(world_point.0, world_point.1, VW, VH);
                        assert_eq!(
                            crate::pick::pick(&scene, &camera, &pol, blend, VW, VH, x, y),
                            Some(region_path)
                        );
                    }
                    let overlay = OverlayFrame {
                        kind: Some(OverlayKind::Policy),
                        marks: ["namespace", "workload", "pod-current", "satellite"]
                            .into_iter()
                            .map(|uid| OverlayMark {
                                uid: uid.into(),
                                tint: Some(Severity::Warn),
                                sparkline: None,
                                label: None,
                            })
                            .collect(),
                    };
                    let stamps = overlay.visible_stamps_typed(
                        &mut StampCache::default(),
                        &scene,
                        camera,
                        &pol,
                        blend,
                        VW,
                        VH,
                        MapType::default(),
                    );
                    assert_eq!(stamps.len(), 1);
                    assert_eq!(stamps[0].screen, Rect::new(400.0, 200.0, 800.0, 600.0));
                }
            }
        }
    }
}

#[test]
fn namespace_handoff_keeps_counts_and_children_on_the_displayed_side() {
    use crate::overlay::{OverlayFrame, OverlayKind, OverlayMark, StampCache};
    let pol = policy(Knobs::default());
    let camera = Camera {
        cx: 55.0,
        cy: 68.0,
        zoom: 2.4,
    };
    let (scene, block) = overview_fixture(true, true);
    let path = crate::PickPath {
        region: 0,
        block: Some(block as u32),
        cell: None,
        sat: None,
    };
    for (from, to) in [(1, 2), (2, 1)] {
        for t in [0.0, 0.49, 0.5, 0.51, 1.0] {
            let blend = StageBlend { from, to, t };
            let shown = if t < 0.5 { from } else { to };
            let eased = t * t * (3.0 - 2.0 * t);
            let expected_detail_alpha = if to == 2 { eased } else { 1.0 - eased };
            let expected_overview_alpha = 1.0 - expected_detail_alpha;
            let opts = FrameOpts {
                policy: &pol,
                theme: &k10s_theme::K10S_DARK.map,
                type_: MapType::default(),
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
            assert_eq!(
                sink.labels.iter().any(|job| job.text == "1 workload"),
                shown == 1
            );
            assert_eq!(
                sink.icons.iter().any(|job| matches!(job, IconJob::Wl(..))),
                shown == 2
            );
            if shown == 1 {
                assert!(expected_overview_alpha > 0.0);
                assert!(
                    sink.labels
                        .iter()
                        .all(|job| (job.color.a - expected_overview_alpha).abs() < 0.001)
                );
            } else {
                assert!(expected_detail_alpha > 0.0);
            }
            assert!((workload_alpha(blend, &pol) - expected_detail_alpha).abs() < 0.001);
            assert!((overview_alpha(blend) - expected_overview_alpha).abs() < 0.001);
            assert_eq!(
                crate::primitive::mark_primitive_typed(
                    &scene,
                    path,
                    camera,
                    &pol,
                    shown,
                    (VW, VH),
                    (0.0, 0.0),
                    MapType::default()
                )
                .is_some(),
                shown == 2
            );
            let (x, y) = camera.w2s(55.0, 68.0, VW, VH);
            assert_eq!(
                crate::pick::pick(&scene, &camera, &pol, blend, VW, VH, x, y)
                    .unwrap()
                    .block,
                (shown == 2).then_some(block as u32)
            );
            let overlay = OverlayFrame {
                kind: Some(OverlayKind::Policy),
                marks: vec![OverlayMark {
                    uid: "workload".into(),
                    tint: Some(Severity::Warn),
                    sparkline: None,
                    label: None,
                }],
            };
            assert_eq!(
                overlay
                    .visible_stamps_typed(
                        &mut StampCache::default(),
                        &scene,
                        camera,
                        &pol,
                        blend,
                        VW,
                        VH,
                        MapType::default()
                    )
                    .len(),
                usize::from(shown == 2)
            );
        }
    }
    let mut machine = k10s_atlas::StageMachine::new(crate::lod::STAGE_FADE_SECS);
    assert_eq!(machine.settle(&pol, 1.0).walk_stage(), 1);
    machine.update(&pol, camera.zoom, 0.0);
    assert!(machine.animating());
    let settled = machine.settle(&pol, camera.zoom);
    assert!(settled.is_settled() && !machine.animating());
    assert_eq!(settled.walk_stage(), 2);
    let sink = walk_at_stage(&scene, camera, settled.walk_stage());
    assert!(sink.icons.iter().any(|job| matches!(job, IconJob::Wl(..))));
    assert!(!sink.labels.iter().any(|job| job.text == "1 workload"));
    let exit_zoom = pol.stage_cell * pol.stage_exit - 0.001;
    let exit = machine.settle(&pol, exit_zoom);
    assert!(exit.is_settled() && !machine.animating());
    assert_eq!(exit.walk_stage(), 1);
    assert!(
        walk_at_stage(
            &scene,
            Camera {
                zoom: exit_zoom,
                ..camera
            },
            exit.walk_stage()
        )
        .labels
        .iter()
        .any(|job| job.text == "1 workload")
    );
}

#[test]
fn overview_budgets_drop_whole_jobs_and_tiny_namespaces_remain_locators() {
    let (mut scene, _) = overview_fixture(false, true);
    let camera = Camera {
        cx: 200.0,
        cy: 150.0,
        zoom: 2.0,
    };
    for max_labels in [0, 1, 2] {
        let pol = LodPolicy {
            max_labels,
            max_icons: 0,
            ..policy(Knobs::default())
        };
        let opts = FrameOpts {
            policy: &pol,
            theme: &k10s_theme::K10S_DARK.map,
            type_: MapType::default(),
            edges_on: false,
            skip_blocks: false,
            hex: false,
        };
        let mut sink = Collect::default();
        let counts = walk(
            viewport(),
            &scene,
            camera,
            StageBlend::settled(1),
            opts,
            &mut sink,
        );
        assert_eq!(
            (counts.labels, counts.labels_dropped, counts.icons_dropped),
            (max_labels, 2 - max_labels, 1)
        );
        if max_labels == 1 {
            assert_eq!(sink.labels[0].text, "payments-production-eu-west");
        }
    }
    scene.regions[0].rect = Rect::new(0.0, 140.0, 400.0, 20.0);
    scene.rebuild_spatial_index();
    let single = walk_at_stage(&scene, camera, 1);
    assert_eq!(single.labels.len(), 1);
    assert_eq!(single.labels[0].text, "payments-production-eu-west");
    assert!(single.icons.is_empty());
    scene.regions[0].rect = Rect::new(192.0, 146.0, 16.0, 8.0);
    scene.rebuild_spatial_index();
    let sink = walk_at_stage(&scene, camera, 1);
    assert_eq!(sink.bg.len(), 2);
    assert!(sink.labels.is_empty() && sink.icons.is_empty() && sink.fg.is_empty());
    assert_eq!(sink.bg[1].bounds.size, size(px(32.0), px(16.0)));
    let pol = policy(Knobs::default());
    assert_eq!(
        crate::pick::pick(
            &scene,
            &camera,
            &pol,
            StageBlend::settled(1),
            VW,
            VH,
            VW * 0.5,
            VH * 0.5
        )
        .unwrap()
        .block,
        None
    );
}

#[test]
fn overview_suppression_preserves_explicit_stress_controls() {
    let (scene, block) = overview_fixture(true, true);
    let camera = Camera {
        cx: 55.0,
        cy: 68.0,
        zoom: 2.4,
    };
    let path = crate::PickPath {
        region: 0,
        block: Some(block as u32),
        cell: None,
        sat: None,
    };
    for knobs in [
        Knobs {
            stress_quads: true,
            ..Default::default()
        },
        Knobs {
            stress_curves: true,
            ..Default::default()
        },
    ] {
        let pol = policy(knobs);
        let opts = FrameOpts {
            policy: &pol,
            theme: &k10s_theme::K10S_DARK.map,
            type_: MapType::default(),
            edges_on: false,
            skip_blocks: false,
            hex: false,
        };
        let blend = StageBlend::settled(1);
        let mut sink = Collect::default();
        let counts = walk(viewport(), &scene, camera, blend, opts, &mut sink);
        assert_eq!(
            counts,
            crate::lod::cull(&scene, &camera, blend, VW, VH, opts)
        );
        assert!(counts.drawn_blocks > 0);
        assert!(!sink.labels.iter().any(|job| job.text == "1 workload"));
        assert!(
            crate::primitive::mark_primitive_typed(
                &scene,
                path,
                camera,
                &pol,
                1,
                (VW, VH),
                (0.0, 0.0),
                MapType::default()
            )
            .is_some()
        );
        assert_eq!(
            workload_alpha(
                StageBlend {
                    from: 1,
                    to: 2,
                    t: 0.0
                },
                &pol
            ),
            1.0
        );
    }
}

#[test]
fn published_namespace_totals_stay_exact_and_zero_observed_pods_never_show_healthy() {
    use k10s_core::{Op, replay};
    use k10s_world::{LayoutMode, PublishBench};

    for mode in [LayoutMode::Spread, LayoutMode::Dense] {
        let initial = [replay::scope("ns-quiet", "quiet", Op::Added)];
        let mut bench = PublishBench::new(&initial, mode);
        let cases = [
            (vec![], 0, 0, "0 workloads", Severity::Unknown),
            (
                vec![replay::owner(
                    "wl-a",
                    "quiet",
                    "a",
                    KindId::DEPLOYMENT,
                    Op::Added,
                )],
                1,
                0,
                "1 workload",
                Severity::Unknown,
            ),
            (
                vec![replay::owner("wl-b", "quiet", "b", KindId::JOB, Op::Added)],
                2,
                0,
                "2 workloads",
                Severity::Unknown,
            ),
            (
                vec![replay::instance(
                    "pod-a",
                    "quiet",
                    "wl-a",
                    State::OK,
                    Op::Added,
                )],
                2,
                1,
                "2 workloads",
                Severity::Ok,
            ),
            (
                vec![replay::instance(
                    "pod-a",
                    "quiet",
                    "wl-a",
                    State::of(ReasonId::CRASH_LOOP_BACK_OFF),
                    Op::Modified,
                )],
                2,
                1,
                "2 workloads",
                Severity::Err,
            ),
            (
                vec![replay::instance(
                    "pod-a",
                    "quiet",
                    "wl-a",
                    State::OK,
                    Op::Deleted,
                )],
                2,
                0,
                "2 workloads",
                Severity::Unknown,
            ),
            (
                vec![replay::owner(
                    "wl-a",
                    "quiet",
                    "a",
                    KindId::DEPLOYMENT,
                    Op::Deleted,
                )],
                1,
                0,
                "1 workload",
                Severity::Unknown,
            ),
            (
                vec![replay::owner(
                    "wl-b",
                    "quiet",
                    "b",
                    KindId::JOB,
                    Op::Deleted,
                )],
                0,
                0,
                "0 workloads",
                Severity::Unknown,
            ),
        ];
        for (events, workloads, observed_pods, expected_count, expected_status) in cases {
            if !events.is_empty() {
                bench.apply_events(&events);
                bench.run_publish();
            }
            let snapshot = bench.snapshot();
            let located = snapshot.locate("ns-quiet").unwrap();
            let region = &snapshot.regions[located.slot];
            assert_eq!(region.weight, observed_pods);
            assert_eq!(snapshot.region_block_indices(located.slot).len(), workloads);
            assert_eq!(region.ext.workload_label.as_ref(), expected_count);
            let (cx, cy) = region.rect.center();
            let camera = Camera { cx, cy, zoom: 2.0 };
            for stage in [0, 1] {
                let sink = walk_at_stage(&snapshot, camera, stage);
                assert_eq!(sink.labels.len(), 2);
                assert_eq!(sink.labels[0].text, "quiet");
                assert_eq!(sink.labels[1].text, expected_count);
                assert_eq!(sink.icons.len(), 1);
                assert!(
                    matches!(&sink.icons[0], IconJob::NamespaceStatus(severity, _) if *severity == expected_status)
                );
                assert!(sink.fg.is_empty());
            }
        }
    }
}
