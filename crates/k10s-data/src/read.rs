//! The read seam: one handle the UI pulls detail through.
//!
//! A [`Reader`] carries the client, the runtime handle, what discovery found,
//! and the probe's capability verdicts. Every method is fire-and-forget onto
//! the data plane's runtime with the answer handed to a caller-supplied
//! callback -- the render thread never blocks on the cluster -- and every
//! outcome is a labelled [`Fetched`]: a 403 arrives as `Denied`, never as an
//! empty panel or an error string a person has to diagnose. Errors that reach
//! text pass through the same redaction filter as everything else in this
//! crate.

use std::collections::HashMap;
use std::sync::Arc;

use kube::Client;

use k10s_core::{Capability, KindId};

use crate::apply::{self, ApplyOutcome, ApplyRequest};
use crate::browse::{self, TablePage};
use crate::describe::{self, DescribeRequest, Described};
use crate::discover::KindTarget;
use crate::exec::{ExecEvent, ExecRequest, ExecSession, ExecTransport, KubeExecTransport};
use crate::forward::{self, ForwardRegistry, ForwardRequest, ForwardRow, KubeForwarder};
use crate::logs::{self, LogChunk, LogRequest, LogStop};
use crate::manifest;
use crate::nodes;
use crate::openapi;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched<T> {
    Ok(T),
    Denied { what: &'static str },
    Failed { what: &'static str, why: String },
}

pub(crate) fn classify<T>(what: &'static str, error: &kube::Error) -> Fetched<T> {
    if let kube::Error::Api(response) = error
        && response.code == 403
    {
        return Fetched::Denied { what };
    }
    Fetched::Failed {
        what,
        why: crate::connect::describe(error as &(dyn std::error::Error + 'static)),
    }
}

// The same classification flattened to one displayable line, for states that
// carry text rather than a Fetched (a forward's Dead reason).
pub(crate) fn classify_text(what: &'static str, error: &kube::Error) -> String {
    match classify::<()>(what, error) {
        Fetched::Denied { what } => format!("{what}: access denied for this account"),
        Fetched::Failed { why, .. } => why,
        Fetched::Ok(()) => unreachable!("classify never returns Ok"),
    }
}

pub(crate) fn collection_path(target: &KindTarget, namespace: Option<&str>) -> String {
    let resource = &target.resource;
    let mut path = if resource.group.is_empty() {
        format!("/api/{}", resource.version)
    } else {
        format!("/apis/{}/{}", resource.group, resource.version)
    };
    if target.namespaced
        && let Some(namespace) = namespace
    {
        path.push_str("/namespaces/");
        path.push_str(namespace);
    }
    path.push('/');
    path.push_str(&resource.plural);
    path
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindRow {
    pub id: KindId,
    // "deployments.apps", "pods" -- the name a kubectl user types.
    pub display: String,
    pub kind: String,
    pub namespaced: bool,
    // None means the probe had no answer: attempted, not gated.
    pub verdict: Option<Capability>,
    // Whether the server serves a patch verb here at all, which is a different
    // question from whether this account may use it.
    pub patchable: bool,
}

#[derive(Clone)]
pub struct Reader {
    client: Client,
    handle: tokio::runtime::Handle,
    targets: Arc<[KindTarget]>,
    verdicts: Arc<HashMap<KindId, Capability>>,
    forwards: ForwardRegistry,
}

impl Reader {
    fn target(&self, kind: KindId) -> Option<KindTarget> {
        self.targets.iter().find(|t| t.id == kind).cloned()
    }

    pub(crate) fn new(
        client: Client,
        targets: Vec<KindTarget>,
        verdicts: &[(KindId, Capability)],
    ) -> Reader {
        let handle = tokio::runtime::Handle::current();
        Reader {
            forwards: ForwardRegistry::new(Arc::new(KubeForwarder::new(
                client.clone(),
                handle.clone(),
            ))),
            client,
            handle,
            targets: targets.into(),
            verdicts: Arc::new(verdicts.iter().copied().collect()),
        }
    }

    pub fn kinds(&self) -> Vec<KindRow> {
        let mut rows: Vec<KindRow> = self
            .targets
            .iter()
            .filter(|target| target.listable)
            .map(|target| KindRow {
                id: target.id,
                display: if target.group().is_empty() {
                    target.plural().to_string()
                } else {
                    format!("{}.{}", target.plural(), target.group())
                },
                kind: target.kind().to_string(),
                namespaced: target.namespaced,
                verdict: self.verdicts.get(&target.id).copied(),
                patchable: target.patchable,
            })
            .collect();
        rows.sort_by(|a, b| a.display.cmp(&b.display));
        rows
    }

    // `continue_token` asks for the page after the one that returned it;
    // None asks for the first page.
    pub fn fetch_table(
        &self,
        kind: KindId,
        continue_token: Option<String>,
        reply: impl FnOnce(Fetched<TablePage>) + Send + 'static,
    ) {
        let Some(target) = self.target(kind) else {
            reply(Fetched::Failed {
                what: "table",
                why: "this kind is not served by the connected cluster".to_string(),
            });
            return;
        };
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(browse::fetch_table(&client, &target, None, continue_token.as_deref()).await);
        });
    }

    pub fn fetch_describe(
        &self,
        request: DescribeRequest,
        reply: impl FnOnce(Fetched<Described>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(describe::fetch_describe(&client, &targets, &request).await);
        });
    }

    pub fn fetch_manifest(
        &self,
        request: DescribeRequest,
        reply: impl FnOnce(Fetched<manifest::Manifest>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(manifest::fetch_manifest(&client, &targets, &request).await);
        });
    }

    // The one mutating method in the crate. Dry run and apply are the same
    // request with one query parameter between them, which is why they are one
    // method: a caller cannot reach the apply without having been able to reach
    // the dry run.
    pub fn apply(&self, request: ApplyRequest, reply: impl FnOnce(ApplyOutcome) + Send + 'static) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(apply::apply(&client, &targets, &request).await);
        });
    }

    pub fn fetch_schema_catalog(
        &self,
        reply: impl FnOnce(Fetched<Vec<openapi::SchemaSource>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(openapi::fetch_catalog(&client).await);
        });
    }

    pub fn fetch_schema_document(
        &self,
        url: String,
        reply: impl FnOnce(Fetched<String>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(openapi::fetch_document(&client, &url).await);
        });
    }

    pub fn fetch_crd_schemas(&self, reply: impl FnOnce(Fetched<String>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(openapi::fetch_crds(&client).await);
        });
    }

    pub fn fetch_containers(
        &self,
        namespace: &str,
        pod: &str,
        reply: impl FnOnce(Fetched<Vec<String>>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let namespace = namespace.to_string();
        let pod = pod.to_string();
        self.handle.spawn(async move {
            reply(logs::fetch_containers(&client, &namespace, &pod).await);
        });
    }

    pub fn fetch_node_table(&self, reply: impl FnOnce(Fetched<TablePage>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(nodes::fetch_node_table(&client).await);
        });
    }

    pub fn follow_log(
        &self,
        request: LogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        logs::follow(&self.handle, self.client.clone(), request, on_chunk)
    }

    // The managed forward registry: list and close are synchronous local
    // state; opening goes through `open_forward` because the target has to
    // be resolved on the cluster first.
    pub fn forwards(&self) -> &ForwardRegistry {
        &self.forwards
    }

    // Resolution only -- what pod and which ports a request means -- with no
    // listener bound; this is the half a scripted API server can prove.
    pub fn resolve_forward(
        &self,
        request: ForwardRequest,
        reply: impl FnOnce(Fetched<forward::ForwardSpec>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(forward::resolve(&client, &request).await);
        });
    }

    // Resolve, then register: the registry's answer (collision, cap) comes
    // back as a labelled failure like everything else.
    pub fn open_forward(
        &self,
        request: ForwardRequest,
        reply: impl FnOnce(Fetched<ForwardRow>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let registry = self.forwards.clone();
        self.handle.spawn(async move {
            match forward::resolve(&client, &request).await {
                Fetched::Ok(spec) => reply(match registry.open(spec) {
                    Ok(row) => Fetched::Ok(row),
                    Err(error) => Fetched::Failed {
                        what: "port-forward",
                        why: error.to_string(),
                    },
                }),
                Fetched::Denied { what } => reply(Fetched::Denied { what }),
                Fetched::Failed { what, why } => reply(Fetched::Failed { what, why }),
            }
        });
    }

    // An interactive exec over the kube transport; the returned session
    // carries input and resize, and dropping it terminates the remote shell.
    pub fn start_exec(
        &self,
        request: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession> {
        KubeExecTransport::new(self.client.clone(), self.handle.clone()).start(request, on_event)
    }

    // One merged follow over the pods the workload's selector matches; the
    // returned guard cancels every underlying follow.
    pub fn follow_workload_logs(
        &self,
        request: logs::WorkloadLogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        let Some(target) = self.target(request.kind) else {
            on_chunk(LogChunk::Failed {
                what: "workload logs",
                why: "this kind is not served by the connected cluster".to_string(),
            });
            return LogStop::noop();
        };
        logs::follow_workload(&self.handle, self.client.clone(), target, request, on_chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::discovery::{ApiCapabilities, ApiResource, Scope};

    fn target(
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        namespaced: bool,
    ) -> KindTarget {
        let mut catalog = k10s_core::Catalog::new();
        crate::discover::intern(
            &mut catalog,
            ApiResource {
                group: group.to_string(),
                version: version.to_string(),
                api_version: if group.is_empty() {
                    version.to_string()
                } else {
                    format!("{group}/{version}")
                },
                kind: kind.to_string(),
                plural: plural.to_string(),
            },
            &ApiCapabilities {
                scope: if namespaced {
                    Scope::Namespaced
                } else {
                    Scope::Cluster
                },
                subresources: Vec::new(),
                operations: vec!["get".into(), "list".into(), "watch".into()],
            },
        )
    }

    #[test]
    fn collection_paths_cover_core_group_and_namespace_scoping() {
        let pods = target("", "v1", "Pod", "pods", true);
        assert_eq!(
            collection_path(&pods, Some("prod")),
            "/api/v1/namespaces/prod/pods"
        );
        assert_eq!(collection_path(&pods, None), "/api/v1/pods");

        let deployments = target("apps", "v1", "Deployment", "deployments", true);
        assert_eq!(
            collection_path(&deployments, Some("prod")),
            "/apis/apps/v1/namespaces/prod/deployments"
        );

        let namespaces = target("", "v1", "Namespace", "namespaces", false);
        assert_eq!(
            collection_path(&namespaces, Some("prod")),
            "/api/v1/namespaces",
            "a cluster-scoped kind never nests under a namespace"
        );
    }
}
