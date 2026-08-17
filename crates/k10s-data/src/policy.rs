//! Policy reports from Kyverno, Gatekeeper, or anything else that publishes
//! the shared report CRDs.
//!
//! `wgpolicyk8s.io` still ships PolicyReport / ClusterPolicyReport. Current
//! documents name `v1beta1`; older clusters still serve `v1alpha2`. Kyverno
//! 1.15+ can also write `openreports.io/v1alpha1` Report / ClusterReport
//! (the successor group). This module probes both groups and the versions
//! each document names. A 404 means that group is not served; the overlay
//! stays off only when neither group answers. A 403 is a labelled denial.
//! Caps bound how many reports and findings we hold, and say so when they
//! bite. Findings are JSON, not a typed CRD client: installing one would
//! be an install.

use std::collections::BTreeMap;

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;
use serde_json::Value;

use k10s_core::Severity;

use crate::read::Fetched;

const WGPOLICY_GROUP: &str = "wgpolicyk8s.io";
const OPENREPORTS_GROUP: &str = "openreports.io";
const WGPOLICY_NAMESPACED: &str = "policyreports";
const WGPOLICY_CLUSTER: &str = "clusterpolicyreports";
const OPENREPORTS_NAMESPACED: &str = "reports";
const OPENREPORTS_CLUSTER: &str = "clusterreports";
const WGPOLICY_FALLBACKS: &[&str] = &["v1beta1", "v1alpha2", "v1alpha1"];
const OPENREPORTS_FALLBACKS: &[&str] = &["v1alpha1"];

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

/// Worst failing finding for one scene object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceTint {
    pub uid: String,
    pub namespace: String,
    pub name: String,
    pub tint: Severity,
}

/// What a fetch held, or the reason it held nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    /// False when the group is not served. The overlay must stay invisible.
    pub served: bool,
    pub reports: Vec<Report>,
    pub truncated: bool,
    /// Some report group answered 403 while another one listed. The kept
    /// reports are shown; the denial must stay visible next to them.
    pub partly_denied: bool,
}

impl Inventory {
    fn unserved() -> Inventory {
        Inventory::default()
    }

    /// Worst finding per resource uid, for OverlayMark.tint.
    /// Pass and skip do not stamp. An empty uid cannot be joined to a scene object.
    pub fn tints(&self) -> Vec<(String, Severity)> {
        self.resource_tints()
            .into_iter()
            .map(|mark| (mark.uid, mark.tint))
            .collect()
    }

    /// Same rollup as [`Inventory::tints`], with the name the snapshot can
    /// still resolve when the uid is not on the published scene.
    pub fn resource_tints(&self) -> Vec<ResourceTint> {
        if !self.served {
            return Vec::new();
        }
        let mut by_uid: BTreeMap<String, ResourceTint> = BTreeMap::new();
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
                    .and_modify(|held| held.tint = held.tint.rollup(tint))
                    .or_insert(ResourceTint {
                        uid: finding.resource_uid.clone(),
                        namespace: report.namespace.clone(),
                        name: finding.resource_name.clone(),
                        tint,
                    });
            }
        }
        by_uid.into_values().collect()
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

/// List PolicyReports and OpenReports. Either group answering is enough.
pub async fn fetch_reports(client: &Client) -> Fetched<Inventory> {
    let mut outcomes = Vec::new();
    match list_group(
        client,
        WGPOLICY_GROUP,
        WGPOLICY_NAMESPACED,
        WGPOLICY_CLUSTER,
        WGPOLICY_FALLBACKS,
    )
    .await
    {
        Ok(pair) => outcomes.extend(pair),
        Err(failed) => return failed,
    }
    match list_group(
        client,
        OPENREPORTS_GROUP,
        OPENREPORTS_NAMESPACED,
        OPENREPORTS_CLUSTER,
        OPENREPORTS_FALLBACKS,
    )
    .await
    {
        Ok(pair) => outcomes.extend(pair),
        Err(failed) => return failed,
    }
    combine_all(outcomes)
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

enum GroupAnswer {
    Served(Vec<String>),
    NotServed,
    Denied,
    Failed(String),
}

fn after_group(error: &kube::Error) -> GroupAnswer {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return GroupAnswer::Denied;
        }
        if response.code == 404 {
            return GroupAnswer::NotServed;
        }
    }
    GroupAnswer::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

