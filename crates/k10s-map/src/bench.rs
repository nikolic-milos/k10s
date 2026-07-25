use std::time::{Duration, Instant};

use k10s_atlas::flight::{
    Flight, FlightAnchors, FlightFrame, FlightResult, Segment, SegmentResult,
};
use k10s_atlas::{Camera, FrameStats};
use k10s_core::SceneSnapshot;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct BenchTotals {
    pub namespaces: u32,
    pub workloads: u32,
    pub pods: u32,
    pub sats: u32,
    pub edges: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub schema_version: u32,
    pub machine: String,
    pub arch: String,
    pub objects: u32,
    pub seed: u64,
    pub layout: String,
    pub viewport: [f32; 2],
    pub totals: BenchTotals,
    pub segments: Vec<SegmentResult>,
}

#[derive(Clone)]
pub struct BenchMeta {
    pub machine: String,
    pub arch: String,
    pub objects: u32,
    pub seed: u64,
    pub layout: String,
    pub json: bool,
}

pub enum BenchFrame {
    Waiting,
    Camera(Camera),
    Idle {
        camera: Camera,
        arm_timer: Option<Duration>,
    },
    Done,
}

impl BenchFrame {
    pub fn needs_frame(&self) -> bool {
        matches!(self, BenchFrame::Waiting | BenchFrame::Camera(_))
    }
}

fn plan(a: &FlightAnchors, vw: f32, _vh: f32) -> Vec<Segment> {
    let at = |cx: f32, cy: f32, zoom: f32| Camera { cx, cy, zoom };
    let (rx, ry) = a.region_center;
    let (hx, hy) = a.hub_center;
    let fit = a.fit;
    let z1 = at(rx, ry, 0.12);

    let hub = at(hx, hy, 2.2);
    let z3 = at(hx, hy, 4.5);

    let world_w = vw / fit.zoom;
    let half_span = (vw / 0.12 * 2.0).min(world_w * 0.35);
    let z1_left = at(rx - half_span, ry, 0.12);
    let z1_right = at(rx + half_span, ry, 0.12);
    let pan_w2 = vw / 2.2;
    let hub_left = at(hx - pan_w2 * 0.5, hy, 2.2);
    let hub_right = at(hx + pan_w2 * 0.5, hy, 2.2);

    let seg = |name, from, to| Segment {
        name,
        from,
        to,
        dur: 3.0,
        measure: true,
        idle: false,
    };
    vec![
        Segment {
            name: "warmup",
            from: fit,
            to: fit,
            dur: 2.0,
            measure: false,
            idle: false,
        },
        seg("Z0 static (island overview)", fit, fit),
        seg("Z0->Z1 fly-in", fit, z1),
        seg("Z1 static (workload cards)", z1, z1),
        seg("Z1 cross-map pan", z1_left, z1_right),
        seg("Z1->Z2 fly-in (hub)", z1, hub),
        seg("Z2 static (hub + satellites)", hub, hub),
        seg("Z2 hub pan", hub_left, hub_right),
        seg("Z2->Z3 fly-in", hub, z3),
        seg("Z3 static (pod detail)", z3, z3),
        seg("Z3->Z0 fly-out", z3, fit),
        Segment {
            name: "Z0 idle (no damage)",
            from: fit,
            to: fit,
            dur: 5.0,
            measure: true,
            idle: true,
        },
    ]
}

pub struct Bench {
    flight: Flight,
    meta: BenchMeta,
}

impl Bench {
    pub fn new(meta: BenchMeta) -> Self {
        Bench {
            flight: Flight::new(plan),
            meta,
        }
    }

    pub fn frame(
        &mut self,
        now: Instant,
        vw: f32,
        vh: f32,
        active: bool,
        scene: &SceneSnapshot,
        stats: &mut FrameStats,
    ) -> BenchFrame {
        match self.flight.frame(now, vw, vh, active, scene, stats) {
            FlightFrame::Waiting => BenchFrame::Waiting,
            FlightFrame::Camera(cam) => BenchFrame::Camera(cam),
            FlightFrame::Idle { camera, arm_timer } => BenchFrame::Idle { camera, arm_timer },
            FlightFrame::Done(result) => {
                self.finish(&result);
                BenchFrame::Done
            }
            FlightFrame::Aborted => std::process::exit(3),
        }
    }

    fn report(&self, r: &FlightResult) -> BenchReport {
        BenchReport {
            schema_version: 2,
            machine: self.meta.machine.clone(),
            arch: self.meta.arch.clone(),
            objects: self.meta.objects,
            seed: self.meta.seed,
            layout: self.meta.layout.clone(),
            viewport: r.viewport,
            totals: BenchTotals {
                namespaces: r.totals.regions,
                workloads: r.totals.blocks,
                pods: r.totals.cells,
                sats: r.totals.sats,
                edges: r.totals.edges,
            },
            segments: r.segments.clone(),
        }
    }

