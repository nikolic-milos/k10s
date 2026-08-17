//! Vault and OpenBao inventory from the Secrets Operator CRs.
//!
//! HashiCorp Vault Secrets Operator still publishes VaultConnection,
//! VaultAuth, VaultStaticSecret, VaultDynamicSecret, and VaultPKISecret on
//! `secrets.hashicorp.com/v1beta1` (that beta is the current documented
//! API, not a leftover). The OpenBao Secrets Operator (archived) was a VSO
//! fork that served the same kinds on the same `secrets.hashicorp.com`
//! group, so one group probe covers both vendors; current OpenBao docs point
//! at External Secrets Operator instead ([`crate::eso`]). OpenBao labels and
//! annotations on the HashiCorp kinds keep a leftover install attributed.
//!
//! Parse keeps the connection address, the auth method type, the secret path,
//! the refresh cadence (`refreshAfter`, or a PKI `expiryOffset`), and Ready. Tokens, kube-auth JWTs, header maps,
//! ciphertext, and static secret bytes are dropped at that boundary, so they
//! cannot appear in Debug, a table cell, or an error string. The generated
//! Kubernetes Secret is never fetched. The Vault HTTP API is not spoken here:
//! a query would need [`crate::reach::Bound`] plus a token the user named,
//! and this inventory does not scrape a Service for one.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;

pub const HASHICORP_GROUP: &str = "secrets.hashicorp.com";

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;

const ALL_KINDS: [Kind; 5] = [
    Kind::VaultConnection,
    Kind::VaultAuth,
    Kind::VaultStaticSecret,
    Kind::VaultDynamicSecret,
    Kind::VaultPKISecret,
];

/// The five CRs this inventory reads. HCPVaultSecretsApp is not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    VaultConnection,
    VaultAuth,
    VaultStaticSecret,
    VaultDynamicSecret,
    VaultPKISecret,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::VaultConnection => "VaultConnection",
            Kind::VaultAuth => "VaultAuth",
            Kind::VaultStaticSecret => "VaultStaticSecret",
            Kind::VaultDynamicSecret => "VaultDynamicSecret",
            Kind::VaultPKISecret => "VaultPKISecret",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::VaultConnection => "vaultconnections",
            Kind::VaultAuth => "vaultauths",
            Kind::VaultStaticSecret => "vaultstaticsecrets",
            Kind::VaultDynamicSecret => "vaultdynamicsecrets",
            Kind::VaultPKISecret => "vaultpkisecrets",
        }
    }

    /// The version we try when the group document names none.
    pub fn version(self) -> &'static str {
        "v1beta1"
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::VaultConnection => "vault connections",
            Kind::VaultAuth => "vault auths",
            Kind::VaultStaticSecret => "vault static secrets",
            Kind::VaultDynamicSecret => "vault dynamic secrets",
            Kind::VaultPKISecret => "vault pki secrets",
        }
    }
}

/// One Secrets Operator CR, reduced to what an inventory shows.
///
/// There is nowhere here for a token, a JWT, ciphertext, or secret bytes.
/// Adding any of those is a decision about secret exposure, not a convenience.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    /// The API group this object was listed from.
    pub group: String,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    /// Connection address. Never a token.
    pub address: String,
    /// Auth method type (`kubernetes`, `jwt`, `appRole`, ...).
    pub auth_method: String,
    /// Mount plus path, or a PKI role. Secret bytes are not this field.
    pub secret_path: String,
    pub refresh: String,
    pub ready: String,
    /// True when the object carries an OpenBao label or annotation.
    pub openbao: bool,
}

/// What one kind's list answered.
///
/// A 404 is [`KindSet::NotServed`]: invisible, not broken. A 403 is
/// [`KindSet::Denied`]. Those are different states on purpose.
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
    pub connections: KindSet,
    pub auths: KindSet,
    pub static_secrets: KindSet,
    pub dynamic_secrets: KindSet,
    pub pki_secrets: KindSet,
}

