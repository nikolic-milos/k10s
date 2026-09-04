//! Tetragon inventory from TracingPolicy CRs the cluster already serves.
//!
//! Official Tetragon docs still publish TracingPolicy on
//! `cilium.io/v1alpha1`. This module lists from that group when the kind
//! is named there, and also probes `tetragon.io` so a fork that moved the
//! CRD is not invisible. `cilium.io` also hosts CiliumNetworkPolicy; a
//! 200 on that group document is not Tetragon. A 404 on a Tetragon kind is
//! [`KindSet::NotServed`]. A 403 is [`KindSet::Denied`]. Nothing is installed
//! to find these objects, and the gRPC getevents stream is never opened.
//!
//! [`DeclaredPolicy`] is what a TracingPolicy asks (hook counts and the
//! container/node/host/pod selectors, never the hook bodies). Upstream
//! TracingPolicy is `+genclient:noStatus` and its spec has no `disabled`
//! field, so `enabled`/`status` populate only on a fork that adds them and
//! are otherwise `enabled` / [`STATUS_ABSENT`] by construction.
//! [`ObservedEvent`] is what Tetragon already emitted in a JSON export.
//! Mixing those is a correctness bug, not a cosmetic one.

use std::collections::BTreeMap;

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;
use crate::served::{GroupAnswer, ListErr, after_group, after_list, group_url, order_versions};

pub const CILIUM_GROUP: &str = "cilium.io";
pub const TETRAGON_GROUP: &str = "tetragon.io";
pub const GROUPS: [&str; 2] = [CILIUM_GROUP, TETRAGON_GROUP];
pub const VERSION: &str = "v1alpha1";

const PAGE_LIMIT: u32 = 200;
pub const MAX_OBJECTS: usize = 2_000;
pub const MAX_FIELD_CHARS: usize = 200;
pub const MAX_PAGE_BYTES: usize = 8 << 20;
pub const MAX_EVENTS: usize = 1_024;
pub const MAX_ARG_BYTES: usize = 256;
pub const MAX_WORKLOADS: usize = 16;

const WORKLOAD_NAMES: &[&str] = &["tetragon", "cilium-tetragon"];
const WORKLOAD_LABEL: &str = "app.kubernetes.io/name";
const WORKLOAD_LABEL_VALUE: &str = "tetragon";

const EVENTS_UNBOUND_WHY: &str =
    "Tetragon getevents is a gRPC stream; this module does not open one";

/// Upstream TracingPolicy is `+genclient:noStatus`: the object carries no
/// status to read, which is not the same as a healthy empty one.
pub const STATUS_ABSENT: &str = "no status on this CRD";

/// The three CRs this inventory reads. Tetragon serves more; those are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    TracingPolicy,
    TracingPolicyNamespaced,
    PodInfo,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::TracingPolicy => "TracingPolicy",
            Kind::TracingPolicyNamespaced => "TracingPolicyNamespaced",
            Kind::PodInfo => "PodInfo",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::TracingPolicy => "tracingpolicies",
            Kind::TracingPolicyNamespaced => "tracingpoliciesnamespaced",
            Kind::PodInfo => "podinfo",
        }
    }

    pub fn namespaced(self) -> bool {
        !matches!(self, Kind::TracingPolicy)
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::TracingPolicy => "tetragon tracingpolicies",
            Kind::TracingPolicyNamespaced => "tetragon tracingpoliciesnamespaced",
            Kind::PodInfo => "tetragon podinfo",
        }
    }
}

/// 404 on the group document is [`GroupState::NotServed`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GroupState {
    Served,
    #[default]
    NotServed,
    Denied,
}

impl GroupState {
    pub fn answered(&self) -> bool {
        !matches!(self, GroupState::NotServed)
    }
}

/// What one kind's list answered.
///
/// A 404 is [`KindSet::NotServed`]: invisible, not broken. A 403 is
/// [`KindSet::Denied`]. Those are different states on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KindSet<T> {
    Served {
        items: Vec<T>,
        truncated: bool,
        unreadable: usize,
    },
    #[default]
    NotServed,
    Denied,
}

