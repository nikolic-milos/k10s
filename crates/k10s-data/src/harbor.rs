//! Harbor projects and scans, only if Harbor is already in the cluster.
//!
//! Harbor's API is reached the same way Grafana is: [`crate::reach`] finds a
//! Service, prefers the API-server proxy, and falls back to a plaintext http
//! URL. An https Harbor is a labelled hole with a system-browser URL, because
//! this crate has no TLS client it can enable without changing the lock. A
//! 404 on `/api/v2.0/projects` is not a mystery error: that Service is not
//! serving Harbor's API, so [`Inventory::served`] is false and the section
//! stays invisible. 401 and 403 stay [`crate::read::Fetched::Denied`].
//!
//! Scan overview rides the artifact list when Harbor includes it. Cosign and
//! SBOM live in [`crate::oci`]: this module joins a scan to the pods that
//! run that digest, and does not verify a signature.

use k10s_core::Severity;
use kube::Client;
use serde_json::Value;

use crate::oci::{DigestIndex, clipped, encode_segment, why_is_not_found};
use crate::reach::{Bound, ToolKind, ToolReach, Unbound};
use crate::read::Fetched;

const WHAT: &str = "harbor";
const PAGE_SIZE: usize = 50;
const MAX_PROJECTS: usize = 200;
const MAX_REPOS: usize = 500;
const MAX_PROJECTS_EXPANDED: usize = 32;
const MAX_REPOS_SCANNED: usize = 32;
const MAX_ARTIFACTS_PER_REPO: usize = 20;
const MAX_PAGES: u32 = 40;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub served: bool,
    pub projects: Vec<Project>,
    pub truncated: bool,
    pub unreadable: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub public: bool,
    pub repo_count: i64,
    pub repositories: Vec<Repository>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub name: String,
    pub artifact_count: i64,
    pub pull_count: i64,
    pub artifacts: Vec<ArtifactScan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactScan {
    pub digest: String,
    pub tags: Vec<String>,
    pub scan: Option<ScanOverview>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOverview {
    pub status: String,
    pub severity: String,
    pub mapped: Severity,
    pub total: u32,
    pub fixable: u32,
    pub critical: u32,
    pub high: u32,
    pub medium: u32,
    pub low: u32,
}

/// A Harbor scan joined to the pods that run that digest. Ranked by severity
/// then by how many containers run it; that is exposure, not a CVSS score.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposedFinding {
    pub digest: String,
    pub repository: String,
    pub severity: String,
    pub mapped: Severity,
    pub total: u32,
    pub pods: Vec<String>,
}

/// Fetch Harbor's project and repository inventory through a bound tool.
///
/// [`ToolReach::Absent`] and a 404 on the projects list both produce
/// `served: false`. [`ToolReach::Unbound`] is a Failed why: Harbor is there,
/// this process cannot speak to it.
pub async fn fetch(client: &Client, reach: &ToolReach) -> Fetched<Inventory> {
    match reach {
        ToolReach::Absent { .. } => Fetched::Ok(Inventory::default()),
        ToolReach::Unbound(unbound) => Fetched::Failed {
            what: WHAT,
            why: unbound_why(unbound),
        },
        ToolReach::Bound(bound) => {
            if bound.kind != ToolKind::Harbor {
                return Fetched::Failed {
                    what: WHAT,
                    why: format!("this bind is {}, not Harbor", bound.kind.as_str()),
                };
            }
            fetch_bound(client, bound).await
        }
    }
}

fn unbound_why(unbound: &Unbound) -> String {
    let mut why = unbound.why.clone();
    if let Some(url) = &unbound.browser_url {
        why.push_str("; open ");
        why.push_str(url);
        why.push_str(" in the system browser");
    }
    why
}

