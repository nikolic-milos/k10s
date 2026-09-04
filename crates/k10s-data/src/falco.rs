//! Falco inventory from what the cluster already runs.
//!
//! k10s never installs Falco or Falcosidekick. Discovery is labels and
//! well-known names on Services and DaemonSets, the same way [`crate::reach`]
//! fingerprints a tool that is already there. Operator CRs are listed only
//! when the Falco Operator groups answer. Current docs serve
//! `instance.falcosecurity.dev/v1alpha1` (`Falco`, `Component`) and
//! `artifact.falcosecurity.dev/v1alpha1` (`Rulesfile`, `Plugin`, `Config`).
//! Older charts and sidekicks still use `falco.org` / `events.falco.org`;
//! those are probed after the official groups. A 404 is
//! [`GroupState::NotServed`] and that group is skipped, a 403 is
//! [`GroupState::Denied`]. Rule ConfigMaps contribute metadata, data key
//! names, and a count of `- rule:` entries. The condition, output, and Lua
//! stay on the server: a planted token in a rule output string must not
//! survive on [`Inventory`]. Falco's gRPC outputs API is
//! [`Outputs::Unbound`]; speaking it would need a new dependency. A log
//! chunk the shell already fetched can be parsed into [`FalcoEvent`] values
//! here, so log follow is reused rather than started from this module.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::{ConfigMap, PodSpec, Service};
use kube::Client;
use kube::api::{Api, ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;

const PAGE_LIMIT: u32 = 200;
const MAX_SCAN: usize = 2_000;
const MAX_RULE_REFS: usize = 32;
const MAX_CM_KEYS: usize = 32;
const MAX_RULE_SCAN_BYTES: usize = 1 << 20;
const FALLBACK_VERSION: &str = "v1alpha1";

/// Ceiling on one list page. A bigger body is refused, not truncated.
pub const MAX_PAGE_BYTES: usize = 8 << 20;
/// Ceiling on one already-fetched log chunk this parser will walk.
pub const MAX_LOG_BYTES: usize = 1 << 20;
pub const MAX_FIELD_CHARS: usize = 200;
pub const MAX_OBJECTS: usize = 2_000;
pub const MAX_WORKLOADS: usize = 200;
pub const MAX_RULE_MAPS: usize = 200;
pub const MAX_EVENTS: usize = 256;

/// Groups we probe. Official operator groups first; older chart names after.
/// Missing ones are skipped.
pub const GROUPS: &[&str] = &[
    "instance.falcosecurity.dev",
    "artifact.falcosecurity.dev",
    "falcosecurity.dev",
    "falco.org",
    "instance.falco.org",
    "artifact.falco.org",
    "events.falco.org",
    "events.falcosecurity.dev",
];

/// Why [`Outputs::Unbound`] is the only outputs answer this crate can give.
pub const OUTPUTS_UNBOUND: &str =
    "Falco's gRPC outputs API is not scraped; speaking it would need a new dependency";

const FALLBACK_KINDS: &[(&str, ListedKind)] = &[
    ("falcos", ListedKind::Resource(CrKind::Falco)),
    ("components", ListedKind::Resource(CrKind::Component)),
    (
        "falcosidekicks",
        ListedKind::Resource(CrKind::Falcosidekick),
    ),
    ("falcotools", ListedKind::Resource(CrKind::FalcoTool)),
    ("plugins", ListedKind::Resource(CrKind::FalcoTool)),
    ("falcorules", ListedKind::Resource(CrKind::FalcoRules)),
    ("rulesfiles", ListedKind::Resource(CrKind::FalcoRules)),
    ("configs", ListedKind::Resource(CrKind::FalcoConfig)),
    ("falcoevents", ListedKind::Event),
];

/// One operator API group after the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupState {
    Served,
    NotServed,
    Denied,
}

impl GroupState {
    pub fn served(&self) -> bool {
        !matches!(self, GroupState::NotServed)
    }
}

/// Which well-known Falco process a Service or DaemonSet is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    Falco,
    Falcosidekick,
}

impl WorkloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadKind::Falco => "Falco",
            WorkloadKind::Falcosidekick => "Falcosidekick",
        }
    }
}

/// Where the fingerprint matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadSource {
    Service,
    DaemonSet,
}

impl WorkloadSource {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadSource::Service => "Service",
            WorkloadSource::DaemonSet => "DaemonSet",
        }
    }
}

/// A Service or DaemonSet that already looks like Falco.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    pub kind: WorkloadKind,
    pub source: WorkloadSource,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    /// Image from a DaemonSet pod template. Empty on a Service.
    pub image: String,
}

/// Workload discovery. [`Workloads::Absent`] is a successful look that
/// matched nothing. [`Workloads::Denied`] is a 403 on the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workloads {
    Found {
        items: Vec<Workload>,
        truncated: bool,
    },
    Absent,
    Denied,
}

impl Workloads {
    pub fn items(&self) -> &[Workload] {
        match self {
            Workloads::Found { items, .. } => items,
            Workloads::Absent | Workloads::Denied => &[],
        }
    }

    pub fn found(&self) -> bool {
        matches!(self, Workloads::Found { .. })
    }
}

/// Operator CR kinds this inventory keeps. The group document may spell
/// FalcoRules as `Rulesfile`; that still lands here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrKind {
    Falco,
    Component,
    Falcosidekick,
    FalcoTool,
    FalcoRules,
    /// `artifact.falcosecurity.dev` `Config`. Its `spec.config` is an
    /// inline configuration fragment that can carry output URLs with
    /// embedded tokens; it must never become a field on [`Resource`].
    FalcoConfig,
}

impl CrKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CrKind::Falco => "Falco",
            CrKind::Component => "Component",
            CrKind::Falcosidekick => "Falcosidekick",
            CrKind::FalcoTool => "FalcoTool",
            CrKind::FalcoRules => "FalcoRules",
            CrKind::FalcoConfig => "Config",
        }
    }
}

/// One operator CR, reduced to identity, image/version, Ready, and rule
/// file names. Inline rule text is not a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: CrKind,
    /// Kind as the version document named it (`Rulesfile`, `Falco`, ...).
    pub kind_name: String,
    pub group: String,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub image: String,
    /// The kind's readiness condition as `Type=Status` (`Available=True`,
    /// `Programmed=False`, legacy `Ready=True`). Empty when none exists.
    pub ready: String,
    pub rules_refs: Vec<String>,
}

/// A rules ConfigMap: metadata, data key names, and how many `- rule:`
/// entries the values contained. Values themselves are not held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMap {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub keys: Vec<String>,
    pub rule_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleMaps {
    Found {
        items: Vec<RuleMap>,
        truncated: bool,
    },
    Absent,
    Denied,
}

