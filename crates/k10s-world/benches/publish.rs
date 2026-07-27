use std::collections::VecDeque;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use k10s_clustergen::stream;
use k10s_clustergen::{ClusterSpec, GenConfig, Scenario, generate};
use k10s_core::{IngestEvent, KindId, Op, Payload, ReasonId, ResourceEvent, SceneSnapshot, State};
use k10s_world::{ExtractBench, LayoutMode, PublishBench, PublishStats, SNAPSHOT_POOL_DEPTH};

const MODE: LayoutMode = LayoutMode::Spread;
const OBJECT_COUNTS: [u32; 2] = [25_000, 50_000];
const CHANGED_PODS: [usize; 5] = [1, 16, 256, 4096, 16_384];
const WARMUP: usize = 8;
const MIN_ITERS: usize = 40;
const MAX_ITERS: usize = 20_000;
const BUDGET: Duration = Duration::from_millis(250);

const P99_MIN_ITERS: usize = 100;

fn tail(iters: usize) -> &'static str {
    if iters >= P99_MIN_ITERS { "p99" } else { "max" }
}

fn tail_value(sorted: &[u64]) -> f64 {
    if sorted.len() >= P99_MIN_ITERS {
        percentile(sorted, 0.99)
    } else {
        sorted.last().copied().unwrap_or(0) as f64
    }
}

struct Case {
    op: &'static str,
    objects: u32,
    pods: usize,
    changed: usize,
    iters: usize,
    samples: usize,
    batch_size: usize,
    p50_rmad: f64,
    p50_us: f64,
    tail_us: f64,
    stats: PublishStats,
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[i] as f64 / 1000.0
}

fn p50_relative_mad(sorted: &[u64]) -> f64 {
    let median = percentile(sorted, 0.50) * 1000.0;
    if median == 0.0 {
        return 0.0;
    }
    let mut deviations: Vec<u64> = sorted
        .iter()
        .map(|sample| sample.abs_diff(median as u64))
        .collect();
    deviations.sort_unstable();
    deviations[(deviations.len() - 1) / 2] as f64 / median
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
    let mut bench = ExtractBench::new(&stream::snapshot(&spec, MODE.emits_attachments()), MODE);
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
        samples: samples.len(),
        batch_size: 1,
        p50_rmad: p50_relative_mad(&samples),
        p50_us: percentile(&samples, 0.50),
        tail_us: tail_value(&samples),
        stats,
    }
}

fn incremental(spec: &ClusterSpec, objects: u32, changed: usize) -> Case {
    let mut bench = PublishBench::new(&stream::snapshot(&spec, MODE.emits_attachments()), MODE);
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
        samples: samples.len(),
        batch_size: 1,
        p50_rmad: p50_relative_mad(&samples),
        p50_us: percentile(&samples, 0.50),
        tail_us: tail_value(&samples),
        stats,
    }
}

fn lapped_reader(spec: &ClusterSpec, objects: u32, changed: usize) -> Case {
    let mut bench = PublishBench::new(&stream::snapshot(&spec, MODE.emits_attachments()), MODE);
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
        samples: samples.len(),
        batch_size: 1,
        p50_rmad: p50_relative_mad(&samples),
        p50_us: percentile(&samples, 0.50),
        tail_us: tail_value(&samples),
        stats,
    }
}

fn live_pod_event(events: &[IngestEvent], op: Op) -> IngestEvent {
    let owner = events
        .iter()
        .find_map(|event| match event {
            IngestEvent::Resource(resource)
                if matches!(resource.payload, Payload::Owner { .. }) =>
            {
                Some(resource)
            }
            _ => None,
        })
        .expect("the generated scene has at least one workload");
    IngestEvent::Resource(ResourceEvent {
        kind: KindId::POD,
        uid: "k10s-bench-live-pod".into(),
        namespace: owner.namespace.clone(),
        name: "bench-live-pod".into(),
        resource_version: 1,
        parent: Some(owner.uid.clone()),
        op,
        payload: Payload::Instance { state: State::OK },
    })
}

fn live_state_event(events: &[IngestEvent], state: State) -> IngestEvent {
    let mut resource = events
        .iter()
        .find_map(|event| match event {
            IngestEvent::Resource(resource)
                if matches!(resource.payload, Payload::Instance { .. }) =>
            {
                Some(resource.clone())
            }
            _ => None,
        })
        .expect("the generated scene has at least one pod");
    resource.op = Op::Modified;
    resource.payload = Payload::Instance { state };
    IngestEvent::Resource(resource)
}

fn apply_and_publish(bench: &mut PublishBench, event: &IngestEvent) -> PublishStats {
    let before = bench.stats();
    bench.apply_events(std::slice::from_ref(event));
    bench.run_publish();
    delta(before, bench.stats())
}