impl<T> KindSet<T> {
    /// False when the kind answered 404.
    pub fn served(&self) -> bool {
        !matches!(self, KindSet::NotServed)
    }

    pub fn items(&self) -> &[T] {
        match self {
            KindSet::Served { items, .. } => items,
            KindSet::NotServed | KindSet::Denied => &[],
        }
    }
}

/// What a TracingPolicy (or TracingPolicyNamespaced) asks. Hook bodies and
/// per-hook arg filters are counted, never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredPolicy {
    pub kind: Kind,
    pub group: String,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub enabled: bool,
    pub status: String,
    pub kprobes: usize,
    pub lsm: usize,
    pub tracepoints: usize,
    pub uprobes: usize,
    pub scope_selector: String,
    pub pod_selector: String,
}

/// PodInfo identity only. No process dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodInfo {
    pub group: String,
    pub version: String,
    pub name: String,
    pub namespace: String,
    pub pod_uid: String,
    pub workload: String,
}

/// A DaemonSet or Service that matched the Tetragon fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    pub kind: WorkloadKind,
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    DaemonSet,
    Service,
}

impl WorkloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadKind::DaemonSet => "DaemonSet",
            WorkloadKind::Service => "Service",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WorkloadSet {
    Found(Vec<Workload>),
    #[default]
    Absent,
    Denied,
}

/// Tetragon getevents is gRPC. This build does not open that stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSource {
    Unbound { why: &'static str },
}

