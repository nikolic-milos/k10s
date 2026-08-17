//! Helm values and revision manifests after the secret-reveal policy.
//!
//! [`crate::helm`] lists releases by reading Helm's own Secrets, then throws
//! the payload away at that boundary so an inventory has nowhere to put a
//! password. This module is the one later step that boundary exists to force:
//! an explicit reveal of one revision, into [`crate::reach::Scratch`] buffers
//! that zeroize on drop and never sit on [`crate::helm::Revision`].
//!
//! Three fields come out, and only those three. `config` is the user values
//! Helm stored. `chart.values` is the chart's defaults. `manifest` is the
//! rendered multi-document YAML text. Notes and hooks stay in the JSON only
//! long enough for serde to ignore them; they are not a field here.
//!
//! Diffing two revisions compares those scratch buffers and returns the diff
//! text as an owned `String`. That string is the action's result, not an
//! inventory field. Rollback is server-side apply of the stored manifest
//! documents with `fieldManager=k10s`. It is not `helm rollback`: hooks do
//! not run, and the outcome says so. Chart rendering still needs Go templates
//! and Sprig; if a `helm` binary is on PATH this module will name its path,
//! and if it is not the absence is labelled rather than approximated.

use std::collections::BTreeMap;
use std::path::PathBuf;

use kube::Client;
use kube::api::{GetParams, ListParams, Request};
use serde::Deserialize;

use k10s_core::KindId;

use crate::apply::{self, ApplyOutcome, ApplyRequest};
use crate::describe::is_secret;
use crate::discover::KindTarget;
use crate::helm;
use crate::reach::Scratch;
use crate::read::{Fetched, classify, collection_path};

const RELEASE_TYPE: &str = "helm.sh/release.v1";
const OWNER_SELECTOR: &str = "owner=helm";
const PAYLOAD_KEY: &str = "release";
const PAGE_LIMIT: u32 = 200;
// Same decompressed cap as `helm.rs`. A reveal is not a way around it.
const MAX_PAYLOAD_BYTES: usize = 8 << 20;
const MAX_DIFF_LINES: usize = 1_024;

/// Why this write is not `helm rollback`.
pub const NOT_HELM_ROLLBACK: &str = "not helm rollback (hooks will not run)";

/// One revision after an explicit reveal: identity, then the three fields
/// that inventory refused, each in a scratch buffer.
///
/// Not `Clone`. A snapshot, a log, and a saved view are all places these
/// bytes must not go, and a clone is how they would get there.
pub struct RevealedRevision {
    pub name: String,
    pub namespace: String,
    pub revision: u32,
    config: Scratch,
    chart_values: Scratch,
    manifest: Scratch,
}

impl RevealedRevision {
    pub fn config(&self) -> &Scratch {
        &self.config
    }

    pub fn chart_values(&self) -> &Scratch {
        &self.chart_values
    }

    pub fn manifest(&self) -> &Scratch {
        &self.manifest
    }
}

/// Locate `helm` on PATH. Rendering a chart is that binary's job; this crate
/// does not contain a template engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelmBinary {
    Ok(PathBuf),
    Absent { why: &'static str },
}

/// One document of a stored manifest, as rollback applied it (or did not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentRollback {
    Applied {
        name: String,
        kind: String,
        outcome: ApplyOutcome,
    },
    Skipped {
        name: String,
        kind: String,
        why: String,
    },
}

/// Server-side apply of a stored revision's manifest documents.
///
/// [`RollbackReport::note`] is always [`NOT_HELM_ROLLBACK`]. Hooks live in a
/// different field of the payload and are never applied here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackReport {
    pub note: &'static str,
    pub documents: Vec<DocumentRollback>,
}

impl RollbackReport {
    fn wrap(documents: Vec<DocumentRollback>) -> RollbackReport {
        RollbackReport {
            note: NOT_HELM_ROLLBACK,
            documents,
        }
    }
}

#[derive(Deserialize)]
struct WireReveal {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    config: serde_json::Value,
    #[serde(default)]
    chart: WireRevealChart,
    #[serde(default)]
    manifest: String,
}