fn topology_update(spec: &ClusterSpec, objects: u32, timed_op: Op) -> Case {
    let events = stream::snapshot(spec, MODE.emits_attachments());
    let added = live_pod_event(&events, Op::Added);
    let deleted = live_pod_event(&events, Op::Deleted);
    let mut bench = PublishBench::new(&events, MODE);
    let (prepare, timed, restore) = match timed_op {
        Op::Added => (None, &added, Some(&deleted)),
        Op::Deleted => (Some(&added), &deleted, None),
        Op::Modified => unreachable!("the topology benchmark measures add and delete"),
    };

    for _ in 0..WARMUP {
        if let Some(event) = prepare {
            apply_and_publish(&mut bench, event);
        }
        apply_and_publish(&mut bench, timed);
        if let Some(event) = restore {
            apply_and_publish(&mut bench, event);
        }
    }

    let mut measured_stats = PublishStats::default();
    let mut samples = Vec::with_capacity(MIN_ITERS);
    let start = Instant::now();
    while budgeted(&samples, &start) {
        if let Some(event) = prepare {
            apply_and_publish(&mut bench, event);
        }
        let sample_start = Instant::now();
        let stats = apply_and_publish(&mut bench, timed);
        samples.push(sample_start.elapsed().as_nanos() as u64);
        assert_eq!(stats.publishes, 1);
        assert_eq!(stats.full_materializes, 1);
        assert_eq!(stats.deep_clones, 0);
        measured_stats.publishes += stats.publishes;
        measured_stats.full_materializes += stats.full_materializes;
        if let Some(event) = restore {
            apply_and_publish(&mut bench, event);
        }
    }
    black_box(bench.snapshot());
    samples.sort_unstable();
    Case {
        op: match timed_op {
            Op::Added => "topology add",
            Op::Deleted => "topology delete",
            Op::Modified => unreachable!(),
        },
        objects,
        pods: bench.pod_count(),
        changed: 1,
        iters: samples.len(),
        samples: samples.len(),
        batch_size: 1,
        p50_rmad: p50_relative_mad(&samples),
        p50_us: percentile(&samples, 0.50),
        tail_us: tail_value(&samples),
        stats: measured_stats,
    }
}

fn live_state_update(spec: &ClusterSpec, objects: u32) -> Case {
    let events = stream::snapshot(spec, MODE.emits_attachments());
    let states = [
        live_state_event(&events, State::of(ReasonId::NOT_READY)),
        live_state_event(&events, State::of(ReasonId::CRASH_LOOP_BACK_OFF)),
    ];
    let mut bench = PublishBench::new(&events, MODE);
    for iteration in 0..WARMUP {
        apply_and_publish(&mut bench, &states[iteration & 1]);
    }

    let before = bench.stats();
    let mut samples = Vec::with_capacity(MIN_ITERS);
    let start = Instant::now();
    while budgeted(&samples, &start) {
        let event = &states[samples.len() & 1];
        let sample_start = Instant::now();
        let stats = apply_and_publish(&mut bench, event);
        samples.push(sample_start.elapsed().as_nanos() as u64);
        assert_eq!(stats.publishes, 1);
        assert_eq!(stats.full_materializes, 0);
        assert_eq!(stats.deep_clones, 0);
    }
    let stats = delta(before, bench.stats());
    black_box(bench.snapshot());
    samples.sort_unstable();
    Case {
        op: "live state",
        objects,
        pods: bench.pod_count(),
        changed: 1,
        iters: samples.len(),
        samples: samples.len(),
        batch_size: 1,
        p50_rmad: p50_relative_mad(&samples),
        p50_us: percentile(&samples, 0.50),
        tail_us: tail_value(&samples),
        stats,
    }
}

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let mut cases = Vec::new();

    for objects in OBJECT_COUNTS {
        let spec = spec_for(objects);
        cases.push(full_materialize(&spec, objects));
        cases.push(live_state_update(&spec, objects));
        cases.push(topology_update(&spec, objects, Op::Added));
        cases.push(topology_update(&spec, objects, Op::Deleted));
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
            "    {:<17} {:>6} changed  p50 {:>9.3} us  {} {:>9.3} us | samples {:>5} x {:>2} rMAD {:>5.1}% publishes {:>5} full {:>5} deep clones {:>5}",
            c.op,
            c.changed,
            c.p50_us,
            tail(c.iters),
            c.tail_us,
            c.samples,
            c.batch_size,
            c.p50_rmad * 100.0,
            c.stats.publishes,
            c.stats.full_materializes,
            c.stats.deep_clones,
        );
    }
}

fn print_json(cases: &[Case]) {
    println!("{{");
    println!("  \"schema_version\": 3,");
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
        println!("      \"samples\": {},", c.samples);
        println!("      \"batch_size\": {},", c.batch_size);
        println!("      \"p50_rmad\": {:.6},", c.p50_rmad);
        println!("      \"p50_us\": {:.3},", c.p50_us);
        println!("      \"{}_us\": {:.3},", tail(c.iters), c.tail_us);
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