impl RuleMaps {
    pub fn items(&self) -> &[RuleMap] {
        match self {
            RuleMaps::Found { items, .. } => items,
            RuleMaps::Absent | RuleMaps::Denied => &[],
        }
    }
}

/// One Falco alert, from a CR or from a log chunk. Time, priority, rule,
/// namespace, pod. Not the formatted `output` string and not syscall args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FalcoEvent {
    pub time: String,
    pub priority: String,
    pub rule: String,
    pub namespace: String,
    pub pod: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSet {
    Served {
        items: Vec<FalcoEvent>,
        truncated: bool,
    },
    NotServed,
    Denied,
}

impl EventSet {
    pub fn items(&self) -> &[FalcoEvent] {
        match self {
            EventSet::Served { items, .. } => items,
            EventSet::NotServed | EventSet::Denied => &[],
        }
    }

    pub fn served(&self) -> bool {
        !matches!(self, EventSet::NotServed)
    }
}

/// Falco's live outputs API. Always unbound here: the gRPC Outputs service
/// is not spoken without a new crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outputs {
    Unbound { why: String },
}

/// What a fetch held. [`Inventory::present`] is false when every Falco
/// group was 404 and no workload or rules ConfigMap was found; that is
/// when [`table_page`] returns `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    pub groups: Vec<(String, GroupState)>,
    pub workloads: Workloads,
    pub resources: Vec<Resource>,
    /// Collections under a served group that answered 403, as
    /// `(group, plural)`. An empty plural is a 403 on the group's own
    /// resource-kind discovery. A denied list is not an empty list.
    pub denied_kinds: Vec<(String, String)>,
    pub rule_maps: RuleMaps,
    pub events: EventSet,
    pub outputs: Outputs,
    pub truncated: bool,
}

impl Default for Inventory {
    fn default() -> Inventory {
        Inventory {
            groups: GROUPS
                .iter()
                .map(|group| ((*group).to_string(), GroupState::NotServed))
                .collect(),
            workloads: Workloads::Absent,
            resources: Vec::new(),
            denied_kinds: Vec::new(),
            rule_maps: RuleMaps::Absent,
            events: EventSet::NotServed,
            outputs: Outputs::Unbound {
                why: OUTPUTS_UNBOUND.to_string(),
            },
            truncated: false,
        }
    }
}

impl Inventory {
    /// True when any probed Falco group answered something other than 404.
    pub fn served(&self) -> bool {
        self.groups.iter().any(|(_, state)| state.served())
    }

    /// True when a UI should open a Falco pane. A 403 on Services or
    /// ConfigMaps alone does not count: that is not a Falco group, and it
    /// is not a found workload.
    pub fn present(&self) -> bool {
        self.served()
            || self.workloads.found()
            || matches!(self.rule_maps, RuleMaps::Found { .. })
            || self.events.served()
    }
}

#[derive(Clone, Copy)]
enum ListedKind {
    Resource(CrKind),
    Event,
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

#[derive(Deserialize, Default)]
struct WireResourceList {
    #[serde(default)]
    resources: Vec<WireResource>,
}

#[derive(Deserialize, Default)]
struct WireResource {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    namespaced: bool,
}

#[derive(Deserialize, Default)]
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

enum CoreOutcome<T> {
    Ok { items: Vec<T>, truncated: bool },
    Denied,
    Failed(String),
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_FIELD_CHARS) {
        Some((at, _)) => {
            let mut cut = text[..at].to_string();
            cut.push('\u{2026}');
            cut
        }
        None => text.to_string(),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
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

fn after_core(error: &kube::Error) -> CoreOutcome<()> {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return CoreOutcome::Denied;
        }
        if response.code == 404 {
            return CoreOutcome::Ok {
                items: Vec::new(),
                truncated: false,
            };
        }
    }
    CoreOutcome::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

/// Any token carrying `falco`, minus CrowdStrike Falcon.
fn looks_falco_token(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("falcon") && !lower.contains("falco-") && !lower.contains("falcos") {
        return false;
    }
    lower.contains("falco")
}

fn labels_match(labels: Option<&BTreeMap<String, String>>) -> bool {
    let Some(labels) = labels else {
        return false;
    };
    labels
        .iter()
        .any(|(key, value)| looks_falco_token(key) || looks_falco_token(value))
}

fn labels_say_sidekick(labels: Option<&BTreeMap<String, String>>) -> bool {
    let Some(labels) = labels else {
        return false;
    };
    labels.iter().any(|(key, value)| {
        let key = key.to_ascii_lowercase();
        let value = value.to_ascii_lowercase();
        key.contains("falcosidekick") || value.contains("falcosidekick")
    })
}

fn workload_kind(name: &str, labels: Option<&BTreeMap<String, String>>) -> Option<WorkloadKind> {
    let lower = name.to_ascii_lowercase();
    if lower == "falcosidekick" || lower.contains("falcosidekick") {
        return Some(WorkloadKind::Falcosidekick);
    }
    if lower == "falco"
        || lower.contains("falco-")
        || lower.ends_with("-falco")
        || (lower.starts_with("falco") && !lower.starts_with("falcon"))
    {
        return Some(WorkloadKind::Falco);
    }
    if !labels_match(labels) {
        return None;
    }
    if labels_say_sidekick(labels) {
        Some(WorkloadKind::Falcosidekick)
    } else {
        Some(WorkloadKind::Falco)
    }
}

fn image_from_pod(spec: Option<&PodSpec>, prefer: &[&str]) -> String {
    let Some(spec) = spec else {
        return String::new();
    };
    for want in prefer {
        if let Some(container) = spec
            .containers
            .iter()
            .find(|container| container.name.eq_ignore_ascii_case(want))
        {
            return clip(container.image.as_deref().unwrap_or(""));
        }
    }
    spec.containers
        .first()
        .and_then(|container| container.image.as_deref())
        .map(clip)
        .unwrap_or_default()
}

/// Whether this Service is Falco or Falcosidekick.
pub fn match_service(svc: &Service) -> Option<Workload> {
    let name = svc.metadata.name.as_deref()?;
    let kind = workload_kind(name, svc.metadata.labels.as_ref())?;
    Some(Workload {
        kind,
        source: WorkloadSource::Service,
        name: clip(name),
        namespace: clip(svc.metadata.namespace.as_deref().unwrap_or("")),
        uid: clip(svc.metadata.uid.as_deref().unwrap_or("")),
        image: String::new(),
    })
}

/// Whether this DaemonSet is Falco or Falcosidekick.
pub fn match_daemon_set(ds: &DaemonSet) -> Option<Workload> {
    let name = ds.metadata.name.as_deref()?;
    let kind = workload_kind(name, ds.metadata.labels.as_ref())?;
    let prefer = match kind {
        WorkloadKind::Falco => &["falco"][..],
        WorkloadKind::Falcosidekick => &["falcosidekick"][..],
    };
    let image = image_from_pod(
        ds.spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref()),
        prefer,
    );
    Some(Workload {
        kind,
        source: WorkloadSource::DaemonSet,
        name: clip(name),
        namespace: clip(ds.metadata.namespace.as_deref().unwrap_or("")),
        uid: clip(ds.metadata.uid.as_deref().unwrap_or("")),
        image,
    })
}

