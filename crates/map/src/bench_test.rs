//! The bench harness's own contract: a flight that gave up asks to leave
//! rather than to be painted again, the plan keeps its shape and zoom targets,
//! and the report keeps its schema v5 keys -- which is what the comparator
//! reads.

use super::*;
use k10s_atlas::flight::{CpuPercentiles, FLIGHT_VIEWPORT, IdleResult, Percentiles};
use k10s_atlas::{CounterStats, DrawnCounts, FrameSpans, SegmentCounters, TextCacheCounts};

fn anchors() -> FlightAnchors {
    let mut fit = Camera::default();
    fit.fit(
        k10s_core::Rect::new(0.0, 0.0, 10_000.0, 6_000.0),
        FLIGHT_VIEWPORT[0],
        FLIGHT_VIEWPORT[1],
    );
    FlightAnchors {
        fit,
        region_center: (1000.0, 800.0),
        block_center: (1050.0, 830.0),
        hub_center: (2400.0, 1400.0),
    }
}

// A frame that gave up owes the caller a leave, not a repaint, and it must
// not be mistaken for a finished flight. This used to be
// `std::process::exit(3)` in the middle of the render callback, which took
// the process out before the world thread was joined or the data plane
// retired -- and the order those two happen in is precisely what lets a watch
// parked on a full sink see a disconnect rather than deadlock.
#[test]
fn a_flight_that_gave_up_asks_to_leave_rather_than_to_be_painted_again() {
    assert!(!BenchFrame::Aborted.needs_frame());
    assert!(!BenchFrame::Done.needs_frame());
    assert!(BenchFrame::Waiting.needs_frame());

    // And the two terminal states stay distinguishable, because the exit
    // status a script reads is the only difference between "the recording is
    // in that file" and "there is no recording".
    assert_ne!(
        std::mem::discriminant(&BenchOp::Abort),
        std::mem::discriminant(&BenchOp::Quit)
    );
}

#[test]
fn plan_keeps_its_shape_and_zoom_targets() {
    let a = anchors();
    let segs = plan(&a, FLIGHT_VIEWPORT[0], FLIGHT_VIEWPORT[1]);

    let first_measured = segs
        .iter()
        .position(|s| s.measure)
        .expect("flight must measure something");
    assert!(
        first_measured >= 1,
        "flight must warm up before it measures"
    );
    assert!(
        segs[..first_measured].iter().all(|s| !s.measure && !s.idle),
        "warmup segments must not report"
    );
    assert!(
        segs[first_measured..].iter().all(|s| s.measure),
        "warmup must precede all measurement"
    );

    let idle: Vec<usize> = segs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.idle)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        idle,
        [segs.len() - 1],
        "exactly one idle segment, and it closes the flight"
    );

    let measured: Vec<&str> = segs.iter().filter(|s| s.measure).map(|s| s.name).collect();
    assert!(measured.len() >= 4, "flight is too thin to be useful");
    let mut distinct = measured.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        measured.len(),
        "measured segment names must be distinct: {measured:?}"
    );
    assert!(
        segs.iter().all(|s| s.dur > 0.0),
        "a zero-length segment measures nothing"
    );

    let targets: Vec<f32> = segs.iter().map(|s| s.to.zoom).collect();
    for zoom in [
        a.fit.zoom,
        0.12,
        crate::lod::STAGE_POD,
        crate::lod::MAX_USEFUL_ZOOM,
    ] {
        assert!(targets.contains(&zoom), "zoom target {zoom} went missing");
    }
    assert!(
        segs.iter()
            .any(|s| s.to.zoom == crate::lod::STAGE_POD && (s.to.cx, s.to.cy) == a.hub_center),
        "hub zoom must reach the Pod stage"
    );
    assert!(
        segs.iter()
            .any(|s| s.to.zoom == crate::lod::MAX_USEFUL_ZOOM && (s.to.cx, s.to.cy) == a.hub_center),
        "pod detail must sit on the hub"
    );
    assert!(
        segs.iter()
            .any(|s| s.to.zoom == 0.12 && s.from.cx != s.to.cx),
        "flight must include a Z1 pan"
    );
}

