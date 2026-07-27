//! What a frame's traversal costs, headless: `frame::walk` next to the cull oracle at the same
//! camera on the same scene.
//!
//! Both `k10s-atlas` benches and `k10s-world/benches/fanout_cull.rs` time `k10s_atlas::cull`, which
//! walks the scene and increments counters. `walk` is what a frame runs: it builds `PaintQuad`s,
//! turns every visible label into a `SharedString`, flattens dashed quadratics into a
//! `PathBuilder` and pushes icon jobs. Whether the oracle stands in for it is a question with a
//! number, and this is where the number comes from -- four per camera, one process, same inputs:
//!
//! * `walk>paint` -- the shipping path, `PaintSink` and all, with the quad and job buffers reused
//!   across calls the way `paint_map` reuses them frame to frame.
//! * `walk>count` -- the same traversal into a sink that counts and drops, so the gap to the
//!   column before it is what the painter's buffers and path builders cost.
//! * `atlas cull` -- the oracle the other three benches time.
//! * `map cull` -- the painter-side oracle: atlas cull plus the hex recount and the flat edge
//!   rescan that `paint_map` asserts against in a debug build.
//!
//! Built with `--features bench-alloc` it stops timing and counts allocations instead, which is
//! ROADMAP §6.7's instrument, and it exits non-zero if the label path allocates at any camera. The
//! two modes are separate builds because the counter costs two atomic read-modify-writes on every
//! allocation and two on every free, all charged to `walk` and none to either oracle: a comparison
//! that bills one side for its own measurement is not a comparison. The label column is what that
//! instrument was built to watch -- 399 allocations per walk at the saturated camera before
//! `LabelJob::text` began sharing the scene's `Arc<str>`, and 0 there now.
//!
//! Run: `cargo bench -p k10s-map --features testing --bench walk [-- --json]`, and again with
//! `--features testing,bench-alloc` for the allocation axis.

use std::hint::black_box;
use std::sync::Arc;
#[cfg(not(feature = "bench-alloc"))]
use std::time::{Duration, Instant};

use gpui::{Bounds, PaintQuad, Pixels, point, px, size};
use k10s_atlas::testing::{SceneSpec, lod_policy, scene as base_scene};
use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
use k10s_core::{
    KindId, NsExt, NsNode, PodExt, PodNode, ReasonId, SatExt, SatNode, SceneSnapshot, Severity,
    State, ToolId, WlExt, WorkloadNode,
};
use k10s_map::FrameOpts;
#[cfg(not(feature = "bench-alloc"))]
use k10s_map::cull as map_cull;
use k10s_map::testing::{FrameSink, IconJob, LabelJob, PaintSink, walk};

/// ROADMAP §6.6: a logical viewport, pinned, or nothing here reproduces on another machine.
const VW: f32 = 1600.0;
const VH: f32 = 1000.0;

/// `SharedString` is a `SmolStr`, and smol_str 0.3.6 keeps this many bytes inline: a longer label
/// holds an `Arc<str>` instead. The `heap` column counts labels against this boundary, which is
/// what stops the zero in the label column from being vacuous -- a camera can emit 399 labels no
/// `SmolStr` can inline and still allocate nothing, because the `Arc` it holds is the scene's.
const INLINE_CAP: usize = 23;

/// The floor exists so the p99 column is a p99: at 51 samples and fewer the 0.99 index rounds to
/// the last one, which makes the number the maximum, and below a hundred a single sample carries
/// more than a whole percentile. `iters` is reported per row so a comparator can check rather than
/// trust.
#[cfg(not(feature = "bench-alloc"))]
const MIN_ITERS: usize = 200;
#[cfg(not(feature = "bench-alloc"))]
const WARMUP: usize = 200;
#[cfg(not(feature = "bench-alloc"))]
const MAX_ITERS: usize = 200_000;
#[cfg(not(feature = "bench-alloc"))]
const BUDGET: Duration = Duration::from_millis(120);

/// Allocation counts are per call and need no tail, only a steady state: enough warmup for the
/// reused buffers to reach their capacity, then a flat average over calls that all do the same
/// work.
#[cfg(feature = "bench-alloc")]
const ALLOC_WARMUP: usize = 64;
#[cfg(feature = "bench-alloc")]
const ALLOC_ITERS: usize = 256;

#[cfg(feature = "bench-alloc")]
#[global_allocator]
static GLOBAL: &stats_alloc::StatsAlloc<std::alloc::System> = &stats_alloc::INSTRUMENTED_SYSTEM;

