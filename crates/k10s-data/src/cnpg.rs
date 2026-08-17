//! CloudNativePG inventory from the CRs the operator already publishes.
//!
//! Cluster, Backup, ScheduledBackup, and Pooler live on `postgresql.cnpg.io`.
//! A cluster that does not serve the group answers 404 and the inventory is
//! invisible, not broken; a 403 is Denied. Nothing is installed to find them,
//! and nothing is reimplemented: this is not a Postgres operator. Superuser
//! access is the Secret *name* the CR already wrote. A password planted on a
//! fixture CR is not a field this module carries, so it cannot appear in
//! Debug, a table, or the document.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;

const GROUP: &str = "postgresql.cnpg.io";

/// The four CRs this inventory reads. CNPG serves more; those are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cluster,
    Backup,
    ScheduledBackup,
    Pooler,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Cluster => "Cluster",
            Kind::Backup => "Backup",
            Kind::ScheduledBackup => "ScheduledBackup",
            Kind::Pooler => "Pooler",
        }
    }

    pub fn group(self) -> &'static str {
        GROUP
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::Cluster => "clusters",
            Kind::Backup => "backups",
            Kind::ScheduledBackup => "scheduledbackups",
            Kind::Pooler => "poolers",
        }
    }

    pub fn version(self) -> &'static str {
        "v1"
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::Cluster => "cnpg clusters",
            Kind::Backup => "cnpg backups",
            Kind::ScheduledBackup => "cnpg scheduledbackups",
            Kind::Pooler => "cnpg poolers",
        }
    }
}

/// One CR, reduced to what an inventory shows.
///
/// [`Resource::superuser_secret`] is a Secret name. There is nowhere here for
/// a password, a connection string, or Secret data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub instances: i64,
    pub ready_instances: i64,
    pub primary: String,
    pub phase: String,
    pub postgres_version: String,
    pub superuser_secret: String,
    pub cluster: String,
    pub schedule: String,
    pub pooler_type: String,
}

/// What one kind's list answered.
///
/// A 404 on the group is [`KindSet::NotServed`]: invisible, not broken. A 403
/// is [`KindSet::Denied`]. Those are different states on purpose; collapsing
/// them would tell someone CNPG is absent when the account was refused.
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
    pub clusters: KindSet,
    pub backups: KindSet,
    pub scheduled_backups: KindSet,
    pub poolers: KindSet,
}

