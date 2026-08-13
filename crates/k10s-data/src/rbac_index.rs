//! RBAC explorer: who is bound to what, built from Roles and Bindings.
//!
//! [`crate::rbac`] asks the API server what *this* account may do, through
//! `SelfSubjectRulesReview`. This module asks a different question of different
//! objects: it lists Roles, ClusterRoles, RoleBindings and ClusterRoleBindings
//! and joins them client-side into a subject × verb × resource × namespace
//! relation. Forward lookup is "what can subject S do?". Reverse lookup is
//! "who can delete secrets in `prod`?", and a ClusterRoleBinding still applies
//! in that namespace.
//!
//! The join is the objects as listed, not the authorizer. Admission webhooks
//! are not evaluated. Implicit groups (`system:authenticated`,
//! `system:serviceaccounts`) are not expanded: a binding that names the group
//! is the answer, not every User. ClusterRole aggregation selectors are not
//! walked; the `rules` already on the object are what the controller wrote, and
//! empty rules grant nothing. Non-resource URLs (`/healthz`, `/api`) are
//! skipped. A ServiceAccount subject with no namespace inherits the
//! RoleBinding's namespace; on a ClusterRoleBinding it is invalid and dropped.
//! The User spelling `system:serviceaccount:ns:name` is treated as the same
//! subject as that ServiceAccount.
//!
//! Four lists, paged, each capped at [`MAX_OBJECTS`]. Reaching the cap, or a
//! continue token the cap refused to follow, is [`Index::incomplete`]: some of
//! the relation, labelled, never silently the whole of it. A 401 or 403 is
//! [`Fetched::Denied`]. A group the server does not serve (404 on every list)
//! is [`Index::served`] false, which is absence, not an empty cluster.

use std::collections::{BTreeSet, HashMap};

use k8s_openapi::api::rbac::v1::{
    ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, RoleRef, Subject,
};
use kube::Client;
use kube::api::{Api, ListParams, Resource};
use serde::de::DeserializeOwned;

use crate::read::Fetched;

/// Ceiling on each of the four lists. Crossing it is incompleteness, not a
/// quieter listing.
pub const MAX_OBJECTS: usize = 5_000;

const PAGE_LIMIT: u32 = 500;
const SA_USER_PREFIX: &str = "system:serviceaccount:";
const COLLECTION_VERBS: &[&str] = &["list", "watch", "deletecollection"];
const RBAC_GROUP: &str = "rbac.authorization.k8s.io";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SubjectKind {
    User,
    Group,
    ServiceAccount,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubjectRef {
    pub kind: SubjectKind,
    pub name: String,
    /// Set for ServiceAccounts; ignored for User and Group.
    pub namespace: Option<String>,
}

impl SubjectRef {
    pub fn user(name: impl Into<String>) -> SubjectRef {
        SubjectRef {
            kind: SubjectKind::User,
            name: name.into(),
            namespace: None,
        }
    }

    pub fn group(name: impl Into<String>) -> SubjectRef {
        SubjectRef {
            kind: SubjectKind::Group,
            name: name.into(),
            namespace: None,
        }
    }

    pub fn service_account(namespace: impl Into<String>, name: impl Into<String>) -> SubjectRef {
        SubjectRef {
            kind: SubjectKind::ServiceAccount,
            name: name.into(),
            namespace: Some(namespace.into()),
        }
    }
}

/// One cell of the relation, spelled the way the Role wrote it (`*` stays `*`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Permission {
    pub verb: String,
    pub api_group: String,
    pub resource: String,
    /// Empty means every name. A non-empty list is an exact whitelist: `*` is
    /// not a wildcard here.
    pub resource_names: Vec<String>,
    /// `None` is cluster-wide (a ClusterRoleBinding). `Some` is the
    /// RoleBinding's namespace.
    pub namespace: Option<String>,
    pub role: String,
    pub binding: String,
}