fn rule_map_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("falco-rules")
        || lower == "falco-custom-rules"
        || (lower.contains("falco") && lower.contains("rule"))
}

fn rule_map_labels(labels: Option<&BTreeMap<String, String>>) -> bool {
    let Some(labels) = labels else {
        return false;
    };
    labels.iter().any(|(key, value)| {
        let key = key.to_ascii_lowercase();
        let value = value.to_ascii_lowercase();
        key == "falco-rules"
            || key.contains("falco-rules")
            || value == "falco-rules"
            || value.contains("falco-rules")
    })
}

/// Count `- rule:` keys in a Falco rules YAML fragment. The fragment is
/// not stored; only the count leaves.
pub fn count_rules(text: &str) -> usize {
    let scan = if text.len() > MAX_RULE_SCAN_BYTES {
        match text.get(..MAX_RULE_SCAN_BYTES) {
            Some(prefix) => prefix,
            None => {
                let mut end = MAX_RULE_SCAN_BYTES;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                &text[..end]
            }
        }
    } else {
        text
    };
    scan.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("- rule:") || trimmed.starts_with("- rule :")
        })
        .count()
}

/// Metadata plus key names plus a rule count. Data values are read and
/// dropped; they never become fields on the result.
pub fn match_rule_map(cm: &ConfigMap) -> Option<RuleMap> {
    let name = cm.metadata.name.as_deref()?;
    if !rule_map_name(name) && !rule_map_labels(cm.metadata.labels.as_ref()) {
        return None;
    }
    let mut keys = Vec::new();
    let mut rule_count = 0usize;
    if let Some(data) = &cm.data {
        for (key, value) in data {
            if keys.len() < MAX_CM_KEYS {
                keys.push(clip(key));
            }
            rule_count = rule_count.saturating_add(count_rules(value));
        }
    }
    Some(RuleMap {
        name: clip(name),
        namespace: clip(cm.metadata.namespace.as_deref().unwrap_or("")),
        uid: clip(cm.metadata.uid.as_deref().unwrap_or("")),
        keys,
        rule_count,
    })
}

fn classify_kind(kind: &str) -> Option<ListedKind> {
    if kind.eq_ignore_ascii_case("Falco") {
        return Some(ListedKind::Resource(CrKind::Falco));
    }
    if kind.eq_ignore_ascii_case("Component") {
        return Some(ListedKind::Resource(CrKind::Component));
    }
    if kind.eq_ignore_ascii_case("Falcosidekick") {
        return Some(ListedKind::Resource(CrKind::Falcosidekick));
    }
    if kind.eq_ignore_ascii_case("FalcoTool") || kind.eq_ignore_ascii_case("Plugin") {
        return Some(ListedKind::Resource(CrKind::FalcoTool));
    }
    if kind.eq_ignore_ascii_case("FalcoRules") || kind.eq_ignore_ascii_case("Rulesfile") {
        return Some(ListedKind::Resource(CrKind::FalcoRules));
    }
    if kind.eq_ignore_ascii_case("Config") {
        return Some(ListedKind::Resource(CrKind::FalcoConfig));
    }
    if kind.eq_ignore_ascii_case("FalcoEvent") || kind.eq_ignore_ascii_case("FalcoEvents") {
        return Some(ListedKind::Event);
    }
    None
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
    if out.is_empty() {
        out.push(FALLBACK_VERSION.to_string());
    }
    out
}

fn group_url(group: &str) -> String {
    format!("/apis/{group}")
}

/// `namespace` here is already gated on the resource's discovery
/// `namespaced` flag: a cluster-scoped kind stays at the cluster
/// collection even when the fetch is scoped.
fn collection_url(group: &str, version: &str, plural: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(namespace) => format!("/apis/{group}/{version}/namespaces/{namespace}/{plural}"),
        None => format!("/apis/{group}/{version}/{plural}"),
    }
}

/// The kind's own readiness condition, as `Type=Status`. The operator
/// publishes `Reconciled`/`Available` on instance kinds and
/// `Programmed`/`ResolvedRefs` on artifact kinds; `Ready` is only the
/// legacy chart shape and is read last.
fn ready_of(kind: CrKind, status: &Value) -> String {
    let wanted: &[&str] = match kind {
        CrKind::Falco | CrKind::Component | CrKind::Falcosidekick => {
            &["Available", "Reconciled", "Ready"]
        }
        CrKind::FalcoTool | CrKind::FalcoRules | CrKind::FalcoConfig => {
            &["Programmed", "ResolvedRefs", "Ready"]
        }
    };
    let Some(conditions) = status.get("conditions").and_then(Value::as_array) else {
        return String::new();
    };
    for want in wanted {
        for condition in conditions {
            if str_field(condition, "type") == *want {
                return clip(&format!("{want}={}", str_field(condition, "status")));
            }
        }
    }
    String::new()
}

fn join_image(repository: &str, tag: &str) -> String {
    if repository.is_empty() {
        return String::new();
    }
    if tag.is_empty() {
        return clip(repository);
    }
    clip(&format!("{repository}:{tag}"))
}

fn image_from_container_list(containers: Option<&Value>) -> String {
    let Some(containers) = containers.and_then(Value::as_array) else {
        return String::new();
    };
    for want in ["falco", "falcosidekick"] {
        for container in containers {
            if str_field(container, "name").eq_ignore_ascii_case(want) {
                let image = str_field(container, "image");
                if !image.is_empty() {
                    return clip(image);
                }
            }
        }
    }
    containers
        .first()
        .map(|container| clip(str_field(container, "image")))
        .unwrap_or_default()
}

