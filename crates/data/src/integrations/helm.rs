//! Helm release inventory from strict Secret metadata, never release payloads.
//!
//! The server selects `type=helm.sh/release.v1` and `owner=helm` and returns
//! only PartialObjectMetadataList. Namespace and Helm's name, version and
//! status labels identify stored revisions. Chart details, deployment times,
//! descriptions, values and manifests are unavailable through this inventory.
//! A refusal never retries for a full Secret. Replies share an eight MiB cap,
//! at most ten pages and two thousand examined entries; malformed metadata is
//! counted and repeated continuation tokens refuse an incomplete inventory.
//!
//! Pure legacy payload decoders remain for caller-owned bytes and their tests.
//! They are not connected to a Kubernetes read route.

use std::collections::BTreeMap;
use std::io::Read;

use base64::Engine;
use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::describe::is_secret;
use crate::discover::KindTarget;
use crate::read::{Fetched, collection_path};

// Helm's own type and label. Both are asked for on the wire rather than
// filtered here, so the transfer itself is narrowed to release Secrets.
const RELEASE_TYPE: &str = "helm.sh/release.v1";
const OWNER_SELECTOR: &str = "owner=helm";
// This is Helm's recorded deployment status, not a workload health check.
const DEPLOYED: &str = "deployed";

const PAGE_LIMIT: u32 = 200;
// Helm keeps ten revisions per release by default and a cluster can hold many
// releases; this is a ceiling on the whole listing, and reaching it is stated
// rather than hidden.
const MAX_REVISIONS: usize = 2_000;
const MAX_PAGES: usize = 10;
const MAX_TOKEN_BYTES: usize = 16 << 10;
const METADATA_ONLY: &str = "unavailable (metadata only)";
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
#[cfg(test)]
const MAX_FIELD_CHARS: usize = 200;

/// One stored revision. Payload-only fields are empty in metadata inventories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub revision: u32,
    // Helm's own word: deployed, superseded, failed, uninstalled,
    // pending-upgrade. Carried as text rather than mapped onto an enum, because
    // a status this build has never heard of is still the status.
    pub status: String,
    // Payload-only fields remain for caller-owned decoder compatibility.
    // Metadata fetches leave them empty rather than infer them from labels.
    pub updated: String,
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
    /// The newest stored revision, whatever Helm marked it. This is the top of
    /// the stack, which is a different question from what is running.
    pub fn latest(&self) -> Option<&Revision> {
        self.revisions.first()
    }

    /// The newest stored revision labelled `deployed`. This is Helm's recorded
    /// state, not evidence of current workload health.
    pub fn current(&self) -> Option<&Revision> {
        self.revisions
            .iter()
            .find(|revision| revision.status == DEPLOYED)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Releases {
    pub releases: Vec<Release>,
    /// The listing stopped at its ceiling, so this is some of the releases
    /// rather than all of them.
    pub truncated: bool,
    /// Selected entries without usable identity or revision labels. They were
    /// seen, so a malformed inventory must not claim no releases exist.
    pub unreadable: usize,
}

// The legacy decoder is exercised with caller-owned fixture bytes only.
#[cfg(test)]
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

#[cfg(test)]
#[derive(Deserialize, Default)]
struct WireInfo {
    #[serde(default)]
    status: String,
    #[serde(default)]
    last_deployed: String,
    #[serde(default)]
    description: String,
}

#[cfg(test)]
#[derive(Deserialize, Default)]
struct WireChart {
    #[serde(default)]
    metadata: WireChartMeta,
}

#[cfg(test)]
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
#[serde(deny_unknown_fields)]
struct WireList {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    #[serde(default)]
    metadata: WireListMeta,
    #[serde(deserialize_with = "metadata_items")]
    items: Vec<WireSecret>,
}

#[derive(Deserialize, Default)]
struct WireListMeta {
    #[serde(default, rename = "continue")]
    cont: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSecret {
    #[serde(rename = "apiVersion")]
    api_version: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    metadata: WireSecretMeta,
}

#[derive(Deserialize, Default)]
struct WireSecretMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    labels: WireLabels,
}

#[derive(Deserialize, Default)]
struct WireLabels {
    #[serde(default)]
    owner: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    status: String,
}

fn metadata_items<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<WireSecret>, D::Error> {
    struct Items;
    impl<'de> serde::de::Visitor<'de> for Items {
        type Value = Vec<WireSecret>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a metadata array or null")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_some<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_seq(self)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut items = Vec::new();
            while items.len() <= MAX_REVISIONS {
                let Some(item) = sequence.next_element()? else {
                    return Ok(items);
                };
                items.push(item);
            }
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(items)
        }
    }
    deserializer.deserialize_option(Items)
}

