use std::collections::HashMap;

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    SelfSubjectRulesReview, SelfSubjectRulesReviewSpec, SubjectRulesReviewStatus,
};
use k10s_core::{Capability, KindId};
use kube::api::PostParams;
use kube::{Api, Client};

use crate::discover::{KindTarget, WatchTarget};

const COLLECTION_VERBS: &[&str] = &["list", "watch", "deletecollection"];

fn is_collection_verb(verb: &str) -> bool {
    COLLECTION_VERBS.contains(&verb)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    groups: Vec<String>,
    resources: Vec<String>,
    verbs: Vec<String>,
    names: Vec<String>,
}

fn matches(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|p| p == "*" || p == value)
}

#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
    incomplete: bool,
    unanswered: bool,
}

impl RuleSet {
    pub fn from_status(status: &SubjectRulesReviewStatus) -> RuleSet {
        RuleSet {
            rules: status
                .resource_rules
                .iter()
                .map(|r| Rule {
                    groups: r.api_groups.clone().unwrap_or_default(),
                    resources: r.resources.clone().unwrap_or_default(),
                    verbs: r.verbs.clone(),
                    names: r.resource_names.clone().unwrap_or_default(),
                })
                .collect(),
            incomplete: status.incomplete,
            unanswered: false,
        }
    }

