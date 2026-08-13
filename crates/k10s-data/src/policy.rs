//! PolicyReport / ClusterPolicyReport from Kyverno or Gatekeeper.
//!
//! Both engines publish the same `wgpolicyk8s.io` CRDs. This module reads those
//! documents as JSON, because the group is not in `k8s-openapi` and installing
//! a typed CRD client would be an install. A 404 means the CRDs are not served
//! and the overlay stays off; a 403 is a labelled denial. Caps bound how many
//! reports and findings we hold, and say so when they bite.

use std::collections::BTreeMap;

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use k10s_core::Severity;

use crate::read::Fetched;

const POLICY_REPORTS: &str = "/apis/wgpolicyk8s.io/v1alpha2/policyreports";
const CLUSTER_POLICY_REPORTS: &str = "/apis/wgpolicyk8s.io/v1alpha2/clusterpolicyreports";

const PAGE_LIMIT: u32 = 200;
pub const MAX_PAGE_BYTES: usize = 8 << 20;

/// Ceiling on reports held from both kinds together.
pub const MAX_REPORTS: usize = 1_000;
/// Ceiling on flattened findings held from both kinds together.
pub const MAX_FINDINGS: usize = 8_192;
pub const MAX_FIELD_CHARS: usize = 200;
const MAX_RESOURCES_PER_RESULT: usize = 256;

/// One PolicyReport or ClusterPolicyReport, reduced to what an overlay needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Empty on a ClusterPolicyReport.
    pub namespace: String,
    pub name: String,
    pub results: Vec<Finding>,
}

/// One policy result, fanned out to a single resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub policy: String,
    /// The engine's own word: pass, fail, warn, error, skip.
    pub result: String,
    pub severity: Severity,
    pub resource_name: String,
    pub resource_kind: String,
    pub resource_uid: String,
}

/// What a fetch held, or the reason it held nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    /// False when the group is not served. The overlay must stay invisible.
    pub served: bool,
    pub reports: Vec<Report>,
    pub truncated: bool,
}

impl Inventory {
    fn unserved() -> Inventory {
        Inventory::default()
    }

    /// Worst finding per resource uid, for OverlayMark.tint.
    /// Pass and skip do not stamp. An empty uid cannot be joined to a scene object.
    pub fn tints(&self) -> Vec<(String, Severity)> {
        if !self.served {
            return Vec::new();
        }
        let mut by_uid: BTreeMap<String, Severity> = BTreeMap::new();
        for report in &self.reports {
            for finding in &report.results {
                if finding.resource_uid.is_empty() {
                    continue;
                }
                let Some(tint) = tint_of(finding) else {
                    continue;
                };
                by_uid
                    .entry(finding.resource_uid.clone())
                    .and_modify(|held| *held = held.rollup(tint))
                    .or_insert(tint);
            }
        }
        by_uid.into_iter().collect()
    }
}

/// Map a PolicyReport `severity` field onto the overlay axis.
pub fn map_severity(raw: &str) -> Severity {
    match raw.trim().to_ascii_lowercase().as_str() {
        "critical" | "high" | "error" | "err" => Severity::Err,
        "medium" | "med" | "low" | "warning" | "warn" => Severity::Warn,
        "info" | "informational" | "none" => Severity::Ok,
        "" => Severity::Unknown,
        _ => Severity::Unknown,
    }
}

/// List namespaced PolicyReports and cluster-scoped ClusterPolicyReports.
pub async fn fetch_reports(client: &Client) -> Fetched<Inventory> {
    let namespaced = list_kind(client, POLICY_REPORTS).await;
    let cluster = list_kind(client, CLUSTER_POLICY_REPORTS).await;
    combine(namespaced, cluster)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ListOutcome {
    Items {
        reports: Vec<Report>,
        truncated: bool,
    },
    NotServed,
    Denied,
    Failed(String),
}

async fn list_kind(client: &Client, path: &'static str) -> ListOutcome {
    let mut reports = Vec::new();
    let mut token: Option<String> = None;
    let mut truncated = false;
    let mut findings = 0usize;
    loop {
        if reports.len() >= MAX_REPORTS || findings >= MAX_FINDINGS {
            truncated = true;
            break;
        }
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let request = match Request::new(path).list(&params) {
            Ok(request) => request,
            Err(error) => return ListOutcome::Failed(error.to_string()),
        };
        let text = match client.request_text(request).await {
            Ok(text) => text,
            Err(error) => return after_list(&error),
        };
        let page = match parse_list(&text) {
            Ok(page) => page,
            Err(PageError::TooLarge) => {
                return ListOutcome::Failed(
                    "the list page is larger than 8 MiB; the page is not shown".to_string(),
                );
            }
            Err(PageError::NotJson(why)) => {
                return ListOutcome::Failed(format!("the list is not JSON: {why}"));
            }
        };
        let (page_reports, page_truncated) = ingest_items(page.items, reports.len(), findings);
        truncated |= page_truncated;
        for report in page_reports {
            findings += report.results.len();
            reports.push(report);
        }
        token = (!page.metadata.cont.is_empty() && !truncated).then_some(page.metadata.cont);
        if token.is_none() {
            break;
        }
    }
    ListOutcome::Items { reports, truncated }
}

fn after_list(error: &kube::Error) -> ListOutcome {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return ListOutcome::Denied;
        }
        if response.code == 404 {
            return ListOutcome::NotServed;
        }
    }
    ListOutcome::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

