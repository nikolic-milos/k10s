mod cli;
mod config;
mod diagnose;
mod launch;
mod provider;
mod startup;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use k10s_clustergen::GenConfig;
use k10s_core::{PreparedScene, WorldCtrl, new_shared_scene};
use k10s_data::DEFAULT_EVENT_SINK_CAPACITY;
use k10s_map::{BenchMeta, MapView};
use k10s_shell::{ConnectOutcome, ConnectRequest, LaunchProvider as _, ScanRequest, Workspace};

use config::{ConfigFiles, DesktopAppearance};
use launch::{Feed, LaunchService};
use startup::StartupBench;

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

const WORLD_CONTROL_CAPACITY: usize = 64;

fn main() {
    let process_started = std::time::Instant::now();
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
    let arguments_parsed = std::time::Instant::now();
    for ignored in &args.ignored {
        eprintln!("k10s: ignoring unrecognized argument {ignored}");
    }
    for flag in args.cluster_flags_without_cluster() {
        eprintln!("k10s: {flag} does nothing without --cluster");
    }
    for flag in args.generator_flags_with_cluster() {
        eprintln!("k10s: {flag} does nothing with --cluster; the cluster is the scene");
    }
    if args.churn_was_overridden() {
        eprintln!("k10s: --churn is ignored with --cluster; the cluster supplies the churn");
    }
    if args.machine.is_some() && !args.measuring() {
        eprintln!("k10s: --machine does nothing without a benchmark");
    }

    if args.list_contexts {
        std::process::exit(list_contexts());
    }
    let startup = args
        .startup_bench
        .then(|| StartupBench::new(process_started, arguments_parsed, &args));

    // Flight recordings retain their historical synchronous generated scene:
    // their timer starts from a fully determined input. Interactive generated
    // scenes and command-line cluster connections both start behind the window
    // through the launch service, so neither CPU work nor network latency can
    // delay the first photon.
    let generate_after_launch = !args.cluster && !args.bench && args.scene_was_named();
    let world_seed = if args.bench {
        let generated = generate(&args);
        eprintln!("k10s: {}", generated.summary);
        k10s_world::WorldSeed::Prepared(generated.scene)
    } else {
        k10s_world::WorldSeed::Events(Vec::new())
    };
    let choose_on_launch = !args.scene_was_named();
    if let Some(startup) = &startup {
        startup.source_ready();
        if args.bench {
            startup.content_ready();
        }
    }

    let scene = new_shared_scene();
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded(WORLD_CONTROL_CAPACITY);
    // One live channel for the whole process, created before the world so any
    // cluster connected behind the window has somewhere to send its deltas.
    // The scene itself never travels down it -- a connection sends
    // `WorldCtrl::Rebuild` and carries its own stream -- so the channel contains
    // only changes that follow the immutable initial snapshot.
    let (live_tx, live_rx) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
    let feed = Feed::new(live_tx);
    // A scene the command line named keeps that command line's churn. One that
    // has not been chosen yet gets none: an empty world has nothing to churn,
    // and whichever choice arrives sets the rate it needs.
    let churn = if choose_on_launch {
        0.0
    } else {
        args.effective_churn()
    };

    let (mut damage_tx, damage_rx) = futures::channel::mpsc::channel(1);
    let published_scene = scene.clone();
    let startup_publish = startup.clone();
    let world = k10s_world::spawn_world(
        world_seed,
        live_rx,
        scene.clone(),
        ctrl_rx,
        args.seed,
        churn,
        args.layout,
        {
            move || {
                if let Some(startup) = &startup_publish {
                    let snapshot = published_scene.load_full();
                    startup.scene_published(&snapshot);
                }
                let _ = damage_tx.try_send(());
            }
        },
    );
    if let Some(startup) = &startup {
        startup.world_spawned();
    }

    let launch = LaunchService::new(feed, ctrl_tx.clone(), &args);
    let connection_failed = Arc::new(AtomicBool::new(false));
    let cluster_reply = args.cluster.then(|| {
        let (reply, receive) = futures::channel::oneshot::channel();
        let startup = startup.clone();
        launch.connect(
            ConnectRequest {
                source: ScanRequest::Detected,
                context: args.context.clone(),
            },
            Box::new(move |outcome| {
                if matches!(&outcome, ConnectOutcome::Connected(_))
                    && let Some(startup) = &startup
                {
                    startup.content_ready();
                }
                if reply.send(outcome).is_err() {
                    eprintln!("k10s: the window closed before the cluster connection completed");
                }
            }),
        );
        receive
    });
    let chooser = launch.clone();
    let generated_reply = generate_after_launch.then(|| {
        let (reply, receive) = futures::channel::oneshot::channel();
        let startup = startup.clone();
        launch.generate(Box::new(move |outcome| {
            if let Some(startup) = &startup {
                startup.content_ready();
            }
            if reply.send(outcome).is_err() {
                eprintln!("k10s: the window closed before the generated scene was ready");
            }
        }));
        receive
    });

    let shutdown_tx = ctrl_tx.clone();
    let bench_meta = args.bench.then(|| BenchMeta {
        machine: args.machine_label(),
        churn: args.effective_churn(),
        arch: cli::platform(),
        objects: args.objects,
        seed: args.seed,
        layout: args.layout.as_str().to_string(),
        json: args.json,
    });
    let window_failed = Arc::new(AtomicBool::new(false));
    // A bench flight that gives up is a failed run, and it must fail the same way
    // everything else here does: after the world thread is joined and the plane is
    // retired, not from inside the frame that noticed.
    let bench_failed = Arc::new(AtomicBool::new(false));
    let bench_status = bench_failed.clone();
    let window_status = window_failed.clone();
    let connection_status = connection_failed.clone();
    // A bench flight runs on the default theme and default keymap, whatever
    // the user's files say: a recording's environment must not depend on the
    // recording machine's home directory.
    let config = if args.measuring() {
        ConfigFiles::none()
    } else {
        ConfigFiles::from_env()
    };
    // The same two paths the editor opens for ctrl-, and the keymap command,
    // so what the poller reloads and what the editor writes are one file.
    let config_paths = config.paths();
    // The first read happens before GPUI starts its event loop. Subsequent
    // reads are dispatched to the background executor by `watch_config`.
    let initial_config = config.read();
    // The X11 icon is a nicety and its absence is survivable; typography is
    // not, so only one of these two failures stops the launch.
    let icon = match k10s_assets::window_icon() {
        Ok(icon) => Some(Arc::new(icon)),
        Err(error) => {
            eprintln!("k10s: {error}; the window will use the desktop's default icon");
            None
        }
    };
    if let Some(startup) = &startup {
        startup.platform_started();
    }
    let startup_status = startup.clone();
    let present_probe = startup
        .as_ref()
        .map(|startup| startup.present_probe(!choose_on_launch));
    let startup_window = startup;
    gpui_platform::application()
        .with_assets(k10s_assets::Assets)
        .run(move |cx| {
            if let Some(startup) = &startup_window {
                startup.application_ready();
            }
            if let Err(error) = k10s_assets::register_fonts(cx) {
                eprintln!("k10s: {error}");
                // Typography is part of the visual contract. Running with a
                // platform fallback would look subtly wrong while presenting
                // itself as the same theme, so fail closed.
                window_status.store(true, Ordering::Relaxed);
                cx.quit();
                return;
            }
            if let Some(startup) = &startup_window {
                startup.fonts_ready();
            }
            config::apply_config(&initial_config, cx);
            config::watch_config(config, initial_config, cx);
            if let Some(startup) = &startup_window {
                startup.configuration_ready();
            }
            let bounds = Bounds::centered(None, size(px(1600.0), px(1000.0)), cx);
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("k10s - Starmap".into()),
                        ..Default::default()
                    }),
                    focus: true,
                    // X11 takes the icon directly; Wayland has no such
                    // protocol and matches the app id to `k10s.desktop`
                    // instead, which is why both are set and why the id must
                    // stay equal to that file's basename.
                    icon,
                    app_id: Some(k10s_assets::APP_ID.to_string()),
                    ..Default::default()
                },
                move |window, cx| {
                    let is_bench = bench_meta.is_some();
                    // Resolve against the appearance the window actually has,
                    // then follow it. A `"mode": "system"` theme sampled once
                    // at startup is a theme that stops following the desktop.
                    cx.set_global(DesktopAppearance(window.appearance().into()));
                    config::publish_theme(cx);
                    window
                        .observe_window_appearance(|window, cx| {
                            cx.set_global(DesktopAppearance(window.appearance().into()));
                            config::publish_theme(cx);
                            cx.refresh_windows();
                        })
                        .detach();
                    let map = cx.new(move |cx| {
                        let map = MapView::new(
                            scene.clone(),
                            ctrl_tx.clone(),
                            bench_meta,
                            bench_status.clone(),
                            damage_rx,
                            cx,
                        );
                        match present_probe {
                            Some(probe) => map.with_present_probe(probe),
                            None => map,
                        }
                    });
                    // The seam is an `Rc` for the shell and an `Arc` inside,
                    // because the connect happens on a thread and the screen
                    // asking for it does not.
                    let chooser =
                        Some(std::rc::Rc::new(chooser)
                            as std::rc::Rc<dyn k10s_shell::LaunchProvider>);
                    let workspace = cx.new(|cx| {
                        Workspace::new(
                            map,
                            is_bench,
                            !choose_on_launch,
                            None,
                            chooser,
                            config_paths.clone(),
                            cx,
                        )
                    });
                    let focus = workspace.read(cx).map_focus_handle(cx);
                    window.focus(&focus, cx);
                    // Nothing is in the world and nothing on the command line
                    // said what should be, so the screen asks. Opened after the
                    // map takes focus, because it takes it straight back.
                    if choose_on_launch {
                        workspace.update(cx, |workspace, cx| workspace.open_launch(window, cx));
                    }
                    if let Some(reply) = generated_reply {
                        workspace.update(cx, |workspace, cx| {
                            workspace.await_generated_scene(reply, cx)
                        });
                    }
                    if let Some(reply) = cluster_reply {
                        workspace.update(cx, |workspace, cx| {
                            let connection_status = connection_status.clone();
                            workspace.await_command_line_connection(
                                reply,
                                move || connection_status.store(true, Ordering::Relaxed),
                                cx,
                            )
                        });
                    }
                    if let Some(startup) = &startup_window {
                        let viewport = window.viewport_size();
                        startup
                            .window_built([f32::from(viewport.width), f32::from(viewport.height)]);
                    }
                    workspace
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
    // Whatever scene was attached, however it was chosen: the plane stops before
    // the thread carrying its stream, which is the order that lets a watch parked
    // on a full sink see a disconnect instead of a deadlock.
    launch.retire();
    if bench_failed.load(Ordering::Relaxed) {
        // Its own status, because a recording that did not happen and a window
        // that would not open are different answers to whatever ran this.
        std::process::exit(3);
    }
    if startup_status
        .as_ref()
        .is_some_and(|startup| startup.failed() || !startup.completed())
    {
        if startup_status
            .as_ref()
            .is_some_and(|startup| !startup.failed())
        {
            eprintln!("k10s: the startup benchmark ended before a useful frame was presented");
        }
        std::process::exit(4);
    }
    if !world_ended_cleanly
        || window_failed.load(Ordering::Relaxed)
        || connection_failed.load(Ordering::Relaxed)
    {
        std::process::exit(1);
    }
}