    // A rules review that got no answer must behave like an incomplete one:
    // the namespace stays in every fallback so its kinds are attempted, and a
    // real denial then surfaces as a labelled stream error instead of the
    // namespace silently vanishing from the map.
    pub fn unanswered() -> RuleSet {
        RuleSet {
            rules: Vec::new(),
            incomplete: true,
            unanswered: true,
        }
    }

    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    pub fn is_unanswered(&self) -> bool {
        self.unanswered
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn allows(&self, group: &str, resource: &str, verb: &str) -> bool {
        self.rules.iter().any(|r| {
            (r.names.is_empty() || !is_collection_verb(verb))
                && matches(&r.groups, group)
                && matches(&r.resources, resource)
                && matches(&r.verbs, verb)
        })
    }

    pub fn allows_reflection(&self, group: &str, resource: &str) -> bool {
        self.allows(group, resource, "list") && self.allows(group, resource, "watch")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScope {
    All,
    Namespaces(Vec<String>),
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Allowed,
    Denied,
    Unanswered,
}

impl Answer {
    fn and(self, other: Answer) -> Answer {
        match (self, other) {
            (Answer::Denied, _) | (_, Answer::Denied) => Answer::Denied,
            (Answer::Unanswered, _) | (_, Answer::Unanswered) => Answer::Unanswered,
            (Answer::Allowed, Answer::Allowed) => Answer::Allowed,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Access {
    cluster_wide: HashMap<KindId, Answer>,
    per_namespace: Vec<(String, RuleSet)>,
    pub degraded: bool,
    pub requests: u32,
}

impl Access {
    pub fn unprobed() -> Access {
        Access {
            degraded: true,
            ..Access::default()
        }
    }

    pub fn namespaces(&self) -> impl Iterator<Item = &str> {
        self.per_namespace.iter().map(|(ns, _)| ns.as_str())
    }

    pub fn unanswered_namespaces(&self) -> impl Iterator<Item = &str> {
        self.per_namespace
            .iter()
            .filter(|(_, rules)| rules.is_unanswered())
            .map(|(ns, _)| ns.as_str())
    }

    pub fn unanswered(&self) -> usize {
        self.cluster_wide
            .values()
            .filter(|answer| **answer == Answer::Unanswered)
            .count()
    }

    pub fn rules(&self, namespace: &str) -> Option<&RuleSet> {
        self.per_namespace
            .iter()
            .find(|(ns, _)| ns == namespace)
            .map(|(_, r)| r)
    }

    pub fn scope_for(&self, target: &KindTarget) -> WatchScope {
        if self.degraded {
            return WatchScope::All;
        }
        let answer = self
            .cluster_wide
            .get(&target.id)
            .copied()
            .unwrap_or(Answer::Unanswered);
        match answer {
            Answer::Allowed | Answer::Unanswered => WatchScope::All,
            Answer::Denied if !target.namespaced => WatchScope::Denied,
            Answer::Denied => {
                let allowed: Vec<String> = self
                    .per_namespace
                    .iter()
                    .filter(|(_, rules)| {
                        rules.allows_reflection(target.group(), target.plural())
                            || rules.is_incomplete()
                    })
                    .map(|(ns, _)| ns.clone())
                    .collect();
                if allowed.is_empty() {
                    WatchScope::Denied
                } else {
                    WatchScope::Namespaces(allowed)
                }
            }
        }
    }

    pub fn verdict(&self, target: &KindTarget) -> Capability {
        if !target.listable || !target.watchable {
            return Capability::Absent;
        }
        match self.scope_for(target) {
            WatchScope::Denied => Capability::Forbidden,
            _ => Capability::Watchable,
        }
    }

    /// Patch, delete, and create as the rules review named them.
    ///
    /// List/watch is a different question: a reader who can list Deployments
    /// still cannot scale them. Incomplete or unanswered rules stay a maybe,
    /// so the click is attempted and a 403 still arrives as Denied.
    pub fn day2_caps(&self, target: &KindTarget) -> crate::day2::Caps {
        crate::day2::Caps {
            patch: self.allows_verb(target, "patch"),
            delete: self.allows_verb(target, "delete"),
            create: self.allows_verb(target, "create"),
        }
    }

    pub fn allows_verb(&self, target: &KindTarget, verb: &str) -> bool {
        if self.degraded {
            return true;
        }
        if self.per_namespace.iter().any(|(_, rules)| {
            rules.allows(target.group(), target.plural(), verb)
                || rules.is_incomplete()
                || rules.is_unanswered()
        }) {
            return true;
        }
        if self.per_namespace.is_empty() {
            return !matches!(self.verdict(target), Capability::Forbidden);
        }
        false
    }
}

pub async fn probe(client: &Client, targets: &[WatchTarget], namespaces: &[String]) -> Access {
    let mut access = Access::default();

    // Every review is independent, so the whole probe is one round trip deep
    // instead of namespaces + 2 x kinds serial ones on the cold-start path.
    let rules_api: Api<SelfSubjectRulesReview> = Api::all(client.clone());
    access.requests += namespaces.len() as u32;
    let rule_sets = futures::future::join_all(namespaces.iter().map(|ns| {
        let api = rules_api.clone();
        async move { api.create(&PostParams::default(), &rules_review(ns)).await }
    }))
    .await;
    let mut rules_failed = 0usize;
    for (ns, outcome) in namespaces.iter().zip(rule_sets) {
        let rules = match outcome {
            Ok(review) => RuleSet::from_status(&review.status.unwrap_or_default()),
            Err(_) => {
                rules_failed += 1;
                RuleSet::unanswered()
            }
        };
        access.per_namespace.push((ns.clone(), rules));
    }

    let access_api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    access.requests += 2 * targets.len() as u32;
    let answers = futures::future::join_all(targets.iter().map(|want| {
        let api = access_api.clone();
        async move {
            let ask = |verb: &'static str| {
                let api = api.clone();
                async move {
                    match api
                        .create(&PostParams::default(), &access_review(&want.target, verb))
                        .await
                    {
                        Ok(review) if review.status.as_ref().is_some_and(|s| s.allowed) => {
                            (Answer::Allowed, 0u32)
                        }
                        Ok(_) => (Answer::Denied, 0),
                        Err(_) => (Answer::Unanswered, 1),
                    }
                }
            };
            let ((list, list_failed), (watch, watch_failed)) =
                futures::join!(ask("list"), ask("watch"));
            (want.target.id, list.and(watch), list_failed + watch_failed)
        }
    }))
    .await;
    let mut reviews_failed = 0u32;
    for (kind, answer, failed) in answers {
        reviews_failed += failed;
        access.cluster_wide.insert(kind, answer);
    }

    let answered = access
        .cluster_wide
        .values()
        .filter(|answer| **answer != Answer::Unanswered)
        .count();
    if reviews_failed > 0 && answered == 0 {
        access.degraded = true;
    }
    if rules_failed > 0 && rules_failed == namespaces.len() {
        access.degraded = true;
    }
    access
}

fn rules_review(namespace: &str) -> SelfSubjectRulesReview {
    SelfSubjectRulesReview {
        metadata: Default::default(),
        spec: SelfSubjectRulesReviewSpec {
            namespace: Some(namespace.to_string()),
        },
        status: None,
    }
}

fn access_review(target: &KindTarget, verb: &str) -> SelfSubjectAccessReview {
    SelfSubjectAccessReview {
        metadata: Default::default(),
        spec: SelfSubjectAccessReviewSpec {
            non_resource_attributes: None,
            resource_attributes: Some(ResourceAttributes {
                group: Some(target.group().to_string()),
                resource: Some(target.plural().to_string()),
                verb: Some(verb.to_string()),
                version: Some(target.resource.version.clone()),
                namespace: None,
                name: None,
                subresource: None,
                field_selector: None,
                label_selector: None,
            }),
        },
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::authorization::v1::ResourceRule;
    use k10s_core::Role;
    use kube::discovery::ApiResource;

    fn rule(groups: &[&str], resources: &[&str], verbs: &[&str], names: &[&str]) -> ResourceRule {
        ResourceRule {
            api_groups: Some(groups.iter().map(|s| (*s).to_string()).collect()),
            resources: Some(resources.iter().map(|s| (*s).to_string()).collect()),
            verbs: verbs.iter().map(|s| (*s).to_string()).collect(),
            resource_names: if names.is_empty() {
                None
            } else {
                Some(names.iter().map(|s| (*s).to_string()).collect())
            },
        }
    }

    fn ruleset(rules: Vec<ResourceRule>, incomplete: bool) -> RuleSet {
        RuleSet::from_status(&SubjectRulesReviewStatus {
            evaluation_error: None,
            incomplete,
            non_resource_rules: Vec::new(),
            resource_rules: rules,
        })
    }

    fn target(group: &str, kind: &str, plural: &str, namespaced: bool, id: KindId) -> KindTarget {
        KindTarget {
            id,
            resource: ApiResource {
                group: group.to_string(),
                version: "v1".to_string(),
                api_version: if group.is_empty() {
                    "v1".into()
                } else {
                    format!("{group}/v1")
                },
                kind: kind.to_string(),
                plural: plural.to_string(),
            },
            role: Role::Owner,
            namespaced,
            listable: true,
            watchable: true,
            patchable: true,
            status_subresource: false,
        }
    }

    fn pods() -> KindTarget {
        target("", "Pod", "pods", true, KindId::POD)
    }

    fn nodes() -> KindTarget {
        target("", "Node", "nodes", false, KindId::NODE)
    }

    fn watch(target: KindTarget) -> WatchTarget {
        WatchTarget {
            target,
            fidelity: crate::discover::Fidelity::Metadata,
            pass_through: false,
        }
    }

    #[test]
    fn an_unanswered_namespace_keeps_its_fallback_on_a_denied_kind() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::POD, Answer::Denied);
        access
            .per_namespace
            .push(("answered-and-denied".into(), ruleset(vec![], false)));
        access
            .per_namespace
            .push(("transiently-failed".into(), RuleSet::unanswered()));

        let scope = access.scope_for(&pods());
        assert_eq!(
            scope,
            WatchScope::Namespaces(vec!["transiently-failed".into()]),
            "a namespace whose rules review failed must be attempted, not silently dropped"
        );
        assert_eq!(
            access.verdict(&pods()),
            Capability::Watchable,
            "the kind stays watchable through the attempted namespace"
        );
        assert_eq!(
            access.unanswered_namespaces().collect::<Vec<_>>(),
            vec!["transiently-failed"],
            "the report must be able to name what it is guessing about"
        );
    }

    #[test]
    fn a_plain_rule_authorizes_exactly_what_it_names() {
        let r = ruleset(
            vec![rule(&[""], &["pods"], &["get", "list", "watch"], &[])],
            false,
        );
        assert!(r.allows("", "pods", "list"));
        assert!(r.allows_reflection("", "pods"));
        assert!(!r.allows("", "pods", "delete"));
        assert!(!r.allows("", "secrets", "list"));
        assert!(!r.allows("apps", "pods", "list"));
    }

    #[test]
    fn wildcards_work_in_every_position() {
        let all = ruleset(vec![rule(&["*"], &["*"], &["*"], &[])], false);
        assert!(all.allows_reflection("", "pods"));
        assert!(all.allows_reflection("kubevirt.io", "virtualmachineinstances"));
        assert!(all.allows("apps", "deployments", "delete"));

        let read_all = ruleset(
            vec![rule(&["*"], &["*"], &["get", "list", "watch"], &[])],
            false,
        );
        assert!(read_all.allows_reflection("batch", "cronjobs"));
        assert!(!read_all.allows("batch", "cronjobs", "create"));
    }

    #[test]
    fn resource_names_do_not_grant_a_collection_verb() {
        let r = ruleset(
            vec![rule(
                &[""],
                &["configmaps"],
                &["get", "list", "watch"],
                &["leader-election"],
            )],
            false,
        );
        assert!(
            r.allows("", "configmaps", "get"),
            "a named get is genuinely granted"
        );
        assert!(!r.allows("", "configmaps", "list"));
        assert!(!r.allows("", "configmaps", "watch"));
        assert!(!r.allows_reflection("", "configmaps"));

        let both = ruleset(
            vec![
                rule(&[""], &["configmaps"], &["get"], &["leader-election"]),
                rule(&[""], &["configmaps"], &["list", "watch"], &[]),
            ],
            false,
        );
        assert!(both.allows_reflection("", "configmaps"));
    }

    #[test]
    fn reflection_needs_both_list_and_watch() {
        let list_only = ruleset(vec![rule(&[""], &["pods"], &["get", "list"], &[])], false);
        assert!(!list_only.allows_reflection("", "pods"));
        let watch_only = ruleset(vec![rule(&[""], &["pods"], &["watch"], &[])], false);
        assert!(!watch_only.allows_reflection("", "pods"));
    }

    #[test]
    fn an_incomplete_rule_set_is_a_maybe_not_a_no() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::POD, Answer::Denied);
        access
            .per_namespace
            .push(("prod".into(), ruleset(Vec::new(), true)));
        assert_eq!(
            access.scope_for(&pods()),
            WatchScope::Namespaces(vec!["prod".into()])
        );
        assert_eq!(access.verdict(&pods()), Capability::Watchable);
    }

    #[test]
    fn cluster_wide_permission_yields_one_stream() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::POD, Answer::Allowed);
        assert_eq!(access.scope_for(&pods()), WatchScope::All);
        assert_eq!(access.verdict(&pods()), Capability::Watchable);
    }