fn image_of(spec: &Value, status: &Value) -> String {
    for text in [str_field(spec, "version"), str_field(status, "version")] {
        if !text.is_empty() {
            return clip(text);
        }
    }
    match spec.get("image") {
        Some(Value::String(image)) if !image.is_empty() => return clip(image),
        Some(Value::Object(map)) => {
            let repository = map.get("repository").and_then(Value::as_str).unwrap_or("");
            let tag = map.get("tag").and_then(Value::as_str).unwrap_or("");
            let joined = join_image(repository, tag);
            if !joined.is_empty() {
                return joined;
            }
        }
        _ => {}
    }
    if let Some(falco) = spec.get("falco") {
        let image = str_field(falco, "image");
        if !image.is_empty() {
            return clip(image);
        }
    }
    if let Some(oci) = spec.pointer("/ociArtifact/image") {
        let joined = join_image(str_field(oci, "repository"), str_field(oci, "tag"));
        if !joined.is_empty() {
            return joined;
        }
    }
    let pod = spec
        .pointer("/podTemplateSpec/spec")
        .or_else(|| spec.pointer("/template/spec"));
    if let Some(pod) = pod {
        let image = image_from_container_list(pod.get("containers"));
        if !image.is_empty() {
            return image;
        }
    }
    let status_image = str_field(status, "image");
    if !status_image.is_empty() {
        return clip(status_image);
    }
    String::new()
}

fn push_ref(out: &mut Vec<String>, text: &str) {
    if text.is_empty() || out.len() >= MAX_RULE_REFS {
        return;
    }
    if text.contains('\n') || text.contains("- rule:") {
        return;
    }
    let clipped = clip(text);
    if !out.iter().any(|have| have == &clipped) {
        out.push(clipped);
    }
}

fn push_ref_value(out: &mut Vec<String>, value: &Value) {
    match value {
        Value::String(text) => push_ref(out, text),
        Value::Object(_) => {
            let name = str_field(value, "name");
            if !name.is_empty() {
                push_ref(out, name);
            }
        }
        Value::Array(items) => {
            for item in items {
                if out.len() >= MAX_RULE_REFS {
                    break;
                }
                push_ref_value(out, item);
            }
        }
        _ => {}
    }
}

fn rules_refs_of(spec: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(reference) = spec.get("configMapRef") {
        match reference {
            Value::String(name) => push_ref(&mut out, name),
            other => push_ref(&mut out, str_field(other, "name")),
        }
    }
    for key in [
        "rulesFile",
        "rulesFiles",
        "rulesfiles",
        "rules",
        "rulesFileRefs",
    ] {
        if let Some(value) = spec.get(key) {
            push_ref_value(&mut out, value);
        }
    }
    if let Some(refs) = spec.pointer("/falcoctl/artifact/install/refs") {
        push_ref_value(&mut out, refs);
    }
    out
}

/// One operator CR. Inline rule YAML is ignored even when the object has it.
pub fn parse_resource(kind: CrKind, group: &str, version: &str, value: &Value) -> Option<Resource> {
    let meta = value.get("metadata")?;
    let name = str_field(meta, "name");
    if name.is_empty() {
        return None;
    }
    let spec = value.get("spec").unwrap_or(&Value::Null);
    let status = value.get("status").unwrap_or(&Value::Null);
    let kind_name = str_field(value, "kind");
    Some(Resource {
        kind,
        kind_name: clip(if kind_name.is_empty() {
            kind.as_str()
        } else {
            kind_name
        }),
        group: clip(group),
        version: clip(version),
        name: clip(name),
        namespace: clip(str_field(meta, "namespace")),
        uid: clip(str_field(meta, "uid")),
        image: image_of(spec, status),
        ready: ready_of(kind, status),
        rules_refs: rules_refs_of(spec),
    })
}

fn field_from_output(fields: &Value, keys: &[&str]) -> String {
    for key in keys {
        match fields.get(*key) {
            Some(Value::String(text)) if is_named(text) => return clip(text),
            Some(Value::Number(number)) => return clip(&number.to_string()),
            _ => {}
        }
    }
    String::new()
}

/// Falco writes `<NA>` for a field it could not resolve, so an event off a host
/// process carries the placeholder rather than nothing. Treating it as text
/// would pin a rule on a pod called `<NA>`; an unresolved field is absent.
fn is_named(text: &str) -> bool {
    !text.is_empty() && text != "<NA>"
}

fn priority_of(value: &Value) -> String {
    match value.get("priority") {
        Some(Value::String(text)) => clip(text),
        Some(Value::Number(number)) => {
            if let Some(n) = number.as_u64() {
                const NAMES: &[&str] = &[
                    "Emergency",
                    "Alert",
                    "Critical",
                    "Error",
                    "Warning",
                    "Notice",
                    "Informational",
                    "Debug",
                ];
                if let Some(name) = NAMES.get(n as usize) {
                    return (*name).to_string();
                }
            }
            clip(&number.to_string())
        }
        _ => String::new(),
    }
}

fn event_from_fields(value: &Value) -> Option<FalcoEvent> {
    let rule = str_field(value, "rule");
    if rule.is_empty() {
        return None;
    }
    let fields = value.get("output_fields").unwrap_or(&Value::Null);
    let namespace = field_from_output(
        fields,
        &["k8s.ns.name", "ka.target.namespace", "k8s.ns", "namespace"],
    );
    let namespace = if namespace.is_empty() {
        clip(str_field(value, "namespace"))
    } else {
        namespace
    };
    let pod = field_from_output(
        fields,
        &["k8s.pod.name", "ka.target.pod.name", "k8s.pod", "pod"],
    );
    let pod = if pod.is_empty() {
        clip(str_field(value, "pod"))
    } else {
        pod
    };
    Some(FalcoEvent {
        time: clip(str_field(value, "time")),
        priority: priority_of(value),
        rule: clip(rule),
        namespace,
        pod,
    })
}

/// One Falco JSON object. `output` and syscall-arg fields are not copied.
pub fn parse_event(value: &Value) -> Option<FalcoEvent> {
    if let Some(event) = event_from_fields(value) {
        return Some(event);
    }
    if let Some(spec) = value.get("spec") {
        if let Some(mut event) = event_from_fields(spec) {
            let meta = value.get("metadata").unwrap_or(&Value::Null);
            if event.namespace.is_empty() {
                event.namespace = clip(str_field(meta, "namespace"));
            }
            if event.time.is_empty() {
                event.time = clip(str_field(meta, "creationTimestamp"));
            }
            if event.pod.is_empty() {
                event.pod = clip(str_field(spec, "pod"));
            }
            return Some(event);
        }
    }
    None
}

fn json_payload(line: &str) -> &str {
    let trimmed = line.trim();
    match trimmed.find('{') {
        Some(at) => &trimmed[at..],
        None => trimmed,
    }
}

fn bounded_chunk(chunk: &str) -> &str {
    if chunk.len() <= MAX_LOG_BYTES {
        return chunk;
    }
    match chunk.get(..MAX_LOG_BYTES) {
        Some(prefix) => prefix,
        None => {
            let mut end = MAX_LOG_BYTES;
            while end > 0 && !chunk.is_char_boundary(end) {
                end -= 1;
            }
            &chunk[..end]
        }
    }
}

fn push_event(into: &mut Vec<FalcoEvent>, value: &Value) -> bool {
    if into.len() >= MAX_EVENTS {
        return false;
    }
    if let Some(event) = parse_event(value) {
        into.push(event);
    }
    into.len() < MAX_EVENTS
}

