mod cli;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use k10s_clustergen::GenConfig;
use k10s_core::{Capability, IngestEvent, WorldCtrl, new_shared_scene};
use k10s_data::read::Fetched;
use k10s_data::{DEFAULT_EVENT_SINK_CAPACITY, DataPlane};
use k10s_map::{BenchMeta, MapView};
use k10s_shell::Workspace;

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

struct Live {
    // Field order is load-bearing: the receiver must drop before the plane, so
    // a watch task blocked on a full bounded sink gets a disconnect error
    // instead of deadlocking the runtime shutdown that plane's drop waits on.
    events: crossbeam_channel::Receiver<IngestEvent>,
    inspector: k10s_data::inspect::Inspector,
    reader: k10s_data::read::Reader,
    _plane: DataPlane,
}

// The shell's provider seam, adapted to the data plane. The shell never sees
// kube; it sees labelled outcomes. Every reply callback runs on the data
// plane's runtime and the shell bridges onto its own executor, so no thread
// is parked waiting for an answer.
struct PlaneProvider {
    inspector: k10s_data::inspect::Inspector,
    reader: k10s_data::read::Reader,
}

impl k10s_shell::ReadProvider for PlaneProvider {
    fn fetch_events(
        &self,
        namespace: &str,
        name: &str,
        reply: k10s_shell::Reply<k10s_shell::Detail>,
    ) {
        self.inspector
            .fetch_events(namespace, name, move |detail| reply(adapt(detail)));
    }

    fn fetch_log_tail(
        &self,
        namespace: &str,
        pod: &str,
        reply: k10s_shell::Reply<k10s_shell::Detail>,
    ) {
        self.inspector
            .fetch_log_tail(namespace, &Arc::from(pod), move |detail| {
                reply(adapt(detail))
            });
    }

    fn kinds(&self) -> Vec<k10s_shell::KindRow> {
        self.reader
            .kinds()
            .into_iter()
            .map(|row| k10s_shell::KindRow {
                id: row.id,
                display: row.display,
                kind: row.kind,
                namespaced: row.namespaced,
                forbidden: row.verdict == Some(Capability::Forbidden),
            })
            .collect()
    }

    fn fetch_table(
        &self,
        kind: k10s_core::KindId,
        reply: k10s_shell::Reply<k10s_shell::TableOutcome>,
    ) {
        self.reader
            .fetch_table(kind, move |fetched| reply(table_outcome(fetched)));
    }

    fn fetch_node_table(&self, reply: k10s_shell::Reply<k10s_shell::TableOutcome>) {
        self.reader
            .fetch_node_table(move |fetched| reply(table_outcome(fetched)));
    }

    fn fetch_describe(
        &self,
        request: &k10s_shell::DescribeRequest,
        reply: k10s_shell::Reply<k10s_shell::DocOutcome>,
    ) {
        let request = k10s_data::describe::DescribeRequest {
            kind: request.kind,
            namespace: request.namespace.clone(),
            name: request.name.clone(),
            uid: request.uid.clone(),
        };
        self.reader.fetch_describe(request, move |fetched| {
            reply(match fetched {
                Fetched::Ok(described) => k10s_shell::DocOutcome::Doc {
                    title: described.title,
                    lines: described.lines,
                },
                Fetched::Denied { what } => k10s_shell::DocOutcome::Denied(what),
                Fetched::Failed { why, .. } => k10s_shell::DocOutcome::Failed(why),
            })
        });
    }

    fn fetch_containers(
        &self,
        namespace: &str,
        pod: &str,
        reply: k10s_shell::Reply<k10s_shell::ContainersOutcome>,
    ) {
        self.reader
            .fetch_containers(namespace, pod, move |fetched| {
                reply(match fetched {
                    Fetched::Ok(containers) => {
                        k10s_shell::ContainersOutcome::Containers(containers)
                    }
                    Fetched::Denied { what } => k10s_shell::ContainersOutcome::Denied(what),
                    Fetched::Failed { why, .. } => k10s_shell::ContainersOutcome::Failed(why),
                })
            });
    }

    fn follow_log(
        &self,
        request: &k10s_shell::LogRequest,
        on_chunk: Box<dyn Fn(k10s_shell::LogChunk) + Send + Sync>,
    ) -> k10s_shell::LogStop {
        use k10s_data::logs::LogChunk;
        let request = k10s_data::logs::LogRequest {
            namespace: request.namespace.clone(),
            pod: request.pod.clone(),
            container: request.container.clone(),
            previous: request.previous,
        };
        let stop = self.reader.follow_log(
            request,
            Box::new(move |chunk| {
                on_chunk(match chunk {
                    LogChunk::Lines(lines) => k10s_shell::LogChunk::Lines(lines),
                    LogChunk::Ended { why } => k10s_shell::LogChunk::Ended(why.to_string()),
                    LogChunk::Denied { what } => k10s_shell::LogChunk::Denied(what),
                    LogChunk::Failed { why, .. } => k10s_shell::LogChunk::Failed(why),
                })
            }),
        );
        k10s_shell::LogStop::new(move || drop(stop))
    }
}

