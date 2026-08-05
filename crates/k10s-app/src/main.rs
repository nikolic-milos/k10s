mod cli;
mod launch;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use k10s_clustergen::GenConfig;
use k10s_core::{Capability, IngestEvent, WorldCtrl, new_shared_scene};
use k10s_data::read::Fetched;
use k10s_data::{DEFAULT_EVENT_SINK_CAPACITY, DataPlane};
use k10s_map::{BenchMeta, MapView};
use k10s_shell::Workspace;

use launch::{Feed, LaunchService};

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

// What a command line that named a cluster connected to, before the window
// opened. The screen's own connections are owned by `LaunchService` instead; both
// end up in the same place, which is why this only carries what main has to hand
// on rather than anything it keeps.
struct Live {
    events: crossbeam_channel::Receiver<IngestEvent>,
    inspector: k10s_data::inspect::Inspector,
    reader: k10s_data::read::Reader,
    plane: DataPlane,
}

// The shell's provider seam, adapted to the data plane. The shell never sees
// kube; it sees labelled outcomes. Every reply callback runs on the data
// plane's runtime and the shell bridges onto its own executor, so no thread
// is parked waiting for an answer.
pub struct PlaneProvider {
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
        continue_token: Option<String>,
        reply: k10s_shell::Reply<k10s_shell::TableOutcome>,
    ) {
        self.reader
            .fetch_table(kind, continue_token, move |fetched| {
                reply(table_outcome(fetched))
            });
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

    // Helm's release inventory, rendered on this side of the seam like a
    // describe is: the shell shows lines and holds no release payload of its
    // own, which is what keeps a payload that can carry secret material from
    // living in a view's state.
    fn fetch_releases(&self, reply: k10s_shell::Reply<k10s_shell::DocOutcome>) {
        self.reader.fetch_releases(None, move |fetched| {
            reply(match fetched {
                Fetched::Ok(releases) => k10s_shell::DocOutcome::Doc {
                    title: "helm releases".to_string(),
                    lines: k10s_data::helm::render(&releases),
                },
                Fetched::Denied { what } => k10s_shell::DocOutcome::Denied(what),
                Fetched::Failed { why, .. } => k10s_shell::DocOutcome::Failed(why),
            })
        });
    }

    fn fetch_manifest(
        &self,
        request: &k10s_shell::DescribeRequest,
        reply: k10s_shell::Reply<k10s_shell::ManifestOutcome>,
    ) {
        let request = k10s_data::describe::DescribeRequest {
            kind: request.kind,
            namespace: request.namespace.clone(),
            name: request.name.clone(),
            uid: request.uid.clone(),
        };
        self.reader.fetch_manifest(request, move |fetched| {
            reply(match fetched {
                Fetched::Ok(manifest) => k10s_shell::ManifestOutcome::Manifest {
                    title: manifest.title,
                    yaml: manifest.yaml,
                    api_version: manifest.api_version,
                    kind: manifest.kind,
                    last_applied: manifest.last_applied,
                    patchable: manifest.patchable,
                    status_subresource: manifest.status_subresource,
                    uid: manifest.uid,
                },
                Fetched::Denied { what } => k10s_shell::ManifestOutcome::Denied(what),
                Fetched::Failed { why, .. } => k10s_shell::ManifestOutcome::Failed(why),
            })
        });
    }

    fn apply(
        &self,
        request: &k10s_shell::ApplyRequest,
        reply: k10s_shell::Reply<k10s_shell::ApplyOutcome>,
    ) {
        use k10s_data::apply::ApplyOutcome;

        let request = k10s_data::apply::ApplyRequest {
            kind: request.kind,
            namespace: request.namespace.clone(),
            name: request.name.clone(),
            yaml: request.yaml.clone(),
            dry_run: request.dry_run,
            force: request.force,
        };
        self.reader.apply(request, move |outcome| {
            reply(match outcome {
                ApplyOutcome::Applied(applied) => k10s_shell::ApplyOutcome::Applied {
                    yaml: applied.yaml,
                    dry_run: applied.dry_run,
                    uid: applied.uid,
                },
                ApplyOutcome::Unrendered(unrendered) => k10s_shell::ApplyOutcome::Unrendered {
                    dry_run: unrendered.dry_run,
                    why: unrendered.why,
                },
                ApplyOutcome::Conflict {
                    message,
                    causes,
                    truncated,
                } => k10s_shell::ApplyOutcome::Conflict {
                    message,
                    causes: causes
                        .into_iter()
                        .map(|cause| k10s_shell::Conflicted {
                            field: cause.field,
                            manager: cause.manager,
                        })
                        .collect(),
                    truncated,
                },
                ApplyOutcome::Stale { message } => k10s_shell::ApplyOutcome::Stale { message },
                ApplyOutcome::Rejected { message, causes } => {
                    k10s_shell::ApplyOutcome::Rejected { message, causes }
                }
                ApplyOutcome::Denied { what, why } => {
                    k10s_shell::ApplyOutcome::Denied { what, why }
                }
                ApplyOutcome::Failed { why } => k10s_shell::ApplyOutcome::Failed(why),
            })
        });
    }

    fn fetch_schema_catalog(&self, reply: k10s_shell::Reply<k10s_shell::SchemaCatalogOutcome>) {
        self.reader.fetch_schema_catalog(move |fetched| {
            reply(match fetched {
                Fetched::Ok(sources) => k10s_shell::SchemaCatalogOutcome::Catalog(
                    sources
                        .into_iter()
                        .map(|source| k10s_shell::SchemaSource {
                            group_version: source.group_version,
                            url: source.url,
                        })
                        .collect(),
                ),
                Fetched::Denied { what } => k10s_shell::SchemaCatalogOutcome::Denied(what),
                Fetched::Failed { why, .. } => k10s_shell::SchemaCatalogOutcome::Failed(why),
            })
        });
    }

    fn fetch_schema_document(
        &self,
        url: &str,
        reply: k10s_shell::Reply<k10s_shell::SchemaTextOutcome>,
    ) {
        self.reader
            .fetch_schema_document(url.to_string(), move |fetched| {
                reply(schema_text_outcome(fetched))
            });
    }

    fn fetch_crd_schemas(&self, reply: k10s_shell::Reply<k10s_shell::SchemaTextOutcome>) {
        self.reader
            .fetch_crd_schemas(move |fetched| reply(schema_text_outcome(fetched)));
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
        let request = k10s_data::logs::LogRequest {
            namespace: request.namespace.clone(),
            pod: request.pod.clone(),
            container: request.container.clone(),
            previous: request.previous,
        };
        let stop = self
            .reader
            .follow_log(request, Box::new(move |chunk| on_chunk(adapt_chunk(chunk))));
        k10s_shell::LogStop::new(move || drop(stop))
    }

    fn follow_workload_logs(
        &self,
        request: &k10s_shell::WorkloadLogRequest,
        on_chunk: Box<dyn Fn(k10s_shell::LogChunk) + Send + Sync>,
    ) -> k10s_shell::LogStop {
        let request = k10s_data::logs::WorkloadLogRequest {
            namespace: request.namespace.clone(),
            kind: request.kind,
            name: request.name.clone(),
        };
        let stop = self
            .reader
            .follow_workload_logs(request, Box::new(move |chunk| on_chunk(adapt_chunk(chunk))));
        k10s_shell::LogStop::new(move || drop(stop))
    }

    fn open_forward(
        &self,
        request: &k10s_shell::ForwardRequest,
        reply: k10s_shell::Reply<k10s_shell::ForwardOutcome>,
    ) {
        let request = k10s_data::forward::ForwardRequest {
            namespace: request.namespace.clone(),
            name: request.name.clone(),
            service: request.service,
        };
        self.reader.open_forward(request, move |fetched| {
            reply(match fetched {
                Fetched::Ok(row) => k10s_shell::ForwardOutcome::Opened(adapt_forward(row)),
                Fetched::Denied { what } => k10s_shell::ForwardOutcome::Denied(what),
                Fetched::Failed { why, .. } => k10s_shell::ForwardOutcome::Failed(why),
            })
        });
    }

    fn list_forwards(&self) -> Vec<k10s_shell::ForwardRow> {
        self.reader
            .forwards()
            .list()
            .into_iter()
            .map(adapt_forward)
            .collect()
    }

    fn close_forward(&self, id: u64) -> bool {
        self.reader.forwards().close(id)
    }

    fn start_exec(
        &self,
        request: &k10s_shell::ExecRequest,
        on_event: Box<dyn Fn(k10s_shell::ExecEvent) + Send + Sync>,
    ) -> Box<dyn k10s_shell::ExecSession> {
        use k10s_data::exec::ExecEvent;
        let request = k10s_data::exec::ExecRequest {
            namespace: request.namespace.clone(),
            pod: request.pod.clone(),
            container: request.container.clone(),
            command: request.command.clone(),
        };
        let session = self.reader.start_exec(
            &request,
            Box::new(move |event| {
                on_event(match event {
                    ExecEvent::Output(bytes) => k10s_shell::ExecEvent::Output(bytes),
                    ExecEvent::Ended { why } => k10s_shell::ExecEvent::Ended(why),
                    ExecEvent::Denied { what } => k10s_shell::ExecEvent::Denied(what),
                    ExecEvent::Failed { why, .. } => k10s_shell::ExecEvent::Failed(why),
                })
            }),
        );
        Box::new(ExecSessionAdapter(session))
    }
}

