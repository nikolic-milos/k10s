//! Translate Kubernetes documents and object operations into provider outcomes.

use k10s_data::read::Fetched;

pub(super) fn adapt(detail: k10s_data::inspect::InspectDetail) -> k10s_provider::Detail {
    use k10s_data::inspect::InspectDetail;
    match detail {
        InspectDetail::Events(lines) => k10s_provider::Detail::Events(
            lines
                .into_iter()
                .map(|line| k10s_provider::EventRow {
                    when: line.last_seen,
                    kind: line.kind,
                    reason: line.reason,
                    message: line.message,
                    count: line.count,
                })
                .collect(),
        ),
        InspectDetail::Log(tail) => k10s_provider::Detail::Log {
            lines: tail.lines,
            note: tail.note,
            previous_unavailable: tail.previous_unavailable,
        },
        InspectDetail::Denied { what } => k10s_provider::Detail::Denied(what),
        InspectDetail::Failed { why, .. } => k10s_provider::Detail::Failed(why),
    }
}

pub(super) fn table_outcome(
    fetched: Fetched<k10s_data::browse::TablePage>,
) -> k10s_provider::TableOutcome {
    match fetched {
        Fetched::Ok(page) => k10s_provider::TableOutcome::Table(k10s_provider::TablePage {
            columns: page
                .columns
                .into_iter()
                .map(|column| k10s_provider::TableColumn {
                    name: column.name,
                    wide: column.wide,
                })
                .collect(),
            rows: page
                .rows
                .into_iter()
                .map(|row| k10s_provider::TableRow {
                    cells: row.cells,
                    name: row.name,
                    namespace: row.namespace,
                    uid: row.uid,
                })
                .collect(),
            truncated: page.truncated,
            continue_token: page.continue_token,
        }),
        Fetched::Denied { what } => k10s_provider::TableOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::TableOutcome::Failed(why),
    }
}

pub(super) fn describe_outcome(
    fetched: Fetched<k10s_data::describe::Described>,
) -> k10s_provider::DocOutcome {
    match fetched {
        Fetched::Ok(described) => k10s_provider::DocOutcome::Doc {
            title: described.title,
            lines: described.lines,
        },
        Fetched::Denied { what } => k10s_provider::DocOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::DocOutcome::Failed(why),
    }
}

pub(super) fn day2_call(request: &k10s_provider::Day2Request) -> k10s_data::day2::Day2Call {
    use k10s_data::day2::{
        Caps, CordonRequest, Day2Call, DebugRequest, DeleteRequest, DrainRequest, EvictRequest,
        RolloutAction, RolloutRequest, ScaleRequest,
    };
    // Caps are filled on the Reader from the probe. A zeroed value here is
    // overwritten before the wire is considered.
    let caps = Caps::default();
    let namespace = request.namespace.clone();
    let name = request.name.clone();
    let confirm = request.confirm;
    match &request.op {
        k10s_provider::Day2Op::Scale { current, replicas } => Day2Call::Scale(ScaleRequest {
            namespace,
            name,
            current: *current,
            replicas: *replicas,
            confirm,
            caps,
        }),
        k10s_provider::Day2Op::Restart => Day2Call::Rollout(RolloutRequest {
            namespace,
            name,
            action: RolloutAction::Restart {
                restarted_at: String::new(),
            },
            confirm,
            caps,
        }),
        k10s_provider::Day2Op::Pause => Day2Call::Rollout(RolloutRequest {
            namespace,
            name,
            action: RolloutAction::Pause,
            confirm,
            caps,
        }),
        k10s_provider::Day2Op::Resume => Day2Call::Rollout(RolloutRequest {
            namespace,
            name,
            action: RolloutAction::Resume,
            confirm,
            caps,
        }),
        k10s_provider::Day2Op::Delete => Day2Call::Delete(DeleteRequest {
            namespace,
            name,
            grace_period_seconds: None,
            confirm,
            caps,
        }),
        k10s_provider::Day2Op::Evict => Day2Call::Evict(EvictRequest {
            namespace: namespace.unwrap_or_default(),
            name,
            grace_period_seconds: None,
            confirm,
            caps,
        }),
        k10s_provider::Day2Op::Cordon { unschedulable } => Day2Call::Cordon(CordonRequest {
            name,
            unschedulable: *unschedulable,
            confirm,
            caps,
        }),
        k10s_provider::Day2Op::Drain { force } => Day2Call::Drain(DrainRequest {
            name,
            force: *force,
            confirm,
            caps,
        }),
        k10s_provider::Day2Op::Debug => Day2Call::Debug(DebugRequest {
            namespace: namespace.unwrap_or_default(),
            name,
            image: "busybox".to_string(),
            confirm,
            caps,
        }),
    }
}

