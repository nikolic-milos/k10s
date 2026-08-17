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

    // Helm's release inventory as a table. The shell never holds a release
    // payload: columns are identity, revision, status, chart, and nothing
    // that could carry values or a manifest.
    fn fetch_releases(&self, reply: k10s_shell::Reply<k10s_shell::TableOutcome>) {
        self.reader
            .fetch_releases(None, move |fetched| reply(releases_outcome(fetched)));
    }

    fn fetch_argo(&self, reply: k10s_shell::Reply<k10s_shell::TableOutcome>) {
        self.reader
            .fetch_argo(move |fetched| reply(argo_outcome(fetched)));
    }

    fn fetch_flux(&self, reply: k10s_shell::Reply<k10s_shell::TableOutcome>) {
        self.reader
            .fetch_flux(move |fetched| reply(flux_outcome(fetched)));
    }

    fn run_day2(
        &self,
        request: &k10s_shell::Day2Request,
        reply: k10s_shell::Reply<k10s_shell::Day2Outcome>,
    ) {
        let call = day2_call(request);
        self.reader.day2(request.kind, call, move |outcome| {
            reply(day2_outcome(outcome))
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

    fn poll_usage(
        &self,
        request: &k10s_shell::UsageRequest,
        on_update: Box<dyn Fn(k10s_shell::UsageOutcome) + Send + Sync>,
    ) -> k10s_shell::LogStop {
        let request = k10s_data::metrics::UsageRequest {
            namespace: request.namespace.clone(),
            target: match &request.target {
                k10s_shell::UsageTarget::Pod { name } => {
                    k10s_data::metrics::UsageTarget::Pod { name: name.clone() }
                }
                k10s_shell::UsageTarget::Workload { kind, name } => {
                    k10s_data::metrics::UsageTarget::Workload {
                        kind: *kind,
                        name: name.clone(),
                    }
                }
            },
            // The cadence is policy and it lives here, not with the view.
            interval: k10s_data::metrics::USAGE_POLL_INTERVAL,
        };
        let stop = self.reader.poll_usage(
            request,
            Box::new(move |outcome| on_update(usage_outcome(outcome))),
        );
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

    fn fetch_overlay(
        &self,
        kind: k10s_map::OverlayKind,
        reply: k10s_shell::Reply<k10s_shell::OverlayOutcome>,
    ) {
        self.reader.fetch_overlay(
            overlay_kind(kind),
            k10s_data::reach::ReachSettings::default(),
            move |fetched| reply(overlay_outcome(fetched)),
        );
    }

    fn fetch_pod_posture(
        &self,
        namespace: &str,
        name: &str,
        reply: k10s_shell::Reply<k10s_shell::PostureOutcome>,
    ) {
        self.reader
            .fetch_pod_posture(namespace.to_string(), name.to_string(), move |fetched| {
                reply(posture_outcome(fetched))
            });
    }

    fn fetch_grafana(&self, reply: k10s_shell::Reply<k10s_shell::GrafanaOutcome>) {
        self.reader
            .fetch_grafana_catalog(move |fetched| reply(grafana_outcome(fetched)));
    }

    fn probe_observe(&self, reply: k10s_shell::Reply<k10s_shell::ObserveReach>) {
        self.reader.probe_observe_tools(move |tools| {
            reply(k10s_shell::ObserveReach {
                prometheus: tool_presence(tools.prometheus),
                loki: tool_presence(tools.loki),
                traces: tool_presence(tools.traces),
            });
        });
    }

    fn query_promql(&self, expr: String, reply: k10s_shell::Reply<k10s_shell::PromOutcome>) {
        let end = unix_secs();
        let start = end - k10s_data::overlay::RANGE_SECS;
        self.reader.query_prometheus(
            expr,
            start,
            end,
            k10s_data::overlay::STEP.to_string(),
            move |fetched| reply(prom_outcome(fetched)),
        );
    }

    fn query_loki(&self, query: String, reply: k10s_shell::Reply<k10s_shell::LokiOutcome>) {
        let end_ns = unix_nanos();
        let start_ns = end_ns.saturating_sub(3_600 * 1_000_000_000);
        self.reader.query_loki(
            k10s_data::loki::RangeQuery {
                query,
                start_ns,
                end_ns,
                limit: 0,
            },
            move |fetched| reply(loki_outcome(fetched)),
        );
    }

    fn lookup_trace(&self, trace_id: String, reply: k10s_shell::Reply<k10s_shell::TraceOutcome>) {
        self.reader
            .lookup_trace(trace_id, move |fetched| reply(trace_outcome(fetched)));
    }

    fn fetch_policy(&self, reply: k10s_shell::Reply<k10s_shell::TableOutcome>) {
        self.reader
            .fetch_policy_reports(move |fetched| reply(policy_outcome(fetched)));
    }

    fn fetch_harbor(&self, reply: k10s_shell::Reply<k10s_shell::TableOutcome>) {
        self.reader
            .fetch_harbor(move |fetched| reply(harbor_outcome(fetched)));
    }

    fn fetch_mesh(&self, reply: k10s_shell::Reply<k10s_shell::TableOutcome>) {
        self.reader
            .fetch_mesh_declared(move |inventory| reply(mesh_outcome(inventory)));
    }

    fn fetch_ecosystem(&self, reply: k10s_shell::Reply<Vec<k10s_shell::EcosystemEntry>>) {
        self.reader.fetch_ecosystem(move |families| {
            reply(
                families
                    .into_iter()
                    .map(|family| k10s_shell::EcosystemEntry {
                        id: family.id,
                        outcome: optional_table_outcome(family.answer),
                    })
                    .collect(),
            )
        });
    }

    fn reveal_helm(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: k10s_shell::Reply<k10s_shell::HelmRevealOutcome>,
    ) {
        self.reader
            .reveal_helm_revision(namespace, name, revision, move |fetched| {
                reply(helm_reveal_outcome(fetched))
            });
    }

    fn diff_helm(
        &self,
        namespace: Option<String>,
        name: String,
        from: u32,
        to: u32,
        reply: k10s_shell::Reply<k10s_shell::DocOutcome>,
    ) {
        self.reader
            .diff_helm_revisions(namespace, name, from, to, move |fetched| {
                reply(helm_diff_outcome(fetched))
            });
    }

    fn rollback_helm(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: k10s_shell::Reply<k10s_shell::HelmRollbackOutcome>,
    ) {
        self.reader
            .rollback_helm_revision(namespace, name, revision, move |fetched| {
                reply(helm_rollback_outcome(fetched))
            });
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

fn usage_outcome(outcome: k10s_data::metrics::UsageOutcome) -> k10s_shell::UsageOutcome {
    use k10s_data::metrics::{UsageOutcome, UsageSource};
    match outcome {
        UsageOutcome::Usage(sample) => k10s_shell::UsageOutcome::Usage(k10s_shell::UsageSample {
            cpu: sample.cpu.map(|cpu| k10s_shell::Millicores(cpu.0)),
            memory: sample.memory.map(|memory| k10s_shell::Bytes(memory.0)),
            cpu_request: sample.cpu_request.map(|cpu| k10s_shell::Millicores(cpu.0)),
            cpu_limit: sample.cpu_limit.map(|cpu| k10s_shell::Millicores(cpu.0)),
            memory_request: sample
                .memory_request
                .map(|memory| k10s_shell::Bytes(memory.0)),
            memory_limit: sample
                .memory_limit
                .map(|memory| k10s_shell::Bytes(memory.0)),
            source: match sample.source {
                UsageSource::MetricsServer => k10s_shell::UsageSource::MetricsServer,
                UsageSource::Kubelet => k10s_shell::UsageSource::Kubelet,
            },
            pods_measured: sample.pods_measured,
            pods_total: sample.pods_total,
            truncated: sample.truncated,
        }),
        UsageOutcome::Denied { what } => k10s_shell::UsageOutcome::Denied(what),
        UsageOutcome::Failed { why, .. } => k10s_shell::UsageOutcome::Failed(why),
        UsageOutcome::Absent { why } => k10s_shell::UsageOutcome::Absent(why),
    }
}

fn schema_text_outcome(fetched: Fetched<String>) -> k10s_shell::SchemaTextOutcome {
    match fetched {
        Fetched::Ok(text) => k10s_shell::SchemaTextOutcome::Text(text),
        Fetched::Denied { what } => k10s_shell::SchemaTextOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::SchemaTextOutcome::Failed(why),
    }
}

fn optional_table_outcome(
    fetched: Fetched<Option<k10s_data::browse::TablePage>>,
) -> k10s_shell::TableOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_shell::TableOutcome::Absent,
        Fetched::Ok(Some(page)) => table_outcome(Fetched::Ok(page)),
        Fetched::Denied { what } => k10s_shell::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::TableOutcome::Failed(why),
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

fn releases_outcome(fetched: Fetched<k10s_data::helm::Releases>) -> k10s_shell::TableOutcome {
    match fetched {
        Fetched::Ok(releases) => table_outcome(Fetched::Ok(k10s_data::helm::table_page(&releases))),
        Fetched::Denied { what } => k10s_shell::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::TableOutcome::Failed(why),
    }
}

fn argo_outcome(fetched: Fetched<k10s_data::argo::Inventory>) -> k10s_shell::TableOutcome {
    match fetched {
        Fetched::Ok(inventory) => match k10s_data::argo::table_page(&inventory) {
            Some(page) => table_outcome(Fetched::Ok(page)),
            None => k10s_shell::TableOutcome::Absent,
        },
        Fetched::Denied { what } => k10s_shell::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::TableOutcome::Failed(why),
    }
}

fn flux_outcome(fetched: Fetched<k10s_data::flux::Inventory>) -> k10s_shell::TableOutcome {
    match fetched {
        Fetched::Ok(inventory) => match k10s_data::flux::table_page(&inventory) {
            Some(page) => table_outcome(Fetched::Ok(page)),
            None => k10s_shell::TableOutcome::Absent,
        },
        Fetched::Denied { what } => k10s_shell::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::TableOutcome::Failed(why),
    }
}

fn day2_call(request: &k10s_shell::Day2Request) -> k10s_data::day2::Day2Call {
    use k10s_data::day2::{
        Caps, CordonRequest, Day2Call, DebugRequest, DeleteRequest, DrainRequest, EvictRequest,
        RolloutAction, RolloutRequest, ScaleRequest,
    };
    // Caps are filled on the Reader from the probe. A zeroed value here is
    // overwritten before the wire is considered.
    let caps = Caps::default();
    let namespace = request.namespace.clone();
    let name = request.name.clone();
    let confirm = request.confirm;
    match &request.op {
        k10s_shell::Day2Op::Scale { current, replicas } => Day2Call::Scale(ScaleRequest {
            namespace,
            name,
            current: *current,
            replicas: *replicas,
            confirm,
            caps,
        }),
        k10s_shell::Day2Op::Restart => Day2Call::Rollout(RolloutRequest {
            namespace,
            name,
            action: RolloutAction::Restart {
                restarted_at: String::new(),
            },
            confirm,
            caps,
        }),
        k10s_shell::Day2Op::Pause => Day2Call::Rollout(RolloutRequest {
            namespace,
            name,
            action: RolloutAction::Pause,
            confirm,
            caps,
        }),
        k10s_shell::Day2Op::Resume => Day2Call::Rollout(RolloutRequest {
            namespace,
            name,
            action: RolloutAction::Resume,
            confirm,
            caps,
        }),
        k10s_shell::Day2Op::Delete => Day2Call::Delete(DeleteRequest {
            namespace,
            name,
            grace_period_seconds: None,
            confirm,
            caps,
        }),
        k10s_shell::Day2Op::Evict => Day2Call::Evict(EvictRequest {
            namespace: namespace.unwrap_or_default(),
            name,
            grace_period_seconds: None,
            confirm,
            caps,
        }),
        k10s_shell::Day2Op::Cordon { unschedulable } => Day2Call::Cordon(CordonRequest {
            name,
            unschedulable: *unschedulable,
            confirm,
            caps,
        }),
        k10s_shell::Day2Op::Drain { force } => Day2Call::Drain(DrainRequest {
            name,
            force: *force,
            confirm,
            caps,
        }),
        k10s_shell::Day2Op::Debug => Day2Call::Debug(DebugRequest {
            namespace: namespace.unwrap_or_default(),
            name,
            image: "busybox".to_string(),
            confirm,
            caps,
        }),
    }
}

fn day2_outcome(outcome: k10s_data::day2::Day2Outcome) -> k10s_shell::Day2Outcome {
    use k10s_data::day2::Day2Outcome;
    match outcome {
        Day2Outcome::Applied(applied) => k10s_shell::Day2Outcome::Applied {
            summary: applied.summary,
            truncated: applied.truncated,
        },
        Day2Outcome::Denied { what, why } => k10s_shell::Day2Outcome::Denied { what, why },
        Day2Outcome::Rejected { message } => k10s_shell::Day2Outcome::Rejected { message },
        Day2Outcome::Failed { why } => k10s_shell::Day2Outcome::Failed { why },
        Day2Outcome::NeedsConfirm { summary, .. } => {
            k10s_shell::Day2Outcome::NeedsConfirm { summary }
        }
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

fn overlay_kind(kind: k10s_map::OverlayKind) -> k10s_data::overlay::Kind {
    match kind {
        k10s_map::OverlayKind::Sync => k10s_data::overlay::Kind::Sync,
        k10s_map::OverlayKind::Metrics => k10s_data::overlay::Kind::Metrics,
        k10s_map::OverlayKind::Policy => k10s_data::overlay::Kind::Policy,
        k10s_map::OverlayKind::MeshDeclared => k10s_data::overlay::Kind::MeshDeclared,
        k10s_map::OverlayKind::MeshObserved => k10s_data::overlay::Kind::MeshObserved,
    }
}

fn overlay_outcome(fetched: Fetched<k10s_data::overlay::Frame>) -> k10s_shell::OverlayOutcome {
    match fetched {
        Fetched::Ok(frame) => k10s_shell::OverlayOutcome::Ready {
            stamps: frame
                .stamps
                .into_iter()
                .map(|stamp| k10s_shell::OverlayStamp {
                    uid: stamp.uid,
                    namespace: stamp.namespace,
                    name: stamp.name,
                    tint: stamp.tint,
                    samples: stamp.samples,
                    label: stamp.label,
                })
                .collect(),
            truncated: frame.truncated,
            note: frame.note,
        },
        Fetched::Denied { what } => k10s_shell::OverlayOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::OverlayOutcome::Failed(why),
    }
}

fn posture_outcome(
    fetched: Fetched<k10s_data::netpol::PodInspection>,
) -> k10s_shell::PostureOutcome {
    match fetched {
        Fetched::Ok(inspection) if !inspection.found => k10s_shell::PostureOutcome::Missing,
        Fetched::Ok(inspection) => {
            let Some(posture) = inspection.posture else {
                return k10s_shell::PostureOutcome::Missing;
            };
            k10s_shell::PostureOutcome::Ready(k10s_shell::PodPostureView {
                ingress_isolated: posture.ingress.isolated,
                ingress_policies: posture.ingress.selecting_policies,
                ingress_names: posture.ingress.policies,
                ingress_truncated: posture.ingress.policies_truncated,
                egress_isolated: posture.egress.isolated,
                egress_policies: posture.egress.selecting_policies,
                egress_names: posture.egress.policies,
                egress_truncated: posture.egress.policies_truncated,
                ports: inspection
                    .ports
                    .into_iter()
                    .map(|port| {
                        format!(
                            "{} {} {}",
                            port.name,
                            match port.protocol {
                                k10s_data::netpol::Protocol::Tcp => "TCP",
                                k10s_data::netpol::Protocol::Udp => "UDP",
                                k10s_data::netpol::Protocol::Sctp => "SCTP",
                            },
                            port.port
                        )
                    })
                    .collect(),
                completeness: completeness_line(posture.completeness),
            })
        }
        Fetched::Denied { what } => k10s_shell::PostureOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::PostureOutcome::Failed(why),
    }
}

fn completeness_line(completeness: k10s_data::netpol::Completeness) -> String {
    use k10s_data::netpol::Completeness;
    match completeness {
        Completeness::Complete => String::new(),
        Completeness::Truncated {
            evaluated_policies,
            total_policies,
        } => format!("policy set truncated; evaluated {evaluated_policies} of {total_policies}"),
        Completeness::IncompleteInventory {
            policies,
            pods,
            namespaces,
        } => {
            let mut parts = Vec::new();
            if policies {
                parts.push("policies");
            }
            if pods {
                parts.push("pods");
            }
            if namespaces {
                parts.push("namespaces");
            }
            format!("inventory incomplete ({})", parts.join(", "))
        }
    }
}

fn tool_presence(seen: k10s_data::read::Seen) -> k10s_shell::ToolPresence {
    match seen {
        k10s_data::read::Seen::Bound => k10s_shell::ToolPresence::Ready,
        k10s_data::read::Seen::Unbound => k10s_shell::ToolPresence::Blocked,
        k10s_data::read::Seen::Absent => k10s_shell::ToolPresence::Missing,
    }
}

/// The uid arrives from a fetched dashboard listing, so it is
/// attacker-shaped; percent-encoding here keeps the launcher's URL gate
/// sufficient. Real Grafana uids are alphanumeric with `-`/`_` and pass
/// through unchanged.
fn grafana_panel_url(base: Option<&str>, uid: &str, panel_id: i64) -> Option<String> {
    let base = base?.trim_end_matches('/');
    if uid.is_empty() {
        return Some(base.to_string());
    }
    let mut encoded = String::with_capacity(uid.len());
    for byte in uid.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => encoded.push(byte as char),
            _ => {
                encoded.push('%');
                encoded.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
                encoded.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap_or('0'));
            }
        }
    }
    Some(format!("{base}/d/{encoded}?viewPanel={panel_id}"))
}

fn panel_kind(kind: k10s_data::grafana::PanelKind) -> k10s_shell::GrafanaPanelKind {
    match kind {
        k10s_data::grafana::PanelKind::Timeseries => k10s_shell::GrafanaPanelKind::Timeseries,
        k10s_data::grafana::PanelKind::Stat => k10s_shell::GrafanaPanelKind::Stat,
        k10s_data::grafana::PanelKind::Gauge => k10s_shell::GrafanaPanelKind::Gauge,
        k10s_data::grafana::PanelKind::Table => k10s_shell::GrafanaPanelKind::Table,
        k10s_data::grafana::PanelKind::Logs => k10s_shell::GrafanaPanelKind::Logs,
        k10s_data::grafana::PanelKind::Heatmap => k10s_shell::GrafanaPanelKind::Heatmap,
        k10s_data::grafana::PanelKind::Bar => k10s_shell::GrafanaPanelKind::Bar,
        k10s_data::grafana::PanelKind::Unsupported => k10s_shell::GrafanaPanelKind::Unsupported,
    }
}

fn query_dialect(dialect: k10s_data::grafana::QueryDialect) -> k10s_shell::QueryDialect {
    match dialect {
        k10s_data::grafana::QueryDialect::PromQL => k10s_shell::QueryDialect::PromQL,
        k10s_data::grafana::QueryDialect::LogQL => k10s_shell::QueryDialect::LogQL,
        k10s_data::grafana::QueryDialect::TraceQL => k10s_shell::QueryDialect::TraceQL,
        k10s_data::grafana::QueryDialect::Unknown => k10s_shell::QueryDialect::Unknown,
    }
}

fn flatten_dashboard(
    dashboard: &k10s_data::grafana::Dashboard,
    browser_base: Option<&str>,
    into: &mut Vec<k10s_shell::GrafanaPanelRow>,
) {
    for panel in &dashboard.panels {
        if panel.queries.is_empty() {
            into.push(k10s_shell::GrafanaPanelRow {
                dashboard_uid: dashboard.uid.clone(),
                dashboard_title: dashboard.title.clone(),
                panel_id: panel.id,
                title: panel.title.clone(),
                kind: panel_kind(panel.kind),
                expr: String::new(),
                dialect: k10s_shell::QueryDialect::Unknown,
                transformed: panel.transformed,
                browser_url: grafana_panel_url(browser_base, &dashboard.uid, panel.id),
            });
            continue;
        }
        for query in &panel.queries {
            into.push(k10s_shell::GrafanaPanelRow {
                dashboard_uid: dashboard.uid.clone(),
                dashboard_title: dashboard.title.clone(),
                panel_id: panel.id,
                title: if panel.title.is_empty() {
                    query.ref_id.clone()
                } else {
                    format!("{} {}", panel.title, query.ref_id)
                },
                kind: panel_kind(panel.kind),
                expr: query.expr.clone(),
                dialect: query_dialect(query.dialect),
                transformed: panel.transformed,
                browser_url: grafana_panel_url(browser_base, &dashboard.uid, panel.id),
            });
        }
    }
}

fn grafana_outcome(
    fetched: Fetched<k10s_data::read::GrafanaCatalog>,
) -> k10s_shell::GrafanaOutcome {
    match fetched {
        Fetched::Ok(catalog) if !catalog.served => k10s_shell::GrafanaOutcome::Absent,
        Fetched::Ok(catalog) => {
            let mut panels = Vec::new();
            for dashboard in &catalog.dashboards {
                flatten_dashboard(dashboard, catalog.browser_base.as_deref(), &mut panels);
            }
            for hit in catalog.extra_hits {
                panels.push(k10s_shell::GrafanaPanelRow {
                    dashboard_uid: hit.uid.clone(),
                    dashboard_title: hit.title.clone(),
                    panel_id: 0,
                    title: hit.title,
                    kind: k10s_shell::GrafanaPanelKind::Unsupported,
                    expr: String::new(),
                    dialect: k10s_shell::QueryDialect::Unknown,
                    transformed: false,
                    browser_url: grafana_panel_url(catalog.browser_base.as_deref(), &hit.uid, 0),
                });
            }
            k10s_shell::GrafanaOutcome::Catalog {
                panels,
                truncated: catalog.truncated,
            }
        }
        Fetched::Denied { what } => k10s_shell::GrafanaOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::GrafanaOutcome::Failed(why),
    }
}

fn series_labels(labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return "{}".to_string();
    }
    let inner = labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{inner}}}")
}

fn prom_outcome(fetched: Fetched<Option<k10s_data::prom::QueryResult>>) -> k10s_shell::PromOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_shell::PromOutcome::Absent,
        Fetched::Ok(Some(result)) => k10s_shell::PromOutcome::Series {
            series: result
                .series
                .into_iter()
                .map(|series| k10s_shell::PromSeriesView {
                    labels: series_labels(&series.labels),
                    points: series.points,
                })
                .collect(),
            truncated: result.truncated,
            dropped_series: result.dropped_series,
        },
        Fetched::Denied { what } => k10s_shell::PromOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::PromOutcome::Failed(why),
    }
}

