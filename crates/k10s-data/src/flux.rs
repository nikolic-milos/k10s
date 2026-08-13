//! Flux inventory from the CRs the controllers already publish.
//!
//! GitRepository and OCIRepository live on `source.toolkit.fluxcd.io`,
//! Kustomization on `kustomize.toolkit.fluxcd.io`, HelmRelease on
//! `helm.toolkit.fluxcd.io`. A cluster that does not serve a group answers
//! 404 and that kind is invisible, not broken; a 403 is Denied. Nothing is
//! installed to find them, and nothing is reimplemented: suspend and resume
//! are a merge-patch of `spec.suspend`, and reconcile-now is the
//! `reconcile.fluxcd.io/requestedAt` annotation Flux already honours.
//!
//! The listing is paged with a ceiling, and every field a CR contributes is
//! clipped on its own, because the payload bound is not a field bound.

use std::time::{SystemTime, UNIX_EPOCH};

use kube::Client;
use kube::api::{ListParams, Patch, PatchParams, Request};
use serde::Deserialize;

use crate::read::{Fetched, classify};

pub const RECONCILE_REQUESTED_AT: &str = "reconcile.fluxcd.io/requestedAt";

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;

const SOURCE_GROUP: &str = "source.toolkit.fluxcd.io";
const KUSTOMIZE_GROUP: &str = "kustomize.toolkit.fluxcd.io";
const HELM_GROUP: &str = "helm.toolkit.fluxcd.io";

/// The four CRs this inventory reads. Flux serves more; those are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    GitRepository,
    OCIRepository,
    Kustomization,
    HelmRelease,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::GitRepository => "GitRepository",
            Kind::OCIRepository => "OCIRepository",
            Kind::Kustomization => "Kustomization",
            Kind::HelmRelease => "HelmRelease",
        }
    }

    pub fn group(self) -> &'static str {
        match self {
            Kind::GitRepository | Kind::OCIRepository => SOURCE_GROUP,
            Kind::Kustomization => KUSTOMIZE_GROUP,
            Kind::HelmRelease => HELM_GROUP,
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::GitRepository => "gitrepositories",
            Kind::OCIRepository => "ocirepositories",
            Kind::Kustomization => "kustomizations",
            Kind::HelmRelease => "helmreleases",
        }
    }

    /// The version we try when the group document names none.
    pub fn version(self) -> &'static str {
        match self {
            Kind::HelmRelease => "v2",
            _ => "v1",
        }
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::GitRepository => "flux gitrepositories",
            Kind::OCIRepository => "flux ocirepositories",
            Kind::Kustomization => "flux kustomizations",
            Kind::HelmRelease => "flux helmreleases",
        }
    }
}

/// One CR, reduced to what an inventory shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    /// The version the list (or the caller) named, used again on a patch.
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    /// The Ready condition's `status`, as the object spelled it.
    pub ready: String,
    pub suspended: bool,
    pub last_applied_revision: String,
    pub source_ref: String,
}

/// What one kind's list answered.
///
/// A 404 on the group is [`KindSet::NotServed`]: invisible, not broken. A 403
/// is [`KindSet::Denied`]. Those are different states on purpose; collapsing
/// them would tell someone Flux is absent when the account was refused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KindSet {
    Served {
        items: Vec<Resource>,
        truncated: bool,
        unreadable: usize,
    },
    #[default]
    NotServed,
    Denied,
}

impl KindSet {
    /// False when the group answered 404.
    pub fn served(&self) -> bool {
        !matches!(self, KindSet::NotServed)
    }

    pub fn items(&self) -> &[Resource] {
        match self {
            KindSet::Served { items, .. } => items,
            KindSet::NotServed | KindSet::Denied => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub git_repositories: KindSet,
    pub oci_repositories: KindSet,
    pub kustomizations: KindSet,
    pub helm_releases: KindSet,
}

impl Inventory {
    /// False when every Flux group answered 404.
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Kind); 4] {
        [
            (&self.git_repositories, Kind::GitRepository),
            (&self.oci_repositories, Kind::OCIRepository),
            (&self.kustomizations, Kind::Kustomization),
            (&self.helm_releases, Kind::HelmRelease),
        ]
    }
}