fn viewport() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(VW), px(VH)),
    }
}

/// What the shipping binary walks with no `K10S_*` set.
fn opts(policy: &LodPolicy) -> FrameOpts<'_> {
    FrameOpts {
        policy,
        edges_on: true,
        skip_blocks: false,
        hex: true,
    }
}

/// Counts what the walk emits and drops it.
///
/// Every method hands its argument to `black_box` before dropping it. A sink that only increments
/// is a sink whose primitives the optimiser may never build -- and a label job is the one that
/// could still allocate, so eliding it would take both the timing and the ratchet with it.
#[derive(Debug, Default)]
struct Count {
    labels: usize,
    heap_labels: usize,
    label_bytes: usize,
}

impl FrameSink for Count {
    fn bg_quad(&mut self, quad: PaintQuad) {
        black_box(quad);
    }

    fn fg_quad(&mut self, quad: PaintQuad) {
        black_box(quad);
    }

    fn label(&mut self, label: LabelJob) {
        self.labels += 1;
        self.label_bytes += label.text.len();
        self.heap_labels += usize::from(label.text.len() > INLINE_CAP);
        black_box(label);
    }

    fn icon(&mut self, icon: IconJob) {
        black_box(icon);
    }

    fn hex_ring(&mut self, ring: &[(f32, f32); 6]) {
        black_box(ring);
    }

    fn curve(&mut self, hub: (f32, f32), ctrl: (f32, f32), sat: (f32, f32)) {
        black_box((hub, ctrl, sat));
    }

    fn edge(&mut self, a: (f32, f32), ctrl: (f32, f32), b: (f32, f32)) {
        black_box((a, ctrl, b));
    }
}

/// The painter's own buffers, held across calls exactly as `MapView` holds them across frames, so
/// what this measures is a steady-state frame and not the first one.
#[derive(Default)]
struct Buffers {
    bg: Vec<PaintQuad>,
    fg: Vec<PaintQuad>,
    labels: Vec<LabelJob>,
    icons: Vec<IconJob>,
}

impl Buffers {
    fn walk(&mut self, scene: &SceneSnapshot, camera: Camera, blend: StageBlend, o: FrameOpts<'_>) {
        let mut sink = PaintSink::new(
            &mut self.bg,
            &mut self.fg,
            &mut self.labels,
            &mut self.icons,
            true,
        );
        let stats = walk(viewport(), scene, camera, blend, o, &mut sink);
        // The paths are handed back for tessellation, which is a later span in `paint_map` and not
        // part of the walk; dropping them here is what keeps this column comparable to `walk_us`.
        black_box(sink.into_paths());
        black_box(stats);
    }
}

/// Give the engine's generic scene the extensions the painter reads. Severities, kinds and tools
/// cycle so no colour or glyph branch folds away. Satellite details stay short because a real one
/// is short -- which is the point of reporting `heap` beside `labels` rather than assuming every
/// label costs an allocation.
fn snapshot(spec: SceneSpec) -> SceneSnapshot {
    const SEVERITIES: [Severity; 4] = [
        Severity::Ok,
        Severity::Warn,
        Severity::Err,
        Severity::Unknown,
    ];
    const REASONS: [ReasonId; 4] = [
        ReasonId::RUNNING,
        ReasonId::NOT_READY,
        ReasonId::CRASH_LOOP_BACK_OFF,
        ReasonId::UNKNOWN,
    ];
    const KINDS: [KindId; 5] = [
        KindId::DEPLOYMENT,
        KindId::STATEFUL_SET,
        KindId::DAEMON_SET,
        KindId::JOB,
        KindId::CRON_JOB,
    ];
    const TOOLS: [ToolId; 4] = [
        ToolId::NONE,
        ToolId::POSTGRES,
        ToolId::ISTIO,
        ToolId::PROMETHEUS,
    ];
    const SAT_KINDS: [KindId; 4] = [
        KindId::SERVICE,
        KindId::VOLUME,
        KindId::CONFIG_MAP,
        KindId::SECRET,
    ];
    const DETAILS: [&str; 4] = ["ClusterIP", "16Gi", "8 keys", "2 items"];

    let base = base_scene(spec);
    SceneSnapshot {
        rev: base.rev,
        bounds: base.bounds,
        regions: base
            .regions
            .iter()
            .enumerate()
            .map(|(i, r)| NsNode {
                rect: r.rect,
                label: r.label.clone(),
                weight: r.weight,
                children: r.children.clone(),
                ext: NsExt {
                    unhealthy_frac: (i % 5) as f32 * 0.15,
                    rollup: SEVERITIES[i % SEVERITIES.len()],
                },
            })
            .collect(),
        blocks: base
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| WorkloadNode {
                rect: b.rect,
                inner: b.inner,
                label: b.label.clone(),
                children: b.children.clone(),
                sats: b.sats.clone(),
                ext: WlExt {
                    kind: KINDS[i % KINDS.len()],
                    tool: TOOLS[i % TOOLS.len()],
                    rollup: SEVERITIES[i % SEVERITIES.len()],
                    ns: 0,
                },
            })
            .collect(),
        cells: base
            .cells
            .iter()
            .enumerate()
            .map(|(i, c)| PodNode {
                rect: c.rect,
                label: c.label.clone(),
                ext: PodExt {
                    state: State::of(REASONS[i % REASONS.len()]),
                },
            })
            .collect(),
        sats: base
            .sats
            .iter()
            .enumerate()
            .map(|(i, s)| SatNode {
                rect: s.rect,
                label: s.label.clone(),
                ext: SatExt {
                    kind: SAT_KINDS[i % SAT_KINDS.len()],
                    detail: Arc::from(DETAILS[i % DETAILS.len()]),
                },
            })
            .collect(),
        edges: base.edges.clone(),
        region_edges: base.region_edges.clone(),
        cross_edges: base.cross_edges.clone(),
        totals: base.totals,
    }
}