async fn fetch_bound(client: &Client, bound: &Bound) -> Fetched<Inventory> {
    let (project_values, mut truncated) = match fetch_array(
        client,
        bound,
        "api/v2.0/projects",
        MAX_PROJECTS,
        NotFound::ServedFalse,
    )
    .await
    {
        Fetched::Ok(None) => {
            return Fetched::Ok(Inventory::default());
        }
        Fetched::Ok(Some(page)) => page,
        Fetched::Denied { what } => return Fetched::Denied { what },
        Fetched::Failed { what, why } => return Fetched::Failed { what, why },
    };

    let mut projects = Vec::new();
    let mut unreadable = 0usize;
    let mut repos_held = 0usize;
    let mut scanned_repos = 0usize;

    for (i, value) in project_values.iter().enumerate() {
        let Some(mut project) = parse_project(value) else {
            unreadable += 1;
            continue;
        };
        if i < MAX_PROJECTS_EXPANDED && repos_held < MAX_REPOS {
            let path = format!(
                "api/v2.0/projects/{}/repositories",
                encode_segment(&project.name)
            );
            match fetch_array(
                client,
                bound,
                &path,
                MAX_REPOS.saturating_sub(repos_held),
                NotFound::Empty,
            )
            .await
            {
                Fetched::Ok(None) => {}
                Fetched::Ok(Some((items, repos_truncated))) => {
                    truncated |= repos_truncated;
                    for item in items {
                        if repos_held == MAX_REPOS {
                            truncated = true;
                            break;
                        }
                        let Some(mut repo) = parse_repository(&project.name, &item) else {
                            unreadable += 1;
                            continue;
                        };
                        if scanned_repos < MAX_REPOS_SCANNED {
                            match fetch_scans(client, bound, &project.name, &repo.name).await {
                                Fetched::Ok(artifacts) => repo.artifacts = artifacts,
                                Fetched::Denied { .. } => {}
                                Fetched::Failed { .. } => unreadable += 1,
                            }
                            scanned_repos += 1;
                        }
                        repos_held += 1;
                        project.repositories.push(repo);
                    }
                }
                Fetched::Denied { .. } => {}
                Fetched::Failed { .. } => unreadable += 1,
            }
        } else if project.repo_count > 0 {
            truncated = true;
        }
        projects.push(project);
    }

    Fetched::Ok(Inventory {
        served: true,
        projects,
        truncated,
        unreadable,
    })
}

enum NotFound {
    ServedFalse,
    Empty,
}

async fn fetch_array(
    client: &Client,
    bound: &Bound,
    base_path: &str,
    cap: usize,
    on_404: NotFound,
) -> Fetched<Option<(Vec<Value>, bool)>> {
    let mut out = Vec::new();
    let mut truncated = false;
    let mut page = 1u32;
    loop {
        if out.len() >= cap {
            truncated = true;
            break;
        }
        let rest = format!("{base_path}?page={page}&page_size={PAGE_SIZE}");
        let bytes = match crate::reach::tool_get(client, bound, &rest).await {
            Fetched::Ok(bytes) => bytes,
            Fetched::Denied { .. } => return Fetched::Denied { what: WHAT },
            Fetched::Failed { why, .. } => {
                if why_is_not_found(&why) {
                    if page == 1 {
                        return match on_404 {
                            NotFound::ServedFalse => Fetched::Ok(None),
                            NotFound::Empty => Fetched::Ok(Some((Vec::new(), false))),
                        };
                    }
                    break;
                }
                if page == 1 {
                    return Fetched::Failed { what: WHAT, why };
                }
                break;
            }
        };
        let items: Vec<Value> = match serde_json::from_slice(&bytes) {
            Ok(items) => items,
            Err(error) => {
                if page == 1 {
                    return Fetched::Failed {
                        what: WHAT,
                        why: format!("Harbor JSON did not parse: {error}"),
                    };
                }
                break;
            }
        };
        let short = items.len() < PAGE_SIZE;
        for item in items {
            if out.len() == cap {
                truncated = true;
                break;
            }
            out.push(item);
        }
        if truncated || short {
            break;
        }
        page += 1;
        if page > MAX_PAGES {
            truncated = true;
            break;
        }
    }
    Fetched::Ok(Some((out, truncated)))
}

async fn fetch_scans(
    client: &Client,
    bound: &Bound,
    project: &str,
    repo: &str,
) -> Fetched<Vec<ArtifactScan>> {
    let rest = format!(
        "api/v2.0/projects/{}/repositories/{}/artifacts?with_scan_overview=true&page=1&page_size={MAX_ARTIFACTS_PER_REPO}",
        encode_segment(project),
        encode_segment(repo),
    );
    match crate::reach::tool_get(client, bound, &rest).await {
        Fetched::Ok(bytes) => match parse_artifacts(&bytes) {
            Ok(artifacts) => Fetched::Ok(artifacts),
            Err(_) => Fetched::Failed {
                what: WHAT,
                why: "Harbor artifact JSON did not parse".to_string(),
            },
        },
        Fetched::Denied { .. } => Fetched::Denied { what: WHAT },
        Fetched::Failed { why, .. } => {
            if why_is_not_found(&why) {
                Fetched::Ok(Vec::new())
            } else {
                Fetched::Failed { what: WHAT, why }
            }
        }
    }
}

