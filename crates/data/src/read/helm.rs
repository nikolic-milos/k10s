//! Stored Helm releases and explicitly confirmed manifest applies.

use super::{Fetched, Reader};
use crate::integrations::{helm, helm_reveal};
use kube::Client;

impl Reader {
    // Helm's own release state, read out of the Secrets it writes: nothing
    // installed, nothing templated, and the narrowest list the server will serve
    // -- `type=helm.sh/release.v1` and `owner=helm` -- so no Secret that is not a
    // release ever crosses the wire.
    pub fn fetch_releases(
        &self,
        namespace: Option<String>,
        reply: impl FnOnce(Fetched<helm::Releases>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(helm::fetch_releases(&client, &targets, namespace.as_deref()).await);
        });
    }

    /// One stored revision after an explicit reveal. The answer is
    /// [`helm_reveal::RevealedRevision`]: not `Clone`, and not an inventory.
    pub fn reveal_helm_revision(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: impl FnOnce(Fetched<helm_reveal::RevealedRevision>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(
                helm_reveal::reveal_revision(
                    &client,
                    &targets,
                    namespace.as_deref(),
                    &name,
                    revision,
                )
                .await,
            );
        });
    }

    /// Diff of two revisions as an owned string. Values use
    /// [`helm_reveal::diff_values`]; manifests are compared here and never
    /// stored on a release row.
    pub fn diff_helm_revisions(
        &self,
        namespace: Option<String>,
        name: String,
        from: u32,
        to: u32,
        reply: impl FnOnce(Fetched<String>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(
                diff_helm_revisions(&client, &targets, namespace.as_deref(), &name, from, to).await,
            );
        });
    }

    /// Server-side apply of a stored revision. The report's note is always
    /// [`helm_reveal::NOT_HELM_ROLLBACK`]. Each request waits at most the
    /// interactive deadline: the list of the revision's release Secret
    /// (selected by name and version), a read of it by its conventional name
    /// when the list finds none, and one apply per document. It stops at the
    /// first document that goes unconfirmed or cannot be sent, so a server that
    /// stopped answering holds it for at most three deadlines: each request's
    /// deadline includes connecting to it.
    pub fn rollback_helm_revision(
        &self,
        namespace: Option<String>,
        name: String,
        revision: u32,
        reply: impl FnOnce(Fetched<helm_reveal::RollbackReport>) + Send + 'static,
    ) {
        let client = self.interactive.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(
                rollback_helm_revision(&client, &targets, namespace.as_deref(), &name, revision)
                    .await,
            );
        });
    }
}
async fn diff_helm_revisions(
    client: &Client,
    targets: &[crate::discover::KindTarget],
    namespace: Option<&str>,
    name: &str,
    from: u32,
    to: u32,
) -> Fetched<String> {
    let left = helm_reveal::reveal_revision(client, targets, namespace, name, from).await;
    let right = helm_reveal::reveal_revision(client, targets, namespace, name, to).await;
    match (left, right) {
        (Fetched::Ok(from_rev), Fetched::Ok(to_rev)) => {
            let values = helm_reveal::diff_values(&from_rev, &to_rev);
            let manifests = match (from_rev.manifest().as_str(), to_rev.manifest().as_str()) {
                (Ok(left), Ok(right)) if left == right => format!(
                    "the stored manifests of revision {} and revision {} are identical",
                    from_rev.revision, to_rev.revision
                ),
                (Ok(left), Ok(right)) => format!(
                    "the stored manifests of revision {} and revision {} differ ({} vs {} lines)",
                    from_rev.revision,
                    to_rev.revision,
                    left.lines().count(),
                    right.lines().count()
                ),
                _ => format!(
                    "the stored manifests of revision {} and revision {} differ and are not UTF-8",
                    from_rev.revision, to_rev.revision
                ),
            };
            Fetched::Ok(format!("{values}\n\n{manifests}\n"))
        }
        (Fetched::Denied { what }, _) | (_, Fetched::Denied { what }) => Fetched::Denied { what },
        (Fetched::Failed { what, why }, _) | (_, Fetched::Failed { what, why }) => {
            Fetched::Failed { what, why }
        }
    }
}

async fn rollback_helm_revision(
    client: &Client,
    targets: &[crate::discover::KindTarget],
    namespace: Option<&str>,
    name: &str,
    revision: u32,
) -> Fetched<helm_reveal::RollbackReport> {
    match helm_reveal::reveal_revision(client, targets, namespace, name, revision).await {
        Fetched::Ok(revealed) => {
            Fetched::Ok(helm_reveal::rollback(client, targets, &revealed).await)
        }
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed {
            what,
            why: format!("nothing was applied: {why}"),
        },
    }
}
