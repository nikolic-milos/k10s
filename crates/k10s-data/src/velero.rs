//! Velero inventory from the CRs the controller already publishes.
//!
//! Backup, Restore, Schedule, BackupStorageLocation, and
//! VolumeSnapshotLocation live on `velero.io`. A cluster that does not serve
//! the group answers 404 and the inventory is invisible, not broken; a 403 is
//! Denied. Nothing is installed to find them, and nothing is reimplemented:
//! this is not a backup engine, and it never downloads a backup tarball or
//! reads a BackupStorageLocation credentials Secret.
//!
//! Creating a backup is a server-side apply of a `velero.io` Backup CR the
//! caller confirmed. That is the CR the controller already honours, not
//! `velero backup create`. `confirm=false` is the first press and does not
//! touch the wire.

use kube::Client;
use kube::api::{ListParams, Patch, PatchParams, Request, ValidationDirective};
use serde::Deserialize;

use crate::apply::FIELD_MANAGER;
use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::{Fetched, classify};

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;

const GROUP: &str = "velero.io";

/// The five CRs this inventory reads. Velero serves more; those are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Backup,
    Restore,
    Schedule,
    BackupStorageLocation,
    VolumeSnapshotLocation,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Backup => "Backup",
            Kind::Restore => "Restore",
            Kind::Schedule => "Schedule",
            Kind::BackupStorageLocation => "BackupStorageLocation",
            Kind::VolumeSnapshotLocation => "VolumeSnapshotLocation",
        }
    }

    pub fn group(self) -> &'static str {
        GROUP
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::Backup => "backups",
            Kind::Restore => "restores",
            Kind::Schedule => "schedules",
            Kind::BackupStorageLocation => "backupstoragelocations",
            Kind::VolumeSnapshotLocation => "volumesnapshotlocations",
        }
    }

    pub fn version(self) -> &'static str {
        "v1"
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::Backup => "velero backups",
            Kind::Restore => "velero restores",
            Kind::Schedule => "velero schedules",
            Kind::BackupStorageLocation => "velero backupstoragelocations",
            Kind::VolumeSnapshotLocation => "velero volumesnapshotlocations",
        }
    }
}

/// One CR, reduced to what an inventory shows.
///
/// There is nowhere here for object-storage credentials or a tarball: those
/// stay on the controller's side of the Secret and the BSL bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub phase: String,
    pub warnings: i64,
    pub errors: i64,
    pub started: String,
    pub completed: String,
    pub storage_location: String,
    /// Joined and clipped. Empty means the CR named none (Velero treats that
    /// as every namespace).
    pub included_namespaces: String,
    pub schedule: String,
    /// BSL `spec.credential.name` only. Never a Secret value.
    pub credential_secret: String,
    /// Restore `spec.backupName`. Empty on every other kind.
    pub backup_name: String,
}

/// What one kind's list answered.
///
/// A 404 on the group is [`KindSet::NotServed`]: invisible, not broken. A 403
/// is [`KindSet::Denied`]. Those are different states on purpose; collapsing
/// them would tell someone Velero is absent when the account was refused.
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
    pub backups: KindSet,
    pub restores: KindSet,
    pub schedules: KindSet,
    pub storage_locations: KindSet,
    pub snapshot_locations: KindSet,
}

impl Inventory {
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Kind); 5] {
        [
            (&self.backups, Kind::Backup),
            (&self.restores, Kind::Restore),
            (&self.schedules, Kind::Schedule),
            (&self.storage_locations, Kind::BackupStorageLocation),
            (&self.snapshot_locations, Kind::VolumeSnapshotLocation),
        ]
    }
}

/// The Backup CR a confirmed apply sends. This is the document, not a CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupDocument {
    pub name: String,
    pub namespace: String,
    pub included_namespaces: Vec<String>,
    pub storage_location: String,
}

/// First press versus the apply that actually went on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Needed,
    Sent,
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
    #[serde(default, rename = "includedNamespaces")]
    included_namespaces: Vec<String>,
    #[serde(default, rename = "storageLocation")]
    storage_location: String,
    #[serde(default)]
    schedule: String,
    #[serde(default)]
    template: WireTemplate,
    #[serde(default, rename = "backupName")]
    backup_name: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    credential: WireCredential,
    #[serde(default, rename = "objectStorage")]
    object_storage: WireObjectStorage,
}

#[derive(Deserialize, Default)]
struct WireTemplate {
    #[serde(default, rename = "includedNamespaces")]
    included_namespaces: Vec<String>,
    #[serde(default, rename = "storageLocation")]
    storage_location: String,
}

