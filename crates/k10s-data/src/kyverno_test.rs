//! Field extraction, caps, the document, 404/403 classification, and the
//! rule-body drop. A cluster is not required.

use super::*;
use serde_json::json;

const PLANTED: &str = "PLANTED-TOKEN-9f3a";

fn cluster_policy_json() -> Value {
    json!({
        "metadata": {
            "name": "require-labels",
            "uid": "uid-cpol",
            "annotations": { "policies.kyverno.io/severity": "medium" }
        },
        "spec": {
            "background": true,
            "validationFailureAction": "Enforce",
            "rules": [
                {
                    "name": "check-team",
                    "match": {
                        "any": [{ "resources": { "kinds": ["Pod", "Deployment"] } }]
                    },
                    "exclude": {
                        "resources": { "namespaces": [PLANTED] }
                    },
                    "validate": {
                        "pattern": { "metadata": { "labels": { "token": PLANTED } } }
                    }
                },
                {
                    "name": "check-ns",
                    "match": {
                        "all": [{ "resources": { "kinds": ["Namespace"] } }]
                    }
                }
            ]
        },
        "status": {
            "ready": true,
            "conditions": [{ "type": "Ready", "status": "True" }]
        }
    })
}

fn namespaced_policy_json() -> Value {
    json!({
        "metadata": { "name": "deny-latest", "namespace": "prod", "uid": "uid-pol" },
        "spec": {
            "background": false,
            "validationFailureAction": "Audit",
            "rules": [{
                "name": "no-latest",
                "match": { "resources": { "kinds": ["Pod"] } }
            }]
        },
        "status": { "ready": false }
    })
}

fn cleanup_json() -> Value {
    json!({
        "metadata": { "name": "stale-pods", "namespace": "prod" },
        "spec": {
            "schedule": "*/5 * * * *",
            "match": {
                "any": [{ "resources": { "kinds": ["Pod"] } }]
            }
        },
        "status": { "conditions": [{ "type": "Ready", "status": "False" }] }
    })
}

fn validating_policy_json() -> Value {
    json!({
        "metadata": {
            "name": "check-labels",
            "uid": "uid-vpol",
            "annotations": { "policies.kyverno.io/severity": "medium" }
        },
        "spec": {
            "validationActions": ["Deny", "Audit"],
            "evaluation": {
                "admission": { "enabled": true },
                "background": { "enabled": true }
            },
            "matchConstraints": {
                "resourceRules": [{
                    "apiGroups": [""],
                    "apiVersions": ["v1"],
                    "operations": ["CREATE", "UPDATE"],
                    "resources": ["pods"],
                    "kinds": ["Pod"]
                }]
            },
            "matchConditions": [{
                "name": "skip-planted",
                "expression": PLANTED
            }],
            "variables": [{ "name": "token", "expression": PLANTED }],
            "validations": [{
                "message": "label environment is required",
                "expression": PLANTED
            }],
            "mutations": [{
                "patchType": "ApplyConfiguration",
                "applyConfiguration": { "expression": PLANTED }
            }],
            "generate": [{ "expression": PLANTED }]
        },
        "status": {
            "conditions": [{ "type": "Ready", "status": "True" }]
        }
    })
}

fn legacy_exception_json() -> Value {
    json!({
        "metadata": { "name": "delta-exception", "namespace": "delta", "uid": "uid-polex" },
        "spec": {
            "exceptions": [{
                "policyName": "disallow-host-namespaces",
                "ruleNames": ["host-namespaces"]
            }],
            "match": { "any": [{ "resources": { "kinds": ["Pod", "Deployment"] } }] }
        }
    })
}

fn resource_from(kind: Kind, version: &str, value: Value) -> Resource {
    parse_item(kind, version, value).expect("the fixture is a Kyverno object")
}

fn leak_surface(inventory: &Inventory) -> String {
    let mut text = format!("{inventory:?}");
    if let Some(page) = table_page(inventory) {
        for row in &page.rows {
            for cell in &row.cells {
                text.push('\n');
                text.push_str(cell);
            }
        }
    }
    text.push('\n');
    text.push_str(&render(inventory).join("\n"));
    text
}