/// Roles and Bindings already fetched, so the join can be tested without a
/// server.
#[derive(Debug, Clone, Default)]
pub struct Documents {
    pub roles: Vec<Role>,
    pub cluster_roles: Vec<ClusterRole>,
    pub role_bindings: Vec<RoleBinding>,
    pub cluster_role_bindings: Vec<ClusterRoleBinding>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub served: bool,
    pub incomplete: bool,
    grants: Vec<Grant>,
}

impl Index {
    pub fn unserved() -> Index {
        Index {
            served: false,
            incomplete: false,
            grants: Vec::new(),
        }
    }

    pub fn from_documents(docs: &Documents) -> Index {
        let mut roles: HashMap<(String, String), Vec<Rule>> = HashMap::new();
        for role in &docs.roles {
            let Some(name) = role.metadata.name.as_deref() else {
                continue;
            };
            let ns = role.metadata.namespace.clone().unwrap_or_default();
            roles.insert((ns, name.to_string()), rules_from(role.rules.as_deref()));
        }
        let mut cluster_roles: HashMap<String, Vec<Rule>> = HashMap::new();
        for role in &docs.cluster_roles {
            let Some(name) = role.metadata.name.as_deref() else {
                continue;
            };
            cluster_roles.insert(name.to_string(), rules_from(role.rules.as_deref()));
        }

        let mut grants = Vec::new();
        for binding in &docs.role_bindings {
            let Some(ns) = binding.metadata.namespace.as_deref() else {
                continue;
            };
            let Some(name) = binding.metadata.name.as_deref() else {
                continue;
            };
            let Some(rules) = resolve_role(&binding.role_ref, Some(ns), &roles, &cluster_roles)
            else {
                continue;
            };
            let subjects = subjects_from(binding.subjects.as_deref(), Some(ns));
            if subjects.is_empty() || rules.is_empty() {
                continue;
            }
            grants.push(Grant {
                subjects,
                rules,
                namespace: Some(ns.to_string()),
                role: binding.role_ref.name.clone(),
                binding: name.to_string(),
            });
        }
        for binding in &docs.cluster_role_bindings {
            let Some(name) = binding.metadata.name.as_deref() else {
                continue;
            };
            if binding.role_ref.kind != "ClusterRole" {
                continue;
            }
            let Some(rules) = resolve_role(&binding.role_ref, None, &roles, &cluster_roles) else {
                continue;
            };
            let subjects = subjects_from(binding.subjects.as_deref(), None);
            if subjects.is_empty() || rules.is_empty() {
                continue;
            }
            grants.push(Grant {
                subjects,
                rules,
                namespace: None,
                role: binding.role_ref.name.clone(),
                binding: name.to_string(),
            });
        }
        Index {
            served: true,
            incomplete: docs.truncated,
            grants,
        }
    }

    /// What subject `S` is bound to do, as the Roles spelled it.
    pub fn what_can(&self, subject: &SubjectRef) -> Vec<Permission> {
        if !self.served {
            return Vec::new();
        }
        let aliases = aliases(subject);
        let mut out = Vec::new();
        for grant in &self.grants {
            if !grant.subjects.iter().any(|bound| aliases.contains(bound)) {
                continue;
            }
            push_permissions(&mut out, grant);
        }
        out.sort();
        out.dedup();
        out
    }

    /// Who can `verb` `resource` in `namespace`. `group` is `""` for core.
    /// `namespace` `None` asks about cluster-scoped access; `Some` asks about
    /// that namespace, and ClusterRoleBindings still apply.
    pub fn who_can(
        &self,
        verb: &str,
        group: &str,
        resource: &str,
        namespace: Option<&str>,
    ) -> Vec<SubjectRef> {
        if !self.served {
            return Vec::new();
        }
        let mut out = BTreeSet::new();
        for grant in &self.grants {
            if !scope_applies(grant.namespace.as_deref(), namespace) {
                continue;
            }
            if !grant
                .rules
                .iter()
                .any(|rule| rule_matches(rule, verb, group, resource, NameQuery::Any))
            {
                continue;
            }
            out.extend(grant.subjects.iter().cloned());
        }
        out.into_iter().collect()
    }

