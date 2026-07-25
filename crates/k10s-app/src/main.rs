mod cli;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use k10s_clustergen::GenConfig;
use k10s_core::{WorldCtrl, new_shared_scene};
use k10s_map::{BenchMeta, MapView};

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed").to_string();
        eprintln!(
            "k10s: thread {name} panicked\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        default_hook(info);
    }));
}

fn main() {
    install_panic_hook();

    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("k10s: {err}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if args.help {
        println!("{}", cli::USAGE);
        return;
    }
    for ignored in &args.ignored {
        eprintln!("k10s: ignoring unrecognized argument {ignored}");
    }

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
        machine: args.machine_label(),
        arch: cli::platform(),
        objects: args.objects,
        seed: args.seed,
        layout: args.layout.as_str().to_string(),
        json: args.json,
    });
    let window_failed = Arc::new(AtomicBool::new(false));
    let window_status = window_failed.clone();
    gpui_platform::application().run(move |cx| {
        let bounds = Bounds::centered(None, size(px(1600.0), px(1000.0)), cx);
        let opened = cx.open_window(
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
        );
        if let Err(err) = opened {
            eprintln!("k10s: cannot open a window: {err}");
            window_status.store(true, Ordering::Relaxed);
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });

    let _ = shutdown_tx.send(WorldCtrl::Shutdown);
    let world_ended_cleanly = world.join().is_ok();
    if !world_ended_cleanly {
        eprintln!("k10s: the world thread panicked, cluster updates had stopped");
    }
    if !world_ended_cleanly || window_failed.load(Ordering::Relaxed) {
        std::process::exit(1);
    }
}