#[test]
fn a_cluster_policy_keeps_action_ready_kinds_and_severity() {
    let resource = resource_from(Kind::ClusterPolicy, "v1", cluster_policy_json());
    assert_eq!(resource.name, "require-labels");
    assert_eq!(resource.namespace, "");
    assert_eq!(resource.uid, "uid-cpol");
    assert_eq!(resource.background, Some(true));
    assert_eq!(resource.validation_failure_action, "Enforce");
    assert_eq!(resource.ready, "True");
    assert_eq!(resource.rule_count, 2);
    assert_eq!(resource.rule_kinds, vec!["Pod", "Deployment", "Namespace"]);
    assert_eq!(resource.severity, "medium");
}

#[test]
fn a_namespaced_policy_keeps_its_namespace_and_status_ready() {
    let resource = resource_from(Kind::Policy, "v1", namespaced_policy_json());
    assert_eq!(resource.namespace, "prod");
    assert_eq!(resource.background, Some(false));
    assert_eq!(resource.validation_failure_action, "Audit");
    assert_eq!(resource.ready, "False");
    assert_eq!(resource.rule_kinds, vec!["Pod"]);
}

#[test]
fn a_cleanup_policy_reads_match_kinds_when_the_group_names_v2() {
    let resource = resource_from(Kind::CleanupPolicy, "v2", cleanup_json());
    assert_eq!(resource.name, "stale-pods");
    assert_eq!(resource.namespace, "prod");
    assert_eq!(resource.ready, "False");
    assert_eq!(resource.rule_count, 1);
    assert_eq!(resource.rule_kinds, vec!["Pod"]);
    assert!(resource.validation_failure_action.is_empty());
}

#[test]
fn a_legacy_policy_exception_keeps_its_namespace_exception_count_and_match_kinds() {
    let resource = resource_from(Kind::LegacyPolicyException, "v2", legacy_exception_json());
    assert_eq!(resource.kind.as_str(), "PolicyException");
    assert_eq!(resource.name, "delta-exception");
    assert_eq!(resource.namespace, "delta");
    assert_eq!(resource.rule_count, 1);
    assert_eq!(resource.rule_kinds, vec!["Pod", "Deployment"]);
    assert!(resource.validation_failure_action.is_empty());
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::ClusterPolicy, "v1", json!({})).is_none());
}