/// The five cameras `benches/cull.rs` sweeps, at the same anchors, so the `atlas cull` column here
/// can be read straight against that bench's committed row.
fn cameras(scene: &SceneSnapshot) -> Vec<(&'static str, Camera)> {
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

/// One workload holding 4,096 pods, framed so the viewport holds more of them than `max_labels`
/// allows. The uniform cameras draw tens of labels, which is what a cluster looks like; this one
/// draws the whole budget, which is the ceiling any allocation ratchet has to hold under.
fn dense_spec() -> SceneSpec {
    SceneSpec {
        regions: 1,
        blocks_per_region: 4,
        cells_per_block: 4096,
        sats_per_block: 2,
        edges_per_region: 0,
    }
}

fn dense_camera(scene: &SceneSnapshot) -> (&'static str, Camera) {
    let (cx, cy) = scene.blocks[0].inner.center();
    ("Z3 saturated", Camera { cx, cy, zoom: 4.0 })
}

struct Row {
    scene: String,
    objects: usize,
    camera: &'static str,
    zoom: f32,
    stats: CullStats,
    heap_labels: usize,
    label_bytes: usize,
    payload: Payload,
}

#[cfg(not(feature = "bench-alloc"))]
type Payload = Timing;
#[cfg(feature = "bench-alloc")]
type Payload = Alloc;

#[cfg(not(feature = "bench-alloc"))]
fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[i] as f64
}

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let policy = lod_policy();
    let mut rows = Vec::new();

    type Pick = fn(&SceneSnapshot) -> Vec<(&'static str, Camera)>;
    let mut run = |name: String, spec: SceneSpec, pick: Pick| {
        let scene = snapshot(spec);
        for (camera_name, camera) in pick(&scene) {
            let blend = StageBlend::settled(policy.stage_for_zoom(camera.zoom));
            let mut count = Count::default();
            let stats = walk(viewport(), &scene, camera, blend, opts(&policy), &mut count);
            rows.push(Row {
                scene: name.clone(),
                objects: spec.total_objects(),
                camera: camera_name,
                zoom: camera.zoom,
                stats,
                heap_labels: count.heap_labels,
                label_bytes: count.label_bytes,
                payload: measure(&scene, camera, blend, &policy),
            });
        }
    };

    for regions in [200usize, 400] {
        run(
            format!("uniform r{regions} b15"),
            SceneSpec::uniform(regions, 15),
            cameras,
        );
    }
    run("cellfan r1 b4 c4096".to_string(), dense_spec(), |s| {
        vec![dense_camera(s)]
    });

    if json {
        print_json(&rows);
    } else {
        print_table(&rows);
    }

    #[cfg(feature = "bench-alloc")]
    if !label_ratchet_holds(&rows) {
        std::process::exit(1);
    }
}