pub fn parse_projects(bytes: &[u8]) -> Result<Vec<Project>, String> {
    if bytes.len() > crate::reach::MAX_BODY_BYTES {
        return Err(format!(
            "Harbor JSON is {} bytes; the cap is {}",
            bytes.len(),
            crate::reach::MAX_BODY_BYTES
        ));
    }
    let values: Vec<Value> = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    Ok(values.iter().filter_map(parse_project).collect())
}

fn parse_project(value: &Value) -> Option<Project> {
    let name = value.get("name").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let public = value
        .get("metadata")
        .and_then(|m| m.get("public"))
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("true"));
    Some(Project {
        name: clipped(name),
        public,
        repo_count: value.get("repo_count").and_then(Value::as_i64).unwrap_or(0),
        repositories: Vec::new(),
    })
}

pub fn parse_repositories(project: &str, bytes: &[u8]) -> Result<Vec<Repository>, String> {
    let values: Vec<Value> = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    Ok(values
        .iter()
        .filter_map(|value| parse_repository(project, value))
        .collect())
}

fn parse_repository(project: &str, value: &Value) -> Option<Repository> {
    let full = value.get("name").and_then(Value::as_str).unwrap_or("");
    let name = full
        .strip_prefix(project)
        .and_then(|s| s.strip_prefix('/'))
        .unwrap_or(full);
    if name.is_empty() {
        return None;
    }
    Some(Repository {
        name: clipped(name),
        artifact_count: value
            .get("artifact_count")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        pull_count: value.get("pull_count").and_then(Value::as_i64).unwrap_or(0),
        artifacts: Vec::new(),
    })
}

pub fn parse_artifacts(bytes: &[u8]) -> Result<Vec<ArtifactScan>, String> {
    if bytes.len() > crate::reach::MAX_BODY_BYTES {
        return Err("Harbor artifact JSON is larger than this view decodes".to_string());
    }
    let values: Vec<Value> = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for value in values.iter().take(MAX_ARTIFACTS_PER_REPO) {
        let digest = value.get("digest").and_then(Value::as_str).unwrap_or("");
        if digest.is_empty() {
            continue;
        }
        let tags = value
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(|tag| tag.get("name").and_then(Value::as_str))
                    .map(clipped)
                    .collect()
            })
            .unwrap_or_default();
        out.push(ArtifactScan {
            digest: digest.to_string(),
            tags,
            scan: parse_scan_overview(value.get("scan_overview")),
        });
    }
    Ok(out)
}