#[test]
fn a_rule_body_and_exclude_blob_are_dropped_at_parse() {
    let resource = resource_from(Kind::ClusterPolicy, "v1", cluster_policy_json());
    let inventory = Inventory {
        cluster_policies: KindSet::Served {
            items: vec![resource],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    let text = leak_surface(&inventory);
    assert!(
        !text.contains(PLANTED),
        "a planted token in validate/exclude must not reach Debug, table, or render: {text}"
    );
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = json!({
        "metadata": {
            "name": huge,
            "annotations": { "policies.kyverno.io/severity": huge }
        },
        "spec": {
            "validationFailureAction": huge,
            "rules": [{ "match": { "resources": { "kinds": [huge] } } }]
        }
    });
    let resource = resource_from(Kind::ClusterPolicy, "v1", value);
    for field in [
        &resource.name,
        &resource.validation_failure_action,
        &resource.severity,
        &resource.rule_kinds[0],
    ] {
        assert!(
            field.chars().count() <= MAX_FIELD_CHARS + 1,
            "every field is clipped where it is carried: {} chars",
            field.chars().count()
        );
        assert!(field.ends_with('\u{2026}'), "and looks clipped");
    }
}

#[test]
fn the_listing_cap_is_stated_when_it_bites() {
    let values =
        (0..=MAX_OBJECTS).map(|index| json!({ "metadata": { "name": format!("policy-{index}") } }));
    let (items, truncated, unreadable) = collect_items(Kind::ClusterPolicy, "v1", values);
    assert_eq!(items.len(), MAX_OBJECTS);
    assert!(truncated);
    assert_eq!(unreadable, 0);
}

#[test]
fn rule_kinds_stop_at_the_kind_cap() {
    let kinds: Vec<String> = (0..=MAX_RULE_KINDS)
        .map(|index| format!("Kind{index}"))
        .collect();
    let value = json!({
        "metadata": { "name": "many" },
        "spec": { "rules": [{ "match": { "resources": { "kinds": kinds } } }] }
    });
    let resource = resource_from(Kind::ClusterPolicy, "v1", value);
    assert_eq!(resource.rule_kinds.len(), MAX_RULE_KINDS);
}

#[test]
fn a_missing_group_is_invisible_and_a_forbidden_one_is_denied() {
    assert!(matches!(
        after_group(&api_error(404)),
        GroupAnswer::NotServed
    ));
    assert!(
        matches!(after_group(&api_error(403)), GroupAnswer::Denied),
        "a 403 is Denied, never an empty inventory that looks like Kyverno is absent"
    );
    assert!(matches!(after_group(&api_error(401)), GroupAnswer::Denied));
    assert!(matches!(
        after_group(&api_error(500)),
        GroupAnswer::Failed(_)
    ));
    assert!(matches!(after_list(&api_error(404)), ListErr::NotFound));
    assert!(matches!(after_list(&api_error(403)), ListErr::Denied));
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn cluster_policy_lists_are_cluster_scoped_even_when_a_namespace_is_named() {
    assert_eq!(
        collection_url(Kind::ClusterPolicy, "v1", Some("prod")),
        "/apis/kyverno.io/v1/clusterpolicies"
    );
    assert_eq!(
        collection_url(Kind::Policy, "v1", Some("prod")),
        "/apis/kyverno.io/v1/namespaces/prod/policies"
    );
    assert_eq!(
        collection_url(Kind::Policy, "v1", None),
        "/apis/kyverno.io/v1/policies"
    );
    assert_eq!(
        collection_url(Kind::CleanupPolicy, "v2", Some("prod")),
        "/apis/kyverno.io/v2/namespaces/prod/cleanuppolicies"
    );
    assert_eq!(
        collection_url(Kind::ValidatingPolicy, "v1", Some("prod")),
        "/apis/policies.kyverno.io/v1/validatingpolicies"
    );
    assert_eq!(
        collection_url(Kind::NamespacedValidatingPolicy, "v1", Some("prod")),
        "/apis/policies.kyverno.io/v1/namespaces/prod/namespacedvalidatingpolicies"
    );
    assert_eq!(
        collection_url(Kind::PolicyException, "v1", None),
        "/apis/policies.kyverno.io/v1/policyexceptions"
    );
    assert_eq!(
        collection_url(Kind::LegacyPolicyException, "v2", Some("prod")),
        "/apis/kyverno.io/v2/namespaces/prod/policyexceptions"
    );
}

#[test]
fn cleanup_versions_are_those_the_group_document_named() {
    assert!(versions_for(Kind::CleanupPolicy, &["v1".into()]).is_empty());
    assert_eq!(
        versions_for(Kind::CleanupPolicy, &["v1".into(), "v2".into()]),
        vec!["v2".to_string()]
    );
    assert_eq!(
        versions_for(Kind::ClusterCleanupPolicy, &["v2beta1".into()]),
        vec!["v2beta1".to_string()]
    );
    assert_eq!(
        versions_for(Kind::ClusterPolicy, &["v1".into()]),
        vec!["v1".to_string()]
    );
    assert_eq!(
        versions_for(Kind::ValidatingPolicy, &["v1".into()]),
        vec!["v1".to_string(), "v1beta1".to_string()]
    );
    assert_eq!(
        versions_for(Kind::ValidatingPolicy, &[]),
        vec!["v1".to_string(), "v1beta1".to_string()]
    );
    assert_eq!(
        versions_for(Kind::LegacyPolicyException, &["v1".into()]),
        vec![
            "v1".to_string(),
            "v2".to_string(),
            "v2beta1".to_string(),
            "v2alpha1".to_string()
        ]
    );
}

#[test]
fn an_unserved_kyverno_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "a 404 group is absence, not an empty list"
    );
}

#[test]
fn a_denied_kyverno_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        cluster_policies: KindSet::Denied,
        ..Inventory::default()
    })
    .expect("Denied is served, so the table exists");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("access denied for this account"),
        "a 403 stays labelled: {text}"
    );
    assert!(text.contains("ClusterPolicy"), "{text}");
}

