//! Helm releases, read out of the Secrets Helm itself writes.
//!
//! This is the ecosystem thesis in its smallest honest form. Helm's release
//! state is not in an API of Helm's: it is one Secret per revision, of type
//! `helm.sh/release.v1`, labelled `owner=helm`, holding a gzipped JSON document
//! Helm's own client wrote. So a release inventory costs nothing but a list and
//! a decode -- no operator, no CRD, no `helm` binary, nothing installed, and
//! nothing reimplemented. §1's second rule is that we drive the ecosystem's
//! tools rather than rebuild them, and this is the read half of driving Helm.
//!
//! What this deliberately does **not** do is template. `helm template` needs Go
//! `text/template` plus Sprig, and a Rust approximation of those diverges
//! silently and produces wrong manifests -- §5.3 marks it `rebuild` for exactly
//! that reason. Stored state is read natively; rendering charts is not our
//! business.
//!
//! **The Secret rule, and the one place the read path bends it.** Everywhere
//! else, a Secret is fetched as `PartialObjectMetadata` and its values cannot
//! reach a document because they were never fetched. A release payload *is* the
//! Secret's value, so this path has to read one. Three things keep that narrow:
//!
//! 1. The list asks the server for `type=helm.sh/release.v1` and
//!    `owner=helm` -- a field selector and a label selector -- so no other
//!    Secret's data is ever transferred, let alone parsed.
//! 2. The payload is reduced at this boundary to the fields an inventory shows:
//!    identity, revision, status, chart, and the sentence Helm wrote about the
//!    release. The rendered manifest, the user's values, the chart's default
//!    values and the hooks are **not carried into any type this module returns**,
//!    so nothing downstream can leak what nothing downstream holds. That is
//!    structural, not a convention: [`Revision`] has nowhere to put them.
//! 3. Which is why the values and manifest views of §5.3's later rows are not
//!    here. They need §5.8's reveal policy -- explicit, per field, into a
//!    scratch buffer that never enters a snapshot -- and a release manifest is
//!    multi-document YAML text rather than a JSON object, so it cannot be
//!    structurally stripped the way [`crate::manifest`] strips one object. That
//!    is a design problem to solve deliberately, not a line to add here.
//!
//! Three bounds exist because every side of this is attacker-shaped data. A
//! gzip member expands without limit unless something says otherwise, so the
//! decompressed payload is read through a cap and refused rather than
//! truncated; the listing is paged with a ceiling on how many revisions it will
//! hold, reported rather than silently applied; and every field a payload
//! contributes is clipped on its own, because the payload cap is not a field cap
//! -- an eight-mebibyte `status` fits inside it, and [`render`] pads every
//! revision of a release to the widest status in it, so one field would multiply
//! by the revision count into a document nothing downstream bounds.

use std::collections::BTreeMap;
use std::io::Read;

use base64::Engine;
use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;

use crate::describe::is_secret;
use crate::discover::KindTarget;
use crate::read::{Fetched, classify, collection_path};

// Helm's own type and label. Both are asked for on the wire rather than
// filtered here, so the transfer itself is narrowed to release Secrets.
const RELEASE_TYPE: &str = "helm.sh/release.v1";
const OWNER_SELECTOR: &str = "owner=helm";
const PAYLOAD_KEY: &str = "release";

const PAGE_LIMIT: u32 = 200;
// Helm keeps ten revisions per release by default and a cluster can hold many
// releases; this is a ceiling on the whole listing, and reaching it is stated
// rather than hidden.
const MAX_REVISIONS: usize = 2_000;
// A gzip member expands without bound unless a reader bounds it. A release
// payload is a rendered manifest and some values; one larger than this is not
// something an inventory can show anyway.
const MAX_PAYLOAD_BYTES: usize = 8 << 20;
// And every *field* the payload contributes is bounded on its own, because the
// payload bound alone is not one: a status of eight mebibytes is inside it, and
// `render` pads every revision of a release to the widest status in it, so one
// field multiplies by the revision count. `describe` bounds its lines for the
// same reason and this is the same document. Chars rather than bytes: the cut has
// to land on a character.
const MAX_FIELD_CHARS: usize = 200;