// The data plane's session behind the shell's trait: same shape, different
// crate, so the shell never links kube.
struct ExecSessionAdapter(Box<dyn k10s_data::exec::ExecSession>);

impl k10s_shell::ExecSession for ExecSessionAdapter {
    fn write(&self, bytes: &[u8]) {
        self.0.write(bytes);
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.0.resize(cols, rows);
    }
}

fn adapt_forward(row: k10s_data::forward::ForwardRow) -> k10s_shell::ForwardRow {
    use k10s_data::forward::ForwardState;
    k10s_shell::ForwardRow {
        id: row.id,
        namespace: row.spec.namespace,
        pod: row.spec.pod,
        local_port: row.spec.local_port,
        remote_port: row.spec.remote_port,
        state: match row.state {
            ForwardState::Opening => k10s_shell::ForwardState::Opening,
            ForwardState::Active => k10s_shell::ForwardState::Active,
            ForwardState::Dead { why } => k10s_shell::ForwardState::Dead(why),
        },
    }
}

fn adapt_chunk(chunk: k10s_data::logs::LogChunk) -> k10s_shell::LogChunk {
    use k10s_data::logs::LogChunk;
    match chunk {
        LogChunk::Lines(lines) => k10s_shell::LogChunk::Lines(lines),
        LogChunk::Ended { why } => k10s_shell::LogChunk::Ended(why.to_string()),
        LogChunk::Denied { what } => k10s_shell::LogChunk::Denied(what),
        LogChunk::Failed { why, .. } => k10s_shell::LogChunk::Failed(why),
    }
}

