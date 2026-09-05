//! Argo CD inventory from Application / ApplicationSet CRs.
//!
//! No Argo API token. No install. Status is what the controller already
//! published on the object: a GET list of `argoproj.io` CRs, the same way Helm
//! is a GET list of release Secrets. Refresh and sync are merge-patches of the
//! annotation and the `operation` field Argo already honours.
//!
//! A cluster that does not serve the group is not a broken Argo view. It is
//! [`Inventory::served`] = false, so a UI stays invisible rather than empty.
//! Drift is not computed here: the CR already names the desired source, the
//! compared source, the live revision, and the managed object refs, and
//! k10s-edit diffs those later.

use kube::Client;
use kube::api::{ListParams, Patch, PatchParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::apply::FIELD_MANAGER;
use crate::browse::{TableColumn, TablePage, TableRow};
use crate::discover::KindTarget;
use crate::read::{Fetched, classify, collection_path};

const GROUP: &str = "argoproj.io";
const APPLICATION: &str = "Application";
const APPLICATION_SET: &str = "ApplicationSet";

const PAGE_LIMIT: u32 = 200;
const MAX_APPS: usize = 2_000;
const MAX_PAGE_BYTES: usize = 8 << 20;
const MAX_FIELD_CHARS: usize = 200;

pub const REFRESH_ANNOTATION: &str = "argocd.argoproj.io/refresh";

/// How Argo spells a refresh on the Application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    Normal,
    Hard,
}

