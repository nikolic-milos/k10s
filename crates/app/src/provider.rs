//! The shell's provider seam, adapted to the data plane.
//!
//! The shell never sees kube; it sees labelled outcomes. A reply callback
//! either runs at once on the calling thread, before any request, or on the
//! data plane's runtime; either way the view bridges it onto its own
//! executor, so no thread is parked waiting for an answer. Each reply is
//! translated by a named function rather than a closure, so the mapping from
//! a data-plane answer to a shell outcome is a value a test can call.

mod kubernetes;

use kubernetes::{
    adapt, apply_outcome, containers_outcome, day2_call, day2_outcome, describe_outcome,
    manifest_outcome, schema_catalog_outcome, schema_text_outcome, table_outcome,
};

mod helm;

use helm::{helm_diff_outcome, helm_reveal_outcome, helm_rollback_outcome, releases_outcome};

mod observe;

use observe::{
    alert_pod_outcome, alertmanager_bound, alertmanager_outcome, grafana_outcome, loki_outcome,
    observe_reach, omitted_notice_answer, optional_table_outcome, prom_outcome, silence_outcome,
    silence_spec, trace_outcome,
};

mod inventory;

use inventory::{argo_outcome, flux_outcome, harbor_outcome, mesh_outcome, policy_outcome};

mod streams;

use streams::{
    ExecSessionAdapter, adapt_chunk, adapt_forward, exec_event, forward_outcome, usage_outcome,
};

mod overlay;

use overlay::{overlay_outcome, posture_outcome};

use std::sync::Arc;

use k10s_core::Capability;
#[cfg(test)]
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

impl k10s_provider::ClusterProvider for PlaneProvider {
    fn fetch_events(
        &self,
        namespace: &str,
        name: &str,
        reply: k10s_provider::Reply<k10s_provider::Detail>,
    ) {
        self.inspector
            .fetch_events(namespace, name, move |detail| reply(adapt(detail)));
    }

    fn fetch_log_tail(
        &self,
        namespace: &str,
        pod: &str,
        reply: k10s_provider::Reply<k10s_provider::Detail>,
    ) {
        self.inspector
            .fetch_log_tail(namespace, &Arc::from(pod), move |detail| {
                reply(adapt(detail))
            });
    }