#[derive(Deserialize, Default)]
struct WireRevealChart {
    #[serde(default)]
    values: serde_json::Value,
}

#[derive(Deserialize)]
struct WireList {
    #[serde(default)]
    metadata: WireListMeta,
    #[serde(default)]
    items: Vec<WireSecret>,
}

#[derive(Deserialize, Default)]
struct WireListMeta {
    #[serde(default, rename = "continue")]
    cont: String,
}

#[derive(Deserialize)]
struct WireSecret {
    #[serde(default)]
    metadata: WireSecretMeta,
    #[serde(default)]
    data: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
struct WireSecretMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
}

/// Decode one already-fetched release payload into scratch buffers.
///
/// The caller has already decided to reveal this revision. Inventory still
/// goes through [`helm::decode`], which cannot carry these fields.
pub fn reveal_payload(encoded: &str) -> Result<RevealedRevision, &'static str> {
    let wire: WireReveal = {
        let json = helm::decode_scratch(encoded)?;
        serde_json::from_slice(json.as_bytes())
            .map_err(|_| "this release's payload is not a Helm release document")?
    };
    if wire.manifest.len() > MAX_PAYLOAD_BYTES {
        return Err("this release's payload is larger than this view decodes");
    }
    Ok(RevealedRevision {
        name: wire.name,
        namespace: wire.namespace,
        revision: wire.version,
        config: Scratch::from_bytes(json_bytes(&wire.config)?),
        chart_values: Scratch::from_bytes(json_bytes(&wire.chart.values)?),
        manifest: Scratch::from_bytes(wire.manifest.into_bytes()),
    })
}

/// Fetch one stored revision and reveal it. The list is still narrowed to
/// Helm's own Secrets; the difference from an inventory is that this decode
/// keeps the values.
pub async fn reveal_revision(
    client: &Client,
    targets: &[KindTarget],
    namespace: Option<&str>,
    name: &str,
    revision: u32,
) -> Fetched<RevealedRevision> {
    let Some(target) = targets.iter().find(|target| is_secret(target)) else {
        return Fetched::Failed {
            what: "helm revision",
            why: "the cluster does not serve Secrets, so stored releases cannot be read"
                .to_string(),
        };
    };
    if !target.listable {
        return Fetched::Failed {
            what: "helm revision",
            why: "the server serves Secrets without a list verb, so stored releases cannot be read"
                .to_string(),
        };
    }
    if !release_name_ok(name) {
        return Fetched::Failed {
            what: "helm revision",
            why: "that is not a Helm release name".to_string(),
        };
    }
    match fetch_encoded(client, target, namespace, name, revision).await {
        Fetched::Ok(encoded) => match reveal_payload(&encoded.payload) {
            Ok(mut revealed) => {
                if revealed.name.is_empty() {
                    revealed.name = if encoded.secret_name.is_empty() {
                        name.to_string()
                    } else {
                        encoded.secret_name
                    };
                }
                if revealed.namespace.is_empty() {
                    revealed.namespace = if !encoded.secret_namespace.is_empty() {
                        encoded.secret_namespace
                    } else if let Some(namespace) = namespace {
                        namespace.to_string()
                    } else {
                        String::new()
                    };
                }
                Fetched::Ok(revealed)
            }
            Err(why) => Fetched::Failed {
                what: "helm revision",
                why: why.to_string(),
            },
        },
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

/// Diff of two revisions' user values. The return is the diff text, not a
/// type that could join an inventory.
pub fn diff_values(from: &RevealedRevision, to: &RevealedRevision) -> String {
    let left = from.config.as_bytes();
    let right = to.config.as_bytes();
    if left == right {
        return format!(
            "the user values of revision {} and revision {} are identical",
            from.revision, to.revision
        );
    }
    let Ok(left) = from.config.as_str() else {
        return format!(
            "the user values of revision {} and revision {} differ and are not UTF-8",
            from.revision, to.revision
        );
    };
    let Ok(right) = to.config.as_str() else {
        return format!(
            "the user values of revision {} and revision {} differ and are not UTF-8",
            from.revision, to.revision
        );
    };
    let left_lines = left.lines().count();
    let right_lines = right.lines().count();
    if left_lines > MAX_DIFF_LINES || right_lines > MAX_DIFF_LINES {
        return format!(
            "the user values of revision {} and revision {} differ and are too large to diff here",
            from.revision, to.revision
        );
    }
    unified(
        &format!("user values, revision {}", from.revision),
        left,
        &format!("user values, revision {}", to.revision),
        right,
    )
}

/// Server-side apply of the stored manifest. Not Helm rollback: hooks will
/// not run.
pub async fn rollback(
    client: &Client,
    targets: &[KindTarget],
    revealed: &RevealedRevision,
) -> RollbackReport {
    let Ok(text) = revealed.manifest.as_str() else {
        return RollbackReport::wrap(vec![DocumentRollback::Skipped {
            name: revealed.name.clone(),
            kind: String::new(),
            why: "this revision's manifest is not UTF-8".to_string(),
        }]);
    };
    let planned = plan_rollback(targets, &revealed.namespace, text);
    if planned.is_empty() {
        return RollbackReport::wrap(vec![DocumentRollback::Skipped {
            name: revealed.name.clone(),
            kind: String::new(),
            why: "this revision stored no rendered manifest".to_string(),
        }]);
    }
    let mut documents = Vec::new();
    for item in planned {
        match item {
            Planned::Skip { name, kind, why } => {
                documents.push(DocumentRollback::Skipped { name, kind, why });
            }
            Planned::Apply(request) => {
                let name = request.name.clone();
                let kind = kind_name(targets, request.kind);
                let outcome = apply::apply(client, targets, &request).await;
                documents.push(DocumentRollback::Applied {
                    name,
                    kind,
                    outcome,
                });
            }
        }
    }
    RollbackReport::wrap(documents)
}

pub fn helm_binary() -> HelmBinary {
    match find_on_path("helm", &std::env::var("PATH").unwrap_or_default()) {
        Some(path) => HelmBinary::Ok(path),
        None => HelmBinary::Absent {
            why: "helm binary not on PATH",
        },
    }
}

fn find_on_path(bin: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file().then_some(candidate)
    })
}

