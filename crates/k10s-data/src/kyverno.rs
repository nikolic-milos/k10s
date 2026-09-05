//! Kyverno policies from the CRs the controller already publishes.
//!
//! ClusterPolicy and Policy live on `kyverno.io` v1. CleanupPolicy and
//! ClusterCleanupPolicy are listed only when that group document names a v2
//! version. Those kinds are deprecated as of Kyverno 1.17, critical-fixes-only
//! through 1.19, and planned for removal in 1.20, so they stay in this
//! inventory: this lab's CRD-only fixtures still serve them. The legacy
//! PolicyException that exempts ClusterPolicy and Policy rules lives on
//! `kyverno.io/v2`; it is a different CRD from the CEL-group one. Current
//! stable CEL policies live on `policies.kyverno.io/v1` (ValidatingPolicy
//! and the other CEL kinds, plus PolicyException when that group serves
//! it). A
//! cluster that does not serve a group answers 404 and that group stays
//! invisible, not broken; the other group can still be served. A 403 is
//! Denied. Nothing is installed to find them, and the admission engine is
//! not reimplemented.
//!
//! Rule bodies and CEL expressions stay out of the inventory: a `validate`
//! pattern, a CEL `expression`, or an `exclude` blob can hold anything, so
//! parse keeps a clipped match-kind list and drops the rest. Findings are
//! not computed here. Kyverno writes those to `wgpolicyk8s.io` PolicyReport
//! CRs or, when enabled, `openreports.io` Report CRs; callers read them
//! through [`crate::policy`].

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::Fetched;
use crate::served::{GroupAnswer, ListErr, after_group, after_list, group_url, order_versions};

pub const GROUP: &str = "kyverno.io";
pub const CEL_GROUP: &str = "policies.kyverno.io";
pub const SEVERITY_ANNOTATION: &str = "policies.kyverno.io/severity";

const PAGE_LIMIT: u32 = 200;
const MAX_OBJECTS: usize = 2_000;
const MAX_FIELD_CHARS: usize = 200;
const MAX_RULE_KINDS: usize = 32;

const LEGACY_KINDS: [Kind; 5] = [
    Kind::ClusterPolicy,
    Kind::Policy,
    Kind::CleanupPolicy,
    Kind::ClusterCleanupPolicy,
    Kind::LegacyPolicyException,
];

const CEL_KINDS: [Kind; 11] = [
    Kind::ValidatingPolicy,
    Kind::NamespacedValidatingPolicy,
    Kind::MutatingPolicy,
    Kind::NamespacedMutatingPolicy,
    Kind::GeneratingPolicy,
    Kind::NamespacedGeneratingPolicy,
    Kind::DeletingPolicy,
    Kind::NamespacedDeletingPolicy,
    Kind::ImageValidatingPolicy,
    Kind::NamespacedImageValidatingPolicy,
    Kind::PolicyException,
];

const CEL_FALLBACKS: &[&str] = &["v1", "v1beta1"];
const LEGACY_EXCEPTION_FALLBACKS: &[&str] = &["v2", "v2beta1", "v2alpha1"];

