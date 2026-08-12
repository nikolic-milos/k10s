use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use k10s_atlas::testing::lod_policy;
use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend, cull};
use k10s_bench::{Config, Samples, measure as measure_samples};
use k10s_clustergen::stream;
use k10s_clustergen::{GenConfig, Scenario, generate};
use k10s_core::{NsNode, PodNode, SatNode, SceneSnapshot, WorkloadNode};
use k10s_world::{ExtractBench, LayoutMode};

const MODE: LayoutMode = LayoutMode::Spread;
const SEED: u64 = 55;
const VW: f32 = 1600.0;
const VH: f32 = 1000.0;
const WARMUP: usize = 100;
const MIN_ITERS: usize = 100;
const MAX_ITERS: usize = 100_000;
const BUDGET: Duration = Duration::from_millis(120);

const TARGETS: [u32; 4] = [4_000, 12_000, 25_000, 50_000];
const SCENARIOS: [Scenario; 3] = [Scenario::Platform, Scenario::NsFanOut, Scenario::WlFanOut];

const BLOCK_ZOOM: f32 = 2.2;
const CELL_ZOOM: f32 = 30.0;

struct Shape {
    scenario: &'static str,
    objects: u32,
    regions: usize,
    blocks: usize,
    cells: usize,
    sats: usize,
    edges: usize,
    cross_edges: usize,
    ns_degree: usize,
    wl_degree: usize,
}

struct Case {
    scenario: &'static str,
    objects: u32,
    ns_degree: usize,
    wl_degree: usize,
    camera_name: &'static str,
    zoom: f32,
    iters: usize,
    samples: usize,
    batch_size: usize,
    p50_rmad: f64,
    p50_ns: f64,
    p99_ns: f64,
    p50_flat_spatial_ns: f64,
    p50_flat_edges_ns: f64,
    p50_no_edges_ns: f64,
    indexed_region_nodes: Option<usize>,
    indexed_region_candidates: Option<usize>,
    indexed_cell_nodes: Option<usize>,
    indexed_cell_candidates: Option<usize>,
    stats: CullStats,
}

fn samples(snap: &SceneSnapshot, policy: &LodPolicy, camera: &Camera, edges_on: bool) -> Samples {
    let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
    measure_samples(Config::new(WARMUP, MIN_ITERS, MAX_ITERS, BUDGET), || {
        black_box(cull(
            black_box(snap),
            black_box(camera),
            policy,
            blend,
            VW,
            VH,
            edges_on,
            false,
        ));
    })
}

fn widest_region(snap: &SceneSnapshot) -> usize {
    (0..snap.regions.len())
        .max_by_key(|&i| snap.regions[i].children.len())
        .unwrap_or(0)
}

fn deepest_block(snap: &SceneSnapshot) -> usize {
    (0..snap.blocks.len())
        .max_by_key(|&i| snap.blocks[i].children.len())
        .unwrap_or(0)
}

