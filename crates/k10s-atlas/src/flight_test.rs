//! What a scripted flight counts and when it gives up: an idle segment counts
//! only window paints, a mid-flight resize is stamped rather than restarting,
//! a scene with no blocks aborts instead of panicking, and the restart budget
//! is a bound the flight honours.

use super::*;
use crate::scene::{BlockNode, CellNode, Rect, RegionNode};
use std::cell::Cell;
use std::rc::Rc;

#[cfg(target_os = "linux")]
#[test]
fn proc_stat_uses_the_reported_clock_rate_and_the_last_name_delimiter() {
    let stat = "42 (worker) with ) delimiters) R 1 2 3 4 5 6 7 8 9 10 250 125";
    assert_eq!(proc_stat_cpu_ms(stat, 250), Some(1_500.0));
    assert_eq!(proc_stat_cpu_ms(stat, 0), None);
    assert_eq!(proc_stat_cpu_ms("malformed", 250), None);
}

fn tiny_scene() -> Scene {
    let block_rect = Rect::new(10.0, 10.0, 50.0, 30.0);
    Scene {
        card_header: 26.0,
        rev: 1,
        bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
        regions: vec![RegionNode {
            rect: Rect::new(0.0, 0.0, 200.0, 100.0),
            label: "region".into(),
            weight: 1,
            children: 0..1,
            ext: (),
        }],
        blocks: vec![BlockNode {
            rect: block_rect,
            inner: block_rect,
            label: "block".into(),
            children: 0..1,
            sats: 0..0,
            ext: (),
        }],
        cells: vec![CellNode {
            rect: Rect::new(12.0, 12.0, 8.0, 8.0),
            label: "cell".into(),
            ext: (),
        }],
        sats: vec![],
        region_blocks: vec![],
        block_cells: vec![],
        block_sats: vec![],
        spatial_index: Default::default(),
        edges: vec![],
        edge_segments: vec![],
        region_edges: vec![],
        region_edge_indexes: vec![],
        cross_edges: 0..0,
        cross_edge_index: Default::default(),
        totals: Totals {
            regions: 1,
            blocks: 1,
            cells: 1,
            sats: 0,
            edges: 0,
        },
    }
}

fn painted_spans() -> FrameSpans {
    FrameSpans {
        walk_us: 820.0,
        quads_us: 140.0,
        paths_us: 310.0,
        icons_us: 260.0,
        text_us: 1180.0,
        hud_us: 190.0,
    }
}

fn test_plan(a: &FlightAnchors, _vw: f32, _vh: f32) -> Vec<Segment> {
    let seg = |name, measure, idle, dur| Segment {
        name,
        from: a.fit,
        to: a.fit,
        dur,
        measure,
        idle,
    };
    vec![
        seg("warmup", false, false, 2.0),
        seg("static", true, false, 3.0),
        seg("idle", true, true, 5.0),
    ]
}

