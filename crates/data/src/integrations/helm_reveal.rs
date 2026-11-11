//! Pure Helm payload decoders for caller-owned bytes and unavailable cluster actions.
//!
//! Release Secret values are never requested. Public revision reveal and stored
//! manifest apply refuse before network access, including when called directly.
//! Existing decoders and pure manifest planning tests remain separate from those
//! routes; a scratch buffer is not permission to fetch a Secret payload.

use std::path::PathBuf;

use kube::Client;
use serde::Deserialize;

use crate::apply::ApplyOutcome;
#[cfg(test)]
use crate::apply::{self, ApplyRequest};
use crate::discover::KindTarget;
use crate::integrations::helm;
use crate::reach::Scratch;
use crate::read::Fetched;

// Same decompressed cap as `helm.rs`. A reveal is not a way around it.
const MAX_PAYLOAD_BYTES: usize = 8 << 20;
const MAX_DIFF_LINES: usize = 1_024;

/// Why payload-backed cluster actions are unavailable.
pub const PAYLOAD_UNAVAILABLE: &str =
    "Helm release payload actions are unavailable: Secret values are never fetched";

/// The legacy report format retained for callers of pure payload helpers.
pub const NOT_HELM_ROLLBACK: &str = "not helm rollback (hooks will not run)";

/// One caller-owned decoded revision, with its fields in scratch buffers.
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

/// The legacy stored-apply report shape. The public action now returns one
/// labelled refusal and never applies documents.
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

/// Decode a caller-owned release payload into scratch buffers.
///
/// The bytes belong to the caller. No Kubernetes route invokes this decoder.
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

/// Refuse a revision payload read before any network request.
pub async fn reveal_revision(
    _client: &Client,
    _targets: &[KindTarget],
    _namespace: Option<&str>,
    _name: &str,
    _revision: u32,
) -> Fetched<RevealedRevision> {
    Fetched::Failed {
        what: "helm revision",
        why: PAYLOAD_UNAVAILABLE.to_string(),
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

/// Refuse stored-manifest apply, including a caller-supplied decoded payload.
pub async fn rollback(
    _client: &Client,
    _targets: &[KindTarget],
    revealed: &RevealedRevision,
) -> RollbackReport {
    RollbackReport::wrap(vec![DocumentRollback::Skipped {
        name: revealed.name.clone(),
        kind: String::new(),
        why: PAYLOAD_UNAVAILABLE.to_string(),
    }])
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

#[cfg(test)]
pub(crate) enum Planned {
    Apply(ApplyRequest),
    Skip {
        name: String,
        kind: String,
        why: String,
    },
}

#[cfg(test)]
pub(crate) struct ManifestDoc {
    yaml: String,
    api_version: String,
    kind: String,
    name: String,
    namespace: Option<String>,
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn is_doc_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "---"
        || trimmed.starts_with("--- ")
        || trimmed.starts_with("---\t")
        || trimmed.starts_with("---#")
}

#[cfg(test)]
struct Identity {
    api_version: String,
    kind: String,
    name: String,
    namespace: Option<String>,
}

#[cfg(test)]
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

#[cfg(test)]
fn scalar(line: &str, key: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix(key)?.strip_prefix(':')?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    Some(unquote(rest))
}

#[cfg(test)]
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

#[cfg(test)]
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
    if crate::describe::is_secret(target) {
        return Planned::Skip {
            name: doc.name,
            kind: doc.kind,
            why: "a stored Secret document is not applied: its values are in the revision \
                  payload, and writing them back would send secret material through the \
                  apply path. Use `helm` for a release whose Secrets must move"
                .to_string(),
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

#[cfg(test)]
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

#[cfg(test)]
fn split_gv(api_version: &str) -> (&str, &str) {
    match api_version.rsplit_once('/') {
        Some((group, version)) => (group, version),
        None => ("", api_version),
    }
}

#[cfg(test)]
#[path = "helm_reveal_test.rs"]
mod tests;