/// What Tetragon already emitted. Never a TracingPolicy row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedKind {
    ProcessExec,
    ProcessKprobe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedEvent {
    pub kind: ObservedKind,
    pub names: String,
    pub binary: String,
    pub flags: String,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservedEvents {
    pub events: Vec<ObservedEvent>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventsError {
    TooLarge { bytes: usize },
    NotJson(String),
}

impl std::fmt::Display for EventsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventsError::TooLarge { bytes } => {
                write!(
                    f,
                    "Tetragon events JSON is {bytes} bytes; the cap is {MAX_PAGE_BYTES}"
                )
            }
            EventsError::NotJson(why) => write!(f, "Tetragon events JSON did not parse: {why}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub cilium: GroupState,
    pub tetragon: GroupState,
    pub tracing_policies: KindSet<DeclaredPolicy>,
    pub tracing_policies_namespaced: KindSet<DeclaredPolicy>,
    pub pod_infos: KindSet<PodInfo>,
    pub workload: WorkloadSet,
}

impl Inventory {
    /// True when a Tetragon kind is visible, or a group was denied so absence
    /// cannot be claimed. `cilium.io` answering for CiliumNetworkPolicy alone
    /// is not Tetragon.
    pub fn served(&self) -> bool {
        self.tracing_policies.served()
            || self.tracing_policies_namespaced.served()
            || self.pod_infos.served()
            || matches!(self.cilium, GroupState::Denied)
            || matches!(self.tetragon, GroupState::Denied)
    }

    fn sets(&self) -> [RowSet<'_>; 3] {
        [
            RowSet::Policies(Kind::TracingPolicy, &self.tracing_policies),
            RowSet::Policies(
                Kind::TracingPolicyNamespaced,
                &self.tracing_policies_namespaced,
            ),
            RowSet::Pods(&self.pod_infos),
        ]
    }
}

enum RowSet<'a> {
    Policies(Kind, &'a KindSet<DeclaredPolicy>),
    Pods(&'a KindSet<PodInfo>),
}

/// Always [`EventSource::Unbound`]: no tonic, no stream.
pub fn event_source() -> EventSource {
    EventSource::Unbound {
        why: EVENTS_UNBOUND_WHY,
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
    spec: Value,
    #[serde(default)]
    status: Value,
    #[serde(default, rename = "workloadType")]
    workload_type: WireType,
    #[serde(default, rename = "workloadObject")]
    workload_object: WireWorkload,
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
    annotations: BTreeMap<String, String>,
    #[serde(default, rename = "ownerReferences")]
    owners: Vec<WireOwner>,
}

#[derive(Deserialize, Default)]
struct WireOwner {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    uid: String,
}

#[derive(Deserialize, Default)]
struct WireType {
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize, Default)]
struct WireWorkload {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
}

fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn clip_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = text[..end].to_string();
    cut.push('\u{2026}');
    cut
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> &'a str {
    for key in keys {
        let text = str_field(value, key);
        if !text.is_empty() {
            return text;
        }
    }
    ""
}

fn hook_count(spec: &Value, key: &str) -> usize {
    spec.get(key)
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

fn label_selector_text(value: Option<&Value>) -> String {
    let Some(selector) = value else {
        return String::new();
    };
    if selector.is_null() {
        return String::new();
    }
    let mut parts = Vec::new();
    if let Some(labels) = selector.get("matchLabels").and_then(Value::as_object) {
        let mut keys: Vec<&String> = labels.keys().collect();
        keys.sort();
        for key in keys {
            let Some(label) = labels.get(key).and_then(Value::as_str) else {
                continue;
            };
            parts.push(format!("{key}={label}"));
        }
    }
    if let Some(exprs) = selector.get("matchExpressions").and_then(Value::as_array) {
        for expr in exprs {
            let key = str_field(expr, "key");
            let op = str_field(expr, "operator");
            if key.is_empty() && op.is_empty() {
                continue;
            }
            let values = expr
                .get("values")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            if values.is_empty() {
                parts.push(format!("{key} {op}").trim().to_string());
            } else {
                parts.push(format!("{key} {op} ({values})"));
            }
        }
    }
    clipped(parts.join(","))
}

fn annotation_disabled(annotations: &BTreeMap<String, String>) -> bool {
    const KEYS: &[&str] = &[
        "tetragon.io/disabled",
        "tracingpolicy.tetragon.io/disabled",
        "cilium.io/disabled",
    ];
    KEYS.iter().any(|key| {
        annotations
            .get(*key)
            .is_some_and(|value| matches!(value.as_str(), "true" | "True" | "1"))
    })
}

fn enabled_of(meta: &WireMeta, spec: &Value, status: &Value) -> bool {
    if spec.get("disabled").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if annotation_disabled(&meta.annotations) {
        return false;
    }
    if status.get("enabled").and_then(Value::as_bool) == Some(false) {
        return false;
    }
    // TP_STATE_DISABLED is a gRPC TracingPolicyState enum value; no CRD
    // stores it, so it must not read as a disabled row.
    let state = first_str(status, &["state", "State"]);
    if state.eq_ignore_ascii_case("disabled") {
        return false;
    }
    true
}

fn status_of(status: &Value) -> String {
    let state = first_str(status, &["state", "State"]);
    let error = first_str(status, &["error", "Error"]);
    let text = match (state.is_empty(), error.is_empty()) {
        (true, true) => condition_status(status),
        (false, true) => state.to_string(),
        (true, false) => error.to_string(),
        (false, false) => format!("{state}: {error}"),
    };
    clipped(text)
}

fn condition_status(status: &Value) -> String {
    let Some(conditions) = status.get("conditions").and_then(Value::as_array) else {
        return String::new();
    };
    for want in ["Ready", "Loaded", "Error"] {
        if let Some(condition) = conditions
            .iter()
            .find(|item| str_field(item, "type") == want)
        {
            let message = str_field(condition, "message");
            if !message.is_empty() {
                return message.to_string();
            }
            return str_field(condition, "status").to_string();
        }
    }
    String::new()
}

fn pod_uid_of(meta: &WireMeta) -> String {
    meta.owners
        .iter()
        .find(|owner| owner.kind == "Pod" && !owner.uid.is_empty())
        .map(|owner| clipped(owner.uid.clone()))
        .unwrap_or_default()
}

fn workload_of(wire: &WireObject) -> String {
    let kind = wire.workload_type.kind.as_str();
    let name = wire.workload_object.name.as_str();
    let namespace = wire.workload_object.namespace.as_str();
    let text = match (kind.is_empty(), name.is_empty(), namespace.is_empty()) {
        (true, true, _) => String::new(),
        (false, true, _) => kind.to_string(),
        (true, false, true) => name.to_string(),
        (true, false, false) => format!("{namespace}/{name}"),
        (false, false, true) => format!("{kind}/{name}"),
        (false, false, false) => format!("{kind}/{namespace}/{name}"),
    };
    clipped(text)
}

// TracingPolicySpec scopes with containerSelector, nodeSelector, and
// hostSelector; the CRD has no namespaceSelector. Each hit is labelled with
// its source so a blank cell means "no selector", not "a selector unread".
fn scope_selector_of(kind: Kind, meta: &WireMeta, spec: &Value) -> String {
    for (label, key) in [
        ("container", "containerSelector"),
        ("node", "nodeSelector"),
        ("host", "hostSelector"),
    ] {
        let text = label_selector_text(spec.get(key));
        if !text.is_empty() {
            return clipped(format!("{label} {text}"));
        }
    }
    if kind == Kind::TracingPolicyNamespaced && !meta.namespace.is_empty() {
        return clipped(format!("ns {}", meta.namespace));
    }
    String::new()
}

fn from_policy(kind: Kind, group: &str, version: &str, wire: WireObject) -> Option<DeclaredPolicy> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    let spec = &wire.spec;
    let status = if wire.status.is_null() {
        STATUS_ABSENT.to_string()
    } else {
        status_of(&wire.status)
    };
    Some(DeclaredPolicy {
        kind,
        group: group.to_string(),
        version: version.to_string(),
        name: clipped(wire.metadata.name.clone()),
        namespace: clipped(wire.metadata.namespace.clone()),
        uid: clipped(wire.metadata.uid.clone()),
        enabled: enabled_of(&wire.metadata, spec, &wire.status),
        status,
        kprobes: hook_count(spec, "kprobes"),
        lsm: hook_count(spec, "lsmhooks"),
        tracepoints: hook_count(spec, "tracepoints"),
        uprobes: hook_count(spec, "uprobes"),
        scope_selector: scope_selector_of(kind, &wire.metadata, spec),
        pod_selector: label_selector_text(spec.get("podSelector")),
    })
}

fn from_podinfo(group: &str, version: &str, wire: WireObject) -> Option<PodInfo> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    Some(PodInfo {
        group: group.to_string(),
        version: version.to_string(),
        name: clipped(wire.metadata.name.clone()),
        namespace: clipped(wire.metadata.namespace.clone()),
        pod_uid: pod_uid_of(&wire.metadata),
        workload: workload_of(&wire),
    })
}

fn parse_policy(kind: Kind, group: &str, version: &str, value: Value) -> Option<DeclaredPolicy> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_policy(kind, group, version, wire)
}