fn table_outcome(fetched: Fetched<k10s_data::browse::TablePage>) -> k10s_shell::TableOutcome {
    match fetched {
        Fetched::Ok(page) => k10s_shell::TableOutcome::Table(k10s_shell::TablePage {
            columns: page
                .columns
                .into_iter()
                .map(|column| k10s_shell::TableColumn {
                    name: column.name,
                    wide: column.wide,
                })
                .collect(),
            rows: page
                .rows
                .into_iter()
                .map(|row| k10s_shell::TableRow {
                    cells: row.cells,
                    name: row.name,
                    namespace: row.namespace,
                    uid: row.uid,
                })
                .collect(),
            truncated: page.truncated,
        }),
        Fetched::Denied { what } => k10s_shell::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::TableOutcome::Failed(why),
    }
}

fn adapt(detail: k10s_data::inspect::InspectDetail) -> k10s_shell::Detail {
    use k10s_data::inspect::InspectDetail;
    match detail {
        InspectDetail::Events(lines) => k10s_shell::Detail::Events(
            lines
                .into_iter()
                .map(|line| k10s_shell::EventRow {
                    when: line.last_seen,
                    kind: line.kind,
                    reason: line.reason,
                    message: line.message,
                    count: line.count,
                })
                .collect(),
        ),
        InspectDetail::Log(tail) => k10s_shell::Detail::Log(tail.lines),
        InspectDetail::Denied { what } => k10s_shell::Detail::Denied(what),
        InspectDetail::Failed { why, .. } => k10s_shell::Detail::Failed(why),
    }
}

