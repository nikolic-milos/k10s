//! Parsing PolicyReport JSON, severity mapping, overlay tints, caps, and
//! the 404/403 classification.

use super::*;
use serde_json::json;

fn kyverno_report() -> Value {
    json!({
        "apiVersion": "wgpolicyk8s.io/v1alpha2",
        "kind": "PolicyReport",
        "metadata": {
            "name": "cpol-disallow-privileged",
            "namespace": "prod"
        },
        "results": [
            {
                "policy": "disallow-privileged",
                "rule": "privileged-containers",
                "result": "fail",
                "severity": "high",
                "message": "privileged is not allowed",
                "resources": [
                    {
                        "apiVersion": "v1",
                        "kind": "Pod",
                        "name": "api",
                        "namespace": "prod",
                        "uid": "pod-api"
                    }
                ]
            },
            {
                "policy": "require-labels",
                "result": "pass",
                "severity": "low",
                "resources": [
                    {
                        "kind": "Pod",
                        "name": "api",
                        "uid": "pod-api"
                    }
                ]
            },
            {
                "policy": "disallow-latest",
                "result": "warn",
                "severity": "medium",
                "resources": [
                    {
                        "kind": "Pod",
                        "name": "worker",
                        "uid": "pod-worker"
                    }
                ]
            }
        ]
    })
}

fn cluster_report() -> Value {
    json!({
        "apiVersion": "wgpolicyk8s.io/v1alpha2",
        "kind": "ClusterPolicyReport",
        "metadata": { "name": "cluster-ns-labels" },
        "results": [
            {
                "policy": "require-ns-labels",
                "result": "fail",
                "severity": "critical",
                "resources": [
                    { "kind": "Namespace", "name": "kube-public", "uid": "ns-public" }
                ]
            }
        ]
    })
}