impl Inventory {
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Kind); 4] {
        [
            (&self.clusters, Kind::Cluster),
            (&self.backups, Kind::Backup),
            (&self.scheduled_backups, Kind::ScheduledBackup),
            (&self.poolers, Kind::Pooler),
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
    instances: i64,
    #[serde(default, rename = "imageName")]
    image_name: String,
    #[serde(default, rename = "superuserSecret")]
    superuser_secret: WireSecretRef,
    #[serde(default)]
    cluster: WireClusterRef,
    #[serde(default)]
    schedule: String,
    #[serde(default, rename = "type")]
    pooler_type: String,
}

#[derive(Deserialize, Default)]
struct WireSecretRef {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireClusterRef {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireStatus {
    #[serde(default)]
    instances: i64,
    #[serde(default, rename = "readyInstances")]
    ready_instances: i64,
    #[serde(default, rename = "currentPrimary")]
    current_primary: String,
    #[serde(default)]
    phase: String,
    #[serde(default)]
    image: String,
    #[serde(default, rename = "pgDataImageInfo")]
    pg_data: WirePgData,
}

#[derive(Deserialize, Default)]
struct WirePgData {
    #[serde(default)]
    image: String,
    #[serde(default, rename = "majorVersion")]
    major_version: i64,
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

fn postgres_version(spec: &WireSpec, status: &WireStatus) -> String {
    if status.pg_data.major_version != 0 {
        return clipped(status.pg_data.major_version.to_string());
    }
    for candidate in [
        status.image.as_str(),
        status.pg_data.image.as_str(),
        spec.image_name.as_str(),
    ] {
        if !candidate.is_empty() {
            return clipped(candidate.to_string());
        }
    }
    String::new()
}

fn instances_of(kind: Kind, spec: &WireSpec, status: &WireStatus) -> i64 {
    match kind {
        Kind::Cluster if status.instances != 0 => status.instances,
        Kind::Cluster | Kind::Pooler => spec.instances,
        Kind::Backup | Kind::ScheduledBackup => 0,
    }
}

fn from_wire(kind: Kind, version: &str, wire: WireObject) -> Option<Resource> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    let instances = instances_of(kind, &wire.spec, &wire.status);
    let postgres_version = postgres_version(&wire.spec, &wire.status);
    Some(Resource {
        kind,
        version: version.to_string(),
        name: clipped(wire.metadata.name),
        namespace: clipped(wire.metadata.namespace),
        uid: clipped(wire.metadata.uid),
        instances,
        ready_instances: wire.status.ready_instances,
        primary: clipped(wire.status.current_primary),
        postgres_version,
        phase: clipped(wire.status.phase),
        superuser_secret: clipped(wire.spec.superuser_secret.name),
        cluster: clipped(wire.spec.cluster.name),
        schedule: clipped(wire.spec.schedule),
        pooler_type: clipped(wire.spec.pooler_type),
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
            what: kinds.first().map(|kind| kind.what()).unwrap_or("cnpg"),
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

/// List the four CNPG kinds. A missing group is invisible; a forbidden one is
/// Denied on every kind.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let sets = match fetch_group(
        client,
        &[
            Kind::Cluster,
            Kind::Backup,
            Kind::ScheduledBackup,
            Kind::Pooler,
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
        clusters: sets.next().unwrap_or_default(),
        backups: sets.next().unwrap_or_default(),
        scheduled_backups: sets.next().unwrap_or_default(),
        poolers: sets.next().unwrap_or_default(),
    })
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

fn instances_label(item: &Resource) -> String {
    match item.kind {
        Kind::Cluster => format!("{}/{}", item.ready_instances, item.instances),
        // PoolerStatus has no readyInstances upstream, so a Pooler states a
        // count and never a ready fraction.
        Kind::Pooler => item.instances.to_string(),
        Kind::Backup | Kind::ScheduledBackup => String::new(),
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
        "Instances",
        "Primary",
        "Version",
        "Secret",
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
                            instances_label(item),
                            item.primary.clone(),
                            item.postgres_version.clone(),
                            item.superuser_secret.clone(),
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
            "CloudNativePG is not served by this cluster".to_string(),
            String::new(),
            "this reads Cluster, Backup, ScheduledBackup and Pooler CRs the \
             operator already publishes; nothing is installed to find them, and \
             a Postgres password is never fetched"
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
        lines.push("no CloudNativePG objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no CloudNativePG object could be read here, though some are stored: every object \
             this account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!(
            "{} CloudNativePG {}",
            total,
            plural(total, "object")
        ));
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
            "{} CloudNativePG {} could not be decoded and {} not shown",
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
            if item.kind == Kind::Cluster {
                line.push_str(&format!(
                    "  {}/{} ready",
                    item.ready_instances, item.instances
                ));
            }
            if item.kind == Kind::Pooler {
                line.push_str(&format!(
                    "  {} {}",
                    item.instances,
                    plural(item.instances.max(0) as usize, "instance")
                ));
            }
            if !item.primary.is_empty() {
                line.push_str("  primary ");
                line.push_str(&item.primary);
            }
            if !item.postgres_version.is_empty() {
                line.push_str("  ");
                line.push_str(&item.postgres_version);
            }
            if !item.superuser_secret.is_empty() {
                line.push_str("  secret ");
                line.push_str(&item.superuser_secret);
            }
            if !item.cluster.is_empty() {
                line.push_str("  cluster ");
                line.push_str(&item.cluster);
            }
            if !item.schedule.is_empty() {
                line.push_str("  ");
                line.push_str(&item.schedule);
            }
            if !item.pooler_type.is_empty() {
                line.push_str("  ");
                line.push_str(&item.pooler_type);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "cnpg_test.rs"]
mod tests;
