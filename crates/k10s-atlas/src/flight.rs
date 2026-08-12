use std::time::{Duration, Instant};

use serde::Serialize;

use crate::camera::Camera;
use crate::scene::{Scene, Totals};
use crate::stats::{DrawnCounts, FrameSpans, FrameStats, SegmentCounters, TextCacheCounts};

const VIEWPORT_STABLE_SECS: f32 = 0.75;
const MAX_RESTARTS: u32 = 5;

const IDLE_WAKE_PAD_SECS: f32 = 0.05;

pub const FLIGHT_VIEWPORT: [f32; 2] = [1600.0, 1000.0];

pub struct Segment {
    pub name: &'static str,
    pub from: Camera,
    pub to: Camera,
    pub dur: f32,
    pub measure: bool,
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
    pub proc_cpu_ms: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentResult {
    pub name: String,
    pub quads: usize,
    pub lines: usize,
    pub glyphs: usize,
    pub icons: usize,
    pub sats: usize,
    pub curves: usize,
    pub edges: usize,
    pub bg_cells: usize,
    pub drawn: DrawnCounts,
    pub labels_dropped: usize,
    pub icons_dropped: usize,
    pub curves_dropped: usize,
    pub counters: SegmentCounters,
    pub text_cache: TextCacheCounts,
    pub spans: FrameSpans,
    pub cpu_ms: CpuPercentiles,
    pub frame_ms: Percentiles,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<IdleResult>,
}

#[derive(Debug, Clone)]
pub struct FlightResult {
    pub viewport: [f32; 2],
    pub window: [f32; 2],
    pub resizes: u32,
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
    cpu_ms: Option<f64>,
}

pub struct Flight {
    planner: Planner,
    segments: Vec<Segment>,
    current: usize,
    seg_start: Instant,
    results: Vec<SegmentResult>,
    viewport: (f32, f32),
    window: (f32, f32),
    resizes: u32,
    totals: Totals,
    last_seen: (f32, f32),
    stable_since: Option<Instant>,
    restarts: u32,
    idle_entry: Option<IdleEntry>,
    cpu_clock: Box<dyn Fn() -> Option<f64>>,
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
            window: (0.0, 0.0),
            resizes: 0,
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
        if let Some(camera) = self.at_rest() {
            return FlightFrame::Idle {
                camera,
                arm_timer: None,
            };
        }

        if scene.rev == 0 || vw <= 0.0 || vh <= 0.0 {
            return FlightFrame::Waiting;
        }

        if !self.segments.is_empty() && self.window != (vw, vh) {
            self.resizes += 1;
        }
        self.window = (vw, vh);

        let (lvw, lvh) = (FLIGHT_VIEWPORT[0], FLIGHT_VIEWPORT[1]);

