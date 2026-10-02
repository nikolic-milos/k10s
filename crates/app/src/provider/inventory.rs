//! Translate tool inventories into provider outcomes.

use super::kubernetes::table_outcome;
use k10s_data::read::Fetched;

pub(super) fn argo_outcome(
    fetched: Fetched<k10s_data::integrations::argo::Inventory>,
) -> k10s_provider::TableOutcome {
    match fetched {
        Fetched::Ok(inventory) => match k10s_data::integrations::argo::table_page(&inventory) {
            Some(page) => table_outcome(Fetched::Ok(page)),
            None => k10s_provider::TableOutcome::Absent,
        },
        Fetched::Denied { what } => k10s_provider::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TableOutcome::Failed(why),
    }
}

pub(super) fn flux_outcome(
    fetched: Fetched<k10s_data::integrations::flux::Inventory>,
) -> k10s_provider::TableOutcome {
    match fetched {
        Fetched::Ok(inventory) => match k10s_data::integrations::flux::table_page(&inventory) {
            Some(page) => table_outcome(Fetched::Ok(page)),
            None => k10s_provider::TableOutcome::Absent,
        },
        Fetched::Denied { what } => k10s_provider::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TableOutcome::Failed(why),
    }
}

pub(super) fn policy_outcome(
    fetched: Fetched<k10s_data::integrations::policy::Inventory>,
) -> k10s_provider::TableOutcome {
    match fetched {
        Fetched::Ok(inventory) if !inventory.served => k10s_provider::TableOutcome::Absent,
        Fetched::Ok(inventory) => {
            let columns = [
                "Namespace",
                "Report",
                "Policy",
                "Result",
                "Severity",
                "Resource",
                "Kind",
            ]
            .iter()
            .map(|name| k10s_provider::TableColumn {
                name: name.to_string(),
                wide: false,
            })
            .collect();
            let mut rows = Vec::new();
            if inventory.partly_denied {
                rows.push(k10s_provider::TableRow {
                    cells: vec![
                        String::new(),
                        "some report groups are denied for this account".to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                    name: "denied".to_string(),
                    namespace: None,
                    uid: "denied:policy-reports".to_string(),
                });
            }
            for report in &inventory.reports {
                for (at, finding) in report.results.iter().enumerate() {
                    rows.push(k10s_provider::TableRow {
                        cells: vec![
                            report.namespace.clone(),
                            report.name.clone(),
                            finding.policy.clone(),
                            finding.result.clone(),
                            severity_word(finding.severity).to_string(),
                            finding.resource_name.clone(),
                            finding.resource_kind.clone(),
                        ],
                        name: finding.resource_name.clone(),
                        namespace: if report.namespace.is_empty() {
                            None
                        } else {
                            Some(report.namespace.clone())
                        },
                        // A per-resource report emits many findings sharing
                        // one resource_uid; the row uid must be unique or
                        // selection restore snaps to the first duplicate.
                        uid: if finding.resource_uid.is_empty() {
                            format!("{}/{}#{at}", report.namespace, report.name)
                        } else {
                            format!("{}#{at}", finding.resource_uid)
                        },
                    });
                }
            }
            k10s_provider::TableOutcome::Table(k10s_provider::TablePage {
                columns,
                rows,
                truncated: inventory.truncated,
                continue_token: None,
            })
        }
        Fetched::Denied { what } => k10s_provider::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TableOutcome::Failed(why),
    }
}

pub(super) fn harbor_outcome(
    fetched: Fetched<k10s_data::integrations::harbor::Inventory>,
) -> k10s_provider::TableOutcome {
    match fetched {
        Fetched::Ok(inventory) if !inventory.served => k10s_provider::TableOutcome::Absent,
        Fetched::Ok(inventory) => {
            let columns = ["Project", "Visibility", "Repository", "Artifacts", "Scan"]
                .iter()
                .map(|name| k10s_provider::TableColumn {
                    name: name.to_string(),
                    wide: false,
                })
                .collect();
            let mut rows = Vec::new();
            for project in &inventory.projects {
                if project.repositories.is_empty() {
                    // repo_count counts repositories, not artifacts; it must
                    // stay under the Repository header.
                    rows.push(k10s_provider::TableRow {
                        cells: vec![
                            project.name.clone(),
                            if project.public { "public" } else { "private" }.to_string(),
                            format!("({} repositories)", project.repo_count),
                            String::new(),
                            String::new(),
                        ],
                        name: project.name.clone(),
                        namespace: None,
                        uid: project.name.clone(),
                    });
                    continue;
                }
                for repo in &project.repositories {
                    // A repository-level security cell must show the worst
                    // scan across its artifacts, not whichever Harbor listed
                    // first.
                    let scan = repo
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
                        })
                        .map(|scan| format!("{} ({})", scan.severity, scan.total))
                        .unwrap_or_default();
                    rows.push(k10s_provider::TableRow {
                        cells: vec![
                            project.name.clone(),
                            if project.public { "public" } else { "private" }.to_string(),
                            repo.name.clone(),
                            repo.artifact_count.to_string(),
                            scan,
                        ],
                        name: repo.name.clone(),
                        namespace: Some(project.name.clone()),
                        uid: format!("{}/{}", project.name, repo.name),
                    });
                }
            }
            k10s_provider::TableOutcome::Table(k10s_provider::TablePage {
                columns,
                rows,
                // Unreadable projects, repos, or scans are missing rows; the
                // listing must not read as complete.
                truncated: inventory.truncated || inventory.unreadable > 0,
                continue_token: None,
            })
        }
        Fetched::Denied { what } => k10s_provider::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TableOutcome::Failed(why),
    }
}