#[derive(Deserialize, Default)]
struct WireCredential {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireObjectStorage {
    #[serde(default)]
    bucket: String,
}

#[derive(Deserialize, Default)]
struct WireStatus {
    #[serde(default)]
    phase: String,
    #[serde(default)]
    warnings: i64,
    #[serde(default)]
    errors: i64,
    #[serde(default, rename = "startTimestamp")]
    start_timestamp: String,
    #[serde(default, rename = "completionTimestamp")]
    completion_timestamp: String,
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

fn join_clipped(parts: &[String]) -> String {
    clipped(parts.join(", "))
}

fn storage_of(kind: Kind, spec: &WireSpec) -> String {
    let text = match kind {
        Kind::Backup => spec.storage_location.as_str(),
        // RestoreSpec has no storageLocation; the storage a Restore reads
        // from is the referenced Backup's (spec.backupName), carried in
        // `backup_name`.
        Kind::Restore => "",
        Kind::Schedule => spec.template.storage_location.as_str(),
        Kind::BackupStorageLocation => {
            if spec.object_storage.bucket.is_empty() {
                spec.provider.as_str()
            } else if spec.provider.is_empty() {
                spec.object_storage.bucket.as_str()
            } else {
                return clipped(format!("{}/{}", spec.provider, spec.object_storage.bucket));
            }
        }
        Kind::VolumeSnapshotLocation => spec.provider.as_str(),
    };
    clipped(text.to_string())
}

fn namespaces_of(kind: Kind, spec: &WireSpec) -> String {
    match kind {
        Kind::Schedule => join_clipped(&spec.template.included_namespaces),
        Kind::Backup | Kind::Restore => join_clipped(&spec.included_namespaces),
        Kind::BackupStorageLocation | Kind::VolumeSnapshotLocation => String::new(),
    }
}

fn schedule_of(kind: Kind, spec: &WireSpec) -> String {
    match kind {
        Kind::Schedule => clipped(spec.schedule.clone()),
        _ => String::new(),
    }
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
        phase: clipped(wire.status.phase),
        warnings: wire.status.warnings,
        errors: wire.status.errors,
        started: clipped(wire.status.start_timestamp),
        completed: clipped(wire.status.completion_timestamp),
        storage_location: storage_of(kind, &wire.spec),
        included_namespaces: namespaces_of(kind, &wire.spec),
        schedule: schedule_of(kind, &wire.spec),
        credential_secret: clipped(wire.spec.credential.name),
        backup_name: clipped(wire.spec.backup_name),
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

fn group_url(group: &str) -> String {
    format!("/apis/{group}")
}

/// The Backup CR body a confirmed apply sends. Kind and apiVersion are
/// Velero's; this is not a `velero backup create` payload.
pub fn backup_document(document: &BackupDocument) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "velero.io/v1",
        "kind": "Backup",
        "metadata": {
            "name": document.name,
            "namespace": document.namespace
        },
        "spec": {
            "includedNamespaces": document.included_namespaces,
            "storageLocation": document.storage_location
        }
    })
}

fn backup_apply_params() -> PatchParams {
    PatchParams {
        field_manager: Some(FIELD_MANAGER.to_string()),
        field_validation: Some(ValidationDirective::Strict),
        ..PatchParams::default()
    }
}