        if self.segments.is_empty() {
            if (vw, vh) != self.last_seen || !active {
                self.last_seen = if active { (vw, vh) } else { (0.0, 0.0) };
                self.stable_since = Some(now);
                let mut cam = Camera::default();
                cam.fit(scene.bounds, lvw, lvh);
                return FlightFrame::Camera(cam);
            }
            let stable = self
                .stable_since
                .is_some_and(|t| (now - t).as_secs_f32() >= VIEWPORT_STABLE_SECS);
            if !stable {
                let mut cam = Camera::default();
                cam.fit(scene.bounds, lvw, lvh);
                return FlightFrame::Camera(cam);
            }
            if !self.build(scene, lvw, lvh, now) {
                return FlightFrame::Aborted;
            }
            stats.reset();
        } else if !active {
            self.restarts += 1;
            if self.restarts >= MAX_RESTARTS {
                eprintln!(
                    "bench: window lost focus {MAX_RESTARTS} times; aborting. Keep the bench window focused, then re-run."
                );
                return FlightFrame::Aborted;
            }
            eprintln!(
                "bench: window lost focus; restarting flight ({}/{MAX_RESTARTS})",
                self.restarts
            );
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

    fn build<R, B, C, S>(
        &mut self,
        scene: &Scene<R, B, C, S>,
        vw: f32,
        vh: f32,
        now: Instant,
    ) -> bool {
        self.viewport = (vw, vh);
        self.resizes = 0;
        self.totals = scene.totals;

        let mut fit = Camera::default();
        fit.fit(scene.bounds, vw, vh);
        let Some((region_index, region)) = scene
            .regions
            .iter()
            .enumerate()
            .filter(|(_, region)| !region.children.is_empty())
            .max_by_key(|(_, r)| r.weight)
        else {
            eprintln!(
                "bench: no region in the scene has a block to fly to; aborting. Raise --objects until one does, then re-run."
            );
            return false;
        };
        let Some(block) = scene
            .region_block_indices(region_index)
            .next()
            .map(|index| &scene.blocks[index])
        else {
            eprintln!("bench: the selected region has no addressable workload; aborting flight");
            return false;
        };

        let mut hub: Option<&crate::scene::BlockNode<B>> = None;
        for b in &scene.blocks {
            let n = b.sats.end - b.sats.start;
            if n > 0 && hub.is_none_or(|h| n > h.sats.end - h.sats.start) {
                hub = Some(b);
            }
        }
        let anchors = FlightAnchors {
            fit,
            region_center: region.rect.center(),
            block_center: block.inner.center(),
            hub_center: hub.unwrap_or(block).inner.center(),
        };

        self.segments = (self.planner)(&anchors, vw, vh);
        assert!(
            !self.segments.is_empty(),
            "planner returned an empty flight"
        );
        self.seg_start = now;
        true
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

                let idle = self.idle_entry.take().map(|e| {
                    let exit = (self.cpu_clock)();
                    IdleResult {
                        dur_s: seg.dur,
                        paints: stats.frames().saturating_sub(e.frames + 1),
                        proc_cpu_ms: match (e.cpu_ms, exit) {
                            (Some(enter), Some(exit)) => Some((exit - enter) as f32),
                            _ => None,
                        },
                    }
                });
                self.results.push(SegmentResult {
                    name: seg.name.to_string(),
                    quads: stats.quads,
                    lines: stats.lines,
                    glyphs: stats.glyphs,
                    icons: stats.icons,
                    sats: stats.sats,
                    curves: stats.curves,
                    edges: stats.edges,
                    bg_cells: stats.bg_cells,
                    drawn: stats.drawn,
                    labels_dropped: stats.labels_dropped,
                    icons_dropped: stats.icons_dropped,
                    curves_dropped: stats.curves_dropped,
                    counters: stats.segment_counters(),
                    text_cache: stats.text_cache,
                    spans: stats.span_p50(),
                    cpu_ms: CpuPercentiles { p50: c50, p99: c99 },
                    frame_ms: Percentiles { p50, p95, p99 },
                    idle,
                });
            }
            self.idle_entry = None;
            stats.reset();
            self.current += 1;
            self.seg_start = now;
            if self.current == self.segments.len() {
                return self.done();
            }
        }
        FlightFrame::Camera(cam)
    }

    fn at_rest(&self) -> Option<Camera> {
        let last = self.segments.last()?;
        (self.current == self.segments.len()).then_some(last.to)
    }

    fn done(&mut self) -> FlightFrame {
        FlightFrame::Done(FlightResult {
            viewport: [self.viewport.0, self.viewport.1],
            window: [self.window.0, self.window.1],
            resizes: self.resizes,
            totals: self.totals,
            segments: std::mem::take(&mut self.results),
            restarts: self.restarts,
        })
    }
}

fn proc_cpu_ms() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|stat| proc_stat_cpu_ms(&stat, rustix::param::clock_ticks_per_second()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn proc_stat_cpu_ms(stat: &str, ticks_per_second: u64) -> Option<f64> {
    if ticks_per_second == 0 {
        return None;
    }
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_ascii_whitespace();
    let user_ticks: u64 = fields.nth(11)?.parse().ok()?;
    let system_ticks: u64 = fields.next()?.parse().ok()?;
    Some((user_ticks + system_ticks) as f64 * 1000.0 / ticks_per_second as f64)
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
#[path = "flight_test.rs"]
mod tests;
