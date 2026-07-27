//! The capability probe: what we are allowed to read, asked before we ask for it.
//!
//! An RBAC-restricted service account is the normal enterprise case, and the
//! failure mode is the worst kind: the app looks like it works and shows empty
//! answers. So the capability set is an input, not an error handler.
//!
//! Two API calls, chosen for cost:
//!
//! - **`SelfSubjectRulesReview`, once per entered namespace.** One request
//!   returns every rule that applies to us in that namespace, which answers
//!   list-and-watch for *all* namespaced kinds at once. Asking per kind would be
//!   two hundred requests to learn what one returns.
//! - **`SelfSubjectAccessReview`, targeted.** A rules review is namespaced, so it
//!   cannot answer a cluster-scoped question, and it cannot answer
//!   "across all namespaces" either. Both need an access review.
//!
//! The matching itself is a pure function over the returned rules, which is the
//! part worth testing hard: RBAC wildcards and the `resourceNames` rule below are
//! exactly where a hand-rolled check is wrong in a way nobody notices until a
//! restricted cluster shows an empty map.

use std::collections::HashMap;

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    SelfSubjectRulesReview, SelfSubjectRulesReviewSpec, SubjectRulesReviewStatus,
};
use k10s_core::{Capability, KindId};
use kube::api::PostParams;
use kube::{Api, Client};

use crate::discover::{KindTarget, WatchTarget};

/// Verbs whose request carries no object name.
///
/// This is why they get their own list: a rule with `resourceNames` authorizes
/// only requests *for those names*, and a collection request has no name, so such
/// a rule grants nothing here. Kubernetes' own authorizer works this way, and
/// treating `resourceNames` as if it granted `list` is the single most likely way
/// to conclude we may watch a kind we may not.
const COLLECTION_VERBS: &[&str] = &["list", "watch", "deletecollection"];

fn is_collection_verb(verb: &str) -> bool {
    COLLECTION_VERBS.contains(&verb)
}

/// One RBAC rule, flattened out of the review's optional fields.
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

/// The rules that apply to us somewhere, as data.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
    incomplete: bool,
}

impl RuleSet {
    /// Reads a `SelfSubjectRulesReview` status.
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
            // The API server says so when it could not evaluate every source of
            // rules. A "no" from an incomplete answer is a maybe, and treating it
            // as a no is how a working affordance gets disabled.
            incomplete: status.incomplete,
        }
    }

    /// The server could not enumerate every rule, so a denial here is not proof.
    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether these rules authorize `verb` on `group`/`resource`.
    ///
    /// `resource` is the plural name an RBAC rule uses (`pods`, not `Pod`), and
    /// `group` is the empty string for core.
    pub fn allows(&self, group: &str, resource: &str, verb: &str) -> bool {
        self.rules.iter().any(|r| {
            (r.names.is_empty() || !is_collection_verb(verb))
                && matches(&r.groups, group)
                && matches(&r.resources, resource)
                && matches(&r.verbs, verb)
        })
    }

    /// Whether a kind can be both listed and watched, which is what a reflector
    /// needs and what [`Capability::Watchable`] means.
    pub fn allows_reflection(&self, group: &str, resource: &str) -> bool {
        self.allows(group, resource, "list") && self.allows(group, resource, "watch")
    }
}

/// Where a kind may be watched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScope {
    /// One stream across the cluster.
    All,
    /// One stream per namespace, because cluster-wide list is denied but these
    /// namespaces are not. This is the case that separates a usable restricted
    /// cluster from an empty map.
    Namespaces(Vec<String>),
    /// Nowhere we can see.
    Denied,
}

/// What a cluster-wide access review said about one kind.
///
/// Three states rather than a boolean because a review that errored — a 503, a
/// timeout, an authorization webhook that hiccuped — said nothing about
/// permission, and recording it as `false` makes it indistinguishable from an
/// authoritative deny. There is no later chance to tell them apart: a genuinely
/// restricted account has every cluster-wide answer denied, so "all false" is its
/// normal state rather than a symptom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Allowed,
    Denied,
    /// The review did not complete. Not a denial, and not a grant.
    Unanswered,
}

impl Answer {
    /// The two verbs of a reflection, combined.
    ///
    /// A denial from either verb denies the pair, because a reflector needs both.
    /// A verb that never answered leaves the pair unanswered unless the other
    /// already settled it: an error on `list` beside a grant on `watch` is still
    /// no answer about listing.
    fn and(self, other: Answer) -> Answer {
        match (self, other) {
            (Answer::Denied, _) | (_, Answer::Denied) => Answer::Denied,
            (Answer::Unanswered, _) | (_, Answer::Unanswered) => Answer::Unanswered,
            (Answer::Allowed, Answer::Allowed) => Answer::Allowed,
        }
    }
}