impl WireSecret {
    fn revision(self) -> Option<(String, String, Revision)> {
        if self
            .kind
            .as_deref()
            .is_some_and(|kind| kind != "PartialObjectMetadata")
            || self
                .api_version
                .as_deref()
                .is_some_and(|version| version != "meta.k8s.io/v1")
        {
            return None;
        }
        let meta = self.metadata;
        let labels = meta.labels;
        if meta.name.is_empty()
            || meta.name.len() > 253
            || meta.namespace.is_empty()
            || meta.namespace.len() > 63
            || labels.owner != "helm"
            || labels.name.is_empty()
            || labels.name.len() > 63
            || labels.status.is_empty()
            || labels.status.len() > 63
            || labels.version.len() > 10
        {
            return None;
        }
        let revision = labels
            .version
            .parse::<u32>()
            .ok()
            .filter(|version| *version > 0)?;
        Some((
            meta.namespace,
            labels.name,
            Revision {
                revision,
                status: labels.status,
                updated: String::new(),
                description: String::new(),
                chart: String::new(),
                chart_version: String::new(),
                app_version: String::new(),
            },
        ))
    }
}

/// A decoded payload: which release it belongs to, and the revision it is.
#[cfg(test)]
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
#[cfg(test)]
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

/// Decode caller-owned bytes into a scratch buffer. No cluster read uses this.
pub(crate) fn decode_scratch(encoded: &str) -> Result<crate::reach::Scratch, &'static str> {
    Ok(crate::reach::Scratch::from_bytes(payload(encoded)?))
}