const WORLD_CONTROL_CAPACITY: usize = 64;

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
    for flag in args.generator_flags_with_cluster() {
        eprintln!("k10s: {flag} does nothing with --cluster; the cluster is the scene");
    }
    if args.churn_was_overridden() {
        eprintln!("k10s: --churn is ignored with --cluster; the cluster supplies the churn");
    }
    if args.machine.is_some() && !args.bench {
        eprintln!("k10s: --machine does nothing without --bench");
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
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded(WORLD_CONTROL_CAPACITY);
    let live_events = live
        .as_ref()
        .map(|connection| connection.events.clone())
        .unwrap_or_else(crossbeam_channel::never);

    let (mut damage_tx, damage_rx) = futures::channel::mpsc::channel(1);
    let world = k10s_world::spawn_world(
        events,
        live_events,
        scene.clone(),
        ctrl_rx,
        args.seed,
        args.effective_churn(),
        args.layout,
        {
            move || {
                let _ = damage_tx.try_send(());
            }
        },
    );

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
    let window_status = window_failed.clone();
    let plane = live
        .as_ref()
        .map(|live| (live.inspector.clone(), live.reader.clone()));
    gpui_platform::application().run(move |cx| {
        cx.bind_keys(k10s_shell::keybindings());
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
                let is_bench = bench_meta.is_some();
                let map = cx.new(|cx| {
                    MapView::new(scene.clone(), ctrl_tx.clone(), bench_meta, damage_rx, cx)
                });
                let provider = plane.clone().map(|(inspector, reader)| {
                    std::rc::Rc::new(PlaneProvider { inspector, reader })
                        as std::rc::Rc<dyn k10s_shell::ReadProvider>
                });
                let workspace = cx.new(|cx| Workspace::new(map, is_bench, provider, cx));
                let focus = workspace.read(cx).map_focus_handle(cx);
                window.focus(&focus, cx);
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
    drop(live);
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

fn connect_cluster(args: &cli::Args) -> Result<(Vec<IngestEvent>, Option<Live>), String> {
    let (tx, rx) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
    let plane = k10s_data::spawn(tx).map_err(|e| format!("cannot start the data plane: {e}"))?;
    let options = k10s_data::Options {
        context: args.context.clone(),
        probe_namespaces: args.namespaces.clone(),
        sync_timeout: args.sync_timeout(),
    };
    let sync = plane.sync(&options).map_err(|e| e.to_string())?;

    eprintln!("k10s: {}", sync.report.summary());
    report_degradation(&sync);

    Ok((
        sync.events,
        Some(Live {
            events: rx,
            inspector: sync.inspector,
            reader: sync.reader,
            _plane: plane,
        }),
    ))
}

fn report_degradation(sync: &k10s_data::Sync) {
    for note in degradation_notes(&sync.report, &sync.catalog, &sync.events) {
        eprintln!("k10s: {note}");
    }
}

fn degradation_notes(
    report: &k10s_data::ClusterReport,
    catalog: &k10s_core::Catalog,
    events: &[IngestEvent],
) -> Vec<String> {
    let name = |kind: k10s_core::KindId| {
        catalog
            .kind(kind)
            .map(|e| e.slug.to_string())
            .unwrap_or_else(|| format!("kind {}", kind.0))
    };
    let mut notes = Vec::new();

    if report.probe_degraded {
        notes.push(
            "the RBAC probe could not run, so every kind is attempted and a denial will show \
             up as a stream error instead of a label"
                .to_string(),
        );
    }
    if report.kinds_unanswered > 0 {
        notes.push(format!(
            "{} kinds got no answer from their cluster-wide access review, so they are \
             attempted rather than gated and a denial on one will show up as a stream error",
            report.kinds_unanswered
        ));
    }
    if !report.namespaces_unanswered.is_empty() {
        notes.push(format!(
            "the rules review for {} got no answer, so denied kinds are still attempted \
             there and a real denial will show up as a stream error instead of an empty map",
            report.namespaces_unanswered.join(", ")
        ));
    }
    if !report.aggregated_discovery {
        notes.push(
            "this server has no aggregated discovery, so discovery cost one request per API group"
                .to_string(),
        );
    }

    let forbidden: Vec<String> = events
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
        notes.push(format!(
            "{} kinds are present but not readable by this account: {}",
            forbidden.len(),
            preview(&forbidden)
        ));
        notes.push(match report.probed_namespaces.as_slice() {
            [] => "no namespace was checked for a narrower grant; --namespace NS adds one to \
                   the probe"
                .to_string(),
            probed => format!(
                "the only namespaces checked for a narrower grant were {}; --namespace NS adds \
                 one to the probe",
                preview(probed)
            ),
        });
    }

    if report.namespaced_streams > 0 {
        notes.push(format!(
            "{} of {} streams are scoped to one namespace rather than to the cluster",
            report.namespaced_streams, report.streams
        ));
    }
    if !report.unsettled.is_empty() {
        let names: Vec<String> = report.unsettled.iter().copied().map(name).collect();
        notes.push(format!(
            "{} kinds did not finish listing inside the timeout and are incomplete: {}",
            names.len(),
            preview(&names)
        ));
    }
    for (kind, reason) in &report.desyncs {
        notes.push(format!("{} stream reported {reason:?}", name(*kind)));
    }

    let stats = report.assemble;
    if stats.unattached > 0 {
        notes.push(format!(
            "{} attachments are not referenced by any workload and are not drawn yet",
            stats.unattached
        ));
    }
    if stats.unknown_namespace > 0 {
        notes.push(format!(
            "{} objects are in namespaces this account cannot list and were left out",
            stats.unknown_namespace
        ));
    }
    if stats.owner_cycles > 0 {
        notes.push(format!(
            "{} objects have a cyclic owner reference chain and were left out",
            stats.owner_cycles
        ));
    }
    if stats.scopes == 0 {
        notes.push(
            "no namespaces were readable, so the map is empty. This is a permissions answer, \
             not an empty cluster."
                .to_string(),
        );
    }
    notes
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{Catalog, KindId};
    use k10s_data::{ClusterReport, assemble::AssembleStats};

    fn readable() -> ClusterReport {
        ClusterReport {
            aggregated_discovery: true,
            assemble: AssembleStats {
                scopes: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn notes_for(report: ClusterReport, events: Vec<IngestEvent>) -> Vec<String> {
        degradation_notes(&report, &Catalog::new(), &events)
    }

    fn forbidden(kind: KindId) -> IngestEvent {
        IngestEvent::Capability {
            kind,
            verdict: Capability::Forbidden,
        }
    }

    #[test]
    fn a_namespace_scoped_stream_is_stated_rather_than_explained() {
        let notes = notes_for(
            ClusterReport {
                streams: 4,
                namespaced_streams: 3,
                ..readable()
            },
            Vec::new(),
        );
        assert_eq!(
            notes,
            vec!["3 of 4 streams are scoped to one namespace rather than to the cluster"],
            "{notes:?}"
        );
    }

    #[test]
    fn a_forbidden_kind_names_the_namespaces_that_were_checked() {
        let notes = notes_for(
            ClusterReport {
                probed_namespaces: vec!["default".into()],
                ..readable()
            },
            vec![forbidden(KindId::SECRET), forbidden(KindId::DEPLOYMENT)],
        );
        assert!(notes.iter().any(|n| n.starts_with("2 kinds are present")));
        let hint = notes
            .iter()
            .find(|n| n.contains("--namespace"))
            .unwrap_or_else(|| panic!("{notes:?}"));
        assert!(hint.contains("default"), "{hint}");

        let unprobed = notes_for(readable(), vec![forbidden(KindId::SECRET)]);
        assert!(
            unprobed
                .iter()
                .any(|n| n.starts_with("no namespace was checked")),
            "{unprobed:?}"
        );
    }

    #[test]
    fn an_unanswered_review_is_reported_apart_from_a_probe_that_could_not_run() {
        let notes = notes_for(
            ClusterReport {
                kinds_unanswered: 2,
                ..readable()
            },
            Vec::new(),
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("2 kinds got no answer"), "{notes:?}");

        let degraded = notes_for(
            ClusterReport {
                probe_degraded: true,
                kinds_unanswered: 2,
                ..readable()
            },
            Vec::new(),
        );
        assert_eq!(degraded.len(), 2, "{degraded:?}");
        assert!(degraded[0].contains("could not run"));
    }
}