/// ROADMAP §6.7's ratchet as a check rather than a paragraph: a walk allocates nothing per label,
/// at every camera, whatever the labels are. The `heap` column is what keeps that from being
/// vacuous, and the failure line names the camera so a regression is one row to look at.
///
/// Only the label term is gated. The structural term belongs to the path builders and the dash
/// scratch, moves with the number of stroked layers a camera touches, and so has no one number to
/// hold it to.
#[cfg(feature = "bench-alloc")]
fn label_ratchet_holds(rows: &[Row]) -> bool {
    let mut held = true;
    for r in rows {
        if r.payload.count_sink.allocs != 0.0 {
            eprintln!(
                "RATCHET: {} {} allocates {:.1} per walk from the label path, floor is 0",
                r.scene, r.camera, r.payload.count_sink.allocs
            );
            held = false;
        }
    }
    held
}

// --- timing mode -------------------------------------------------------------------------------

#[cfg(not(feature = "bench-alloc"))]
struct Timing {
    iters: usize,
    walk_paint: (f64, f64),
    walk_count: (f64, f64),
    atlas_cull: (f64, f64),
    map_cull: (f64, f64),
}

#[cfg(not(feature = "bench-alloc"))]
fn samples(mut run: impl FnMut()) -> (f64, f64, usize) {
    for _ in 0..WARMUP {
        run();
    }
    let mut out = Vec::with_capacity(MIN_ITERS);
    let start = Instant::now();
    while out.len() < MAX_ITERS && (out.len() < MIN_ITERS || start.elapsed() < BUDGET) {
        let t = Instant::now();
        run();
        out.push(t.elapsed().as_nanos() as u64);
    }
    out.sort_unstable();
    (
        percentile(&out, 0.50),
        percentile(&out, 0.99),
        out.len(),
    )
}

#[cfg(not(feature = "bench-alloc"))]
fn measure(
    scene: &SceneSnapshot,
    camera: Camera,
    blend: StageBlend,
    policy: &LodPolicy,
) -> Timing {
    let o = opts(policy);
    let mut buffers = Buffers::default();
    let (paint_p50, paint_p99, iters) =
        samples(|| buffers.walk(black_box(scene), black_box(camera), blend, o));

    let mut count = Count::default();
    let (count_p50, count_p99, _) = samples(|| {
        black_box(walk(
            viewport(),
            black_box(scene),
            black_box(camera),
            blend,
            o,
            &mut count,
        ));
    });
    black_box(&count);

    let (atlas_p50, atlas_p99, _) = samples(|| {
        black_box(k10s_atlas::cull(
            black_box(scene),
            black_box(&camera),
            policy,
            blend,
            VW,
            VH,
            o.edges_on,
            o.skip_blocks,
        ));
    });

    let (map_p50, map_p99, _) = samples(|| {
        black_box(map_cull(
            black_box(scene),
            black_box(&camera),
            blend,
            VW,
            VH,
            o,
        ));
    });

    Timing {
        iters,
        walk_paint: (paint_p50, paint_p99),
        walk_count: (count_p50, count_p99),
        atlas_cull: (atlas_p50, atlas_p99),
        map_cull: (map_p50, map_p99),
    }
}

#[cfg(not(feature = "bench-alloc"))]
fn print_table(rows: &[Row]) {
    println!("k10s headless walk bench - logical viewport {VW:.0}x{VH:.0}, no GPU");
    println!(
        "  walk>paint is the shipping frame path; atlas cull is what benches/cull.rs times at these same cameras"
    );
    println!(
        "  oracle ratio = walk>paint p50 / atlas cull p50: 1.0 means the oracle is a fair proxy for the painter"
    );
    println!("  allocations: rebuild with --features testing,bench-alloc");

    let mut current = String::new();
    for r in rows {
        if r.scene != current {
            current = r.scene.clone();
            println!("\n  {} - {} objects", r.scene, r.objects);
        }
        let t = &r.payload;
        println!(
            "    {:<13} zoom {:>6.2}  walk>paint p50 {:>9.0} p99 {:>9.0} | walk>count p50 {:>9.0} | atlas cull p50 {:>9.0} | map cull p50 {:>9.0} ns | ratio {:>6.2}x | iters {:>6} | quads {:>6} labels {:>4} (heap {:>4}, {:>6} B) icons {:>4} sats {:>4} curves {:>4} edges {:>4} hex {:>4}",
            r.camera,
            r.zoom,
            t.walk_paint.0,
            t.walk_paint.1,
            t.walk_count.0,
            t.atlas_cull.0,
            t.map_cull.0,
            ratio(t.walk_paint.0, t.atlas_cull.0),
            t.iters,
            r.stats.quads,
            r.stats.labels,
            r.heap_labels,
            r.label_bytes,
            r.stats.icons,
            r.stats.drawn_sats,
            r.stats.curves,
            r.stats.edges,
            r.stats.bg_cells,
        );
    }
}