fn parse_podinfo(group: &str, version: &str, value: Value) -> Option<PodInfo> {
    let wire: WireObject = serde_json::from_value(value).ok()?;
    from_podinfo(group, version, wire)
}

fn versions_for(group_versions: &[String]) -> Vec<String> {
    let mut out = group_versions.to_vec();
    if !out.iter().any(|have| have == VERSION) {
        out.push(VERSION.to_string());
    }
    out
}

fn collection_url(group: &str, version: &str, kind: Kind, namespace: Option<&str>) -> String {
    let mut path = format!("/apis/{group}/{version}");
    if let Some(namespace) = namespace.filter(|_| kind.namespaced()) {
        path.push_str("/namespaces/");
        path.push_str(namespace);
    }
    path.push('/');
    path.push_str(kind.plural());
    path
}

fn hooks_cell(policy: &DeclaredPolicy) -> String {
    format!(
        "kprobe={} lsm={} tracepoint={} uprobe={}",
        policy.kprobes, policy.lsm, policy.tracepoints, policy.uprobes
    )
}

fn selector_cell(policy: &DeclaredPolicy) -> String {
    match (
        policy.scope_selector.is_empty(),
        policy.pod_selector.is_empty(),
    ) {
        (true, true) => String::new(),
        (false, true) => policy.scope_selector.clone(),
        (true, false) => policy.pod_selector.clone(),
        (false, false) => clipped(format!(
            "{}; pod {}",
            policy.scope_selector, policy.pod_selector
        )),
    }
}