/// Parse an already-fetched Falco log chunk. The shell owns follow; this
/// only turns JSON lines (or one JSON array) into [`FalcoEvent`] values.
/// Host paths and command lines in `output` / `output_fields` are not kept.
pub fn parse_log_chunk(chunk: &str) -> Vec<FalcoEvent> {
    let chunk = bounded_chunk(chunk);
    let mut events = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(chunk) {
        match value {
            Value::Array(items) => {
                for item in items {
                    if !push_event(&mut events, &item) {
                        break;
                    }
                }
                return events;
            }
            Value::Object(_) => {
                let _ = push_event(&mut events, &value);
                return events;
            }
            _ => {}
        }
    }
    for line in chunk.lines() {
        if events.len() >= MAX_EVENTS {
            break;
        }
        let payload = json_payload(line);
        if !payload.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        let _ = push_event(&mut events, &value);
    }
    events
}

enum PageError {
    TooLarge,
    NotJson,
}

fn parse_list(text: &str) -> Result<WireList, PageError> {
    if text.len() > MAX_PAGE_BYTES {
        return Err(PageError::TooLarge);
    }
    serde_json::from_str(text).map_err(|_| PageError::NotJson)
}

async fn probe_group(client: &Client, group: &str) -> GroupAnswer {
    let request = match http::Request::get(group_url(group)).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    match client.request::<WireGroup>(request).await {
        Ok(doc) => GroupAnswer::Served(order_versions(
            &doc.preferred.version,
            doc.versions.into_iter().map(|item| item.version).collect(),
        )),
        Err(error) => after_group(&error),
    }
}

async fn resource_list(
    client: &Client,
    group: &str,
    version: &str,
) -> Result<Vec<(String, ListedKind, bool)>, ListErr> {
    let path = format!("/apis/{group}/{version}");
    let request = match http::Request::get(&path).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return Err(ListErr::Failed(error.to_string())),
    };
    match client.request::<WireResourceList>(request).await {
        Ok(doc) => {
            let mut out = Vec::new();
            for resource in doc.resources {
                if resource.name.contains('/') {
                    continue;
                }
                if let Some(listed) = classify_kind(&resource.kind) {
                    if !out.iter().any(|(name, _, _)| name == &resource.name) {
                        out.push((resource.name, listed, resource.namespaced));
                    }
                }
            }
            Ok(out)
        }
        Err(error) => Err(after_list(&error)),
    }
}

async fn list_plural(
    client: &Client,
    group: &str,
    version: &str,
    plural: &str,
    namespace: Option<&str>,
) -> Result<(WireList, bool), ListErr> {
    let path = collection_url(group, version, plural, namespace);
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
            Err(error) => return Err(ListErr::Failed(error.to_string())),
        };
        let text = match client.request_text(request).await {
            Ok(text) => text,
            Err(error) if items.is_empty() => return Err(after_list(&error)),
            Err(error) => {
                return Err(ListErr::Failed(crate::connect::describe(
                    &error as &(dyn std::error::Error + 'static),
                )));
            }
        };
        let page = match parse_list(&text) {
            Ok(page) => page,
            Err(PageError::TooLarge) => {
                return Err(ListErr::Failed(
                    "the list page is larger than 8 MiB; the page is not shown".to_string(),
                ));
            }
            Err(PageError::NotJson) => {
                return Err(ListErr::Failed("the list is not JSON".to_string()));
            }
        };
        items.extend(page.items);
        token = (!page.metadata.cont.is_empty()).then_some(page.metadata.cont);
        if items.len() >= MAX_OBJECTS.max(MAX_EVENTS) {
            truncated = token.is_some() || items.len() > MAX_OBJECTS.max(MAX_EVENTS);
            break;
        }
        if token.is_none() {
            break;
        }
    }
    Ok((
        WireList {
            metadata: WireListMeta::default(),
            items,
        },
        truncated,
    ))
}

fn take_resources(
    kind: CrKind,
    kind_name: &str,
    group: &str,
    version: &str,
    items: Vec<Value>,
    into: &mut Vec<Resource>,
    truncated: &mut bool,
) {
    for value in items {
        if into.len() >= MAX_OBJECTS {
            *truncated = true;
            break;
        }
        let mut parsed = match parse_resource(kind, group, version, &value) {
            Some(parsed) => parsed,
            None => continue,
        };
        if parsed.kind_name.is_empty() {
            parsed.kind_name = clip(kind_name);
        }
        into.push(parsed);
    }
}

fn take_events(items: Vec<Value>, events: &mut EventSet, truncated: &mut bool) {
    let (mut held, mut event_cap) = match events {
        EventSet::Served {
            items,
            truncated: was,
        } => (std::mem::take(items), *was),
        EventSet::Denied | EventSet::NotServed => (Vec::new(), false),
    };
    for value in items {
        if held.len() >= MAX_EVENTS {
            event_cap = true;
            break;
        }
        if let Some(event) = parse_event(&value) {
            held.push(event);
        }
    }
    *truncated |= event_cap;
    *events = EventSet::Served {
        items: held,
        truncated: event_cap,
    };
}

