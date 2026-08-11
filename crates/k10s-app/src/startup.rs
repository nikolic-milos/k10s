//! Process-start to first-presentation measurement.
//!
//! The clock begins at the first line of Rust `main`, like Zed's startup
//! clock. That deliberately excludes the dynamic loader. GPUI does not expose
//! GPU/compositor presentation feedback, so "presented" here means the first
//! platform frame callback after GPUI submitted the observed frame. It is the
//! closest portable boundary available without teaching GPUI a new API, and
//! the report names it rather than claiming physical scan-out.

use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use gpui::App;
use k10s_core::SceneSnapshot;
use k10s_map::PresentProbe;
use serde::Serialize;

use crate::cli;

const SCHEMA_VERSION: u8 = 2;

#[derive(Clone)]
pub struct StartupBench {
    state: Arc<Mutex<State>>,
    failed: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
}

impl StartupBench {
    pub fn new(started: Instant, arguments_parsed: Instant, args: &cli::Args) -> Self {
        let source = if args.cluster {
            "cluster"
        } else if args.scene_was_named() {
            "generator"
        } else {
            "launch"
        };
        let generator = (source == "generator").then(|| GeneratorMeta {
            objects: args.objects,
            seed: args.seed,
            scenario: args.scenario.as_str().to_string(),
            layout: args.layout.as_str().to_string(),
            churn_per_second: args.effective_churn(),
        });
        Self {
            state: Arc::new(Mutex::new(State {
                started,
                arguments_parsed,
                source_ready: None,
                content_ready: None,
                matching_scene_published: None,
                world_spawned: None,
                platform_started: None,
                application_ready: None,
                fonts_ready: None,
                configuration_ready: None,
                window_built: None,
                first_presented: None,
                viewport: None,
                machine: args.machine_label(),
                platform: cli::platform(),
                source,
                generator,
                json: args.json,
            })),
            failed: Arc::new(AtomicBool::new(false)),
            completed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn source_ready(&self) {
        set_once(&mut self.lock().source_ready, Instant::now());
    }

    pub fn content_ready(&self) {
        set_once(&mut self.lock().content_ready, Instant::now());
    }

    /// Record publication of a snapshot that can represent the requested scene.
    ///
    /// This is deliberately independent of `content_ready`: the generator and
    /// world run on different threads, so correctness cannot depend on which
    /// callback wins a race. The report defines scene readiness as the later of
    /// content completion and matching-snapshot publication.
    pub fn scene_published(&self, scene: &SceneSnapshot) {
        let mut state = self.lock();
        if state.scene_matches_request(scene) {
            set_once(&mut state.matching_scene_published, Instant::now());
        }
    }

    pub fn world_spawned(&self) {
        set_once(&mut self.lock().world_spawned, Instant::now());
    }

    pub fn platform_started(&self) {
        set_once(&mut self.lock().platform_started, Instant::now());
    }

    pub fn application_ready(&self) {
        set_once(&mut self.lock().application_ready, Instant::now());
    }

    pub fn fonts_ready(&self) {
        set_once(&mut self.lock().fonts_ready, Instant::now());
    }

    pub fn configuration_ready(&self) {
        set_once(&mut self.lock().configuration_ready, Instant::now());
    }

    pub fn window_built(&self, viewport: [f32; 2]) {
        let mut state = self.lock();
        set_once(&mut state.window_built, Instant::now());
        state.viewport.get_or_insert(viewport);
    }

    /// Observe the first frame, and optionally wait for a published scene.
    ///
    /// A bare launch is useful when its chooser appears. A named generator or
    /// cluster is useful only after its replacement snapshot crossed the same
    /// presentation boundary.
    pub fn present_probe(&self, wait_for_scene: bool) -> PresentProbe {
        let first = self.clone();
        let probe = PresentProbe::first(move |presented_at, cx| {
            if wait_for_scene {
                first.record_first(presented_at);
            } else {
                first.finish(presented_at, cx);
            }
        });
        if wait_for_scene {
            let readiness = self.clone();
            let useful = self.clone();
            probe.on_scene_when(
                move |scene| readiness.scene_is_useful(scene),
                move |presented_at, cx| useful.finish(presented_at, cx),
            )
        } else {
            probe
        }
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    pub fn completed(&self) -> bool {
        self.completed.load(Ordering::Relaxed)
    }

    fn record_first(&self, presented_at: Instant) {
        set_once(&mut self.lock().first_presented, presented_at);
    }

    fn scene_is_useful(&self, scene: &SceneSnapshot) -> bool {
        let state = self.lock();
        state.content_ready.is_some() && state.scene_matches_request(scene)
    }
}

impl State {
    fn scene_matches_request(&self, scene: &SceneSnapshot) -> bool {
        // Named scenes are rebuilt behind the initial empty world. World
        // revisions are monotonic across that replacement, so revision two is
        // the first value that can belong to the requested cluster or
        // generator -- including an intentionally empty one.
        if scene.rev == 0 || (self.source != "launch" && scene.rev < 2) {
            return false;
        }
        match &self.generator {
            Some(generator) if generator.objects > 0 => {
                let totals = scene.totals;
                totals.regions != 0
                    || totals.blocks != 0
                    || totals.cells != 0
                    || totals.sats != 0
                    || totals.edges != 0
            }
            _ => true,
        }
    }
}

impl StartupBench {
    fn finish(&self, useful_presented: Instant, cx: &mut App) {
        let report = {
            let mut state = self.lock();
            set_once(&mut state.first_presented, useful_presented);
            state.report(useful_presented)
        };
        match report {
            Ok((report, json)) => {
                if let Err(error) = write_report(&report, json) {
                    self.fail(format!("cannot write the startup report: {error}"));
                } else {
                    self.completed.store(true, Ordering::Relaxed);
                }
            }
            Err(error) => self.fail(error.to_string()),
        }
        cx.quit();
    }

    fn fail(&self, message: String) {
        self.failed.store(true, Ordering::Relaxed);
        eprintln!("k10s: {message}");
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn set_once(slot: &mut Option<Instant>, at: Instant) {
    slot.get_or_insert(at);
}

struct State {
    started: Instant,
    arguments_parsed: Instant,
    source_ready: Option<Instant>,
    content_ready: Option<Instant>,
    matching_scene_published: Option<Instant>,
    world_spawned: Option<Instant>,
    platform_started: Option<Instant>,
    application_ready: Option<Instant>,
    fonts_ready: Option<Instant>,
    configuration_ready: Option<Instant>,
    window_built: Option<Instant>,
    first_presented: Option<Instant>,
    viewport: Option<[f32; 2]>,
    machine: String,
    platform: String,
    source: &'static str,
    generator: Option<GeneratorMeta>,
    json: bool,
}

impl State {
    fn report(&self, useful_presented: Instant) -> Result<(StartupReport, bool), MissingMilestone> {
        let source_ready = self.source_ready.ok_or(MissingMilestone("source_ready"))?;
        let world_spawned = self
            .world_spawned
            .ok_or(MissingMilestone("world_spawned"))?;
        let platform_started = self
            .platform_started
            .ok_or(MissingMilestone("platform_started"))?;
        let application_ready = self
            .application_ready
            .ok_or(MissingMilestone("application_ready"))?;
        let fonts_ready = self.fonts_ready.ok_or(MissingMilestone("fonts_ready"))?;
        let configuration_ready = self
            .configuration_ready
            .ok_or(MissingMilestone("configuration_ready"))?;
        let window_built = self.window_built.ok_or(MissingMilestone("window_built"))?;
        let first_presented = self
            .first_presented
            .ok_or(MissingMilestone("first_presented"))?;
        let viewport = self.viewport.ok_or(MissingMilestone("viewport"))?;
        let content_ready = match (self.source, self.content_ready) {
            ("launch", content_ready) => content_ready,
            (_, Some(content_ready)) => Some(content_ready),
            (_, None) => return Err(MissingMilestone("content_ready")),
        };
        let scene_ready = match (self.source, content_ready, self.matching_scene_published) {
            ("launch", _, _) => None,
            (_, Some(content_ready), Some(scene_published)) => {
                Some(content_ready.max(scene_published))
            }
            (_, _, None) => return Err(MissingMilestone("matching_scene_published")),
            (_, None, _) => return Err(MissingMilestone("content_ready")),
        };
        let report = StartupReport {
            schema_version: SCHEMA_VERSION,
            mode: "startup",
            machine: self.machine.clone(),
            platform: self.platform.clone(),
            source: self.source,
            generator: self.generator.clone(),
            viewport,
            milestones_ms: Milestones {
                arguments_parsed: elapsed_ms(self.started, self.arguments_parsed),
                source_ready: elapsed_ms(self.started, source_ready),
                content_ready: content_ready.map(|at| elapsed_ms(self.started, at)),
                scene_ready: scene_ready.map(|at| elapsed_ms(self.started, at)),
                world_spawned: elapsed_ms(self.started, world_spawned),
                platform_started: elapsed_ms(self.started, platform_started),
                application_ready: elapsed_ms(self.started, application_ready),
                fonts_ready: elapsed_ms(self.started, fonts_ready),
                configuration_ready: elapsed_ms(self.started, configuration_ready),
                window_built: elapsed_ms(self.started, window_built),
                first_presented: elapsed_ms(self.started, first_presented),
                useful_presented: elapsed_ms(self.started, useful_presented),
            },
            phases_ms: Phases {
                argument_parse: elapsed_ms(self.started, self.arguments_parsed),
                source_prepare: elapsed_ms(self.arguments_parsed, source_ready),
                content_prepare: content_ready.map(|at| elapsed_ms(self.arguments_parsed, at)),
                scene_ready_after_content: scene_ready
                    .zip(content_ready)
                    .map(|(scene, content)| elapsed_ms(content, scene)),
                world_start: elapsed_ms(source_ready, world_spawned),
                application_setup: elapsed_ms(world_spawned, platform_started),
                platform_launch: elapsed_ms(platform_started, application_ready),
                font_registration: elapsed_ms(application_ready, fonts_ready),
                configuration: elapsed_ms(fonts_ready, configuration_ready),
                window_open: elapsed_ms(configuration_ready, window_built),
                first_present: elapsed_ms(window_built, first_presented),
                useful_after_first: elapsed_ms(first_presented, useful_presented),
                useful_after_content: content_ready.map(|at| elapsed_ms(at, useful_presented)),
                useful_after_scene: scene_ready.map(|at| elapsed_ms(at, useful_presented)),
                total: elapsed_ms(self.started, useful_presented),
            },
        };
        Ok((report, self.json))
    }
}

#[derive(Debug)]
struct MissingMilestone(&'static str);

impl std::fmt::Display for MissingMilestone {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "startup measurement reached a useful frame without milestone {}",
            self.0
        )
    }
}

fn elapsed_ms(start: Instant, end: Instant) -> f64 {
    end.saturating_duration_since(start).as_secs_f64() * 1_000.0
}

#[derive(Clone, Serialize)]
struct GeneratorMeta {
    objects: u32,
    seed: u64,
    scenario: String,
    layout: String,
    churn_per_second: f32,
}

#[derive(Serialize)]
struct StartupReport {
    schema_version: u8,
    mode: &'static str,
    machine: String,
    platform: String,
    source: &'static str,
    generator: Option<GeneratorMeta>,
    viewport: [f32; 2],
    milestones_ms: Milestones,
    phases_ms: Phases,
}

#[derive(Serialize)]
struct Milestones {
    arguments_parsed: f64,
    source_ready: f64,
    content_ready: Option<f64>,
    scene_ready: Option<f64>,
    world_spawned: f64,
    platform_started: f64,
    application_ready: f64,
    fonts_ready: f64,
    configuration_ready: f64,
    window_built: f64,
    first_presented: f64,
    useful_presented: f64,
}

#[derive(Serialize)]
struct Phases {
    argument_parse: f64,
    source_prepare: f64,
    content_prepare: Option<f64>,
    scene_ready_after_content: Option<f64>,
    world_start: f64,
    application_setup: f64,
    platform_launch: f64,
    font_registration: f64,
    configuration: f64,
    window_open: f64,
    first_present: f64,
    useful_after_first: f64,
    useful_after_content: Option<f64>,
    useful_after_scene: Option<f64>,
    total: f64,
}

fn write_report(report: &StartupReport, json: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if json {
        serde_json::to_writer(&mut output, report).map_err(io::Error::other)?;
        writeln!(output)?;
    } else {
        let source_prepare = report
            .phases_ms
            .content_prepare
            .unwrap_or(report.phases_ms.source_prepare);
        writeln!(
            output,
            "startup {}: first {:.2} ms, useful {:.2} ms \
             [source {:.2}, scene {:.2}, platform {:.2}, fonts {:.2}, window {:.2}, present {:.2}]",
            report.source,
            report.milestones_ms.first_presented,
            report.milestones_ms.useful_presented,
            source_prepare,
            report.phases_ms.scene_ready_after_content.unwrap_or(0.0),
            report.phases_ms.platform_launch,
            report.phases_ms.font_registration,
            report.phases_ms.window_open,
            report.phases_ms.first_present,
        )?;
    }
    output.flush()
}

#[cfg(test)]
#[path = "startup_test.rs"]
mod tests;