// A generated scene and the one line that describes it. The line used to go
// straight to stderr, which was fine when the generator was the only way in;
// now it is also what the launch screen puts in the status bar, because somebody
// who started from a desktop entry has no stderr to read.
pub struct Generated {
    pub scene: PreparedScene,
    pub summary: String,
}

fn generate(args: &cli::Args) -> Generated {
    let t0 = std::time::Instant::now();
    let spec = k10s_clustergen::generate(&GenConfig {
        seed: args.seed,
        target_objects: args.objects,
        scenario: args.scenario,
    });
    let summary = format!(
        "generated {} namespaces / {} workloads / {} pods / {} sats / {} edges (seed {}, scenario {}, layout {}) in {:.1?}",
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
    Generated {
        scene: k10s_clustergen::stream::prepared(spec, args.layout.emits_attachments()),
        summary,
    }
}

fn list_contexts() -> i32 {
    let (tx, _rx) = crossbeam_channel::bounded(1);
    let plane = match k10s_data::spawn(tx) {
        Ok(plane) => plane,
        Err(err) => {
            eprintln!("k10s: {err}");
            return 1;
        }
    };
    match plane.contexts() {
        Ok(contexts) if contexts.is_empty() => {
            eprintln!("k10s: the kubeconfig declares no contexts");
            1
        }
        Ok(contexts) => {
            for name in contexts {
                println!("{name}");
            }
            0
        }
        Err(err) => {
            eprintln!("k10s: {err}");
            1
        }
    }
}