#[cfg(not(feature = "bench-alloc"))]
fn ratio(walk: f64, cull: f64) -> f64 {
    if cull > 0.0 { walk / cull } else { 0.0 }
}

#[cfg(not(feature = "bench-alloc"))]
fn print_json(rows: &[Row]) {
    println!("{{");
    println!("  \"schema_version\": 1,");
    println!("  \"mode\": \"timing\",");
    println!("  \"viewport\": [{VW}, {VH}],");
    println!("  \"cases\": [");
    for (i, r) in rows.iter().enumerate() {
        let comma = if i + 1 == rows.len() { "" } else { "," };
        let t = &r.payload;
        println!("    {{");
        print_common(r);
        println!("      \"iters\": {},", t.iters);
        println!("      \"walk_paint_p50_ns\": {:.0},", t.walk_paint.0);
        println!("      \"walk_paint_p99_ns\": {:.0},", t.walk_paint.1);
        println!("      \"walk_count_p50_ns\": {:.0},", t.walk_count.0);
        println!("      \"walk_count_p99_ns\": {:.0},", t.walk_count.1);
        println!("      \"atlas_cull_p50_ns\": {:.0},", t.atlas_cull.0);
        println!("      \"atlas_cull_p99_ns\": {:.0},", t.atlas_cull.1);
        println!("      \"map_cull_p50_ns\": {:.0},", t.map_cull.0);
        println!("      \"map_cull_p99_ns\": {:.0},", t.map_cull.1);
        println!(
            "      \"oracle_ratio\": {:.3}",
            ratio(t.walk_paint.0, t.atlas_cull.0)
        );
        println!("    }}{comma}");
    }
    println!("  ]");
    println!("}}");
}

// --- allocation mode ---------------------------------------------------------------------------

/// Allocations, reallocations and bytes per `walk` call. `alloc` and `realloc` are apart because
/// they answer different questions: a label copy is a fresh block, a path builder outgrowing its
/// buffer is a resize, and only the first is what §6.7 means by a per-label-per-frame allocation.
#[cfg(feature = "bench-alloc")]
#[derive(Debug, Clone, Copy, Default)]
struct Per {
    allocs: f64,
    reallocs: f64,
    bytes: f64,
}

#[cfg(feature = "bench-alloc")]
struct Alloc {
    count_sink: Per,
    paint_sink: Per,
}

#[cfg(feature = "bench-alloc")]
fn per_call(mut run: impl FnMut()) -> Per {
    for _ in 0..ALLOC_WARMUP {
        run();
    }
    let region = stats_alloc::Region::new(GLOBAL);
    for _ in 0..ALLOC_ITERS {
        run();
    }
    let s = region.change();
    let n = ALLOC_ITERS as f64;
    Per {
        allocs: s.allocations as f64 / n,
        reallocs: s.reallocations as f64 / n,
        bytes: s.bytes_allocated as f64 / n,
    }
}

#[cfg(feature = "bench-alloc")]
fn measure(scene: &SceneSnapshot, camera: Camera, blend: StageBlend, policy: &LodPolicy) -> Alloc {
    let o = opts(policy);
    let mut count = Count::default();
    let count_sink = per_call(|| {
        black_box(walk(viewport(), scene, camera, blend, o, &mut count));
    });
    black_box(&count);

    let mut buffers = Buffers::default();
    let paint_sink = per_call(|| buffers.walk(scene, camera, blend, o));

    Alloc {
        count_sink,
        paint_sink,
    }
}

/// What the sink is responsible for: the four path builders and the dash scratch, with whatever the
/// traversal allocates on its own subtracted off. That subtrahend is 0 at every camera now, so this
/// reads the same as `total`; it stays subtracted so a label regression lands in the `label` column
/// instead of being smeared into this one.
#[cfg(feature = "bench-alloc")]
fn structural(a: &Alloc) -> f64 {
    a.paint_sink.allocs - a.count_sink.allocs
}

