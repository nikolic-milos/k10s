use std::hint::black_box;
use std::sync::Arc;

use futures::channel::mpsc;
use gpui::{AppContext as _, HeadlessAppContext, NoopTextSystem, WindowHandle, px, size};
use k10s_atlas::{Camera, TextCacheCounts};
use k10s_clustergen::stream;
use k10s_clustergen::{GenConfig, Scenario, generate};
use k10s_core::{WorldCtrl, new_shared_scene};
use k10s_map::MapView;
use k10s_world::{ExtractBench, LayoutMode};

const OBJECTS: u32 = 4_000;
const WARMUP: usize = 64;
const ITERS: usize = 256;
const VW: f32 = 1600.0;
const VH: f32 = 1000.0;

#[global_allocator]
static GLOBAL: &stats_alloc::StatsAlloc<std::alloc::System> = &stats_alloc::INSTRUMENTED_SYSTEM;

fn draw(app: &mut HeadlessAppContext, window: WindowHandle<MapView>) {
    window
        .update(app, |_, window, cx| {
            window.refresh();
            cx.notify();
        })
        .expect("the headless map window remains open");
    app.run_until_parked();
}

#[derive(Clone, Copy)]
struct Allocation {
    allocations: f64,
    reallocations: f64,
    bytes: f64,
    cache: TextCacheCounts,
}

fn paint_snapshot(
    app: &HeadlessAppContext,
    window: WindowHandle<MapView>,
) -> k10s_map::testing::PaintSnapshot {
    window
        .read_with(app, |map, _| map.testing_paint_snapshot())
        .expect("read map paint counters after draw")
}

fn measure(
    app: &mut HeadlessAppContext,
    window: WindowHandle<MapView>,
    cached: bool,
) -> Allocation {
    let mut before = paint_snapshot(app, window);
    let mut allocations = 0;
    let mut reallocations = 0;
    let mut bytes = 0;
    let mut lines = 0;
    let mut scalars = 0;
    let mut fit_refused = 0;
    let mut paint_errors = 0;
    let mut cache = TextCacheCounts::default();
    for sample in 0..ITERS {
        let allocation = {
            let region = stats_alloc::Region::new(GLOBAL);
            draw(app, window);
            region.change()
        };
        let after = paint_snapshot(app, window);
        assert_eq!(
            after.frames,
            before.frames + 1,
            "sample {sample} must enter exactly one map paint callback"
        );
        assert!(
            after.paint_ok_lines > 0,
            "sample {sample} made no successful text paint calls"
        );
        assert!(
            after.paint_ok_source_scalars > 0,
            "sample {sample} had no source scalars in successful paint calls"
        );
        assert_eq!(
            after.paint_errors, 0,
            "sample {sample} failed to paint a line"
        );
        if cached {
            assert!(
                after.cache.hits > 0,
                "sample {sample} did not reuse prepared text"
            );
            assert_eq!(
                after.cache.misses, 0,
                "sample {sample} missed its warm cache"
            );
            assert_eq!(
                after.cache.evictions, 0,
                "sample {sample} evicted warm text"
            );
        } else {
            assert!(
                after.cache.misses > 0,
                "sample {sample} shaped no uncached text"
            );
            assert_eq!(
                after.cache.hits, 0,
                "sample {sample} used the disabled cache"
            );
            assert_eq!(
                after.cache.evictions, 0,
                "sample {sample} evicted from the disabled cache"
            );
        }
        allocations += allocation.allocations;
        reallocations += allocation.reallocations;
        bytes += allocation.bytes_allocated;
        lines += after.paint_ok_lines;
        scalars += after.paint_ok_source_scalars;
        fit_refused += after.fit_refused;
        paint_errors += after.paint_errors;
        cache.hits += after.cache.hits;
        cache.misses += after.cache.misses;
        cache.evictions += after.cache.evictions;
        before = after;
    }
    eprintln!(
        "k10s paint coverage: cache={} draws={ITERS} successful_line_calls={lines} source_scalars_in_successful_calls={scalars} fit_refused={fit_refused} paint_errors={paint_errors} hits={} misses={} evictions={}",
        if cached { "cached" } else { "uncached-control" },
        cache.hits,
        cache.misses,
        cache.evictions,
    );
    let calls = ITERS as f64;
    Allocation {
        allocations: allocations as f64 / calls,
        reallocations: reallocations as f64 / calls,
        bytes: bytes as f64 / calls,
        cache: before.cache,
    }
}