impl Inventory {
    /// False when `secrets.hashicorp.com` answered 404.
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Kind); 5] {
        [
            (&self.connections, Kind::VaultConnection),
            (&self.auths, Kind::VaultAuth),
            (&self.static_secrets, Kind::VaultStaticSecret),
            (&self.dynamic_secrets, Kind::VaultDynamicSecret),
            (&self.pki_secrets, Kind::VaultPKISecret),
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
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

/// Only the fields an inventory may keep. `token`, `jwt`, `headers`, `data`,
/// and `ciphertext` sit on the wire and are ignored by serde.
#[derive(Deserialize, Default)]
struct WireSpec {
    #[serde(default)]
    address: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    mount: String,
    #[serde(default)]
    role: String,
    #[serde(default, rename = "refreshAfter")]
    refresh_after: String,
    #[serde(default, rename = "expiryOffset")]
    expiry_offset: String,
}

#[derive(Deserialize, Default)]
struct WireStatus {
    #[serde(default)]
    valid: Value,
    #[serde(default)]
    conditions: Vec<WireCondition>,
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

fn mentions_openbao(map: &std::collections::BTreeMap<String, String>) -> bool {
    map.iter().any(|(key, value)| {
        key.to_ascii_lowercase().contains("openbao")
            || value.to_ascii_lowercase().contains("openbao")
    })
}

fn openbao_of(meta: &WireMeta) -> bool {
    mentions_openbao(&meta.labels) || mentions_openbao(&meta.annotations)
}

fn ready_of(status: &WireStatus) -> String {
    if let Some(condition) = status
        .conditions
        .iter()
        .find(|condition| condition.type_name == "Ready")
    {
        return clipped(condition.status.clone());
    }
    match &status.valid {
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => clipped(text.clone()),
        _ => String::new(),
    }
}

fn secret_path_of(spec: &WireSpec) -> String {
    let leaf = if !spec.path.is_empty() {
        spec.path.as_str()
    } else {
        spec.role.as_str()
    };
    if leaf.is_empty() {
        return String::new();
    }
    if spec.mount.is_empty() {
        clipped(leaf.to_string())
    } else {
        clipped(format!("{}/{}", spec.mount, leaf))
    }
}

fn refresh_of(spec: &WireSpec) -> String {
    if !spec.refresh_after.is_empty() {
        clipped(spec.refresh_after.clone())
    } else {
        clipped(spec.expiry_offset.clone())
    }
}

fn from_wire(kind: Kind, group: &str, version: &str, wire: WireObject) -> Option<Resource> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    let secret_path = secret_path_of(&wire.spec);
    let refresh = refresh_of(&wire.spec);
    let openbao = openbao_of(&wire.metadata);
    let ready = ready_of(&wire.status);
    Some(Resource {
        kind,
        group: group.to_string(),
        version: version.to_string(),
        name: clipped(wire.metadata.name),
        namespace: clipped(wire.metadata.namespace),
        uid: clipped(wire.metadata.uid),
        address: clipped(wire.spec.address),
        auth_method: clipped(wire.spec.method),
        secret_path,
        refresh,
        ready,
        openbao,
    })
}

fn parse_item(kind: Kind, group: &str, version: &str, value: Value) -> Option<Resource> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_wire(kind, group, version, wire)
}

fn collect_items(
    kind: Kind,
    group: &str,
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
        match parse_item(kind, group, version, value) {
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
    for fallback in [kind.version(), "v1"] {
        if !out.iter().any(|have| have == fallback) {
            out.push(fallback.to_string());
        }
    }
    out
}

fn collection_url(group: &str, kind: Kind, version: &str, namespace: Option<&str>) -> String {
    let mut path = format!("/apis/{group}/{version}");
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
    group: &str,
    kind: Kind,
    version: &str,
    namespace: Option<&str>,
) -> Result<KindSet, ListErr> {
    let path = collection_url(group, kind, version, namespace);
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
            collect_items(kind, group, version, page.items);
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
    group: &str,
    kind: Kind,
    group_versions: &[String],
    namespace: Option<&str>,
) -> Result<KindSet, Fetched<Inventory>> {
    for version in versions_for(kind, group_versions) {
        match list_at_version(client, group, kind, &version, namespace).await {
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
    namespace: Option<&str>,
) -> Result<[KindSet; 5], Fetched<Inventory>> {
    match probe_group(client, group).await {
        GroupAnswer::NotServed => Ok(std::array::from_fn(|_| KindSet::NotServed)),
        GroupAnswer::Denied => Ok(std::array::from_fn(|_| KindSet::Denied)),
        GroupAnswer::Failed(why) => Err(Fetched::Failed { what: "vault", why }),
        GroupAnswer::Served(versions) => {
            let mut sets = std::array::from_fn(|_| KindSet::NotServed);
            for (index, kind) in ALL_KINDS.iter().enumerate() {
                sets[index] = list_kind(client, group, *kind, &versions, namespace).await?;
            }
            Ok(sets)
        }
    }
}

/// List Vault / OpenBao Secrets Operator CRs. Both operators served these
/// kinds on `secrets.hashicorp.com`, so one probe answers for both vendors.
/// A missing group is invisible; a forbidden one is Denied on every kind.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let sets = match fetch_group(client, HASHICORP_GROUP, namespace).await {
        Ok(sets) => sets,
        Err(failed) => return failed,
    };
    let mut sets = sets.into_iter();
    Fetched::Ok(Inventory {
        connections: sets.next().unwrap_or_default(),
        auths: sets.next().unwrap_or_default(),
        static_secrets: sets.next().unwrap_or_default(),
        dynamic_secrets: sets.next().unwrap_or_default(),
        pki_secrets: sets.next().unwrap_or_default(),
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

fn vendor_label(item: &Resource) -> &'static str {
    if item.openbao { "OpenBao" } else { "Vault" }
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
        "Vendor",
        "Address",
        "Auth",
        "Path",
        "Refresh",
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
                        format!(
                            "{}/{}/{}/{}",
                            item.group,
                            item.kind.as_str(),
                            item.namespace,
                            item.name
                        )
                    } else {
                        item.uid.clone()
                    };
                    rows.push(TableRow {
                        cells: vec![
                            item.kind.as_str().to_string(),
                            item.name.clone(),
                            item.namespace.clone(),
                            ready_label(&item.ready),
                            vendor_label(item).to_string(),
                            item.address.clone(),
                            item.auth_method.clone(),
                            item.secret_path.clone(),
                            item.refresh.clone(),
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
            "Vault and OpenBao Secrets Operator CRs are not served by this cluster".to_string(),
            String::new(),
            "this reads VaultConnection, VaultAuth, VaultStaticSecret, \
             VaultDynamicSecret and VaultPKISecret on secrets.hashicorp.com; \
             the archived OpenBao Secrets Operator served the same kinds on \
             that group. Nothing is installed to find them, and the Vault \
             HTTP API is not spoken"
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
        lines.push(
            "no Vault or OpenBao secrets-operator objects are stored in this cluster".to_string(),
        );
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no Vault or OpenBao object could be read here, though some are stored: every \
             object this account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!(
            "{} Vault/OpenBao {}",
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
            "{} Vault/OpenBao {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            lines.push(object_label(item));
            let mut line = format!(
                "  {}  {}  {}",
                item.kind.as_str(),
                vendor_label(item),
                ready_label(&item.ready)
            );
            if !item.address.is_empty() {
                line.push_str("  ");
                line.push_str(&item.address);
            }
            if !item.auth_method.is_empty() {
                line.push_str("  ");
                line.push_str(&item.auth_method);
            }
            if !item.secret_path.is_empty() {
                line.push_str("  ");
                line.push_str(&item.secret_path);
            }
            if !item.refresh.is_empty() {
                line.push_str("  ");
                line.push_str(&item.refresh);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "vault_test.rs"]
mod tests;