#[cfg(feature = "bench-alloc")]
fn print_table(rows: &[Row]) {
    println!("k10s headless walk allocation bench - logical viewport {VW:.0}x{VH:.0}, no GPU");
    println!(
        "  label = allocations a walk into a counting sink makes; the ratchet below holds it at 0 however many labels run past {INLINE_CAP} bytes"
    );
    println!(
        "  structural = total minus label: what the real `PaintSink` adds, being two lyon vectors per stroked layer the camera touches plus a dash scratch on any camera that draws a curve"
    );
    println!("  timings are suppressed here: the counting allocator charges walk and not the oracles");

    let mut current = String::new();
    for r in rows {
        if r.scene != current {
            current = r.scene.clone();
            println!("\n  {} - {} objects", r.scene, r.objects);
        }
        let a = &r.payload;
        println!(
            "    {:<13} zoom {:>6.2}  labels {:>4} (heap {:>4}) | allocs/walk label {:>7.1} structural {:>7.1} total {:>7.1} | reallocs/walk {:>7.1} | bytes/walk {:>9.0} | quads {:>6} icons {:>4} curves {:>4} edges {:>4} hex {:>4}",
            r.camera,
            r.zoom,
            r.stats.labels,
            r.heap_labels,
            a.count_sink.allocs,
            structural(a),
            a.paint_sink.allocs,
            a.paint_sink.reallocs,
            a.paint_sink.bytes,
            r.stats.quads,
            r.stats.icons,
            r.stats.curves,
            r.stats.edges,
            r.stats.bg_cells,
        );
    }

    let worst_labels = rows
        .iter()
        .map(|r| r.payload.count_sink.allocs)
        .fold(0.0, f64::max);
    let worst_heap = rows.iter().map(|r| r.heap_labels).max().unwrap_or(0);
    println!(
        "\n  ratchet: worst label allocs/walk {worst_labels:.1} against a floor of 0, on frames carrying up to {worst_heap} labels past the inline cap"
    );
}

#[cfg(feature = "bench-alloc")]
fn print_json(rows: &[Row]) {
    println!("{{");
    println!("  \"schema_version\": 1,");
    println!("  \"mode\": \"alloc\",");
    println!("  \"viewport\": [{VW}, {VH}],");
    println!("  \"inline_cap\": {INLINE_CAP},");
    println!("  \"iters\": {ALLOC_ITERS},");
    println!("  \"cases\": [");
    for (i, r) in rows.iter().enumerate() {
        let comma = if i + 1 == rows.len() { "" } else { "," };
        let a = &r.payload;
        println!("    {{");
        print_common(r);
        println!("      \"label_allocs\": {:.3},", a.count_sink.allocs);
        println!("      \"structural_allocs\": {:.3},", structural(a));
        println!("      \"total_allocs\": {:.3},", a.paint_sink.allocs);
        println!("      \"label_reallocs\": {:.3},", a.count_sink.reallocs);
        println!("      \"total_reallocs\": {:.3},", a.paint_sink.reallocs);
        println!("      \"label_bytes_alloc\": {:.0},", a.count_sink.bytes);
        println!("      \"total_bytes_alloc\": {:.0}", a.paint_sink.bytes);
        println!("    }}{comma}");
    }
    println!("  ]");
    println!("}}");
}

/// The counters both modes report, so a row is identifiable and the work behind a number is on the
/// same line as the number.
fn print_common(r: &Row) {
    println!("      \"scene\": \"{}\",", r.scene);
    println!("      \"objects\": {},", r.objects);
    println!("      \"camera\": \"{}\",", r.camera);
    println!("      \"zoom\": {},", r.zoom);
    println!("      \"quads\": {},", r.stats.quads);
    println!("      \"labels\": {},", r.stats.labels);
    println!("      \"heap_labels\": {},", r.heap_labels);
    println!("      \"label_bytes\": {},", r.label_bytes);
    println!("      \"labels_dropped\": {},", r.stats.labels_dropped);
    println!("      \"icons\": {},", r.stats.icons);
    println!("      \"sats\": {},", r.stats.drawn_sats);
    println!("      \"curves\": {},", r.stats.curves);
    println!("      \"edges\": {},", r.stats.edges);
    println!("      \"bg_cells\": {},", r.stats.bg_cells);
    println!("      \"drawn_regions\": {},", r.stats.drawn_regions);
    println!("      \"drawn_blocks\": {},", r.stats.drawn_blocks);
    println!("      \"drawn_cells\": {},", r.stats.drawn_cells);
}