/// The policy CRs this inventory reads. Kyverno serves more; those are not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ClusterPolicy,
    Policy,
    CleanupPolicy,
    ClusterCleanupPolicy,
    /// The `kyverno.io/v2` PolicyException that exempts ClusterPolicy and
    /// Policy rules; a different CRD from [`Kind::PolicyException`].
    LegacyPolicyException,
    ValidatingPolicy,
    NamespacedValidatingPolicy,
    MutatingPolicy,
    NamespacedMutatingPolicy,
    GeneratingPolicy,
    NamespacedGeneratingPolicy,
    DeletingPolicy,
    NamespacedDeletingPolicy,
    ImageValidatingPolicy,
    NamespacedImageValidatingPolicy,
    PolicyException,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ClusterPolicy => "ClusterPolicy",
            Kind::Policy => "Policy",
            Kind::CleanupPolicy => "CleanupPolicy",
            Kind::ClusterCleanupPolicy => "ClusterCleanupPolicy",
            Kind::LegacyPolicyException => "PolicyException",
            Kind::ValidatingPolicy => "ValidatingPolicy",
            Kind::NamespacedValidatingPolicy => "NamespacedValidatingPolicy",
            Kind::MutatingPolicy => "MutatingPolicy",
            Kind::NamespacedMutatingPolicy => "NamespacedMutatingPolicy",
            Kind::GeneratingPolicy => "GeneratingPolicy",
            Kind::NamespacedGeneratingPolicy => "NamespacedGeneratingPolicy",
            Kind::DeletingPolicy => "DeletingPolicy",
            Kind::NamespacedDeletingPolicy => "NamespacedDeletingPolicy",
            Kind::ImageValidatingPolicy => "ImageValidatingPolicy",
            Kind::NamespacedImageValidatingPolicy => "NamespacedImageValidatingPolicy",
            Kind::PolicyException => "PolicyException",
        }
    }

    pub fn group(self) -> &'static str {
        if self.is_cel() { CEL_GROUP } else { GROUP }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::ClusterPolicy => "clusterpolicies",
            Kind::Policy => "policies",
            Kind::CleanupPolicy => "cleanuppolicies",
            Kind::ClusterCleanupPolicy => "clustercleanuppolicies",
            Kind::LegacyPolicyException => "policyexceptions",
            Kind::ValidatingPolicy => "validatingpolicies",
            Kind::NamespacedValidatingPolicy => "namespacedvalidatingpolicies",
            Kind::MutatingPolicy => "mutatingpolicies",
            Kind::NamespacedMutatingPolicy => "namespacedmutatingpolicies",
            Kind::GeneratingPolicy => "generatingpolicies",
            Kind::NamespacedGeneratingPolicy => "namespacedgeneratingpolicies",
            Kind::DeletingPolicy => "deletingpolicies",
            Kind::NamespacedDeletingPolicy => "namespaceddeletingpolicies",
            Kind::ImageValidatingPolicy => "imagevalidatingpolicies",
            Kind::NamespacedImageValidatingPolicy => "namespacedimagevalidatingpolicies",
            Kind::PolicyException => "policyexceptions",
        }
    }

    pub fn namespaced(self) -> bool {
        matches!(
            self,
            Kind::Policy
                | Kind::CleanupPolicy
                | Kind::LegacyPolicyException
                | Kind::NamespacedValidatingPolicy
                | Kind::NamespacedMutatingPolicy
                | Kind::NamespacedGeneratingPolicy
                | Kind::NamespacedDeletingPolicy
                | Kind::NamespacedImageValidatingPolicy
                | Kind::PolicyException
        )
    }

    pub fn what(self) -> &'static str {
        match self {
            Kind::ClusterPolicy => "kyverno clusterpolicies",
            Kind::Policy => "kyverno policies",
            Kind::CleanupPolicy => "kyverno cleanuppolicies",
            Kind::ClusterCleanupPolicy => "kyverno clustercleanuppolicies",
            Kind::LegacyPolicyException => "kyverno legacy policyexceptions",
            Kind::ValidatingPolicy => "kyverno validatingpolicies",
            Kind::NamespacedValidatingPolicy => "kyverno namespacedvalidatingpolicies",
            Kind::MutatingPolicy => "kyverno mutatingpolicies",
            Kind::NamespacedMutatingPolicy => "kyverno namespacedmutatingpolicies",
            Kind::GeneratingPolicy => "kyverno generatingpolicies",
            Kind::NamespacedGeneratingPolicy => "kyverno namespacedgeneratingpolicies",
            Kind::DeletingPolicy => "kyverno deletingpolicies",
            Kind::NamespacedDeletingPolicy => "kyverno namespaceddeletingpolicies",
            Kind::ImageValidatingPolicy => "kyverno imagevalidatingpolicies",
            Kind::NamespacedImageValidatingPolicy => "kyverno namespacedimagevalidatingpolicies",
            Kind::PolicyException => "kyverno policyexceptions",
        }
    }

    fn is_cleanup(self) -> bool {
        matches!(self, Kind::CleanupPolicy | Kind::ClusterCleanupPolicy)
    }

    fn is_cel(self) -> bool {
        !matches!(
            self,
            Kind::ClusterPolicy
                | Kind::Policy
                | Kind::CleanupPolicy
                | Kind::ClusterCleanupPolicy
                | Kind::LegacyPolicyException
        )
    }
}