#[expect(clippy::too_many_arguments)]
async fn list_named(
    client: &Client,
    group: &str,
    versions: &[String],
    plural: &str,
    listed: ListedKind,
    namespace: Option<&str>,
    resources: &mut Vec<Resource>,
    events: &mut EventSet,
    denied: &mut Vec<(String, String)>,
    truncated: &mut bool,
) -> Result<(), Fetched<Inventory>> {
    for version in versions {
        match list_plural(client, group, version, plural, namespace).await {
            Ok((page, page_truncated)) => {
                *truncated |= page_truncated;
                match listed {
                    ListedKind::Resource(kind) => take_resources(
                        kind,
                        kind.as_str(),
                        group,
                        version,
                        page.items,
                        resources,
                        truncated,
                    ),
                    ListedKind::Event => take_events(page.items, events, truncated),
                }
                return Ok(());
            }
            Err(ListErr::NotFound) => continue,
            Err(ListErr::Denied) => {
                match listed {
                    // The group answered but this collection did not: a
                    // denied kind is recorded, never shown as zero objects.
                    ListedKind::Resource(_) => {
                        let entry = (group.to_string(), plural.to_string());
                        if !denied.contains(&entry) {
                            denied.push(entry);
                        }
                    }
                    ListedKind::Event => {
                        if matches!(events, EventSet::NotServed) {
                            *events = EventSet::Denied;
                        }
                    }
                }
                return Ok(());
            }
            Err(ListErr::Failed(why)) => {
                return Err(Fetched::Failed { what: "falco", why });
            }
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
async fn list_group(
    client: &Client,
    group: &str,
    versions: &[String],
    namespace: Option<&str>,
    resources: &mut Vec<Resource>,
    events: &mut EventSet,
    denied: &mut Vec<(String, String)>,
    truncated: &mut bool,
) -> Result<(), Fetched<Inventory>> {
    let mut named = Vec::new();
    let mut saw_list = false;
    for version in versions {
        match resource_list(client, group, version).await {
            Ok(found) => {
                saw_list = true;
                for (plural, listed, namespaced) in found {
                    if !named.iter().any(|(have, _, _)| have == &plural) {
                        named.push((plural, listed, namespaced));
                    }
                }
                break;
            }
            Err(ListErr::NotFound) => continue,
            // A 403 on the group's own resource-kind discovery: the empty
            // plural marks that nothing under it could even be named.
            Err(ListErr::Denied) => {
                let entry = (group.to_string(), String::new());
                if !denied.contains(&entry) {
                    denied.push(entry);
                }
                return Ok(());
            }
            Err(ListErr::Failed(why)) => {
                return Err(Fetched::Failed { what: "falco", why });
            }
        }
    }
    if !saw_list {
        // Every known Falco CRD is Namespaced (verified against the
        // falco-operator chart's CRDs), so the fallback plurals are probed
        // at the namespaced path when the fetch is scoped; a cluster-scoped
        // variant would answer 404 there, the same skip as a missing kind.
        named.extend(
            FALLBACK_KINDS
                .iter()
                .map(|(plural, listed)| ((*plural).to_string(), *listed, true)),
        );
    }
    for (plural, listed, namespaced) in named {
        list_named(
            client,
            group,
            versions,
            &plural,
            listed,
            namespace.filter(|_| namespaced),
            resources,
            events,
            denied,
            truncated,
        )
        .await?;
        if resources.len() >= MAX_OBJECTS {
            *truncated = true;
            break;
        }
    }
    Ok(())
}

async fn list_services(client: &Client, namespace: Option<&str>) -> CoreOutcome<Workload> {
    let api: Api<Service> = match namespace {
        Some(namespace) => Api::namespaced(client.clone(), namespace),
        None => Api::all(client.clone()),
    };
    let mut items = Vec::new();
    let mut token: Option<String> = None;
    let mut scanned = 0usize;
    let mut truncated = false;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = token.as_deref() {
            params = params.continue_token(token);
        }
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(error) => {
                return match after_core(&error) {
                    CoreOutcome::Denied => CoreOutcome::Denied,
                    CoreOutcome::Failed(why) => CoreOutcome::Failed(why),
                    CoreOutcome::Ok { .. } => CoreOutcome::Ok { items, truncated },
                };
            }
        };
        for svc in page.items {
            scanned += 1;
            if scanned > MAX_SCAN {
                truncated = true;
                break;
            }
            if let Some(hit) = match_service(&svc) {
                if items.len() >= MAX_WORKLOADS {
                    truncated = true;
                    break;
                }
                items.push(hit);
            }
        }
        token = page.metadata.continue_.filter(|s| !s.is_empty());
        if token.is_none() || scanned > MAX_SCAN || items.len() >= MAX_WORKLOADS {
            break;
        }
    }
    CoreOutcome::Ok { items, truncated }
}

async fn list_daemon_sets(client: &Client, namespace: Option<&str>) -> CoreOutcome<Workload> {
    let api: Api<DaemonSet> = match namespace {
        Some(namespace) => Api::namespaced(client.clone(), namespace),
        None => Api::all(client.clone()),
    };
    let mut items = Vec::new();
    let mut token: Option<String> = None;
    let mut scanned = 0usize;
    let mut truncated = false;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = token.as_deref() {
            params = params.continue_token(token);
        }
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(error) => {
                return match after_core(&error) {
                    CoreOutcome::Denied => CoreOutcome::Denied,
                    CoreOutcome::Failed(why) => CoreOutcome::Failed(why),
                    CoreOutcome::Ok { .. } => CoreOutcome::Ok { items, truncated },
                };
            }
        };
        for ds in page.items {
            scanned += 1;
            if scanned > MAX_SCAN {
                truncated = true;
                break;
            }
            if let Some(hit) = match_daemon_set(&ds) {
                if items.len() >= MAX_WORKLOADS {
                    truncated = true;
                    break;
                }
                items.push(hit);
            }
        }
        token = page.metadata.continue_.filter(|s| !s.is_empty());
        if token.is_none() || scanned > MAX_SCAN || items.len() >= MAX_WORKLOADS {
            break;
        }
    }
    CoreOutcome::Ok { items, truncated }
}

fn finish_workloads(mut items: Vec<Workload>, truncated: bool) -> Workloads {
    if items.is_empty() {
        return Workloads::Absent;
    }
    items.sort_by(|a, b| {
        (
            a.kind.as_str(),
            a.source.as_str(),
            a.namespace.as_str(),
            a.name.as_str(),
        )
            .cmp(&(
                b.kind.as_str(),
                b.source.as_str(),
                b.namespace.as_str(),
                b.name.as_str(),
            ))
    });
    Workloads::Found { items, truncated }
}

async fn discover_workloads(client: &Client, namespace: Option<&str>) -> Result<Workloads, String> {
    let services = list_services(client, namespace).await;
    let daemonsets = list_daemon_sets(client, namespace).await;
    match (services, daemonsets) {
        (CoreOutcome::Failed(why), _) | (_, CoreOutcome::Failed(why)) => Err(why),
        (CoreOutcome::Denied, CoreOutcome::Denied) => Ok(Workloads::Denied),
        (
            CoreOutcome::Ok {
                items: a,
                truncated: t1,
            },
            CoreOutcome::Ok {
                items: b,
                truncated: t2,
            },
        ) => {
            let mut items = a;
            items.extend(b);
            Ok(finish_workloads(items, t1 || t2))
        }
        (CoreOutcome::Ok { items, truncated }, CoreOutcome::Denied)
        | (CoreOutcome::Denied, CoreOutcome::Ok { items, truncated }) => {
            if items.is_empty() {
                Ok(Workloads::Denied)
            } else {
                Ok(finish_workloads(items, truncated))
            }
        }
    }
}