impl Refresh {
    pub fn as_str(self) -> &'static str {
        match self {
            Refresh::Normal => "normal",
            Refresh::Hard => "hard",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Source {
    pub repo: String,
    pub revision: String,
    pub path: String,
    pub chart: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Destination {
    pub server: String,
    pub namespace: String,
    pub name: String,
}

/// One object the Application already recorded in `status.resources`.
///
/// Identity plus the controller's own sync/health words. Not a live GET, and
/// not a three-way: the CR does not carry desired and live manifests here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagedResource {
    pub group: String,
    pub version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub sync: String,
    pub health: String,
}

/// Desired versus live as the Application already wrote them.
///
/// `desired` is `spec.source` (repo + targetRevision). `compared` is
/// `status.sync.comparedTo`. `live_revision` is `status.sync.revision`, the
/// SHA the controller compared against the cluster. k10s-edit can GET those
/// refs later; this module does not.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DriftRefs {
    pub desired: Source,
    pub compared: Source,
    pub live_revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Application {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub destination: Destination,
    pub sources: Vec<Source>,
    pub sync: String,
    pub health: String,
    pub drift: DriftRefs,
    pub resources: Vec<ManagedResource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationSet {
    pub name: String,
    pub namespace: String,
    pub destination: Destination,
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub applications: Vec<Application>,
    pub application_sets: Vec<ApplicationSet>,
    /// The listing stopped at [`MAX_APPS`] on either kind.
    pub truncated: bool,
    /// False when discovery has no argoproj kinds, or every list 404s. The UI
    /// stays invisible. An empty served inventory is a cluster that has the
    /// CRDs and no Applications.
    pub served: bool,
    /// The Application kind takes a patch verb, so refresh and sync are
    /// offered. Discovery, not this account: a 403 still arrives as Denied.
    pub patchable: bool,
}

impl Inventory {
    pub(crate) fn unserved() -> Inventory {
        Inventory::default()
    }
}

#[derive(Deserialize)]
struct WireList {
    #[serde(default)]
    metadata: WireListMeta,
    #[serde(default)]
    items: Vec<Value>,
}

#[derive(Deserialize, Default)]
struct WireListMeta {
    #[serde(default, rename = "continue")]
    cont: String,
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
struct WireApp {
    #[serde(default)]
    metadata: WireMeta,
    #[serde(default)]
    spec: WireAppSpec,
    #[serde(default)]
    status: WireAppStatus,
}

#[derive(Deserialize, Default)]
struct WireAppSpec {
    #[serde(default)]
    source: WireSource,
    #[serde(default)]
    sources: Vec<WireSource>,
    #[serde(default)]
    destination: WireDest,
}

#[derive(Deserialize, Default)]
struct WireSource {
    #[serde(default, rename = "repoURL")]
    repo: String,
    #[serde(default, rename = "targetRevision")]
    revision: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    chart: String,
}

#[derive(Deserialize, Default)]
struct WireDest {
    #[serde(default)]
    server: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireAppStatus {
    #[serde(default)]
    sync: WireSync,
    #[serde(default)]
    health: WireHealth,
    #[serde(default)]
    resources: Vec<WireResource>,
}

#[derive(Deserialize, Default)]
struct WireSync {
    #[serde(default)]
    status: String,
    #[serde(default)]
    revision: String,
    #[serde(default, rename = "comparedTo")]
    compared_to: WireCompared,
}

#[derive(Deserialize, Default)]
struct WireCompared {
    #[serde(default)]
    source: WireSource,
}

#[derive(Deserialize, Default)]
struct WireHealth {
    #[serde(default)]
    status: String,
}

#[derive(Deserialize, Default)]
struct WireResource {
    #[serde(default)]
    group: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    uid: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    health: WireHealth,
}

#[derive(Deserialize, Default)]
struct WireAppSet {
    #[serde(default)]
    metadata: WireMeta,
    #[serde(default)]
    spec: WireAppSetSpec,
}

#[derive(Deserialize, Default)]
struct WireAppSetSpec {
    #[serde(default)]
    template: WireAppSetTemplate,
}

#[derive(Deserialize, Default)]
struct WireAppSetTemplate {
    #[serde(default)]
    spec: WireAppSpec,
}

pub(crate) struct ArgoKinds<'a> {
    pub(crate) applications: Option<&'a KindTarget>,
    pub(crate) application_sets: Option<&'a KindTarget>,
}

fn find<'a>(targets: &'a [KindTarget], kind: &str) -> Option<&'a KindTarget> {
    targets
        .iter()
        .find(|target| target.group() == GROUP && target.kind() == kind)
}

pub(crate) fn argo_kinds(targets: &[KindTarget]) -> Option<ArgoKinds<'_>> {
    let applications = find(targets, APPLICATION);
    let application_sets = find(targets, APPLICATION_SET);
    if applications.is_none() && application_sets.is_none() {
        None
    } else {
        Some(ArgoKinds {
            applications,
            application_sets,
        })
    }
}

fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn source_from(wire: &WireSource) -> Source {
    Source {
        repo: clipped(wire.repo.clone()),
        revision: clipped(wire.revision.clone()),
        path: clipped(wire.path.clone()),
        chart: clipped(wire.chart.clone()),
    }
}

fn dest_from(wire: &WireDest) -> Destination {
    Destination {
        server: clipped(wire.server.clone()),
        namespace: clipped(wire.namespace.clone()),
        name: clipped(wire.name.clone()),
    }
}

fn sources_of(spec: &WireAppSpec) -> Vec<Source> {
    if !spec.sources.is_empty() {
        return spec.sources.iter().map(source_from).collect();
    }
    let source = source_from(&spec.source);
    if source.repo.is_empty() && source.revision.is_empty() && source.chart.is_empty() {
        Vec::new()
    } else {
        vec![source]
    }
}

fn first_source(sources: &[Source]) -> Source {
    sources.first().cloned().unwrap_or_default()
}

pub(crate) fn application_from_value(value: Value) -> Option<Application> {
    let wire: WireApp = serde_json::from_value(value).ok()?;
    let name = clipped(wire.metadata.name);
    if name.is_empty() {
        return None;
    }
    let sources = sources_of(&wire.spec);
    let compared = source_from(&wire.status.sync.compared_to.source);
    Some(Application {
        name,
        namespace: clipped(wire.metadata.namespace),
        uid: clipped(wire.metadata.uid),
        destination: dest_from(&wire.spec.destination),
        drift: DriftRefs {
            desired: first_source(&sources),
            compared,
            live_revision: clipped(wire.status.sync.revision),
        },
        sources,
        sync: clipped(wire.status.sync.status),
        health: clipped(wire.status.health.status),
        resources: wire
            .status
            .resources
            .into_iter()
            .map(|resource| ManagedResource {
                group: clipped(resource.group),
                version: clipped(resource.version),
                kind: clipped(resource.kind),
                namespace: clipped(resource.namespace),
                name: clipped(resource.name),
                uid: clipped(resource.uid),
                sync: clipped(resource.status),
                health: clipped(resource.health.status),
            })
            .collect(),
    })
}

pub(crate) fn applicationset_from_value(value: Value) -> Option<ApplicationSet> {
    let wire: WireAppSet = serde_json::from_value(value).ok()?;
    let name = clipped(wire.metadata.name);
    if name.is_empty() {
        return None;
    }
    Some(ApplicationSet {
        name,
        namespace: clipped(wire.metadata.namespace),
        destination: dest_from(&wire.spec.template.spec.destination),
        sources: sources_of(&wire.spec.template.spec),
    })
}

pub(crate) fn applications_from_items(
    items: impl IntoIterator<Item = Value>,
) -> (Vec<Application>, bool) {
    let mut apps = Vec::new();
    let mut truncated = false;
    for item in items {
        if apps.len() == MAX_APPS {
            truncated = true;
            break;
        }
        if let Some(app) = application_from_value(item) {
            apps.push(app);
        }
    }
    apps.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    (apps, truncated)
}

pub(crate) fn applicationsets_from_items(
    items: impl IntoIterator<Item = Value>,
) -> (Vec<ApplicationSet>, bool) {
    let mut sets = Vec::new();
    let mut truncated = false;
    for item in items {
        if sets.len() == MAX_APPS {
            truncated = true;
            break;
        }
        if let Some(set) = applicationset_from_value(item) {
            sets.push(set);
        }
    }
    sets.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    (sets, truncated)
}

#[derive(Debug)]
pub(crate) enum PageError {
    TooLarge,
    NotJson(String),
}

fn parse_list(text: &str) -> Result<WireList, PageError> {
    if text.len() > MAX_PAGE_BYTES {
        return Err(PageError::TooLarge);
    }
    serde_json::from_str(text).map_err(|error| PageError::NotJson(error.to_string()))
}

#[cfg(test)]
pub(crate) fn parse_page(text: &str) -> Result<Vec<Value>, PageError> {
    Ok(parse_list(text)?.items)
}

enum KindList {
    Absent,
    Items { items: Vec<Value>, truncated: bool },
}

fn page_failed(what: &'static str, error: PageError) -> Fetched<KindList> {
    match error {
        PageError::TooLarge => Fetched::Failed {
            what,
            why: "the list page is larger than 8 MiB; the page is not shown".to_string(),
        },
        PageError::NotJson(why) => Fetched::Failed {
            what,
            why: format!("the list is not JSON: {why}"),
        },
    }
}

#[derive(Debug)]
pub(crate) enum ListMiss {
    Absent,
    Denied,
}

pub(crate) fn list_miss(error: &kube::Error) -> Option<ListMiss> {
    if let kube::Error::Api(response) = error {
        match response.code {
            404 => return Some(ListMiss::Absent),
            401 | 403 => return Some(ListMiss::Denied),
            _ => {}
        }
    }
    None
}

async fn list_kind(
    client: &Client,
    target: &KindTarget,
    namespace: Option<&str>,
    what: &'static str,
) -> Fetched<KindList> {
    if !target.listable {
        return Fetched::Failed {
            what,
            why: format!(
                "the server serves {} without a list verb, so stored objects cannot be read",
                target.kind()
            ),
        };
    }
    let path = collection_path(target, namespace);
    let mut items = Vec::new();
    let mut token: Option<String> = None;
    let mut truncated = false;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = match Request::new(path.clone()).list(&params) {
            Ok(request) => request,
            Err(error) => {
                return Fetched::Failed {
                    what,
                    why: error.to_string(),
                };
            }
        };
        let text = match client.request_text(request).await {
            Ok(text) => text,
            Err(error) => {
                return match list_miss(&error) {
                    Some(ListMiss::Absent) => Fetched::Ok(KindList::Absent),
                    Some(ListMiss::Denied) => Fetched::Denied { what },
                    None => classify(what, &error),
                };
            }
        };
        let page = match parse_list(&text) {
            Ok(page) => page,
            Err(error) => return page_failed(what, error),
        };
        for item in page.items {
            if items.len() == MAX_APPS {
                truncated = true;
                break;
            }
            items.push(item);
        }
        token = (!page.metadata.cont.is_empty() && !truncated).then_some(page.metadata.cont);
        if token.is_none() {
            break;
        }
    }
    Fetched::Ok(KindList::Items { items, truncated })
}