    /// Whether `subject` is bound to `verb` this `resource` in this scope.
    /// `name` `None` asks for an unrestricted grant; `Some` also matches a
    /// whitelist that names that object. Named grants never satisfy a
    /// collection verb.
    pub fn allows(
        &self,
        subject: &SubjectRef,
        verb: &str,
        group: &str,
        resource: &str,
        namespace: Option<&str>,
        name: Option<&str>,
    ) -> bool {
        if !self.served {
            return false;
        }
        let aliases = aliases(subject);
        let query = match name {
            Some(name) => NameQuery::This(name),
            None => NameQuery::Unrestricted,
        };
        self.grants.iter().any(|grant| {
            scope_applies(grant.namespace.as_deref(), namespace)
                && grant.subjects.iter().any(|bound| aliases.contains(bound))
                && grant
                    .rules
                    .iter()
                    .any(|rule| rule_matches(rule, verb, group, resource, query))
        })
    }
}

/// List the four RBAC kinds and join them. The SelfSubjectRulesReview probe is
/// a different module; this does not call it.
pub async fn fetch(client: &Client) -> Fetched<Index> {
    let (roles, cluster_roles, role_bindings, cluster_role_bindings) = tokio::join!(
        list_kind::<Role>(client),
        list_kind::<ClusterRole>(client),
        list_kind::<RoleBinding>(client),
        list_kind::<ClusterRoleBinding>(client),
    );
    match decide([
        roles.status(),
        cluster_roles.status(),
        role_bindings.status(),
        cluster_role_bindings.status(),
    ]) {
        Decision::Denied => Fetched::Denied { what: "rbac" },
        Decision::NotServed => Fetched::Ok(Index::unserved()),
        Decision::Failed(why) => Fetched::Failed { what: "rbac", why },
        Decision::Ok { incomplete } => {
            let truncated = incomplete
                || roles.truncated()
                || cluster_roles.truncated()
                || role_bindings.truncated()
                || cluster_role_bindings.truncated();
            Fetched::Ok(Index::from_documents(&Documents {
                roles: roles.into_items(),
                cluster_roles: cluster_roles.into_items(),
                role_bindings: role_bindings.into_items(),
                cluster_role_bindings: cluster_role_bindings.into_items(),
                truncated,
            }))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    subjects: Vec<SubjectRef>,
    rules: Vec<Rule>,
    namespace: Option<String>,
    role: String,
    binding: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    verbs: Vec<String>,
    groups: Vec<String>,
    resources: Vec<String>,
    names: Vec<String>,
}

#[derive(Clone, Copy)]
enum NameQuery<'a> {
    Any,
    Unrestricted,
    This(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WireStatus {
    Ok { truncated: bool },
    Denied,
    NotServed,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Denied,
    NotServed,
    Failed(String),
    Ok { incomplete: bool },
}

enum ListOutcome<T> {
    Ok { items: Vec<T>, truncated: bool },
    Denied,
    NotServed,
    Failed(String),
}

impl<T> ListOutcome<T> {
    fn from_error(error: &kube::Error) -> ListOutcome<T> {
        match interpret(error) {
            ListError::Denied => ListOutcome::Denied,
            ListError::NotServed => ListOutcome::NotServed,
            ListError::Failed(why) => ListOutcome::Failed(why),
        }
    }

    fn status(&self) -> WireStatus {
        match self {
            ListOutcome::Ok { truncated, .. } => WireStatus::Ok {
                truncated: *truncated,
            },
            ListOutcome::Denied => WireStatus::Denied,
            ListOutcome::NotServed => WireStatus::NotServed,
            ListOutcome::Failed(why) => WireStatus::Failed(why.clone()),
        }
    }

    fn truncated(&self) -> bool {
        matches!(
            self,
            ListOutcome::Ok {
                truncated: true,
                ..
            }
        )
    }

    fn into_items(self) -> Vec<T> {
        match self {
            ListOutcome::Ok { items, .. } => items,
            _ => Vec::new(),
        }
    }
}

fn decide(parts: [WireStatus; 4]) -> Decision {
    if parts.iter().any(|part| matches!(part, WireStatus::Denied)) {
        return Decision::Denied;
    }
    for part in &parts {
        if let WireStatus::Failed(why) = part {
            return Decision::Failed(why.clone());
        }
    }
    if parts
        .iter()
        .all(|part| matches!(part, WireStatus::NotServed))
    {
        return Decision::NotServed;
    }
    let incomplete = parts.iter().any(|part| match part {
        WireStatus::Ok { truncated } => *truncated,
        WireStatus::NotServed => true,
        _ => false,
    });
    Decision::Ok { incomplete }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ListError {
    Denied,
    NotServed,
    Failed(String),
}

fn interpret(error: &kube::Error) -> ListError {
    match error {
        kube::Error::Api(response) if matches!(response.code, 401 | 403) => ListError::Denied,
        kube::Error::Api(response) if response.code == 404 => ListError::NotServed,
        error => ListError::Failed(crate::connect::describe(
            error as &(dyn std::error::Error + 'static),
        )),
    }
}

async fn list_kind<K>(client: &Client) -> ListOutcome<K>
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug,
    <K as Resource>::DynamicType: Default,
{
    let api: Api<K> = Api::all(client.clone());
    let mut items = Vec::new();
    let mut token: Option<String> = None;
    let mut truncated = false;
    loop {
        if items.len() >= MAX_OBJECTS {
            break;
        }
        let remaining = MAX_OBJECTS - items.len();
        let mut params = ListParams::default().limit(PAGE_LIMIT.min(remaining as u32));
        if let Some(token) = token.as_deref() {
            params = params.continue_token(token);
        }
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(error) => return ListOutcome::from_error(&error),
        };
        let more = page
            .metadata
            .continue_
            .as_deref()
            .is_some_and(|token| !token.is_empty());
        for item in page.items {
            if items.len() >= MAX_OBJECTS {
                truncated = true;
                break;
            }
            items.push(item);
        }
        if truncated {
            break;
        }
        if more && items.len() >= MAX_OBJECTS {
            truncated = true;
            break;
        }
        token = more
            .then(|| page.metadata.continue_.clone())
            .flatten()
            .filter(|token| !token.is_empty());
        if token.is_none() {
            break;
        }
    }
    ListOutcome::Ok { items, truncated }
}

fn rules_from(rules: Option<&[PolicyRule]>) -> Vec<Rule> {
    let Some(rules) = rules else {
        return Vec::new();
    };
    rules
        .iter()
        .filter(|rule| {
            rule.non_resource_urls
                .as_ref()
                .is_none_or(|urls| urls.is_empty())
                && rule
                    .resources
                    .as_ref()
                    .is_some_and(|resources| !resources.is_empty())
        })
        .map(|rule| Rule {
            verbs: rule.verbs.clone(),
            groups: rule.api_groups.clone().unwrap_or_default(),
            resources: rule.resources.clone().unwrap_or_default(),
            names: rule.resource_names.clone().unwrap_or_default(),
        })
        .filter(|rule| !rule.verbs.is_empty() && !rule.resources.is_empty())
        .collect()
}

fn resolve_role(
    role_ref: &RoleRef,
    binding_namespace: Option<&str>,
    roles: &HashMap<(String, String), Vec<Rule>>,
    cluster_roles: &HashMap<String, Vec<Rule>>,
) -> Option<Vec<Rule>> {
    if !role_ref_group_ok(role_ref) {
        return None;
    }
    match (role_ref.kind.as_str(), binding_namespace) {
        ("Role", Some(ns)) => roles.get(&(ns.to_string(), role_ref.name.clone())).cloned(),
        ("ClusterRole", _) => cluster_roles.get(&role_ref.name).cloned(),
        _ => None,
    }
}

fn role_ref_group_ok(role_ref: &RoleRef) -> bool {
    match role_ref.api_group.as_str() {
        "" | RBAC_GROUP => true,
        _ => false,
    }
}

fn subjects_from(subjects: Option<&[Subject]>, binding_namespace: Option<&str>) -> Vec<SubjectRef> {
    let Some(subjects) = subjects else {
        return Vec::new();
    };
    subjects
        .iter()
        .filter_map(|subject| subject_ref(subject, binding_namespace))
        .collect()
}

fn subject_ref(subject: &Subject, binding_namespace: Option<&str>) -> Option<SubjectRef> {
    if subject.name.is_empty() {
        return None;
    }
    match subject.kind.as_str() {
        "User" => {
            if subject
                .namespace
                .as_deref()
                .is_some_and(|ns| !ns.is_empty())
            {
                return None;
            }
            Some(SubjectRef::user(&subject.name))
        }
        "Group" => {
            if subject
                .namespace
                .as_deref()
                .is_some_and(|ns| !ns.is_empty())
            {
                return None;
            }
            Some(SubjectRef::group(&subject.name))
        }
        "ServiceAccount" => {
            let namespace = subject
                .namespace
                .as_deref()
                .filter(|ns| !ns.is_empty())
                .or(binding_namespace)?;
            Some(SubjectRef::service_account(namespace, &subject.name))
        }
        _ => None,
    }
}

fn aliases(subject: &SubjectRef) -> Vec<SubjectRef> {
    let mut out = vec![subject.clone()];
    match subject.kind {
        SubjectKind::ServiceAccount => {
            if let Some(namespace) = &subject.namespace {
                out.push(SubjectRef::user(format!(
                    "{SA_USER_PREFIX}{namespace}:{}",
                    subject.name
                )));
            }
        }
        SubjectKind::User => {
            if let Some(rest) = subject.name.strip_prefix(SA_USER_PREFIX)
                && let Some((namespace, name)) = rest.split_once(':')
                && !namespace.is_empty()
                && !name.is_empty()
                && !name.contains(':')
            {
                out.push(SubjectRef::service_account(namespace, name));
            }
        }
        SubjectKind::Group => {}
    }
    out
}

fn scope_applies(grant_namespace: Option<&str>, query_namespace: Option<&str>) -> bool {
    match (grant_namespace, query_namespace) {
        (None, _) => true,
        (Some(grant), Some(query)) => grant == query,
        (Some(_), None) => false,
    }
}

fn rule_matches(
    rule: &Rule,
    verb: &str,
    group: &str,
    resource: &str,
    names: NameQuery<'_>,
) -> bool {
    star_or_eq(&rule.verbs, verb)
        && star_or_eq(&rule.groups, group)
        && resource_matches(&rule.resources, resource)
        && names_allow(&rule.names, verb, names)
}

fn star_or_eq(patterns: &[String], value: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| pattern == "*" || pattern == value)
}

fn resource_matches(patterns: &[String], resource: &str) -> bool {
    patterns.iter().any(|pattern| {
        if pattern == resource {
            return true;
        }
        if pattern == "*" {
            return !resource.contains('/');
        }
        if let Some(prefix) = pattern.strip_suffix("/*") {
            return !prefix.is_empty()
                && resource.starts_with(prefix)
                && resource.as_bytes().get(prefix.len()) == Some(&b'/');
        }
        if let Some(suffix) = pattern.strip_prefix("*/") {
            return !suffix.is_empty() && resource.ends_with(&format!("/{suffix}"));
        }
        false
    })
}

fn names_allow(names: &[String], verb: &str, query: NameQuery<'_>) -> bool {
    if names.is_empty() {
        return true;
    }
    if is_collection_verb(verb) {
        return false;
    }
    match query {
        NameQuery::Any => true,
        NameQuery::Unrestricted => false,
        NameQuery::This(want) => names.iter().any(|name| name == want),
    }
}

fn is_collection_verb(verb: &str) -> bool {
    COLLECTION_VERBS.contains(&verb)
}

fn push_permissions(out: &mut Vec<Permission>, grant: &Grant) {
    for rule in &grant.rules {
        for verb in &rule.verbs {
            for api_group in &rule.groups {
                for resource in &rule.resources {
                    out.push(Permission {
                        verb: verb.clone(),
                        api_group: api_group.clone(),
                        resource: resource.clone(),
                        resource_names: rule.names.clone(),
                        namespace: grant.namespace.clone(),
                        role: grant.role.clone(),
                        binding: grant.binding.clone(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "rbac_index_test.rs"]
mod tests;