fn loki_outcome(fetched: Fetched<Option<k10s_data::loki::Logs>>) -> k10s_shell::LokiOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_shell::LokiOutcome::Absent,
        Fetched::Ok(Some(logs)) => {
            let mut lines = Vec::new();
            for stream in &logs.streams {
                let labels = series_labels(&stream.labels);
                for line in &stream.lines {
                    lines.push(format!("{} {labels} {}", line.ts_ns, line.line));
                }
            }
            k10s_shell::LokiOutcome::Logs {
                lines,
                truncated: logs.truncated,
            }
        }
        Fetched::Denied { what } => k10s_shell::LokiOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::LokiOutcome::Failed(why),
    }
}

fn trace_outcome(fetched: Fetched<Option<k10s_data::traces::Trace>>) -> k10s_shell::TraceOutcome {
    match fetched {
        Fetched::Ok(None) => k10s_shell::TraceOutcome::Absent,
        Fetched::Ok(Some(trace)) => k10s_shell::TraceOutcome::Trace {
            trace_id: trace.trace_id,
            spans: trace
                .spans
                .into_iter()
                .map(|span| k10s_shell::SpanView {
                    id: span.id,
                    parent: span.parent,
                    name: span.name,
                    service: span.service,
                    start_us: span.start_us,
                    duration_us: span.duration_us,
                    status: span.status,
                })
                .collect(),
        },
        Fetched::Denied { what } => k10s_shell::TraceOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::TraceOutcome::Failed(why),
    }
}