fn row_uid(kind: Kind, namespace: &str, name: &str, uid: &str) -> String {
    if uid.is_empty() {
        format!("{}/{}/{}", kind.as_str(), namespace, name)
    } else {
        uid.to_string()
    }
}

/// Native list rows. `None` when no Tetragon kind is served and no group was
/// denied, so a UI stays invisible rather than opening an empty pane. A
/// denied kind is a labelled row, not absence.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    if !inventory.served() {
        return None;
    }
    let columns = [
        "Kind",
        "Name",
        "Namespace",
        "Enabled",
        "Status",
        "Hooks",
        "Selector",
    ]
    .iter()
    .map(|name| TableColumn {
        name: name.to_string(),
        wide: false,
    })
    .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    for set in inventory.sets() {
        match set {
            RowSet::Policies(kind, set) => push_policy_rows(&mut rows, &mut truncated, kind, set),
            RowSet::Pods(set) => push_pod_rows(&mut rows, &mut truncated, set),
        }
    }
    Some(TablePage {
        columns,
        rows,
        truncated,
        continue_token: None,
    })
}

fn push_policy_rows(
    rows: &mut Vec<TableRow>,
    truncated: &mut bool,
    kind: Kind,
    set: &KindSet<DeclaredPolicy>,
) {
    match set {
        KindSet::NotServed => {}
        KindSet::Denied => rows.push(denied_row(kind)),
        KindSet::Served {
            items,
            truncated: cap,
            ..
        } => {
            *truncated |= *cap;
            for item in items {
                rows.push(TableRow {
                    cells: vec![
                        item.kind.as_str().to_string(),
                        item.name.clone(),
                        item.namespace.clone(),
                        if item.enabled {
                            "enabled".to_string()
                        } else {
                            "disabled".to_string()
                        },
                        item.status.clone(),
                        hooks_cell(item),
                        selector_cell(item),
                    ],
                    name: item.name.clone(),
                    namespace: if item.namespace.is_empty() {
                        None
                    } else {
                        Some(item.namespace.clone())
                    },
                    uid: row_uid(item.kind, &item.namespace, &item.name, &item.uid),
                });
            }
        }
    }
}

fn push_pod_rows(rows: &mut Vec<TableRow>, truncated: &mut bool, set: &KindSet<PodInfo>) {
    match set {
        KindSet::NotServed => {}
        KindSet::Denied => rows.push(denied_row(Kind::PodInfo)),
        KindSet::Served {
            items,
            truncated: cap,
            ..
        } => {
            *truncated |= *cap;
            for item in items {
                rows.push(TableRow {
                    cells: vec![
                        Kind::PodInfo.as_str().to_string(),
                        item.name.clone(),
                        item.namespace.clone(),
                        String::new(),
                        item.workload.clone(),
                        String::new(),
                        String::new(),
                    ],
                    name: item.name.clone(),
                    namespace: Some(item.namespace.clone()),
                    uid: row_uid(Kind::PodInfo, &item.namespace, &item.name, &item.pod_uid),
                });
            }
        }
    }
}

fn denied_row(kind: Kind) -> TableRow {
    TableRow {
        cells: vec![
            kind.as_str().to_string(),
            String::new(),
            String::new(),
            "access denied for this account".to_string(),
            String::new(),
            String::new(),
            String::new(),
        ],
        name: kind.as_str().to_string(),
        namespace: None,
        uid: format!("denied:{}", kind.as_str()),
    }
}