/// The sweep is metadata-only -- names and labels decide the match, so the
/// cluster's ConfigMap bodies never cross the wire. Only a matched map gets
/// its body fetched, for key names and a rule count.
async fn list_rule_maps(client: &Client, namespace: Option<&str>) -> Result<RuleMaps, String> {
    let api: Api<ConfigMap> = match namespace {
        Some(namespace) => Api::namespaced(client.clone(), namespace),
        None => Api::all(client.clone()),
    };
    let mut matched: Vec<(String, String)> = Vec::new();
    let mut token: Option<String> = None;
    let mut scanned = 0usize;
    let mut truncated = false;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = token.as_deref() {
            params = params.continue_token(token);
        }
        let page = match api.list_metadata(&params).await {
            Ok(page) => page,
            Err(error) => {
                return match after_core(&error) {
                    CoreOutcome::Denied => Ok(RuleMaps::Denied),
                    CoreOutcome::Failed(why) => Err(why),
                    CoreOutcome::Ok { .. } => Ok(RuleMaps::Absent),
                };
            }
        };
        for meta in page.items {
            scanned += 1;
            if scanned > MAX_SCAN {
                truncated = true;
                break;
            }
            let name = meta.metadata.name.as_deref().unwrap_or("");
            let namespace = meta.metadata.namespace.as_deref().unwrap_or("");
            if name.is_empty() || namespace.is_empty() {
                continue;
            }
            if !rule_map_name(name) && !rule_map_labels(meta.metadata.labels.as_ref()) {
                continue;
            }
            if matched.len() >= MAX_RULE_MAPS {
                truncated = true;
                break;
            }
            matched.push((namespace.to_string(), name.to_string()));
        }
        token = page.metadata.continue_.filter(|s| !s.is_empty());
        if token.is_none() || scanned > MAX_SCAN || matched.len() >= MAX_RULE_MAPS {
            break;
        }
    }
    let mut items = Vec::new();
    for (namespace, name) in matched {
        let api: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
        match api.get(&name).await {
            Ok(cm) => {
                if let Some(hit) = match_rule_map(&cm) {
                    items.push(hit);
                }
            }
            Err(error) => match after_core(&error) {
                // A map deleted between the sweep and the body fetch is
                // absent now; the others still count.
                CoreOutcome::Ok { .. } => {}
                CoreOutcome::Denied => return Ok(RuleMaps::Denied),
                CoreOutcome::Failed(why) => return Err(why),
            },
        }
    }
    if items.is_empty() {
        return Ok(RuleMaps::Absent);
    }
    items.sort_by(|a, b| {
        (a.namespace.as_str(), a.name.as_str()).cmp(&(b.namespace.as_str(), b.name.as_str()))
    });
    Ok(RuleMaps::Found { items, truncated })
}

/// Probe Falco groups, list the CRs they name, and inventory Services,
/// DaemonSets, and rules ConfigMaps that are already there. Nothing is
/// installed. The gRPC outputs API is not contacted. `Some(namespace)`
/// scopes every list to that namespace; a kind the group's discovery
/// document marks cluster-scoped stays at the cluster collection.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let mut groups = Vec::with_capacity(GROUPS.len());
    let mut resources = Vec::new();
    let mut denied_kinds = Vec::new();
    let mut events = EventSet::NotServed;
    let mut truncated = false;

    for group in GROUPS {
        match probe_group(client, group).await {
            GroupAnswer::NotServed => {
                groups.push(((*group).to_string(), GroupState::NotServed));
            }
            GroupAnswer::Denied => {
                groups.push(((*group).to_string(), GroupState::Denied));
            }
            GroupAnswer::Failed(why) => {
                return Fetched::Failed { what: "falco", why };
            }
            GroupAnswer::Served(versions) => {
                groups.push(((*group).to_string(), GroupState::Served));
                if let Err(failed) = list_group(
                    client,
                    group,
                    &versions,
                    namespace,
                    &mut resources,
                    &mut events,
                    &mut denied_kinds,
                    &mut truncated,
                )
                .await
                {
                    return failed;
                }
            }
        }
    }

    let workloads = match discover_workloads(client, namespace).await {
        Ok(workloads) => workloads,
        Err(why) => {
            return Fetched::Failed {
                what: "falco workloads",
                why,
            };
        }
    };
    let rule_maps = match list_rule_maps(client, namespace).await {
        Ok(rule_maps) => rule_maps,
        Err(why) => {
            return Fetched::Failed {
                what: "falco rules",
                why,
            };
        }
    };
    if let Workloads::Found { truncated: cap, .. } = &workloads {
        truncated |= *cap;
    }
    if let RuleMaps::Found { truncated: cap, .. } = &rule_maps {
        truncated |= *cap;
    }
    if let EventSet::Served { truncated: cap, .. } = &events {
        truncated |= *cap;
    }

    Fetched::Ok(Inventory {
        groups,
        workloads,
        resources,
        denied_kinds,
        rule_maps,
        events,
        outputs: Outputs::Unbound {
            why: OUTPUTS_UNBOUND.to_string(),
        },
        truncated,
    })
}

fn ready_label(ready: &str) -> String {
    if ready.is_empty() {
        return "no readiness condition".to_string();
    }
    ready.to_string()
}

fn has_outputs_endpoint(inventory: &Inventory) -> bool {
    inventory.workloads.found()
        || inventory
            .resources
            .iter()
            .any(|resource| matches!(resource.kind, CrKind::Falco | CrKind::Falcosidekick))
}

fn row(kind: &str, name: &str, namespace: &str, ready: &str, detail: &str, uid: &str) -> TableRow {
    TableRow {
        cells: vec![
            kind.to_string(),
            name.to_string(),
            namespace.to_string(),
            ready.to_string(),
            detail.to_string(),
        ],
        name: name.to_string(),
        namespace: if namespace.is_empty() {
            None
        } else {
            Some(namespace.to_string())
        },
        uid: uid.to_string(),
    }
}

