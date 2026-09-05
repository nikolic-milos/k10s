//! Kargo inventory from the CRs the controller already publishes.
//!
//! Stage, Warehouse, Freight, and Project (when served) live on
//! `kargo.akuity.io`. A cluster that does not serve the group answers 404 and
//! the inventory is invisible, not broken; a 403 is Denied. Nothing is
//! installed to find them, and nothing is reimplemented: this is not a
//! promotion engine.
//!
//! Refresh is the merge-patch of `kargo.akuity.io/refresh` that Kargo already
//! honours on Warehouse, Stage, and Promotion (docs.kargo.io, Annotations and
//! Labels). The value is a unique RFC3339 timestamp, the same spelling Kargo's
//! own API writes. `confirm=false` is the first press and does not touch the
//! wire.
//!
//! Promotion is a Promotion CR the controller owns. Kargo does not document a
//! promote annotation or a spec patch that starts one, so this module does not
//! invent either.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use kube::Client;
use kube::api::{ListParams, Patch, PatchParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::{Fetched, classify};
use crate::served::{GroupAnswer, ListErr, after_group, after_list, group_url, order_versions};

/// Documented on Warehouse, Stage, and Promotion. A unique value (UUID or
/// "now") triggers reconciliation when it changes.
pub const REFRESH_ANNOTATION: &str = "kargo.akuity.io/refresh";

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;

const GROUP: &str = "kargo.akuity.io";

/// The four CRs this inventory reads. Kargo serves more; those are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stage,
    Warehouse,
    Freight,
    Project,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Stage => "Stage",
            Kind::Warehouse => "Warehouse",
            Kind::Freight => "Freight",
            Kind::Project => "Project",
        }
    }

    pub fn group(self) -> &'static str {
        GROUP
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::Stage => "stages",
            Kind::Warehouse => "warehouses",
            Kind::Freight => "freights",
            Kind::Project => "projects",
        }
    }

    pub fn version(self) -> &'static str {
        "v1alpha1"
    }

    pub fn namespaced(self) -> bool {
        !matches!(self, Kind::Project)
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::Stage => "kargo stages",
            Kind::Warehouse => "kargo warehouses",
            Kind::Freight => "kargo freight",
            Kind::Project => "kargo projects",
        }
    }

    /// Kargo honours `kargo.akuity.io/refresh` on Warehouse, Stage, and
    /// Promotion only; patching it onto Freight or Project would be accepted
    /// by the API server and ignored by Kargo, a false success.
    pub fn refreshable(self) -> bool {
        matches!(self, Kind::Stage | Kind::Warehouse)
    }
}

/// One CR, reduced to what an inventory shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub phase: String,
    pub health: String,
    pub freight: String,
    pub verified: String,
    pub warehouse: String,
}

/// What one kind's list answered.
///
/// A 404 on the group is [`KindSet::NotServed`]: invisible, not broken. A 403
/// is [`KindSet::Denied`]. Those are different states on purpose; collapsing
/// them would tell someone Kargo is absent when the account was refused.
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
    pub stages: KindSet,
    pub warehouses: KindSet,
    pub freight: KindSet,
    pub projects: KindSet,
}

impl Inventory {
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Kind); 4] {
        [
            (&self.stages, Kind::Stage),
            (&self.warehouses, Kind::Warehouse),
            (&self.freight, Kind::Freight),
            (&self.projects, Kind::Project),
        ]
    }
}

/// First press versus the refresh that actually went on the wire.
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
    /// Freight puts origin next to metadata, not under spec.
    #[serde(default)]
    origin: WireOrigin,
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
    #[serde(default, rename = "requestedFreight")]
    requested_freight: Vec<WireFreightRequest>,
    #[serde(default)]
    subscriptions: Vec<WireSubscription>,
}

#[derive(Deserialize, Default)]
struct WireFreightRequest {
    #[serde(default)]
    origin: WireOrigin,
}

#[derive(Deserialize, Default)]
struct WireOrigin {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireSubscription {
    #[serde(default)]
    git: WireRepo,
    #[serde(default)]
    image: WireRepo,
    #[serde(default)]
    chart: WireRepo,
}

#[derive(Deserialize, Default)]
struct WireRepo {
    #[serde(default, rename = "repoURL")]
    repo_url: String,
}

#[derive(Deserialize, Default)]
struct WireStatus {
    #[serde(default)]
    phase: String,
    #[serde(default)]
    conditions: Vec<WireCondition>,
    #[serde(default)]
    health: WireHealth,
    #[serde(default, rename = "freightSummary")]
    freight_summary: String,
    #[serde(default, rename = "freightHistory")]
    freight_history: Vec<WireFreightCollection>,
    #[serde(default, rename = "lastFreightID")]
    last_freight_id: String,
    #[serde(default, rename = "verifiedIn")]
    verified_in: BTreeMap<String, Value>,
    #[serde(default, rename = "currentPromotion")]
    current_promotion: WirePromotionRef,
    #[serde(default, rename = "lastPromotion")]
    last_promotion: WirePromotionRef,
}

#[derive(Deserialize, Default)]
struct WireCondition {
    #[serde(default, rename = "type")]
    type_name: String,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize, Default)]