fn schema_text_outcome(fetched: Fetched<String>) -> k10s_shell::SchemaTextOutcome {
    match fetched {
        Fetched::Ok(text) => k10s_shell::SchemaTextOutcome::Text(text),
        Fetched::Denied { what } => k10s_shell::SchemaTextOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::SchemaTextOutcome::Failed(why),
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
            continue_token: page.continue_token,
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

// The user's settings, keymap and themes, hot-reloaded by a content poll: no
// watcher dependency, no work under --bench, and all recurring file I/O runs
// on GPUI's background executor.
#[derive(Clone)]
struct ConfigFiles {
    settings: Option<std::path::PathBuf>,
    keymap: Option<std::path::PathBuf>,
    themes: Option<std::path::PathBuf>,
}

#[derive(PartialEq)]
struct ConfigText {
    settings: String,
    keymap: String,
    // Every `themes/*.json`, by file name so a note can say which file it came
    // from, and sorted so two files that both define a name resolve the same
    // way on every start.
    themes: Vec<(String, String)>,
}

impl ConfigFiles {
    fn none() -> ConfigFiles {
        ConfigFiles {
            settings: None,
            keymap: None,
            themes: None,
        }
    }

    fn from_env() -> ConfigFiles {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| std::path::PathBuf::from(home).join(".config"))
            })
            .map(|path| path.join("k10s"));
        match base {
            Some(dir) => ConfigFiles {
                settings: Some(dir.join("settings.json")),
                keymap: Some(dir.join("keymap.json")),
                themes: Some(dir.join("themes")),
            },
            None => ConfigFiles::none(),
        }
    }

    // The shell needs both paths present to offer the settings and keymap
    // commands; a platform with no config home offers neither.
    fn paths(&self) -> Option<k10s_shell::ConfigPaths> {
        Some(k10s_shell::ConfigPaths {
            settings: self.settings.clone()?,
            keymap: self.keymap.clone()?,
        })
    }

    fn read(&self) -> ConfigText {
        let read = |path: &Option<std::path::PathBuf>| {
            path.as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .unwrap_or_default()
        };
        ConfigText {
            settings: read(&self.settings),
            keymap: read(&self.keymap),
            themes: self.read_themes(),
        }
    }

    fn read_themes(&self) -> Vec<(String, String)> {
        let Some(dir) = &self.themes else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<(String, String)> = entries
            .flatten()
            .filter(|entry| {
                entry.path().extension().is_some_and(|ext| ext == "json")
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
            })
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                std::fs::read_to_string(entry.path())
                    .ok()
                    .map(|text| (name, text))
            })
            .collect();
        files.sort();
        files
    }

    fn watchable(&self) -> bool {
        self.settings.is_some() || self.keymap.is_some() || self.themes.is_some()
    }
}

