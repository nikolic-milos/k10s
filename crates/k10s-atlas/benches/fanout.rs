use std::hint::black_box;
use std::time::Duration;

use k10s_atlas::testing::{SceneSpec, cross_scene, lod_policy, scene};
use k10s_atlas::{
    BlockNode, Camera, CellNode, CullStats, Edge, LodPolicy, RegionNode, Scene, StageBlend, cull,
};
use k10s_bench::{Config, Samples, measure as measure_samples};

const VW: f32 = 1600.0;
const VH: f32 = 1000.0;
const WARMUP: usize = 200;
const MIN_ITERS: usize = 200;
const MAX_ITERS: usize = 200_000;
const BUDGET: Duration = Duration::from_millis(150);

const DEGREES: [usize; 10] = [16, 32, 64, 128, 256, 512, 1024, 2048, 5000, 10_000];
const EDGE_DEGREES: [usize; 8] = [0, 16, 64, 256, 1024, 3000, 6000, 12_000];

const BLOCK_ZOOM: f32 = 2.2;
const CELL_ZOOM: f32 = 30.0;
const CELL_SWEEP_BLOCKS: usize = 4;
const EDGE_SWEEP_BLOCKS: usize = 512;
const CROSS_SWEEP_REGIONS: usize = 2;
const CROSS_SWEEP_BLOCKS: usize = 256;
const CELLS_PER_BLOCK: usize = 5;
const SATS_PER_BLOCK: usize = 2;

const UNIT: &str = "unit";
const PADDED: &str = "padded";
const BOTH_EXTS: &[&str] = &[UNIT, PADDED];
const PADDED_ONLY: &[&str] = &[PADDED];

#[derive(Debug, Clone, Copy)]
struct Pad<const N: usize>([u8; N]);

impl<const N: usize> Default for Pad<N> {
    fn default() -> Self {
        Pad([0u8; N])
    }
}

type NsPad = Pad<8>;
type WlPad = Pad<12>;
type PodPad = Pad<8>;
type SatPad = Pad<24>;
type PaddedScene = Scene<NsPad, WlPad, PodPad, SatPad>;

struct Case {
    sweep: &'static str,
    child: &'static str,
    degree: usize,
    ext: &'static str,
    child_bytes: usize,
    child_kib: f64,
    zoom: f32,
    iters: usize,
    samples: usize,
    batch_size: usize,
    p50_rmad: f64,
    p50_ns: f64,
    p99_ns: f64,
    p50_flat_edges_ns: Option<f64>,
    ns_per_child: f64,
    stats: CullStats,
    drawn_constant: bool,
}

fn measure<R, B, C, S>(
    scene: &Scene<R, B, C, S>,
    policy: &LodPolicy,
    camera: &Camera,
) -> (Samples, CullStats) {
    let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
    let stats = cull(scene, camera, policy, blend, VW, VH, true, false);
    let samples = measure_samples(Config::new(WARMUP, MIN_ITERS, MAX_ITERS, BUDGET), || {
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
    });
    (samples, stats)
}

fn measure_with_flat_control<R: Clone, B: Clone, C: Clone, S: Clone>(
    scene: &Scene<R, B, C, S>,
    policy: &LodPolicy,
    camera: &Camera,
    compare_flat: bool,
) -> (Samples, CullStats, Option<f64>) {
    let (samples, stats) = measure(scene, policy, camera);
    let flat_p50 = compare_flat.then(|| {
        let mut flat = scene.clone();
        flat.region_edge_indexes.clear();
        flat.cross_edge_index = Default::default();
        let (flat_samples, flat_stats) = measure(&flat, policy, camera);
        assert_eq!(drawn_work(&stats), drawn_work(&flat_stats));
        flat_samples.percentile(0.50)
    });
    (samples, stats, flat_p50)
}

fn pad_ext(s: Scene) -> PaddedScene {
    PaddedScene {
        rev: s.rev,
        bounds: s.bounds,
        regions: s
            .regions
            .into_iter()
            .map(|n| RegionNode {
                rect: n.rect,
                label: n.label,
                weight: n.weight,
                children: n.children,
                ext: NsPad::default(),
            })
            .collect(),
        blocks: s
            .blocks
            .into_iter()
            .map(|n| BlockNode {
                rect: n.rect,
                inner: n.inner,
                label: n.label,
                children: n.children,
                sats: n.sats,
                ext: WlPad::default(),
            })
            .collect(),
        cells: s
            .cells
            .into_iter()
            .map(|n| CellNode {
                rect: n.rect,
                label: n.label,
                ext: PodPad::default(),
            })
            .collect(),
        sats: s
            .sats
            .into_iter()
            .map(|n| CellNode {
                rect: n.rect,
                label: n.label,
                ext: SatPad::default(),
            })
            .collect(),
        region_blocks: s.region_blocks,
        block_cells: s.block_cells,
        block_sats: s.block_sats,
        spatial_index: s.spatial_index,
        edges: s.edges,
        edge_segments: s.edge_segments,
        region_edges: s.region_edges,
        region_edge_indexes: s.region_edge_indexes,
        cross_edges: s.cross_edges,
        cross_edge_index: s.cross_edge_index,
        totals: s.totals,
    }
}