// One field, at a length a line can hold. Truncated with the ellipsis
// `describe` uses, so a clipped value looks clipped rather than merely short.
#[cfg(test)]
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
    let mut tokens: Vec<String> = Vec::new();
    let mut held = 0usize;
    let mut unreadable = 0usize;
    let mut truncated = false;
    let mut bytes = 0usize;
    let mut pages = 0usize;
    loop {
        if pages == MAX_PAGES {
            return metadata_failure(
                "Helm releases exceed 10 metadata pages; incomplete inventory is refused",
            );
        }
        if bytes == crate::reach::MAX_BODY_BYTES {
            return metadata_failure(
                "Helm release metadata exceed 8 MiB; incomplete inventory is refused",
            );
        }
        let mut params = ListParams::default()
            .limit(PAGE_LIMIT)
            .labels(OWNER_SELECTOR)
            .fields(&fields);
        if let Some(token) = tokens.last() {
            params = params.continue_token(token);
        }
        let request = match Request::new(path.clone()).list_metadata(&params) {
            Ok(request) => request,
            Err(error) => {
                return Fetched::Failed {
                    what: "helm releases",
                    why: error.to_string(),
                };
            }
        };
        let response = match crate::reach::bounded_request_with_status(
            client,
            "helm releases",
            request,
            crate::reach::MAX_BODY_BYTES - bytes,
        )
        .await
        .1
        {
            Fetched::Ok(response) => response,
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { what, why } => return Fetched::Failed { what, why },
        };
        bytes += response.len();
        pages += 1;
        let page: WireList = match serde_json::from_slice(&response) {
            Ok(page) => page,
            Err(_) => {
                return metadata_failure(
                    "Helm releases returned an unsupported metadata response; no full Secret inventory is used",
                );
            }
        };
        if page.kind != "PartialObjectMetadataList" || page.api_version != "meta.k8s.io/v1" {
            return metadata_failure(
                "Helm releases did not return PartialObjectMetadataList; no full Secret inventory is used",
            );
        }
        for secret in page.items {
            if held == MAX_REVISIONS {
                truncated = true;
                break;
            }
            held += 1;
            let Some((namespace, name, revision)) = secret.revision() else {
                unreadable += 1;
                continue;
            };
            stored.entry((namespace, name)).or_default().push(revision);
        }
        if !page.metadata.cont.is_empty() && held == MAX_REVISIONS {
            truncated = true;
        }
        if !truncated
            && (page.metadata.cont.len() > MAX_TOKEN_BYTES || tokens.contains(&page.metadata.cont))
        {
            return metadata_failure(
                "Helm release metadata have an oversized or repeated continuation token; incomplete inventory is refused",
            );
        }
        if page.metadata.cont.is_empty() || truncated {
            break;
        }
        tokens.push(page.metadata.cont);
    }
    let releases = stored
        .into_iter()
        .map(|((namespace, name), mut revisions)| {
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

fn metadata_failure(why: &str) -> Fetched<Releases> {
    Fetched::Failed {
        what: "helm releases",
        why: why.to_string(),
    }
}

/// One row per release, preferring the revision labelled deployed. History stays on
/// [`Release::revisions`]; a table is not a document, so it does not pad every
/// stored revision onto the screen. Cells are the inventory fields only:
/// [`Revision`] still has nowhere for values or manifests.
pub fn table_page(releases: &Releases) -> TablePage {
    let columns = ["Name", "Namespace", "Revision", "Status", "Chart"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let rows = crate::browse::unreadable_row("Helm release metadata", releases.unreadable, 5, 3)
        .into_iter()
        .chain(releases.releases.iter().filter_map(|release| {
            let latest = release.latest()?;
            let revision = release.current().unwrap_or(latest);
            Some(TableRow {
                cells: vec![
                    release.name.clone(),
                    release.namespace.clone(),
                    revision.revision.to_string(),
                    status_label(revision, latest),
                    chart_label(revision),
                ],
                name: release.name.clone(),
                namespace: Some(release.namespace.clone()),
                uid: format!("{}/{}", release.namespace, release.name),
            })
        }))
        .collect();
    TablePage {
        columns,
        rows,
        truncated: releases.truncated,
        continue_token: None,
    }
}

/// Preserve both the recorded deployed revision and any newer pending or failed
/// revision. A latest-only row would hide Helm's unfinished operation.
fn status_label(running: &Revision, latest: &Revision) -> String {
    if running.revision == latest.revision {
        return running.status.clone();
    }
    format!(
        "{} (revision {} {})",
        running.status, latest.revision, latest.status
    )
}

fn chart_label(revision: &Revision) -> String {
    match (revision.chart.as_str(), revision.chart_version.as_str()) {
        ("", "") => METADATA_ONLY.to_string(),
        (name, "") => name.to_string(),
        (name, version) => format!("{name}-{version}"),
    }
}

/// The inventory as a document, rendered here for the same reason a describe is:
/// one deterministic rendering is what makes it gateable by a test rather than
/// by a screenshot. The shell lists releases as a table; this stays for the
/// history a table does not show.
pub fn render(releases: &Releases) -> Vec<String> {
    let mut lines = Vec::new();
    if releases.releases.is_empty() && releases.unreadable == 0 {
        lines.push("no Helm release metadata is visible to this account".to_string());
        lines.push(String::new());
        lines.push(
            "this reads Secret metadata only (type helm.sh/release.v1, label \
             owner=helm); releases using the ConfigMap storage driver or namespaces outside \
             this account's access are not covered"
                .to_string(),
        );
    } else if releases.releases.is_empty() {
        // Not "no releases are stored": some are, and they were seen. Saying the
        // cluster is empty here would contradict the very field that counted
        // them, and it is the loudest line in the document.
        lines.push(
            "no Helm release identity could be read here, though some are stored: every selected \
             entry has unusable release metadata"
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
            "{} release {} have unusable metadata and {} not shown",
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
            let chart = chart_label(revision);
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
            "chart, app version, deployment time, description, values and rendered manifests \
             are unavailable: release Secret payloads are never fetched"
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