/// List Applications and ApplicationSets the controller already published.
///
/// Discovery miss or a 404 list is [`Inventory::served`] = false, not an
/// error. 403 is Denied.
pub async fn fetch_inventory(
    client: &Client,
    targets: &[KindTarget],
    namespace: Option<&str>,
) -> Fetched<Inventory> {
    let Some(kinds) = argo_kinds(targets) else {
        return Fetched::Ok(Inventory::unserved());
    };
    let mut served = false;
    let mut truncated = false;
    let mut applications = Vec::new();
    let mut application_sets = Vec::new();
    let patchable = kinds.applications.is_some_and(|target| target.patchable);

    if let Some(target) = kinds.applications {
        match list_kind(client, target, namespace, "argo applications").await {
            Fetched::Ok(KindList::Absent) => {}
            Fetched::Ok(KindList::Items {
                items,
                truncated: page_truncated,
            }) => {
                served = true;
                truncated |= page_truncated;
                let (apps, cap) = applications_from_items(items);
                truncated |= cap;
                applications = apps;
            }
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { what, why } => return Fetched::Failed { what, why },
        }
    }
    if let Some(target) = kinds.application_sets {
        match list_kind(client, target, namespace, "argo application sets").await {
            Fetched::Ok(KindList::Absent) => {}
            Fetched::Ok(KindList::Items {
                items,
                truncated: page_truncated,
            }) => {
                served = true;
                truncated |= page_truncated;
                let (sets, cap) = applicationsets_from_items(items);
                truncated |= cap;
                application_sets = sets;
            }
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { what, why } => return Fetched::Failed { what, why },
        }
    }
    if !served {
        return Fetched::Ok(Inventory::unserved());
    }
    Fetched::Ok(Inventory {
        applications,
        application_sets,
        truncated,
        served: true,
        patchable,
    })
}

