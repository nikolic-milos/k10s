//! The read seam: one handle the UI pulls detail through.
//!
//! A [`Reader`] carries both clients, the runtime handle, what discovery found,
//! and the probe's capability verdicts. The four writes a person confirms (an
//! apply, a day-2 call, a stored-manifest apply and a silence) go through the
//! interactive client, with every read inside them that decides the write, a
//! dry run's read of the object included, and each is judged by its answer in
//! `connect::send_write`; any write added later belongs there too. Everything
//! else, the long-lived streams included, uses the sync client.
//!
//! Every method is fire-and-forget onto the data plane's runtime with the
//! answer handed to a caller-supplied callback -- the render thread never
//! blocks on the cluster -- and every outcome is a labelled [`Fetched`]: an
//! account the cluster refuses -- 401 or 403, the same pair a watch calls
//! forbidden -- arrives as `Denied`, never as an empty panel or an error string
//! a person has to diagnose. Errors that reach text pass through the same
//! redaction filter as everything else in this crate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use k10s_core::{Capability, KindId};
use kube::Client;

use crate::day2;
use crate::discover::KindTarget;
use crate::forward::{ForwardRegistry, KubeForwarder};
use crate::netpol;

mod helm;
mod inventory;
mod kubernetes;
mod observe;
mod overlay;
mod streams;

pub use inventory::EcosystemFamily;
pub use observe::{GrafanaCatalog, ObserveTools, Seen};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched<T> {
    Ok(T),
    Denied { what: &'static str },
    Failed { what: &'static str, why: String },
}

pub(crate) fn classify<T>(what: &'static str, error: &kube::Error) -> Fetched<T> {
    if let kube::Error::Api(response) = error
        && matches!(response.code, 401 | 403)
    {
        return Fetched::Denied { what };
    }
    Fetched::Failed {
        what,
        why: crate::connect::describe(error as &(dyn std::error::Error + 'static)),
    }
}

/// A failed read in the server's own words: an API status's message, or its
/// code when it carries none, and anything else as [`crate::connect::describe`]
/// words it. kube's Display of an API error is a Debug rendering of the whole
/// Status, which is not a sentence.
pub(crate) fn server_sentence(error: &kube::Error) -> String {
    match error {
        kube::Error::Api(status) => crate::connect::status_sentence(status),
        other => crate::connect::describe(other as &(dyn std::error::Error + 'static)),
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
    interactive: Client,
    handle: tokio::runtime::Handle,
    targets: Arc<[KindTarget]>,
    verdicts: Arc<HashMap<KindId, Capability>>,
    caps: Arc<HashMap<KindId, day2::Caps>>,
    forwards: ForwardRegistry,
    netpol: Arc<Mutex<Option<(Instant, netpol::Inventory)>>>,
}

impl Reader {
    fn target(&self, kind: KindId) -> Option<KindTarget> {
        self.targets.iter().find(|t| t.id == kind).cloned()
    }

    pub(crate) fn new(
        client: Client,
        interactive: Client,
        targets: Vec<KindTarget>,
        verdicts: &[(KindId, Capability)],
        caps: &[(KindId, day2::Caps)],
    ) -> Reader {
        let handle = tokio::runtime::Handle::current();
        Reader {
            forwards: ForwardRegistry::new(Arc::new(KubeForwarder::new(
                client.clone(),
                handle.clone(),
            ))),
            client,
            interactive,
            handle,
            targets: targets.into(),
            verdicts: Arc::new(verdicts.iter().copied().collect()),
            caps: Arc::new(caps.iter().copied().collect()),
            netpol: Arc::new(Mutex::new(None)),
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

    fn api_error(code: u16) -> kube::Error {
        kube::Error::Api(Box::new(kube::core::Status {
            code,
            reason: "Unauthorized".to_string(),
            message: "no".to_string(),
            ..Default::default()
        }))
    }

    #[test]
    fn an_account_the_cluster_refuses_is_denied_whichever_code_it_uses() {
        for code in [401, 403] {
            assert_eq!(
                classify::<()>("pods", &api_error(code)),
                Fetched::Denied { what: "pods" },
                "a watch stops on {code} as forbidden: a panel must say the same thing"
            );
        }
        assert!(matches!(
            classify::<()>("pods", &api_error(500)),
            Fetched::Failed { what: "pods", .. }
        ));
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