fn combine(namespaced: ListOutcome, cluster: ListOutcome) -> Fetched<Inventory> {
    use ListOutcome::*;
    match (namespaced, cluster) {
        (Denied, _) | (_, Denied) => Fetched::Denied {
            what: "policy reports",
        },
        (Failed(why), _) | (_, Failed(why)) => Fetched::Failed {
            what: "policy reports",
            why,
        },
        (NotServed, NotServed) => Fetched::Ok(Inventory::unserved()),
        (Items { reports, truncated }, NotServed) | (NotServed, Items { reports, truncated }) => {
            Fetched::Ok(finalize(Inventory {
                served: true,
                reports,
                truncated,
            }))
        }
        (
            Items {
                reports: mut namespaced,
                truncated: t1,
            },
            Items {
                reports: cluster,
                truncated: t2,
            },
        ) => {
            namespaced.extend(cluster);
            Fetched::Ok(finalize(Inventory {
                served: true,
                reports: namespaced,
                truncated: t1 || t2,
            }))
        }
    }
}

fn finalize(mut inventory: Inventory) -> Inventory {
    let mut findings = 0usize;
    let mut kept = Vec::new();
    for mut report in inventory.reports {
        if kept.len() >= MAX_REPORTS || findings >= MAX_FINDINGS {
            inventory.truncated = true;
            break;
        }
        let remaining = MAX_FINDINGS - findings;
        if report.results.len() > remaining {
            report.results.truncate(remaining);
            inventory.truncated = true;
        }
        findings += report.results.len();
        kept.push(report);
    }
    inventory.reports = kept;
    inventory
}

fn ingest_items(
    items: Vec<Value>,
    reports_already: usize,
    findings_already: usize,
) -> (Vec<Report>, bool) {
    let mut reports = Vec::new();
    let mut truncated = false;
    let mut findings = findings_already;
    for item in items {
        if reports_already + reports.len() >= MAX_REPORTS || findings >= MAX_FINDINGS {
            truncated = true;
            break;
        }
        let mut report = parse_report(&item);
        let remaining = MAX_FINDINGS.saturating_sub(findings);
        if report.results.len() > remaining {
            report.results.truncate(remaining);
            truncated = true;
        }
        findings += report.results.len();
        reports.push(report);
    }
    (reports, truncated)
}

/// Parse one PolicyReport or ClusterPolicyReport document.
pub fn parse_report(value: &Value) -> Report {
    let meta = value.get("metadata").unwrap_or(&Value::Null);
    let mut results = Vec::new();
    if let Some(array) = value.get("results").and_then(Value::as_array) {
        for item in array {
            expand_result(item, &mut results);
            if results.len() >= MAX_FINDINGS {
                break;
            }
        }
    }
    Report {
        namespace: clip(str_field(meta, "namespace")),
        name: clip(str_field(meta, "name")),
        results,
    }
}

fn expand_result(value: &Value, into: &mut Vec<Finding>) {
    let policy = clip(str_field(value, "policy"));
    let result = clip(str_field(value, "result"));
    let severity_raw = str_field(value, "severity");
    let severity = finding_severity(&result, severity_raw);
    let resources = match value.get("resources").and_then(Value::as_array) {
        Some(array) => array.as_slice(),
        None => &[],
    };
    if resources.is_empty() {
        into.push(Finding {
            policy,
            result,
            severity,
            resource_name: String::new(),
            resource_kind: String::new(),
            resource_uid: String::new(),
        });
        return;
    }
    for resource in resources.iter().take(MAX_RESOURCES_PER_RESULT) {
        if into.len() >= MAX_FINDINGS {
            return;
        }
        into.push(Finding {
            policy: policy.clone(),
            result: result.clone(),
            severity,
            resource_name: clip(str_field(resource, "name")),
            resource_kind: clip(str_field(resource, "kind")),
            resource_uid: clip(str_field(resource, "uid")),
        });
    }
}

fn finding_severity(result: &str, severity: &str) -> Severity {
    if !severity.trim().is_empty() {
        return map_severity(severity);
    }
    match result.trim().to_ascii_lowercase().as_str() {
        "pass" | "passed" | "ok" => Severity::Ok,
        "warn" | "warning" => Severity::Warn,
        "fail" | "failed" | "error" | "err" => Severity::Err,
        _ => Severity::Unknown,
    }
}

fn tint_of(finding: &Finding) -> Option<Severity> {
    match finding.result.trim().to_ascii_lowercase().as_str() {
        "pass" | "passed" | "ok" | "skip" | "skipped" => None,
        _ => Some(finding.severity),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_FIELD_CHARS) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

enum PageError {
    TooLarge,
    NotJson(String),
}

fn parse_list(text: &str) -> Result<WireList, PageError> {
    if text.len() > MAX_PAGE_BYTES {
        return Err(PageError::TooLarge);
    }
    serde_json::from_str(text).map_err(|error| PageError::NotJson(error.to_string()))
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

#[cfg(test)]
#[path = "policy_test.rs"]
mod tests;