fn block_camera<R, B, C, S>(s: &Scene<R, B, C, S>, zoom: f32) -> Camera {
    let (cx, cy) = s.blocks[0].inner.center();
    Camera { cx, cy, zoom }
}

fn cell_camera<R, B, C, S>(s: &Scene<R, B, C, S>, zoom: f32) -> Camera {
    let pitch = s.cells[1].rect.x - s.cells[0].rect.x;
    let (cx, cy) = s.cells[0].rect.center();
    Camera {
        cx: cx - pitch * 0.5 + VW / (2.0 * zoom),
        cy: cy - pitch * 0.5 + VH / (2.0 * zoom),
        zoom,
    }
}

fn fit_region_camera<R, B, C, S>(s: &Scene<R, B, C, S>, _zoom: f32) -> Camera {
    let mut cam = Camera::default();
    cam.fit(s.regions[0].rect, VW, VH);
    cam
}

fn fit_block_camera<R, B, C, S>(s: &Scene<R, B, C, S>, _zoom: f32) -> Camera {
    let mut cam = Camera::default();
    cam.fit(s.blocks[0].inner, VW, VH);
    cam
}

fn far_block_camera<R, B, C, S>(s: &Scene<R, B, C, S>, zoom: f32) -> Camera {
    let last = s.regions[0].children.end as usize - 1;
    let (cx, cy) = s.blocks[last].inner.center();
    Camera { cx, cy, zoom }
}

fn drawn_work(st: &CullStats) -> [usize; 11] {
    [
        st.quads,
        st.labels,
        st.icons,
        st.drawn_regions,
        st.drawn_blocks,
        st.drawn_cells,
        st.aggregated_blocks,
        st.aggregated_cells,
        st.drawn_sats,
        st.curves,
        st.edges,
    ]
}

#[derive(Clone, Copy)]
enum WorkExpectation {
    Variable,
    Constant,
    AggregateAboveLimit,
}

struct Sweep {
    name: &'static str,
    child: &'static str,
    zoom: f32,
    degrees: &'static [usize],
    exts: &'static [&'static str],
    spec: fn(usize) -> SceneSpec,
    cross_scene: bool,
    flat_edge_control: bool,
    camera: fn(&Scene, f32) -> Camera,
    unit_bytes: usize,
    padded_bytes: usize,
    expected_work: WorkExpectation,
}

fn block_fan_spec(degree: usize) -> SceneSpec {
    SceneSpec {
        regions: 1,
        blocks_per_region: degree,
        cells_per_block: CELLS_PER_BLOCK,
        sats_per_block: SATS_PER_BLOCK,
        edges_per_region: 0,
    }
}

fn cell_fan_spec(degree: usize) -> SceneSpec {
    SceneSpec {
        regions: 1,
        blocks_per_region: CELL_SWEEP_BLOCKS,
        cells_per_block: degree,
        sats_per_block: SATS_PER_BLOCK,
        edges_per_region: 0,
    }
}

fn edge_fan_spec(degree: usize) -> SceneSpec {
    SceneSpec {
        regions: 1,
        blocks_per_region: EDGE_SWEEP_BLOCKS,
        cells_per_block: CELLS_PER_BLOCK,
        sats_per_block: SATS_PER_BLOCK,
        edges_per_region: degree,
    }
}

fn cross_fan_spec(_degree: usize) -> SceneSpec {
    SceneSpec {
        regions: CROSS_SWEEP_REGIONS,
        blocks_per_region: CROSS_SWEEP_BLOCKS,
        cells_per_block: CELLS_PER_BLOCK,
        sats_per_block: SATS_PER_BLOCK,
        edges_per_region: 0,
    }
}