fn cameras(snap: &SceneSnapshot) -> Vec<(&'static str, Camera)> {
    let region_index = widest_region(snap);
    let block_index = deepest_block(snap);
    let region = &snap.regions[region_index];
    let block = &snap.blocks[block_index];

    let mut fit_all = Camera::default();
    fit_all.fit(snap.bounds, VW, VH);
    let mut fit_region = Camera::default();
    fit_region.fit(region.rect, VW, VH);
    let mut fit_block = Camera::default();
    fit_block.fit(block.inner, VW, VH);

    let first_region_block = snap
        .region_block_indices(region_index)
        .next()
        .expect("the widest namespace has a workload");
    let (bx, by) = snap.blocks[first_region_block].inner.center();
    let mut block_cells = snap.block_cell_indices(block_index);
    let cell = block_cells.next().expect("the deepest workload has a pod");
    let cell_cam = if block.children.len() >= 2 {
        let next_cell = block_cells
            .next()
            .expect("a workload with at least two pods exposes both slots");
        let pitch = snap.cells[next_cell].rect.x - snap.cells[cell].rect.x;
        let (cx, cy) = snap.cells[cell].rect.center();
        Camera {
            cx: cx - pitch * 0.5 + VW / (2.0 * CELL_ZOOM),
            cy: cy - pitch * 0.5 + VH / (2.0 * CELL_ZOOM),
            zoom: CELL_ZOOM,
        }
    } else {
        let (cx, cy) = block.inner.center();
        Camera {
            cx,
            cy,
            zoom: CELL_ZOOM,
        }
    };

    let (rx, ry) = region.rect.center();

    vec![
        ("Z0 fit all", fit_all),
        (
            "Z1 widest ns",
            Camera {
                cx: rx,
                cy: ry,
                zoom: 0.10,
            },
        ),
        (
            "Z2 wide ns",
            Camera {
                cx: rx,
                cy: ry,
                zoom: 0.30,
            },
        ),
        (
            "Z2 widest ns",
            Camera {
                cx: bx,
                cy: by,
                zoom: BLOCK_ZOOM,
            },
        ),
        ("Z3 deepest wl", cell_cam),
        ("fit widest ns", fit_region),
        ("fit deepest wl", fit_block),
    ]
}

fn snapshot_for(scenario: Scenario, objects: u32) -> Arc<SceneSnapshot> {
    let spec = generate(&GenConfig {
        seed: SEED,
        target_objects: objects,
        scenario,
    });
    let mut bench = ExtractBench::new(&stream::snapshot(&spec, MODE.emits_attachments()), MODE);
    bench.run_extract();
    let snap = bench.snapshot();
    drop(bench);
    snap
}

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let policy = lod_policy();
    let mut shapes = Vec::new();
    let mut cases = Vec::new();

    for scenario in SCENARIOS {
        for objects in TARGETS {
            let snap = snapshot_for(scenario, objects);
            let ns_degree = snap.regions[widest_region(&snap)].children.len();
            let wl_degree = snap.blocks[deepest_block(&snap)].children.len();
            shapes.push(Shape {
                scenario: scenario.as_str(),
                objects,
                regions: snap.regions.len(),
                blocks: snap.blocks.len(),
                cells: snap.cells.len(),
                sats: snap.sats.len(),
                edges: snap.edges.len(),
                cross_edges: snap.cross_edges.len(),
                ns_degree,
                wl_degree,
            });

            for (camera_name, camera) in cameras(&snap) {
                let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
                let stats = cull(&snap, &camera, &policy, blend, VW, VH, true, false);
                let visible = camera.visible_world(VW, VH);
                let indexed_region_stats = snap.indexed_region_candidate_stats(&visible);
                let indexed_cell_stats = (stats.stage >= 2)
                    .then(|| {
                        snap.indexed_block_cell_candidate_stats(deepest_block(&snap), &visible)
                    })
                    .flatten();
                if scenario == Scenario::WlFanOut && camera_name == "Z3 deepest wl" {
                    assert_eq!(
                        stats.aggregated_blocks, 0,
                        "a deeply zoomed workload must retain individual visible pods"
                    );
                }
                if scenario == Scenario::WlFanOut
                    && camera_name == "fit deepest wl"
                    && wl_degree > policy.max_cells_per_block
                {
                    assert!(stats.aggregated_blocks > 0);
                    assert!(stats.aggregated_cells >= wl_degree);
                    assert!(stats.drawn_cells <= policy.max_cells_per_block);
                }
                let on = samples(&snap, &policy, &camera, true);
                let off = samples(&snap, &policy, &camera, false);
                cases.push(Case {
                    scenario: scenario.as_str(),
                    objects,
                    ns_degree,
                    wl_degree,
                    camera_name,
                    zoom: camera.zoom,
                    iters: on.iterations(),
                    samples: on.sample_count(),
                    batch_size: on.batch_size(),
                    p50_rmad: on.p50_relative_mad(),
                    p50_ns: on.percentile(0.50),
                    p99_ns: on.percentile(0.99),
                    p50_flat_spatial_ns: 0.0,
                    p50_flat_edges_ns: 0.0,
                    p50_no_edges_ns: off.percentile(0.50),
                    indexed_region_nodes: indexed_region_stats.map(|stats| stats.0),
                    indexed_region_candidates: indexed_region_stats.map(|stats| stats.1),
                    indexed_cell_nodes: indexed_cell_stats.map(|stats| stats.0),
                    indexed_cell_candidates: indexed_cell_stats.map(|stats| stats.1),
                    stats,
                });
            }
        }
    }

    let mut case_index = 0usize;
    for scenario in SCENARIOS {
        for objects in TARGETS {
            let mut snap = snapshot_for(scenario, objects);
            let flat =
                Arc::get_mut(&mut snap).expect("the benchmark owns its only snapshot handle");
            flat.region_edge_indexes.clear();
            flat.cross_edge_index = Default::default();
            for (_, camera) in cameras(&snap) {
                let flat_edges = samples(&snap, &policy, &camera, true);
                cases[case_index].p50_flat_edges_ns = flat_edges.percentile(0.50);
                case_index += 1;
            }
        }
    }
    assert_eq!(case_index, cases.len());

    case_index = 0;
    for scenario in SCENARIOS {
        for objects in TARGETS {
            let mut snap = snapshot_for(scenario, objects);
            Arc::get_mut(&mut snap)
                .expect("the benchmark owns its only snapshot handle")
                .spatial_index = Default::default();
            for (_, camera) in cameras(&snap) {
                let flat_spatial = samples(&snap, &policy, &camera, true);
                cases[case_index].p50_flat_spatial_ns = flat_spatial.percentile(0.50);
                case_index += 1;
            }
        }
    }
    assert_eq!(case_index, cases.len());

    if json {
        print_json(&shapes, &cases);
    } else {
        print_table(&shapes, &cases);
    }
}

