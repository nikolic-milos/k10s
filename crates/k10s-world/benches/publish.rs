use std::collections::VecDeque;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use k10s_clustergen::{ClusterSpec, GenConfig, Scenario, generate};
use k10s_core::SceneSnapshot;
use k10s_world::{ExtractBench, LayoutMode, PublishBench, PublishStats, SNAPSHOT_POOL_DEPTH};

const MODE: LayoutMode = LayoutMode::Spread;
const OBJECT_COUNTS: [u32; 2] = [25_000, 50_000];
const CHANGED_PODS: [usize; 5] = [1, 16, 256, 4096, 16_384];
const WARMUP: usize = 8;
const MIN_ITERS: usize = 40;
const MAX_ITERS: usize = 20_000;
const BUDGET: Duration = Duration::from_millis(250);

struct Case {
    op: &'static str,
    objects: u32,
    pods: usize,
    changed: usize,
    iters: usize,
    p50_us: f64,
    p99_us: f64,
    stats: PublishStats,
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[i] as f64 / 1000.0
}

fn delta(before: PublishStats, after: PublishStats) -> PublishStats {
    PublishStats {
        publishes: after.publishes - before.publishes,
        full_materializes: after.full_materializes - before.full_materializes,
        deep_clones: after.deep_clones - before.deep_clones,
    }
}

fn spec_for(objects: u32) -> ClusterSpec {
    generate(&GenConfig {
        seed: 42,
        target_objects: objects,
        scenario: Scenario::Platform,
    })
}

fn budgeted(samples: &[u64], start: &Instant) -> bool {
    samples.len() < MAX_ITERS && (samples.len() < MIN_ITERS || start.elapsed() < BUDGET)
}

fn full_materialize(spec: &ClusterSpec, objects: u32) -> Case {
    let mut bench = ExtractBench::new(spec.clone(), MODE);
    for _ in 0..WARMUP {
        bench.run_extract();
    }
    let before = bench.stats();
    let mut samples = Vec::with_capacity(MIN_ITERS);
    let start = Instant::now();
    while budgeted(&samples, &start) {
        let t = Instant::now();
        bench.run_extract();
        samples.push(t.elapsed().as_nanos() as u64);
    }
    let stats = delta(before, bench.stats());
    let snap = bench.snapshot();
    let pods = snap.cells.len();
    black_box(snap);
    samples.sort_unstable();
    Case {
        op: "full materialize",
        objects,
        pods,
        changed: pods,
        iters: samples.len(),
        p50_us: percentile(&samples, 0.50),
        p99_us: percentile(&samples, 0.99),
        stats,
    }
}

fn incremental(spec: &ClusterSpec, objects: u32, changed: usize) -> Case {
    let mut bench = PublishBench::new(spec.clone(), MODE);
    for _ in 0..WARMUP {
        bench.flip_pods(changed);
        bench.run_publish();
    }
    let before = bench.stats();
    let mut samples = Vec::with_capacity(MIN_ITERS);
    let start = Instant::now();
    while budgeted(&samples, &start) {
        bench.flip_pods(changed);
        let t = Instant::now();
        bench.run_publish();
        samples.push(t.elapsed().as_nanos() as u64);
    }
    let stats = delta(before, bench.stats());
    black_box(bench.snapshot());
    samples.sort_unstable();
    Case {
        op: "incremental",
        objects,
        pods: bench.pod_count(),
        changed: changed.min(bench.pod_count()),
        iters: samples.len(),
        p50_us: percentile(&samples, 0.50),
        p99_us: percentile(&samples, 0.99),
        stats,
    }
}

fn lapped_reader(spec: &ClusterSpec, objects: u32, changed: usize) -> Case {
    let mut bench = PublishBench::new(spec.clone(), MODE);
    let mut recent: VecDeque<Arc<SceneSnapshot>> = VecDeque::with_capacity(SNAPSHOT_POOL_DEPTH);
    let lap = |bench: &mut PublishBench, recent: &mut VecDeque<Arc<SceneSnapshot>>| {
        recent.push_back(bench.snapshot());
        if recent.len() > SNAPSHOT_POOL_DEPTH {
            recent.pop_front();
        }
    };
    for _ in 0..WARMUP {
        bench.flip_pods(changed);
        bench.run_publish();
        lap(&mut bench, &mut recent);
    }
    let before = bench.stats();
    let mut samples = Vec::with_capacity(MIN_ITERS);
    let start = Instant::now();
    while budgeted(&samples, &start) {
        bench.flip_pods(changed);
        let t = Instant::now();
        bench.run_publish();
        samples.push(t.elapsed().as_nanos() as u64);
        lap(&mut bench, &mut recent);
    }
    let stats = delta(before, bench.stats());
    black_box(recent);
    samples.sort_unstable();
    Case {
        op: "lapped reader",
        objects,
        pods: bench.pod_count(),
        changed: changed.min(bench.pod_count()),
        iters: samples.len(),
        p50_us: percentile(&samples, 0.50),
        p99_us: percentile(&samples, 0.99),
        stats,
    }
}

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let mut cases = Vec::new();

    for objects in OBJECT_COUNTS {
        let spec = spec_for(objects);
        cases.push(full_materialize(&spec, objects));
        for changed in CHANGED_PODS {
            cases.push(incremental(&spec, objects, changed));
        }
        cases.push(lapped_reader(&spec, objects, 1));
        cases.push(lapped_reader(&spec, objects, *CHANGED_PODS.last().unwrap()));
    }

    if json {
        print_json(&cases);
    } else {
        print_table(&cases);
    }
}

fn print_table(cases: &[Case]) {
    println!(
        "k10s-world publish bench - {} layout, snapshot pool depth {SNAPSHOT_POOL_DEPTH}, no GPU",
        MODE.as_str()
    );
    let mut current = 0u32;
    for c in cases {
        if c.objects != current {
            current = c.objects;
            println!("\n  {} objects, {} pods", c.objects, c.pods);
        }
        println!(
            "    {:<17} {:>6} changed  p50 {:>9.3} us  p99 {:>9.3} us | iters {:>5} publishes {:>5} full {:>5} deep clones {:>5}",
            c.op,
            c.changed,
            c.p50_us,
            c.p99_us,
            c.iters,
            c.stats.publishes,
            c.stats.full_materializes,
            c.stats.deep_clones,
        );
    }
}

fn print_json(cases: &[Case]) {
    println!("{{");
    println!("  \"schema_version\": 1,");
    println!("  \"mode\": \"{}\",", MODE.as_str());
    println!("  \"snapshot_pool_depth\": {SNAPSHOT_POOL_DEPTH},");
    println!("  \"cases\": [");
    for (i, c) in cases.iter().enumerate() {
        let comma = if i + 1 == cases.len() { "" } else { "," };
        println!("    {{");
        println!("      \"op\": \"{}\",", c.op);
        println!("      \"objects\": {},", c.objects);
        println!("      \"pods\": {},", c.pods);
        println!("      \"changed\": {},", c.changed);
        println!("      \"iters\": {},", c.iters);
        println!("      \"p50_us\": {:.3},", c.p50_us);
        println!("      \"p99_us\": {:.3},", c.p99_us);
        println!("      \"publishes\": {},", c.stats.publishes);
        println!(
            "      \"full_materializes\": {},",
            c.stats.full_materializes
        );
        println!("      \"deep_clones\": {}", c.stats.deep_clones);
        println!("    }}{comma}");
    }
    println!("  ]");
    println!("}}");
}