/// One stored revision of a release, reduced to what an inventory shows.
///
/// There is deliberately nowhere here for the rendered manifest, the values, or
/// the hooks: see this module's contract. Adding a field for any of them is a
/// decision about secret exposure, not a convenience.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub revision: u32,
    // Helm's own word: deployed, superseded, failed, uninstalled,
    // pending-upgrade. Carried as text rather than mapped onto an enum, because
    // a status this build has never heard of is still the status.
    pub status: String,
    // `info.last_deployed`, as the payload spells it. Not parsed into a time
    // type: nothing here does arithmetic on it, and reformatting somebody's
    // timestamp is a way to be subtly wrong about a time zone.
    pub updated: String,
    // The sentence Helm wrote about this revision ("Upgrade complete").
    pub description: String,
    pub chart: String,
    pub chart_version: String,
    pub app_version: String,
}

/// One release: every stored revision of it, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub name: String,
    pub namespace: String,
    pub revisions: Vec<Revision>,
}

impl Release {
    /// The revision a cluster is actually running, which is the highest one
    /// stored. Absent only for a release with no revisions, which this module
    /// never builds.
    pub fn current(&self) -> Option<&Revision> {
        self.revisions.first()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Releases {
    pub releases: Vec<Release>,
    /// The listing stopped at its ceiling, so this is some of the releases
    /// rather than all of them.
    pub truncated: bool,
    /// Release Secrets whose payload would not decode. Counted, never dropped
    /// silently: a release that cannot be read is not a release that is not
    /// there, and the difference matters to whoever is looking for it.
    pub unreadable: usize,
}

// Exactly the fields above, and no others. serde ignores the rest of the
// payload, which is how the manifest and the values stay out of this process's
// long-lived memory rather than being carried and then not shown.
#[derive(Deserialize)]
struct WireRelease {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    info: WireInfo,
    #[serde(default)]
    chart: WireChart,
}

#[derive(Deserialize, Default)]
struct WireInfo {
    #[serde(default)]
    status: String,
    #[serde(default)]
    last_deployed: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize, Default)]
struct WireChart {
    #[serde(default)]
    metadata: WireChartMeta,
}

#[derive(Deserialize, Default)]
struct WireChartMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default, rename = "appVersion")]
    app_version: String,
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

/// A decoded payload: which release it belongs to, and the revision it is.
pub(crate) struct Stored {
    pub(crate) name: String,
    pub(crate) namespace: String,
    pub(crate) revision: Revision,
}

/// Decode one release Secret's payload.
///
/// Helm gzips the JSON and base64s the result, and the API server base64s
/// whatever is in `data`, so a raw read carries two layers and a typed client's
/// read carries one. Which it is is decided by looking at the bytes rather than
/// by trusting the caller to know: gzip announces itself with two magic bytes,
/// and JSON with a brace. A version of Helm that stopped compressing is
/// therefore read by the same function.
pub(crate) fn decode(encoded: &str) -> Result<Stored, &'static str> {
    let json = payload(encoded)?;
    let wire: WireRelease = serde_json::from_slice(&json)
        .map_err(|_| "this release's payload is not a Helm release document")?;
    Ok(Stored {
        name: clipped(wire.name),
        namespace: clipped(wire.namespace),
        revision: Revision {
            revision: wire.version,
            status: clipped(wire.info.status),
            updated: clipped(wire.info.last_deployed),
            description: clipped(wire.info.description),
            chart: clipped(wire.chart.metadata.name),
            chart_version: clipped(wire.chart.metadata.version),
            app_version: clipped(wire.chart.metadata.app_version),
        },
    })
}

/// The same gzip/base64 layers [`decode`] reads, returned as a scratch buffer
/// rather than reduced to an inventory. [`Revision`] still has nowhere to put
/// the values; this is the reveal path's entrance, and the only other one.
pub(crate) fn decode_scratch(encoded: &str) -> Result<crate::reach::Scratch, &'static str> {
    Ok(crate::reach::Scratch::from_bytes(payload(encoded)?))
}

