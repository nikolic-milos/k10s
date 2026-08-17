//! External Secrets Operator inventory from the CRs the controller publishes.
//!
//! SecretStore, ClusterSecretStore, ExternalSecret, and ClusterExternalSecret
//! live on `external-secrets.io`. A cluster that does not serve the group
//! answers 404 and the inventory stays invisible, not broken; a 403 is
//! Denied. Nothing is installed to find them, and no provider is spoken.
//!
//! The Secret rule is structural. Parse keeps the target Secret name, the
//! store driver name, a refresh interval, the Ready condition, and the key
//! names `spec.data[].secretKey` declares. `spec.data[].remoteRef`,
//! `spec.dataFrom`, and `spec.provider.*.auth` never become fields, so they
//! cannot appear in Debug, a table cell, or an error string. The generated
//! Secret's data is never fetched.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;

pub const GROUP: &str = "external-secrets.io";

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;
const MAX_KEY_NAMES: usize = 32;

/// The four CRs this inventory reads. ESO serves more; those are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    SecretStore,
    ClusterSecretStore,
    ExternalSecret,
    ClusterExternalSecret,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::SecretStore => "SecretStore",
            Kind::ClusterSecretStore => "ClusterSecretStore",
            Kind::ExternalSecret => "ExternalSecret",
            Kind::ClusterExternalSecret => "ClusterExternalSecret",
        }
    }

    pub fn group(self) -> &'static str {
        GROUP
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::SecretStore => "secretstores",
            Kind::ClusterSecretStore => "clustersecretstores",
            Kind::ExternalSecret => "externalsecrets",
            Kind::ClusterExternalSecret => "clusterexternalsecrets",
        }
    }

    /// Preferred version when the group document names none.
    pub fn version(self) -> &'static str {
        "v1"
    }

    pub fn namespaced(self) -> bool {
        matches!(self, Kind::SecretStore | Kind::ExternalSecret)
    }

    pub fn is_store(self) -> bool {
        matches!(self, Kind::SecretStore | Kind::ClusterSecretStore)
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::SecretStore => "eso secretstores",
            Kind::ClusterSecretStore => "eso clustersecretstores",
            Kind::ExternalSecret => "eso externalsecrets",
            Kind::ClusterExternalSecret => "eso clusterexternalsecrets",
        }
    }
}

/// One ESO CR, reduced to what an inventory shows.
///
/// There is nowhere here for provider auth, `spec.data` remote references,
/// or the generated Secret's bytes. A token planted in those fields is
/// dropped at parse and cannot reach Debug or a table cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub version: String,
    pub name: String,
    /// Empty on a cluster-scoped store or ClusterExternalSecret.
    pub namespace: String,
    pub uid: String,
    /// Provider driver on a store (`vault`, `aws`, ...), or the referenced
    /// store kind on an ExternalSecret.
    pub store_type: String,
    pub refresh_interval: String,
    pub ready: String,
    /// Target Secret name only. Never its data.
    pub target_secret: String,
    /// Key names `spec.data` declares. `dataFrom` keys are unknowable
    /// without reading the generated Secret, so they are never listed.
    pub key_names: Vec<String>,
}

/// What one kind's list answered.
///
/// A 404 on the group is [`KindSet::NotServed`]: invisible, not broken. A 403
/// is [`KindSet::Denied`]. Those are different states on purpose.
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
    pub secret_stores: KindSet,
    pub cluster_secret_stores: KindSet,
    pub external_secrets: KindSet,
    pub cluster_external_secrets: KindSet,
}