    #[test]
    fn a_namespace_scoped_grant_falls_back_to_per_namespace_streams() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::POD, Answer::Denied);
        access.per_namespace.push((
            "team-a".into(),
            ruleset(vec![rule(&[""], &["pods"], &["list", "watch"], &[])], false),
        ));
        access.per_namespace.push((
            "team-b".into(),
            ruleset(vec![rule(&[""], &["pods"], &["get"], &[])], false),
        ));
        assert_eq!(
            access.scope_for(&pods()),
            WatchScope::Namespaces(vec!["team-a".into()])
        );
        assert_eq!(access.verdict(&pods()), Capability::Watchable);
    }

    #[test]
    fn a_cluster_scoped_kind_has_no_namespace_fallback() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::NODE, Answer::Denied);
        access.per_namespace.push((
            "team-a".into(),
            ruleset(vec![rule(&["*"], &["*"], &["*"], &[])], false),
        ));
        assert_eq!(access.scope_for(&nodes()), WatchScope::Denied);
        assert_eq!(access.verdict(&nodes()), Capability::Forbidden);
    }

    #[test]
    fn an_unanswered_review_is_attempted_at_either_scope() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::NODE, Answer::Unanswered);
        access.cluster_wide.insert(KindId::POD, Answer::Unanswered);
        access.per_namespace.push((
            "team-a".into(),
            ruleset(vec![rule(&[""], &["pods"], &["list", "watch"], &[])], false),
        ));
        assert_eq!(access.scope_for(&nodes()), WatchScope::All);
        assert_eq!(access.verdict(&nodes()), Capability::Watchable);
        assert_eq!(
            access.scope_for(&pods()),
            WatchScope::All,
            "a grant in one namespace is no reason to stop asking about the cluster"
        );
    }

    #[test]
    fn one_unanswered_kind_leaves_its_neighbours_alone() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::NODE, Answer::Unanswered);
        access.cluster_wide.insert(KindId::POD, Answer::Denied);
        access.cluster_wide.insert(KindId::SECRET, Answer::Denied);
        access.per_namespace.push((
            "team-a".into(),
            ruleset(vec![rule(&[""], &["pods"], &["list", "watch"], &[])], false),
        ));
        assert_eq!(access.scope_for(&nodes()), WatchScope::All);
        assert_eq!(
            access.scope_for(&pods()),
            WatchScope::Namespaces(vec!["team-a".into()])
        );
        let secrets = target("", "Secret", "secrets", true, KindId::SECRET);
        assert_eq!(access.scope_for(&secrets), WatchScope::Denied);
        assert_eq!(access.unanswered(), 1);
    }

    #[test]
    fn a_denial_on_either_verb_denies_the_pair_and_an_error_does_not() {
        use Answer::{Allowed, Denied, Unanswered};
        assert_eq!(Allowed.and(Allowed), Allowed);
        assert_eq!(Allowed.and(Denied), Denied);
        assert_eq!(Denied.and(Unanswered), Denied);
        assert_eq!(Unanswered.and(Denied), Denied);
        assert_eq!(Allowed.and(Unanswered), Unanswered);
        assert_eq!(Unanswered.and(Unanswered), Unanswered);
    }

    #[test]
    fn forbidden_and_absent_are_different_answers() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::SECRET, Answer::Denied);
        let mut unwatchable = target("", "Secret", "secrets", true, KindId::SECRET);
        unwatchable.watchable = false;
        assert_eq!(access.verdict(&unwatchable), Capability::Absent);

        let watchable = target("", "Secret", "secrets", true, KindId::SECRET);
        assert_eq!(access.verdict(&watchable), Capability::Forbidden);
    }

    #[test]
    fn an_unprobed_access_set_attempts_everything() {
        let access = Access::unprobed();
        assert!(access.degraded);
        assert_eq!(access.scope_for(&pods()), WatchScope::All);
        assert_eq!(access.scope_for(&nodes()), WatchScope::All);
        assert_eq!(access.verdict(&pods()), Capability::Watchable);
        let caps = access.day2_caps(&pods());
        assert!(
            caps.patch && caps.delete && caps.create,
            "unprobed day-2 still tries the wire: {caps:?}"
        );
    }

    #[test]
    fn day2_caps_come_from_patch_delete_create_not_list_watch() {
        let mut access = Access::default();
        access
            .cluster_wide
            .insert(KindId::DEPLOYMENT, Answer::Allowed);
        access.per_namespace.push((
            "g2".into(),
            ruleset(
                vec![rule(
                    &["apps"],
                    &["deployments"],
                    &["get", "list", "watch"],
                    &[],
                )],
                false,
            ),
        ));
        let deploy = target(
            "apps",
            "Deployment",
            "deployments",
            true,
            KindId::DEPLOYMENT,
        );
        let caps = access.day2_caps(&deploy);
        assert!(
            !caps.patch && !caps.delete && !caps.create,
            "a reader who can list must not be told they can scale: {caps:?}"
        );

        let mut admin = Access::default();
        admin
            .cluster_wide
            .insert(KindId::DEPLOYMENT, Answer::Allowed);
        admin.per_namespace.push((
            "g2".into(),
            ruleset(vec![rule(&["*"], &["*"], &["*"], &[])], false),
        ));
        let admin_caps = admin.day2_caps(&deploy);
        assert!(
            admin_caps.patch && admin_caps.delete && admin_caps.create,
            "star verbs grant the day-2 clicks: {admin_caps:?}"
        );
    }

    #[test]
    fn a_kind_the_probe_answered_no_for_is_denied() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::POD, Answer::Denied);
        assert_eq!(access.scope_for(&pods()), WatchScope::Denied);
        assert_eq!(access.verdict(&pods()), Capability::Forbidden);
    }

    #[test]
    fn the_reviews_we_send_name_the_plural_resource_and_no_namespace() {
        let review = access_review(&pods(), "watch");
        let attrs = review.spec.resource_attributes.expect("attributes");
        assert_eq!(attrs.resource.as_deref(), Some("pods"));
        assert_eq!(attrs.group.as_deref(), Some(""));
        assert_eq!(attrs.verb.as_deref(), Some("watch"));
        assert_eq!(attrs.version.as_deref(), Some("v1"));
        assert_eq!(
            attrs.namespace, None,
            "an unset namespace is what asks about the whole cluster"
        );
        assert_eq!(rules_review("prod").spec.namespace.as_deref(), Some("prod"));
    }

    #[test]
    fn a_ruleset_with_omitted_optional_fields_denies_instead_of_panicking() {
        let bare = ruleset(
            vec![ResourceRule {
                api_groups: None,
                resources: None,
                resource_names: None,
                verbs: vec!["list".into()],
            }],
            false,
        );
        assert!(!bare.allows("", "pods", "list"));
        assert!(!bare.is_empty());
    }

    #[test]
    fn watch_targets_carry_through_to_verdicts() {
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::POD, Answer::Allowed);
        let w = watch(pods());
        assert_eq!(access.verdict(&w.target), Capability::Watchable);
    }
}