/// Everything the probe learned.
#[derive(Debug, Clone, Default)]
pub struct Access {
    /// Access-review answers for a whole-cluster list-and-watch, per kind.
    cluster_wide: HashMap<KindId, Answer>,
    /// Rules review per namespace, in the order the namespaces were given.
    per_namespace: Vec<(String, RuleSet)>,
    /// The probe itself could not run. Then the capability set cannot gate
    /// anything and the watch's own 403 becomes the verdict instead.
    pub degraded: bool,
    /// How many API calls the probe cost, for the cold-start report.
    pub requests: u32,
}

impl Access {
    /// An access set that knows nothing, so every kind is attempted and the
    /// stream's own error is the answer.
    pub fn unprobed() -> Access {
        Access {
            degraded: true,
            ..Access::default()
        }
    }

    pub fn namespaces(&self) -> impl Iterator<Item = &str> {
        self.per_namespace.iter().map(|(ns, _)| ns.as_str())
    }

    /// How many kinds the cluster-wide review left unanswered.
    ///
    /// Worth reporting rather than only acting on: those kinds are attempted
    /// rather than gated, so a denial on one arrives as a stream error instead of
    /// a label.
    pub fn unanswered(&self) -> usize {
        self.cluster_wide
            .values()
            .filter(|answer| **answer == Answer::Unanswered)
            .count()
    }

    /// The rules that apply in one namespace, if it was probed.
    pub fn rules(&self, namespace: &str) -> Option<&RuleSet> {
        self.per_namespace
            .iter()
            .find(|(ns, _)| ns == namespace)
            .map(|(_, r)| r)
    }