#[derive(Deserialize, Default)]
struct WireGroup {
    #[serde(default)]
    versions: Vec<WireGroupVersion>,
    #[serde(default, rename = "preferredVersion")]
    preferred: WireGroupVersion,
}

#[derive(Deserialize, Default)]
struct WireGroupVersion {
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
struct WireList {
    #[serde(default)]
    metadata: WireListMeta,
    #[serde(default)]
    items: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct WireListMeta {
    #[serde(default, rename = "continue")]
    cont: String,
}

#[derive(Deserialize, Default)]
struct WireObject {
    #[serde(default)]
    metadata: WireMeta,
    #[serde(default)]
    spec: WireSpec,
    #[serde(default)]
    status: WireStatus,
}

#[derive(Deserialize, Default)]
struct WireMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    uid: String,
}

#[derive(Deserialize, Default)]
struct WireSpec {
    #[serde(default)]
    suspend: bool,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "sourceRef")]
    source_ref: WireRef,
    #[serde(default)]
    chart: WireChart,
    #[serde(default, rename = "chartRef")]
    chart_ref: WireRef,
}

#[derive(Deserialize, Default)]
struct WireChart {
    #[serde(default)]
    spec: WireChartSpec,
}

#[derive(Deserialize, Default)]
struct WireChartSpec {
    #[serde(default, rename = "sourceRef")]
    source_ref: WireRef,
}

#[derive(Deserialize, Default)]
struct WireRef {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
}

#[derive(Deserialize, Default)]
struct WireStatus {
    #[serde(default)]
    conditions: Vec<WireCondition>,
    #[serde(default, rename = "lastAppliedRevision")]
    last_applied_revision: String,
    #[serde(default, rename = "lastAttemptedRevision")]
    last_attempted_revision: String,
    #[serde(default)]
    artifact: WireArtifact,
}

#[derive(Deserialize, Default)]
struct WireArtifact {
    #[serde(default)]
    revision: String,
}

#[derive(Deserialize, Default)]
struct WireCondition {
    #[serde(default, rename = "type")]
    type_name: String,
    #[serde(default)]
    status: String,
}

enum GroupAnswer {
    Served(Vec<String>),
    NotServed,
    Denied,
    Failed(String),
}

enum ListErr {
    NotFound,
    Denied,
    Failed(String),
}

fn after_group(error: &kube::Error) -> GroupAnswer {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return GroupAnswer::Denied;
        }
        if response.code == 404 {
            return GroupAnswer::NotServed;
        }
    }
    GroupAnswer::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

fn after_list(error: &kube::Error) -> ListErr {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return ListErr::Denied;
        }
        if response.code == 404 {
            return ListErr::NotFound;
        }
    }
    ListErr::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn ready_of(conditions: &[WireCondition]) -> String {
    conditions
        .iter()
        .find(|condition| condition.type_name == "Ready")
        .map(|condition| clipped(condition.status.clone()))
        .unwrap_or_default()
}

fn revision_of(kind: Kind, status: &WireStatus) -> String {
    let text = match kind {
        Kind::GitRepository | Kind::OCIRepository => status.artifact.revision.as_str(),
        Kind::Kustomization | Kind::HelmRelease => {
            if !status.last_applied_revision.is_empty() {
                status.last_applied_revision.as_str()
            } else {
                status.last_attempted_revision.as_str()
            }
        }
    };
    clipped(text.to_string())
}

fn format_ref(reference: &WireRef) -> String {
    if reference.name.is_empty() {
        return String::new();
    }
    match (reference.kind.as_str(), reference.namespace.as_str()) {
        ("", "") => reference.name.clone(),
        (kind, "") => format!("{kind}/{}", reference.name),
        ("", namespace) => format!("{namespace}/{}", reference.name),
        (kind, namespace) => format!("{kind}/{namespace}/{}", reference.name),
    }
}

fn source_ref_of(kind: Kind, spec: &WireSpec) -> String {
    let text = match kind {
        Kind::GitRepository | Kind::OCIRepository => spec.url.clone(),
        Kind::Kustomization => format_ref(&spec.source_ref),
        Kind::HelmRelease => {
            let chart_ref = format_ref(&spec.chart_ref);
            if chart_ref.is_empty() {
                format_ref(&spec.chart.spec.source_ref)
            } else {
                chart_ref
            }
        }
    };
    clipped(text)
}