/// One policy CR, reduced to what an inventory shows.
///
/// There is nowhere here for a rule body, a CEL expression, an exclude list,
/// or a PolicyReport finding. Adding any of those is a decision about secret
/// exposure, not a convenience.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub version: String,
    pub name: String,
    /// Empty on a cluster-scoped policy.
    pub namespace: String,
    pub uid: String,
    pub background: Option<bool>,
    pub validation_failure_action: String,
    /// The Ready or Available condition's `status`, or `status.ready` spelled as True/False.
    pub ready: String,
    pub rule_count: usize,
    /// Kinds each rule's match applies to, clipped and de-duplicated.
    pub rule_kinds: Vec<String>,
    pub severity: String,
}

/// What one kind's list answered.
///
/// A 404 on the group is [`KindSet::NotServed`]: invisible, not broken. A 403
/// is [`KindSet::Denied`]. Those are different states on purpose; collapsing
/// them would tell someone Kyverno is absent when the account was refused.
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
    pub cluster_policies: KindSet,
    pub policies: KindSet,
    pub cleanup_policies: KindSet,
    pub cluster_cleanup_policies: KindSet,
    pub legacy_policy_exceptions: KindSet,
    pub validating_policies: KindSet,
    pub namespaced_validating_policies: KindSet,
    pub mutating_policies: KindSet,
    pub namespaced_mutating_policies: KindSet,
    pub generating_policies: KindSet,
    pub namespaced_generating_policies: KindSet,
    pub deleting_policies: KindSet,
    pub namespaced_deleting_policies: KindSet,
    pub image_validating_policies: KindSet,
    pub namespaced_image_validating_policies: KindSet,
    pub policy_exceptions: KindSet,
}

impl Inventory {
    /// False when both Kyverno groups answered 404.
    pub fn served(&self) -> bool {
        self.sets().iter().any(|(set, _)| set.served())
    }