    /// Where a kind may be watched.
    ///
    /// Cluster-wide first, because one stream beats N. A namespaced kind that is
    /// denied cluster-wide falls back to the namespaces whose rules allow it,
    /// which is what makes a developer with access to two namespaces see two
    /// namespaces rather than nothing.
    ///
    /// Only a definite denial narrows anything. An unanswered review is attempted
    /// cluster-wide, for the same reason `degraded` attempts everything: a 403
    /// from the stream is a labelled capability, and narrowing to the probed
    /// namespaces would show a fraction of a cluster we may be allowed to read
    /// whole, with nothing to say it was a fraction.
    pub fn scope_for(&self, target: &KindTarget) -> WatchScope {
        if self.degraded {
            // Nothing was learned, so attempt it: a 403 from the stream is a
            // labelled capability, while refusing to try is a silent empty list.
            return WatchScope::All;
        }
        // A kind nobody asked about is in the same position as one whose review
        // errored, and `probe` asks about every kind it is given.
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

    /// The verdict to publish for a kind.
    ///
    /// `Absent` and `Forbidden` are different answers to a user: absent means the
    /// cluster does not have it, forbidden means it does and we may not look.
    pub fn verdict(&self, target: &KindTarget) -> Capability {
        if !target.listable || !target.watchable {
            // The contract has no verdict for "served, readable, but not
            // watchable"; treating it as absent keeps it invisible rather than
            // showing a kind that can never populate.
            return Capability::Absent;
        }
        match self.scope_for(target) {
            WatchScope::Denied => Capability::Forbidden,
            _ => Capability::Watchable,
        }
    }
}

/// Runs the probe. One rules review per namespace, one access review per kind.
///
/// Never fails: a probe that cannot run degrades to [`Access::unprobed`], because
/// a cluster where `SelfSubjectAccessReview` is denied is still a cluster we can
/// try to read.
pub async fn probe(client: &Client, targets: &[WatchTarget], namespaces: &[String]) -> Access {
    let mut access = Access::default();

    let rules_api: Api<SelfSubjectRulesReview> = Api::all(client.clone());
    let mut rules_failed = 0u32;
    for ns in namespaces {
        access.requests += 1;
        match rules_api
            .create(&PostParams::default(), &rules_review(ns))
            .await
        {
            Ok(review) => {
                let status = review.status.unwrap_or_default();
                access
                    .per_namespace
                    .push((ns.clone(), RuleSet::from_status(&status)));
            }
            Err(_) => rules_failed += 1,
        }
    }

    let access_api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    let mut reviews_failed = 0u32;
    for want in targets {
        let mut answer = Answer::Allowed;
        for verb in ["list", "watch"] {
            access.requests += 1;
            let verdict = match access_api
                .create(&PostParams::default(), &access_review(&want.target, verb))
                .await
            {
                Ok(review) if review.status.as_ref().is_some_and(|s| s.allowed) => Answer::Allowed,
                Ok(_) => Answer::Denied,
                Err(_) => {
                    reviews_failed += 1;
                    Answer::Unanswered
                }
            };
            answer = answer.and(verdict);
        }
        access.cluster_wide.insert(want.target.id, answer);
    }

    // Distinguish "the probe ran and said no" from "the probe could not run".
    // Only the first may gate anything.
    //
    // The cluster-wide half used to ask whether every answer was false, which is
    // the state a restricted account is legitimately in: one errored review then
    // declared the whole probe useless, sent every kind cluster-wide and threw
    // away the per-namespace fallback. An error is now `Unanswered` on its own
    // kind, so what is left to ask here is whether any kind was answered at all.
    let answered = access
        .cluster_wide
        .values()
        .filter(|answer| **answer != Answer::Unanswered)
        .count();
    if reviews_failed > 0 && answered == 0 {
        access.degraded = true;
    }
    if rules_failed > 0 && access.per_namespace.is_empty() && !namespaces.is_empty() {
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

/// An access review for a whole-cluster request: `namespace` left unset means
/// "across all namespaces" for a namespaced resource, and "at cluster scope" for
/// a cluster-scoped one.
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
        // The subtlety: `resourceNames: [my-cm]` with verb `list` looks like list
        // permission and is not, because a list request carries no name. Reading
        // it as a grant is how we conclude we may watch a kind we may not, then
        // hammer the API server with 403s.
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

        // And an unrestricted rule alongside it still grants the collection.
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
        // A reflector lists then watches; list alone produces a stream that
        // errors on every poll.
        let list_only = ruleset(vec![rule(&[""], &["pods"], &["get", "list"], &[])], false);
        assert!(!list_only.allows_reflection("", "pods"));
        let watch_only = ruleset(vec![rule(&[""], &["pods"], &["watch"], &[])], false);
        assert!(!watch_only.allows_reflection("", "pods"));
    }

    #[test]
    fn an_incomplete_rule_set_is_a_maybe_not_a_no() {
        // The API server says `incomplete` when it could not evaluate every
        // authorizer. Disabling an affordance on that basis labels a working
        // feature as forbidden.
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
        // The restricted-developer case, and the one the roadmap calls the normal
        // enterprise case.
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
        // The state a boolean could not hold. A review that errored said nothing
        // about permission, and recording it as a denial made one transient
        // failure permanent: a cluster-scoped kind became unwatchable outright,
        // and a namespaced one shrank to whatever the probed namespaces allow.
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
        // Unanswered is per kind: the neighbours keep the answers they got, so the
        // per-namespace fallback still stands beside one attempt. That an errored
        // review no longer flips `degraded` as well — which would send every one
        // of them cluster-wide — takes a request to fail, so it is checked against
        // the scripted API server instead.
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
        // A reflector lists and watches, so the two answers combine. The ordering
        // that matters is the middle pair: a definite denial outranks an error,
        // because `list` denied is denied whatever `watch` failed to say.
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
        // Absent means the cluster does not serve it, so it is invisible.
        // Forbidden means it does and we may not look, so it is labelled.
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
        // A cluster that denies SelfSubjectAccessReview is still a cluster we can
        // try to read; the stream's own 403 becomes the verdict.
        let access = Access::unprobed();
        assert!(access.degraded);
        assert_eq!(access.scope_for(&pods()), WatchScope::All);
        assert_eq!(access.scope_for(&nodes()), WatchScope::All);
        assert_eq!(access.verdict(&pods()), Capability::Watchable);
    }

    #[test]
    fn a_kind_the_probe_answered_no_for_is_denied() {
        // The difference from `unprobed`: here the probe ran and the answer was
        // no, which is a real verdict. A kind it never answered for is not this
        // case -- that reads `Unanswered` and attempts the stream.
        let mut access = Access::default();
        access.cluster_wide.insert(KindId::POD, Answer::Denied);
        assert_eq!(access.scope_for(&pods()), WatchScope::Denied);
        assert_eq!(access.verdict(&pods()), Capability::Forbidden);
    }

    #[test]
    fn the_reviews_we_send_name_the_plural_resource_and_no_namespace() {
        // A resource attribute naming `Pod` instead of `pods` is silently always
        // denied, which would make every cluster look restricted.
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
        // Every field of a ResourceRule except `verbs` is optional in the API,
        // and a real server does omit them.
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
