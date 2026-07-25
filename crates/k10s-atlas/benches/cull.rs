use std::hint::black_box;
use std::time::{Duration, Instant};

use k10s_atlas::testing::{SceneSpec, lod_policy, scene};
use k10s_atlas::{Camera, CullStats, LodPolicy, Scene, StageBlend, cull};

const VW: f32 = 1600.0;
const VH: f32 = 1000.0;
const WARMUP: usize = 200;
const MIN_ITERS: usize = 200;
const MAX_ITERS: usize = 200_000;
const BUDGET: Duration = Duration::from_millis(120);

struct Case {
    scene_name: String,
    objects: usize,
    regions: usize,
    blocks_per_region: usize,
    camera_name: &'static str,
    zoom: f32,
    p50_ns: f64,
    p99_ns: f64,
    stats: CullStats,
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[i] as f64
}

fn measure(scene: &Scene, policy: &LodPolicy, camera: &Camera) -> (f64, f64, CullStats) {
    let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
    let stats = cull(scene, camera, policy, blend, VW, VH, true, false);

    for _ in 0..WARMUP {
        black_box(cull(
            black_box(scene),
            black_box(camera),
            policy,
            blend,
            VW,
            VH,
            true,
            false,
        ));
    }

    let mut samples = Vec::with_capacity(MIN_ITERS);
    let start = Instant::now();
    while samples.len() < MAX_ITERS && (samples.len() < MIN_ITERS || start.elapsed() < BUDGET) {
        let t = Instant::now();
        black_box(cull(
            black_box(scene),
            black_box(camera),
            policy,
            blend,
            VW,
            VH,
            true,
            false,
        ));
        samples.push(t.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    (
        percentile(&samples, 0.50),
        percentile(&samples, 0.99),
        stats,
    )
}

fn cameras(scene: &Scene) -> Vec<(&'static str, Camera)> {
    let mut fit = Camera::default();
    fit.fit(scene.bounds, VW, VH);

    let (rx, ry) = scene.regions[0].rect.center();
    let (bx, by) = scene.blocks[0].inner.center();

    vec![
        ("Z0 fit", fit),
        (
            "Z1 region",
            Camera {
                cx: rx,
                cy: ry,
                zoom: 0.12,
            },
        ),
        (
            "Z2 hub",
            Camera {
                cx: bx,
                cy: by,
                zoom: 2.2,
            },
        ),
        (
            "Z3 pod",
            Camera {
                cx: bx,
                cy: by,
                zoom: 4.5,
            },
        ),
        (
            "Z4 extreme",
            Camera {
                cx: bx,
                cy: by,
                zoom: 24.0,
            },
        ),
    ]
}

fn specs() -> Vec<(String, SceneSpec)> {
    let mut out = Vec::new();
    for regions in [200usize, 400, 800, 1600] {
        let spec = SceneSpec::uniform(regions, 15);
        out.push((format!("uniform r{regions} b15"), spec));
    }
    for blocks in [500usize, 2000, 8000] {
        let spec = SceneSpec::fan_out(blocks);
        out.push((format!("fanout r1 b{blocks}"), spec));
    }
    out
}

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let policy = lod_policy();
    let mut cases = Vec::new();

    for (name, spec) in specs() {
        let s = scene(spec);
        for (camera_name, camera) in cameras(&s) {
            let (p50, p99, stats) = measure(&s, &policy, &camera);
            cases.push(Case {
                scene_name: name.clone(),
                objects: spec.total_objects(),
                regions: spec.regions,
                blocks_per_region: spec.blocks_per_region,
                camera_name,
                zoom: camera.zoom,
                p50_ns: p50,
                p99_ns: p99,
                stats,
            });
        }
    }

    if json {
        print_json(&cases);
    } else {
        print_table(&cases);
    }
}

fn print_table(cases: &[Case]) {
    println!("k10s headless cull bench - logical viewport {VW:.0}x{VH:.0}, no GPU");
    let mut current = String::new();
    for c in cases {
        if c.scene_name != current {
            current = c.scene_name.clone();
            println!(
                "\n  {} - {} objects ({} regions x {} blocks)",
                c.scene_name, c.objects, c.regions, c.blocks_per_region
            );
        }
        println!(
            "    {:<12} zoom {:>6.2}  p50 {:>9.0} ns  p99 {:>9.0} ns | quads {:>6} labels {:>4} icons {:>4} sats {:>5} curves {:>5} edges {:>5} | drawn r/b/c {:>5}/{:>6}/{:>7}",
            c.camera_name,
            c.zoom,
            c.p50_ns,
            c.p99_ns,
            c.stats.quads,
            c.stats.labels,
            c.stats.icons,
            c.stats.drawn_sats,
            c.stats.curves,
            c.stats.edges,
            c.stats.drawn_regions,
            c.stats.drawn_blocks,
            c.stats.drawn_cells,
        );
    }
}

fn print_json(cases: &[Case]) {
    println!("{{");
    println!("  \"schema_version\": 1,");
    println!("  \"viewport\": [{VW}, {VH}],");
    println!("  \"cases\": [");
    for (i, c) in cases.iter().enumerate() {
        let comma = if i + 1 == cases.len() { "" } else { "," };
        println!("    {{");
        println!("      \"scene\": \"{}\",", c.scene_name);
        println!("      \"objects\": {},", c.objects);
        println!("      \"regions\": {},", c.regions);
        println!("      \"blocks_per_region\": {},", c.blocks_per_region);
        println!("      \"camera\": \"{}\",", c.camera_name);
        println!("      \"zoom\": {},", c.zoom);
        println!("      \"p50_ns\": {:.0},", c.p50_ns);
        println!("      \"p99_ns\": {:.0},", c.p99_ns);
        println!("      \"quads\": {},", c.stats.quads);
        println!("      \"labels\": {},", c.stats.labels);
        println!("      \"icons\": {},", c.stats.icons);
        println!("      \"sats\": {},", c.stats.drawn_sats);
        println!("      \"curves\": {},", c.stats.curves);
        println!("      \"edges\": {},", c.stats.edges);
        println!("      \"drawn_regions\": {},", c.stats.drawn_regions);
        println!("      \"drawn_blocks\": {},", c.stats.drawn_blocks);
        println!("      \"drawn_cells\": {}", c.stats.drawn_cells);
        println!("    }}{comma}");
    }
    println!("  ]");
    println!("}}");
}