fn sweeps() -> Vec<Sweep> {
    vec![
        Sweep {
            name: "A namespace fan-out, fixed camera, drawn work constant",
            child: "block",
            zoom: BLOCK_ZOOM,
            degrees: &DEGREES,
            exts: BOTH_EXTS,
            spec: block_fan_spec,
            cross_scene: false,
            flat_edge_control: false,
            camera: block_camera,
            unit_bytes: size_of::<BlockNode>(),
            padded_bytes: size_of::<BlockNode<WlPad>>(),
            expected_work: WorkExpectation::Constant,
        },
        Sweep {
            name: "B workload fan-out, fixed camera, drawn work constant",
            child: "cell",
            zoom: CELL_ZOOM,
            degrees: &DEGREES,
            exts: BOTH_EXTS,
            spec: cell_fan_spec,
            cross_scene: false,
            flat_edge_control: false,
            camera: cell_camera,
            unit_bytes: size_of::<CellNode>(),
            padded_bytes: size_of::<CellNode<PodPad>>(),
            expected_work: WorkExpectation::Constant,
        },
        Sweep {
            name: "C edge fan-out inside one visible region, 512 blocks",
            child: "edge",
            zoom: BLOCK_ZOOM,
            degrees: &EDGE_DEGREES,
            exts: BOTH_EXTS,
            spec: edge_fan_spec,
            cross_scene: false,
            flat_edge_control: true,
            camera: block_camera,
            unit_bytes: size_of::<Edge>(),
            padded_bytes: size_of::<Edge>(),
            expected_work: WorkExpectation::Variable,
        },
        Sweep {
            name: "C-cross edge fan-out in the ungated tail, 2 regions x 256 blocks",
            child: "edge",
            zoom: BLOCK_ZOOM,
            degrees: &EDGE_DEGREES,
            exts: BOTH_EXTS,
            spec: cross_fan_spec,
            cross_scene: true,
            flat_edge_control: true,
            camera: far_block_camera,
            unit_bytes: size_of::<Edge>(),
            padded_bytes: size_of::<Edge>(),
            expected_work: WorkExpectation::Variable,
        },
        Sweep {
            name: "D namespace fan-out, camera fitted to the fanned-out region",
            child: "block",
            zoom: 0.0,
            degrees: &DEGREES,
            exts: PADDED_ONLY,
            spec: block_fan_spec,
            cross_scene: false,
            flat_edge_control: false,
            camera: fit_region_camera,
            unit_bytes: size_of::<BlockNode>(),
            padded_bytes: size_of::<BlockNode<WlPad>>(),
            expected_work: WorkExpectation::Variable,
        },
        Sweep {
            name: "E workload fan-out, camera fitted to the fanned-out block",
            child: "cell",
            zoom: 0.0,
            degrees: &DEGREES,
            exts: PADDED_ONLY,
            spec: cell_fan_spec,
            cross_scene: false,
            flat_edge_control: false,
            camera: fit_block_camera,
            unit_bytes: size_of::<CellNode>(),
            padded_bytes: size_of::<CellNode<PodPad>>(),
            expected_work: WorkExpectation::AggregateAboveLimit,
        },
    ]
}

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let policy = lod_policy();
    let mut cases = Vec::new();

    for sweep in sweeps() {
        for &ext in sweep.exts {
            let mut baseline: Option<[usize; 11]> = None;
            for &degree in sweep.degrees {
                let spec = (sweep.spec)(degree);
                let unit = if sweep.cross_scene {
                    cross_scene(spec, degree)
                } else {
                    scene(spec)
                };
                let camera = (sweep.camera)(&unit, sweep.zoom);
                let (samples, stats, flat_p50) = if ext == UNIT {
                    measure_with_flat_control(&unit, &policy, &camera, sweep.flat_edge_control)
                } else {
                    measure_with_flat_control(
                        &pad_ext(unit),
                        &policy,
                        &camera,
                        sweep.flat_edge_control,
                    )
                };
                let p50 = samples.percentile(0.50);
                let p99 = samples.percentile(0.99);
                let work = drawn_work(&stats);
                let constant = *baseline.get_or_insert(work) == work;
                match sweep.expected_work {
                    WorkExpectation::Constant => {
                        assert!(
                            constant,
                            "{} changed drawn work at degree {degree}: {work:?}",
                            sweep.name
                        );
                        assert_eq!(stats.aggregated_blocks, 0, "{} aggregated", sweep.name);
                    }
                    WorkExpectation::AggregateAboveLimit if degree > policy.max_cells_per_block => {
                        assert!(
                            stats.aggregated_blocks > 0,
                            "{} did not aggregate degree {degree}",
                            sweep.name
                        );
                        assert!(
                            stats.aggregated_cells >= degree,
                            "{} aggregated only {} of {degree} cells",
                            sweep.name,
                            stats.aggregated_cells
                        );
                        assert!(
                            stats.drawn_cells <= policy.max_cells_per_block,
                            "{} still drew {} cells at degree {degree}",
                            sweep.name,
                            stats.drawn_cells
                        );
                    }
                    WorkExpectation::AggregateAboveLimit => {
                        assert_eq!(
                            stats.aggregated_blocks, 0,
                            "{} aggregated early",
                            sweep.name
                        );
                    }
                    WorkExpectation::Variable => {}
                }
                let child_bytes = if ext == UNIT {
                    sweep.unit_bytes
                } else {
                    sweep.padded_bytes
                };
                cases.push(Case {
                    sweep: sweep.name,
                    child: sweep.child,
                    degree,
                    ext,
                    child_bytes,
                    child_kib: (degree * child_bytes) as f64 / 1024.0,
                    zoom: camera.zoom,
                    iters: samples.iterations(),
                    samples: samples.sample_count(),
                    batch_size: samples.batch_size(),
                    p50_rmad: samples.p50_relative_mad(),
                    p50_ns: p50,
                    p99_ns: p99,
                    p50_flat_edges_ns: flat_p50,
                    ns_per_child: if degree == 0 {
                        0.0
                    } else {
                        p50 / degree as f64
                    },
                    stats,
                    drawn_constant: constant,
                });
            }
        }
    }

    if json {
        print_json(&cases);
    } else {
        print_table(&cases);
    }
}

