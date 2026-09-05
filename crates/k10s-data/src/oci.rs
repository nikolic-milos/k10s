//! OCI tags, manifests, referrers. imageID to running pod.
//!
//! A running pod already names the digest it pulled: kubelet writes it on
//! `status.containerStatuses[].imageID`. Mapping that string onto a pod list
//! is a parse, not a registry round trip, and it is the one supply-chain read
//! that costs nothing installed. Tags, manifests and referrers are the other
//! half: OCI distribution v2 over whatever [`crate::reach`] already bound.
//!
//! There is no TLS client in this crate's lock that we can turn on without a
//! packages conversation, so an https registry is [`crate::reach::ToolReach::Unbound`]
//! with a system-browser URL, never a silent empty list. An empty referrer
//! list would look like "no Cosign, no SBOM". An unbound fetch says why it
//! did not look. Listing a referrer is not verifying a signature; nothing
//! here claims otherwise.
//!
//! Credentials follow the secret rule. A docker config is parsed from bytes
//! the caller already holds. An `imagePullSecret` is parsed only after the
//! caller revealed its value into [`crate::reach::Scratch`]. This module never
//! lists Secrets, never reads `~/.docker/config.json` itself, and the types
//! it returns have nowhere to put a password.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use serde::Deserialize;

use crate::reach::{Scratch, ToolReach, Unbound};
use crate::read::Fetched;

pub const MAX_IMAGE_ID_BYTES: usize = 1_024;
pub const MAX_DOCKER_CONFIG_BYTES: usize = 1 << 20;
pub const MAX_REGISTRIES: usize = 200;
pub const MAX_TAGS: usize = 256;
pub const MAX_REFERRERS: usize = 256;
pub const MAX_POD_ROWS: usize = 2_000;
pub const MAX_FIELD_CHARS: usize = 200;

const WHAT: &str = "oci";

/// One container's imageID, reduced to a repository and a digest when those
/// are present. `raw` is clipped; a digest that parsed is stored whole, because
/// it is the join key to Harbor scans and to referrers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageId {
    pub raw: String,
    pub runtime: Option<String>,
    pub repository: Option<String>,
    pub digest: Option<String>,
}

/// One running container that resolved to a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningImage {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    pub image: String,
    pub image_id: ImageId,
}

/// Digest to the pods that run it. Pods whose imageID has no digest are
/// counted, not silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DigestIndex {
    pub by_digest: BTreeMap<String, Vec<RunningImage>>,
    pub truncated: bool,
    pub without_digest: usize,
}