impl Inventory {
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Kind); 4] {
        [
            (&self.secret_stores, Kind::SecretStore),
            (&self.cluster_secret_stores, Kind::ClusterSecretStore),
            (&self.external_secrets, Kind::ExternalSecret),
            (&self.cluster_external_secrets, Kind::ClusterExternalSecret),
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
    items: Vec<Value>,
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

/// Only the fields an inventory may keep. `data[].remoteRef`, `dataFrom`,
/// and provider auth sit on the wire and are ignored by serde.
#[derive(Deserialize, Default)]
struct WireSpec {
    /// A duration string on ExternalSecret ("1h") but an integer number of
    /// seconds on SecretStore and ClusterSecretStore, so this stays a
    /// [`Value`]: typing it `String` made every store that set the field
    /// fail deserialization and vanish from the inventory.
    #[serde(default, rename = "refreshInterval")]
    refresh_interval: Value,
    #[serde(default, rename = "secretStoreRef")]
    secret_store_ref: WireRef,
    #[serde(default)]
    target: WireTarget,
    #[serde(default)]
    provider: Value,
    #[serde(default)]
    data: Vec<WireDataEntry>,
    #[serde(default, rename = "externalSecretSpec")]
    external_secret_spec: WireNested,
}

#[derive(Deserialize, Default)]
struct WireNested {
    #[serde(default, rename = "refreshInterval")]
    refresh_interval: Value,
    #[serde(default, rename = "secretStoreRef")]
    secret_store_ref: WireRef,
    #[serde(default)]
    target: WireTarget,
    #[serde(default)]
    data: Vec<WireDataEntry>,
}

#[derive(Deserialize, Default)]
struct WireRef {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireTarget {
    #[serde(default)]
    name: String,
}

/// One `spec.data` entry, reduced to the key name it puts in the target
/// Secret. `remoteRef` stays on the wire.
#[derive(Deserialize, Default)]
struct WireDataEntry {
    #[serde(default, rename = "secretKey")]
    secret_key: String,
}

#[derive(Deserialize, Default)]
struct WireStatus {
    #[serde(default)]
    conditions: Vec<WireCondition>,
    #[serde(default)]
    binding: WireBinding,
}

#[derive(Deserialize, Default)]
struct WireBinding {
    #[serde(default)]
    name: String,
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

fn driver_of(provider: &Value) -> String {
    provider
        .as_object()
        .and_then(|map| map.keys().next())
        .cloned()
        .map(clipped)
        .unwrap_or_default()
}

fn store_type_of(kind: Kind, spec: &WireSpec) -> String {
    if kind.is_store() {
        return driver_of(&spec.provider);
    }
    let reference = if kind == Kind::ClusterExternalSecret {
        &spec.external_secret_spec.secret_store_ref
    } else {
        &spec.secret_store_ref
    };
    if reference.kind.is_empty() {
        clipped(reference.name.clone())
    } else if reference.name.is_empty() {
        clipped(reference.kind.clone())
    } else {
        clipped(format!("{}/{}", reference.kind, reference.name))
    }
}

fn refresh_of(kind: Kind, spec: &WireSpec) -> String {
    let value = if kind == Kind::ClusterExternalSecret {
        &spec.external_secret_spec.refresh_interval
    } else {
        &spec.refresh_interval
    };
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(seconds) => format!("{seconds}s"),
        _ => String::new(),
    };
    clipped(text)
}

fn target_of(kind: Kind, spec: &WireSpec, status: &WireStatus) -> String {
    if kind.is_store() {
        return String::new();
    }
    let named = if kind == Kind::ClusterExternalSecret {
        spec.external_secret_spec.target.name.as_str()
    } else {
        spec.target.name.as_str()
    };
    if !named.is_empty() {
        return clipped(named.to_string());
    }
    clipped(status.binding.name.clone())
}

/// Key names from `spec.data[].secretKey`, the one place ESO declares them:
/// no ESO version publishes key names in status, and `dataFrom` keys cannot
/// be known without reading the generated Secret.
fn key_names_of(kind: Kind, spec: &WireSpec) -> Vec<String> {
    let entries = if kind == Kind::ClusterExternalSecret {
        &spec.external_secret_spec.data
    } else {
        &spec.data
    };
    let mut names = Vec::new();
    for entry in entries {
        if entry.secret_key.is_empty() {
            continue;
        }
        let text = clipped(entry.secret_key.clone());
        if names.iter().any(|have| have == &text) {
            continue;
        }
        if names.len() == MAX_KEY_NAMES {
            break;
        }
        names.push(text);
    }
    names
}

fn from_wire(kind: Kind, version: &str, wire: WireObject) -> Option<Resource> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    Some(Resource {
        kind,
        version: version.to_string(),
        name: clipped(wire.metadata.name),
        namespace: if kind.namespaced() {
            clipped(wire.metadata.namespace)
        } else {
            String::new()
        },
        uid: clipped(wire.metadata.uid),
        store_type: store_type_of(kind, &wire.spec),
        refresh_interval: refresh_of(kind, &wire.spec),
        ready: ready_of(&wire.status.conditions),
        target_secret: target_of(kind, &wire.spec, &wire.status),
        key_names: key_names_of(kind, &wire.spec),
    })
}

fn parse_item(kind: Kind, version: &str, value: Value) -> Option<Resource> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_wire(kind, version, wire)
}

fn collect_items(
    kind: Kind,
    version: &str,
    values: impl IntoIterator<Item = Value>,
) -> (Vec<Resource>, bool, usize) {
    let mut items = Vec::new();
    let mut unreadable = 0usize;
    let mut truncated = false;
    for value in values {
        if items.len() == MAX_OBJECTS {
            truncated = true;
            break;
        }
        match parse_item(kind, version, value) {
            Some(resource) => items.push(resource),
            None => unreadable += 1,
        }
    }
    (items, truncated, unreadable)
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
    for fallback in [kind.version(), "v1beta1"] {
        if !out.iter().any(|have| have == fallback) {
            out.push(fallback.to_string());
        }
    }
    out
}

fn collection_url(kind: Kind, version: &str, namespace: Option<&str>) -> String {
    let mut path = format!("/apis/{}/{version}", kind.group());
    if kind.namespaced()
        && let Some(namespace) = namespace
    {
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
        let (page_items, page_truncated, page_unreadable) =
            collect_items(kind, version, page.items);
        unreadable += page_unreadable;
        for resource in page_items {
            if items.len() == MAX_OBJECTS {
                truncated = true;
                break;
            }
            items.push(resource);
        }
        truncated |= page_truncated;
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

/// List ESO CRs. A missing group is invisible; a forbidden one is Denied
/// on every kind and does not look like absence.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let versions = match probe_group(client, GROUP).await {
        GroupAnswer::NotServed => return Fetched::Ok(Inventory::default()),
        GroupAnswer::Denied => {
            return Fetched::Ok(Inventory {
                secret_stores: KindSet::Denied,
                cluster_secret_stores: KindSet::Denied,
                external_secrets: KindSet::Denied,
                cluster_external_secrets: KindSet::Denied,
            });
        }
        GroupAnswer::Failed(why) => {
            return Fetched::Failed {
                what: "external secrets",
                why,
            };
        }
        GroupAnswer::Served(versions) => versions,
    };
    let secret_stores = match list_kind(client, Kind::SecretStore, &versions, namespace).await {
        Ok(set) => set,
        Err(failed) => return failed,
    };
    let cluster_secret_stores =
        match list_kind(client, Kind::ClusterSecretStore, &versions, namespace).await {
            Ok(set) => set,
            Err(failed) => return failed,
        };
    let external_secrets = match list_kind(client, Kind::ExternalSecret, &versions, namespace).await
    {
        Ok(set) => set,
        Err(failed) => return failed,
    };
    let cluster_external_secrets =
        match list_kind(client, Kind::ClusterExternalSecret, &versions, namespace).await {
            Ok(set) => set,
            Err(failed) => return failed,
        };
    Fetched::Ok(Inventory {
        secret_stores,
        cluster_secret_stores,
        external_secrets,
        cluster_external_secrets,
    })
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

fn keys_label(keys: &[String]) -> String {
    clipped(keys.join(", "))
}

fn object_label(item: &Resource) -> String {
    if item.namespace.is_empty() {
        item.name.clone()
    } else {
        format!("{}/{}", item.namespace, item.name)
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
        "Ready",
        "Store",
        "Refresh",
        "Target",
        "Keys",
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
                            ready_label(&item.ready),
                            item.store_type.clone(),
                            item.refresh_interval.clone(),
                            item.target_secret.clone(),
                            keys_label(&item.key_names),
                        ],
                        name: item.name.clone(),
                        namespace: if item.namespace.is_empty() {
                            None
                        } else {
                            Some(item.namespace.clone())
                        },
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
            "External Secrets Operator is not served by this cluster".to_string(),
            String::new(),
            "this reads SecretStore, ClusterSecretStore, ExternalSecret and \
             ClusterExternalSecret CRs the controller already publishes; \
             nothing is installed to find them, and generated Secret data is \
             never fetched"
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
        lines.push("no External Secrets objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no External Secrets object could be read here, though some are stored: every \
             object this account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!(
            "{} External Secrets {}",
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
            "{} External Secrets {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            lines.push(object_label(item));
            let mut line = format!("  {}  {}", item.kind.as_str(), ready_label(&item.ready));
            if !item.store_type.is_empty() {
                line.push_str("  ");
                line.push_str(&item.store_type);
            }
            if !item.refresh_interval.is_empty() {
                line.push_str("  ");
                line.push_str(&item.refresh_interval);
            }
            if !item.target_secret.is_empty() {
                line.push_str("  secret ");
                line.push_str(&item.target_secret);
            }
            let keys = keys_label(&item.key_names);
            if !keys.is_empty() {
                line.push_str("  keys ");
                line.push_str(&keys);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "eso_test.rs"]
mod tests;
