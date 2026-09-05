//! Pure join of already-fetched Roles and Bindings: forward and reverse
//! lookup, wildcards, named grants, and the labelled degradation of a
//! truncated or missing RBAC API. No API server.

use super::*;
use k8s_openapi::api::rbac::v1::{AggregationRule, PolicyRule, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};

fn rule(groups: &[&str], resources: &[&str], verbs: &[&str], names: &[&str]) -> PolicyRule {
    PolicyRule {
        api_groups: Some(groups.iter().map(|s| (*s).to_string()).collect()),
        resources: Some(resources.iter().map(|s| (*s).to_string()).collect()),
        verbs: verbs.iter().map(|s| (*s).to_string()).collect(),
        resource_names: if names.is_empty() {
            None
        } else {
            Some(names.iter().map(|s| (*s).to_string()).collect())
        },
        non_resource_urls: None,
    }
}

fn meta(name: &str, namespace: Option<&str>) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: namespace.map(str::to_string),
        ..Default::default()
    }
}

fn role(namespace: &str, name: &str, rules: Vec<PolicyRule>) -> Role {
    Role {
        metadata: meta(name, Some(namespace)),
        rules: Some(rules),
    }
}

fn cluster_role(name: &str, rules: Vec<PolicyRule>) -> ClusterRole {
    ClusterRole {
        metadata: meta(name, None),
        rules: Some(rules),
        aggregation_rule: None,
    }
}

fn role_ref(kind: &str, name: &str) -> RoleRef {
    RoleRef {
        api_group: Some(RBAC_GROUP.to_string()),
        kind: kind.to_string(),
        name: name.to_string(),
    }
}

fn subject(kind: &str, name: &str, namespace: Option<&str>) -> Subject {
    Subject {
        kind: kind.to_string(),
        name: name.to_string(),
        namespace: namespace.map(str::to_string),
        api_group: None,
    }
}

fn role_binding(
    namespace: &str,
    name: &str,
    kind: &str,
    role_name: &str,
    subjects: Vec<Subject>,
) -> RoleBinding {
    RoleBinding {
        metadata: meta(name, Some(namespace)),
        role_ref: role_ref(kind, role_name),
        subjects: Some(subjects),
    }
}

fn cluster_role_binding(name: &str, role_name: &str, subjects: Vec<Subject>) -> ClusterRoleBinding {
    ClusterRoleBinding {
        metadata: meta(name, None),
        role_ref: role_ref("ClusterRole", role_name),
        subjects: Some(subjects),
    }
}

fn index(docs: Documents) -> Index {
    Index::from_documents(&docs)
}