/// A docker config or kubernetes.io/dockerconfigjson document, with the
/// credential blobs left behind. `has_auth` is a presence bit, not a password.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DockerConfig {
    pub registries: Vec<RegistryHint>,
    pub creds_store: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHint {
    pub host: String,
    pub has_auth: bool,
    pub helper: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerConfigError {
    TooLarge { bytes: usize },
    NotJson(String),
    NotAConfig,
}

impl std::fmt::Display for DockerConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DockerConfigError::TooLarge { bytes } => write!(
                f,
                "docker config is {bytes} bytes; the cap is {MAX_DOCKER_CONFIG_BYTES}"
            ),
            DockerConfigError::NotJson(why) => write!(f, "docker config JSON did not parse: {why}"),
            DockerConfigError::NotAConfig => {
                write!(f, "JSON is not a docker config (no auths object)")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferrerKind {
    CosignSignature,
    Sbom,
    Attestation,
    Other,
}

/// One OCI referrer descriptor. Presence of a Cosign or SBOM artifact, not a
/// verdict that the signature is valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referrer {
    pub digest: String,
    pub media_type: String,
    pub artifact_type: String,
    pub kind: ReferrerKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Referrers {
    pub repository: String,
    pub subject: String,
    pub items: Vec<Referrer>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TagList {
    pub name: String,
    pub tags: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManifestSummary {
    pub media_type: String,
    pub schema_version: u32,
    pub artifact_type: String,
    pub config_digest: String,
    pub layers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    TooLarge { bytes: usize },
    NotJson(String),
    NotThisDocument,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::TooLarge { bytes } => write!(
                f,
                "registry JSON is {bytes} bytes; the cap is {}",
                crate::reach::MAX_BODY_BYTES
            ),
            RegistryError::NotJson(why) => write!(f, "registry JSON did not parse: {why}"),
            RegistryError::NotThisDocument => {
                write!(f, "JSON is not the OCI document this view asked for")
            }
        }
    }
}

/// Parse kubelet `imageID`. Runtime prefix, repository, digest when present.
pub fn parse_image_id(image_id: &str) -> ImageId {
    let trimmed = image_id.trim();
    let cut = byte_prefix(trimmed, MAX_IMAGE_ID_BYTES);
    if cut.is_empty() {
        return ImageId {
            raw: String::new(),
            runtime: None,
            repository: None,
            digest: None,
        };
    }

    let (runtime, rest) = match cut.split_once("://") {
        Some((runtime, rest)) => (Some(clipped(runtime)), rest),
        None => (None, cut),
    };

    let (repository, digest) = match rest.rsplit_once('@') {
        Some((repo, digest)) => {
            let digest = parse_digest(digest).map(str::to_string);
            let repository = if repo.is_empty() || looks_like_digest(repo) {
                None
            } else {
                Some(clipped(repo))
            };
            (repository, digest)
        }
        None if looks_like_digest(rest) => (None, Some(rest.to_string())),
        None => {
            // A CRI that stores only `sha256:` after the runtime, or a name
            // with no digest at all.
            if let Some(digest) = rest
                .find("sha256:")
                .or_else(|| rest.find("sha512:"))
                .and_then(|at| parse_digest(&rest[at..]).map(str::to_string))
            {
                (None, Some(digest))
            } else if rest.is_empty() {
                (None, None)
            } else {
                (Some(clipped(rest)), None)
            }
        }
    };

    ImageId {
        raw: clipped(cut),
        runtime,
        repository,
        digest,
    }
}

fn byte_prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut at = max;
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    &text[..at]
}

fn looks_like_digest(text: &str) -> bool {
    parse_digest(text).is_some()
}

fn parse_digest(text: &str) -> Option<&str> {
    let text = text.trim();
    let (algo, hex) = text.split_once(':')?;
    if algo != "sha256" && algo != "sha512" {
        return None;
    }
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let want = if algo == "sha256" { 64 } else { 128 };
    if hex.len() != want {
        return None;
    }
    Some(text)
}

/// Digest to pod names from `containerStatuses[].imageID` only. Init and
/// ephemeral statuses are a different question and are not mixed in.
pub fn digest_index(pods: &[Pod]) -> DigestIndex {
    let mut by_digest: BTreeMap<String, Vec<RunningImage>> = BTreeMap::new();
    let mut truncated = false;
    let mut without_digest = 0usize;
    let mut held = 0usize;

    for pod in pods {
        let namespace = pod.metadata.namespace.as_deref().unwrap_or("").to_string();
        let name = pod.metadata.name.as_deref().unwrap_or("").to_string();
        let statuses = pod
            .status
            .as_ref()
            .and_then(|status| status.container_statuses.as_deref())
            .unwrap_or(&[]);
        for status in statuses {
            if held == MAX_POD_ROWS {
                truncated = true;
                break;
            }
            let parsed = parse_image_id(&status.image_id);
            let Some(digest) = parsed.digest.clone() else {
                without_digest += 1;
                continue;
            };
            held += 1;
            by_digest.entry(digest).or_default().push(RunningImage {
                namespace: clipped(&namespace),
                pod: clipped(&name),
                container: clipped(&status.name),
                image: clipped(&status.image),
                image_id: parsed,
            });
        }
        if truncated {
            break;
        }
    }

    DigestIndex {
        by_digest,
        truncated,
        without_digest,
    }
}

/// Names of `spec.imagePullSecrets` on a pod. Names only: reading the Secret
/// is an explicit reveal into Scratch, not something this function does.
pub fn pull_secret_names(pod: &Pod) -> Vec<String> {
    pod.spec
        .as_ref()
        .map(|spec| {
            spec.image_pull_secrets
                .iter()
                .flatten()
                .map(|secret| clipped(&secret.name))
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `~/.docker/config.json` or a dockerconfigjson Secret value from bytes
/// the caller already holds. Auth blobs are not stored on the returned type.
pub fn parse_docker_config(bytes: &[u8]) -> Result<DockerConfig, DockerConfigError> {
    if bytes.len() > MAX_DOCKER_CONFIG_BYTES {
        return Err(DockerConfigError::TooLarge { bytes: bytes.len() });
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| DockerConfigError::NotJson(error.to_string()))?;
    let Some(object) = value.as_object() else {
        return Err(DockerConfigError::NotAConfig);
    };

    let creds_store = object
        .get("credsStore")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(clipped);

    let helpers: BTreeMap<String, String> = object
        .get("credHelpers")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(host, helper)| Some((host_of(host), clipped(helper.as_str()?))))
                .collect()
        })
        .unwrap_or_default();

    let auths = match object.get("auths").and_then(|v| v.as_object()) {
        Some(auths) => auths,
        None if object.values().any(looks_like_auth_entry) => object,
        None if creds_store.is_some() || !helpers.is_empty() => {
            return Ok(DockerConfig {
                registries: helpers
                    .into_iter()
                    .map(|(host, helper)| RegistryHint {
                        host,
                        has_auth: false,
                        helper: Some(helper),
                    })
                    .take(MAX_REGISTRIES)
                    .collect(),
                creds_store,
                truncated: false,
            });
        }
        None => return Err(DockerConfigError::NotAConfig),
    };

    let mut registries: Vec<RegistryHint> = Vec::new();
    let mut truncated = false;
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (registry, entry) in auths {
        if registries.len() == MAX_REGISTRIES {
            truncated = true;
            break;
        }
        let host = host_of(registry);
        if host.is_empty() {
            continue;
        }
        let has_auth = entry
            .get("auth")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
            || entry
                .get("username")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty());
        let helper = helpers.get(&host).cloned();
        if let Some(at) = seen.get(&host).copied() {
            registries[at].has_auth |= has_auth;
            if registries[at].helper.is_none() {
                registries[at].helper = helper;
            }
            continue;
        }
        seen.insert(host.clone(), registries.len());
        registries.push(RegistryHint {
            host,
            has_auth,
            helper,
        });
    }
    for (host, helper) in helpers {
        if seen.contains_key(&host) {
            continue;
        }
        if registries.len() == MAX_REGISTRIES {
            truncated = true;
            break;
        }
        seen.insert(host.clone(), registries.len());
        registries.push(RegistryHint {
            host,
            has_auth: false,
            helper: Some(helper),
        });
    }

    Ok(DockerConfig {
        registries,
        creds_store,
        truncated,
    })
}

