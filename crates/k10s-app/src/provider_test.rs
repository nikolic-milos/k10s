//! The translation layer between the data plane's answers and the shell's
//! outcomes, driven with hand-built values. What these pin is preservation:
//! a denial keeps its label, a failure keeps its reason, and the fields that
//! decide behaviour downstream -- an apply's uid, a conflict's truncation, a
//! table's continue token -- cross the seam untouched.

use super::*;

#[test]
fn a_describe_maps_its_document_and_keeps_denial_and_failure_labels() {
    let doc = describe_outcome(Fetched::Ok(k10s_data::describe::Described {
        title: "pod api-1".to_string(),
        lines: vec!["line".to_string()],
    }));
    match doc {
        k10s_shell::DocOutcome::Doc { title, lines } => {
            assert_eq!(title, "pod api-1");
            assert_eq!(lines, vec!["line".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        describe_outcome(Fetched::Denied { what: "events" }),
        k10s_shell::DocOutcome::Denied("events")
    ));
    match describe_outcome(Fetched::Failed {
        what: "describe",
        why: "timed out".to_string(),
    }) {
        k10s_shell::DocOutcome::Failed(why) => assert_eq!(why, "timed out"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_release_inventory_renders_on_this_side_of_the_seam() {
    let releases = k10s_data::helm::Releases {
        releases: vec![k10s_data::helm::Release {
            name: "ingress".to_string(),
            namespace: "infra".to_string(),
            revisions: vec![k10s_data::helm::Revision {
                revision: 3,
                status: "deployed".to_string(),
                updated: "2026-08-01T10:00:00Z".to_string(),
                description: "Upgrade complete".to_string(),
                chart: "ingress-nginx".to_string(),
                chart_version: "4.11.0".to_string(),
                app_version: "1.11.0".to_string(),
            }],
        }],
        truncated: false,
        unreadable: 0,
    };
    let expected = k10s_data::helm::render(&releases);
    match releases_outcome(Fetched::Ok(releases)) {
        k10s_shell::DocOutcome::Doc { title, lines } => {
            assert_eq!(title, "helm releases");
            assert_eq!(lines, expected);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_manifest_crosses_field_for_field_including_an_absent_uid() {
    let fetched = Fetched::Ok(k10s_data::manifest::Manifest {
        title: "deploy api".to_string(),
        yaml: "kind: Deployment\n".to_string(),
        api_version: "apps/v1".to_string(),
        kind: "Deployment".to_string(),
        last_applied: None,
        patchable: true,
        status_subresource: true,
        uid: None,
    });
    match manifest_outcome(fetched) {
        k10s_shell::ManifestOutcome::Manifest {
            title,
            yaml,
            api_version,
            kind,
            last_applied,
            patchable,
            status_subresource,
            uid,
        } => {
            assert_eq!(title, "deploy api");
            assert_eq!(yaml, "kind: Deployment\n");
            assert_eq!(api_version, "apps/v1");
            assert_eq!(kind, "Deployment");
            assert_eq!(last_applied, None);
            assert!(patchable && status_subresource);
            // "Cannot tell" must survive the seam as an absence, never as "".
            assert_eq!(uid, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_apply_arm_keeps_what_the_review_reads() {
    use k10s_data::apply::ApplyOutcome as Plane;

    match apply_outcome(Plane::Applied(k10s_data::apply::Applied {
        yaml: "y".to_string(),
        dry_run: true,
        uid: Some("abc".to_string()),
    })) {
        k10s_shell::ApplyOutcome::Applied { yaml, dry_run, uid } => {
            assert_eq!(
                (yaml.as_str(), dry_run, uid.as_deref()),
                ("y", true, Some("abc"))
            );
        }
        other => panic!("{other:?}"),
    }

    // A real apply whose answer would not render is still a write that
    // happened; dry_run is what lets the review say which.
    match apply_outcome(Plane::Unrendered(k10s_data::apply::Unrendered {
        dry_run: false,
        why: "too deep".to_string(),
    })) {
        k10s_shell::ApplyOutcome::Unrendered { dry_run, why } => {
            assert!(!dry_run);
            assert_eq!(why, "too deep");
        }
        other => panic!("{other:?}"),
    }

    match apply_outcome(Plane::Conflict {
        message: "conflict".to_string(),
        causes: vec![k10s_data::apply::Conflicted {
            field: ".spec.replicas".to_string(),
            manager: "hpa".to_string(),
        }],
        truncated: true,
    }) {
        k10s_shell::ApplyOutcome::Conflict {
            causes, truncated, ..
        } => {
            assert_eq!(causes[0].field, ".spec.replicas");
            assert_eq!(causes[0].manager, "hpa");
            // A truncated cause list must not read as the complete set.
            assert!(truncated);
        }
        other => panic!("{other:?}"),
    }

    match apply_outcome(Plane::Rejected {
        message: "invalid".to_string(),
        causes: vec!["spec.bogus: unknown field".to_string()],
    }) {
        k10s_shell::ApplyOutcome::Rejected { message, causes } => {
            assert_eq!(message, "invalid");
            assert_eq!(causes.len(), 1);
        }
        other => panic!("{other:?}"),
    }

    // The sentences the review shows, asserted as sentences: a swapped or
    // renamed message is the failure mode a shape-only check cannot see.
    match apply_outcome(Plane::Stale {
        message: "newer".to_string(),
    }) {
        k10s_shell::ApplyOutcome::Stale { message } => assert_eq!(message, "newer"),
        other => panic!("{other:?}"),
    }
    match apply_outcome(Plane::Denied {
        what: "apply",
        why: "forbidden".to_string(),
    }) {
        k10s_shell::ApplyOutcome::Denied { what, why } => {
            assert_eq!((what, why.as_str()), ("apply", "forbidden"));
        }
        other => panic!("{other:?}"),
    }
    match apply_outcome(Plane::Failed {
        why: "io".to_string(),
    }) {
        k10s_shell::ApplyOutcome::Failed(why) => assert_eq!(why, "io"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_arms_that_only_carry_a_label_still_carry_it() {
    // Every remaining translator arm that decides UI copy or a row's state.
    // These are the ones a seam loses silently: nothing fails to compile when
    // a reason stops crossing, it just stops being on screen.
    match adapt_chunk(k10s_data::logs::LogChunk::Lines(vec!["one".to_string()])) {
        k10s_shell::LogChunk::Lines(lines) => assert_eq!(lines, vec!["one".to_string()]),
        other => panic!("{other:?}"),
    }
    match adapt_chunk(k10s_data::logs::LogChunk::Failed {
        what: "logs",
        why: "connection reset".to_string(),
    }) {
        k10s_shell::LogChunk::Failed(why) => assert_eq!(why, "connection reset"),
        other => panic!("{other:?}"),
    }

    let opening = k10s_data::forward::ForwardRow {
        id: 1,
        spec: k10s_data::forward::ForwardSpec {
            namespace: "prod".to_string(),
            pod: "api-1".to_string(),
            local_port: 8080,
            remote_port: 80,
        },
        state: k10s_data::forward::ForwardState::Opening,
    };
    let active = k10s_data::forward::ForwardRow {
        state: k10s_data::forward::ForwardState::Active,
        ..opening.clone()
    };
    assert!(matches!(
        adapt_forward(opening).state,
        k10s_shell::ForwardState::Opening
    ));
    assert!(matches!(
        adapt_forward(active).state,
        k10s_shell::ForwardState::Active
    ));

    match table_outcome(Fetched::<k10s_data::browse::TablePage>::Failed {
        what: "table",
        why: "the list timed out".to_string(),
    }) {
        k10s_shell::TableOutcome::Failed(why) => assert_eq!(why, "the list timed out"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        table_outcome(Fetched::<k10s_data::browse::TablePage>::Denied { what: "pods" }),
        k10s_shell::TableOutcome::Denied("pods")
    ));
    match schema_text_outcome(Fetched::Failed {
        what: "schema",
        why: "not served".to_string(),
    }) {
        k10s_shell::SchemaTextOutcome::Failed(why) => assert_eq!(why, "not served"),
        other => panic!("{other:?}"),
    }
    match containers_outcome(Fetched::Failed {
        what: "containers",
        why: "gone".to_string(),
    }) {
        k10s_shell::ContainersOutcome::Failed(why) => assert_eq!(why, "gone"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn schema_answers_keep_their_sources_and_their_labels() {
    match schema_catalog_outcome(Fetched::Ok(vec![k10s_data::openapi::SchemaSource {
        group_version: "apps/v1".to_string(),
        url: "/openapi/v3/apis/apps/v1".to_string(),
    }])) {
        k10s_shell::SchemaCatalogOutcome::Catalog(sources) => {
            assert_eq!(sources[0].group_version, "apps/v1");
            assert_eq!(sources[0].url, "/openapi/v3/apis/apps/v1");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        schema_text_outcome(Fetched::Denied { what: "schemas" }),
        k10s_shell::SchemaTextOutcome::Denied("schemas")
    ));
    match schema_text_outcome(Fetched::Ok("{}".to_string())) {
        k10s_shell::SchemaTextOutcome::Text(text) => assert_eq!(text, "{}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_table_page_keeps_its_pagination_facts() {
    let page = k10s_data::browse::TablePage {
        columns: vec![k10s_data::browse::TableColumn {
            name: "Name".to_string(),
            wide: false,
        }],
        rows: vec![k10s_data::browse::TableRow {
            cells: vec!["api".to_string()],
            name: "api".to_string(),
            namespace: Some("prod".to_string()),
            uid: "uid-1".to_string(),
        }],
        truncated: true,
        continue_token: Some("page-2".to_string()),
    };
    match table_outcome(Fetched::Ok(page)) {
        k10s_shell::TableOutcome::Table(table) => {
            assert_eq!(table.columns[0].name, "Name");
            assert_eq!(table.rows[0].uid, "uid-1");
            assert!(table.truncated);
            assert_eq!(table.continue_token.as_deref(), Some("page-2"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn containers_and_exec_events_and_log_chunks_translate_arm_for_arm() {
    assert!(matches!(
        containers_outcome(Fetched::Ok(vec!["app".to_string()])),
        k10s_shell::ContainersOutcome::Containers(c) if c == ["app".to_string()]
    ));

    assert!(matches!(
        exec_event(k10s_data::exec::ExecEvent::Output(b"hi".to_vec())),
        k10s_shell::ExecEvent::Output(bytes) if bytes == b"hi"
    ));
    match exec_event(k10s_data::exec::ExecEvent::Ended {
        why: "process exited".to_string(),
    }) {
        k10s_shell::ExecEvent::Ended(why) => assert_eq!(why, "process exited"),
        other => panic!("{other:?}"),
    }

    match adapt_chunk(k10s_data::logs::LogChunk::Ended { why: "rotated" }) {
        k10s_shell::LogChunk::Ended(why) => assert_eq!(why, "rotated"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        adapt_chunk(k10s_data::logs::LogChunk::Denied { what: "logs" }),
        k10s_shell::LogChunk::Denied("logs")
    ));
}

#[test]
fn a_dead_forward_keeps_the_reason_it_died() {
    let row = k10s_data::forward::ForwardRow {
        id: 7,
        spec: k10s_data::forward::ForwardSpec {
            namespace: "prod".to_string(),
            pod: "api-1".to_string(),
            local_port: 8080,
            remote_port: 80,
        },
        state: k10s_data::forward::ForwardState::Dead {
            why: "pod is gone".to_string(),
        },
    };
    match forward_outcome(Fetched::Ok(row)) {
        k10s_shell::ForwardOutcome::Opened(row) => {
            assert_eq!(row.id, 7);
            assert_eq!((row.local_port, row.remote_port), (8080, 80));
            match row.state {
                k10s_shell::ForwardState::Dead(why) => assert_eq!(why, "pod is gone"),
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_usage_sample_crosses_typed_and_every_label_survives() {
    use k10s_data::metrics as plane;

    let sample = plane::UsageSample {
        cpu: Some(plane::Millicores(250)),
        memory: Some(plane::Bytes(32 * 1024 * 1024)),
        cpu_request: Some(plane::Millicores(500)),
        cpu_limit: None,
        memory_request: Some(plane::Bytes(64 * 1024 * 1024)),
        memory_limit: Some(plane::Bytes(128 * 1024 * 1024)),
        source: plane::UsageSource::Kubelet,
        pods_measured: 2,
        pods_total: 3,
        truncated: true,
    };
    match usage_outcome(plane::UsageOutcome::Usage(sample)) {
        k10s_shell::UsageOutcome::Usage(sample) => {
            assert_eq!(sample.cpu, Some(k10s_shell::Millicores(250)));
            assert_eq!(sample.memory, Some(k10s_shell::Bytes(32 * 1024 * 1024)));
            assert_eq!(sample.cpu_request, Some(k10s_shell::Millicores(500)));
            // "No limit" must survive the seam as an absence, never as zero.
            assert_eq!(sample.cpu_limit, None);
            assert_eq!(
                sample.memory_request,
                Some(k10s_shell::Bytes(64 * 1024 * 1024))
            );
            assert_eq!(
                sample.memory_limit,
                Some(k10s_shell::Bytes(128 * 1024 * 1024))
            );
            assert_eq!(sample.source, k10s_shell::UsageSource::Kubelet);
            assert_eq!(
                (sample.pods_measured, sample.pods_total),
                (2, 3),
                "a partial sum keeps saying how partial it is"
            );
            assert!(sample.truncated);
        }
        other => panic!("{other:?}"),
    }

    assert!(matches!(
        usage_outcome(plane::UsageOutcome::Denied {
            what: "pod metrics"
        }),
        k10s_shell::UsageOutcome::Denied("pod metrics")
    ));
    match usage_outcome(plane::UsageOutcome::Failed {
        what: "node metrics",
        why: "did not parse".to_string(),
    }) {
        k10s_shell::UsageOutcome::Failed(why) => assert_eq!(why, "did not parse"),
        other => panic!("{other:?}"),
    }
    match usage_outcome(plane::UsageOutcome::Absent {
        why: "metrics-server is not installed".to_string(),
    }) {
        k10s_shell::UsageOutcome::Absent(why) => {
            assert_eq!(why, "metrics-server is not installed");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn inspect_details_become_event_rows_with_their_counts() {
    let detail = adapt(k10s_data::inspect::InspectDetail::Events(vec![
        k10s_data::inspect::EventLine {
            last_seen: "2m".to_string(),
            kind: "Warning".to_string(),
            reason: "BackOff".to_string(),
            message: "restarting".to_string(),
            count: 12,
        },
    ]));
    match detail {
        k10s_shell::Detail::Events(rows) => {
            assert_eq!(rows[0].when, "2m");
            assert_eq!(rows[0].kind, "Warning");
            assert_eq!(rows[0].reason, "BackOff");
            assert_eq!(rows[0].count, 12);
        }
        other => panic!("{other:?}"),
    }
}
