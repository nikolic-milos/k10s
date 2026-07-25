use std::time::{Duration, Instant};

use serde::Serialize;

use crate::camera::Camera;
use crate::scene::{Scene, Totals};
use crate::stats::{FrameSpans, FrameStats};

const VIEWPORT_STABLE_SECS: f32 = 0.75;
const MAX_RESTARTS: u32 = 5;

const IDLE_WAKE_PAD_SECS: f32 = 0.05;

pub struct Segment {
    pub name: &'static str,
    pub from: Camera,
    pub to: Camera,
    pub dur: f32,
    pub measure: bool,
    pub gate_frame: bool,
    pub idle: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Percentiles {
    pub p50: f32,
    pub p95: f32,
    pub p99: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuPercentiles {
    pub p50: f32,
    pub p99: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdleResult {
    pub dur_s: f32,
    pub paints: u64,
    pub proc_cpu_ms: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentResult {
    pub name: String,
    pub gate_frame: bool,
    pub frame_ms: Percentiles,
    pub cpu_ms: CpuPercentiles,
    pub spans: FrameSpans,
    pub quads: usize,
    pub lines: usize,
    pub glyphs: usize,
    pub edges: usize,
    pub sats: usize,
    pub curves: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<IdleResult>,
}

#[derive(Debug, Clone)]
pub struct FlightResult {
    pub viewport: [f32; 2],
    pub totals: Totals,
    pub segments: Vec<SegmentResult>,
    pub restarts: u32,
}

pub struct FlightAnchors {
    pub fit: Camera,
    pub region_center: (f32, f32),
    pub block_center: (f32, f32),
    pub hub_center: (f32, f32),
}

pub type Planner = Box<dyn Fn(&FlightAnchors, f32, f32) -> Vec<Segment>>;

pub enum FlightFrame {
    Waiting,
    Camera(Camera),

    Idle {
        camera: Camera,
        arm_timer: Option<Duration>,
    },
    Done(FlightResult),
    Aborted,
}

impl FlightFrame {
    pub fn needs_frame(&self) -> bool {
        matches!(self, FlightFrame::Waiting | FlightFrame::Camera(_))
    }
}

struct IdleEntry {
    frames: u64,
    cpu_ms: f64,
}

pub struct Flight {
    planner: Planner,
    segments: Vec<Segment>,
    current: usize,
    seg_start: Instant,
    results: Vec<SegmentResult>,
    viewport: (f32, f32),
    totals: Totals,
    last_seen: (f32, f32),
    stable_since: Option<Instant>,
    restarts: u32,
    idle_entry: Option<IdleEntry>,
    cpu_clock: Box<dyn Fn() -> f64>,
}

impl Flight {
    pub fn new(planner: impl Fn(&FlightAnchors, f32, f32) -> Vec<Segment> + 'static) -> Self {
        Flight {
            planner: Box::new(planner),
            segments: Vec::new(),
            current: 0,
            seg_start: Instant::now(),
            results: Vec::new(),
            viewport: (0.0, 0.0),
            totals: Totals::default(),
            last_seen: (0.0, 0.0),
            stable_since: None,
            restarts: 0,
            idle_entry: None,
            cpu_clock: Box::new(proc_cpu_ms),
        }
    }

    pub fn frame<R, B, C, S>(
        &mut self,
        now: Instant,
        vw: f32,
        vh: f32,
        active: bool,
        scene: &Scene<R, B, C, S>,
        stats: &mut FrameStats,
    ) -> FlightFrame {
        if scene.rev == 0 || vw <= 0.0 || vh <= 0.0 {
            return FlightFrame::Waiting;
        }

        if self.segments.is_empty() {
            if (vw, vh) != self.last_seen || !active {
                self.last_seen = if active { (vw, vh) } else { (0.0, 0.0) };
                self.stable_since = Some(now);
                let mut cam = Camera::default();
                cam.fit(scene.bounds, vw, vh);
                return FlightFrame::Camera(cam);
            }
            let stable = self
                .stable_since
                .is_some_and(|t| (now - t).as_secs_f32() >= VIEWPORT_STABLE_SECS);
            if !stable {
                let mut cam = Camera::default();
                cam.fit(scene.bounds, vw, vh);
                return FlightFrame::Camera(cam);
            }
            self.build(scene, vw, vh, now);
            stats.reset();
        } else if (vw, vh) != self.viewport || !active {
            self.restarts += 1;
            let why = if active {
                "viewport changed"
            } else {
                "window lost focus"
            };
            eprintln!(
                "bench: {why} ({:.0}x{:.0} -> {vw:.0}x{vh:.0}, active={active}); restarting flight ({}/{MAX_RESTARTS})",
                self.viewport.0, self.viewport.1, self.restarts
            );
            if self.restarts >= MAX_RESTARTS {
                eprintln!(
                    "bench: conditions keep changing; aborting. Keep the bench window focused and at a fixed size, then re-run."
                );
                return FlightFrame::Aborted;
            }
            self.results.clear();
            self.segments.clear();
            self.current = 0;
            self.last_seen = (0.0, 0.0);
            self.stable_since = Some(now);
            self.idle_entry = None;
            return FlightFrame::Waiting;
        }

        self.step(now, stats)
    }

    fn build<R, B, C, S>(&mut self, scene: &Scene<R, B, C, S>, vw: f32, vh: f32, now: Instant) {
        self.viewport = (vw, vh);
        self.totals = scene.totals;

        let mut fit = Camera::default();
        fit.fit(scene.bounds, vw, vh);
        let big = scene
            .regions
            .iter()
            .max_by_key(|r| r.weight)
            .expect("flight needs a non-empty scene");
        let block = &scene.blocks[big.children.start as usize];

        let mut hub: Option<&crate::scene::BlockNode<B>> = None;
        for b in &scene.blocks {
            let n = b.sats.end - b.sats.start;
            if n > 0 && hub.is_none_or(|h| n > h.sats.end - h.sats.start) {
                hub = Some(b);
            }
        }
        let anchors = FlightAnchors {
            fit,
            region_center: big.rect.center(),
            block_center: block.inner.center(),
            hub_center: hub.unwrap_or(block).inner.center(),
        };

        self.segments = (self.planner)(&anchors, vw, vh);
        assert!(
            !self.segments.is_empty(),
            "planner returned an empty flight"
        );
        self.seg_start = now;
    }

    fn step(&mut self, now: Instant, stats: &mut FrameStats) -> FlightFrame {
        let seg = &self.segments[self.current];
        let elapsed = (now - self.seg_start).as_secs_f32();
        let t = (elapsed / seg.dur).min(1.0);
        let cam = lerp_cam(seg.from, seg.to, smoothstep(t));

        if seg.idle && t < 1.0 {
            let arm_timer = if self.idle_entry.is_none() {
                self.idle_entry = Some(IdleEntry {
                    frames: stats.frames(),
                    cpu_ms: (self.cpu_clock)(),
                });
                Some(Duration::from_secs_f32(
                    seg.dur - elapsed + IDLE_WAKE_PAD_SECS,
                ))
            } else {
                None
            };
            return FlightFrame::Idle {
                camera: cam,
                arm_timer,
            };
        }

        if t >= 1.0 {
            if seg.measure {
                let (p50, p95, p99) = stats.frame_percentiles();
                let (c50, c99) = stats.cpu_percentiles();

                let idle = self.idle_entry.take().map(|e| IdleResult {
                    dur_s: seg.dur,
                    paints: stats.frames().saturating_sub(e.frames + 1),
                    proc_cpu_ms: ((self.cpu_clock)() - e.cpu_ms) as f32,
                });
                self.results.push(SegmentResult {
                    name: seg.name.to_string(),
                    gate_frame: seg.gate_frame,
                    frame_ms: Percentiles { p50, p95, p99 },
                    cpu_ms: CpuPercentiles { p50: c50, p99: c99 },
                    spans: stats.span_p50(),
                    quads: stats.quads,
                    lines: stats.lines,
                    glyphs: stats.glyphs,
                    edges: stats.edges,
                    sats: stats.sats,
                    curves: stats.curves,
                    idle,
                });
            }
            self.idle_entry = None;
            stats.reset();
            self.current += 1;
            self.seg_start = now;
            if self.current == self.segments.len() {
                return FlightFrame::Done(FlightResult {
                    viewport: [self.viewport.0, self.viewport.1],
                    totals: self.totals,
                    segments: std::mem::take(&mut self.results),
                    restarts: self.restarts,
                });
            }
        }
        FlightFrame::Camera(cam)
    }
}

fn proc_cpu_ms() -> f64 {
    #[cfg(target_os = "linux")]
    {
        const TICK_MS: f64 = 1000.0 / 100.0;

        if let Ok(stat) = std::fs::read_to_string("/proc/self/stat")
            && let Some((_, rest)) = stat.rsplit_once(')')
        {
            let mut fields = rest.split_ascii_whitespace();
            let utime: f64 = fields.nth(11).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let stime: f64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            return (utime + stime) * TICK_MS;
        }
        0.0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0.0
    }
}

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

fn lerp_cam(a: Camera, b: Camera, t: f32) -> Camera {
    Camera {
        cx: a.cx + (b.cx - a.cx) * t,
        cy: a.cy + (b.cy - a.cy) * t,
        zoom: (a.zoom.ln() + (b.zoom.ln() - a.zoom.ln()) * t).exp(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{BlockNode, CellNode, Rect, RegionNode};
    use std::cell::Cell;
    use std::rc::Rc;

    fn tiny_scene() -> Scene {
        let block_rect = Rect::new(10.0, 10.0, 50.0, 30.0);
        Scene {
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
            edges: vec![],
            region_edges: vec![],
            cross_edges: 0..0,
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
            gate_frame: false,
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
        let cpu = Rc::new(Cell::new(1000.0f64));
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
        cpu.set(1000.0 + 12.5);

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
        assert!(
            (idle.proc_cpu_ms - 12.5).abs() < 1e-3,
            "cpu {}",
            idle.proc_cpu_ms
        );
        assert_eq!(idle.dur_s, 5.0);
        assert!(!last.gate_frame);
        assert_eq!(
            result.segments.iter().filter(|s| s.idle.is_some()).count(),
            1,
            "exactly one idle segment"
        );
        assert_eq!(result.totals.cells, 1);
        assert_eq!(result.viewport, [vw, vh]);
        assert_eq!(result.restarts, 0);
    }

    #[test]
    fn restart_budget_aborts() {
        let mut flight = Flight::new(test_plan);
        let scene = tiny_scene();
        let mut stats = FrameStats::default();
        let t0 = Instant::now();
        let at = |s: f32| t0 + Duration::from_secs_f32(s);

        let mut now_s = 0.0f32;
        let mut aborted = false;
        for round in 0..MAX_RESTARTS + 1 {
            let vw = 1600.0 + round as f32;
            assert!(matches!(
                flight.frame(at(now_s), vw, 1000.0, true, &scene, &mut stats),
                FlightFrame::Camera(_)
            ));
            now_s += 1.0;
            match flight.frame(at(now_s), vw, 1000.0, true, &scene, &mut stats) {
                FlightFrame::Camera(_) => {}
                other => panic!(
                    "expected planned flight, got needs_frame={}",
                    other.needs_frame()
                ),
            }
            now_s += 0.5;
            match flight.frame(at(now_s), vw + 100.0, 1000.0, true, &scene, &mut stats) {
                FlightFrame::Waiting => {}
                FlightFrame::Aborted => {
                    aborted = true;
                    break;
                }
                _ => panic!("resize must restart or abort"),
            }
            now_s += 0.5;
        }
        assert!(aborted, "flight must abort after {MAX_RESTARTS} restarts");
    }
}