// One field, at a length a line can hold. Truncated with the ellipsis
// `describe` uses, so a clipped value looks clipped rather than merely short.
fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

enum Shape {
    Gzip,
    Json,
    Neither,
}

fn shape(bytes: &[u8]) -> Shape {
    match bytes {
        [0x1f, 0x8b, ..] => Shape::Gzip,
        _ => match bytes.iter().find(|byte| !byte.is_ascii_whitespace()) {
            Some(b'{') => Shape::Json,
            _ => Shape::Neither,
        },
    }
}

fn payload(encoded: &str) -> Result<Vec<u8>, &'static str> {
    let engine = base64::engine::general_purpose::STANDARD;
    let once = engine
        .decode(encoded.trim())
        .map_err(|_| "this release's payload is not base64")?;
    let bytes = match shape(&once) {
        Shape::Gzip | Shape::Json => once,
        // The layer Helm applied itself, under the one the API server applied.
        // At most one extra round: a third would be guessing.
        Shape::Neither => {
            let inner = std::str::from_utf8(&once)
                .map_err(|_| "this release's payload is neither gzip, JSON, nor base64")?;
            engine
                .decode(inner.trim())
                .map_err(|_| "this release's payload is neither gzip, JSON, nor base64")?
        }
    };
    match shape(&bytes) {
        Shape::Gzip => inflate(&bytes),
        Shape::Json => Ok(bytes),
        Shape::Neither => Err("this release's payload is neither gzip nor JSON"),
    }
}

// Bounded on the *decompressed* side, which is the side an attacker chooses: a
// few hundred compressed bytes can name gigabytes. Refused rather than
// truncated, because half a JSON document is not a release.
fn inflate(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_PAYLOAD_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| "this release's payload did not decompress")?;
    if out.len() > MAX_PAYLOAD_BYTES {
        return Err("this release's payload is larger than this view decodes");
    }
    Ok(out)
}

pub(crate) async fn fetch_releases(
    client: &Client,
    targets: &[KindTarget],
    namespace: Option<&str>,
) -> Fetched<Releases> {
    let Some(target) = targets.iter().find(|target| is_secret(target)) else {
        // A cluster that does not serve Secrets cannot be storing Helm releases
        // in them. Absent is invisible, not broken -- §1's discovery rule.
        return Fetched::Ok(Releases::default());
    };
    if !target.listable {
        return Fetched::Failed {
            what: "helm releases",
            why: "the server serves Secrets without a list verb, so stored releases cannot be \
                  read"
                .to_string(),
        };
    }
    let path = collection_path(target, namespace);
    let fields = format!("type={RELEASE_TYPE}");
    let mut stored: BTreeMap<(String, String), Vec<Revision>> = BTreeMap::new();
    let mut token: Option<String> = None;
    let mut held = 0usize;
    let mut unreadable = 0usize;
    let mut truncated = false;
    loop {
        let mut params = ListParams::default()
            .limit(PAGE_LIMIT)
            .labels(OWNER_SELECTOR)
            .fields(&fields);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = match Request::new(path.clone()).list(&params) {
            Ok(request) => request,
            Err(error) => {
                return Fetched::Failed {
                    what: "helm releases",
                    why: error.to_string(),
                };
            }
        };
        let page = match client.request::<WireList>(request).await {
            Ok(page) => page,
            Err(error) => return classify("helm releases", &error),
        };
        for secret in page.items {
            if held == MAX_REVISIONS {
                truncated = true;
                break;
            }
            let Some(encoded) = secret.data.get(PAYLOAD_KEY) else {
                unreadable += 1;
                continue;
            };
            match decode(encoded) {
                Ok(found) => {
                    held += 1;
                    // The payload's own identity first: it is what Helm wrote,
                    // where the Secret's name is a convention about spelling it.
                    // The Secret's namespace stands in when the payload has
                    // none, since that is where the release is stored.
                    let name = if found.name.is_empty() {
                        secret.metadata.name.clone()
                    } else {
                        found.name
                    };
                    let namespace = if found.namespace.is_empty() {
                        secret.metadata.namespace.clone()
                    } else {
                        found.namespace
                    };
                    stored
                        .entry((namespace, name))
                        .or_default()
                        .push(found.revision);
                }
                Err(_) => unreadable += 1,
            }
        }
        token = (!page.metadata.cont.is_empty() && !truncated).then_some(page.metadata.cont);
        if token.is_none() {
            break;
        }
    }
    let releases = stored
        .into_iter()
        .map(|((namespace, name), mut revisions)| {
            // Newest first, which is the order a person reads a history in, and
            // it makes the running revision the first element rather than a
            // search.
            revisions.sort_by_key(|revision| std::cmp::Reverse(revision.revision));
            Release {
                name,
                namespace,
                revisions,
            }
        })
        .collect();
    Fetched::Ok(Releases {
        releases,
        truncated,
        unreadable,
    })
}