fn json_bytes(value: &serde_json::Value) -> Result<Vec<u8>, &'static str> {
    let empty = serde_json::json!({});
    let value = if value.is_null() { &empty } else { value };
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|_| "this release's values are not JSON")?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err("this release's payload is larger than this view decodes");
    }
    if !bytes.ends_with(&[b'\n']) {
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn release_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && name.len() <= 53
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

struct Encoded {
    payload: String,
    secret_name: String,
    secret_namespace: String,
}

fn encoded_of(secret: WireSecret) -> Option<Encoded> {
    Some(Encoded {
        payload: secret.data.get(PAYLOAD_KEY)?.clone(),
        secret_name: secret.metadata.name,
        secret_namespace: secret.metadata.namespace,
    })
}

async fn fetch_encoded(
    client: &Client,
    target: &KindTarget,
    namespace: Option<&str>,
    name: &str,
    revision: u32,
) -> Fetched<Encoded> {
    let path = collection_path(target, namespace);
    let fields = format!("type={RELEASE_TYPE}");
    let labels = format!("{OWNER_SELECTOR},name={name},version={revision}");
    let mut token: Option<String> = None;
    loop {
        let mut params = ListParams::default()
            .limit(PAGE_LIMIT)
            .labels(&labels)
            .fields(&fields);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = match Request::new(path.clone()).list(&params) {
            Ok(request) => request,
            Err(error) => {
                return Fetched::Failed {
                    what: "helm revision",
                    why: error.to_string(),
                };
            }
        };
        let page = match client.request::<WireList>(request).await {
            Ok(page) => page,
            Err(error) => return classify("helm revision", &error),
        };
        for secret in page.items {
            if let Some(encoded) = encoded_of(secret) {
                return Fetched::Ok(encoded);
            }
        }
        token = (!page.metadata.cont.is_empty()).then_some(page.metadata.cont);
        if token.is_none() {
            break;
        }
    }
    if let Some(namespace) = namespace {
        return get_conventional(client, target, namespace, name, revision).await;
    }
    Fetched::Failed {
        what: "helm revision",
        why: format!("no stored revision {revision} of {name}"),
    }
}

async fn get_conventional(
    client: &Client,
    target: &KindTarget,
    namespace: &str,
    name: &str,
    revision: u32,
) -> Fetched<Encoded> {
    let secret_name = format!("sh.helm.release.v1.{name}.v{revision}");
    let request = match Request::new(collection_path(target, Some(namespace)))
        .get(&secret_name, &GetParams::default())
    {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what: "helm revision",
                why: error.to_string(),
            };
        }
    };
    match client.request::<WireSecret>(request).await {
        Ok(secret) => match encoded_of(secret) {
            Some(encoded) => Fetched::Ok(encoded),
            None => Fetched::Failed {
                what: "helm revision",
                why: format!("no stored revision {revision} of {name}"),
            },
        },
        Err(error) => classify("helm revision", &error),
    }
}