fn severity_word(severity: k10s_core::Severity) -> &'static str {
    match severity {
        k10s_core::Severity::Ok => "ok",
        k10s_core::Severity::Unknown => "unknown",
        k10s_core::Severity::Warn => "warn",
        k10s_core::Severity::Err => "err",
    }
}

fn policy_outcome(fetched: Fetched<k10s_data::policy::Inventory>) -> k10s_shell::TableOutcome {
    match fetched {
        Fetched::Ok(inventory) if !inventory.served => k10s_shell::TableOutcome::Absent,
        Fetched::Ok(inventory) => {
            let columns = [
                "Namespace",
                "Report",
                "Policy",
                "Result",
                "Severity",
                "Resource",
                "Kind",
            ]
            .iter()
            .map(|name| k10s_shell::TableColumn {
                name: name.to_string(),
                wide: false,
            })
            .collect();
            let mut rows = Vec::new();
            if inventory.partly_denied {
                rows.push(k10s_shell::TableRow {
                    cells: vec![
                        String::new(),
                        "some report groups are denied for this account".to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                    name: "denied".to_string(),
                    namespace: None,
                    uid: "denied:policy-reports".to_string(),
                });
            }
            for report in &inventory.reports {
                for (at, finding) in report.results.iter().enumerate() {
                    rows.push(k10s_shell::TableRow {
                        cells: vec![
                            report.namespace.clone(),
                            report.name.clone(),
                            finding.policy.clone(),
                            finding.result.clone(),
                            severity_word(finding.severity).to_string(),
                            finding.resource_name.clone(),
                            finding.resource_kind.clone(),
                        ],
                        name: finding.resource_name.clone(),
                        namespace: if report.namespace.is_empty() {
                            None
                        } else {
                            Some(report.namespace.clone())
                        },
                        // A per-resource report emits many findings sharing
                        // one resource_uid; the row uid must be unique or
                        // selection restore snaps to the first duplicate.
                        uid: if finding.resource_uid.is_empty() {
                            format!("{}/{}#{at}", report.namespace, report.name)
                        } else {
                            format!("{}#{at}", finding.resource_uid)
                        },
                    });
                }
            }
            k10s_shell::TableOutcome::Table(k10s_shell::TablePage {
                columns,
                rows,
                truncated: inventory.truncated,
                continue_token: None,
            })
        }
        Fetched::Denied { what } => k10s_shell::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::TableOutcome::Failed(why),
    }
}

fn harbor_outcome(fetched: Fetched<k10s_data::harbor::Inventory>) -> k10s_shell::TableOutcome {
    match fetched {
        Fetched::Ok(inventory) if !inventory.served => k10s_shell::TableOutcome::Absent,
        Fetched::Ok(inventory) => {
            let columns = ["Project", "Visibility", "Repository", "Artifacts", "Scan"]
                .iter()
                .map(|name| k10s_shell::TableColumn {
                    name: name.to_string(),
                    wide: false,
                })
                .collect();
            let mut rows = Vec::new();
            for project in &inventory.projects {
                if project.repositories.is_empty() {
                    // repo_count counts repositories, not artifacts; it must
                    // stay under the Repository header.
                    rows.push(k10s_shell::TableRow {
                        cells: vec![
                            project.name.clone(),
                            if project.public { "public" } else { "private" }.to_string(),
                            format!("({} repositories)", project.repo_count),
                            String::new(),
                            String::new(),
                        ],
                        name: project.name.clone(),
                        namespace: None,
                        uid: project.name.clone(),
                    });
                    continue;
                }
                for repo in &project.repositories {
                    // A repository-level security cell must show the worst
                    // scan across its artifacts, not whichever Harbor listed
                    // first.
                    let scan = repo
                        .artifacts
                        .iter()
                        .filter_map(|artifact| artifact.scan.as_ref())
                        .max_by_key(|scan| {
                            (
                                scan.mapped,
                                scan.critical,
                                scan.high,
                                scan.medium,
                                scan.low,
                                scan.total,
                            )
                        })
                        .map(|scan| format!("{} ({})", scan.severity, scan.total))
                        .unwrap_or_default();
                    rows.push(k10s_shell::TableRow {
                        cells: vec![
                            project.name.clone(),
                            if project.public { "public" } else { "private" }.to_string(),
                            repo.name.clone(),
                            repo.artifact_count.to_string(),
                            scan,
                        ],
                        name: repo.name.clone(),
                        namespace: Some(project.name.clone()),
                        uid: format!("{}/{}", project.name, repo.name),
                    });
                }
            }
            k10s_shell::TableOutcome::Table(k10s_shell::TablePage {
                columns,
                rows,
                // Unreadable projects, repos, or scans are missing rows; the
                // listing must not read as complete.
                truncated: inventory.truncated || inventory.unreadable > 0,
                continue_token: None,
            })
        }
        Fetched::Denied { what } => k10s_shell::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::TableOutcome::Failed(why),
    }
}

fn mesh_outcome(inventory: k10s_data::mesh::MeshInventory) -> k10s_shell::TableOutcome {
    use k10s_data::mesh::GroupState;
    if !inventory.present() {
        return match (&inventory.istio, &inventory.linkerd) {
            (GroupState::Denied, _) | (_, GroupState::Denied) => {
                k10s_shell::TableOutcome::Denied("mesh")
            }
            (GroupState::Failed { why }, _) | (_, GroupState::Failed { why }) => {
                k10s_shell::TableOutcome::Failed(why.clone())
            }
            _ => k10s_shell::TableOutcome::Absent,
        };
    }
    // One group answering must not mask the sibling's denial or failure: with
    // nothing to show the error is the whole answer, and with rows to show
    // the page must still say it is partial.
    if inventory.objects.is_empty() {
        match (&inventory.istio, &inventory.linkerd) {
            (GroupState::Denied, _) | (_, GroupState::Denied) => {
                return k10s_shell::TableOutcome::Denied("mesh");
            }
            (GroupState::Failed { why }, _) | (_, GroupState::Failed { why }) => {
                return k10s_shell::TableOutcome::Failed(why.clone());
            }
            _ => {}
        }
    }
    let degraded = !matches!(inventory.istio, GroupState::Served | GroupState::Absent)
        || !matches!(inventory.linkerd, GroupState::Served | GroupState::Absent);
    let columns = [
        "Kind",
        "Namespace",
        "Name",
        "Hosts",
        "Destinations",
        "Gateways",
    ]
    .iter()
    .map(|name| k10s_shell::TableColumn {
        name: name.to_string(),
        wide: false,
    })
    .collect();
    let rows = inventory
        .objects
        .iter()
        .map(|object| k10s_shell::TableRow {
            cells: vec![
                object.kind.as_str().to_string(),
                object.namespace.clone(),
                object.name.clone(),
                object.hosts.join(","),
                object.destinations.join(","),
                object.gateways.join(","),
            ],
            name: object.name.clone(),
            namespace: if object.namespace.is_empty() {
                None
            } else {
                Some(object.namespace.clone())
            },
            uid: format!(
                "{}/{}/{}",
                object.kind.as_str(),
                object.namespace,
                object.name
            ),
        })
        .collect();
    k10s_shell::TableOutcome::Table(k10s_shell::TablePage {
        columns,
        rows,
        truncated: inventory.truncated || degraded,
        continue_token: None,
    })
}

fn scratch_text(scratch: &k10s_data::reach::Scratch, what: &str) -> Result<String, String> {
    scratch
        .as_str()
        .map(str::to_string)
        .map_err(|_| format!("{what} is not UTF-8"))
}

fn helm_reveal_outcome(
    fetched: Fetched<k10s_data::helm_reveal::RevealedRevision>,
) -> k10s_shell::HelmRevealOutcome {
    match fetched {
        Fetched::Ok(revealed) => {
            let config = match scratch_text(revealed.config(), "user values") {
                Ok(text) => text,
                Err(why) => return k10s_shell::HelmRevealOutcome::Failed(why),
            };
            let chart_values = match scratch_text(revealed.chart_values(), "chart values") {
                Ok(text) => text,
                Err(why) => return k10s_shell::HelmRevealOutcome::Failed(why),
            };
            let manifest = match scratch_text(revealed.manifest(), "manifest") {
                Ok(text) => text,
                Err(why) => return k10s_shell::HelmRevealOutcome::Failed(why),
            };
            k10s_shell::HelmRevealOutcome::Revealed(k10s_shell::HelmReveal {
                name: revealed.name.clone(),
                namespace: revealed.namespace.clone(),
                revision: revealed.revision,
                config,
                chart_values,
                manifest,
            })
        }
        Fetched::Denied { what } => k10s_shell::HelmRevealOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::HelmRevealOutcome::Failed(why),
    }
}

fn helm_diff_outcome(fetched: Fetched<String>) -> k10s_shell::DocOutcome {
    match fetched {
        Fetched::Ok(text) => k10s_shell::DocOutcome::Doc {
            title: "helm revision diff".to_string(),
            lines: text.lines().map(str::to_string).collect(),
        },
        Fetched::Denied { what } => k10s_shell::DocOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::DocOutcome::Failed(why),
    }
}

fn apply_line(outcome: &k10s_data::apply::ApplyOutcome) -> String {
    use k10s_data::apply::ApplyOutcome;
    match outcome {
        ApplyOutcome::Applied(applied) => {
            if applied.dry_run {
                "dry-run applied".to_string()
            } else {
                "applied".to_string()
            }
        }
        ApplyOutcome::Unrendered(unrendered) => format!("applied ({})", unrendered.why),
        ApplyOutcome::Conflict { message, .. } => format!("conflict: {message}"),
        ApplyOutcome::Stale { message } => format!("stale: {message}"),
        ApplyOutcome::Rejected { message, .. } => format!("rejected: {message}"),
        ApplyOutcome::Denied { what, why } => format!("{what}: {why}"),
        ApplyOutcome::Failed { why } => why.clone(),
    }
}

fn helm_rollback_outcome(
    fetched: Fetched<k10s_data::helm_reveal::RollbackReport>,
) -> k10s_shell::HelmRollbackOutcome {
    match fetched {
        Fetched::Ok(report) => {
            let mut lines = vec![report.note.to_string(), String::new()];
            for document in &report.documents {
                match document {
                    k10s_data::helm_reveal::DocumentRollback::Applied {
                        name,
                        kind,
                        outcome,
                    } => lines.push(format!("{kind} {name}: {}", apply_line(outcome))),
                    k10s_data::helm_reveal::DocumentRollback::Skipped { name, kind, why } => {
                        lines.push(format!("{kind} {name}: skipped ({why})"))
                    }
                }
            }
            k10s_shell::HelmRollbackOutcome::Report {
                note: report.note.to_string(),
                lines,
            }
        }
        Fetched::Denied { what } => k10s_shell::HelmRollbackOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_shell::HelmRollbackOutcome::Failed(why),
    }
}

fn unix_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

fn unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
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