fn backup_apply_request(document: &BackupDocument) -> Result<http::Request<Vec<u8>>, String> {
    let path = collection_url(
        Kind::Backup,
        Kind::Backup.version(),
        Some(document.namespace.as_str()),
    );
    Request::new(path)
        .patch(
            &document.name,
            &backup_apply_params(),
            &Patch::Apply(backup_document(document)),
        )
        .map_err(|error| error.to_string())
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
    kinds: &[Kind],
    namespace: Option<&str>,
) -> Result<Vec<KindSet>, Fetched<Inventory>> {
    match probe_group(client, GROUP).await {
        GroupAnswer::NotServed => Ok(kinds.iter().map(|_| KindSet::NotServed).collect()),
        GroupAnswer::Denied => Ok(kinds.iter().map(|_| KindSet::Denied).collect()),
        GroupAnswer::Failed(why) => Err(Fetched::Failed {
            what: kinds.first().map(|kind| kind.what()).unwrap_or("velero"),
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

/// List the five Velero kinds. A missing group is invisible; a forbidden one
/// is Denied on every kind.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let sets = match fetch_group(
        client,
        &[
            Kind::Backup,
            Kind::Restore,
            Kind::Schedule,
            Kind::BackupStorageLocation,
            Kind::VolumeSnapshotLocation,
        ],
        namespace,
    )
    .await
    {
        Ok(sets) => sets,
        Err(failed) => return failed,
    };
    let mut sets = sets.into_iter();
    Fetched::Ok(Inventory {
        backups: sets.next().unwrap_or_default(),
        restores: sets.next().unwrap_or_default(),
        schedules: sets.next().unwrap_or_default(),
        storage_locations: sets.next().unwrap_or_default(),
        snapshot_locations: sets.next().unwrap_or_default(),
    })
}

/// Server-side apply of a Velero Backup CR. `confirm=false` returns
/// [`Confirm::Needed`] and does not touch the wire.
pub async fn apply_backup(
    client: &Client,
    document: &BackupDocument,
    confirm: bool,
) -> Fetched<Confirm> {
    if !confirm {
        return Fetched::Ok(Confirm::Needed);
    }
    if document.name.is_empty() || document.namespace.is_empty() {
        return Fetched::Failed {
            what: Kind::Backup.what(),
            why: "a Velero Backup CR needs a name and a namespace".to_string(),
        };
    }
    let request = match backup_apply_request(document) {
        Ok(request) => request,
        Err(why) => {
            return Fetched::Failed {
                what: Kind::Backup.what(),
                why,
            };
        }
    };
    match client.request::<serde_json::Value>(request).await {
        Ok(_) => Fetched::Ok(Confirm::Sent),
        Err(error) => classify(Kind::Backup.what(), &error),
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

fn extra_of(item: &Resource) -> String {
    match item.kind {
        Kind::Schedule => item.schedule.clone(),
        Kind::Restore if !item.backup_name.is_empty() => item.backup_name.clone(),
        Kind::BackupStorageLocation if !item.credential_secret.is_empty() => {
            format!("secret {}", item.credential_secret)
        }
        _ => item.included_namespaces.clone(),
    }
}

/// Native list rows. `None` when the group answered 404, so a UI stays
/// invisible rather than opening an empty pane. A denied kind is a labelled
/// row, not absence.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served() {
        return None;
    }
    let columns = [
        "Kind",
        "Name",
        "Namespace",
        "Phase",
        "Warnings",
        "Errors",
        "Storage",
        "Detail",
    ]
    .iter()
    .map(|name| TableColumn {
        name: name.to_string(),
        wide: false,
    })
    .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    for (set, kind) in inventory.sets() {
        match set {
            KindSet::NotServed => {}
            KindSet::Denied => {
                rows.push(TableRow {
                    cells: vec![
                        kind.as_str().to_string(),
                        String::new(),
                        String::new(),
                        "access denied for this account".to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                    name: kind.as_str().to_string(),
                    namespace: None,
                    uid: format!("denied:{}", kind.as_str()),
                });
            }
            KindSet::Served {
                items,
                truncated: cap,
                ..
            } => {
                truncated |= *cap;
                for item in items {
                    let uid = if item.uid.is_empty() {
                        format!("{}/{}/{}", item.kind.as_str(), item.namespace, item.name)
                    } else {
                        item.uid.clone()
                    };
                    rows.push(TableRow {
                        cells: vec![
                            item.kind.as_str().to_string(),
                            item.name.clone(),
                            item.namespace.clone(),
                            item.phase.clone(),
                            item.warnings.to_string(),
                            item.errors.to_string(),
                            item.storage_location.clone(),
                            extra_of(item),
                        ],
                        name: item.name.clone(),
                        namespace: Some(item.namespace.clone()),
                        uid,
                    });
                }
            }
        }
    }
    Some(TablePage {
        columns,
        rows,
        truncated,
        continue_token: None,
    })
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
            "Velero is not served by this cluster".to_string(),
            String::new(),
            "this reads Backup, Restore, Schedule, BackupStorageLocation and \
             VolumeSnapshotLocation CRs the controller already publishes; nothing \
             is installed to find them, no tarball is downloaded, and a \
             BackupStorageLocation credentials Secret is never read"
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
        lines.push("no Velero objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 {
        lines.push(
            "no Velero object could be read here, though some are stored: every object this \
             account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!("{} Velero {}", total, plural(total, "object")));
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
            "{} Velero {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            lines.push(format!("{}/{}", item.namespace, item.name));
            let mut line = format!("  {}  {}", item.kind.as_str(), item.phase);
            if item.warnings != 0 || item.errors != 0 {
                line.push_str(&format!(
                    "  {} warnings  {} errors",
                    item.warnings, item.errors
                ));
            }
            if !item.storage_location.is_empty() {
                line.push_str("  ");
                line.push_str(&item.storage_location);
            }
            if !item.schedule.is_empty() {
                line.push_str("  ");
                line.push_str(&item.schedule);
            }
            if !item.included_namespaces.is_empty() {
                line.push_str("  ");
                line.push_str(&item.included_namespaces);
            }
            if !item.backup_name.is_empty() {
                line.push_str("  backup ");
                line.push_str(&item.backup_name);
            }
            if !item.started.is_empty() {
                line.push_str("  ");
                line.push_str(&item.started);
            }
            if !item.completed.is_empty() {
                line.push_str("  ");
                line.push_str(&item.completed);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "velero_test.rs"]
mod tests;