struct WireHealth {
    #[serde(default)]
    status: String,
}

#[derive(Deserialize, Default)]
struct WireFreightCollection {
    #[serde(default)]
    items: BTreeMap<String, WireFreightRef>,
    #[serde(default, rename = "verificationHistory")]
    verification_history: Vec<WireVerification>,
}

#[derive(Deserialize, Default)]
struct WireFreightRef {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireVerification {
    #[serde(default)]
    phase: String,
}

#[derive(Deserialize, Default)]
struct WirePromotionRef {
    /// Required upstream, so an empty name means the reference is absent.
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: WirePromotionStatus,
}

#[derive(Deserialize, Default)]
struct WirePromotionStatus {
    #[serde(default)]
    phase: String,
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

fn format_origin(origin: &WireOrigin) -> String {
    match (origin.kind.as_str(), origin.name.as_str()) {
        ("", "") => String::new(),
        (kind, "") => kind.to_string(),
        ("", name) => name.to_string(),
        (kind, name) => format!("{kind}/{name}"),
    }
}

fn condition_of(conditions: &[WireCondition], type_name: &str) -> String {
    conditions
        .iter()
        .find(|condition| condition.type_name == type_name)
        .map(|condition| clipped(condition.status.clone()))
        .unwrap_or_default()
}

fn phase_of(kind: Kind, status: &WireStatus) -> String {
    if !status.phase.is_empty() {
        return clipped(status.phase.clone());
    }
    if matches!(kind, Kind::Stage) {
        // currentPromotion is the promotion running now; lastPromotion is the
        // one that already finished, so it must not speak while one runs.
        if !status.current_promotion.status.phase.is_empty() {
            return clipped(status.current_promotion.status.phase.clone());
        }
        if !status.current_promotion.name.is_empty() {
            return "Running".to_string();
        }
        if !status.last_promotion.status.phase.is_empty() {
            return clipped(status.last_promotion.status.phase.clone());
        }
    }
    condition_of(&status.conditions, "Ready")
}

fn health_of(status: &WireStatus) -> String {
    if !status.health.status.is_empty() {
        return clipped(status.health.status.clone());
    }
    condition_of(&status.conditions, "Healthy")
}

fn freight_of(kind: Kind, status: &WireStatus) -> String {
    match kind {
        Kind::Stage if !status.freight_summary.is_empty() => {
            clipped(status.freight_summary.clone())
        }
        Kind::Stage => {
            let Some(current) = status.freight_history.first() else {
                return String::new();
            };
            let names: Vec<String> = current
                .items
                .values()
                .filter(|item| !item.name.is_empty())
                .map(|item| item.name.clone())
                .collect();
            join_clipped(&names)
        }
        Kind::Warehouse => clipped(status.last_freight_id.clone()),
        Kind::Freight | Kind::Project => String::new(),
    }
}

fn verified_of(kind: Kind, status: &WireStatus) -> String {
    match kind {
        Kind::Stage => status
            .freight_history
            .first()
            .and_then(|current| current.verification_history.first())
            .map(|item| clipped(item.phase.clone()))
            .unwrap_or_default(),
        Kind::Freight => {
            let stages: Vec<String> = status.verified_in.keys().cloned().collect();
            join_clipped(&stages)
        }
        Kind::Warehouse | Kind::Project => String::new(),
    }
}

fn warehouse_of(kind: Kind, spec: &WireSpec, origin: &WireOrigin) -> String {
    match kind {
        Kind::Stage => {
            let names: Vec<String> = spec
                .requested_freight
                .iter()
                .map(|item| format_origin(&item.origin))
                .filter(|item| !item.is_empty())
                .collect();
            join_clipped(&names)
        }
        Kind::Warehouse => {
            let urls: Vec<String> = spec
                .subscriptions
                .iter()
                .flat_map(|item| {
                    [
                        item.git.repo_url.as_str(),
                        item.image.repo_url.as_str(),
                        item.chart.repo_url.as_str(),
                    ]
                })
                .filter(|url| !url.is_empty())
                .map(|url| url.to_string())
                .collect();
            join_clipped(&urls)
        }
        Kind::Freight => clipped(format_origin(origin)),
        Kind::Project => String::new(),
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
        phase: phase_of(kind, &wire.status),
        health: health_of(&wire.status),
        freight: freight_of(kind, &wire.status),
        verified: verified_of(kind, &wire.status),
        warehouse: warehouse_of(kind, &wire.spec, &wire.origin),
    })
}

fn parse_item(kind: Kind, version: &str, value: Value) -> Option<Resource> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_wire(kind, version, wire)
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
    if kind.namespaced() {
        if let Some(namespace) = namespace {
            path.push_str("/namespaces/");
            path.push_str(namespace);
        }
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
    let namespace = if target.kind.namespaced() {
        Some(target.namespace.as_str())
    } else {
        None
    };
    collection_url(target.kind, version, namespace)
}

/// RFC3339 UTC from a Unix timestamp. Kargo accepts any unique string; this
/// is the value its own refresh API writes.
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

fn refresh_patch(at: &str) -> Patch<Value> {
    Patch::Merge(serde_json::json!({
        "metadata": {
            "annotations": {
                REFRESH_ANNOTATION: at
            }
        }
    }))
}

fn refresh_request(target: &Resource, at: &str) -> Result<http::Request<Vec<u8>>, String> {
    Request::new(object_collection_url(target))
        .patch(&target.name, &PatchParams::default(), &refresh_patch(at))
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
            what: kinds.first().map(|kind| kind.what()).unwrap_or("kargo"),
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

/// List Stage, Warehouse, Freight, and Project if the group serves them. A
/// missing group is invisible; a forbidden one is Denied on every kind.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let sets = match fetch_group(
        client,
        &[Kind::Stage, Kind::Warehouse, Kind::Freight, Kind::Project],
        namespace,
    )
    .await
    {
        Ok(sets) => sets,
        Err(failed) => return failed,
    };
    let mut sets = sets.into_iter();
    Fetched::Ok(Inventory {
        stages: sets.next().unwrap_or_default(),
        warehouses: sets.next().unwrap_or_default(),
        freight: sets.next().unwrap_or_default(),
        projects: sets.next().unwrap_or_default(),
    })
}

