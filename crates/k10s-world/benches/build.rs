//! Where the first snapshot's time goes: fold, layout, assemble, publish.
//!
//! Deliberately a coarse phase splitter and not a latency gate. Each phase is
//! milliseconds to seconds at these object counts, so a median of five samples
//! assigns a regression to a phase without the budgeted sampling and rMAD the
//! nanosecond benches (`publish`, `fanout_cull`) need to be believed. Read a
//! number here to learn which phase moved, then measure that phase properly.

use std::hint::black_box;
use std::time::Duration;

use k10s_clustergen::stream;
use k10s_clustergen::{GenConfig, Scenario, generate};
use k10s_world::{
    LayoutMode, WorldBuildProfile, profile_prepared_world_build, profile_world_build,
};

const OBJECT_COUNTS: [u32; 2] = [25_000, 1_000_000];
const WARMUP: usize = 1;
const SAMPLES: usize = 5;
const MODE: LayoutMode = LayoutMode::Spread;

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn medians(samples: &[WorldBuildProfile]) -> WorldBuildProfile {
    let field = |read: fn(&WorldBuildProfile) -> Duration| {
        let mut values: Vec<_> = samples.iter().map(read).collect();
        median(&mut values)
    };
    WorldBuildProfile {
        fold: field(|sample| sample.fold),
        layout: field(|sample| sample.layout),
        assemble: field(|sample| sample.assemble),
        publish: field(|sample| sample.publish),
        total: field(|sample| sample.total),
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn run_case(
    source: &str,
    objects: u32,
    mut profile_build: impl FnMut() -> (WorldBuildProfile, std::sync::Arc<k10s_core::SceneSnapshot>),
) {
    for _ in 0..WARMUP {
        let (_, snapshot) = profile_build();
        black_box(snapshot);
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let (profile, snapshot) = profile_build();
        samples.push(profile);
        black_box(snapshot);
    }
    let profile = medians(&samples);
    println!(
        "{source:<8}  {objects:>7}  {:>8.2}  {:>8.2}  {:>8.2}  {:>8.2}  {:>8.2}",
        milliseconds(profile.fold),
        milliseconds(profile.layout),
        milliseconds(profile.assemble),
        milliseconds(profile.publish),
        milliseconds(profile.total),
    );
}

fn main() {
    println!(
        "k10s-world initial build - {} layout, {SAMPLES} samples, no renderer",
        MODE.as_str()
    );
    println!("source     objects      fold    layout  assemble  publish    total");
    for objects in OBJECT_COUNTS {
        let spec = generate(&GenConfig {
            seed: 55,
            target_objects: objects,
            scenario: Scenario::Platform,
        });
        let events = stream::snapshot(&spec, MODE.emits_attachments());
        let prepared = stream::prepared(spec, MODE.emits_attachments());
        run_case("events", objects, || profile_world_build(&events, MODE));
        run_case("prepared", objects, || {
            profile_prepared_world_build(&prepared, MODE)
        });
    }
}
