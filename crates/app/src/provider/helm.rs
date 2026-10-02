//! Translate stored Helm release answers into provider outcomes.

use super::kubernetes::table_outcome;
use k10s_data::read::Fetched;

pub(super) fn releases_outcome(
    fetched: Fetched<k10s_data::integrations::helm::Releases>,
) -> k10s_provider::TableOutcome {
    match fetched {
        Fetched::Ok(releases) => table_outcome(Fetched::Ok(
            k10s_data::integrations::helm::table_page(&releases),
        )),
        Fetched::Denied { what } => k10s_provider::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TableOutcome::Failed(why),
    }
}

fn scratch_text(scratch: &k10s_data::reach::Scratch, what: &str) -> Result<String, String> {
    scratch
        .as_str()
        .map(str::to_string)
        .map_err(|_| format!("{what} is not UTF-8"))
}

pub(super) fn helm_reveal_outcome(
    fetched: Fetched<k10s_data::integrations::helm_reveal::RevealedRevision>,
) -> k10s_provider::HelmRevealOutcome {
    match fetched {
        Fetched::Ok(revealed) => {
            let config = match scratch_text(revealed.config(), "user values") {
                Ok(text) => text,
                Err(why) => return k10s_provider::HelmRevealOutcome::Failed(why),
            };
            let chart_values = match scratch_text(revealed.chart_values(), "chart values") {
                Ok(text) => text,
                Err(why) => return k10s_provider::HelmRevealOutcome::Failed(why),
            };
            let manifest = match scratch_text(revealed.manifest(), "manifest") {
                Ok(text) => text,
                Err(why) => return k10s_provider::HelmRevealOutcome::Failed(why),
            };
            k10s_provider::HelmRevealOutcome::Revealed(k10s_provider::HelmReveal {
                name: revealed.name.clone(),
                namespace: revealed.namespace.clone(),
                revision: revealed.revision,
                config,
                chart_values,
                manifest,
            })
        }
        Fetched::Denied { what } => k10s_provider::HelmRevealOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::HelmRevealOutcome::Failed(why),
    }
}

pub(super) fn helm_diff_outcome(fetched: Fetched<String>) -> k10s_provider::DocOutcome {
    match fetched {
        Fetched::Ok(text) => k10s_provider::DocOutcome::Doc {
            title: "helm revision diff".to_string(),
            lines: text.lines().map(str::to_string).collect(),
        },
        Fetched::Denied { what } => k10s_provider::DocOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::DocOutcome::Failed(why),
    }
}

fn apply_line(outcome: &k10s_data::apply::ApplyOutcome) -> String {
    use k10s_data::apply::ApplyOutcome;
    match outcome {
        ApplyOutcome::Applied(_) => "applied".to_string(),
        ApplyOutcome::DryRun(_) => "dry-run applied".to_string(),
        ApplyOutcome::Unrendered(unrendered) => {
            format!(
                "accepted, but its answer cannot be shown ({})",
                unrendered.why
            )
        }
        ApplyOutcome::Conflict { message, .. } => format!("conflict: {message}"),
        ApplyOutcome::Stale { message } => format!("stale: {message}"),
        ApplyOutcome::Rejected { message, .. } => format!("rejected: {message}"),
        ApplyOutcome::Denied { what, why } => format!("{what}: {why}"),
        ApplyOutcome::Unconfirmed { why } => format!("it may have landed: {why}"),
        ApplyOutcome::NotSent { why } => format!("nothing was sent: {why}"),
        ApplyOutcome::Failed { why } => why.clone(),
    }
}

pub(super) fn helm_rollback_outcome(
    fetched: Fetched<k10s_data::integrations::helm_reveal::RollbackReport>,
) -> k10s_provider::HelmRollbackOutcome {
    match fetched {
        Fetched::Ok(report) => {
            let mut lines = vec![report.note.to_string(), String::new()];
            for document in &report.documents {
                match document {
                    k10s_data::integrations::helm_reveal::DocumentRollback::Applied {
                        name,
                        kind,
                        outcome,
                    } => lines.push(format!("{kind} {name}: {}", apply_line(outcome))),
                    k10s_data::integrations::helm_reveal::DocumentRollback::Skipped {
                        name,
                        kind,
                        why,
                    } => lines.push(format!("{kind} {name}: skipped ({why})")),
                }
            }
            k10s_provider::HelmRollbackOutcome::Report {
                note: report.note.to_string(),
                lines,
            }
        }
        Fetched::Denied { what } => k10s_provider::HelmRollbackOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::HelmRollbackOutcome::Failed(why),
    }
}
