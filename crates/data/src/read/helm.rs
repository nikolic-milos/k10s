//! Metadata-only Helm release inventory and unavailable payload actions.

use super::{Fetched, Reader};
use crate::integrations::{helm, helm_reveal};

impl Reader {
    /// Stored revision labels only, through strict Secret metadata requests.
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

    /// Payload reveal is unavailable under the metadata-only Secret contract.
    pub fn reveal_helm_revision(
        &self,
        _namespace: Option<String>,
        _name: String,
        _revision: u32,
        reply: impl FnOnce(Fetched<helm_reveal::RevealedRevision>) + Send + 'static,
    ) {
        self.handle.spawn(async move {
            reply(Fetched::Failed {
                what: "helm revision",
                why: helm_reveal::PAYLOAD_UNAVAILABLE.to_string(),
            });
        });
    }

    /// Payload diff is unavailable and never reads either stored revision.
    pub fn diff_helm_revisions(
        &self,
        _namespace: Option<String>,
        _name: String,
        _from: u32,
        _to: u32,
        reply: impl FnOnce(Fetched<String>) + Send + 'static,
    ) {
        self.handle.spawn(async move {
            reply(Fetched::Failed {
                what: "helm revision",
                why: helm_reveal::PAYLOAD_UNAVAILABLE.to_string(),
            });
        });
    }

    /// Stored-revision apply refuses before a read or write is sent.
    pub fn rollback_helm_revision(
        &self,
        _namespace: Option<String>,
        _name: String,
        _revision: u32,
        reply: impl FnOnce(Fetched<helm_reveal::RollbackReport>) + Send + 'static,
    ) {
        self.handle.spawn(async move {
            reply(Fetched::Failed {
                what: "helm revision",
                why: format!("nothing was applied: {}", helm_reveal::PAYLOAD_UNAVAILABLE),
            });
        });
    }
}