fn print_table(shapes: &[Shape], cases: &[Case]) {
    println!(
        "k10s-world fan-out cull bench - {} layout, seed {SEED}, logical viewport {VW:.0}x{VH:.0}, no GPU",
        MODE.as_str()
    );
    println!(
        "  real k10s-core node strides: {} / {} / {} / {} B for ns / workload / pod / sat",
        size_of::<NsNode>(),
        size_of::<WorkloadNode>(),
        size_of::<PodNode>(),
        size_of::<SatNode>(),
    );
    println!(
        "  ns degree = workloads under the widest namespace, wl degree = pods under the deepest workload"
    );

    println!("\n  generated shapes");
    for s in shapes {
        println!(
            "    {:<10} {:>6} objects  ns {:>4} wl {:>6} pods {:>6} sats {:>6} edges {:>6} cross {:>5} | ns degree {:>6} wl degree {:>6}",
            s.scenario,
            s.objects,
            s.regions,
            s.blocks,
            s.cells,
            s.sats,
            s.edges,
            s.cross_edges,
            s.ns_degree,
            s.wl_degree,
        );
    }

    let mut current = ("", 0u32);
    for c in cases {
        if (c.scenario, c.objects) != current {
            current = (c.scenario, c.objects);
            println!(
                "\n  {} at {} objects - ns degree {}, wl degree {}",
                c.scenario, c.objects, c.ns_degree, c.wl_degree
            );
        }
        println!(
            "    {:<15} zoom {:>7.3}  p50 {:>10.1} ns  p99 {:>10.1} ns  flat-spatial p50 {:>10.1} ns  flat-edges p50 {:>10.1} ns  no-edges p50 {:>10.1} ns  samples {:>6} x {:>5}  rMAD {:>5.1}% | quads {:>6} labels {:>4} icons {:>5} sats {:>5} curves {:>5} edges {:>5} | drawn r/b/c {:>4}/{:>6}/{:>6} aggregate b/c {:>2}/{:>6}",
            c.camera_name,
            c.zoom,
            c.p50_ns,
            c.p99_ns,
            c.p50_flat_spatial_ns,
            c.p50_flat_edges_ns,
            c.p50_no_edges_ns,
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
            c.stats.aggregated_blocks,
            c.stats.aggregated_cells,
        );
    }
}