#[test]
fn idle_segment_counts_only_window_paints() {
    let mut flight = Flight::new(test_plan);
    let cpu = Rc::new(Cell::new(Some(1000.0f64)));
    let cpu_reader = cpu.clone();
    flight.cpu_clock = Box::new(move || cpu_reader.get());

    let scene = tiny_scene();
    let mut stats = FrameStats::default();
    let (vw, vh) = (1600.0, 1000.0);
    let t0 = Instant::now();
    let at = |s: f32| t0 + Duration::from_secs_f32(s);

    assert!(matches!(
        flight.frame(at(0.0), vw, vh, true, &scene, &mut stats),
        FlightFrame::Camera(_)
    ));
    assert!(matches!(
        flight.frame(at(1.0), vw, vh, true, &scene, &mut stats),
        FlightFrame::Camera(_)
    ));
    let n_segs = flight.segments.len();
    assert!(
        flight.segments[n_segs - 1].idle,
        "flight must end with the idle segment"
    );

    let mut now_s = 1.0f32;
    for _ in 0..n_segs - 1 {
        stats.push_spans(painted_spans());
        now_s += flight.segments[flight.current].dur + 0.01;
        assert!(matches!(
            flight.frame(at(now_s), vw, vh, true, &scene, &mut stats),
            FlightFrame::Camera(_)
        ));
    }

    let entry_s = now_s + 0.05;
    let FlightFrame::Idle {
        arm_timer: Some(arm),
        ..
    } = flight.frame(at(entry_s), vw, vh, true, &scene, &mut stats)
    else {
        panic!("idle entry must arm the wake timer");
    };
    assert!(
        (arm.as_secs_f32() - 5.0).abs() < 0.2,
        "wake ~= dur: {arm:?}"
    );
    stats.begin_frame(at(entry_s + 0.001), false);

    for dt in [1.0, 2.0] {
        let f = flight.frame(at(entry_s + dt), vw, vh, true, &scene, &mut stats);
        assert!(matches!(
            f,
            FlightFrame::Idle {
                arm_timer: None,
                ..
            }
        ));
        assert!(!f.needs_frame(), "idle must not request animation frames");
        stats.begin_frame(at(entry_s + dt + 0.001), false);
    }
    cpu.set(Some(1000.0 + 12.5));

    let FlightFrame::Done(result) =
        flight.frame(at(entry_s + 5.06), vw, vh, true, &scene, &mut stats)
    else {
        panic!("flight must complete");
    };

    assert_eq!(
        result.segments[0].spans,
        painted_spans(),
        "attributed spans must reach the segment report"
    );

    let last = result.segments.last().expect("segments recorded");
    assert_eq!(
        last.spans.paint_total_us(),
        0.0,
        "an idle segment paints nothing, so it attributes nothing"
    );
    let idle = last
        .idle
        .as_ref()
        .expect("idle segment must carry an idle result");
    assert_eq!(
        idle.paints, 2,
        "entry + closing frames are not window paints"
    );
    assert_eq!(idle.proc_cpu_ms, Some(12.5));
    assert_eq!(idle.dur_s, 5.0);
    assert_eq!(
        result.segments.iter().filter(|s| s.idle.is_some()).count(),
        1,
        "exactly one idle segment"
    );
    assert_eq!(result.totals.cells, 1);
    assert_eq!(result.viewport, FLIGHT_VIEWPORT);
    assert_eq!(result.window, [vw, vh]);
    assert_eq!(result.resizes, 0);
    assert_eq!(result.restarts, 0);
}

#[test]
fn a_mid_flight_resize_does_not_restart_and_is_stamped() {
    let mut flight = Flight::new(test_plan);
    let scene = tiny_scene();
    let mut stats = FrameStats::default();
    let t0 = Instant::now();
    let at = |s: f32| t0 + Duration::from_secs_f32(s);

    assert!(matches!(
        flight.frame(at(0.0), 1600.0, 1000.0, true, &scene, &mut stats),
        FlightFrame::Camera(_)
    ));
    assert!(matches!(
        flight.frame(at(1.0), 1600.0, 1000.0, true, &scene, &mut stats),
        FlightFrame::Camera(_)
    ));
    let n_segs = flight.segments.len();
    assert!(n_segs > 0, "flight must have built");

    let mut now_s = 1.0f32;
    let (mut vw, mut vh) = (1600.0, 1000.0);
    for seg in 0..n_segs - 1 {
        if seg == 1 {
            (vw, vh) = (1920.0, 1080.0);
        }
        stats.push_spans(painted_spans());
        now_s += flight.segments[flight.current].dur + 0.01;
        match flight.frame(at(now_s), vw, vh, true, &scene, &mut stats) {
            FlightFrame::Camera(_) => {}
            FlightFrame::Idle { .. } => {}
            other => panic!(
                "a resize must not restart the letterboxed flight; got needs_frame={}",
                other.needs_frame()
            ),
        }
    }

    let entry_s = now_s + 0.05;
    assert!(matches!(
        flight.frame(at(entry_s), vw, vh, true, &scene, &mut stats),
        FlightFrame::Idle { .. }
    ));
    stats.begin_frame(at(entry_s + 0.001), false);

    let FlightFrame::Done(result) =
        flight.frame(at(entry_s + 5.06), vw, vh, true, &scene, &mut stats)
    else {
        panic!("flight must complete despite the resize");
    };
    assert_eq!(result.viewport, FLIGHT_VIEWPORT);
    assert_eq!(result.window, [1920.0, 1080.0]);
    assert!(result.resizes > 0, "the resize must be stamped as taint");
    assert_eq!(result.restarts, 0, "a resize is not a restart");
}

