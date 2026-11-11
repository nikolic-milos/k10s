use std::hint::black_box;
use std::time::Duration;

use k10s_atlas::testing::{SceneSpec, lod_policy, scene};
use k10s_atlas::{Camera, CullStats, LodPolicy, Scene, StageBlend, cull};
use k10s_bench::{Config, measure as measure_samples};

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
    iters: usize,
    samples: usize,
    batch_size: usize,
    p50_rmad: f64,
    p50_ns: f64,
    p99_ns: f64,
    stats: CullStats,
}

fn measure(scene: &Scene, policy: &LodPolicy, camera: &Camera) -> k10s_bench::Samples {
    let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
    measure_samples(Config::new(WARMUP, MIN_ITERS, MAX_ITERS, BUDGET), || {
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
    })
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
            let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
            let stats = cull(&s, &camera, &policy, blend, VW, VH, true, false);
            let samples = measure(&s, &policy, &camera);
            cases.push(Case {
                scene_name: name.clone(),
                objects: spec.total_objects(),
                regions: spec.regions,
                blocks_per_region: spec.blocks_per_region,
                camera_name,
                zoom: camera.zoom,
                iters: samples.iterations(),
                samples: samples.sample_count(),
                batch_size: samples.batch_size(),
                p50_rmad: samples.p50_relative_mad(),
                p50_ns: samples.percentile(0.50),
                p99_ns: samples.percentile(0.99),
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
            "    {:<12} zoom {:>6.2}  p50 {:>9.1} ns  p99 {:>9.1} ns  samples {:>6} x {:>5}  rMAD {:>5.1}% | quads {:>6} labels {:>4} icons {:>4} sats {:>5} curves {:>5} edges {:>5} | drawn r/b/c {:>5}/{:>6}/{:>7}",
            c.camera_name,
            c.zoom,
            c.p50_ns,
            c.p99_ns,
            c.samples,
            c.batch_size,
            c.p50_rmad * 100.0,
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
    println!("  \"schema_version\": 3,");
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
        println!("      \"iters\": {},", c.iters);
        println!("      \"samples\": {},", c.samples);
        println!("      \"batch_size\": {},", c.batch_size);
        println!("      \"p50_rmad\": {:.6},", c.p50_rmad);
        println!("      \"p50_ns\": {:.3},", c.p50_ns);
        println!("      \"p99_ns\": {:.3},", c.p99_ns);
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