pub(crate) fn refresh_patch(mode: Refresh) -> Value {
    serde_json::json!({
        "metadata": {
            "annotations": {
                REFRESH_ANNOTATION: mode.as_str()
            }
        }
    })
}

/// The merge-patch body Argo's controller honours for a sync.
///
/// The requested operation lives on the Application object itself (`operation`),
/// which is what the CRD names. It is not `spec.operation`.
pub(crate) fn sync_patch() -> Value {
    serde_json::json!({
        "operation": {
            "sync": {}
        }
    })
}

pub(crate) fn gate_action<'a>(
    targets: &'a [KindTarget],
    what: &'static str,
) -> Result<&'a KindTarget, Fetched<()>> {
    let Some(target) = find(targets, APPLICATION) else {
        return Err(Fetched::Failed {
            what,
            why: "this kind is not served by the connected cluster".to_string(),
        });
    };
    if !target.patchable {
        return Err(Fetched::Failed {
            what,
            why: format!(
                "the server serves {} without a patch verb, so it cannot be patched",
                target.kind()
            ),
        });
    }
    Ok(target)
}

async fn patch_application(
    client: &Client,
    targets: &[KindTarget],
    namespace: &str,
    name: &str,
    what: &'static str,
    body: &Value,
) -> Fetched<()> {
    let target = match gate_action(targets, what) {
        Ok(target) => target,
        Err(fetched) => return fetched,
    };
    let params = PatchParams {
        field_manager: Some(FIELD_MANAGER.to_string()),
        ..PatchParams::default()
    };
    let request = match Request::new(collection_path(target, Some(namespace))).patch(
        name,
        &params,
        &Patch::Merge(body),
    ) {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what,
                why: error.to_string(),
            };
        }
    };
    match client.request::<Value>(request).await {
        Ok(_) => Fetched::Ok(()),
        Err(error) => classify(what, &error),
    }
}

/// Patch `argocd.argoproj.io/refresh` to `normal` or `hard`.
pub async fn refresh(
    client: &Client,
    targets: &[KindTarget],
    namespace: &str,
    name: &str,
    mode: Refresh,
) -> Fetched<()> {
    patch_application(
        client,
        targets,
        namespace,
        name,
        "argo refresh",
        &refresh_patch(mode),
    )
    .await
}

/// Patch `operation.sync` so the controller starts a sync.
pub async fn sync(
    client: &Client,
    targets: &[KindTarget],
    namespace: &str,
    name: &str,
) -> Fetched<()> {
    patch_application(client, targets, namespace, name, "argo sync", &sync_patch()).await
}

fn resource_label(resource: &ManagedResource) -> String {
    match (resource.group.as_str(), resource.namespace.as_str()) {
        ("", "") => format!("{}/{}", resource.kind, resource.name),
        ("", ns) => format!("{}/{ns}/{}", resource.kind, resource.name),
        (group, "") => format!("{group}/{}/{}", resource.kind, resource.name),
        (group, ns) => format!("{group}/{}/{ns}/{}", resource.kind, resource.name),
    }
}

fn dest_label(dest: &Destination) -> String {
    if !dest.name.is_empty() && !dest.namespace.is_empty() {
        format!("{}/{}", dest.name, dest.namespace)
    } else if !dest.namespace.is_empty() {
        dest.namespace.clone()
    } else if !dest.name.is_empty() {
        dest.name.clone()
    } else {
        dest.server.clone()
    }
}