fn alice() -> SubjectRef {
    SubjectRef::user("alice")
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn a_role_binding_authorizes_exactly_what_it_names() {
    let idx = index(Documents {
        roles: vec![role(
            "prod",
            "pod-reader",
            vec![rule(&[""], &["pods"], &["get", "list", "watch"], &[])],
        )],
        role_bindings: vec![role_binding(
            "prod",
            "read-pods",
            "Role",
            "pod-reader",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(idx.allows(&alice(), "list", "", "pods", Some("prod"), None));
    assert!(idx.allows(&alice(), "get", "", "pods", Some("prod"), Some("api")));
    assert!(
        !idx.allows(&alice(), "delete", "", "pods", Some("prod"), None),
        "a read Role is not a delete"
    );
    assert!(
        !idx.allows(&alice(), "list", "", "secrets", Some("prod"), None),
        "pods are not secrets"
    );
    assert!(
        !idx.allows(&alice(), "list", "apps", "pods", Some("prod"), None),
        "core pods are not apps pods"
    );
    assert_eq!(idx.who_can("list", "", "pods", Some("prod")), vec![alice()]);
}

#[test]
fn wildcards_work_in_every_position() {
    let idx = index(Documents {
        cluster_roles: vec![cluster_role("all", vec![rule(&["*"], &["*"], &["*"], &[])])],
        cluster_role_bindings: vec![cluster_role_binding(
            "bind-all",
            "all",
            vec![subject("Group", "system:masters", None)],
        )],
        ..Documents::default()
    });
    let masters = SubjectRef::group("system:masters");
    assert!(idx.allows(&masters, "delete", "", "secrets", Some("prod"), None));
    assert!(idx.allows(
        &masters,
        "create",
        "apps",
        "deployments",
        Some("kube-system"),
        None
    ));
    assert!(idx.allows(&masters, "delete", "", "nodes", None, None));
    let can = idx.what_can(&masters);
    assert_eq!(can.len(), 1);
    assert_eq!(can[0].verb, "*");
    assert_eq!(can[0].api_group, "*");
    assert_eq!(can[0].resource, "*");
    assert_eq!(can[0].namespace, None);
}

#[test]
fn empty_resource_names_cover_the_whole_collection() {
    let idx = index(Documents {
        roles: vec![role(
            "prod",
            "cm",
            vec![rule(&[""], &["configmaps"], &["get", "list"], &[])],
        )],
        role_bindings: vec![role_binding(
            "prod",
            "bind",
            "Role",
            "cm",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(idx.allows(&alice(), "list", "", "configmaps", Some("prod"), None));
    assert!(idx.allows(
        &alice(),
        "get",
        "",
        "configmaps",
        Some("prod"),
        Some("leader")
    ));
}

#[test]
fn named_resource_names_do_not_grant_a_collection_verb() {
    let idx = index(Documents {
        roles: vec![role(
            "prod",
            "named",
            vec![rule(
                &[""],
                &["configmaps"],
                &["get", "list", "watch", "delete"],
                &["leader-election"],
            )],
        )],
        role_bindings: vec![role_binding(
            "prod",
            "bind",
            "Role",
            "named",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(
        idx.allows(
            &alice(),
            "get",
            "",
            "configmaps",
            Some("prod"),
            Some("leader-election")
        ),
        "a named get is genuinely granted"
    );
    assert!(
        !idx.allows(&alice(), "get", "", "configmaps", Some("prod"), None),
        "a named grant is not an unrestricted get"
    );
    assert!(!idx.allows(&alice(), "list", "", "configmaps", Some("prod"), None));
    assert!(!idx.allows(&alice(), "watch", "", "configmaps", Some("prod"), None));
    assert!(
        idx.who_can("delete", "", "configmaps", Some("prod"))
            .contains(&alice()),
        "who can delete still names the subject: they can delete that one object"
    );
    assert!(
        idx.who_can("list", "", "configmaps", Some("prod"))
            .is_empty(),
        "a named list is not a list"
    );
}

#[test]
fn a_cluster_role_binding_applies_in_every_namespace() {
    let idx = index(Documents {
        cluster_roles: vec![cluster_role(
            "secret-deleter",
            vec![rule(&[""], &["secrets"], &["delete"], &[])],
        )],
        cluster_role_bindings: vec![cluster_role_binding(
            "delete-secrets",
            "secret-deleter",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(idx.allows(&alice(), "delete", "", "secrets", Some("prod"), None));
    assert!(idx.allows(&alice(), "delete", "", "secrets", Some("kube-system"), None));
    assert!(idx.allows(&alice(), "delete", "", "secrets", None, None));
    assert_eq!(
        idx.who_can("delete", "", "secrets", Some("prod")),
        vec![alice()],
        "who can delete secrets in prod includes a cluster binding"
    );
}

#[test]
fn a_role_binding_stays_inside_its_namespace() {
    let idx = index(Documents {
        roles: vec![role(
            "prod",
            "secret-deleter",
            vec![rule(&[""], &["secrets"], &["delete"], &[])],
        )],
        role_bindings: vec![role_binding(
            "prod",
            "bind",
            "Role",
            "secret-deleter",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(idx.allows(&alice(), "delete", "", "secrets", Some("prod"), None));
    assert!(!idx.allows(&alice(), "delete", "", "secrets", Some("other"), None));
    assert!(
        !idx.allows(&alice(), "delete", "", "secrets", None, None),
        "a namespaced binding does not grant cluster-scoped access"
    );
    assert!(
        idx.who_can("delete", "", "secrets", Some("other"))
            .is_empty()
    );
    assert!(idx.who_can("delete", "", "nodes", None).is_empty());
}

#[test]
fn a_role_binding_can_point_at_a_cluster_role() {
    let idx = index(Documents {
        cluster_roles: vec![cluster_role(
            "edit",
            vec![rule(&[""], &["secrets"], &["delete"], &[])],
        )],
        role_bindings: vec![role_binding(
            "prod",
            "bind-edit",
            "ClusterRole",
            "edit",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(idx.allows(&alice(), "delete", "", "secrets", Some("prod"), None));
    assert!(
        !idx.allows(&alice(), "delete", "", "secrets", Some("other"), None),
        "the ClusterRole is global; the RoleBinding is not"
    );
}

#[test]
fn a_cluster_role_binding_that_points_at_a_role_grants_nothing() {
    let mut binding =
        cluster_role_binding("broken", "pod-reader", vec![subject("User", "alice", None)]);
    binding.role_ref.kind = "Role".into();
    let idx = index(Documents {
        roles: vec![role(
            "prod",
            "pod-reader",
            vec![rule(&[""], &["pods"], &["*"], &[])],
        )],
        cluster_role_bindings: vec![binding],
        ..Documents::default()
    });
    assert!(idx.what_can(&alice()).is_empty());
}

#[test]
fn reverse_lookup_includes_cluster_bindings_that_apply() {
    let idx = index(Documents {
        roles: vec![
            role(
                "prod",
                "ns-delete",
                vec![rule(&[""], &["secrets"], &["delete"], &[])],
            ),
            role(
                "other",
                "ns-delete",
                vec![rule(&[""], &["secrets"], &["delete"], &[])],
            ),
        ],
        cluster_roles: vec![cluster_role(
            "cluster-delete",
            vec![rule(&[""], &["secrets"], &["delete"], &[])],
        )],
        role_bindings: vec![
            role_binding(
                "prod",
                "ns-bind",
                "Role",
                "ns-delete",
                vec![subject("User", "alice", None)],
            ),
            role_binding(
                "other",
                "other-bind",
                "Role",
                "ns-delete",
                vec![subject("User", "bob", None)],
            ),
        ],
        cluster_role_bindings: vec![cluster_role_binding(
            "cluster-bind",
            "cluster-delete",
            vec![subject("Group", "sre", None)],
        )],
        ..Documents::default()
    });
    let who = idx.who_can("delete", "", "secrets", Some("prod"));
    assert_eq!(
        who,
        vec![alice(), SubjectRef::group("sre")],
        "prod sees its RoleBinding and every ClusterRoleBinding; other-ns is out"
    );
    assert_eq!(
        idx.who_can("delete", "", "secrets", Some("other")),
        vec![SubjectRef::user("bob"), SubjectRef::group("sre")]
    );
}

#[test]
fn a_truncated_listing_is_labelled_incomplete() {
    let complete = index(Documents::default());
    assert!(complete.served);
    assert!(!complete.incomplete);

    let truncated = index(Documents {
        truncated: true,
        ..Documents::default()
    });
    assert!(truncated.served);
    assert!(
        truncated.incomplete,
        "a cap is a labelled hole, not a quieter listing"
    );
}

#[test]
fn a_missing_rbac_api_is_not_served() {
    assert_eq!(interpret(&api_error(404)), ListError::NotServed);
    assert_eq!(
        decide([
            WireStatus::NotServed,
            WireStatus::NotServed,
            WireStatus::NotServed,
            WireStatus::NotServed,
        ]),
        Decision::NotServed
    );
    let idx = Index::unserved();
    assert!(!idx.served);
    assert!(!idx.incomplete);
    assert!(
        idx.who_can("delete", "", "secrets", Some("prod"))
            .is_empty()
    );
    assert!(!idx.allows(&alice(), "delete", "", "secrets", Some("prod"), None));
}

#[test]
fn a_forbidden_list_is_denied() {
    for code in [401, 403] {
        assert_eq!(
            interpret(&api_error(code)),
            ListError::Denied,
            "{code} is an administrator's no, not an empty index"
        );
    }
    assert!(matches!(interpret(&api_error(500)), ListError::Failed(_)));
    assert_eq!(
        decide([
            WireStatus::Denied,
            WireStatus::Ok { truncated: false },
            WireStatus::NotServed,
            WireStatus::Failed("boom".into()),
        ]),
        Decision::Denied,
        "a 403 on one kind refuses the whole explorer"
    );
}

#[test]
fn a_failed_list_beats_absence_and_a_partial_list_is_incomplete() {
    assert_eq!(
        decide([
            WireStatus::Failed("no".into()),
            WireStatus::NotServed,
            WireStatus::NotServed,
            WireStatus::NotServed,
        ]),
        Decision::Failed("no".into())
    );
    assert_eq!(
        decide([
            WireStatus::Ok { truncated: false },
            WireStatus::NotServed,
            WireStatus::Ok { truncated: false },
            WireStatus::Ok { truncated: false },
        ]),
        Decision::Ok { incomplete: true },
        "one kind the server did not serve is some of the relation, labelled"
    );
    assert_eq!(
        decide([
            WireStatus::Ok { truncated: true },
            WireStatus::Ok { truncated: false },
            WireStatus::Ok { truncated: false },
            WireStatus::Ok { truncated: false },
        ]),
        Decision::Ok { incomplete: true }
    );
}

#[test]
fn a_service_account_inherits_the_role_binding_namespace() {
    let idx = index(Documents {
        roles: vec![role(
            "prod",
            "reader",
            vec![rule(&[""], &["pods"], &["get"], &[])],
        )],
        role_bindings: vec![role_binding(
            "prod",
            "bind",
            "Role",
            "reader",
            vec![subject("ServiceAccount", "default", None)],
        )],
        ..Documents::default()
    });
    let sa = SubjectRef::service_account("prod", "default");
    assert!(idx.allows(&sa, "get", "", "pods", Some("prod"), None));
    assert_eq!(idx.who_can("get", "", "pods", Some("prod")), vec![sa]);
}

#[test]
fn a_service_account_without_a_namespace_is_dropped_from_a_cluster_binding() {
    let idx = index(Documents {
        cluster_roles: vec![cluster_role(
            "reader",
            vec![rule(&[""], &["pods"], &["get"], &[])],
        )],
        cluster_role_bindings: vec![cluster_role_binding(
            "bind",
            "reader",
            vec![subject("ServiceAccount", "default", None)],
        )],
        ..Documents::default()
    });
    assert!(
        idx.who_can("get", "", "pods", Some("prod")).is_empty(),
        "a ClusterRoleBinding must name the ServiceAccount's namespace"
    );
}

#[test]
fn the_user_spelling_of_a_service_account_matches_the_sa_subject() {
    let idx = index(Documents {
        roles: vec![role(
            "prod",
            "reader",
            vec![rule(&[""], &["pods"], &["get"], &[])],
        )],
        role_bindings: vec![role_binding(
            "prod",
            "bind",
            "Role",
            "reader",
            vec![subject("User", "system:serviceaccount:prod:default", None)],
        )],
        ..Documents::default()
    });
    let sa = SubjectRef::service_account("prod", "default");
    assert!(
        idx.allows(&sa, "get", "", "pods", Some("prod"), None),
        "the authorizer treats the User spelling as that ServiceAccount"
    );
}

#[test]
fn a_star_resource_does_not_grant_a_subresource() {
    let idx = index(Documents {
        cluster_roles: vec![cluster_role(
            "star",
            vec![
                rule(&[""], &["*"], &["get"], &[]),
                rule(&[""], &["pods/*"], &["get"], &[]),
                rule(&[""], &["*/log"], &["get"], &[]),
            ],
        )],
        cluster_role_bindings: vec![cluster_role_binding(
            "bind",
            "star",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(idx.allows(&alice(), "get", "", "pods", Some("prod"), None));
    assert!(
        idx.allows(&alice(), "get", "", "pods/log", Some("prod"), None),
        "pods/* and */log are how a subresource is named"
    );
    let star_only = index(Documents {
        cluster_roles: vec![cluster_role(
            "star-only",
            vec![rule(&[""], &["*"], &["get"], &[])],
        )],
        cluster_role_bindings: vec![cluster_role_binding(
            "bind",
            "star-only",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(star_only.allows(&alice(), "get", "", "pods", Some("prod"), None));
    assert!(
        !star_only.allows(&alice(), "get", "", "pods/log", Some("prod"), None),
        "* matches a resource, not a subresource"
    );
}

#[test]
fn an_unresolved_role_ref_grants_nothing() {
    let idx = index(Documents {
        role_bindings: vec![role_binding(
            "prod",
            "bind",
            "Role",
            "missing",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(idx.what_can(&alice()).is_empty());
    assert!(idx.who_can("get", "", "pods", Some("prod")).is_empty());
}

#[test]
fn aggregation_uses_the_rules_already_on_the_cluster_role() {
    let mut aggregated = cluster_role(
        "aggregated",
        vec![rule(&[""], &["secrets"], &["delete"], &[])],
    );
    aggregated.aggregation_rule = Some(AggregationRule {
        cluster_role_selectors: Some(vec![LabelSelector::default()]),
    });
    let other = cluster_role("other", vec![rule(&[""], &["pods"], &["delete"], &[])]);
    let idx = index(Documents {
        cluster_roles: vec![aggregated, other.clone()],
        cluster_role_bindings: vec![cluster_role_binding(
            "bind",
            "aggregated",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(
        idx.allows(&alice(), "delete", "", "secrets", Some("prod"), None),
        "the rules field is what the aggregation controller wrote"
    );
    assert!(
        !idx.allows(&alice(), "delete", "", "pods", Some("prod"), None),
        "selectors are not walked; other ClusterRoles stay other ClusterRoles"
    );

    let mut empty = cluster_role("empty-agg", vec![]);
    empty.aggregation_rule = Some(AggregationRule {
        cluster_role_selectors: Some(vec![LabelSelector::default()]),
    });
    let idx = index(Documents {
        cluster_roles: vec![empty, other],
        cluster_role_bindings: vec![cluster_role_binding(
            "bind",
            "empty-agg",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(
        idx.what_can(&alice()).is_empty(),
        "empty rules grant nothing, even when an aggregation selector is set"
    );
}

#[test]
fn a_non_resource_url_rule_is_not_a_secret_grant() {
    let idx = index(Documents {
        cluster_roles: vec![ClusterRole {
            metadata: meta("urls", None),
            rules: Some(vec![PolicyRule {
                verbs: vec!["get".into()],
                non_resource_urls: Some(vec!["/healthz".into()]),
                api_groups: None,
                resources: None,
                resource_names: None,
            }]),
            aggregation_rule: None,
        }],
        cluster_role_bindings: vec![cluster_role_binding(
            "bind",
            "urls",
            vec![subject("User", "alice", None)],
        )],
        ..Documents::default()
    });
    assert!(!idx.allows(&alice(), "get", "", "secrets", Some("prod"), None));
    assert!(idx.who_can("get", "", "secrets", Some("prod")).is_empty());
}

#[test]
fn resource_matching_follows_the_star_and_slash_rules() {
    assert!(resource_matches(&["*".into()], "pods"));
    assert!(!resource_matches(&["*".into()], "pods/log"));
    assert!(resource_matches(&["pods".into()], "pods"));
    assert!(!resource_matches(&["pods".into()], "pods/log"));
    assert!(resource_matches(&["pods/*".into()], "pods/log"));
    assert!(!resource_matches(&["pods/*".into()], "pods"));
    assert!(resource_matches(&["*/log".into()], "pods/log"));
    assert!(!resource_matches(&["*/log".into()], "log"));
}

#[test]
fn the_object_cap_is_five_thousand_per_kind() {
    assert_eq!(MAX_OBJECTS, 5_000);
}