fn order_versions(preferred: &str, versions: Vec<String>, fallbacks: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    if !preferred.is_empty() {
        out.push(preferred.to_string());
    }
    for version in versions {
        if version.is_empty() || out.iter().any(|have| have == &version) {
            continue;
        }
        out.push(version);
    }
    for fallback in fallbacks {
        if !out.iter().any(|have| have == fallback) {
            out.push((*fallback).to_string());
        }
    }
    out
}

fn collection_url(group: &str, version: &str, plural: &str) -> String {
    format!("/apis/{group}/{version}/{plural}")
}

async fn probe_group(client: &Client, group: &str) -> GroupAnswer {
    let request = match http::Request::get(format!("/apis/{group}")).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    match client.request::<WireGroup>(request).await {
        Ok(document) => GroupAnswer::Served(order_versions(
            &document.preferred.version,
            document
                .versions
                .into_iter()
                .map(|item| item.version)
                .collect(),
            if group == WGPOLICY_GROUP {
                WGPOLICY_FALLBACKS
            } else {
                OPENREPORTS_FALLBACKS
            },
        )),
        Err(error) => after_group(&error),
    }
}

async fn list_group(
    client: &Client,
    group: &str,
    namespaced: &str,
    cluster: &str,
    fallbacks: &[&str],
) -> Result<[ListOutcome; 2], Fetched<Inventory>> {
    let versions = match probe_group(client, group).await {
        GroupAnswer::NotServed => {
            return Ok([ListOutcome::NotServed, ListOutcome::NotServed]);
        }
        GroupAnswer::Denied => return Ok([ListOutcome::Denied, ListOutcome::Denied]),
        GroupAnswer::Failed(why) => {
            return Err(Fetched::Failed {
                what: "policy reports",
                why,
            });
        }
        GroupAnswer::Served(versions) => {
            if versions.is_empty() {
                fallbacks
                    .iter()
                    .map(|version| (*version).to_string())
                    .collect()
            } else {
                versions
            }
        }
    };
    Ok([
        list_kind_versions(client, group, namespaced, &versions).await,
        list_kind_versions(client, group, cluster, &versions).await,
    ])
}

async fn list_kind_versions(
    client: &Client,
    group: &str,
    plural: &str,
    versions: &[String],
) -> ListOutcome {
    for version in versions {
        match list_kind(client, &collection_url(group, version, plural)).await {
            ListOutcome::NotServed => continue,
            other => return other,
        }
    }
    ListOutcome::NotServed
}

async fn list_kind(client: &Client, path: &str) -> ListOutcome {
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

fn combine_all(outcomes: Vec<ListOutcome>) -> Fetched<Inventory> {
    let mut reports = Vec::new();
    let mut truncated = false;
    let mut any_items = false;
    let mut any_denied = false;
    let mut failure: Option<String> = None;
    for outcome in outcomes {
        match outcome {
            ListOutcome::Items {
                reports: more,
                truncated: page_truncated,
            } => {
                any_items = true;
                truncated |= page_truncated;
                reports.extend(more);
            }
            ListOutcome::Denied => any_denied = true,
            ListOutcome::Failed(why) => {
                failure.get_or_insert(why);
            }
            ListOutcome::NotServed => {}
        }
    }
    // One group's denial or failure must not discard another group's
    // answered reports. Only when nothing answered does the whole fetch
    // carry that state.
    if !any_items {
        if any_denied {
            return Fetched::Denied {
                what: "policy reports",
            };
        }
        if let Some(why) = failure {
            return Fetched::Failed {
                what: "policy reports",
                why,
            };
        }
        return Fetched::Ok(Inventory::unserved());
    }
    Fetched::Ok(finalize(Inventory {
        served: true,
        reports,
        truncated,
        partly_denied: any_denied,
    }))
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

/// Parse one PolicyReport, ClusterPolicyReport, Report, or ClusterReport.
pub fn parse_report(value: &Value) -> Report {
    let meta = value.get("metadata").unwrap_or(&Value::Null);
    let scope = value.get("scope");
    let mut results = Vec::new();
    if let Some(array) = value.get("results").and_then(Value::as_array) {
        for item in array {
            expand_result(item, scope, &mut results);
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

fn expand_result(value: &Value, scope: Option<&Value>, into: &mut Vec<Finding>) {
    let policy = clip(str_field(value, "policy"));
    let result = clip(str_field(value, "result"));
    let severity_raw = str_field(value, "severity");
    let severity = finding_severity(&result, severity_raw);
    let resources = match value.get("resources").and_then(Value::as_array) {
        Some(array) if !array.is_empty() => array.as_slice(),
        _ => scope.map(std::slice::from_ref).unwrap_or(&[]),
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
