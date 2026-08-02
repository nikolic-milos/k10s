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

use crate::browse::{self, TablePage};
use crate::describe::{self, DescribeRequest, Described};
use crate::discover::KindTarget;
use crate::logs::{self, LogChunk, LogRequest, LogStop};
use crate::nodes;

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
}

#[derive(Clone)]
pub struct Reader {
    client: Client,
    handle: tokio::runtime::Handle,
    targets: Arc<[KindTarget]>,
    verdicts: Arc<HashMap<KindId, Capability>>,
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
        Reader {
            client,
            handle: tokio::runtime::Handle::current(),
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
            })
            .collect();
        rows.sort_by(|a, b| a.display.cmp(&b.display));
        rows
    }

    pub fn fetch_table(
        &self,
        kind: KindId,
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
            reply(browse::fetch_table(&client, &target, None).await);
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
