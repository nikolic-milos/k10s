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
            let IconJob::Wl(_, bounds) = job else {
                continue;
            };
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
            IconJob::Wl(_, bounds) => Some(*bounds),
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
