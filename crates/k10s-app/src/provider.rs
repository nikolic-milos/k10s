//! The shell's provider seam, adapted to the data plane.
//!
//! The shell never sees kube; it sees labelled outcomes. Every reply callback
//! runs on the data plane's runtime and the shell bridges onto its own
//! executor, so no thread is parked waiting for an answer. Each reply is
//! translated by a named function rather than a closure, so the mapping from
//! a data-plane answer to a shell outcome is a value a test can call.

use std::sync::Arc;

use k10s_core::Capability;
use k10s_data::read::Fetched;

pub(crate) struct PlaneProvider {
    inspector: k10s_data::inspect::Inspector,
    reader: k10s_data::read::Reader,
}

impl PlaneProvider {
    pub(crate) fn new(
        inspector: k10s_data::inspect::Inspector,
        reader: k10s_data::read::Reader,
    ) -> PlaneProvider {
        PlaneProvider { inspector, reader }
    }
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
        self.reader
            .fetch_describe(request, move |fetched| reply(describe_outcome(fetched)));
    }

    // Helm's release inventory, rendered on this side of the seam like a
    // describe is: the shell shows lines and holds no release payload of its
    // own, which is what keeps a payload that can carry secret material from
    // living in a view's state.
    fn fetch_releases(&self, reply: k10s_shell::Reply<k10s_shell::DocOutcome>) {
        self.reader
            .fetch_releases(None, move |fetched| reply(releases_outcome(fetched)));
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
        self.reader
            .fetch_manifest(request, move |fetched| reply(manifest_outcome(fetched)));
    }

    fn apply(
        &self,
        request: &k10s_shell::ApplyRequest,
        reply: k10s_shell::Reply<k10s_shell::ApplyOutcome>,
    ) {
        let request = k10s_data::apply::ApplyRequest {
            kind: request.kind,
            namespace: request.namespace.clone(),
            name: request.name.clone(),
            yaml: request.yaml.clone(),
            dry_run: request.dry_run,
            force: request.force,
        };
        self.reader
            .apply(request, move |outcome| reply(apply_outcome(outcome)));
    }

    fn fetch_schema_catalog(&self, reply: k10s_shell::Reply<k10s_shell::SchemaCatalogOutcome>) {
        self.reader
            .fetch_schema_catalog(move |fetched| reply(schema_catalog_outcome(fetched)));
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
                reply(containers_outcome(fetched))
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
        self.reader
            .open_forward(request, move |fetched| reply(forward_outcome(fetched)));
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
        let request = k10s_data::exec::ExecRequest {
            namespace: request.namespace.clone(),
            pod: request.pod.clone(),
            container: request.container.clone(),
            command: request.command.clone(),
        };
        let session = self
            .reader
            .start_exec(&request, Box::new(move |event| on_event(exec_event(event))));
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

fn describe_outcome(fetched: Fetched<k10s_data::describe::Described>) -> k10s_shell::DocOutcome {
    match fetched {
        Fetched::Ok(described) => k10s_shell::DocOutcome::Doc {
            title: described.title,
            lines: described.lines,
        },
        Fetched::Denied { what } => k10s_shell::DocOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::DocOutcome::Failed(why),
    }
}

fn releases_outcome(fetched: Fetched<k10s_data::helm::Releases>) -> k10s_shell::DocOutcome {
    match fetched {
        Fetched::Ok(releases) => k10s_shell::DocOutcome::Doc {
            title: "helm releases".to_string(),
            lines: k10s_data::helm::render(&releases),
        },
        Fetched::Denied { what } => k10s_shell::DocOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::DocOutcome::Failed(why),
    }
}

fn manifest_outcome(
    fetched: Fetched<k10s_data::manifest::Manifest>,
) -> k10s_shell::ManifestOutcome {
    match fetched {
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
    }
}

fn apply_outcome(outcome: k10s_data::apply::ApplyOutcome) -> k10s_shell::ApplyOutcome {
    use k10s_data::apply::ApplyOutcome;
    match outcome {
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
        ApplyOutcome::Denied { what, why } => k10s_shell::ApplyOutcome::Denied { what, why },
        ApplyOutcome::Failed { why } => k10s_shell::ApplyOutcome::Failed(why),
    }
}

fn schema_catalog_outcome(
    fetched: Fetched<Vec<k10s_data::openapi::SchemaSource>>,
) -> k10s_shell::SchemaCatalogOutcome {
    match fetched {
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
    }
}

fn containers_outcome(fetched: Fetched<Vec<String>>) -> k10s_shell::ContainersOutcome {
    match fetched {
        Fetched::Ok(containers) => k10s_shell::ContainersOutcome::Containers(containers),
        Fetched::Denied { what } => k10s_shell::ContainersOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::ContainersOutcome::Failed(why),
    }
}

fn forward_outcome(fetched: Fetched<k10s_data::forward::ForwardRow>) -> k10s_shell::ForwardOutcome {
    match fetched {
        Fetched::Ok(row) => k10s_shell::ForwardOutcome::Opened(adapt_forward(row)),
        Fetched::Denied { what } => k10s_shell::ForwardOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::ForwardOutcome::Failed(why),
    }
}

fn exec_event(event: k10s_data::exec::ExecEvent) -> k10s_shell::ExecEvent {
    use k10s_data::exec::ExecEvent;
    match event {
        ExecEvent::Output(bytes) => k10s_shell::ExecEvent::Output(bytes),
        ExecEvent::Ended { why } => k10s_shell::ExecEvent::Ended(why),
        ExecEvent::Denied { what } => k10s_shell::ExecEvent::Denied(what),
        ExecEvent::Failed { why, .. } => k10s_shell::ExecEvent::Failed(why),
    }
}

#[cfg(test)]
#[path = "provider_test.rs"]
mod tests;