fn source_label(source: &Source) -> String {
    match (
        source.repo.as_str(),
        source.revision.as_str(),
        source.chart.as_str(),
    ) {
        ("", "", "") => String::new(),
        (repo, "", "") => repo.to_string(),
        (repo, rev, "") => format!("{repo}@{rev}"),
        (repo, rev, chart) if !rev.is_empty() => format!("{repo}@{rev} chart {chart}"),
        (repo, _, chart) => format!("{repo} chart {chart}"),
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

/// Native list rows. `None` when the group is not served, so a UI stays
/// invisible rather than opening an empty pane. An empty `Some` is a cluster
/// that has the CRDs and no Applications.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served {
        return None;
    }
    let columns = ["Kind", "Name", "Namespace", "Sync", "Health", "Destination"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let mut rows =
        Vec::with_capacity(inventory.applications.len() + inventory.application_sets.len());
    for app in &inventory.applications {
        let uid = if app.uid.is_empty() {
            format!("{}/{}", app.namespace, app.name)
        } else {
            app.uid.clone()
        };
        rows.push(TableRow {
            cells: vec![
                APPLICATION.to_string(),
                app.name.clone(),
                app.namespace.clone(),
                app.sync.clone(),
                app.health.clone(),
                dest_label(&app.destination),
            ],
            name: app.name.clone(),
            namespace: Some(app.namespace.clone()),
            uid,
        });
    }
    for set in &inventory.application_sets {
        rows.push(TableRow {
            cells: vec![
                APPLICATION_SET.to_string(),
                set.name.clone(),
                set.namespace.clone(),
                String::new(),
                String::new(),
                dest_label(&set.destination),
            ],
            name: set.name.clone(),
            namespace: Some(set.namespace.clone()),
            uid: format!("applicationset/{}/{}", set.namespace, set.name),
        });
    }
    Some(TablePage {
        columns,
        rows,
        truncated: inventory.truncated,
        continue_token: None,
    })
}

/// The inventory as a document, rendered here for the same reason a describe is:
/// one deterministic rendering is what makes it gateable by a test rather than
/// by a screenshot.
pub fn render(inventory: &Inventory) -> Vec<String> {
    if !inventory.served {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if inventory.applications.is_empty() && inventory.application_sets.is_empty() {
        lines.push("no Argo CD Applications are in this cluster".to_string());
        lines.push(String::new());
        lines.push(
            "this reads Application and ApplicationSet CRs the controller already publishes; \
             nothing is installed to find them, and no Argo API token is used"
                .to_string(),
        );
    } else {
        let apps = inventory.applications.len();
        let sets = inventory.application_sets.len();
        let mut head = format!("{} {}", apps, plural(apps, "application"));
        if sets > 0 {
            head.push_str(&format!(
                ", {} application {}",
                sets,
                if sets == 1 { "set" } else { "sets" }
            ));
        }
        lines.push(head);
    }
    if inventory.truncated {
        lines.push(format!(
            "the listing stopped at {MAX_APPS} objects, so this is some of them rather than all",
        ));
    }
    for app in &inventory.applications {
        lines.push(String::new());
        let mut line = format!("{}/{}", app.namespace, app.name);
        if !app.sync.is_empty() {
            line.push_str(&format!("  {}", app.sync));
        }
        if !app.health.is_empty() {
            line.push_str(&format!("  {}", app.health));
        }
        if let Some(source) = app.sources.first() {
            let label = source_label(source);
            if !label.is_empty() {
                line.push_str(&format!("  {label}"));
            }
        }
        let dest = dest_label(&app.destination);
        if !dest.is_empty() {
            line.push_str(&format!("  dest {dest}"));
        }
        if !app.drift.live_revision.is_empty() {
            line.push_str(&format!("  live {}", app.drift.live_revision));
        }
        lines.push(line);
        for resource in &app.resources {
            let mut row = format!("  {}", resource_label(resource));
            if !resource.sync.is_empty() {
                row.push_str(&format!("  {}", resource.sync));
            }
            if !resource.health.is_empty() {
                row.push_str(&format!("  {}", resource.health));
            }
            lines.push(row);
        }
    }
    for set in &inventory.application_sets {
        lines.push(String::new());
        let mut line = format!("{}/{}", set.namespace, set.name);
        if let Some(source) = set.sources.first() {
            let label = source_label(source);
            if !label.is_empty() {
                line.push_str(&format!("  {label}"));
            }
        }
        let dest = dest_label(&set.destination);
        if !dest.is_empty() {
            line.push_str(&format!("  dest {dest}"));
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
#[path = "argo_test.rs"]
mod tests;