fn from_wire(kind: Kind, version: &str, wire: WireObject) -> Option<Resource> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    Some(Resource {
        kind,
        version: version.to_string(),
        name: clipped(wire.metadata.name),
        namespace: clipped(wire.metadata.namespace),
        uid: clipped(wire.metadata.uid),
        ready: ready_of(&wire.status.conditions),
        suspended: wire.spec.suspend,
        last_applied_revision: revision_of(kind, &wire.status),
        source_ref: source_ref_of(kind, &wire.spec),
    })
}

fn parse_item(kind: Kind, version: &str, value: serde_json::Value) -> Option<Resource> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_wire(kind, version, wire)
}

fn order_versions(preferred: &str, versions: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    if !preferred.is_empty() {
        out.push(preferred.to_string());
    }
    for version in versions {
        if version.is_empty() || out.iter().any(|have| have == &version) {
            continue;
        }
        out.push(version);
    }
    out
}

fn versions_for(kind: Kind, group_versions: &[String]) -> Vec<String> {
    let mut out = group_versions.to_vec();
    let fallback = kind.version().to_string();
    if !out.iter().any(|have| have == &fallback) {
        out.push(fallback);
    }
    out
}

fn collection_url(kind: Kind, version: &str, namespace: Option<&str>) -> String {
    let mut path = format!("/apis/{}/{version}", kind.group());
    if let Some(namespace) = namespace {
        path.push_str("/namespaces/");
        path.push_str(namespace);
    }
    path.push('/');
    path.push_str(kind.plural());
    path
}

fn object_collection_url(target: &Resource) -> String {
    let version = if target.version.is_empty() {
        target.kind.version()
    } else {
        target.version.as_str()
    };
    collection_url(target.kind, version, Some(target.namespace.as_str()))
}

fn group_url(group: &str) -> String {
    format!("/apis/{group}")
}

/// RFC3339 UTC from a Unix timestamp. Flux parses this; we do not invent a
/// second reconcile API.
pub fn rfc3339(secs: u64, nanos: u32) -> String {
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let hour = sod / 3_600;
    let min = (sod % 3_600) / 60;
    let sec = sod % 60;
    let (year, month, day) = civil_from_unix_days(days);
    if nanos == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{nanos:09}Z")
    }
}

fn rfc3339_now() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    rfc3339(elapsed.as_secs(), elapsed.subsec_nanos())
}

fn civil_from_unix_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

fn suspend_patch(suspend: bool) -> Patch<serde_json::Value> {
    Patch::Merge(serde_json::json!({ "spec": { "suspend": suspend } }))
}

fn reconcile_patch(at: &str) -> Patch<serde_json::Value> {
    Patch::Merge(serde_json::json!({
        "metadata": {
            "annotations": {
                RECONCILE_REQUESTED_AT: at
            }
        }
    }))
}

fn patch_request(
    target: &Resource,
    patch: &Patch<serde_json::Value>,
) -> Result<http::Request<Vec<u8>>, String> {
    Request::new(object_collection_url(target))
        .patch(&target.name, &PatchParams::default(), patch)
        .map_err(|error| error.to_string())
}

fn suspend_request(target: &Resource, suspend: bool) -> Result<http::Request<Vec<u8>>, String> {
    patch_request(target, &suspend_patch(suspend))
}

fn reconcile_request(target: &Resource, at: &str) -> Result<http::Request<Vec<u8>>, String> {
    patch_request(target, &reconcile_patch(at))
}

async fn probe_group(client: &Client, group: &str) -> GroupAnswer {
    let request = match http::Request::get(group_url(group)).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    match client.request::<WireGroup>(request).await {
        Ok(group) => {
            let versions = order_versions(&group.preferred.version, {
                group
                    .versions
                    .into_iter()
                    .map(|item| item.version)
                    .collect()
            });
            GroupAnswer::Served(versions)
        }
        Err(error) => after_group(&error),
    }
}

