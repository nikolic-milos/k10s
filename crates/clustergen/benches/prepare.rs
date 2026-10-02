use std::hint::black_box;
use std::time::{Duration, Instant};

use k10s_clustergen::stream;
use k10s_clustergen::{GenConfig, Scenario, generate};

const OBJECT_COUNTS: [u32; 2] = [25_000, 1_000_000];
const WARMUP: usize = 1;
const SAMPLES: usize = 5;

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn sample(objects: u32) -> (Duration, Duration, Duration) {
    let total = Instant::now();
    let phase = Instant::now();
    let spec = generate(&GenConfig {
        seed: 55,
        target_objects: objects,
        scenario: Scenario::Platform,
    });
    let generate = phase.elapsed();

    let phase = Instant::now();
    let prepared = stream::prepared(spec, true);
    let prepare = phase.elapsed();
    let elapsed = total.elapsed();
    black_box(prepared);
    (generate, prepare, elapsed)
}

fn main() {
    println!("k10s-clustergen prepared-scene construction - {SAMPLES} samples");
    println!("objects     generate   prepare     total");
    for objects in OBJECT_COUNTS {
        for _ in 0..WARMUP {
            black_box(sample(objects));
        }
        let mut generate = Vec::with_capacity(SAMPLES);
        let mut prepare = Vec::with_capacity(SAMPLES);
        let mut total = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let (generated, prepared, elapsed) = sample(objects);
            generate.push(generated);
            prepare.push(prepared);
            total.push(elapsed);
        }
        println!(
            "{objects:>7}  {:>9.2} {:>9.2} {:>9.2}",
            milliseconds(median(&mut generate)),
            milliseconds(median(&mut prepare)),
            milliseconds(median(&mut total)),
        );
    }
}
