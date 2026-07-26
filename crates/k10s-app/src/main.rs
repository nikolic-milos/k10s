mod cli;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use k10s_clustergen::GenConfig;
use k10s_core::{Capability, IngestEvent, Intake, WorldCtrl, new_shared_scene};
use k10s_data::DataPlane;
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

/// Whatever is keeping the data plane alive, held for the life of the process.
///
/// Dropping the [`DataPlane`] drops the tokio runtime, which ends every watch, so
/// this is not an unused binding: it is the thing that keeps the cluster connected.
struct Live {
    _plane: DataPlane,
    drain: std::thread::JoinHandle<()>,
    stop: Arc<AtomicBool>,
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
    for flag in args.cluster_flags_without_cluster() {
        eprintln!("k10s: {flag} does nothing without --cluster");
    }
    if args.churn_was_overridden() {
        eprintln!("k10s: --churn is ignored with --cluster; the cluster supplies the churn");
    }

    if args.list_contexts {
        std::process::exit(list_contexts());
    }

    let (events, live) = if args.cluster {
        match connect_cluster(&args) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("k10s: {err}");
                std::process::exit(1);
            }
        }
    } else {
        (generate(&args), None)
    };

    let scene = new_shared_scene();
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();

    let (damage_tx, damage_rx) = futures::channel::mpsc::unbounded();
    // The app wires a producer to the world through the ingestion contract; the
    // world no longer knows either producer exists.
    let world = k10s_world::spawn_world(
        events,
        scene.clone(),
        ctrl_rx,
        args.seed,
        args.effective_churn(),
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
    if let Some(live) = live {
        live.stop.store(true, Ordering::Relaxed);
        let _ = live.drain.join();
    }
    if !world_ended_cleanly || window_failed.load(Ordering::Relaxed) {
        std::process::exit(1);
    }
}

fn generate(args: &cli::Args) -> Vec<IngestEvent> {
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
    k10s_clustergen::stream::snapshot(&spec, args.layout.emits_attachments())
}

fn list_contexts() -> i32 {
    // A channel nobody reads: listing contexts touches no cluster, so nothing is
    // ever sent.
    let (tx, _rx) = crossbeam_channel::unbounded();
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

/// Connects, syncs, and leaves the watches running.
fn connect_cluster(args: &cli::Args) -> Result<(Vec<IngestEvent>, Option<Live>), String> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let plane = k10s_data::spawn(tx).map_err(|e| format!("cannot start the data plane: {e}"))?;
    let options = k10s_data::Options {
        context: args.context.clone(),
        probe_namespaces: args.namespaces.clone(),
        sync_timeout: args.sync_timeout(),
    };
    let sync = plane.sync(&options).map_err(|e| e.to_string())?;

    eprintln!("k10s: {}", sync.report.summary());
    report_degradation(&sync);

    // The world folds a whole initial sync and has no incremental path yet, so live
    // events are drained and counted rather than applied. Phase D replaces the
    // discard with the world's own intake; until then this keeps the queue bounded
    // and makes the live path exercised rather than theoretical.
    let stop = Arc::new(AtomicBool::new(false));
    let drain = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("k10s-live-drain".into())
            .spawn(move || {
                // One tick's worth, drained into a reused buffer, exactly the way
                // the world thread will: the coalescing counters then mean the same
                // thing here as they will there.
                const TICK: std::time::Duration = std::time::Duration::from_millis(200);
                let mut intake = Intake::new();
                let mut batch = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    match rx.recv_timeout(TICK) {
                        Ok(event) => intake.push(event),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                    if !intake.is_empty() {
                        intake.drain_into(&mut batch);
                        batch.clear();
                    }
                }
            })
            .map_err(|e| format!("cannot start the live drain thread: {e}"))?
    };

    Ok((
        sync.events,
        Some(Live {
            _plane: plane,
            drain,
            stop,
        }),
    ))
}

/// Says out loud whatever the cluster would otherwise let us show as an empty map.
///
/// The failure mode the roadmap calls the worst kind is an RBAC-restricted cluster
/// where the app looks like it works. Every line here exists so that cannot happen
/// silently.
fn report_degradation(sync: &k10s_data::Sync) {
    let report = &sync.report;
    let name = |kind: k10s_core::KindId| {
        sync.catalog
            .kind(kind)
            .map(|e| e.slug.to_string())
            .unwrap_or_else(|| format!("kind {}", kind.0))
    };

    if report.probe_degraded {
        eprintln!(
            "k10s: the RBAC probe could not run, so every kind is attempted and a denial \
             will show up as a stream error instead of a label"
        );
    }
    if !report.aggregated_discovery {
        eprintln!(
            "k10s: this server has no aggregated discovery, so discovery cost one request per API group"
        );
    }

    let forbidden: Vec<String> = sync
        .events
        .iter()
        .filter_map(|e| match e {
            IngestEvent::Capability {
                kind,
                verdict: Capability::Forbidden,
            } => Some(name(*kind)),
            _ => None,
        })
        .collect();
    if !forbidden.is_empty() {
        eprintln!(
            "k10s: {} kinds are present but not readable by this account: {}",
            forbidden.len(),
            preview(&forbidden)
        );
    }

    if report.namespaced_streams > 0 {
        eprintln!(
            "k10s: cluster-wide list is denied for some kinds, so {} streams are scoped to the \
             namespaces given with --namespace",
            report.namespaced_streams
        );
    }
    if !report.unsettled.is_empty() {
        let names: Vec<String> = report.unsettled.iter().copied().map(name).collect();
        eprintln!(
            "k10s: {} kinds did not finish listing inside the timeout and are incomplete: {}",
            names.len(),
            preview(&names)
        );
    }
    for (kind, reason) in &report.desyncs {
        eprintln!("k10s: {} stream reported {reason:?}", name(*kind));
    }

    let stats = report.assemble;
    if stats.unattached > 0 {
        eprintln!(
            "k10s: {} attachments are not referenced by any workload and are not drawn yet",
            stats.unattached
        );
    }
    if stats.unknown_namespace > 0 {
        eprintln!(
            "k10s: {} objects are in namespaces this account cannot list and were left out",
            stats.unknown_namespace
        );
    }
    if stats.owner_cycles > 0 {
        eprintln!(
            "k10s: {} objects have a cyclic owner reference chain and were left out",
            stats.owner_cycles
        );
    }
    if stats.scopes == 0 {
        eprintln!(
            "k10s: no namespaces were readable, so the map is empty. This is a permissions \
             answer, not an empty cluster."
        );
    }
}

/// The first few names plus a count, so a cluster with two hundred denied kinds
/// does not print two hundred lines.
fn preview(names: &[String]) -> String {
    const SHOWN: usize = 6;
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{}, and {} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}