/// Merge-patch `kargo.akuity.io/refresh` to now, RFC3339. Kargo already
/// honours this; there is no other refresh API here.
pub async fn refresh(client: &Client, target: &Resource, confirm: bool) -> Fetched<Confirm> {
    refresh_at(client, target, confirm, &rfc3339_now()).await
}

async fn refresh_at(
    client: &Client,
    target: &Resource,
    confirm: bool,
    at: &str,
) -> Fetched<Confirm> {
    if !target.kind.refreshable() {
        return Fetched::Failed {
            what: target.kind.what(),
            why: format!(
                "Kargo does not honour {REFRESH_ANNOTATION} on this kind; only \
                 Warehouse, Stage, and Promotion reconcile on it"
            ),
        };
    }
    if !confirm {
        return Fetched::Ok(Confirm::Needed);
    }
    let request = match refresh_request(target, at) {
        Ok(request) => request,
        Err(why) => {
            return Fetched::Failed {
                what: target.kind.what(),
                why,
            };
        }
    };
    match client.request::<Value>(request).await {
        Ok(_) => Fetched::Ok(Confirm::Sent),
        Err(error) => classify(target.kind.what(), &error),
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
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
        "Health",
        "Freight",
        "Verified",
        "Warehouse",
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
                            item.health.clone(),
                            item.freight.clone(),
                            item.verified.clone(),
                            item.warehouse.clone(),
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
            "Kargo is not served by this cluster".to_string(),
            String::new(),
            "this reads Stage, Warehouse, Freight and Project CRs the controller \
             already publishes; nothing is installed to find them, and a refresh \
             is the kargo.akuity.io/refresh annotation Kargo already honours"
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
        lines.push("no Kargo objects are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 && denied == 0 {
        lines.push(
            "no Kargo object could be read here, though some are stored: every object this \
             account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!("{} Kargo {}", total, plural(total, "object")));
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
            "{} Kargo {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "object"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            let identity = if item.namespace.is_empty() {
                item.name.clone()
            } else {
                format!("{}/{}", item.namespace, item.name)
            };
            lines.push(identity);
            let mut line = format!("  {}", item.kind.as_str());
            if !item.phase.is_empty() {
                line.push_str("  ");
                line.push_str(&item.phase);
            }
            if !item.health.is_empty() {
                line.push_str("  ");
                line.push_str(&item.health);
            }
            if !item.freight.is_empty() {
                line.push_str("  freight ");
                line.push_str(&item.freight);
            }
            if !item.verified.is_empty() {
                line.push_str("  verified ");
                line.push_str(&item.verified);
            }
            if !item.warehouse.is_empty() {
                line.push_str("  ");
                line.push_str(&item.warehouse);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "kargo_test.rs"]
mod tests;