fn main() {
    let json = std::env::args().any(|arg| arg == "--json");
    let mode = LayoutMode::Spread;
    let cluster = generate(&GenConfig {
        seed: 42,
        target_objects: OBJECTS,
        scenario: Scenario::Platform,
    });
    let events = stream::snapshot(&cluster, mode.emits_attachments());
    let mut extract = ExtractBench::new(&events, mode);
    extract.run_extract();
    let scene = extract.snapshot();
    let (cx, cy) = scene.blocks[0].inner.center();
    let camera = Camera { cx, cy, zoom: 4.5 };
    let shared = new_shared_scene();
    shared.store(scene);

    let (ctrl, _ctrl_rx) = crossbeam_channel::bounded::<WorldCtrl>(1);
    let (_damage, damage_rx) = mpsc::channel(1);
    let mut app = HeadlessAppContext::new(Arc::new(NoopTextSystem::new()));
    let window = app
        .open_window(size(px(VW), px(VH)), move |_, cx| {
            // No flight runs here -- `bench` is `None` -- so nothing ever sets
            // this, and it is constructed once outside the measured region.
            let bench_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            cx.new(|cx| MapView::new(shared, ctrl, None, bench_failed, damage_rx, cx))
        })
        .expect("create a headless map window");
    window
        .update(&mut app, |map, _, cx| {
            map.testing_set_camera(camera);
            cx.notify();
        })
        .expect("set the benchmark camera");

    eprintln!(
        "k10s paint sizes: ShapedLine={} LabelJob={}",
        std::mem::size_of::<gpui::ShapedLine>(),
        std::mem::size_of::<k10s_map::testing::LabelJob>(),
    );
    for _ in 0..WARMUP {
        draw(&mut app, window);
    }
    let warm_cache = window
        .read_with(&app, |map, _| map.testing_text_cache())
        .expect("read warm text-cache counters");
    assert!(warm_cache.hits > 0, "warm paints must reuse shaped labels");
    assert_eq!(warm_cache.misses, 0, "the label cache must be warm");

    let cached = measure(&mut app, window, true);
    window
        .update(&mut app, |map, _, cx| {
            map.testing_enable_text_cache(false);
            cx.notify();
        })
        .expect("disable the cache control");
    let uncached = measure(&mut app, window, false);
    black_box((cached, uncached));

    if json {
        println!("{{");
        println!("  \"schema_version\": 2,");
        println!("  \"mode\": \"full-paint-alloc\",");
        println!("  \"objects\": {OBJECTS},");
        println!("  \"viewport\": [{VW}, {VH}],");
        println!("  \"warmup\": {WARMUP},");
        println!("  \"iters\": {ITERS},");
        println!("  \"cases\": [");
        print_allocation("cached", cached, true);
        print_allocation("uncached-control", uncached, false);
        println!("  ]");
        println!("}}");
    } else {
        println!("k10s headless full-paint allocation bench - {OBJECTS} objects, {VW:.0}x{VH:.0}");
        println!(
            "  cached:   {:.1} allocations, {:.1} reallocations, {:.0} bytes",
            cached.allocations, cached.reallocations, cached.bytes,
        );
        println!(
            "  uncached: {:.1} allocations, {:.1} reallocations, {:.0} bytes",
            uncached.allocations, uncached.reallocations, uncached.bytes,
        );
    }
}

fn print_allocation(name: &str, allocation: Allocation, comma: bool) {
    println!("    {{");
    println!("      \"cache\": \"{name}\",");
    println!(
        "      \"allocations_per_paint\": {:.3},",
        allocation.allocations
    );
    println!(
        "      \"reallocations_per_paint\": {:.3},",
        allocation.reallocations
    );
    println!("      \"bytes_per_paint\": {:.0},", allocation.bytes);
    println!("      \"text_cache_hits\": {},", allocation.cache.hits);
    println!("      \"text_cache_misses\": {},", allocation.cache.misses);
    println!(
        "      \"text_cache_evictions\": {}",
        allocation.cache.evictions
    );
    println!("    }}{}", if comma { "," } else { "" });
}