async fn list_at_version(
    client: &Client,
    kind: Kind,
    version: &str,
    namespace: Option<&str>,
) -> Result<KindSet, ListErr> {
    let path = collection_url(kind, version, namespace);
    let mut items = Vec::new();
    let mut unreadable = 0usize;
    let mut token: Option<String> = None;
    let mut truncated = false;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = match Request::new(path.clone()).list(&params) {
            Ok(request) => request,
            Err(error) => return Err(ListErr::Failed(error.to_string())),
        };
        let page = match client.request::<WireList>(request).await {
            Ok(page) => page,
            Err(error) if items.is_empty() && unreadable == 0 => return Err(after_list(&error)),
            Err(error) => {
                return Err(ListErr::Failed(crate::connect::describe(
                    &error as &(dyn std::error::Error + 'static),
                )));
            }
        };
        for value in page.items {
            if items.len() == MAX_OBJECTS {
                truncated = true;
                break;
            }
            match parse_item(kind, version, value) {
                Some(resource) => items.push(resource),
                None => unreadable += 1,
            }
        }
        token = (!page.metadata.cont.is_empty() && !truncated).then_some(page.metadata.cont);
        if token.is_none() {
            break;
        }
    }
    Ok(KindSet::Served {
        items,
        truncated,
        unreadable,
    })
}

async fn list_kind(
    client: &Client,
    kind: Kind,
    group_versions: &[String],
    namespace: Option<&str>,
) -> Result<KindSet, Fetched<Inventory>> {
    for version in versions_for(kind, group_versions) {
        match list_at_version(client, kind, &version, namespace).await {
            Ok(set) => return Ok(set),
            Err(ListErr::NotFound) => continue,
            Err(ListErr::Denied) => return Ok(KindSet::Denied),
            Err(ListErr::Failed(why)) => {
                return Err(Fetched::Failed {
                    what: kind.what(),
                    why,
                });
            }
        }
    }
    Ok(KindSet::NotServed)
}

async fn fetch_group(
    client: &Client,
    group: &str,
    kinds: &[Kind],
    namespace: Option<&str>,
) -> Result<Vec<KindSet>, Fetched<Inventory>> {
    match probe_group(client, group).await {
        GroupAnswer::NotServed => Ok(kinds.iter().map(|_| KindSet::NotServed).collect()),
        GroupAnswer::Denied => Ok(kinds.iter().map(|_| KindSet::Denied).collect()),
        GroupAnswer::Failed(why) => Err(Fetched::Failed {
            what: kinds.first().map(|kind| kind.what()).unwrap_or("flux"),
            why,
        }),
        GroupAnswer::Served(versions) => {
            let mut sets = Vec::with_capacity(kinds.len());
            for kind in kinds {
                sets.push(list_kind(client, *kind, &versions, namespace).await?);
            }
            Ok(sets)
        }
    }
}

/// List the four Flux kinds. A missing group is invisible; a forbidden one is
/// Denied on that kind and does not hide the others.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let source = match fetch_group(
        client,
        SOURCE_GROUP,
        &[Kind::GitRepository, Kind::OCIRepository],
        namespace,
    )
    .await
    {
        Ok(sets) => sets,
        Err(failed) => return failed,
    };
    let kustomize =
        match fetch_group(client, KUSTOMIZE_GROUP, &[Kind::Kustomization], namespace).await {
            Ok(sets) => sets,
            Err(failed) => return failed,
        };
    let helm = match fetch_group(client, HELM_GROUP, &[Kind::HelmRelease], namespace).await {
        Ok(sets) => sets,
        Err(failed) => return failed,
    };
    let mut source = source.into_iter();
    let mut kustomize = kustomize.into_iter();
    let mut helm = helm.into_iter();
    Fetched::Ok(Inventory {
        git_repositories: source.next().unwrap_or_default(),
        oci_repositories: source.next().unwrap_or_default(),
        kustomizations: kustomize.next().unwrap_or_default(),
        helm_releases: helm.next().unwrap_or_default(),
    })
}

async fn send_patch(
    client: &Client,
    target: &Resource,
    request: http::Request<Vec<u8>>,
) -> Fetched<()> {
    match client.request::<serde_json::Value>(request).await {
        Ok(_) => Fetched::Ok(()),
        Err(error) => classify(target.kind.what(), &error),
    }
}