    fn sets(&self) -> [(&KindSet, Kind); 16] {
        [
            (&self.cluster_policies, Kind::ClusterPolicy),
            (&self.policies, Kind::Policy),
            (&self.cleanup_policies, Kind::CleanupPolicy),
            (&self.cluster_cleanup_policies, Kind::ClusterCleanupPolicy),
            (&self.legacy_policy_exceptions, Kind::LegacyPolicyException),
            (&self.validating_policies, Kind::ValidatingPolicy),
            (
                &self.namespaced_validating_policies,
                Kind::NamespacedValidatingPolicy,
            ),
            (&self.mutating_policies, Kind::MutatingPolicy),
            (
                &self.namespaced_mutating_policies,
                Kind::NamespacedMutatingPolicy,
            ),
            (&self.generating_policies, Kind::GeneratingPolicy),
            (
                &self.namespaced_generating_policies,
                Kind::NamespacedGeneratingPolicy,
            ),
            (&self.deleting_policies, Kind::DeletingPolicy),
            (
                &self.namespaced_deleting_policies,
                Kind::NamespacedDeletingPolicy,
            ),
            (&self.image_validating_policies, Kind::ImageValidatingPolicy),
            (
                &self.namespaced_image_validating_policies,
                Kind::NamespacedImageValidatingPolicy,
            ),
            (&self.policy_exceptions, Kind::PolicyException),
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
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
struct WireSpec {
    #[serde(default)]
    background: Option<bool>,
    #[serde(default, rename = "validationFailureAction")]
    validation_failure_action: Value,
    #[serde(default, rename = "validationActions")]
    validation_actions: Value,
    #[serde(default)]
    rules: Vec<WireRule>,
    #[serde(default)]
    exceptions: Vec<Value>,
    #[serde(default, rename = "match")]
    match_resources: WireMatch,
    #[serde(default)]
    evaluation: WireEvaluation,
    #[serde(default, rename = "matchConstraints")]
    match_constraints: WireMatchConstraints,
    #[serde(default)]
    validations: Vec<Value>,
    #[serde(default)]
    mutations: Vec<Value>,
    #[serde(default)]
    generate: Vec<Value>,
    #[serde(default)]
    generations: Vec<Value>,
    #[serde(default)]
    deletions: Vec<Value>,
    #[serde(default)]
    conditions: Vec<Value>,
    #[serde(default, rename = "policyRefs")]
    policy_refs: Vec<Value>,
}

#[derive(Deserialize, Default)]
struct WireRule {
    #[serde(default, rename = "match")]
    match_resources: WireMatch,
}

#[derive(Deserialize, Default)]
struct WireMatch {
    #[serde(default)]
    any: Vec<WireFilter>,
    #[serde(default)]
    all: Vec<WireFilter>,
    #[serde(default)]
    resources: WireResources,
}

#[derive(Deserialize, Default)]
struct WireFilter {
    #[serde(default)]
    resources: WireResources,
}

#[derive(Deserialize, Default)]
struct WireResources {
    #[serde(default)]
    kinds: Vec<String>,
}

#[derive(Deserialize, Default)]
struct WireEvaluation {
    #[serde(default)]
    background: WireEnabled,
}

#[derive(Deserialize, Default)]
struct WireEnabled {
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Deserialize, Default)]
struct WireMatchConstraints {
    #[serde(default, rename = "resourceRules")]
    resource_rules: Vec<WireResourceRule>,
}

#[derive(Deserialize, Default)]
struct WireResourceRule {
    #[serde(default)]
    resources: Vec<String>,
    #[serde(default)]
    kinds: Vec<String>,
}

#[derive(Deserialize, Default)]
struct WireStatus {
    #[serde(default)]
    ready: Value,
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

fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn action_of(value: &Value) -> String {
    match value {
        Value::String(text) => clipped(text.clone()),
        Value::Array(items) => clipped(
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Value::Object(map) => map
            .get("type")
            .and_then(Value::as_str)
            .map(|text| clipped(text.to_string()))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn action_of_spec(kind: Kind, spec: &WireSpec) -> String {
    if kind.is_cel() {
        let actions = action_of(&spec.validation_actions);
        if !actions.is_empty() {
            return actions;
        }
    }
    action_of(&spec.validation_failure_action)
}

fn background_of(kind: Kind, spec: &WireSpec) -> Option<bool> {
    if kind.is_cel() {
        // evaluation.admission is an independent flag, not a fallback: a
        // policy with admission disabled is still background-enabled by
        // default. An unset background flag carries no tag.
        return spec.evaluation.background.enabled;
    }
    spec.background
}

fn ready_of(status: &WireStatus) -> String {
    for want in ["Ready", "Available"] {
        if let Some(condition) = status
            .conditions
            .iter()
            .find(|condition| condition.type_name == want)
        {
            return clipped(condition.status.clone());
        }
    }
    match &status.ready {
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => clipped(text.clone()),
        _ => String::new(),
    }
}

fn push_kinds(kinds: &[String], into: &mut Vec<String>) {
    for kind in kinds {
        if kind.is_empty() {
            continue;
        }
        let text = clipped(kind.clone());
        if into.iter().any(|have| have == &text) {
            continue;
        }
        if into.len() == MAX_RULE_KINDS {
            return;
        }
        into.push(text);
    }
}

fn kinds_from_match(matched: &WireMatch, into: &mut Vec<String>) {
    push_kinds(&matched.resources.kinds, into);
    for filter in matched.any.iter().chain(matched.all.iter()) {
        push_kinds(&filter.resources.kinds, into);
    }
}

fn cel_rule_count(spec: &WireSpec) -> usize {
    for count in [
        spec.validations.len(),
        spec.mutations.len(),
        spec.generate.len(),
        spec.generations.len(),
        spec.deletions.len(),
    ] {
        if count > 0 {
            return count;
        }
    }
    if !spec.conditions.is_empty() {
        return spec.conditions.len();
    }
    if !spec.policy_refs.is_empty() {
        return spec.policy_refs.len();
    }
    if spec.match_constraints.resource_rules.is_empty() {
        0
    } else {
        1
    }
}

fn cel_rule_kinds(spec: &WireSpec) -> Vec<String> {
    let mut kinds = Vec::new();
    for rule in &spec.match_constraints.resource_rules {
        push_kinds(&rule.resources, &mut kinds);
        push_kinds(&rule.kinds, &mut kinds);
    }
    kinds
}

fn rule_kinds_of(kind: Kind, spec: &WireSpec) -> (usize, Vec<String>) {
    if kind.is_cel() {
        return (cel_rule_count(spec), cel_rule_kinds(spec));
    }
    if kind == Kind::LegacyPolicyException {
        let mut kinds = Vec::new();
        kinds_from_match(&spec.match_resources, &mut kinds);
        return (spec.exceptions.len(), kinds);
    }
    if kind.is_cleanup() {
        let mut kinds = Vec::new();
        kinds_from_match(&spec.match_resources, &mut kinds);
        let count = if kinds.is_empty() && spec.match_resources.any.is_empty() {
            0
        } else {
            1
        };
        return (count, kinds);
    }
    let mut kinds = Vec::new();
    for rule in &spec.rules {
        kinds_from_match(&rule.match_resources, &mut kinds);
    }
    (spec.rules.len(), kinds)
}

fn from_wire(kind: Kind, version: &str, wire: WireObject) -> Option<Resource> {
    if wire.metadata.name.is_empty() {
        return None;
    }
    let (rule_count, rule_kinds) = rule_kinds_of(kind, &wire.spec);
    let severity = wire
        .metadata
        .annotations
        .get(SEVERITY_ANNOTATION)
        .cloned()
        .unwrap_or_default();
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
        background: background_of(kind, &wire.spec),
        validation_failure_action: action_of_spec(kind, &wire.spec),
        ready: ready_of(&wire.status),
        rule_count,
        rule_kinds,
        severity: clipped(severity),
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

fn versions_for(kind: Kind, group_versions: &[String]) -> Vec<String> {
    if kind.is_cleanup() {
        return group_versions
            .iter()
            .filter(|version| version.starts_with("v2"))
            .cloned()
            .collect();
    }
    let mut out = group_versions.to_vec();
    let fallbacks: &[&str] = if kind == Kind::LegacyPolicyException {
        LEGACY_EXCEPTION_FALLBACKS
    } else if kind.is_cel() {
        CEL_FALLBACKS
    } else {
        // Only the classic ClusterPolicy and Policy reach here; both are v1.
        &["v1"]
    };
    for fallback in fallbacks {
        if !out.iter().any(|have| have == fallback) {
            out.push((*fallback).to_string());
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

fn take_set(sets: &mut Vec<(Kind, KindSet)>, kind: Kind) -> KindSet {
    sets.iter()
        .position(|(have, _)| *have == kind)
        .map(|index| sets.swap_remove(index).1)
        .unwrap_or_default()
}

fn inventory_from(mut legacy: Vec<(Kind, KindSet)>, mut cel: Vec<(Kind, KindSet)>) -> Inventory {
    Inventory {
        cluster_policies: take_set(&mut legacy, Kind::ClusterPolicy),
        policies: take_set(&mut legacy, Kind::Policy),
        cleanup_policies: take_set(&mut legacy, Kind::CleanupPolicy),
        cluster_cleanup_policies: take_set(&mut legacy, Kind::ClusterCleanupPolicy),
        legacy_policy_exceptions: take_set(&mut legacy, Kind::LegacyPolicyException),
        validating_policies: take_set(&mut cel, Kind::ValidatingPolicy),
        namespaced_validating_policies: take_set(&mut cel, Kind::NamespacedValidatingPolicy),
        mutating_policies: take_set(&mut cel, Kind::MutatingPolicy),
        namespaced_mutating_policies: take_set(&mut cel, Kind::NamespacedMutatingPolicy),
        generating_policies: take_set(&mut cel, Kind::GeneratingPolicy),
        namespaced_generating_policies: take_set(&mut cel, Kind::NamespacedGeneratingPolicy),
        deleting_policies: take_set(&mut cel, Kind::DeletingPolicy),
        namespaced_deleting_policies: take_set(&mut cel, Kind::NamespacedDeletingPolicy),
        image_validating_policies: take_set(&mut cel, Kind::ImageValidatingPolicy),
        namespaced_image_validating_policies: take_set(
            &mut cel,
            Kind::NamespacedImageValidatingPolicy,
        ),
        policy_exceptions: take_set(&mut cel, Kind::PolicyException),
    }
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
    let versions = versions_for(kind, group_versions);
    if versions.is_empty() {
        return Ok(KindSet::NotServed);
    }
    for version in versions {
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
    namespace: Option<&str>,
) -> Result<Vec<(Kind, KindSet)>, Fetched<Inventory>> {
    let kinds: &[Kind] = if group == CEL_GROUP {
        &CEL_KINDS
    } else {
        &LEGACY_KINDS
    };
    match probe_group(client, group).await {
        GroupAnswer::NotServed => Ok(kinds
            .iter()
            .map(|kind| (*kind, KindSet::NotServed))
            .collect()),
        GroupAnswer::Denied => Ok(kinds.iter().map(|kind| (*kind, KindSet::Denied)).collect()),
        GroupAnswer::Failed(why) => Err(Fetched::Failed {
            what: "kyverno",
            why,
        }),
        GroupAnswer::Served(versions) => {
            let mut sets = Vec::with_capacity(kinds.len());
            for kind in kinds {
                sets.push((*kind, list_kind(client, *kind, &versions, namespace).await?));
            }
            Ok(sets)
        }
    }
}

/// List Kyverno policy CRs. Served when either group answers. A missing pair
/// of groups is invisible; a forbidden one is Denied on that group's kinds
/// and does not hide the other.
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let legacy = match fetch_group(client, GROUP, namespace).await {
        Ok(sets) => sets,
        Err(failed) => return failed,
    };
    let cel = match fetch_group(client, CEL_GROUP, namespace).await {
        Ok(sets) => sets,
        Err(failed) => return failed,
    };
    Fetched::Ok(inventory_from(legacy, cel))
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        return word.to_string();
    }
    if word == "policy" {
        return "policies".to_string();
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

fn kinds_label(kinds: &[String]) -> String {
    clipped(kinds.join(", "))
}

fn object_label(item: &Resource) -> String {
    if item.namespace.is_empty() {
        item.name.clone()
    } else {
        format!("{}/{}", item.namespace, item.name)
    }
}

/// Native list rows. `None` when both groups answered 404, so a UI stays
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
        "Action",
        "Rules",
        "Kinds",
        "Severity",
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
                    // what(), not as_str(): PolicyException names two kinds,
                    // one per group, and a denied row per kind needs its own uid.
                    uid: format!("denied:{}", kind.what()),
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
                            item.validation_failure_action.clone(),
                            item.rule_count.to_string(),
                            kinds_label(&item.rule_kinds),
                            item.severity.clone(),
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
            "Kyverno is not served by this cluster".to_string(),
            String::new(),
            "this reads ClusterPolicy and Policy CRs the controller already \
             publishes, plus ValidatingPolicy and the other CEL kinds on \
             policies.kyverno.io when that group is served. CleanupPolicy is \
             listed only when kyverno.io names a v2 version. nothing is \
             installed to find them, so a cluster without Kyverno shows as \
             empty here. Policy results are PolicyReport and OpenReports, \
             not this listing"
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
        lines.push("no Kyverno policies are stored in this cluster".to_string());
    } else if total == 0 && unreadable > 0 {
        lines.push(
            "no Kyverno policy could be read here, though some are stored: every object this \
             account can see failed to decode"
                .to_string(),
        );
    } else if total > 0 {
        lines.push(format!("{} Kyverno {}", total, plural(total, "policy")));
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
            "{} Kyverno {} could not be decoded and {} not shown",
            unreadable,
            plural(unreadable, "policy"),
            if unreadable == 1 { "is" } else { "are" },
        ));
    }
    for (set, _) in &sets {
        for item in set.items() {
            lines.push(String::new());
            lines.push(object_label(item));
            let mut line = format!(
                "  {}  {}  {} rules",
                item.kind.as_str(),
                ready_label(&item.ready),
                item.rule_count
            );
            if let Some(background) = item.background {
                line.push_str(if background {
                    "  background"
                } else {
                    "  admission-only"
                });
            }
            if !item.validation_failure_action.is_empty() {
                line.push_str("  ");
                line.push_str(&item.validation_failure_action);
            }
            let kinds = kinds_label(&item.rule_kinds);
            if !kinds.is_empty() {
                line.push_str("  ");
                line.push_str(&kinds);
            }
            if !item.severity.is_empty() {
                line.push_str("  ");
                line.push_str(&item.severity);
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "kyverno_test.rs"]
mod tests;