    fn kinds(&self) -> Vec<k10s_provider::KindRow> {
        self.reader
            .kinds()
            .into_iter()
            .map(|row| k10s_provider::KindRow {
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
        reply: k10s_provider::Reply<k10s_provider::TableOutcome>,
    ) {
        self.reader
            .fetch_table(kind, continue_token, move |fetched| {
                reply(table_outcome(fetched))
            });
    }

    fn fetch_node_table(&self, reply: k10s_provider::Reply<k10s_provider::TableOutcome>) {
        self.reader
            .fetch_node_table(move |fetched| reply(table_outcome(fetched)));
    }

    fn fetch_describe(
        &self,
        request: &k10s_provider::DescribeRequest,
        reply: k10s_provider::Reply<k10s_provider::DocOutcome>,
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
    fn fetch_releases(&self, reply: k10s_provider::Reply<k10s_provider::TableOutcome>) {
        self.reader
            .fetch_releases(None, move |fetched| reply(releases_outcome(fetched)));
    }

    fn fetch_argo(&self, reply: k10s_provider::Reply<k10s_provider::TableOutcome>) {
        self.reader
            .fetch_argo(move |fetched| reply(argo_outcome(fetched)));
    }

    fn fetch_flux(&self, reply: k10s_provider::Reply<k10s_provider::TableOutcome>) {
        self.reader
            .fetch_flux(move |fetched| reply(flux_outcome(fetched)));
    }

    fn run_day2(
        &self,
        request: &k10s_provider::Day2Request,
        reply: k10s_provider::Reply<k10s_provider::Day2Outcome>,
    ) {
        let call = day2_call(request);
        self.reader.day2(request.kind, call, move |outcome| {
            reply(day2_outcome(outcome))
        });
    }

    fn fetch_manifest(
        &self,
        request: &k10s_provider::DescribeRequest,
        reply: k10s_provider::Reply<k10s_provider::ManifestOutcome>,
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
        request: &k10s_provider::ApplyRequest,
        reply: k10s_provider::Reply<k10s_provider::ApplyOutcome>,
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

    fn fetch_schema_catalog(
        &self,
        reply: k10s_provider::Reply<k10s_provider::SchemaCatalogOutcome>,
    ) {
        self.reader
            .fetch_schema_catalog(move |fetched| reply(schema_catalog_outcome(fetched)));
    }

    fn fetch_schema_document(
        &self,
        url: &str,
        reply: k10s_provider::Reply<k10s_provider::SchemaTextOutcome>,
    ) {
        self.reader
            .fetch_schema_document(url.to_string(), move |fetched| {
                reply(schema_text_outcome(fetched))
            });
    }

    fn fetch_crd_schemas(&self, reply: k10s_provider::Reply<k10s_provider::SchemaTextOutcome>) {
        self.reader
            .fetch_crd_schemas(move |fetched| reply(schema_text_outcome(fetched)));
    }

    fn fetch_containers(
        &self,
        namespace: &str,
        pod: &str,
        reply: k10s_provider::Reply<k10s_provider::ContainersOutcome>,
    ) {
        self.reader
            .fetch_containers(namespace, pod, move |fetched| {
                reply(containers_outcome(fetched))
            });
    }

    fn follow_log(
        &self,
        request: &k10s_provider::LogRequest,
        on_chunk: Box<dyn Fn(k10s_provider::LogChunk) + Send + Sync>,
    ) -> k10s_provider::LogStop {
        let request = k10s_data::logs::LogRequest {
            namespace: request.namespace.clone(),
            pod: request.pod.clone(),
            container: request.container.clone(),
            previous: request.previous,
        };
        let stop = self
            .reader
            .follow_log(request, Box::new(move |chunk| on_chunk(adapt_chunk(chunk))));
        k10s_provider::LogStop::new(move || drop(stop))
    }

    fn follow_workload_logs(
        &self,
        request: &k10s_provider::WorkloadLogRequest,
        on_chunk: Box<dyn Fn(k10s_provider::LogChunk) + Send + Sync>,
    ) -> k10s_provider::LogStop {
        let request = k10s_data::logs::WorkloadLogRequest {
            namespace: request.namespace.clone(),
            kind: request.kind,
            name: request.name.clone(),
        };
        let stop = self
            .reader
            .follow_workload_logs(request, Box::new(move |chunk| on_chunk(adapt_chunk(chunk))));
        k10s_provider::LogStop::new(move || drop(stop))
    }

    fn poll_usage(
        &self,
        request: &k10s_provider::UsageRequest,
        on_update: Box<dyn Fn(k10s_provider::UsageOutcome) + Send + Sync>,
    ) -> k10s_provider::LogStop {
        let request = k10s_data::metrics::UsageRequest {
            namespace: request.namespace.clone(),
            target: match &request.target {
                k10s_provider::UsageTarget::Pod { name } => {
                    k10s_data::metrics::UsageTarget::Pod { name: name.clone() }
                }
                k10s_provider::UsageTarget::Workload { kind, name } => {
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
        k10s_provider::LogStop::new(move || drop(stop))
    }

    fn open_forward(
        &self,
        request: &k10s_provider::ForwardRequest,
        reply: k10s_provider::Reply<k10s_provider::ForwardOutcome>,
    ) {
        let request = k10s_data::forward::ForwardRequest {
            namespace: request.namespace.clone(),
            name: request.name.clone(),
            service: request.service,
        };
        self.reader
            .open_forward(request, move |fetched| reply(forward_outcome(fetched)));
    }

    fn list_forwards(&self) -> Vec<k10s_provider::ForwardRow> {
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
        request: &k10s_provider::ExecRequest,
        on_event: Box<dyn Fn(k10s_provider::ExecEvent) + Send + Sync>,
    ) -> Box<dyn k10s_provider::ExecSession> {
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
        reply: k10s_provider::Reply<k10s_provider::OverlayOutcome>,
    ) {
        self.reader.fetch_overlay(
            kind,
            k10s_data::reach::ReachSettings::default(),
            move |fetched| reply(overlay_outcome(fetched)),
        );
    }

    fn fetch_pod_posture(
        &self,
        namespace: &str,
        name: &str,
        reply: k10s_provider::Reply<k10s_provider::PostureOutcome>,
    ) {
        self.reader
            .fetch_pod_posture(namespace.to_string(), name.to_string(), move |fetched| {
                reply(posture_outcome(fetched))
            });
    }

    fn fetch_grafana(
        &self,
        reply: k10s_provider::Reply<k10s_provider::GrafanaOutcome>,
    ) -> k10s_provider::RequestStop {
        let stop = self
            .reader
            .observe_grafana(move |fetched| reply(grafana_outcome(fetched)));
        k10s_provider::RequestStop::new(move || drop(stop))
    }

    fn probe_observe(
        &self,
        reply: k10s_provider::Reply<k10s_provider::ObserveReach>,
    ) -> k10s_provider::RequestStop {
        let stop = self.reader.observe_probe(move |fetched| {
            reply(observe_reach(fetched));
        });
        k10s_provider::RequestStop::new(move || drop(stop))
    }

    fn resolve_observe_scope(
        &self,
        request: k10s_provider::DescribeRequest,
        reply: k10s_provider::Reply<k10s_provider::ScopeOutcome>,
    ) -> k10s_provider::RequestStop {
        let data_request = k10s_data::describe::DescribeRequest {
            kind: request.kind,
            namespace: request.namespace.clone(),
            name: request.name.clone(),
            uid: request.uid.clone(),
        };
        let stop = self
            .reader
            .resolve_observe_scope(data_request, move |fetched| {
                use k10s_provider::{ObserveScope, PodIdentity, ScopeOutcome};
                reply(match fetched {
                    k10s_data::read::Fetched::Ok(scope) => ScopeOutcome::Ready(ObserveScope {
                        request,
                        namespace: scope.namespace,
                        pods: scope
                            .pods
                            .into_iter()
                            .map(|pod| PodIdentity {
                                name: pod.name,
                                uid: pod.uid,
                                node: pod.node,
                            })
                            .collect(),
                    }),
                    k10s_data::read::Fetched::Denied { what } => ScopeOutcome::Denied(what),
                    k10s_data::read::Fetched::Failed { why, .. } => ScopeOutcome::Failed(why),
                });
            });
        k10s_provider::RequestStop::new(move || drop(stop))
    }

    fn query_panel(
        &self,
        panel: k10s_provider::GrafanaPanelRow,
        scope: k10s_provider::ObserveScope,
        window: k10s_provider::QueryWindow,
        reply: k10s_provider::Reply<k10s_provider::PromOutcome>,
    ) -> k10s_provider::RequestStop {
        if let Err(why) = window.validate() {
            reply(k10s_provider::PromOutcome::Failed(why.to_string()));
            return k10s_provider::RequestStop::noop();
        }
        if let Some(why) = panel.unsupported {
            reply(k10s_provider::PromOutcome::Failed(why));
            return k10s_provider::RequestStop::noop();
        }
        if panel.dialect != k10s_provider::QueryDialect::PromQL {
            reply(k10s_provider::PromOutcome::Failed(
                "unsupported datasource query dialect".into(),
            ));
            return k10s_provider::RequestStop::noop();
        }
        let variables = k10s_data::integrations::grafana::Variables {
            namespace: scope.namespace,
            pods: scope.pods.iter().map(|pod| pod.name.clone()).collect(),
            node: (scope.pods.len() == 1)
                .then(|| scope.pods[0].node.clone())
                .filter(|node| !node.is_empty()),
            start_ms: window.start_ms,
            end_ms: window.end_ms,
            step_ms: window.step_ms,
            min_interval: panel.min_interval,
        };
        let target = k10s_data::integrations::grafana::PanelQuery {
            ref_id: panel.ref_id,
            expr: panel.expr,
            dialect: k10s_data::integrations::grafana::QueryDialect::PromQL,
            datasource: panel.datasource_uid,
            min_step: panel.min_step,
            unsupported: None,
        };
        let stop = self
            .reader
            .observe_panel(target, variables, move |fetched| {
                reply(prom_outcome(fetched))
            });
        k10s_provider::RequestStop::new(move || drop(stop))
    }

    fn query_promql(
        &self,
        request: k10s_provider::PromRequest,
        reply: k10s_provider::Reply<k10s_provider::PromOutcome>,
    ) -> k10s_provider::RequestStop {
        if let Err(why) = request.window.validate() {
            reply(k10s_provider::PromOutcome::Failed(why.to_string()));
            return k10s_provider::RequestStop::noop();
        }
        let stop = self.reader.observe_prometheus(
            request.expr,
            request.window.start_ms as f64 / 1000.0,
            request.window.end_ms as f64 / 1000.0,
            k10s_data::integrations::grafana::duration_text(request.window.step_ms),
            move |fetched| reply(prom_outcome(fetched)),
        );
        k10s_provider::RequestStop::new(move || drop(stop))
    }

    fn query_loki(
        &self,
        request: k10s_provider::LokiRequest,
        reply: k10s_provider::Reply<k10s_provider::LokiOutcome>,
    ) -> k10s_provider::RequestStop {
        if let Err(why) = request.window.validate() {
            reply(k10s_provider::LokiOutcome::Failed(why.to_string()));
            return k10s_provider::RequestStop::noop();
        }
        let stop = self.reader.observe_loki(
            k10s_data::integrations::loki::RangeQuery {
                query: request.query,
                start_ns: request.window.start_ms as u64 * 1_000_000,
                end_ns: request.window.end_ms as u64 * 1_000_000,
                limit: request.limit.clamp(1, 2000),
            },
            move |fetched| reply(loki_outcome(fetched)),
        );
        k10s_provider::RequestStop::new(move || drop(stop))
    }

    fn lookup_trace(
        &self,
        trace_id: String,
        reply: k10s_provider::Reply<k10s_provider::TraceOutcome>,
    ) -> k10s_provider::RequestStop {
        let stop = self
            .reader
            .observe_trace(trace_id, move |fetched| reply(trace_outcome(fetched)));
        k10s_provider::RequestStop::new(move || drop(stop))
    }

    fn fetch_policy(&self, reply: k10s_provider::Reply<k10s_provider::TableOutcome>) {
        self.reader
            .fetch_policy_reports(move |fetched| reply(policy_outcome(fetched)));
    }

    fn fetch_harbor(&self, reply: k10s_provider::Reply<k10s_provider::TableOutcome>) {
        self.reader
            .fetch_harbor(move |fetched| reply(harbor_outcome(fetched)));
    }

    fn fetch_mesh(&self, reply: k10s_provider::Reply<k10s_provider::TableOutcome>) {
        self.reader
            .fetch_mesh_declared(move |inventory| reply(mesh_outcome(inventory)));
    }

    fn fetch_ecosystem(&self, reply: k10s_provider::Reply<Vec<k10s_provider::EcosystemEntry>>) {
        self.reader.fetch_ecosystem(move |families| {
            reply(
                families
                    .into_iter()
                    .map(|family| k10s_provider::EcosystemEntry {
                        id: family.id,
                        outcome: optional_table_outcome(family.answer),
                    })
                    .collect(),
            )
        });
    }

    fn fetch_alert_pod(
        &self,
        uid: String,
        reply: k10s_provider::Reply<k10s_provider::AlertPodOutcome>,
    ) {
        if let Some(outcome) = omitted_notice_answer(&uid) {
            reply(outcome);
            return;
        }
        self.reader.fetch_alertmanager_alerts(move |answer| {
            reply(alert_pod_outcome(&uid, answer));
        });
    }

    fn bind_alertmanager(&self, reply: k10s_provider::Reply<k10s_provider::AlertmanagerOutcome>) {
        self.reader.bind_tool(
            k10s_data::reach::ToolKind::Alertmanager,
            k10s_data::reach::ReachSettings::default(),
            move |answer| reply(alertmanager_outcome(answer)),
        );
    }

    fn create_silence(
        &self,
        request: &k10s_provider::SilenceRequest,
        confirm: bool,
        reply: k10s_provider::Reply<k10s_provider::SilenceOutcome>,
    ) {
        let spec = match silence_spec(request) {
            Ok(spec) => spec,
            Err(why) => {
                reply(k10s_provider::SilenceOutcome::Failed(why));
                return;
            }
        };
        let window = spec.starts_at.clone()..spec.ends_at.clone();
        self.reader.create_silence(
            alertmanager_bound(&request.endpoint),
            spec,
            confirm,
            move |answer| reply(silence_outcome(answer, window)),
        );
    }

    fn reveal_helm(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: k10s_provider::Reply<k10s_provider::HelmRevealOutcome>,
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
        reply: k10s_provider::Reply<k10s_provider::DocOutcome>,
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
        reply: k10s_provider::Reply<k10s_provider::HelmRollbackOutcome>,
    ) {
        self.reader
            .rollback_helm_revision(namespace, name, revision, move |fetched| {
                reply(helm_rollback_outcome(fetched))
            });
    }
}

#[cfg(test)]
#[path = "provider_test.rs"]
mod tests;