#[test]
fn flight_emits_satellites_and_pod_detail_in_published_layouts() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::frame::tests::{Collect, viewport};
    use crate::frame::{FrameOpts, IconJob, walk};
    use crate::lod::{Knobs, policy};
    use crate::primitive::Projection;
    use k10s_atlas::StageMachine;
    use k10s_core::{
        IngestEvent, KindId, Op, Payload, ReasonId, ResourceEvent, Severity, State, replay,
    };
    use k10s_world::{LayoutMode, PublishBench};

    let policy = policy(Knobs::default());
    for mode in [LayoutMode::Spread, LayoutMode::Dense] {
        for pods in [1, 5, 200] {
            let case = format!("{mode:?}, {pods} Pods");
            let mut events = vec![
                replay::scope("ns-flight", "flight", Op::Added),
                replay::owner(
                    "flight-owner",
                    "flight",
                    "flight-workload",
                    KindId::DEPLOYMENT,
                    Op::Added,
                ),
            ];
            events.extend((0..pods).map(|index| {
                replay::instance(
                    &format!("flight-workload-pod-{index}"),
                    "flight",
                    "flight-owner",
                    State::of(ReasonId::FAILED),
                    Op::Added,
                )
            }));
            if mode.emits_attachments() {
                for index in 0..2 {
                    events.push(IngestEvent::Resource(ResourceEvent {
                        kind: KindId::SERVICE,
                        uid: format!("flight-service-{index}").into(),
                        namespace: "flight".into(),
                        name: format!("flight-service-{index}").into(),
                        resource_version: 0,
                        parent: Some("flight-owner".into()),
                        op: Op::Added,
                        payload: Payload::Attached {
                            kind: KindId::SERVICE,
                            detail: "ClusterIP".into(),
                        },
                    }));
                }
            }
            let scene = PublishBench::new(&events, mode).snapshot();
            let schedule = Rc::new(RefCell::new(Vec::new()));
            let captured = schedule.clone();
            let mut flight = Flight::new(move |anchors, vw, vh| {
                let segments = plan(anchors, vw, vh);
                *captured.borrow_mut() = segments.iter().map(|s| (s.name, s.dur)).collect();
                segments
            });
            let mut stats = FrameStats::default();
            let mut stages = StageMachine::new(crate::lod::STAGE_FADE_SECS);
            let (vw, vh) = (FLIGHT_VIEWPORT[0], FLIGHT_VIEWPORT[1]);
            let mut now = Instant::now();
            assert!(matches!(
                flight.frame(now, vw, vh, true, &scene, &mut stats),
                FlightFrame::Camera(_)
            ));
            now += Duration::from_secs(1);
            let FlightFrame::Camera(camera) = flight.frame(now, vw, vh, true, &scene, &mut stats)
            else {
                panic!("{case}: the stable viewport did not start the flight");
            };
            stages.update(&policy, camera.zoom, 1.0);
            let mut visited = Vec::new();
            for (name, duration) in schedule.take() {
                for half in 0..2 {
                    let dt = duration * 0.5;
                    now += Duration::from_secs_f32(dt);
                    let frame = flight.frame(now, vw, vh, true, &scene, &mut stats);
                    let camera = match frame {
                        FlightFrame::Camera(camera) | FlightFrame::Idle { camera, .. } => camera,
                        FlightFrame::Done(_) => continue,
                        _ => panic!("{case}: flight stopped during {name}"),
                    };
                    let blend = stages.update(&policy, camera.zoom, dt);
                    if half != 0 {
                        continue;
                    }
                    let stage = match name {
                        "Z1 static (workload cards)" => 1,
                        "Z2 static (hub + satellites)" => 2,
                        "Z3 static (pod detail)" => 3,
                        _ => continue,
                    };
                    assert!(blend.is_settled(), "{case}: {name}");
                    assert_eq!(blend.walk_stage(), stage, "{case}: {name}");
                    visited.push(stage);
                    if stage == 1 || (stage == 2 && !mode.emits_attachments()) {
                        continue;
                    }
                    for ui_size in [8.0, 14.0, 32.0] {
                        let type_ = k10s_theme::Typography {
                            ui_size,
                            ..Default::default()
                        }
                        .map();
                        let mut sink = Collect::default();
                        walk(
                            viewport(),
                            &scene,
                            camera,
                            blend,
                            FrameOpts {
                                theme: &k10s_theme::K10S_DARK.map,
                                policy: &policy,
                                type_,
                                edges_on: true,
                                skip_blocks: false,
                                hex: true,
                            },
                            &mut sink,
                        );
                        if stage == 2 {
                            assert!(
                                scene.sats.iter().any(|sat| {
                                    sink.labels.iter().any(|job| job.text == sat.label.as_ref())
                                }),
                                "{case}, {ui_size}px: the hub has no satellite name"
                            );
                            assert!(
                                sink.labels.iter().any(|job| job.text == "ClusterIP"),
                                "{case}, {ui_size}px: the hub has no satellite detail"
                            );
                            continue;
                        }
                        let cell = scene
                            .cells
                            .iter()
                            .find(|cell| {
                                sink.labels
                                    .iter()
                                    .any(|job| job.text == cell.label.as_ref())
                            })
                            .unwrap_or_else(|| {
                                panic!("{case}, {ui_size}px: no observed Pod name was emitted")
                            });
                        let projection = Projection::new(camera, (vw, vh), (0.0, 0.0));
                        let heading = projection.workload_card(&scene, 0, stage, type_).heading;
                        let body = projection
                            .cell(cell.rect, stage, type_, heading)
                            .expect("the named Pod has a body");
                        assert!(sink.fg.iter().any(|quad| quad.bounds == body.bounds));
                        assert!(
                            sink.labels.iter().any(|job| {
                                job.text == "Failed"
                                    && job.x >= f32::from(body.bounds.origin.x)
                                    && job.y >= f32::from(body.bounds.origin.y)
                                    && job.x + job.width <= f32::from(body.bounds.right())
                                    && job.y + job.height <= f32::from(body.bounds.bottom())
                            }),
                            "{case}, {ui_size}px: the named Pod has no status"
                        );
                        assert!(
                            sink.icons.iter().any(|job| matches!(
                                job,
                                IconJob::Status(Severity::Err, mark)
                                    if mark.bounds.origin.x >= body.bounds.origin.x
                                        && mark.bounds.origin.y >= body.bounds.origin.y
                                        && mark.bounds.right() <= body.bounds.right()
                                        && mark.bounds.bottom() <= body.bounds.bottom()
                            )),
                            "{case}, {ui_size}px: the named Pod has no status glyph"
                        );
                    }
                }
            }
            assert_eq!(visited, [1, 2, 3], "{case}");
        }
    }
}