    fn finish(&self, result: &FlightResult) {
        let report = self.report(result);
        let table = render_table(&report, result.restarts);
        if self.meta.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("serialize bench")
            );
            eprint!("{table}");
        } else {
            print!("{table}");
        }
    }
}

fn render_table(report: &BenchReport, restarts: u32) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "k10s bench [{}] - {} ns / {} wl / {} pods / {} sats / {} edges - viewport {:.0}x{:.0}{}",
        report.layout,
        report.totals.namespaces,
        report.totals.workloads,
        report.totals.pods,
        report.totals.sats,
        report.totals.edges,
        report.viewport[0],
        report.viewport[1],
        if restarts > 0 {
            format!(" ({restarts} restart(s) after resize)")
        } else {
            String::new()
        },
    );
    for r in &report.segments {
        if let Some(idle) = &r.idle {
            let _ = writeln!(
                out,
                "  {:<28} idle {:.0} s: {} paints, {:.1} ms process cpu",
                r.name, idle.dur_s, idle.paints, idle.proc_cpu_ms,
            );
            continue;
        }
        let _ = writeln!(
            out,
            "  {:<28} quads {:>6}  lines {:>4}  glyphs {:>6}  icons {:>4}  sats {:>4}  curves {:>4}  edges {:>4}  hex {:>4}  drawn ns/wl/pods {}/{}/{}  dropped {}L/{}I/{}C",
            r.name,
            r.quads,
            r.lines,
            r.glyphs,
            r.icons,
            r.sats,
            r.curves,
            r.edges,
            r.bg_cells,
            r.drawn.regions,
            r.drawn.blocks,
            r.drawn.cells,
            r.labels_dropped,
            r.icons_dropped,
            r.curves_dropped,
        );
        let _ = writeln!(
            out,
            "  {:<28} spans us  walk {:>6.0}  quads {:>5.0}  paths {:>6.0}  icons {:>5.0}  text {:>6.0}  hud {:>5.0}",
            "",
            r.spans.walk_us,
            r.spans.quads_us,
            r.spans.paths_us,
            r.spans.icons_us,
            r.spans.text_us,
            r.spans.hud_us,
        );
        let _ = writeln!(
            out,
            "  {:<28} cpu p50 {:5.2}  p99 {:5.2} ms (hud excluded)  |  frame p50 {:5.1}  p95 {:5.1}  p99 {:5.1} ms (informational, vsync-bound)",
            "", r.cpu_ms.p50, r.cpu_ms.p99, r.frame_ms.p50, r.frame_ms.p95, r.frame_ms.p99,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_atlas::flight::{CpuPercentiles, IdleResult, Percentiles};
    use k10s_atlas::{DrawnCounts, FrameSpans};

    fn anchors() -> FlightAnchors {
        let mut fit = Camera::default();
        fit.fit(
            k10s_core::Rect::new(0.0, 0.0, 10_000.0, 6_000.0),
            1600.0,
            1000.0,
        );
        FlightAnchors {
            fit,
            region_center: (1000.0, 800.0),
            block_center: (1050.0, 830.0),
            hub_center: (2400.0, 1400.0),
        }
    }

    #[test]
    fn plan_keeps_its_shape_and_zoom_targets() {
        let a = anchors();
        let segs = plan(&a, 1600.0, 1000.0);

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
        for zoom in [a.fit.zoom, 0.12, 2.2, 4.5] {
            assert!(targets.contains(&zoom), "zoom target {zoom} went missing");
        }
        assert!(
            segs.iter()
                .any(|s| s.to.zoom == 2.2 && (s.to.cx, s.to.cy) == a.hub_center),
            "hub zoom must show two-line sat labels"
        );
        assert!(
            segs.iter()
                .any(|s| s.to.zoom == 4.5 && (s.to.cx, s.to.cy) == a.hub_center),
            "pod detail must sit on the hub"
        );
        assert!(
            segs.iter()
                .any(|s| s.to.zoom == 0.12 && s.from.cx != s.to.cx),
            "flight must include a Z1 pan"
        );
    }

    #[test]
    fn report_json_keeps_schema_v2_keys() {
        let meta = BenchMeta {
            machine: "m".into(),
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
            viewport: [1600.0, 1000.0],
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
                    proc_cpu_ms: 1.5,
                })),
            ],
            restarts: 0,
        };
        let v = serde_json::to_value(bench.report(&result)).unwrap();

        assert_eq!(v["schema_version"], 2);
        assert_eq!(v["layout"], "spread");
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
            "spans",
            "cpu_ms",
            "frame_ms",
        ] {
            assert!(s0.contains_key(key), "segment missing {key}");
        }
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
    }
}