/// Native list rows. `None` when no Falco group is served and no Falco
/// workload or rules ConfigMap was found, so a UI stays invisible rather
/// than opening an empty pane.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.present() {
        return None;
    }
    let columns = ["Kind", "Name", "Namespace", "Ready", "Detail"]
        .iter()
        .map(|name| TableColumn {
            name: name.to_string(),
            wide: false,
        })
        .collect();
    let mut rows = Vec::new();
    let mut truncated = inventory.truncated;

    for (group, state) in &inventory.groups {
        if matches!(state, GroupState::Denied) {
            rows.push(row(
                group,
                "",
                "",
                "access denied for this account",
                "",
                &format!("denied:{group}"),
            ));
        }
    }

    for (group, plural) in &inventory.denied_kinds {
        let detail = if plural.is_empty() {
            "the group's resource kinds could not be listed"
        } else {
            ""
        };
        rows.push(row(
            group,
            plural,
            "",
            "access denied for this account",
            detail,
            &format!("denied:{group}/{plural}"),
        ));
    }

    for item in inventory.workloads.items() {
        let uid = if item.uid.is_empty() {
            format!(
                "{}/{}/{}/{}",
                item.source.as_str(),
                item.kind.as_str(),
                item.namespace,
                item.name
            )
        } else {
            item.uid.clone()
        };
        rows.push(row(
            item.kind.as_str(),
            &item.name,
            &item.namespace,
            item.source.as_str(),
            &item.image,
            &uid,
        ));
    }

    for item in &inventory.resources {
        let uid = if item.uid.is_empty() {
            format!("{}/{}/{}", item.kind.as_str(), item.namespace, item.name)
        } else {
            item.uid.clone()
        };
        let mut detail = item.image.clone();
        if !item.rules_refs.is_empty() {
            if !detail.is_empty() {
                detail.push(' ');
            }
            detail.push_str(&item.rules_refs.join(","));
        }
        rows.push(row(
            item.kind.as_str(),
            &item.name,
            &item.namespace,
            &ready_label(&item.ready),
            &detail,
            &uid,
        ));
    }

    for item in inventory.rule_maps.items() {
        let uid = if item.uid.is_empty() {
            format!("rules/{}/{}", item.namespace, item.name)
        } else {
            item.uid.clone()
        };
        rows.push(row(
            "RuleMap",
            &item.name,
            &item.namespace,
            &format!("{} rules", item.rule_count),
            &item.keys.join(","),
            &uid,
        ));
    }

    match &inventory.events {
        EventSet::Denied => {
            rows.push(row(
                "Event",
                "",
                "",
                "access denied for this account",
                "",
                "denied:falco-events",
            ));
        }
        EventSet::Served {
            items,
            truncated: cap,
        } => {
            truncated |= *cap;
            for item in items {
                let uid = format!(
                    "event/{}/{}/{}/{}",
                    item.time, item.rule, item.namespace, item.pod
                );
                rows.push(row(
                    "Event",
                    &item.rule,
                    &item.namespace,
                    &item.priority,
                    &item.pod,
                    &clip(&uid),
                ));
            }
        }
        EventSet::NotServed => {}
    }

    if has_outputs_endpoint(inventory) {
        let Outputs::Unbound { why } = &inventory.outputs;
        rows.push(row(
            "Outputs",
            "",
            "",
            "Unbound",
            why,
            "falco-outputs-unbound",
        ));
    }

    Some(TablePage {
        columns,
        rows,
        truncated,
        continue_token: None,
    })
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    format!("{word}s")
}

/// The inventory as a document. Rule bodies and syscall args are not
/// rendered; that is what makes a planted token in a rule output testable.
pub fn render(inventory: &Inventory) -> Vec<String> {
    if !inventory.present() {
        return vec![
            "Falco is not in this cluster".to_string(),
            String::new(),
            "this reads Services, DaemonSets, rules ConfigMaps, and operator CRs \
             the cluster already has; nothing is installed to find them, so a \
             cluster without Falco shows as empty here"
                .to_string(),
        ];
    }

    let mut lines = Vec::new();
    let workloads = inventory.workloads.items().len();
    let resources = inventory.resources.len();
    let maps = inventory.rule_maps.items().len();
    let events = inventory.events.items().len();

    let denied_somewhere = !inventory.denied_kinds.is_empty()
        || matches!(inventory.workloads, Workloads::Denied)
        || matches!(inventory.rule_maps, RuleMaps::Denied)
        || matches!(inventory.events, EventSet::Denied)
        || inventory
            .groups
            .iter()
            .any(|(_, state)| matches!(state, GroupState::Denied));

    if workloads == 0 && resources == 0 && maps == 0 && events == 0 {
        // A denied list is not an empty one: zero objects can only be
        // claimed when nothing was refused.
        if denied_somewhere {
            lines.push(
                "whether Falco objects are stored is unknown: part of the \
                 listing was denied for this account"
                    .to_string(),
            );
        } else {
            lines.push("no Falco objects are stored in this cluster".to_string());
        }
    } else {
        let mut head = Vec::new();
        if workloads > 0 {
            head.push(format!(
                "{workloads} Falco {}",
                plural(workloads, "workload")
            ));
        }
        if resources > 0 {
            head.push(format!("{resources} Falco {}", plural(resources, "object")));
        }
        if maps > 0 {
            head.push(format!("{maps} rules {}", plural(maps, "ConfigMap")));
        }
        if events > 0 {
            head.push(format!("{events} Falco {}", plural(events, "event")));
        }
        lines.push(head.join(", "));
    }

    for (group, state) in &inventory.groups {
        if matches!(state, GroupState::Denied) {
            lines.push(format!("{group}: access denied for this account"));
        }
    }
    for (group, plural) in &inventory.denied_kinds {
        if plural.is_empty() {
            lines.push(format!(
                "{group}: resource discovery denied for this account"
            ));
        } else {
            lines.push(format!("{group}/{plural}: access denied for this account"));
        }
    }
    if matches!(inventory.events, EventSet::Denied) {
        lines.push("falco events: access denied for this account".to_string());
    }
    if inventory.truncated {
        lines.push(
            "the listing stopped at a cap, so this is some of them rather than all".to_string(),
        );
    }
    if has_outputs_endpoint(inventory) {
        let Outputs::Unbound { why } = &inventory.outputs;
        lines.push(why.clone());
    }

    for item in inventory.workloads.items() {
        lines.push(String::new());
        let mut line = format!(
            "{}/{}  {}  {}",
            item.namespace,
            item.name,
            item.kind.as_str(),
            item.source.as_str()
        );
        if !item.image.is_empty() {
            line.push_str("  ");
            line.push_str(&item.image);
        }
        lines.push(line);
    }
    for item in &inventory.resources {
        lines.push(String::new());
        let mut line = format!(
            "{}/{}  {}  {}",
            item.namespace,
            item.name,
            item.kind.as_str(),
            ready_label(&item.ready)
        );
        if !item.image.is_empty() {
            line.push_str("  ");
            line.push_str(&item.image);
        }
        if !item.rules_refs.is_empty() {
            line.push_str("  rules ");
            line.push_str(&item.rules_refs.join(","));
        }
        lines.push(line);
    }
    for item in inventory.rule_maps.items() {
        lines.push(String::new());
        lines.push(format!(
            "{}/{}  {} rules  keys {}",
            item.namespace,
            item.name,
            item.rule_count,
            item.keys.join(",")
        ));
    }
    for item in inventory.events.items() {
        lines.push(String::new());
        let mut line = item.rule.clone();
        if !item.priority.is_empty() {
            line.push_str("  ");
            line.push_str(&item.priority);
        }
        if !item.namespace.is_empty() || !item.pod.is_empty() {
            line.push_str("  ");
            line.push_str(&item.namespace);
            if !item.pod.is_empty() {
                line.push('/');
                line.push_str(&item.pod);
            }
        }
        if !item.time.is_empty() {
            line.push_str("  ");
            line.push_str(&item.time);
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
#[path = "falco_test.rs"]
mod tests;