pub(super) fn mesh_outcome(
    inventory: k10s_data::integrations::mesh::MeshInventory,
) -> k10s_provider::TableOutcome {
    use k10s_data::integrations::mesh::GroupState;
    if !inventory.present() {
        return match (&inventory.istio, &inventory.linkerd) {
            (GroupState::Denied, _) | (_, GroupState::Denied) => {
                k10s_provider::TableOutcome::Denied("mesh")
            }
            (GroupState::Failed { why }, _) | (_, GroupState::Failed { why }) => {
                k10s_provider::TableOutcome::Failed(why.clone())
            }
            _ => k10s_provider::TableOutcome::Absent,
        };
    }
    // One group answering must not mask the sibling's denial or failure: with
    // nothing to show the error is the whole answer, and with rows to show
    // the page must still say it is partial.
    if inventory.objects.is_empty() {
        match (&inventory.istio, &inventory.linkerd) {
            (GroupState::Denied, _) | (_, GroupState::Denied) => {
                return k10s_provider::TableOutcome::Denied("mesh");
            }
            (GroupState::Failed { why }, _) | (_, GroupState::Failed { why }) => {
                return k10s_provider::TableOutcome::Failed(why.clone());
            }
            _ => {}
        }
    }
    let degraded = !matches!(inventory.istio, GroupState::Served | GroupState::Absent)
        || !matches!(inventory.linkerd, GroupState::Served | GroupState::Absent);
    let columns = [
        "Kind",
        "Namespace",
        "Name",
        "Hosts",
        "Destinations",
        "Gateways",
    ]
    .iter()
    .map(|name| k10s_provider::TableColumn {
        name: name.to_string(),
        wide: false,
    })
    .collect();
    let rows = inventory
        .objects
        .iter()
        .map(|object| k10s_provider::TableRow {
            cells: vec![
                object.kind.as_str().to_string(),
                object.namespace.clone(),
                object.name.clone(),
                object.hosts.join(","),
                object.destinations.join(","),
                object.gateways.join(","),
            ],
            name: object.name.clone(),
            namespace: if object.namespace.is_empty() {
                None
            } else {
                Some(object.namespace.clone())
            },
            uid: format!(
                "{}/{}/{}",
                object.kind.as_str(),
                object.namespace,
                object.name
            ),
        })
        .collect();
    k10s_provider::TableOutcome::Table(k10s_provider::TablePage {
        columns,
        rows,
        truncated: inventory.truncated || degraded,
        continue_token: None,
    })
}

fn severity_word(severity: k10s_core::Severity) -> &'static str {
    match severity {
        k10s_core::Severity::Ok => "ok",
        k10s_core::Severity::Unknown => "unknown",
        k10s_core::Severity::Warn => "warn",
        k10s_core::Severity::Err => "err",
    }
}
