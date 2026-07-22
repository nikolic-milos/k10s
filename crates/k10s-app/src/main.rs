use gpui::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use k10s_clustergen::{GenConfig, Scenario};
use k10s_core::{WorldCtrl, new_shared_scene};
use k10s_map::{BenchMeta, MapView};
use k10s_world::LayoutMode;

struct Args {
    objects: u32,
    seed: u64,
    churn: f32,
    scenario: Scenario,
    layout: LayoutMode,
    bench: bool,
    json: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        objects: 25_000,
        seed: 55,
        churn: 120.0,
        scenario: Scenario::Platform,
        layout: LayoutMode::Spread,
        bench: false,
        json: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .unwrap_or_else(|| panic!("missing value for {name}"))
        };
        match flag.as_str() {
            "--objects" => args.objects = val("--objects").parse().expect("--objects: u32"),
            "--seed" => args.seed = val("--seed").parse().expect("--seed: u64"),
            "--churn" => args.churn = val("--churn").parse().expect("--churn: f32"),
            "--scenario" => {
                let v = val("--scenario");
                args.scenario = Scenario::parse(&v)
                    .unwrap_or_else(|| panic!("--scenario: platform|observability|data, got {v}"));
            }
            "--layout" => {
                let v = val("--layout");
                args.layout = LayoutMode::parse(&v)
                    .unwrap_or_else(|| panic!("--layout: spread|dense, got {v}"));
            }
            "--bench" => args.bench = true,
            "--json" => args.json = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage: k10s [--objects N] [--seed S] [--churn EVENTS_PER_SEC] [--scenario platform|observability|data] [--layout spread|dense] [--bench] [--json]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown flag {other}"),
        }
    }
    if args.json && !args.bench {
        eprintln!("--json requires --bench");
        std::process::exit(2);
    }
    args
}

fn machine_id() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

fn main() {
    let args = parse_args();

    let t0 = std::time::Instant::now();
    let spec = k10s_clustergen::generate(&GenConfig {
        seed: args.seed,
        target_objects: args.objects,
        scenario: args.scenario,
    });
    eprintln!(
        "k10s: generated {} namespaces / {} workloads / {} pods / {} sats / {} edges (seed {}, scenario {}, layout {}) in {:.1?}",
        spec.namespaces.len(),
        spec.total_workloads,
        spec.total_pods,
        spec.total_sats,
        spec.total_edges,
        args.seed,
        args.scenario.as_str(),
        args.layout.as_str(),
        t0.elapsed(),
    );

    let scene = new_shared_scene();
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();

    let (damage_tx, damage_rx) = futures::channel::mpsc::unbounded();
    let world = k10s_world::spawn_world(
        spec,
        scene.clone(),
        ctrl_rx,
        args.seed,
        args.churn,
        args.layout,
        {
            move || {
                let _ = damage_tx.unbounded_send(());
            }
        },
    );

    let shutdown_tx = ctrl_tx.clone();
    let bench_meta = args.bench.then(|| BenchMeta {
        machine: machine_id(),
        arch: std::env::consts::ARCH.to_string(),
        objects: args.objects,
        seed: args.seed,
        layout: args.layout.as_str().to_string(),
        json: args.json,
    });
    gpui_platform::application().run(move |cx| {
        let bounds = Bounds::centered(None, size(px(1600.0), px(1000.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("k10s - Starmap".into()),
                    ..Default::default()
                }),
                focus: true,
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| {
                    MapView::new(scene.clone(), ctrl_tx.clone(), bench_meta, damage_rx, cx)
                });
                let focus = view.read(cx).focus_handle();
                window.focus(&focus, cx);
                view
            },
        )
        .expect("open k10s window");
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });

    let _ = shutdown_tx.send(WorldCtrl::Shutdown);
    let _ = world.join();
}