/// Whether a DaemonSet or Service is the Tetragon workload. Exact names
/// `tetragon` / `cilium-tetragon`, or label `app.kubernetes.io/name=tetragon`.
pub fn matches_workload(name: &str, labels: &BTreeMap<String, String>) -> bool {
    let lower = name.to_ascii_lowercase();
    if WORKLOAD_NAMES.iter().any(|want| lower == *want) {
        return true;
    }
    labels
        .get(WORKLOAD_LABEL)
        .is_some_and(|value| value.eq_ignore_ascii_case(WORKLOAD_LABEL_VALUE))
}

fn parse_workload(kind: WorkloadKind, value: &Value) -> Option<Workload> {
    let meta = value.get("metadata")?;
    let name = meta.get("name").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let labels = meta
        .get("labels")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    if !matches_workload(name, &labels) {
        return None;
    }
    Some(Workload {
        kind,
        namespace: clipped(
            meta.get("namespace")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        name: clipped(name.to_string()),
    })
}

async fn probe_group(client: &Client, group: &str) -> GroupAnswer {
    let request = match http::Request::get(group_url(group)).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    match client.request::<WireGroup>(request).await {
        Ok(doc) => {
            let versions = order_versions(&doc.preferred.version, {
                doc.versions.into_iter().map(|item| item.version).collect()
            });
            GroupAnswer::Served(versions)
        }
        Err(error) => after_group(&error),
    }
}

async fn list_at_version<T, F>(
    client: &Client,
    path: String,
    mut parse: F,
) -> Result<KindSet<T>, ListErr>
where
    F: FnMut(Value) -> Option<T>,
{
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
            match parse(value) {
                Some(item) => items.push(item),
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

async fn list_kind<T, F>(
    client: &Client,
    kind: Kind,
    sources: &[(GroupState, &'static str, Vec<String>)],
    namespace: Option<&str>,
    mut parse: F,
) -> Result<KindSet<T>, Fetched<Inventory>>
where
    F: FnMut(&'static str, &str, Value) -> Option<T>,
{
    let mut saw_denied = false;
    for (state, group, versions) in sources {
        match state {
            GroupState::NotServed => continue,
            GroupState::Denied => {
                saw_denied = true;
                continue;
            }
            GroupState::Served => {}
        }
        for version in versions_for(versions) {
            let path = collection_url(group, &version, kind, namespace);
            let mut parse_item = |value: Value| parse(group, &version, value);
            match list_at_version(client, path, &mut parse_item).await {
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
    }
    if saw_denied {
        Ok(KindSet::Denied)
    } else {
        Ok(KindSet::NotServed)
    }
}

async fn list_core(
    client: &Client,
    path: &str,
    kind: WorkloadKind,
    into: &mut Vec<Workload>,
) -> Result<bool, ListErr> {
    let mut token: Option<String> = None;
    let mut truncated = false;
    loop {
        if into.len() >= MAX_WORKLOADS {
            return Ok(true);
        }
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = match Request::new(path).list(&params) {
            Ok(request) => request,
            Err(error) => return Err(ListErr::Failed(error.to_string())),
        };
        let page = match client.request::<WireList>(request).await {
            Ok(page) => page,
            Err(error) => return Err(after_list(&error)),
        };
        for value in page.items {
            if into.len() >= MAX_WORKLOADS {
                truncated = true;
                break;
            }
            if let Some(hit) = parse_workload(kind, &value) {
                into.push(hit);
            }
        }
        token = (!page.metadata.cont.is_empty() && !truncated).then_some(page.metadata.cont);
        if token.is_none() {
            break;
        }
    }
    Ok(truncated)
}

async fn find_workload(client: &Client) -> WorkloadSet {
    let mut found = Vec::new();
    match list_core(
        client,
        "/apis/apps/v1/daemonsets",
        WorkloadKind::DaemonSet,
        &mut found,
    )
    .await
    {
        Ok(_) => {}
        Err(ListErr::Denied) => return WorkloadSet::Denied,
        Err(ListErr::NotFound | ListErr::Failed(_)) => {}
    }
    if found.len() < MAX_WORKLOADS {
        match list_core(
            client,
            "/api/v1/services",
            WorkloadKind::Service,
            &mut found,
        )
        .await
        {
            Ok(_) => {}
            Err(ListErr::Denied) if found.is_empty() => return WorkloadSet::Denied,
            Err(ListErr::Denied | ListErr::NotFound | ListErr::Failed(_)) => {}
        }
    }
    if found.is_empty() {
        WorkloadSet::Absent
    } else {
        found.truncate(MAX_WORKLOADS);
        WorkloadSet::Found(found)
    }
}

fn into_group(answer: GroupAnswer) -> Result<(GroupState, Vec<String>), String> {
    match answer {
        GroupAnswer::Served(versions) => Ok((GroupState::Served, versions)),
        GroupAnswer::NotServed => Ok((GroupState::NotServed, Vec::new())),
        GroupAnswer::Denied => Ok((GroupState::Denied, Vec::new())),
        GroupAnswer::Failed(why) => Err(why),
    }
}

/// List TracingPolicy, TracingPolicyNamespaced, and PodInfo if either group
/// serves them. A missing Tetragon kind is invisible; a forbidden one is Denied.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let cilium_answer = into_group(probe_group(client, CILIUM_GROUP).await);
    let tetragon_answer = into_group(probe_group(client, TETRAGON_GROUP).await);
    let (cilium, cilium_versions, tetragon, tetragon_versions) =
        match (cilium_answer, tetragon_answer) {
            (Err(why), Err(_))
            | (Err(why), Ok((GroupState::NotServed, _)))
            | (Ok((GroupState::NotServed, _)), Err(why)) => {
                return Fetched::Failed {
                    what: "tetragon",
                    why,
                };
            }
            (Err(_), Ok((state, versions))) => (GroupState::NotServed, Vec::new(), state, versions),
            (Ok((state, versions)), Err(_)) => (state, versions, GroupState::NotServed, Vec::new()),
            (Ok((cilium, cilium_versions)), Ok((tetragon, tetragon_versions))) => {
                (cilium, cilium_versions, tetragon, tetragon_versions)
            }
        };

    let sources = [
        (tetragon.clone(), TETRAGON_GROUP, tetragon_versions),
        (cilium.clone(), CILIUM_GROUP, cilium_versions),
    ];

    let tracing_policies = match list_kind(
        client,
        Kind::TracingPolicy,
        &sources,
        namespace,
        |group, version, value| parse_policy(Kind::TracingPolicy, group, version, value),
    )
    .await
    {
        Ok(set) => set,
        Err(failed) => return failed,
    };
    let tracing_policies_namespaced = match list_kind(
        client,
        Kind::TracingPolicyNamespaced,
        &sources,
        namespace,
        |group, version, value| parse_policy(Kind::TracingPolicyNamespaced, group, version, value),
    )
    .await
    {
        Ok(set) => set,
        Err(failed) => return failed,
    };
    let pod_infos = match list_kind(
        client,
        Kind::PodInfo,
        &sources,
        namespace,
        |group, version, value| parse_podinfo(group, version, value),
    )
    .await
    {
        Ok(set) => set,
        Err(failed) => return failed,
    };

    let workload = find_workload(client).await;
    Fetched::Ok(Inventory {
        cilium,
        tetragon,
        tracing_policies,
        tracing_policies_namespaced,
        pod_infos,
        workload,
    })
}

fn object_get<'a>(value: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    value.get(snake).or_else(|| value.get(camel))
}

fn process_of(event: &Value) -> &Value {
    event.get("process").unwrap_or(&Value::Null)
}

fn names_of(process: &Value, event: &Value) -> String {
    if let Some(names) = event.get("names").or_else(|| process.get("names")) {
        if let Some(text) = names.as_str() {
            if !text.is_empty() {
                return clipped(text.to_string());
            }
        }
        if let Some(items) = names.as_array() {
            let joined = items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join(",");
            if !joined.is_empty() {
                return clipped(joined);
            }
        }
    }
    let pod = process.get("pod").unwrap_or(&Value::Null);
    let namespace = str_field(pod, "namespace");
    let name = str_field(pod, "name");
    let text = match (namespace.is_empty(), name.is_empty()) {
        (true, true) => String::new(),
        (false, true) => namespace.to_string(),
        (true, false) => name.to_string(),
        (false, false) => format!("{namespace}/{name}"),
    };
    clipped(text)
}

fn arg_brief(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let object = value.as_object()?;
    for key in [
        "string_arg",
        "stringArg",
        "bytes_arg",
        "bytesArg",
        "file_arg",
        "fileArg",
    ] {
        if let Some(field) = object.get(key) {
            if let Some(text) = field.as_str() {
                return Some(text.to_string());
            }
            let path = first_str(field, &["path", "Path"]);
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn args_of(process: &Value, event: &Value) -> String {
    if let Some(args) = event.get("args") {
        if let Some(items) = args.as_array() {
            let joined = items
                .iter()
                .filter_map(arg_brief)
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.is_empty() {
                return clip_bytes(&joined, MAX_ARG_BYTES);
            }
        }
    }
    let arguments = first_str(process, &["arguments", "args"]);
    clip_bytes(arguments, MAX_ARG_BYTES)
}

fn observed_from_object(value: &Value) -> Option<ObservedEvent> {
    let (kind, event) = if let Some(event) = object_get(value, "process_exec", "processExec") {
        (ObservedKind::ProcessExec, event)
    } else if let Some(event) = object_get(value, "process_kprobe", "processKprobe") {
        (ObservedKind::ProcessKprobe, event)
    } else {
        return None;
    };
    let process = process_of(event);
    Some(ObservedEvent {
        kind,
        names: names_of(process, event),
        binary: clipped(first_str(process, &["binary"]).to_string()),
        flags: clipped(first_str(process, &["flags"]).to_string()),
        args: args_of(process, event),
    })
}

fn ingest_value(value: &Value, into: &mut ObservedEvents) {
    if into.events.len() >= MAX_EVENTS {
        into.truncated = true;
        return;
    }
    if let Some(items) = value.as_array() {
        for item in items {
            ingest_value(item, into);
            if into.truncated {
                return;
            }
        }
        return;
    }
    if let Some(items) = value.get("events").and_then(Value::as_array) {
        for item in items {
            ingest_value(item, into);
            if into.truncated {
                return;
            }
        }
        return;
    }
    if let Some(event) = observed_from_object(value) {
        if into.events.len() >= MAX_EVENTS {
            into.truncated = true;
            return;
        }
        into.events.push(event);
    }
}

/// Parse Tetragon JSON / proto-json getevents bytes already in hand.
/// Does not open a gRPC stream. Caps event count and arg bytes.
pub fn parse_events(bytes: &[u8]) -> Result<ObservedEvents, EventsError> {
    if bytes.len() > MAX_PAGE_BYTES {
        return Err(EventsError::TooLarge { bytes: bytes.len() });
    }
    if bytes.is_empty() {
        return Ok(ObservedEvents::default());
    }
    let mut out = ObservedEvents::default();
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        ingest_value(&value, &mut out);
        return Ok(out);
    }
    let text =
        std::str::from_utf8(bytes).map_err(|error| EventsError::NotJson(error.to_string()))?;
    let mut parsed_any = false;
    let mut last_err = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => {
                parsed_any = true;
                ingest_value(&value, &mut out);
                if out.truncated {
                    break;
                }
            }
            Err(error) => last_err = error.to_string(),
        }
    }
    if parsed_any {
        return Ok(out);
    }
    Err(EventsError::NotJson(last_err))
}

#[cfg(test)]
#[path = "tetragon_test.rs"]
mod tests;