pub(super) fn day2_outcome(outcome: k10s_data::day2::Day2Outcome) -> k10s_provider::Day2Outcome {
    use k10s_data::day2::Day2Outcome;
    match outcome {
        Day2Outcome::Applied(applied) => k10s_provider::Day2Outcome::Applied {
            summary: applied.summary,
            truncated: applied.truncated,
        },
        Day2Outcome::Denied { what, why } => k10s_provider::Day2Outcome::Denied { what, why },
        Day2Outcome::Rejected { message } => k10s_provider::Day2Outcome::Rejected { message },
        Day2Outcome::Failed { why } => k10s_provider::Day2Outcome::Failed { why },
        Day2Outcome::NeedsConfirm { summary, .. } => {
            k10s_provider::Day2Outcome::NeedsConfirm { summary }
        }
    }
}

pub(super) fn manifest_outcome(
    fetched: Fetched<k10s_data::manifest::Manifest>,
) -> k10s_provider::ManifestOutcome {
    match fetched {
        Fetched::Ok(manifest) => k10s_provider::ManifestOutcome::Manifest {
            title: manifest.title,
            yaml: manifest.yaml,
            api_version: manifest.api_version,
            kind: manifest.kind,
            last_applied: manifest.last_applied,
            patchable: manifest.patchable,
            status_subresource: manifest.status_subresource,
            uid: manifest.uid,
        },
        Fetched::Denied { what } => k10s_provider::ManifestOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::ManifestOutcome::Failed(why),
    }
}

pub(super) fn apply_outcome(
    outcome: k10s_data::apply::ApplyOutcome,
) -> k10s_provider::ApplyOutcome {
    use k10s_data::apply::ApplyOutcome;
    match outcome {
        ApplyOutcome::Applied(applied) => k10s_provider::ApplyOutcome::Applied { uid: applied.uid },
        ApplyOutcome::DryRun(dry) => k10s_provider::ApplyOutcome::DryRun {
            yaml: dry.yaml,
            uid: dry.uid,
            live: match dry.live {
                k10s_data::apply::Live::Read { yaml, uid } => {
                    k10s_provider::LiveCopy::Read { yaml, uid }
                }
                k10s_data::apply::Live::Absent => k10s_provider::LiveCopy::Absent,
            },
        },
        ApplyOutcome::Unrendered(unrendered) => k10s_provider::ApplyOutcome::Unrendered {
            why: unrendered.why,
        },
        ApplyOutcome::Conflict {
            message,
            causes,
            truncated,
        } => k10s_provider::ApplyOutcome::Conflict {
            message,
            causes: causes
                .into_iter()
                .map(|cause| k10s_provider::Conflicted {
                    field: cause.field,
                    manager: cause.manager,
                })
                .collect(),
            truncated,
        },
        ApplyOutcome::Stale { message } => k10s_provider::ApplyOutcome::Stale { message },
        ApplyOutcome::Rejected { message, causes } => {
            k10s_provider::ApplyOutcome::Rejected { message, causes }
        }
        ApplyOutcome::Denied { what, why } => k10s_provider::ApplyOutcome::Denied { what, why },
        ApplyOutcome::Unconfirmed { why } => k10s_provider::ApplyOutcome::Unconfirmed(why),
        ApplyOutcome::NotSent { why } => {
            k10s_provider::ApplyOutcome::Failed(format!("nothing was sent: {why}"))
        }
        ApplyOutcome::Failed { why } => k10s_provider::ApplyOutcome::Failed(why),
    }
}

pub(super) fn schema_text_outcome(fetched: Fetched<String>) -> k10s_provider::SchemaTextOutcome {
    match fetched {
        Fetched::Ok(text) => k10s_provider::SchemaTextOutcome::Text(text),
        Fetched::Denied { what } => k10s_provider::SchemaTextOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::SchemaTextOutcome::Failed(why),
    }
}

pub(super) fn schema_catalog_outcome(
    fetched: Fetched<Vec<k10s_data::openapi::SchemaSource>>,
) -> k10s_provider::SchemaCatalogOutcome {
    match fetched {
        Fetched::Ok(sources) => k10s_provider::SchemaCatalogOutcome::Catalog(
            sources
                .into_iter()
                .map(|source| k10s_provider::SchemaSource {
                    group_version: source.group_version,
                    url: source.url,
                })
                .collect(),
        ),
        Fetched::Denied { what } => k10s_provider::SchemaCatalogOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::SchemaCatalogOutcome::Failed(why),
    }
}

pub(super) fn containers_outcome(
    fetched: Fetched<Vec<String>>,
) -> k10s_provider::ContainersOutcome {
    match fetched {
        Fetched::Ok(containers) => k10s_provider::ContainersOutcome::Containers(containers),
        Fetched::Denied { what } => k10s_provider::ContainersOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::ContainersOutcome::Failed(why),
    }
}