#[test]
fn idle_proc_cpu_is_none_when_clock_is_unmeasurable() {
    let mut flight = Flight::new(test_plan);
    flight.cpu_clock = Box::new(|| None);

    let scene = tiny_scene();
    let mut stats = FrameStats::default();
    let (vw, vh) = (1600.0, 1000.0);
    let t0 = Instant::now();
    let at = |s: f32| t0 + Duration::from_secs_f32(s);

    assert!(matches!(
        flight.frame(at(0.0), vw, vh, true, &scene, &mut stats),
        FlightFrame::Camera(_)
    ));
    assert!(matches!(
        flight.frame(at(1.0), vw, vh, true, &scene, &mut stats),
        FlightFrame::Camera(_)
    ));
    let n_segs = flight.segments.len();

    let mut now_s = 1.0f32;
    for _ in 0..n_segs - 1 {
        stats.push_spans(painted_spans());
        now_s += flight.segments[flight.current].dur + 0.01;
        assert!(matches!(
            flight.frame(at(now_s), vw, vh, true, &scene, &mut stats),
            FlightFrame::Camera(_)
        ));
    }

    let entry_s = now_s + 0.05;
    assert!(matches!(
        flight.frame(at(entry_s), vw, vh, true, &scene, &mut stats),
        FlightFrame::Idle {
            arm_timer: Some(_),
            ..
        }
    ));
    stats.begin_frame(at(entry_s + 0.001), false);

    let FlightFrame::Done(result) =
        flight.frame(at(entry_s + 5.06), vw, vh, true, &scene, &mut stats)
    else {
        panic!("flight must complete");
    };
    let idle = result
        .segments
        .last()
        .and_then(|s| s.idle.as_ref())
        .expect("idle segment must carry an idle result");
    assert_eq!(idle.proc_cpu_ms, None);
}

#[test]
fn a_frame_after_done_neither_reports_nor_restarts() {
    let rest = Camera {
        cx: 7.0,
        cy: 9.0,
        zoom: 3.0,
    };
    let mut flight = Flight::new(move |a: &FlightAnchors, _vw: f32, _vh: f32| {
        vec![Segment {
            name: "static",
            from: a.fit,
            to: rest,
            dur: 1.0,
            measure: true,
            idle: false,
        }]
    });
    let scene = tiny_scene();
    let mut stats = FrameStats::default();
    let (vw, vh) = (1600.0, 1000.0);
    let t0 = Instant::now();
    let at = |s: f32| t0 + Duration::from_secs_f32(s);

    assert!(matches!(
        flight.frame(at(0.0), vw, vh, true, &scene, &mut stats),
        FlightFrame::Camera(_)
    ));
    assert!(matches!(
        flight.frame(at(1.0), vw, vh, true, &scene, &mut stats),
        FlightFrame::Camera(_)
    ));
    let FlightFrame::Done(result) = flight.frame(at(2.5), vw, vh, true, &scene, &mut stats) else {
        panic!("flight must complete");
    };
    assert_eq!(result.segments.len(), 1);

    let mut now_s = 2.6;
    for (what, vw, vh, active) in [
        ("a queued damage notify", vw, vh, true),
        ("the window going inactive", vw, vh, false),
        ("a resize as the window closes", vw * 0.5, vh * 0.5, true),
    ] {
        let frame = flight.frame(at(now_s), vw, vh, active, &scene, &mut stats);
        assert!(!frame.needs_frame(), "{what} must not ask for a repaint");
        let FlightFrame::Idle { camera, arm_timer } = frame else {
            panic!("{what} must not re-enter the flight");
        };
        assert!(arm_timer.is_none(), "{what} must not arm a wake timer");
        assert_eq!(
            (camera.cx, camera.cy, camera.zoom),
            (rest.cx, rest.cy, rest.zoom),
            "{what} must hold the camera the flight ended on"
        );
        now_s += 0.1;
    }
    assert_eq!(
        flight.restarts, 0,
        "a finished flight has nothing left to restart"
    );
}

