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
                    let geometry = projection.workload(&scene, 0, 3, type_);
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
fn normal_map_rows_keep_their_existing_pitch_and_heading_height() {
    let type_ = MapType::default();
    assert_eq!(
        row_pitch(type_.workload, type_.line_height),
        type_.workload * type_.line_height
    );
    assert_eq!(
        row_pitch(type_.pod, type_.line_height),
        type_.pod * type_.line_height
    );
    assert_eq!(crate::primitive::name_band_height(type_), 54.0);
}

#[test]
fn removing_the_only_pod_keeps_observed_truth_and_a_bounded_workload_target() {
    let mut scene = small_workload(State::OK, true);
    scene.blocks[0].children = 0..0;
    let ids = Arc::make_mut(&mut scene.ids);
    ids.cells[2] = Arc::from("");
    scene.identity_rev += 1;
    scene.rebuild_spatial_index();
    let camera = Camera {
        cx: 55.0,
        cy: 68.0,
        zoom: crate::lod::STAGE_POD_LABEL,
    };
    let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
    let geometry = projection.workload(&scene, 0, 3, MapType::default());
    let sink = walk_at(&scene, camera);
    assert_eq!(geometry.body.bounds.size, size(px(168.0), px(74.0)));
    assert!(
        sink.fg
            .iter()
            .any(|quad| quad.bounds == geometry.body.bounds)
    );
    assert!(sink.labels.iter().any(|job| job.text == "0 observed pods"));
    assert!(
        !sink
            .labels
            .iter()
            .any(|job| job.text == "Healthy" || job.text == scene.cells[2].label.as_ref())
    );
    let point = geometry.heading.unwrap().bounds.center();
    let pol = policy(Knobs::default());
    let picked = crate::pick::pick(
        &scene,
        &camera,
        &pol,
        StageBlend::settled(3),
        VW,
        VH,
        f32::from(point.x),
        f32::from(point.y),
    )
    .unwrap();
    assert_eq!((picked.block, picked.cell), (Some(0), None));
    let overlay = crate::OverlayFrame {
        kind: Some(crate::OverlayKind::Policy),
        marks: vec![crate::OverlayMark {
            uid: "pod-current".into(),
            tint: Some(Severity::Err),
            sparkline: None,
            label: None,
        }],
    };
    assert!(
        overlay
            .visible_stamps(&scene, camera, &pol, StageBlend::settled(3), VW, VH)
            .is_empty()
    );
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

// The user-visible half of the same change, and the bound that replaces
// "it fits in the card".
//
// A glyph is deliberately allowed to be larger than its card: a workload
// with two replicas has a card a few pixels across, and a card is not what
// a person recognises it by. What it may never exceed is the HALO -- the
// space that workload owns, which no other workload's halo overlaps --
// because that is what keeps one namespace's glyphs off each other.
#[test]
fn a_workload_glyph_grows_to_fill_the_space_the_workload_owns() {
    const WAS_FIXED_AT: f32 = 12.0;
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
        biggest >= WAS_FIXED_AT * 3.0,
        "the glyph never got past {biggest} px, which is what the constant it \
             replaced already managed"
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
    let sink = walk_at(&scene, camera);
    let bounds = sink
        .icons
        .iter()
        .find_map(|job| match job {
            IconJob::Wl(_, primitive) => Some(primitive.bounds),
            _ => None,
        })
        .expect("the workload glyph was drawn without chrome");
    let halo_short = halo.w.min(halo.h) * zoom;
    assert!(
        f32::from(bounds.size.width) >= halo_short * 0.4,
        "a {:?} glyph in a {halo_short} px halo is still a decoration",
        bounds.size.width
    );
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
    let scene = one_of_each(State::OK);
    let block = &scene.blocks[0];
    let center = block.inner.center();
    let mid = Camera {
        cx: center.0,
        cy: center.1,
        zoom: 0.20,
    };
    let mid_frame = walk_at(&scene, mid);
    assert_eq!(mid_frame.fg.len(), 0, "the medallion retained a card");
    assert_eq!(
        mid_frame
            .icons
            .iter()
            .filter(|job| matches!(job, IconJob::Wl(..) | IconJob::ToolId(..)))
            .count(),
        1
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
        StageBlend::settled(1),
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

// A namespace keeps its silhouette for as long as it exists. The corner
// radii are hashed from the island's ORIGIN precisely so that growing or
// shrinking a namespace -- which both layout engines do by moving its far
// edges only -- cannot make it pop into a different shape.
#[test]
fn an_island_keeps_its_shape_when_its_namespace_resizes() {
    let short = 400.0f32;
    assert!(
        short >= super::ISLAND_DETAIL_MIN_PX,
        "below the threshold every island is cut to one radius and this              proves nothing"
    );
    let small = island_radii(&Rect::new(120.0, 64.0, 400.0, 300.0), short);
    let grown = island_radii(&Rect::new(120.0, 64.0, 900.0, 700.0), short);
    assert_eq!(small, grown, "the same island changed shape as it grew");

    let moved = island_radii(&Rect::new(121.0, 64.0, 400.0, 300.0), short);
    assert_ne!(
        small, moved,
        "two islands a pixel apart are cut to the same die"
    );

    // Under the threshold the caller never asks: a small island is cut to
    // one radius, which is the measured trade -- at that size the difference
    // between corners is a pixel or two, and computing it doubled the cost
    // of the fit camera's walk. The walk is what proves the threshold is
    // wired up, so it is checked through a real frame rather than here.
    let scene = one_of_each(State::OK);
    let far = Camera {
        cx: 150.0,
        cy: 120.0,
        zoom: 0.05,
    };
    let corners = walk_at(&scene, far).bg[1].corner_radii;
    assert_eq!(corners.top_left, corners.top_right);
    assert_eq!(corners.top_left, corners.bottom_left);
    assert_eq!(corners.top_left, corners.bottom_right);
    assert!(f32::from(corners.top_left) > 0.0, "a square island");

    let near = Camera {
        cx: 150.0,
        cy: 120.0,
        zoom: 2.0,
    };
    let corners = walk_at(&scene, near).bg[1].corner_radii;
    assert!(
        corners.top_left != corners.bottom_right || corners.top_right != corners.bottom_left,
        "an island large enough to show a coastline was cut square"
    );

    // Bounded by construction: a corner radius is a fraction of the short
    // side and never more than half of it, or gpui clamps it and the
    // silhouette stops being the one that was chosen.
    for corners in [small, grown, moved] {
        for radius in [
            corners.top_left,
            corners.top_right,
            corners.bottom_right,
            corners.bottom_left,
        ] {
            let radius = f32::from(radius);
            assert!(
                radius > 0.0 && radius <= short * 0.5,
                "{radius} is not a usable corner on a {short} px island"
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
    let card_zoom = (pol.block_chrome_min_px + 1.0) / inner.w;
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
            .workload(&scene, 0, blend.walk_stage(), MapType::default())
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
            let heading = projection.workload(&scene, 0, 3, type_).heading;
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
    let scene = one_of_each(State::OK);
    let camera = Camera {
        cx: 55.0,
        cy: 105.0,
        zoom: 16.0,
    };
    let type_ = k10s_theme::MapType::default();
    let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
    let heading = projection
        .workload(&scene, 0, 3, type_)
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
    let scene = one_of_each(State::OK);
    let type_ = k10s_theme::MapType::default();
    let mut observed = 0;
    for cx in [55.0, 95.0, 130.0] {
        for cy in [81.0, 100.0, 125.0] {
            let camera = Camera { cx, cy, zoom: 16.0 };
            let projection = Projection::new(camera, (VW, VH), (0.0, 0.0));
            let band = projection
                .workload(&scene, 0, 3, type_)
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