fn looks_like_auth_entry(value: &serde_json::Value) -> bool {
    value
        .as_object()
        .is_some_and(|entry| entry.contains_key("auth") || entry.contains_key("username"))
}

/// The same parse, from a revealed imagePullSecret. The Secret is not fetched
/// here; the caller already put its dockerconfigjson bytes into Scratch.
pub fn parse_docker_config_from_scratch(
    scratch: &Scratch,
) -> Result<DockerConfig, DockerConfigError> {
    parse_docker_config(scratch.as_bytes())
}

/// Copy one registry's username:password into a new Scratch. The docker
/// config type still does not hold it. Helpers are named, not invoked.
pub fn reveal_registry_auth(scratch: &Scratch, registry: &str) -> Option<Scratch> {
    let value: serde_json::Value = serde_json::from_slice(scratch.as_bytes()).ok()?;
    let object = value.as_object()?;
    let auths = match object.get("auths").and_then(|v| v.as_object()) {
        Some(auths) => auths,
        None if object.values().any(looks_like_auth_entry) => object,
        None => return None,
    };
    let want = host_of(registry);
    for (name, entry) in auths {
        if !same_registry(&host_of(name), &want) {
            continue;
        }
        let object = entry.as_object()?;
        if let (Some(user), Some(pass)) = (
            object.get("username").and_then(|v| v.as_str()),
            object.get("password").and_then(|v| v.as_str()),
        ) {
            if user.is_empty() && pass.is_empty() {
                continue;
            }
            return Some(Scratch::from_bytes(format!("{user}:{pass}").into_bytes()));
        }
        let auth = object.get("auth").and_then(|v| v.as_str())?;
        if auth.is_empty() {
            continue;
        }
        let engine = base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let decoded = engine.decode(auth.trim()).ok()?;
        return Some(Scratch::from_bytes(decoded));
    }
    None
}