#[test]
fn a_zero_weight_scene_anchors_on_a_region_that_has_blocks() {
    fn anchor_plan(a: &FlightAnchors, _vw: f32, _vh: f32) -> Vec<Segment> {
        let (bx, by) = a.block_center;
        vec![Segment {
            name: "anchor",
            from: a.fit,
            to: Camera {
                cx: bx,
                cy: by,
                zoom: 1.0,
            },
            dur: 1.0,
            measure: false,
            idle: false,
        }]
    }

    let mut scene = tiny_scene();
    scene.regions[0].weight = 0;
    scene.regions.push(RegionNode {
        rect: Rect::new(200.0, 0.0, 200.0, 100.0),
        label: "empty".into(),
        weight: 0,
        children: 1..1,
        ext: (),
    });

    let mut flight = Flight::new(anchor_plan);
    assert!(flight.build(
        &scene,
        FLIGHT_VIEWPORT[0],
        FLIGHT_VIEWPORT[1],
        Instant::now()
    ));
    let to = flight.segments[0].to;
    assert_eq!(
        (to.cx, to.cy),
        scene.blocks[0].inner.center(),
        "the anchor must be a block of the region that owns it"
    );
}

#[test]
fn a_scene_with_no_blocks_aborts_instead_of_panicking() {
    let mut no_regions = tiny_scene();
    no_regions.regions.clear();
    no_regions.blocks.clear();
    no_regions.cells.clear();

    let mut no_blocks = tiny_scene();
    no_blocks.regions[0].weight = 0;
    no_blocks.regions[0].children = 0..0;
    no_blocks.blocks.clear();
    no_blocks.cells.clear();

    for (what, scene) in [("no regions", no_regions), ("no blocks", no_blocks)] {
        let mut flight = Flight::new(test_plan);
        let mut stats = FrameStats::default();
        let (vw, vh) = (1600.0, 1000.0);
        let t0 = Instant::now();
        let at = |s: f32| t0 + Duration::from_secs_f32(s);

        assert!(matches!(
            flight.frame(at(0.0), vw, vh, true, &scene, &mut stats),
            FlightFrame::Camera(_)
        ));
        assert!(
            matches!(
                flight.frame(at(1.0), vw, vh, true, &scene, &mut stats),
                FlightFrame::Aborted
            ),
            "{what} must abort with a reason, not panic in a paint pass"
        );
    }
}

#[test]
fn restart_budget_aborts() {
    let mut flight = Flight::new(test_plan);
    let scene = tiny_scene();
    let mut stats = FrameStats::default();
    let t0 = Instant::now();
    let at = |s: f32| t0 + Duration::from_secs_f32(s);
    let (vw, vh) = (800.0, 600.0);

    let mut now_s = 0.0f32;
    let mut aborted = false;
    for _ in 0..MAX_RESTARTS + 1 {
        assert!(matches!(
            flight.frame(at(now_s), vw, vh, true, &scene, &mut stats),
            FlightFrame::Camera(_)
        ));
        now_s += 1.0;
        match flight.frame(at(now_s), vw, vh, true, &scene, &mut stats) {
            FlightFrame::Camera(_) => {}
            other => panic!(
                "expected planned flight, got needs_frame={}",
                other.needs_frame()
            ),
        }
        now_s += 0.5;
        match flight.frame(at(now_s), vw, vh, false, &scene, &mut stats) {
            FlightFrame::Waiting => {}
            FlightFrame::Aborted => {
                aborted = true;
                break;
            }
            _ => panic!("focus loss must restart or abort"),
        }
        now_s += 0.5;
    }
    assert!(aborted, "flight must abort after {MAX_RESTARTS} restarts");
}