enum Edit<'a> {
    Keep(&'a str),
    Del(&'a str),
    Ins(&'a str),
}

fn unified(left_title: &str, left: &str, right_title: &str, right: &str) -> String {
    let a: Vec<&str> = left.lines().collect();
    let b: Vec<&str> = right.lines().collect();
    let mut out = format!("--- {left_title}\n+++ {right_title}\n");
    for edit in edits(&a, &b) {
        match edit {
            Edit::Keep(line) => {
                out.push(' ');
                out.push_str(line);
                out.push('\n');
            }
            Edit::Del(line) => {
                out.push('-');
                out.push_str(line);
                out.push('\n');
            }
            Edit::Ins(line) => {
                out.push('+');
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

fn edits<'a>(a: &'a [&str], b: &'a [&str]) -> Vec<Edit<'a>> {
    let n = a.len();
    let m = b.len();
    // One flat (n+1)x(m+1) buffer: a Vec per row would be over a thousand
    // allocations at MAX_DIFF_LINES.
    let w = m + 1;
    let mut dp = vec![0u32; (n + 1) * w];
    for i in 0..n {
        for j in 0..m {
            dp[(i + 1) * w + j + 1] = if a[i] == b[j] {
                dp[i * w + j] + 1
            } else {
                dp[i * w + j + 1].max(dp[(i + 1) * w + j])
            };
        }
    }
    let mut edits = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            edits.push(Edit::Keep(a[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i * w + j - 1] >= dp[i.saturating_sub(1) * w + j]) {
            edits.push(Edit::Ins(b[j - 1]));
            j -= 1;
        } else {
            edits.push(Edit::Del(a[i - 1]));
            i -= 1;
        }
    }
    edits.reverse();
    edits
}

pub(crate) enum Planned {
    Apply(ApplyRequest),
    Skip {
        name: String,
        kind: String,
        why: String,
    },
}

pub(crate) struct ManifestDoc {
    yaml: String,
    api_version: String,
    kind: String,
    name: String,
    namespace: Option<String>,
}

pub(crate) fn plan_rollback(
    targets: &[KindTarget],
    release_namespace: &str,
    manifest: &str,
) -> Vec<Planned> {
    split_manifest(manifest)
        .into_iter()
        .map(|doc| plan_one(targets, release_namespace, doc))
        .collect()
}

pub(crate) fn split_manifest(text: &str) -> Vec<ManifestDoc> {
    let mut docs = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if is_doc_start(line) {
            if let Some(doc) = take_doc(&mut current) {
                docs.push(doc);
            }
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if let Some(doc) = take_doc(&mut current) {
        docs.push(doc);
    }
    docs
}

fn take_doc(current: &mut String) -> Option<ManifestDoc> {
    if current.trim().is_empty() {
        current.clear();
        return None;
    }
    let yaml = std::mem::take(current);
    let identity = identity(&yaml)?;
    Some(ManifestDoc {
        yaml,
        api_version: identity.api_version,
        kind: identity.kind,
        name: identity.name,
        namespace: identity.namespace,
    })
}

fn is_doc_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "---"
        || trimmed.starts_with("--- ")
        || trimmed.starts_with("---\t")
        || trimmed.starts_with("---#")
}

struct Identity {
    api_version: String,
    kind: String,
    name: String,
    namespace: Option<String>,
}

fn identity(doc: &str) -> Option<Identity> {
    let mut api_version = String::new();
    let mut kind = String::new();
    let mut name = String::new();
    let mut namespace = None;
    let mut in_metadata = false;
    let mut metadata_indent: Option<usize> = None;
    for line in doc.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if in_metadata {
            if indent == 0 {
                in_metadata = false;
            } else {
                if metadata_indent.is_none() {
                    metadata_indent = Some(indent);
                }
                if metadata_indent == Some(indent) {
                    if let Some(value) = scalar(line, "name") {
                        name = value;
                    } else if let Some(value) = scalar(line, "namespace") {
                        namespace = Some(value);
                    }
                }
                continue;
            }
        }
        if indent != 0 {
            continue;
        }
        if let Some(value) = scalar(line, "apiVersion") {
            api_version = value;
        } else if let Some(value) = scalar(line, "kind") {
            kind = value;
        } else if let Some(rest) = trimmed.strip_prefix("metadata:") {
            if rest.trim().is_empty() {
                in_metadata = true;
            }
        }
    }
    if kind.is_empty() || name.is_empty() {
        return None;
    }
    Some(Identity {
        api_version,
        kind,
        name,
        namespace,
    })
}

fn scalar(line: &str, key: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix(key)?.strip_prefix(':')?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    Some(unquote(rest))
}

fn unquote(s: &str) -> String {
    let s = match s.find(" #") {
        Some(at) => s[..at].trim(),
        None => s,
    };
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

fn plan_one(targets: &[KindTarget], release_namespace: &str, doc: ManifestDoc) -> Planned {
    let Some(target) = find_target(targets, &doc.api_version, &doc.kind) else {
        return Planned::Skip {
            name: doc.name,
            kind: doc.kind,
            why: "this kind is not served by the connected cluster".to_string(),
        };
    };
    if !target.patchable {
        return Planned::Skip {
            name: doc.name,
            kind: doc.kind,
            why: format!(
                "the server serves {} without a patch verb, so it cannot be applied",
                target.kind()
            ),
        };
    }
    let namespace = if target.namespaced {
        let ns = doc
            .namespace
            .as_deref()
            .filter(|ns| !ns.is_empty())
            .unwrap_or(release_namespace);
        if ns.is_empty() {
            return Planned::Skip {
                name: doc.name,
                kind: doc.kind,
                why: "this namespaced document has no namespace".to_string(),
            };
        }
        Some(ns.to_string())
    } else {
        None
    };
    Planned::Apply(ApplyRequest {
        kind: target.id,
        namespace,
        name: doc.name,
        yaml: doc.yaml,
        dry_run: false,
        force: false,
    })
}

fn find_target<'a>(
    targets: &'a [KindTarget],
    api_version: &str,
    kind: &str,
) -> Option<&'a KindTarget> {
    let (group, version) = split_gv(api_version);
    targets
        .iter()
        .find(|target| {
            target.kind() == kind && target.group() == group && target.resource.version == version
        })
        .or_else(|| {
            targets
                .iter()
                .find(|target| target.kind() == kind && target.group() == group)
        })
}

fn split_gv(api_version: &str) -> (&str, &str) {
    match api_version.rsplit_once('/') {
        Some((group, version)) => (group, version),
        None => ("", api_version),
    }
}

fn kind_name(targets: &[KindTarget], id: KindId) -> String {
    targets
        .iter()
        .find(|target| target.id == id)
        .map(|target| target.kind().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "helm_reveal_test.rs"]
mod tests;