fn print_table(cases: &[Case]) {
    println!("k10s headless fan-out cull bench - logical viewport {VW:.0}x{VH:.0}, no GPU");
    println!("  one fanned-out parent per scene, fixed camera per sweep");
    println!(
        "  `padded` ext reproduces the k10s-core node strides ({} / {} / {} / {} B for region / block / cell / sat)",
        size_of::<RegionNode<NsPad>>(),
        size_of::<BlockNode<WlPad>>(),
        size_of::<CellNode<PodPad>>(),
        size_of::<CellNode<SatPad>>(),
    );
    println!("  `*` marks a row whose drawn counters differ from the first row of its sweep");

    let mut current = "";
    let mut current_ext = "";
    for c in cases {
        if c.sweep != current || c.ext != current_ext {
            current = c.sweep;
            current_ext = c.ext;
            println!(
                "\n  {} - {} ext, {} B/{}",
                c.sweep, c.ext, c.child_bytes, c.child
            );
        }
        println!(
            "    {:>6} {:<5} zoom {:>6.3}  p50 {:>9.1} ns  p99 {:>9.1} ns  flat-edges p50 {:>9} ns  samples {:>6} x {:>5}  rMAD {:>5.1}%  {:>7.3} ns/{:<5} {:>8.1} KiB{} | quads {:>6} labels {:>4} icons {:>5} sats {:>5} curves {:>5} edges {:>5} | drawn r/b/c {:>3}/{:>6}/{:>6} aggregate b/c {:>2}/{:>6}",
            c.degree,
            c.child,
            c.zoom,
            c.p50_ns,
            c.p99_ns,
            c.p50_flat_edges_ns
                .map(|ns| format!("{ns:.0}"))
                .unwrap_or_else(|| "-".to_string()),
            c.samples,
            c.batch_size,
            c.p50_rmad * 100.0,
            c.ns_per_child,
            c.child,
            c.child_kib,
            if c.drawn_constant { " " } else { "*" },
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

fn print_json(cases: &[Case]) {
    println!("{{");
    println!("  \"schema_version\": 5,");
    println!("  \"viewport\": [{VW}, {VH}],");
    println!("  \"cases\": [");
    for (i, c) in cases.iter().enumerate() {
        let comma = if i + 1 == cases.len() { "" } else { "," };
        println!("    {{");
        println!("      \"sweep\": \"{}\",", c.sweep);
        println!("      \"child\": \"{}\",", c.child);
        println!("      \"degree\": {},", c.degree);
        println!("      \"ext\": \"{}\",", c.ext);
        println!("      \"child_bytes\": {},", c.child_bytes);
        println!("      \"child_kib\": {:.1},", c.child_kib);
        println!("      \"zoom\": {},", c.zoom);
        println!("      \"iters\": {},", c.iters);
        println!("      \"samples\": {},", c.samples);
        println!("      \"batch_size\": {},", c.batch_size);
        println!("      \"p50_rmad\": {:.6},", c.p50_rmad);
        println!("      \"p50_ns\": {:.3},", c.p50_ns);
        println!("      \"p99_ns\": {:.3},", c.p99_ns);
        match c.p50_flat_edges_ns {
            Some(ns) => println!("      \"p50_flat_edges_ns\": {ns:.0},"),
            None => println!("      \"p50_flat_edges_ns\": null,"),
        }
        println!("      \"ns_per_child\": {:.4},", c.ns_per_child);
        println!("      \"drawn_constant\": {},", c.drawn_constant);
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
