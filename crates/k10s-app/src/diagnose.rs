//! What connecting to this cluster could not do, said once at startup.
//!
//! Every note names a degradation the rest of the app will otherwise express
//! only as an absence -- a kind missing, a namespace empty, a denial surfacing
//! as a stream error -- so the person who launched from a terminal reads the
//! reason before the map opens. The launch screen reuses the same notes for
//! whoever has no stderr to read.

use k10s_core::{Capability, IngestEvent};

pub(crate) fn degradation_notes(
    report: &k10s_data::ClusterReport,
    catalog: &k10s_core::Catalog,
    events: &[IngestEvent],
) -> Vec<String> {
    let name = |kind: k10s_core::KindId| {
        catalog
            .kind(kind)
            .map(|e| e.slug.to_string())
            .unwrap_or_else(|| format!("kind {}", kind.0))
    };
    let mut notes = Vec::new();

    if report.probe_degraded {
        notes.push(
            "the RBAC probe could not run, so every kind is attempted and a denial will show \
             up as a stream error instead of a label"
                .to_string(),
        );
    }
    if report.kinds_unanswered > 0 {
        notes.push(format!(
            "{} kinds got no answer from their cluster-wide access review, so they are \
             attempted rather than gated and a denial on one will show up as a stream error",
            report.kinds_unanswered
        ));
    }
    if !report.namespaces_unanswered.is_empty() {
        notes.push(format!(
            "the rules review for {} got no answer, so denied kinds are still attempted \
             there and a real denial will show up as a stream error instead of an empty map",
            report.namespaces_unanswered.join(", ")
        ));
    }
    if !report.aggregated_discovery {
        notes.push(
            "this server has no aggregated discovery, so discovery cost one request per API group"
                .to_string(),
        );
    }

    let forbidden: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            IngestEvent::Capability {
                kind,
                verdict: Capability::Forbidden,
            } => Some(name(*kind)),
            _ => None,
        })
        .collect();
    if !forbidden.is_empty() {
        notes.push(format!(
            "{} kinds are present but not readable by this account: {}",
            forbidden.len(),
            preview(&forbidden)
        ));
        notes.push(match report.probed_namespaces.as_slice() {
            [] => "no namespace was checked for a narrower grant; --namespace NS adds one to \
                   the probe"
                .to_string(),
            probed => format!(
                "the only namespaces checked for a narrower grant were {}; --namespace NS adds \
                 one to the probe",
                preview(probed)
            ),
        });
    }

    if report.namespaced_streams > 0 {
        notes.push(format!(
            "{} of {} streams are scoped to one namespace rather than to the cluster",
            report.namespaced_streams, report.streams
        ));
    }
    if !report.unsettled.is_empty() {
        let names: Vec<String> = report.unsettled.iter().copied().map(name).collect();
        notes.push(format!(
            "{} kinds did not finish listing inside the timeout and are incomplete: {}",
            names.len(),
            preview(&names)
        ));
    }
    for (kind, reason) in &report.desyncs {
        notes.push(format!("{} stream reported {reason:?}", name(*kind)));
    }

    let stats = report.assemble;
    if stats.unattached > 0 {
        notes.push(format!(
            "{} attachments are not referenced by any workload and are not drawn yet",
            stats.unattached
        ));
    }
    if stats.unknown_namespace > 0 {
        notes.push(format!(
            "{} objects are in namespaces this account cannot list and were left out",
            stats.unknown_namespace
        ));
    }
    if stats.owner_cycles > 0 {
        notes.push(format!(
            "{} objects have a cyclic owner reference chain and were left out",
            stats.owner_cycles
        ));
    }
    if stats.scopes == 0 {
        notes.push(
            "no namespaces were readable, so the map is empty. This is a permissions answer, \
             not an empty cluster."
                .to_string(),
        );
    }
    notes
}

fn preview(names: &[String]) -> String {
    const SHOWN: usize = 6;
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{}, and {} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{Catalog, KindId};
    use k10s_data::{ClusterReport, assemble::AssembleStats};

    fn readable() -> ClusterReport {
        ClusterReport {
            aggregated_discovery: true,
            assemble: AssembleStats {
                scopes: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn notes_for(report: ClusterReport, events: Vec<IngestEvent>) -> Vec<String> {
        degradation_notes(&report, &Catalog::new(), &events)
    }

    fn forbidden(kind: KindId) -> IngestEvent {
        IngestEvent::Capability {
            kind,
            verdict: Capability::Forbidden,
        }
    }

    #[test]
    fn a_namespace_scoped_stream_is_stated_rather_than_explained() {
        let notes = notes_for(
            ClusterReport {
                streams: 4,
                namespaced_streams: 3,
                ..readable()
            },
            Vec::new(),
        );
        assert_eq!(
            notes,
            vec!["3 of 4 streams are scoped to one namespace rather than to the cluster"],
            "{notes:?}"
        );
    }

    #[test]
    fn a_forbidden_kind_names_the_namespaces_that_were_checked() {
        let notes = notes_for(
            ClusterReport {
                probed_namespaces: vec!["default".into()],
                ..readable()
            },
            vec![forbidden(KindId::SECRET), forbidden(KindId::DEPLOYMENT)],
        );
        assert!(notes.iter().any(|n| n.starts_with("2 kinds are present")));
        let hint = notes
            .iter()
            .find(|n| n.contains("--namespace"))
            .unwrap_or_else(|| panic!("{notes:?}"));
        assert!(hint.contains("default"), "{hint}");

        let unprobed = notes_for(readable(), vec![forbidden(KindId::SECRET)]);
        assert!(
            unprobed
                .iter()
                .any(|n| n.starts_with("no namespace was checked")),
            "{unprobed:?}"
        );
    }

    #[test]
    fn an_unanswered_review_is_reported_apart_from_a_probe_that_could_not_run() {
        let notes = notes_for(
            ClusterReport {
                kinds_unanswered: 2,
                ..readable()
            },
            Vec::new(),
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("2 kinds got no answer"), "{notes:?}");

        let degraded = notes_for(
            ClusterReport {
                probe_degraded: true,
                kinds_unanswered: 2,
                ..readable()
            },
            Vec::new(),
        );
        assert_eq!(degraded.len(), 2, "{degraded:?}");
        assert!(degraded[0].contains("could not run"));
    }
}