#[test]
fn report_json_keeps_schema_v5_keys() {
    let meta = BenchMeta {
        machine: "linux-x86_64-i5-12600k".into(),
        churn: 120.0,
        arch: "x".into(),
        objects: 1,
        seed: 42,
        layout: "spread".into(),
        json: true,
    };
    let bench = Bench::new(meta);
    let seg = |idle| SegmentResult {
        name: "s".into(),
        quads: 10,
        lines: 2,
        glyphs: 41,
        icons: 6,
        sats: 5,
        curves: 4,
        edges: 1,
        bg_cells: 96,
        drawn: DrawnCounts {
            regions: 3,
            blocks: 20,
            cells: 100,
        },
        labels_dropped: 7,
        icons_dropped: 8,
        curves_dropped: 9,
        counters: SegmentCounters {
            quads: CounterStats {
                min: 10,
                max: 480,
                p99: 460,
            },
            glyphs: CounterStats {
                min: 41,
                max: 3316,
                p99: 3300,
            },
            ..SegmentCounters::default()
        },
        text_cache: TextCacheCounts {
            hits: 11,
            misses: 12,
            evictions: 13,
        },
        spans: FrameSpans {
            walk_us: 80.0,
            quads_us: 14.0,
            paths_us: 31.0,
            icons_us: 26.0,
            text_us: 90.0,
            hud_us: 19.0,
        },
        cpu_ms: CpuPercentiles {
            p50: 0.25,
            p99: 0.5,
        },
        frame_ms: Percentiles {
            p50: 1.0,
            p95: 2.0,
            p99: 3.0,
        },
        idle,
    };
    let result = FlightResult {
        viewport: FLIGHT_VIEWPORT,
        window: [1512.0, 837.0],
        resizes: 0,
        totals: k10s_atlas::Totals {
            regions: 3,
            blocks: 20,
            cells: 100,
            sats: 40,
            edges: 7,
        },
        segments: vec![
            seg(None),
            seg(Some(IdleResult {
                dur_s: 5.0,
                paints: 0,
                proc_cpu_ms: Some(1.5),
            })),
        ],
        restarts: 0,
    };
    let report = bench.report(&result);
    let v = serde_json::to_value(&report).unwrap();

    assert_eq!(v["schema_version"], 5);
    assert_eq!(v["machine"], "linux-x86_64-i5-12600k");
    assert_eq!(v["churn"], 120.0);
    assert_eq!(v["layout"], "spread");
    assert_eq!(v["viewport"], serde_json::json!([1600.0, 1000.0]));
    assert_eq!(
        v["window"],
        serde_json::json!([1512.0, 837.0]),
        "the real window must survive as provenance next to the pinned viewport"
    );
    assert_eq!(v["resizes"], 0);
    let totals = v["totals"].as_object().unwrap();
    let mut keys: Vec<_> = totals.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["edges", "namespaces", "pods", "sats", "workloads"]);
    assert_eq!(v["totals"]["namespaces"], 3);
    assert_eq!(v["totals"]["pods"], 100);
    assert_eq!(v["totals"]["sats"], 40);

    let s0 = v["segments"][0].as_object().unwrap();
    for key in [
        "name",
        "quads",
        "lines",
        "glyphs",
        "icons",
        "sats",
        "curves",
        "edges",
        "bg_cells",
        "drawn",
        "labels_dropped",
        "icons_dropped",
        "curves_dropped",
        "counters",
        "text_cache",
        "spans",
        "cpu_ms",
        "frame_ms",
    ] {
        assert!(s0.contains_key(key), "segment missing {key}");
    }
    assert_eq!(v["segments"][0]["counters"]["quads"]["min"], 10);
    assert_eq!(v["segments"][0]["counters"]["quads"]["max"], 480);
    assert_eq!(v["segments"][0]["counters"]["quads"]["p99"], 460);
    assert_eq!(v["segments"][0]["counters"]["glyphs"]["max"], 3316);
    assert!(
        !s0.contains_key("gate_frame"),
        "no code gates frames, so the report must not advertise it"
    );
    assert!(!s0.contains_key("idle"), "idle must be skipped when None");
    assert_eq!(v["segments"][0]["frame_ms"]["p99"], 3.0);
    assert_eq!(v["segments"][0]["cpu_ms"]["p50"], 0.25);
    assert_eq!(v["segments"][0]["lines"], 2);
    assert_eq!(v["segments"][0]["glyphs"], 41);
    assert_eq!(v["segments"][0]["labels_dropped"], 7);
    assert_eq!(v["segments"][0]["bg_cells"], 96);
    assert_eq!(v["segments"][0]["drawn"]["cells"], 100);
    assert_eq!(v["segments"][0]["text_cache"]["hits"], 11);
    let spans = v["segments"][0]["spans"].as_object().unwrap();
    let mut keys: Vec<_> = spans.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "hud_us", "icons_us", "paths_us", "quads_us", "text_us", "walk_us"
        ]
    );
    assert_eq!(v["segments"][0]["spans"]["walk_us"], 80.0);
    assert_eq!(v["segments"][0]["sats"], 5);
    assert_eq!(v["segments"][0]["curves"], 4);
    let idle = v["segments"][1]["idle"].as_object().unwrap();
    let mut keys: Vec<_> = idle.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["dur_s", "paints", "proc_cpu_ms"]);

    let table = render_table(&report, 0);
    assert!(
        table.contains("machine=linux-x86_64-i5-12600k"),
        "header must stamp machine: {table}"
    );
    assert!(
        table.contains("churn=120"),
        "header must stamp churn: {table}"
    );
    assert!(
        table.contains("idle 5 s @ churn 120: 0 paints, 1.5 ms process cpu"),
        "idle line must name churn: {table}"
    );
    assert!(
        table.contains("envelope (min..max~p99)  quads 10..480~460"),
        "an animated segment must print its counter envelope: {table}"
    );
}

