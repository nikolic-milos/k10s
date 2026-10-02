//! Kyverno CRs listed through kube Request: group probe, 404 vs 403, the
//! wire path, a planted rule-body token, table presence, and paging.

use crate::*;
use k10s_data::kyverno::{self, CEL_GROUP, GROUP, KindSet};
use k10s_data::read::Fetched;

const PLANTED: &str = "PLANTED-TOKEN-9f3a";

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn group_v1() -> String {
    r#"{"kind":"APIGroup","name":"kyverno.io",
        "versions":[{"groupVersion":"kyverno.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"kyverno.io/v1","version":"v1"}}"#
        .to_string()
}

fn group_v1_v2() -> String {
    r#"{"kind":"APIGroup","name":"kyverno.io",
        "versions":[
            {"groupVersion":"kyverno.io/v1","version":"v1"},
            {"groupVersion":"kyverno.io/v2","version":"v2"}
        ],
        "preferredVersion":{"groupVersion":"kyverno.io/v1","version":"v1"}}"#
        .to_string()
}

fn cel_group_v1() -> String {
    r#"{"kind":"APIGroup","name":"policies.kyverno.io",
        "versions":[{"groupVersion":"policies.kyverno.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"policies.kyverno.io/v1","version":"v1"}}"#
        .to_string()
}

fn cluster_policy_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": {
            "name": "require-labels",
            "uid": "uid-cpol",
            "annotations": { "policies.kyverno.io/severity": "medium" }
        },
        "spec": {
            "background": true,
            "validationFailureAction": "Enforce",
            "rules": [{
                "name": "check",
                "match": { "any": [{ "resources": { "kinds": ["Pod"] } }] },
                "exclude": { "resources": { "namespaces": [PLANTED] } },
                "validate": { "pattern": { "metadata": { "labels": { "token": PLANTED } } } }
            }]
        },
        "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
    })
}

fn list(kind: &str, items: &[serde_json::Value]) -> String {
    list_on("kyverno.io/v1", kind, items)
}

fn list_on(api_version: &str, kind: &str, items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": format!("{kind}List"),
        "apiVersion": api_version,
        "metadata": {},
        "items": items
    })
    .to_string()
}

fn validating_policy_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "check-labels", "uid": "uid-vpol" },
        "spec": {
            "validationActions": ["Deny"],
            "evaluation": { "background": { "enabled": true } },
            "matchConstraints": {
                "resourceRules": [{
                    "apiGroups": [""],
                    "apiVersions": ["v1"],
                    "operations": ["CREATE", "UPDATE"],
                    "resources": ["pods"]
                }]
            },
            "validations": [{
                "message": "label required",
                "expression": PLANTED
            }]
        },
        "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
    })
}

fn legacy_exception_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "delta-exception", "namespace": "delta", "uid": "uid-polex" },
        "spec": {
            "exceptions": [{
                "policyName": "disallow-host-namespaces",
                "ruleNames": ["host-namespaces"]
            }],
            "match": { "any": [{ "resources": { "kinds": ["Pod"] } }] }
        }
    })
}

fn probed_group(script: &Script, group: &str) -> bool {
    script.seen().iter().any(|request| {
        request.path == format!("/apis/{group}")
            || request.path.starts_with(&format!("/apis/{group}?"))
    })
}