fn parse_scan_overview(value: Option<&Value>) -> Option<ScanOverview> {
    let map = value.and_then(Value::as_object)?;
    let report = map.values().next()?;
    let status = clipped(
        report
            .get("scan_status")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let severity = clipped(report.get("severity").and_then(Value::as_str).unwrap_or(""));
    let summary = report.get("summary");
    let counts = summary
        .and_then(|s| s.get("summary"))
        .and_then(Value::as_object);
    let count = |key: &str| -> u32 {
        counts
            .and_then(|m| m.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32
    };
    Some(ScanOverview {
        mapped: map_severity(&severity),
        total: summary
            .and_then(|s| s.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        fixable: summary
            .and_then(|s| s.get("fixable"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        critical: count("Critical"),
        high: count("High"),
        medium: count("Medium"),
        low: count("Low"),
        status,
        severity,
    })
}

fn map_severity(severity: &str) -> Severity {
    match severity.to_ascii_lowercase().as_str() {
        "critical" | "high" => Severity::Err,
        "medium" => Severity::Warn,
        "low" | "negligible" | "none" => Severity::Ok,
        _ => Severity::Unknown,
    }
}

/// Join Harbor scan overviews to pods that run the same digest. Ranked by
/// mapped severity, then by how many containers run that digest.
pub fn join_scans(inventory: &Inventory, index: &DigestIndex) -> Vec<ExposedFinding> {
    if !inventory.served {
        return Vec::new();
    }
    let mut out = Vec::new();
    for project in &inventory.projects {
        for repo in &project.repositories {
            let repository = if project.name.is_empty() {
                repo.name.clone()
            } else {
                format!("{}/{}", project.name, repo.name)
            };
            for artifact in &repo.artifacts {
                let Some(scan) = &artifact.scan else {
                    continue;
                };
                if scan.total == 0 && scan.severity.is_empty() {
                    continue;
                }
                let pods = index
                    .by_digest
                    .get(&artifact.digest)
                    .map(|runners| {
                        let mut names: Vec<String> = runners
                            .iter()
                            .map(|r| format!("{}/{}", r.namespace, r.pod))
                            .collect();
                        names.sort();
                        names.dedup();
                        names
                    })
                    .unwrap_or_default();
                out.push(ExposedFinding {
                    digest: artifact.digest.clone(),
                    repository: clipped(&repository),
                    severity: scan.severity.clone(),
                    mapped: scan.mapped,
                    total: scan.total,
                    pods,
                });
            }
        }
    }
    out.sort_by(|a, b| {
        b.mapped
            .cmp(&a.mapped)
            .then(b.pods.len().cmp(&a.pods.len()))
            .then(b.total.cmp(&a.total))
            .then(a.digest.cmp(&b.digest))
    });
    out
}

pub fn render(inventory: &Inventory) -> Vec<String> {
    let mut lines = Vec::new();
    if !inventory.served {
        lines.push("Harbor is not serving its API in this cluster".to_string());
        lines.push(String::new());
        lines.push(
            "a 404 on /api/v2.0/projects, or no Harbor Service, is absence: the section stays \
             invisible rather than showing an empty catalog"
                .to_string(),
        );
        return lines;
    }
    if inventory.projects.is_empty() && inventory.unreadable == 0 {
        lines.push("Harbor is serving no projects this account can see".to_string());
    } else if inventory.projects.is_empty() {
        lines.push(
            "no Harbor project could be read here, though some were listed: every project \
             this account can see failed to parse"
                .to_string(),
        );
    } else {
        let repos: usize = inventory
            .projects
            .iter()
            .map(|project| project.repositories.len())
            .sum();
        lines.push(format!(
            "{} {}, {} {}",
            inventory.projects.len(),
            if inventory.projects.len() == 1 {
                "project"
            } else {
                "projects"
            },
            repos,
            if repos == 1 {
                "repository"
            } else {
                "repositories"
            },
        ));
    }
    if inventory.truncated {
        lines.push(
            "the listing stopped at its ceiling, so this is some of the catalog rather than all"
                .to_string(),
        );
    }
    if inventory.unreadable > 0 {
        lines.push(format!(
            "{} Harbor {} could not be read and {} not shown",
            inventory.unreadable,
            if inventory.unreadable == 1 {
                "row"
            } else {
                "rows"
            },
            if inventory.unreadable == 1 {
                "is"
            } else {
                "are"
            },
        ));
    }
    for project in &inventory.projects {
        lines.push(String::new());
        let vis = if project.public { "public" } else { "private" };
        lines.push(format!(
            "{}  {vis}  {} repos",
            project.name, project.repo_count
        ));
        for repo in &project.repositories {
            let mut line = format!("  {}", repo.name);
            // The worst scan across the repo's artifacts, not whichever
            // Harbor listed first — same rule as the table cell.
            let worst = repo
                .artifacts
                .iter()
                .filter_map(|artifact| artifact.scan.as_ref())
                .max_by_key(|scan| {
                    (
                        scan.mapped,
                        scan.critical,
                        scan.high,
                        scan.medium,
                        scan.low,
                        scan.total,
                    )
                });
            if let Some(scan) = worst
                && !scan.severity.is_empty()
            {
                line.push_str(&format!(
                    "  scan {} ({} findings)",
                    scan.severity, scan.total
                ));
            }
            lines.push(line);
        }
    }
    lines
}

pub fn render_findings(findings: &[ExposedFinding]) -> Vec<String> {
    let mut lines = Vec::new();
    if findings.is_empty() {
        lines.push("no Harbor scan findings join to a running digest".to_string());
        return lines;
    }
    lines.push(format!(
        "{} {} ranked by severity then exposure",
        findings.len(),
        if findings.len() == 1 {
            "finding"
        } else {
            "findings"
        },
    ));
    for finding in findings {
        let pods = if finding.pods.is_empty() {
            "no running pod".to_string()
        } else {
            format!(
                "{} {}",
                finding.pods.len(),
                if finding.pods.len() == 1 {
                    "pod"
                } else {
                    "pods"
                }
            )
        };
        lines.push(format!(
            "  {}  {}  {}  {pods}",
            finding.severity, finding.repository, finding.digest
        ));
        for pod in &finding.pods {
            lines.push(format!("    {pod}"));
        }
    }
    lines
}

#[cfg(test)]
#[path = "harbor_test.rs"]
mod tests;