#[test]
fn a_served_kyverno_fixture_is_one_row_per_object() {
    let policy = resource_from(Kind::ClusterPolicy, "v1", cluster_policy_json());
    let page = table_page(&Inventory {
        cluster_policies: KindSet::Served {
            items: vec![policy],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "require-labels");
    assert_eq!(page.rows[0].cells[0], "ClusterPolicy");
    assert_eq!(page.rows[0].cells[3], "Ready");
    assert_eq!(page.rows[0].cells[4], "Enforce");
    assert_eq!(page.rows[0].cells[6], "Pod, Deployment, Namespace");
    assert!(!page.rows[0].cells.join(" ").contains(PLANTED));
}

#[test]
fn a_missing_kyverno_group_renders_as_not_installed_and_points_at_policy_reports() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served());
    assert_eq!(lines[0], "Kyverno is not served by this cluster");
    let text = lines.join("\n");
    assert!(text.contains("ClusterPolicy"), "{text}");
    assert!(
        text.contains("PolicyReport"),
        "findings stay in PolicyReport: {text}"
    );
    assert!(text.contains("nothing is installed to find them"), "{text}");
}

#[test]
fn an_inventory_that_could_not_read_anything_does_not_claim_there_is_nothing() {
    let lines = render(&Inventory {
        cluster_policies: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 3,
        },
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(
        !text.contains("no Kyverno policies are stored"),
        "three are stored and were seen: {text}"
    );
    assert!(lines[0].contains("though some are stored"), "{text}");
}

#[test]
fn undecodable_objects_are_stated_even_when_another_kind_is_denied() {
    let lines = render(&Inventory {
        cluster_policies: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 3,
        },
        policies: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(
        lines[0].contains("though some are stored"),
        "a denied kind must not swallow the undecodable count: {text}"
    );
    assert!(
        text.contains("kyverno policies: access denied for this account"),
        "{text}"
    );
}

#[test]
fn both_denied_exception_kinds_are_two_rows_with_their_own_uids() {
    let page = table_page(&Inventory {
        legacy_policy_exceptions: KindSet::Denied,
        policy_exceptions: KindSet::Denied,
        ..Inventory::default()
    })
    .expect("Denied is served, so the table exists");
    assert_eq!(page.rows.len(), 2);
    assert_ne!(
        page.rows[0].uid, page.rows[1].uid,
        "each group's PolicyException keeps its own denied row"
    );
}

#[test]
fn a_history_renders_ready_action_kinds_and_states_a_cap() {
    let cluster = resource_from(Kind::ClusterPolicy, "v1", cluster_policy_json());
    let namespaced = resource_from(Kind::Policy, "v1", namespaced_policy_json());
    let lines = render(&Inventory {
        cluster_policies: KindSet::Served {
            items: vec![cluster],
            truncated: true,
            unreadable: 2,
        },
        policies: KindSet::Served {
            items: vec![namespaced],
            truncated: false,
            unreadable: 0,
        },
        cleanup_policies: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.starts_with("2 Kyverno policies"), "{text}");
    assert!(text.contains("require-labels"), "{text}");
    assert!(text.contains("prod/deny-latest"), "{text}");
    assert!(text.contains("Enforce"), "{text}");
    assert!(text.contains("Pod, Deployment, Namespace"), "{text}");
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
    assert!(
        text.contains("kyverno cleanuppolicies: access denied for this account"),
        "{text}"
    );
    assert!(
        !text.contains("CleanupPolicy\n") && !text.contains("stale-pods"),
        "a kind the group did not serve stays invisible: {text}"
    );
    assert!(!text.contains(PLANTED), "{text}");
}

#[test]
fn a_cel_validating_policy_keeps_actions_ready_kinds_and_background() {
    let resource = resource_from(Kind::ValidatingPolicy, "v1", validating_policy_json());
    assert_eq!(resource.name, "check-labels");
    assert_eq!(resource.namespace, "");
    assert_eq!(resource.uid, "uid-vpol");
    assert_eq!(resource.background, Some(true));
    assert_eq!(resource.validation_failure_action, "Deny, Audit");
    assert_eq!(resource.ready, "True");
    assert_eq!(resource.rule_count, 1);
    assert_eq!(resource.rule_kinds, vec!["pods", "Pod"]);
    assert_eq!(resource.severity, "medium");
}

#[test]
fn a_cel_policy_with_admission_disabled_does_not_borrow_that_flag_for_background() {
    let resource = resource_from(
        Kind::ValidatingPolicy,
        "v1",
        json!({
            "metadata": { "name": "check" },
            "spec": {
                "evaluation": { "admission": { "enabled": false } },
                "validations": [{ "expression": PLANTED }]
            }
        }),
    );
    assert_eq!(
        resource.background, None,
        "admission.enabled is an independent flag; unset background carries no tag"
    );
    let inventory = Inventory {
        validating_policies: KindSet::Served {
            items: vec![resource],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    let text = render(&inventory).join("\n");
    assert!(
        !text.contains("admission-only"),
        "a disabled admission flag is not a disabled background flag: {text}"
    );
}

#[test]
fn a_cel_expression_is_dropped_at_parse() {
    let resource = resource_from(Kind::ValidatingPolicy, "v1", validating_policy_json());
    let inventory = Inventory {
        validating_policies: KindSet::Served {
            items: vec![resource],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    let text = leak_surface(&inventory);
    assert!(
        !text.contains(PLANTED),
        "a planted CEL expression must not reach Debug, table, or render: {text}"
    );
}

#[test]
fn a_cel_available_condition_is_ready_when_ready_is_absent() {
    let resource = resource_from(
        Kind::ValidatingPolicy,
        "v1",
        json!({
            "metadata": { "name": "check" },
            "spec": {
                "validationActions": ["Deny"],
                "matchConstraints": {
                    "resourceRules": [{ "resources": ["deployments"] }]
                },
                "validations": [{ "expression": PLANTED }]
            },
            "status": { "conditions": [{ "type": "Available", "status": "True" }] }
        }),
    );
    assert_eq!(resource.ready, "True");
    assert_eq!(resource.rule_kinds, vec!["deployments"]);
    assert!(!format!("{resource:?}").contains(PLANTED));
}

#[test]
fn a_cel_404_does_not_hide_a_served_legacy_group() {
    let policy = resource_from(Kind::ClusterPolicy, "v1", cluster_policy_json());
    let inventory = Inventory {
        cluster_policies: KindSet::Served {
            items: vec![policy],
            truncated: false,
            unreadable: 0,
        },
        validating_policies: KindSet::NotServed,
        ..Inventory::default()
    };
    assert!(
        inventory.served(),
        "policies.kyverno.io 404 must not hide kyverno.io"
    );
    assert!(matches!(inventory.validating_policies, KindSet::NotServed));
    let page = table_page(&inventory).expect("legacy served is a table");
    assert_eq!(page.rows[0].name, "require-labels");
}

#[test]
fn a_served_cel_group_is_visible_when_legacy_is_absent() {
    let policy = resource_from(Kind::ValidatingPolicy, "v1", validating_policy_json());
    let inventory = Inventory {
        validating_policies: KindSet::Served {
            items: vec![policy],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    assert!(inventory.served());
    assert!(matches!(inventory.cluster_policies, KindSet::NotServed));
    assert!(table_page(&inventory).is_some());
}

#[test]
fn both_groups_404_is_invisible() {
    let inventory = Inventory::default();
    assert!(!inventory.served());
    assert!(matches!(inventory.cluster_policies, KindSet::NotServed));
    assert!(matches!(inventory.validating_policies, KindSet::NotServed));
    assert!(
        table_page(&inventory).is_none(),
        "both groups 404 is absence, not an empty list"
    );
}