fn print_json(shapes: &[Shape], cases: &[Case]) {
    println!("{{");
    println!("  \"schema_version\": 5,");
    println!("  \"mode\": \"{}\",", MODE.as_str());
    println!("  \"seed\": {SEED},");
    println!("  \"viewport\": [{VW}, {VH}],");
    println!("  \"shapes\": [");
    for (i, s) in shapes.iter().enumerate() {
        let comma = if i + 1 == shapes.len() { "" } else { "," };
        println!(
            "    {{ \"scenario\": \"{}\", \"objects\": {}, \"regions\": {}, \"blocks\": {}, \"cells\": {}, \"sats\": {}, \"edges\": {}, \"cross_edges\": {}, \"ns_degree\": {}, \"wl_degree\": {} }}{comma}",
            s.scenario,
            s.objects,
            s.regions,
            s.blocks,
            s.cells,
            s.sats,
            s.edges,
            s.cross_edges,
            s.ns_degree,
            s.wl_degree,
        );
    }
    println!("  ],");
    println!("  \"cases\": [");
    for (i, c) in cases.iter().enumerate() {
        let comma = if i + 1 == cases.len() { "" } else { "," };
        println!("    {{");
        println!("      \"scenario\": \"{}\",", c.scenario);
        println!("      \"objects\": {},", c.objects);
        println!("      \"ns_degree\": {},", c.ns_degree);
        println!("      \"wl_degree\": {},", c.wl_degree);
        println!("      \"camera\": \"{}\",", c.camera_name);
        println!("      \"zoom\": {},", c.zoom);
        println!("      \"iters\": {},", c.iters);
        println!("      \"samples\": {},", c.samples);
        println!("      \"batch_size\": {},", c.batch_size);
        println!("      \"p50_rmad\": {:.6},", c.p50_rmad);
        println!("      \"p50_ns\": {:.3},", c.p50_ns);
        println!("      \"p99_ns\": {:.3},", c.p99_ns);
        println!(
            "      \"p50_flat_spatial_ns\": {:.3},",
            c.p50_flat_spatial_ns
        );
        println!("      \"p50_flat_edges_ns\": {:.3},", c.p50_flat_edges_ns);
        println!("      \"p50_no_edges_ns\": {:.3},", c.p50_no_edges_ns);
        match c.indexed_region_nodes {
            Some(nodes) => println!("      \"indexed_region_nodes\": {nodes},"),
            None => println!("      \"indexed_region_nodes\": null,"),
        }
        match c.indexed_region_candidates {
            Some(candidates) => {
                println!("      \"indexed_region_candidates\": {candidates},")
            }
            None => println!("      \"indexed_region_candidates\": null,"),
        }
        match c.indexed_cell_nodes {
            Some(nodes) => println!("      \"indexed_cell_nodes\": {nodes},"),
            None => println!("      \"indexed_cell_nodes\": null,"),
        }
        match c.indexed_cell_candidates {
            Some(candidates) => {
                println!("      \"indexed_cell_candidates\": {candidates},")
            }
            None => println!("      \"indexed_cell_candidates\": null,"),
        }
        println!("      \"quads\": {},", c.stats.quads);
        println!("      \"labels\": {},", c.stats.labels);
        println!("      \"icons\": {},", c.stats.icons);
        println!("      \"sats\": {},", c.stats.drawn_sats);
        println!("      \"curves\": {},", c.stats.curves);
        println!("      \"edges\": {},", c.stats.edges);
        println!("      \"drawn_regions\": {},", c.stats.drawn_regions);
        println!("      \"drawn_blocks\": {},", c.stats.drawn_blocks);
        println!("      \"drawn_cells\": {},", c.stats.drawn_cells);
        println!(
            "      \"aggregated_blocks\": {},",
            c.stats.aggregated_blocks
        );
        println!("      \"aggregated_cells\": {}", c.stats.aggregated_cells);
        println!("    }}{comma}");
    }
    println!("  ]");
    println!("}}");
}