fn inventory(reports: Vec<Value>) -> Inventory {
    let (parsed, truncated) = ingest_items(reports, 0, 0);
    finalize(Inventory {
        served: true,
        reports: parsed,
        truncated,
    })
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn a_policy_report_extracts_identity_results_and_the_resource_they_name() {
    let report = parse_report(&kyverno_report());
    assert_eq!(report.namespace, "prod");
    assert_eq!(report.name, "cpol-disallow-privileged");
    assert_eq!(report.results.len(), 3);
    assert_eq!(report.results[0].policy, "disallow-privileged");
    assert_eq!(report.results[0].result, "fail");
    assert_eq!(report.results[0].severity, Severity::Err);
    assert_eq!(report.results[0].resource_name, "api");
    assert_eq!(report.results[0].resource_kind, "Pod");
    assert_eq!(report.results[0].resource_uid, "pod-api");
}

#[test]
fn a_cluster_policy_report_has_no_namespace() {
    let report = parse_report(&cluster_report());
    assert_eq!(report.namespace, "");
    assert_eq!(report.name, "cluster-ns-labels");
    assert_eq!(report.results[0].resource_kind, "Namespace");
}

#[test]
fn a_result_with_several_resources_fans_out_one_finding_each() {
    let report = parse_report(&json!({
        "metadata": { "name": "multi", "namespace": "dev" },
        "results": [{
            "policy": "disallow-root",
            "result": "fail",
            "severity": "high",
            "resources": [
                { "kind": "Pod", "name": "a", "uid": "uid-a" },
                { "kind": "Pod", "name": "b", "uid": "uid-b" }
            ]
        }]
    }));
    assert_eq!(report.results.len(), 2);
    assert_eq!(report.results[0].resource_uid, "uid-a");
    assert_eq!(report.results[1].resource_uid, "uid-b");
    assert_eq!(report.results[1].policy, "disallow-root");
}

#[test]
fn severity_words_map_onto_the_overlay_axis() {
    assert_eq!(map_severity("critical"), Severity::Err);
    assert_eq!(map_severity("HIGH"), Severity::Err);
    assert_eq!(map_severity("medium"), Severity::Warn);
    assert_eq!(map_severity("low"), Severity::Warn);
    assert_eq!(map_severity("info"), Severity::Ok);
    assert_eq!(map_severity(""), Severity::Unknown);
    assert_eq!(map_severity("exotic"), Severity::Unknown);
}

#[test]
fn overlay_tints_roll_up_the_worst_finding_per_uid_and_skip_passes() {
    let tints = inventory(vec![kyverno_report()]).tints();
    assert_eq!(
        tints,
        vec![
            ("pod-api".to_string(), Severity::Err),
            ("pod-worker".to_string(), Severity::Warn),
        ]
    );
}

#[test]
fn an_empty_uid_cannot_tint_and_an_unserved_inventory_stays_invisible() {
    let report = parse_report(&json!({
        "metadata": { "name": "orphan" },
        "results": [{ "policy": "x", "result": "fail", "severity": "high" }]
    }));
    assert!(report.results[0].resource_uid.is_empty());
    let served = Inventory {
        served: true,
        reports: vec![report],
        truncated: false,
    };
    assert!(served.tints().is_empty());
    assert!(Inventory::unserved().tints().is_empty());
}

#[test]
fn a_404_is_not_served_and_a_403_is_denied() {
    assert_eq!(after_list(&api_error(404)), ListOutcome::NotServed);
    assert_eq!(after_list(&api_error(401)), ListOutcome::Denied);
    assert_eq!(after_list(&api_error(403)), ListOutcome::Denied);
    assert!(matches!(
        after_list(&api_error(500)),
        ListOutcome::Failed(_)
    ));
}

#[test]
fn both_kinds_missing_is_invisible_and_a_denial_wins() {
    assert_eq!(
        combine(ListOutcome::NotServed, ListOutcome::NotServed),
        Fetched::Ok(Inventory::unserved())
    );
    let ready = ListOutcome::Items {
        reports: vec![parse_report(&cluster_report())],
        truncated: false,
    };
    match combine(ready.clone(), ListOutcome::NotServed) {
        Fetched::Ok(inventory) => {
            assert!(inventory.served);
            assert_eq!(inventory.reports.len(), 1);
        }
        other => panic!("a served kind is visible, got {other:?}"),
    }
    assert_eq!(
        combine(ready, ListOutcome::Denied),
        Fetched::Denied {
            what: "policy reports"
        }
    );
}

#[test]
fn a_result_without_a_severity_word_falls_back_to_the_result_word() {
    let report = parse_report(&json!({
        "metadata": { "name": "bare" },
        "results": [
            { "policy": "a", "result": "fail", "resources": [{ "uid": "u1", "kind": "Pod", "name": "p" }] },
            { "policy": "b", "result": "skip", "resources": [{ "uid": "u2", "kind": "Pod", "name": "q" }] }
        ]
    }));
    assert_eq!(report.results[0].severity, Severity::Err);
    assert_eq!(report.results[1].severity, Severity::Unknown);
    let tints = Inventory {
        served: true,
        reports: vec![report],
        truncated: false,
    }
    .tints();
    assert_eq!(tints, vec![("u1".to_string(), Severity::Err)]);
}

#[test]
fn caps_stop_the_listing_rather_than_growing_without_bound() {
    let items: Vec<Value> = (0..(MAX_REPORTS + 5))
        .map(|i| {
            json!({
                "metadata": { "name": format!("r{i}"), "namespace": "ns" },
                "results": [{
                    "policy": "p",
                    "result": "fail",
                    "severity": "high",
                    "resources": [{ "kind": "Pod", "name": "x", "uid": format!("u{i}") }]
                }]
            })
        })
        .collect();
    let held = inventory(items);
    assert!(held.truncated);
    assert_eq!(held.reports.len(), MAX_REPORTS);
}

#[test]
fn a_findings_cap_truncates_inside_a_report() {
    let resources: Vec<Value> = (0..32)
        .map(|i| json!({ "uid": format!("u{i}"), "kind": "Pod", "name": "p" }))
        .collect();
    let item = json!({
        "metadata": { "name": "big" },
        "results": [{
            "policy": "p",
            "result": "fail",
            "severity": "high",
            "resources": resources
        }]
    });
    let (reports, truncated) = ingest_items(vec![item], 0, MAX_FINDINGS - 3);
    assert!(truncated);
    assert_eq!(reports[0].results.len(), 3);
}

#[test]
fn a_page_over_the_byte_cap_is_refused_rather_than_parsed() {
    let huge = "x".repeat(MAX_PAGE_BYTES + 1);
    assert!(matches!(parse_list(&huge), Err(PageError::TooLarge)));
    assert!(matches!(parse_list("{"), Err(PageError::NotJson(_))));
}

#[test]
fn one_enormous_field_is_clipped_to_a_line() {
    let huge = "p".repeat(MAX_FIELD_CHARS + 80);
    let report = parse_report(&json!({
        "metadata": { "name": huge, "namespace": "ns" },
        "results": [{ "policy": huge, "result": "fail", "severity": "high" }]
    }));
    assert_eq!(report.name.chars().count(), MAX_FIELD_CHARS);
    assert_eq!(report.results[0].policy.chars().count(), MAX_FIELD_CHARS);
}