/// Merge-patch `spec.suspend`. `allowed` is the capability gate: a false
/// value is Denied and does not touch the wire.
pub async fn set_suspended(
    client: &Client,
    target: &Resource,
    suspend: bool,
    allowed: bool,
) -> Fetched<()> {
    if !allowed {
        return Fetched::Denied {
            what: target.kind.what(),
        };
    }
    let request = match suspend_request(target, suspend) {
        Ok(request) => request,
        Err(why) => {
            return Fetched::Failed {
                what: target.kind.what(),
                why,
            };
        }
    };
    send_patch(client, target, request).await
}

/// Merge-patch `reconcile.fluxcd.io/requestedAt` to now, RFC3339. Flux
/// already honours this; there is no other reconcile API here.
pub async fn reconcile_now(client: &Client, target: &Resource, allowed: bool) -> Fetched<()> {
    reconcile_at(client, target, allowed, &rfc3339_now()).await
}

async fn reconcile_at(client: &Client, target: &Resource, allowed: bool, at: &str) -> Fetched<()> {
    if !allowed {
        return Fetched::Denied {
            what: target.kind.what(),
        };
    }
    let request = match reconcile_request(target, at) {
        Ok(request) => request,
        Err(why) => {
            return Fetched::Failed {
                what: target.kind.what(),
                why,
            };
        }
    };
    send_patch(client, target, request).await
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

fn ready_label(status: &str) -> String {
    match status {
        "True" => "Ready".to_string(),
        "False" => "not ready".to_string(),
        "Unknown" => "unknown".to_string(),
        "" => "no Ready condition".to_string(),
        other => other.to_string(),
    }
}

/// The inventory as a document, rendered here for the same reason a describe
/// is: one deterministic rendering is what makes it gateable by a test.
pub fn render(inventory: &Inventory) -> Vec<String> {
    let sets = inventory.sets();
    if sets
        .iter()
        .all(|(set, _)| matches!(set, KindSet::NotServed))
    {
        return vec![
            "Flux is not served by this cluster".to_string(),
            String::new(),
            "this reads GitRepository, OCIRepository, Kustomization and HelmRelease CRs the \
             controllers already publish; nothing is installed to find them, so a cluster \
             without Flux shows as empty here"
                .to_string(),
        ];
    }

    let total: usize = sets.iter().map(|(set, _)| set.items().len()).sum();
    let unreadable: usize = sets
        .iter()
        .map(|(set, _)| match set {
            KindSet::Served { unreadable, .. } => *unreadable,
            KindSet::NotServed | KindSet::Denied => 0,
        })
        .sum();
    let truncated = sets.iter().any(|(set, _)| match set {
        KindSet::Served { truncated, .. } => *truncated,
        KindSet::NotServed | KindSet::Denied => false,
    });
    let denied = sets
        .iter()
        .filter(|(set, _)| matches!(set, KindSet::Denied))
        .count();

    let mut lines = Vec::new();
    if total == 0 && unreadable == 0 && denied == 0 {
        lines.push("no Flux objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no Flux object could be read here, though some are stored: every object this \
             account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!("{} Flux {}", total, plural(total, "object")));
    }
    for (set, kind) in &sets {
        if matches!(set, KindSet::Denied) {
            lines.push(format!("{}: access denied for this account", kind.what()));
        }
    }
    if truncated {
        lines.push(format!(
            "the listing stopped at {MAX_OBJECTS} objects per kind, so this is some of them \
             rather than all",
        ));
    }
    if unreadable > 0 && total > 0 {
        lines.push(format!(
            "{} Flux {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            lines.push(format!("{}/{}", item.namespace, item.name));
            let mut line = format!("  {}  {}", item.kind.as_str(), ready_label(&item.ready));
            if item.suspended {
                line.push_str("  suspended");
            }
            if !item.source_ref.is_empty() {
                line.push_str("  ");
                line.push_str(&item.source_ref);
            }
            if !item.last_applied_revision.is_empty() {
                line.push_str("  ");
                line.push_str(&item.last_applied_revision);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "flux_test.rs"]
mod tests;
