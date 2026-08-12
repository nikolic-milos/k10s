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
    pub churn: f32,
    pub arch: String,
    pub objects: u32,
    pub seed: u64,
    pub layout: String,
    pub viewport: [f32; 2],
    pub window: [f32; 2],
    pub resizes: u32,
    pub totals: BenchTotals,
    pub segments: Vec<SegmentResult>,
}

#[derive(Clone)]
pub struct BenchMeta {
    pub machine: String,
    pub churn: f32,
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
    /// The flight cannot run to completion and has already said why. Kept apart
    /// from `Done` because a recording that did not happen must not look, to
    /// whatever is reading the exit code, like one that did.
    Aborted,
}

impl BenchFrame {
    pub fn needs_frame(&self) -> bool {
        matches!(self, BenchFrame::Waiting | BenchFrame::Camera(_))
    }
}

// What the hosting view still owes the driver after a frame: the gpui side
// effects the driver cannot perform itself. Everything else -- camera,
// pacing, reporting -- the driver has already applied.
#[must_use]
pub enum BenchOp {
    Continue,
    ArmTimer(Duration),
    Quit,
    /// Leave, and let the caller fail. The reason is already on stderr; what the
    /// caller owes is an orderly shutdown and a non-zero status, in that order.
    Abort,
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

    // The whole flight-driving decision, out of the view: apply the frame to
    // the camera, keep the pacer fed, and hand back only the effects that
    // need a window context.
    pub fn drive(
        &mut self,
        now: Instant,
        vw: f32,
        vh: f32,
        active: bool,
        scene: &SceneSnapshot,
        stats: &mut FrameStats,
        camera: &mut Camera,
        pacer: &mut k10s_atlas::FramePacer,
    ) -> BenchOp {
        let frame = self.frame(now, vw, vh, active, scene, stats);
        if frame.needs_frame() {
            pacer.request_frame();
        }
        match frame {
            BenchFrame::Waiting => BenchOp::Continue,
            BenchFrame::Camera(cam) => {
                *camera = cam;
                BenchOp::Continue
            }
            BenchFrame::Idle {
                camera: cam,
                arm_timer,
            } => {
                *camera = cam;
                match arm_timer {
                    Some(delay) => BenchOp::ArmTimer(delay),
                    None => BenchOp::Continue,
                }
            }
            BenchFrame::Done => BenchOp::Quit,
            BenchFrame::Aborted => BenchOp::Abort,
        }
    }

    fn frame(
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
            FlightFrame::Aborted => BenchFrame::Aborted,
        }
    }

    fn report(&self, r: &FlightResult) -> BenchReport {
        BenchReport {
            schema_version: 5,
            machine: self.meta.machine.clone(),
            churn: self.meta.churn,
            arch: self.meta.arch.clone(),
            objects: self.meta.objects,
            seed: self.meta.seed,
            layout: self.meta.layout.clone(),
            viewport: r.viewport,
            window: r.window,
            resizes: r.resizes,
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
        "k10s bench [{}] machine={} churn={} - {} ns / {} wl / {} pods / {} sats / {} edges - viewport {:.0}x{:.0}{}",
        report.layout,
        report.machine,
        report.churn,
        report.totals.namespaces,
        report.totals.workloads,
        report.totals.pods,
        report.totals.sats,
        report.totals.edges,
        report.viewport[0],
        report.viewport[1],
        if restarts > 0 {
            format!(" ({restarts} restart(s) after focus loss)")
        } else {
            String::new()
        },
    );
    if report.resizes > 0 {
        let _ = writeln!(
            out,
            "  window resized {} time(s) mid-flight (last {:.0}x{:.0}); the letterboxed \
             counters hold, wall-clock timings may not",
            report.resizes, report.window[0], report.window[1],
        );
    }
    for r in &report.segments {
        if let Some(idle) = &r.idle {
            let cpu = match idle.proc_cpu_ms {
                Some(ms) => format!("{ms:.1} ms process cpu"),
                None => "n/a process cpu".to_string(),
            };
            let _ = writeln!(
                out,
                "  {:<28} idle {:.0} s @ churn {}: {} paints, {cpu}",
                r.name, idle.dur_s, report.churn, idle.paints,
            );
            continue;
        }
        let _ = writeln!(
            out,
            "  {:<28} quads {:>6}  lines {:>4}  glyphs {:>6}  icons {:>4}  sats {:>4}  curves {:>4}  edges {:>4}  hex {:>4}  drawn ns/wl/pods {}/{}/{}  dropped {}L/{}I/{}C  text cache {}H/{}M/{}E",
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
            r.text_cache.hits,
            r.text_cache.misses,
            r.text_cache.evictions,
        );
        if !r.counters.is_steady() {
            let range = |c: &k10s_atlas::CounterStats| format!("{}..{}~{}", c.min, c.max, c.p99);
            let _ = writeln!(
                out,
                "  {:<28} envelope (min..max~p99)  quads {}  lines {}  glyphs {}  icons {}  sats {}  curves {}  edges {}  hex {}",
                "",
                range(&r.counters.quads),
                range(&r.counters.lines),
                range(&r.counters.glyphs),
                range(&r.counters.icons),
                range(&r.counters.sats),
                range(&r.counters.curves),
                range(&r.counters.edges),
                range(&r.counters.bg_cells),
            );
        }
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
#[path = "bench_test.rs"]
mod tests;