#[test]
fn a_404_kyverno_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { kyverno::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.cluster_policies, KindSet::NotServed));
    assert!(matches!(inventory.validating_policies, KindSet::NotServed));
    assert!(kyverno::table_page(&inventory).is_none());
    assert!(
        probed_group(&script, GROUP) && probed_group(&script, CEL_GROUP),
        "both groups are probed: {:?}",
        script.seen()
    );
    assert!(
        script.requests_for("clusterpolicies").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script.requests_for("validatingpolicies").is_empty(),
        "a 404 CEL group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_kyverno_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/kyverno.io", 403, status(403, "Forbidden"));
    let runtime = runtime();
    let fetched = runtime.block_on(async { kyverno::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.cluster_policies, KindSet::Denied));
    assert!(
        inventory.cluster_policies.served(),
        "403 is Denied, not served: false"
    );
    assert!(kyverno::table_page(&inventory).is_some());
    assert!(
        script.requests_for("clusterpolicies").is_empty(),
        "a 403 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn kyverno_objects_are_listed_from_the_crs_and_a_v1_group_does_not_ask_for_cleanup() {
    let script = Script::default();
    script.route("GET", "/apis/kyverno.io", 200, group_v1());
    script.route(
        "GET",
        "/apis/kyverno.io/v1/clusterpolicies?",
        200,
        list("ClusterPolicy", &[cluster_policy_item()]),
    );
    script.route(
        "GET",
        "/apis/kyverno.io/v1/policies?",
        200,
        list("Policy", &[]),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { kyverno::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    let policy = &inventory.cluster_policies.items()[0];
    assert_eq!(policy.name, "require-labels");
    assert_eq!(policy.validation_failure_action, "Enforce");
    assert_eq!(policy.rule_kinds, vec!["Pod"]);
    assert!(matches!(inventory.cleanup_policies, KindSet::NotServed));
    assert!(
        matches!(inventory.validating_policies, KindSet::NotServed),
        "a 404 on policies.kyverno.io leaves CEL kinds unserved"
    );
    assert!(kyverno::table_page(&inventory).is_some());
    assert!(
        probed_group(&script, CEL_GROUP),
        "the CEL group is probed even when only kyverno.io is served: {:?}",
        script.seen()
    );
    assert!(
        script.requests_for("validatingpolicies").is_empty(),
        "a 404 CEL group must not be listed: {:?}",
        script.seen()
    );

    let seen = script.seen();
    assert!(
        seen.iter()
            .any(|request| request.path.starts_with("/apis/kyverno.io")
                && !request.path.contains("clusterpolicies")
                && !request.path.contains("policies")),
        "the group document is probed: {seen:?}"
    );
    assert_eq!(script.requests_for("clusterpolicies").len(), 1);
    assert!(
        script.requests_for("cleanuppolicies").is_empty(),
        "cleanup is listed only when the group names v2: {seen:?}"
    );
    let surface = format!("{:?}{}", inventory, kyverno::render(&inventory).join("\n"));
    assert!(!surface.contains(PLANTED), "{surface}");
    drop(runtime);
}

#[test]
fn a_v2_group_lists_cleanup_policies_and_follows_a_continue_token() {
    let script = Script::default();
    script.route("GET", "/apis/kyverno.io", 200, group_v1_v2());
    script.route(
        "GET",
        "/apis/kyverno.io/v1/clusterpolicies?",
        200,
        serde_json::json!({
            "kind": "ClusterPolicyList",
            "metadata": { "continue": "page-2" },
            "items": [cluster_policy_item()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/kyverno.io/v1/clusterpolicies?",
        200,
        list(
            "ClusterPolicy",
            &[serde_json::json!({
                "metadata": { "name": "deny-root" },
                "spec": { "rules": [{ "match": { "resources": { "kinds": ["Pod"] } } }] }
            })],
        ),
    );
    script.route(
        "GET",
        "/apis/kyverno.io/v1/policies?",
        200,
        list("Policy", &[]),
    );
    script.route(
        "GET",
        "/apis/kyverno.io/v2/cleanuppolicies?",
        200,
        serde_json::json!({
            "kind": "CleanupPolicyList",
            "items": [{
                "metadata": { "name": "stale", "namespace": "prod" },
                "spec": { "match": { "any": [{ "resources": { "kinds": ["Pod"] } }] } }
            }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/kyverno.io/v2/clustercleanuppolicies?",
        200,
        list("ClusterCleanupPolicy", &[]),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { kyverno::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    assert_eq!(
        inventory
            .cluster_policies
            .items()
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["require-labels", "deny-root"]
    );
    assert_eq!(inventory.cleanup_policies.items()[0].name, "stale");
    let lists = script.requests_for("clusterpolicies");
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
    let page = kyverno::table_page(&inventory).expect("served");
    assert!(page.rows.iter().any(|row| row.name == "require-labels"));
    drop(runtime);
}

#[test]
fn a_legacy_policy_exception_is_listed_from_kyverno_io_v2_when_the_cel_group_404s() {
    let script = Script::default();
    script.route("GET", "/apis/kyverno.io", 200, group_v1_v2());
    script.route(
        "GET",
        "/apis/kyverno.io/v2/policyexceptions?",
        200,
        list_on(
            "kyverno.io/v2",
            "PolicyException",
            &[legacy_exception_item()],
        ),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { kyverno::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served legacy exception listing must resolve: {fetched:?}");
    };
    let exception = &inventory.legacy_policy_exceptions.items()[0];
    assert_eq!(exception.name, "delta-exception");
    assert_eq!(exception.namespace, "delta");
    assert_eq!(exception.rule_count, 1);
    assert!(
        matches!(inventory.policy_exceptions, KindSet::NotServed),
        "the 404 CEL group stays its own state, not the legacy exceptions'"
    );
    let page = kyverno::table_page(&inventory).expect("served");
    assert!(
        page.rows
            .iter()
            .any(|row| row.name == "delta-exception" && row.cells[0] == "PolicyException"),
        "{:?}",
        page.rows
    );
    assert!(
        kyverno::render(&inventory)
            .join("\n")
            .contains("delta/delta-exception"),
        "the exception that neuters a policy is visible"
    );
    drop(runtime);
}

#[test]
fn a_404_legacy_group_with_cel_served_is_visible() {
    let script = Script::default();
    script.route("GET", "/apis/policies.kyverno.io", 200, cel_group_v1());
    script.route(
        "GET",
        "/apis/policies.kyverno.io/v1/validatingpolicies?",
        200,
        list_on(
            "policies.kyverno.io/v1",
            "ValidatingPolicy",
            &[validating_policy_item()],
        ),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { kyverno::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served CEL listing must resolve: {fetched:?}");
    };
    assert!(inventory.served());
    assert!(matches!(inventory.cluster_policies, KindSet::NotServed));
    assert_eq!(
        inventory.validating_policies.items()[0].name,
        "check-labels"
    );
    assert_eq!(
        inventory.validating_policies.items()[0].validation_failure_action,
        "Deny"
    );
    assert_eq!(
        inventory.validating_policies.items()[0].rule_kinds,
        vec!["pods"]
    );
    assert_eq!(inventory.validating_policies.items()[0].rule_count, 1);
    assert!(kyverno::table_page(&inventory).is_some());
    assert!(
        script.requests_for("clusterpolicies").is_empty(),
        "a 404 kyverno.io group must not be listed: {:?}",
        script.seen()
    );
    let surface = format!("{:?}{}", inventory, kyverno::render(&inventory).join("\n"));
    assert!(!surface.contains(PLANTED), "{surface}");
    drop(runtime);
}