#[test]
fn table_names_churn_zero_when_asked() {
    let report = BenchReport {
        schema_version: 5,
        machine: "ci-runner".into(),
        churn: 0.0,
        arch: "x".into(),
        objects: 1,
        seed: 1,
        layout: "dense".into(),
        viewport: [800.0, 600.0],
        window: [800.0, 600.0],
        resizes: 0,
        totals: BenchTotals {
            namespaces: 1,
            workloads: 2,
            pods: 3,
            sats: 4,
            edges: 5,
        },
        segments: vec![SegmentResult {
            name: "Z0 idle (no damage)".into(),
            quads: 0,
            lines: 0,
            glyphs: 0,
            icons: 0,
            sats: 0,
            curves: 0,
            edges: 0,
            bg_cells: 0,
            drawn: DrawnCounts {
                regions: 0,
                blocks: 0,
                cells: 0,
            },
            labels_dropped: 0,
            icons_dropped: 0,
            curves_dropped: 0,
            counters: SegmentCounters::default(),
            text_cache: TextCacheCounts {
                hits: 0,
                misses: 0,
                evictions: 0,
            },
            spans: FrameSpans {
                walk_us: 0.0,
                quads_us: 0.0,
                paths_us: 0.0,
                icons_us: 0.0,
                text_us: 0.0,
                hud_us: 0.0,
            },
            cpu_ms: CpuPercentiles { p50: 0.0, p99: 0.0 },
            frame_ms: Percentiles {
                p50: 0.0,
                p95: 0.0,
                p99: 0.0,
            },
            idle: Some(IdleResult {
                dur_s: 5.0,
                paints: 0,
                proc_cpu_ms: None,
            }),
        }],
    };
    let table = render_table(&report, 0);
    assert!(table.starts_with(
            "k10s bench [dense] machine=ci-runner churn=0 - 1 ns / 2 wl / 3 pods / 4 sats / 5 edges - viewport 800x600\n"
        ));
    assert!(table.contains("@ churn 0: 0 paints, n/a process cpu"));
}