fn host_of(registry: &str) -> String {
    let trimmed = registry.trim();
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let host = without_scheme.split('/').next().unwrap_or(without_scheme);
    clipped(&host.trim_end_matches('/').to_ascii_lowercase())
}

fn same_registry(a: &str, b: &str) -> bool {
    a == b || (docker_hub(a) && docker_hub(b))
}

fn docker_hub(host: &str) -> bool {
    matches!(
        host,
        "docker.io" | "index.docker.io" | "registry-1.docker.io"
    )
}

pub(crate) fn why_is_not_found(why: &str) -> bool {
    let lower = why.to_ascii_lowercase();
    why.contains("404") || why.contains("NotFound") || lower.contains("not found")
}

pub fn parse_referrers(bytes: &[u8]) -> Result<Vec<Referrer>, RegistryError> {
    if bytes.len() > crate::reach::MAX_BODY_BYTES {
        return Err(RegistryError::TooLarge { bytes: bytes.len() });
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| RegistryError::NotJson(error.to_string()))?;
    let manifests = if let Some(manifests) = value.get("manifests").and_then(|v| v.as_array()) {
        manifests
    } else if let Some(referrers) = value.get("referrers").and_then(|v| v.as_array()) {
        referrers
    } else if let Some(array) = value.as_array() {
        array
    } else {
        return Err(RegistryError::NotThisDocument);
    };

    let mut items = Vec::new();
    for desc in manifests {
        if items.len() == MAX_REFERRERS {
            break;
        }
        let digest = desc
            .get("digest")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if digest.is_empty() {
            continue;
        }
        let media_type = clipped(desc.get("mediaType").and_then(|v| v.as_str()).unwrap_or(""));
        let artifact_type = clipped(
            desc.get("artifactType")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let kind = referrer_kind(&artifact_type, &media_type);
        items.push(Referrer {
            digest: digest_or_clip(&digest),
            media_type,
            artifact_type,
            kind,
        });
    }
    Ok(items)
}

fn referrer_kind(artifact_type: &str, media_type: &str) -> ReferrerKind {
    let blob = format!(
        "{} {}",
        artifact_type.to_ascii_lowercase(),
        media_type.to_ascii_lowercase()
    );
    if blob.contains("cosign") || blob.contains("simplesigning") {
        ReferrerKind::CosignSignature
    } else if blob.contains("spdx") || blob.contains("cyclonedx") || blob.contains("sbom") {
        ReferrerKind::Sbom
    } else if blob.contains("in-toto") || blob.contains("dsse") || blob.contains("attestation") {
        ReferrerKind::Attestation
    } else {
        ReferrerKind::Other
    }
}

pub fn parse_tags(bytes: &[u8]) -> Result<TagList, RegistryError> {
    if bytes.len() > crate::reach::MAX_BODY_BYTES {
        return Err(RegistryError::TooLarge { bytes: bytes.len() });
    }
    let wire: WireTags =
        serde_json::from_slice(bytes).map_err(|error| RegistryError::NotJson(error.to_string()))?;
    let mut truncated = false;
    let mut tags = Vec::new();
    for tag in wire.tags {
        if tags.len() == MAX_TAGS {
            truncated = true;
            break;
        }
        if tag.is_empty() {
            continue;
        }
        tags.push(clipped(&tag));
    }
    Ok(TagList {
        name: clipped(&wire.name),
        tags,
        truncated,
    })
}

#[derive(Deserialize)]
struct WireTags {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tags: Vec<String>,
}

pub fn parse_manifest(bytes: &[u8]) -> Result<ManifestSummary, RegistryError> {
    if bytes.len() > crate::reach::MAX_BODY_BYTES {
        return Err(RegistryError::TooLarge { bytes: bytes.len() });
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| RegistryError::NotJson(error.to_string()))?;
    if !value.is_object() {
        return Err(RegistryError::NotThisDocument);
    }
    let config_digest = value
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let layers = value
        .get("layers")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .or_else(|| {
            value
                .get("manifests")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
        })
        .unwrap_or(0);
    Ok(ManifestSummary {
        media_type: clipped(
            value
                .get("mediaType")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        ),
        schema_version: value
            .get("schemaVersion")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        artifact_type: clipped(
            value
                .get("artifactType")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        ),
        config_digest: digest_or_clip(config_digest),
        layers,
    })
}

/// GET `/v2/{repository}/referrers/{digest}` through a bound proxy or
/// plaintext http URL. Unbound (https, no TLS client) is a Failed why, not an
/// empty list, and nothing here verifies a Cosign signature.
pub async fn fetch_referrers(
    client: &Client,
    reach: &ToolReach,
    repository: &str,
    digest: &str,
) -> Fetched<Referrers> {
    let Some(digest) = parse_digest(digest) else {
        return Fetched::Failed {
            what: WHAT,
            why: format!("{digest} is not a sha256 or sha512 digest"),
        };
    };
    let Some(repo) = oci_repository(repository) else {
        return Fetched::Failed {
            what: WHAT,
            why: "repository name is empty".to_string(),
        };
    };
    let rest = format!("v2/{repo}/referrers/{digest}");
    match registry_get(client, reach, &rest).await {
        Fetched::Ok(bytes) => match parse_referrers(&bytes) {
            Ok(items) => {
                let truncated = items.len() == MAX_REFERRERS;
                Fetched::Ok(Referrers {
                    repository: clipped(repository),
                    subject: digest.to_string(),
                    items,
                    truncated,
                })
            }
            Err(error) => Fetched::Failed {
                what: WHAT,
                why: error.to_string(),
            },
        },
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { why, .. } => {
            if why_is_not_found(&why) {
                Fetched::Failed {
                    what: WHAT,
                    why: "this registry does not serve OCI referrers (404); Cosign signatures and \
                          SBOMs cannot be listed, and nothing was verified"
                        .to_string(),
                }
            } else {
                Fetched::Failed { what: WHAT, why }
            }
        }
    }
}

pub async fn fetch_tags(client: &Client, reach: &ToolReach, repository: &str) -> Fetched<TagList> {
    let Some(repo) = oci_repository(repository) else {
        return Fetched::Failed {
            what: WHAT,
            why: "repository name is empty".to_string(),
        };
    };
    let rest = format!("v2/{repo}/tags/list");
    match registry_get(client, reach, &rest).await {
        Fetched::Ok(bytes) => match parse_tags(&bytes) {
            Ok(tags) => Fetched::Ok(tags),
            Err(error) => Fetched::Failed {
                what: WHAT,
                why: error.to_string(),
            },
        },
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { why, .. } => Fetched::Failed { what: WHAT, why },
    }
}

pub async fn fetch_manifest(
    client: &Client,
    reach: &ToolReach,
    repository: &str,
    reference: &str,
) -> Fetched<ManifestSummary> {
    let Some(repo) = oci_repository(repository) else {
        return Fetched::Failed {
            what: WHAT,
            why: "repository name is empty".to_string(),
        };
    };
    if !safe_reference(reference) {
        return Fetched::Failed {
            what: WHAT,
            why: "manifest reference is not a tag or digest".to_string(),
        };
    }
    let rest = format!("v2/{repo}/manifests/{reference}");
    match registry_get(client, reach, &rest).await {
        Fetched::Ok(bytes) => match parse_manifest(&bytes) {
            Ok(summary) => Fetched::Ok(summary),
            Err(error) => Fetched::Failed {
                what: WHAT,
                why: error.to_string(),
            },
        },
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { why, .. } => Fetched::Failed { what: WHAT, why },
    }
}

async fn registry_get(client: &Client, reach: &ToolReach, rest: &str) -> Fetched<Vec<u8>> {
    match reach {
        ToolReach::Absent { .. } => Fetched::Failed {
            what: WHAT,
            why: "no registry is bound, so tags, manifests, and referrers cannot be fetched"
                .to_string(),
        },
        ToolReach::Unbound(unbound) => Fetched::Failed {
            what: WHAT,
            why: unbound_why(unbound),
        },
        ToolReach::Bound(bound) => match crate::reach::tool_get(client, bound, rest).await {
            Fetched::Ok(bytes) => Fetched::Ok(bytes),
            Fetched::Denied { .. } => Fetched::Denied { what: WHAT },
            Fetched::Failed { why, .. } => Fetched::Failed { what: WHAT, why },
        },
    }
}

fn unbound_why(unbound: &Unbound) -> String {
    let mut why = unbound.why.clone();
    why.push_str("; Cosign signatures and SBOMs are not listed and not verified");
    if let Some(url) = &unbound.browser_url {
        why.push_str("; open ");
        why.push_str(url);
        why.push_str(" in the system browser");
    }
    why
}

fn oci_repository(name: &str) -> Option<String> {
    let name = name.trim().trim_matches('/');
    if name.is_empty() {
        return None;
    }
    if name.contains("..") {
        return None;
    }
    let mut out = String::new();
    for (i, part) in name.split('/').enumerate() {
        if part.is_empty() {
            return None;
        }
        if i > 0 {
            out.push('/');
        }
        out.push_str(&encode_segment(part));
    }
    Some(out)
}

fn safe_reference(reference: &str) -> bool {
    if parse_digest(reference).is_some() {
        return true;
    }
    let bytes = reference.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub(crate) fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn digest_or_clip(text: &str) -> String {
    match parse_digest(text) {
        Some(digest) => digest.to_string(),
        None => clipped(text),
    }
}

pub(crate) fn clipped(text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

/// The digest map as a document. Same reason helm renders: one deterministic
/// rendering is what a test can gate, rather than a screenshot.
pub fn render_index(index: &DigestIndex) -> Vec<String> {
    let mut lines = Vec::new();
    if index.by_digest.is_empty() {
        lines.push("no running container has a digest in its imageID".to_string());
    } else {
        let digests = index.by_digest.len();
        let pods: usize = index.by_digest.values().map(Vec::len).sum();
        lines.push(format!(
            "{digests} {}, {pods} running {}",
            if digests == 1 { "digest" } else { "digests" },
            if pods == 1 { "container" } else { "containers" },
        ));
    }
    if index.truncated {
        lines.push(format!(
            "the listing stopped at {MAX_POD_ROWS} containers, so this is some of them rather \
             than all"
        ));
    }
    if index.without_digest > 0 {
        lines.push(format!(
            "{} container {} had no digest on imageID and {} not mapped",
            index.without_digest,
            if index.without_digest == 1 {
                "status"
            } else {
                "statuses"
            },
            if index.without_digest == 1 {
                "is"
            } else {
                "are"
            },
        ));
    }
    for (digest, runners) in &index.by_digest {
        lines.push(String::new());
        lines.push(digest.clone());
        for runner in runners {
            lines.push(format!(
                "  {}/{}  {}  {}",
                runner.namespace, runner.pod, runner.container, runner.image
            ));
        }
    }
    lines
}

pub fn render_referrers(referrers: &Referrers) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "referrers of {}@{}",
        referrers.repository, referrers.subject
    ));
    if referrers.items.is_empty() {
        lines.push(
            "the registry listed no referrers; that is not a verification, and it is not a \
             Cosign or SBOM result"
                .to_string(),
        );
    } else {
        lines.push(format!(
            "{} {}, listed not verified",
            referrers.items.len(),
            if referrers.items.len() == 1 {
                "referrer"
            } else {
                "referrers"
            },
        ));
    }
    if referrers.truncated {
        lines.push(format!(
            "the listing stopped at {MAX_REFERRERS} referrers, so this is some of them rather \
             than all"
        ));
    }
    for item in &referrers.items {
        let kind = match item.kind {
            ReferrerKind::CosignSignature => "cosign",
            ReferrerKind::Sbom => "sbom",
            ReferrerKind::Attestation => "attestation",
            ReferrerKind::Other => "other",
        };
        lines.push(format!("  {kind}  {}", item.digest));
    }
    lines.push(String::new());
    lines.push(
        "k10s does not verify Cosign signatures or SBOM attestations; it lists the referrers \
         the registry served"
            .to_string(),
    );
    lines
}

#[cfg(test)]
#[path = "oci_test.rs"]
mod tests;
