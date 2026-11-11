//! Kubernetes reads and confirmed object writes.

use super::{Fetched, Reader};
use crate::apply::{ApplyOutcome, ApplyRequest};
use crate::browse::TablePage;
use crate::describe::{DescribeRequest, Described};
use crate::integrations::flux;
use crate::{apply, browse, day2, describe, logs, manifest, nodes, openapi};
use k10s_core::KindId;
use std::time::{SystemTime, UNIX_EPOCH};

impl Reader {
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

    // Server-side apply. Dry run and apply are the same request with one query
    // parameter between them, which is why they are one method: a caller cannot
    // reach the apply without having been able to reach the dry run. A dry run
    // reads the object first, so it is two requests and waits at most two
    // interactive deadlines. Day-2 clicks are a different method; they are not
    // documents.
    pub fn apply(&self, request: ApplyRequest, reply: impl FnOnce(ApplyOutcome) + Send + 'static) {
        let client = self.interactive.clone();
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

    /// Scale, rollout, delete, evict, cordon, drain, debug. Caps are filled
    /// here from the probe so a caller cannot skip the gate; confirm still
    /// lives on the request, so the first press never writes (a drain's ask
    /// reads the node's pods). Each request waits at most the interactive
    /// deadline, and a drain makes at most eighteen: its plan read, the
    /// cordon and sixteen evictions.
    pub fn day2(
        &self,
        kind: KindId,
        mut call: day2::Day2Call,
        reply: impl FnOnce(day2::Day2Outcome) + Send + 'static,
    ) {
        let Some(target) = self.target(kind) else {
            reply(day2::Day2Outcome::Failed {
                why: "this kind is not served by the connected cluster".to_string(),
            });
            return;
        };
        call.set_caps(self.caps_for(kind));
        if let day2::Day2Call::Rollout(request) = &mut call
            && let day2::RolloutAction::Restart { restarted_at } = &mut request.action
            && restarted_at.is_empty()
        {
            let elapsed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            *restarted_at = flux::rfc3339(elapsed.as_secs(), elapsed.subsec_nanos());
        }
        let client = self.interactive.clone();
        self.handle.spawn(async move {
            reply(day2::run(&client, &target, &call).await);
        });
    }

    fn caps_for(&self, kind: KindId) -> day2::Caps {
        self.caps.get(&kind).copied().unwrap_or_default()
    }

    pub fn fetch_node_table(&self, reply: impl FnOnce(Fetched<TablePage>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(nodes::fetch_node_table(&client).await);
        });
    }
}