/// The inventory as a document, rendered here for the same reason a describe is:
/// the shell's text item shows lines, and one deterministic rendering is what
/// makes it gateable by a test rather than by a screenshot.
pub fn render(releases: &Releases) -> Vec<String> {
    let mut lines = Vec::new();
    if releases.releases.is_empty() && releases.unreadable == 0 {
        lines.push("no Helm releases are stored in this cluster".to_string());
        lines.push(String::new());
        lines.push(
            "this reads the Secrets Helm itself writes (type helm.sh/release.v1, label \
             owner=helm); nothing is installed to find them, so a cluster that uses the ConfigMap \
             storage driver, or a namespace this account cannot list, shows as empty here"
                .to_string(),
        );
    } else if releases.releases.is_empty() {
        // Not "no releases are stored": some are, and they were seen. Saying the
        // cluster is empty here would contradict the very field that counted
        // them, and it is the loudest line in the document.
        lines.push(
            "no Helm release could be read here, though some are stored: every release Secret \
             this account can see failed to decode"
                .to_string(),
        );
    } else {
        let count = releases.releases.len();
        let revisions: usize = releases
            .releases
            .iter()
            .map(|release| release.revisions.len())
            .sum();
        lines.push(format!(
            "{} {}, {} stored {}",
            count,
            plural(count, "release"),
            revisions,
            plural(revisions, "revision"),
        ));
    }
    if releases.truncated {
        lines.push(format!(
            "the listing stopped at {MAX_REVISIONS} revisions, so this is some of them rather \
             than all",
        ));
    }
    if releases.unreadable > 0 {
        lines.push(format!(
            "{} release {} could not be decoded and {} not shown",
            releases.unreadable,
            plural(releases.unreadable, "secret"),
            if releases.unreadable == 1 {
                "is"
            } else {
                "are"
            },
        ));
    }
    for release in &releases.releases {
        lines.push(String::new());
        lines.push(format!("{}/{}", release.namespace, release.name));
        let width = release
            .revisions
            .iter()
            .map(|revision| revision.status.chars().count())
            .max()
            .unwrap_or(0);
        for revision in &release.revisions {
            let chart = match (revision.chart.as_str(), revision.chart_version.as_str()) {
                ("", "") => "chart unnamed".to_string(),
                (name, "") => name.to_string(),
                (name, version) => format!("{name}-{version}"),
            };
            let mut line = format!(
                "  rev {:<4} {:<width$}  {chart}",
                revision.revision, revision.status,
            );
            if !revision.app_version.is_empty() {
                line.push_str(&format!("  app {}", revision.app_version));
            }
            if !revision.updated.is_empty() {
                line.push_str(&format!("  {}", revision.updated));
            }
            if !revision.description.is_empty() {
                line.push_str(&format!("  {}", revision.description));
            }
            lines.push(line);
        }
    }
    if !releases.releases.is_empty() {
        lines.push(String::new());
        lines.push(
            "values and rendered manifests are not shown here: a release payload can carry secret \
             material, and revealing it needs an explicit per-field policy rather than a view that \
             prints everything it decoded"
                .to_string(),
        );
    }
    lines
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

#[cfg(test)]
#[path = "helm_test.rs"]
mod tests;