// The window's light/dark appearance, held at app scope because k10s opens
// exactly one window and the theme is published as an app global. The window
// observer writes it on every `appearance_changed`; nothing samples it once at
// startup, because a desktop that switches at sunset must move the editor
// with it.
struct DesktopAppearance(k10s_theme::Appearance);

impl gpui::Global for DesktopAppearance {}

fn appearance(cx: &gpui::App) -> k10s_theme::Appearance {
    cx.try_global::<DesktopAppearance>()
        .map(|current| current.0)
        .unwrap_or_default()
}

fn apply_config(text: &ConfigText, cx: &mut gpui::App) {
    let mut registry = k10s_theme::ThemeRegistry::builtin();
    for (file, body) in &text.themes {
        let loaded = k10s_theme::parse_family(body);
        for note in &loaded.notes {
            eprintln!("k10s: themes/{file}: {note}");
        }
        if let Some(family) = loaded.family {
            registry.add_family(family);
        }
    }

    let mut loaded = k10s_shell::settings::parse(&text.settings);
    // The families the text system really has, asked after registration: an
    // unknown family must be a note, never gpui's silent platform fallback.
    let available = cx.text_system().all_font_names();
    loaded.notes.extend(k10s_shell::settings::resolve_families(
        &mut loaded.settings,
        &available,
    ));
    for note in &loaded.notes {
        eprintln!("k10s: {note}");
    }

    cx.set_global(k10s_theme::ActiveRegistry(std::sync::Arc::new(registry)));
    cx.set_global(k10s_theme::ActiveTypography(
        loaded.settings.typography.clone(),
    ));
    // Onto gpui itself, not only into our own global: gpui refreshes every
    // window when it changes and its own animation elements consult it, so
    // telling it once is what makes the setting mean the same thing everywhere.
    cx.set_reduce_motion(loaded.settings.reduce_motion);
    cx.set_global(k10s_shell::settings::ActiveSettings(loaded.settings));
    publish_theme(cx);

    let (parsed, notes) = k10s_shell::keymap::parse_keymap(&text.keymap);
    for note in &notes {
        eprintln!("k10s: {note}");
    }
    let (user_bindings, notes) = k10s_shell::keymap::build(&parsed, cx);
    for note in &notes {
        eprintln!("k10s: {note}");
    }
    let defaults = k10s_shell::keybindings();
    let input_suppressors =
        k10s_shell::input_suppressors(defaults.iter().chain(user_bindings.iter()));
    cx.clear_key_bindings();
    cx.bind_keys(defaults);
    // Bound after the defaults, so the user's file wins ties.
    cx.bind_keys(user_bindings);
    // Deeper input contexts must still capture any new plain Workspace key
    // introduced by the user's file. Explicit Palette/Typing/Terminal
    // bindings are detected above and remain authoritative.
    cx.bind_keys(input_suppressors);
}

// Resolve the settings' theme selection against the appearance the window is
// showing right now, patch it with any overrides, and publish it. Called on
// every settings reload and on every appearance change, because both of those
// can change the answer without changing the other.
fn publish_theme(cx: &mut gpui::App) {
    let settings = k10s_shell::settings::active(cx).clone();
    let registry = k10s_theme::registry(cx).clone();
    let appearance = appearance(cx);
    let name = settings.theme.name(appearance);
    let theme = match registry.get(name) {
        Some(theme) => theme.clone(),
        None => {
            let known: Vec<String> = registry
                .names()
                .into_iter()
                .map(|name| name.to_string())
                .collect();
            eprintln!(
                "k10s: settings name an unknown theme {name:?}; themes: {}",
                known.join(", ")
            );
            registry.default_for(appearance).clone()
        }
    };
    let theme = if settings.theme_overrides.is_empty() {
        theme
    } else {
        let mut patched = (*theme).clone();
        settings.theme_overrides.apply(&mut patched);
        std::sync::Arc::new(patched)
    };
    cx.set_global(k10s_theme::ActiveTheme(theme));
}

fn watch_config(config: ConfigFiles, mut last: ConfigText, cx: &mut gpui::App) {
    if !config.watchable() {
        return;
    }
    let background = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        loop {
            background.timer(std::time::Duration::from_secs(2)).await;
            // File I/O never belongs on GPUI's foreground executor. Reading
            // two tiny files is cheap, but a stalled network-mounted home
            // directory otherwise turns a harmless settings poll into a
            // visible frame hitch.
            let reader = config.clone();
            let now = background.spawn(async move { reader.read() }).await;
            if now != last {
                cx.update(|cx| {
                    apply_config(&now, cx);
                    cx.refresh_windows();
                });
                last = now;
            }
        }
    })
    .detach();
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

    // A command line that named a cluster still connects before the window
    // opens: somebody who said what they wanted has said it, and failing at a
    // prompt is the right answer to `--context prd`. Everything else opens the
    // window first and asks.
    let (events, live) = if args.cluster {
        match connect_cluster(&args) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("k10s: {err}");
                std::process::exit(1);
            }
        }
    } else if args.scene_was_named() {
        let generated = generate(&args);
        eprintln!("k10s: {}", generated.summary);
        (generated.events, None)
    } else {
        (Vec::new(), None)
    };
    let choose_on_launch = !args.scene_was_named();

    let scene = new_shared_scene();
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded(WORLD_CONTROL_CAPACITY);
    // One live channel for the whole process, created before the world so a
    // cluster connected after the window is open has somewhere to send its
    // deltas. The scene itself never travels down it -- a choice sends
    // `WorldCtrl::Rebuild` and carries its own stream -- so under `--bench` and
    // `--cluster` nothing changes except that the channel exists.
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
    let world = k10s_world::spawn_world(
        events,
        live_rx,
        scene.clone(),
        ctrl_rx,
        args.seed,
        churn,
        args.layout,
        {
            move || {
                let _ = damage_tx.try_send(());
            }
        },
    );

    let launch = LaunchService::new(feed, ctrl_tx.clone(), &args);
    let plane = live.map(|live| {
        let provider = (live.inspector, live.reader);
        launch.adopt_command_line(live.plane, live.events);
        provider
    });
    let chooser = launch.clone();

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
    // A bench flight runs on the default theme and default keymap, whatever
    // the user's files say: a recording's environment must not depend on the
    // recording machine's home directory.
    let config = if args.bench {
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
    gpui_platform::application()
        .with_assets(k10s_assets::Assets)
        .run(move |cx| {
            if let Err(error) = k10s_assets::register_fonts(cx) {
                eprintln!("k10s: {error}");
                // Typography is part of the visual contract. Running with a
                // platform fallback would look subtly wrong while presenting
                // itself as the same theme, so fail closed.
                window_status.store(true, Ordering::Relaxed);
                cx.quit();
                return;
            }
            apply_config(&initial_config, cx);
            watch_config(config, initial_config, cx);
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
                |window, cx| {
                    let is_bench = bench_meta.is_some();
                    // Resolve against the appearance the window actually has,
                    // then follow it. A `"mode": "system"` theme sampled once
                    // at startup is a theme that stops following the desktop.
                    cx.set_global(DesktopAppearance(window.appearance().into()));
                    publish_theme(cx);
                    window
                        .observe_window_appearance(|window, cx| {
                            cx.set_global(DesktopAppearance(window.appearance().into()));
                            publish_theme(cx);
                            cx.refresh_windows();
                        })
                        .detach();
                    let map = cx.new(|cx| {
                        MapView::new(
                            scene.clone(),
                            ctrl_tx.clone(),
                            bench_meta,
                            bench_status.clone(),
                            damage_rx,
                            cx,
                        )
                    });
                    let provider = plane.clone().map(|(inspector, reader)| {
                        std::rc::Rc::new(PlaneProvider { inspector, reader })
                            as std::rc::Rc<dyn k10s_shell::ReadProvider>
                    });
                    // The seam is an `Rc` for the shell and an `Arc` inside,
                    // because the connect happens on a thread and the screen
                    // asking for it does not.
                    let chooser =
                        Some(std::rc::Rc::new(chooser)
                            as std::rc::Rc<dyn k10s_shell::LaunchProvider>);
                    let workspace = cx.new(|cx| {
                        Workspace::new(map, is_bench, provider, chooser, config_paths.clone(), cx)
                    });
                    let focus = workspace.read(cx).map_focus_handle(cx);
                    window.focus(&focus, cx);
                    // Nothing is in the world and nothing on the command line
                    // said what should be, so the screen asks. Opened after the
                    // map takes focus, because it takes it straight back.
                    if choose_on_launch {
                        workspace.update(cx, |workspace, cx| workspace.open_launch(window, cx));
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
    if !world_ended_cleanly || window_failed.load(Ordering::Relaxed) {
        std::process::exit(1);
    }
}

// A generated scene and the one line that describes it. The line used to go
// straight to stderr, which was fine when the generator was the only way in;
// now it is also what the launch screen puts in the status bar, because somebody
// who started from a desktop entry has no stderr to read.
pub struct Generated {
    pub events: Vec<IngestEvent>,
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
        events: k10s_clustergen::stream::snapshot(&spec, args.layout.emits_attachments()),
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

fn connect_cluster(args: &cli::Args) -> Result<(Vec<IngestEvent>, Option<Live>), String> {
    let (tx, rx) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
    let plane = k10s_data::spawn(tx).map_err(|e| format!("cannot start the data plane: {e}"))?;
    let options = k10s_data::Options {
        context: args.context.clone(),
        kubeconfig: None,
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
            plane,
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
