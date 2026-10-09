//! The translation layer between the data plane's answers and the shell's
//! outcomes, driven with hand-built values. What these pin is preservation:
//! a denial keeps its label, a failure keeps its reason, and the fields that
//! decide behaviour downstream -- an apply's uid, a conflict's truncation, a
//! table's continue token -- cross the seam untouched.

use super::*;

#[test]
fn a_loki_only_probe_preserves_missing_denied_and_failed_states() {
    use k10s_data::read::Seen;
    use k10s_provider::ToolPresence;
    assert_eq!(
        loki_probe_outcome(Fetched::Ok(Seen::Bound)),
        Ok(ToolPresence::Ready)
    );
    assert_eq!(
        loki_probe_outcome(Fetched::Ok(Seen::Absent)),
        Ok(ToolPresence::Missing)
    );
    assert_eq!(
        loki_probe_outcome(Fetched::Denied { what: "services" }),
        Err("services: access denied for this account".into())
    );
    assert_eq!(
        loki_probe_outcome(Fetched::Failed {
            what: "Loki",
            why: "Loki storage is unavailable".into()
        }),
        Err("Loki storage is unavailable".into())
    );
}

#[test]
fn the_typed_missing_previous_state_crosses_the_seam_without_copying_log_lines() {
    for previous_unavailable in [false, true] {
        let lines = vec!["actual current-container line".to_string()];
        let allocation = lines.as_ptr();
        let note = Some("current container producer; previous log unavailable".to_string());
        let k10s_provider::Detail::Log {
            lines,
            note: mapped_note,
            previous_unavailable: mapped_state,
        } = adapt(k10s_data::inspect::InspectDetail::Log(
            k10s_data::inspect::LogTail {
                lines,
                note: note.clone(),
                previous_unavailable,
            },
        ))
        else {
            panic!("the kubelet log outcome");
        };
        assert_eq!(lines.as_ptr(), allocation);
        assert_eq!(lines, ["actual current-container line"]);
        assert_eq!(mapped_note, note);
        assert_eq!(mapped_state, previous_unavailable);
    }
}

#[test]
fn log_trace_links_use_structured_metadata_and_indexed_labels() {
    use k10s_data::integrations::loki::{LogLine, LogStream, Logs};
    use k10s_provider::LokiOutcome;
    const TRACE: &str = "20EC76BBC3F0628BCCADB4D44DA6BD8A";
    const SHORT_TRACE: &str = "20EC76BBC3F0628B";
    for (label_trace, metadata_trace, expected) in [
        (Some(TRACE), None, Some(TRACE.to_ascii_lowercase())),
        (None, Some(TRACE), Some(TRACE.to_ascii_lowercase())),
        (
            Some(SHORT_TRACE),
            None,
            Some(SHORT_TRACE.to_ascii_lowercase()),
        ),
        (
            None,
            Some(SHORT_TRACE),
            Some(SHORT_TRACE.to_ascii_lowercase()),
        ),
        (Some(TRACE), Some("invalid"), None),
        (Some(TRACE), Some("0000000000000000"), None),
        (Some(TRACE), Some("00000000000000000000000000000000"), None),
        (None, None, None),
    ] {
        let mut labels = vec![("k8s_pod_uid".into(), "current-uid".into())];
        if let Some(trace) = label_trace {
            labels.push(("trace_id".into(), trace.into()));
        }
        let metadata = metadata_trace
            .map(|trace| vec![("trace_id".into(), trace.into())])
            .unwrap_or_default();
        let outcome = loki_outcome(Fetched::Ok(Some(Logs {
            streams: vec![LogStream {
                labels,
                lines: vec![LogLine {
                    ts_ns: 1_791_237_376_351_862_530,
                    line: "pilot.complete".into(),
                    metadata,
                }],
            }],
            ..Default::default()
        })));
        let LokiOutcome::Logs { entries, .. } = outcome else {
            panic!("log entries must cross the provider: {outcome:?}");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].trace_id, expected);
        assert_eq!(entries[0].attribute("k8s_pod_uid"), Some("current-uid"));
    }
}

#[test]
fn refused_log_identity_reaches_the_provider_as_a_failure_without_entries() {
    for metadata in [
        serde_json::json!({"k8s_pod_uid": 10}),
        serde_json::json!({"k8s_pod_name": "x".repeat(257)}),
        serde_json::json!({"trace_id": null}),
    ] {
        let body = serde_json::json!({"status":"success","data":{"resultType":"streams","result":[{
            "stream":{"k8s_namespace_name":"prod","k8s_pod_name":"api","k8s_pod_uid":"current-uid","trace_id":"0123456789abcdef0123456789abcdef"},
            "values":[["1","actual line", metadata]]
        }]}}).to_string();
        let why = k10s_data::integrations::loki::parse(body.as_bytes())
            .expect_err("malformed identity must not become an entry");
        assert!(why.starts_with("Loki entry metadata"));
        assert_eq!(
            loki_outcome(Fetched::Failed {
                what: "loki",
                why: why.clone()
            }),
            k10s_provider::LokiOutcome::Failed(why)
        );
    }
}

#[test]
fn bounded_entry_identity_shadows_stream_identity_even_when_empty() {
    for uid in ["", "retired-uid"] {
        let body = serde_json::json!({"status":"success","data":{"resultType":"streams","result":[{
            "stream":{"k8s_namespace_name":"prod","k8s_pod_name":"api","k8s_pod_uid":"current-uid"},
            "values":[["1","actual line", {"k8s_pod_uid":uid}]]
        }]}})
        .to_string();
        let logs = k10s_data::integrations::loki::parse(body.as_bytes()).expect("bounded metadata");
        let k10s_provider::LokiOutcome::Logs { entries, .. } =
            loki_outcome(Fetched::Ok(Some(logs)))
        else {
            panic!("valid log data");
        };
        assert_eq!(entries[0].attribute("k8s_pod_uid"), Some(uid));
        assert_ne!(entries[0].attribute("k8s_pod_uid"), Some("current-uid"));
    }
}

#[test]
fn a_failed_observe_probe_stays_blocked_and_keeps_its_sentence() {
    let reach = observe_reach(Fetched::Failed {
        what: "telemetry",
        why: "telemetry is busy".into(),
    });
    assert_eq!(reach.error.as_deref(), Some("telemetry is busy"));
    assert_eq!(reach.prometheus, k10s_provider::ToolPresence::Blocked);
    assert_eq!(reach.loki, k10s_provider::ToolPresence::Blocked);
    assert_eq!(reach.traces, k10s_provider::ToolPresence::Blocked);
}

#[test]
fn an_individual_observe_probe_failure_does_not_hide_healthy_tools() {
    use k10s_data::read::{ObserveTools, Seen};
    use k10s_provider::ToolPresence;
    let reach = observe_reach(Fetched::Ok(ObserveTools {
        prometheus: Seen::Unbound,
        loki: Seen::Bound,
        traces: Seen::Absent,
        errors: vec!["Prometheus: storage unavailable".into()],
    }));
    assert_eq!(reach.prometheus, ToolPresence::Blocked);
    assert_eq!(reach.loki, ToolPresence::Ready);
    assert_eq!(reach.traces, ToolPresence::Missing);
    assert_eq!(
        reach.error.as_deref(),
        Some("Prometheus: storage unavailable")
    );
}

#[test]
fn a_describe_maps_its_document_and_keeps_denial_and_failure_labels() {
    let doc = describe_outcome(Fetched::Ok(k10s_data::describe::Described {
        title: "pod api-1".to_string(),
        lines: vec!["line".to_string()],
    }));
    match doc {
        k10s_provider::DocOutcome::Doc { title, lines } => {
            assert_eq!(title, "pod api-1");
            assert_eq!(lines, vec!["line".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        describe_outcome(Fetched::Denied { what: "events" }),
        k10s_provider::DocOutcome::Denied("events")
    ));
    match describe_outcome(Fetched::Failed {
        what: "describe",
        why: "timed out".to_string(),
    }) {
        k10s_provider::DocOutcome::Failed(why) => assert_eq!(why, "timed out"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_release_inventory_crosses_as_a_table() {
    let releases = k10s_data::integrations::helm::Releases {
        releases: vec![k10s_data::integrations::helm::Release {
            name: "ingress".to_string(),
            namespace: "infra".to_string(),
            revisions: vec![k10s_data::integrations::helm::Revision {
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
    match releases_outcome(Fetched::Ok(releases)) {
        k10s_provider::TableOutcome::Table(page) => {
            assert_eq!(
                page.rows[0].cells,
                ["ingress", "infra", "3", "deployed", "ingress-nginx-4.11.0"]
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        releases_outcome(Fetched::Denied {
            what: "helm releases"
        }),
        k10s_provider::TableOutcome::Denied("helm releases")
    ));
}

#[test]
fn gitops_absence_is_invisible_and_a_denial_stays_labelled() {
    assert!(matches!(
        argo_outcome(Fetched::Ok(
            k10s_data::integrations::argo::Inventory::default()
        )),
        k10s_provider::TableOutcome::Absent
    ));
    assert!(matches!(
        flux_outcome(Fetched::Ok(
            k10s_data::integrations::flux::Inventory::default()
        )),
        k10s_provider::TableOutcome::Absent
    ));
    assert!(matches!(
        argo_outcome(Fetched::Denied {
            what: "argo applications"
        }),
        k10s_provider::TableOutcome::Denied("argo applications")
    ));
    let denied_flux = k10s_data::integrations::flux::Inventory {
        git_repositories: k10s_data::integrations::flux::KindSet::Denied,
        ..k10s_data::integrations::flux::Inventory::default()
    };
    match flux_outcome(Fetched::Ok(denied_flux)) {
        k10s_provider::TableOutcome::Table(page) => {
            let text = page
                .rows
                .iter()
                .flat_map(|row| row.cells.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                text.contains("access denied for this account"),
                "Denied is labelled, not Absent: {text}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_day2_needs_confirm_crosses_the_summary_and_not_a_write() {
    match day2_outcome(k10s_data::day2::Day2Outcome::NeedsConfirm {
        blast: k10s_data::day2::Blast::Replicas { from: 3, to: 0 },
        summary: "scale api from 3 to 0 replicas".to_string(),
    }) {
        k10s_provider::Day2Outcome::NeedsConfirm { summary } => {
            assert_eq!(summary, "scale api from 3 to 0 replicas");
        }
        other => panic!("{other:?}"),
    }
    match day2_outcome(k10s_data::day2::Day2Outcome::Denied {
        what: "scale",
        why: "this account cannot patch".to_string(),
    }) {
        k10s_provider::Day2Outcome::Denied { what, why } => {
            assert_eq!((what, why.as_str()), ("scale", "this account cannot patch"));
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
        k10s_provider::ManifestOutcome::Manifest {
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

    // A dry run's own read of the object is the review's left side, so it has
    // to cross whole, the absent case included.
    for (read, crossed) in [
        (
            k10s_data::apply::Live::Read {
                yaml: "l".to_string(),
                uid: Some("abc".to_string()),
            },
            k10s_provider::LiveCopy::Read {
                yaml: "l".to_string(),
                uid: Some("abc".to_string()),
            },
        ),
        (
            k10s_data::apply::Live::Absent,
            k10s_provider::LiveCopy::Absent,
        ),
    ] {
        match apply_outcome(Plane::DryRun(k10s_data::apply::DryRun {
            yaml: "y".to_string(),
            uid: Some("abc".to_string()),
            live: read,
        })) {
            k10s_provider::ApplyOutcome::DryRun { yaml, uid, live } => {
                assert_eq!(
                    (yaml.as_str(), uid.as_deref(), live),
                    ("y", Some("abc"), crossed)
                );
            }
            other => panic!("{other:?}"),
        }
    }
    // A real apply crosses as a real apply, with the uid that says which object
    // it landed on.
    match apply_outcome(Plane::Applied(k10s_data::apply::Applied {
        uid: Some("abc".to_string()),
    })) {
        k10s_provider::ApplyOutcome::Applied { uid } => assert_eq!(uid.as_deref(), Some("abc")),
        other => panic!("{other:?}"),
    }

    match apply_outcome(Plane::Unrendered(k10s_data::apply::Unrendered {
        why: "too deep".to_string(),
    })) {
        k10s_provider::ApplyOutcome::Unrendered { why } => assert_eq!(why, "too deep"),
        other => panic!("{other:?}"),
    }

    // A real apply that never left crosses as the failure it is, saying so.
    match apply_outcome(Plane::NotSent {
        why: "exec credential plugin failed".to_string(),
    }) {
        k10s_provider::ApplyOutcome::Failed(why) => {
            assert_eq!(why, "nothing was sent: exec credential plugin failed")
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
        k10s_provider::ApplyOutcome::Conflict {
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
        k10s_provider::ApplyOutcome::Rejected { message, causes } => {
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
        k10s_provider::ApplyOutcome::Stale { message } => assert_eq!(message, "newer"),
        other => panic!("{other:?}"),
    }
    match apply_outcome(Plane::Denied {
        what: "apply",
        why: "forbidden".to_string(),
    }) {
        k10s_provider::ApplyOutcome::Denied { what, why } => {
            assert_eq!((what, why.as_str()), ("apply", "forbidden"));
        }
        other => panic!("{other:?}"),
    }
    // A write that may have landed must not cross as a failure, whose contract
    // is that nothing was written: its words and the standing note differ.
    match apply_outcome(Plane::Unconfirmed {
        why: "no answer within 35 s".to_string(),
    }) {
        k10s_provider::ApplyOutcome::Unconfirmed(why) => assert_eq!(why, "no answer within 35 s"),
        other => panic!("{other:?}"),
    }
    match apply_outcome(Plane::Failed {
        why: "io".to_string(),
    }) {
        k10s_provider::ApplyOutcome::Failed(why) => assert_eq!(why, "io"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_arms_that_only_carry_a_label_still_carry_it() {
    // Every remaining translator arm that decides UI copy or a row's state.
    // These are the ones a seam loses silently: nothing fails to compile when
    // a reason stops crossing, it just stops being on screen.
    match adapt_chunk(k10s_data::logs::LogChunk::Lines(vec!["one".to_string()])) {
        k10s_provider::LogChunk::Lines(lines) => assert_eq!(lines, vec!["one".to_string()]),
        other => panic!("{other:?}"),
    }
    match adapt_chunk(k10s_data::logs::LogChunk::Failed {
        what: "logs",
        why: "connection reset".to_string(),
    }) {
        k10s_provider::LogChunk::Failed(why) => assert_eq!(why, "connection reset"),
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
        k10s_provider::ForwardState::Opening
    ));
    assert!(matches!(
        adapt_forward(active).state,
        k10s_provider::ForwardState::Active
    ));

    match table_outcome(Fetched::<k10s_data::browse::TablePage>::Failed {
        what: "table",
        why: "the list timed out".to_string(),
    }) {
        k10s_provider::TableOutcome::Failed(why) => assert_eq!(why, "the list timed out"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        table_outcome(Fetched::<k10s_data::browse::TablePage>::Denied { what: "pods" }),
        k10s_provider::TableOutcome::Denied("pods")
    ));
    match schema_text_outcome(Fetched::Failed {
        what: "schema",
        why: "not served".to_string(),
    }) {
        k10s_provider::SchemaTextOutcome::Failed(why) => assert_eq!(why, "not served"),
        other => panic!("{other:?}"),
    }
    match containers_outcome(Fetched::Failed {
        what: "containers",
        why: "gone".to_string(),
    }) {
        k10s_provider::ContainersOutcome::Failed(why) => assert_eq!(why, "gone"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn schema_answers_keep_their_sources_and_their_labels() {
    match schema_catalog_outcome(Fetched::Ok(vec![k10s_data::openapi::SchemaSource {
        group_version: "apps/v1".to_string(),
        url: "/openapi/v3/apis/apps/v1".to_string(),
    }])) {
        k10s_provider::SchemaCatalogOutcome::Catalog(sources) => {
            assert_eq!(sources[0].group_version, "apps/v1");
            assert_eq!(sources[0].url, "/openapi/v3/apis/apps/v1");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        schema_text_outcome(Fetched::Denied { what: "schemas" }),
        k10s_provider::SchemaTextOutcome::Denied("schemas")
    ));
    match schema_text_outcome(Fetched::Ok("{}".to_string())) {
        k10s_provider::SchemaTextOutcome::Text(text) => assert_eq!(text, "{}"),
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
        k10s_provider::TableOutcome::Table(table) => {
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
        k10s_provider::ContainersOutcome::Containers(c) if c == ["app".to_string()]
    ));

    assert!(matches!(
        exec_event(k10s_data::exec::ExecEvent::Output(b"hi".to_vec())),
        k10s_provider::ExecEvent::Output(bytes) if bytes == b"hi"
    ));
    match exec_event(k10s_data::exec::ExecEvent::Ended {
        why: "process exited".to_string(),
    }) {
        k10s_provider::ExecEvent::Ended(why) => assert_eq!(why, "process exited"),
        other => panic!("{other:?}"),
    }

    match adapt_chunk(k10s_data::logs::LogChunk::Ended { why: "rotated" }) {
        k10s_provider::LogChunk::Ended(why) => assert_eq!(why, "rotated"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        adapt_chunk(k10s_data::logs::LogChunk::Denied { what: "logs" }),
        k10s_provider::LogChunk::Denied("logs")
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
        k10s_provider::ForwardOutcome::Opened(row) => {
            assert_eq!(row.id, 7);
            assert_eq!((row.local_port, row.remote_port), (8080, 80));
            match row.state {
                k10s_provider::ForwardState::Dead(why) => assert_eq!(why, "pod is gone"),
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
        k10s_provider::UsageOutcome::Usage(sample) => {
            assert_eq!(sample.cpu, Some(k10s_provider::Millicores(250)));
            assert_eq!(sample.memory, Some(k10s_provider::Bytes(32 * 1024 * 1024)));
            assert_eq!(sample.cpu_request, Some(k10s_provider::Millicores(500)));
            // "No limit" must survive the seam as an absence, never as zero.
            assert_eq!(sample.cpu_limit, None);
            assert_eq!(
                sample.memory_request,
                Some(k10s_provider::Bytes(64 * 1024 * 1024))
            );
            assert_eq!(
                sample.memory_limit,
                Some(k10s_provider::Bytes(128 * 1024 * 1024))
            );
            assert_eq!(sample.source, k10s_provider::UsageSource::Kubelet);
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
        k10s_provider::UsageOutcome::Denied("pod metrics")
    ));
    match usage_outcome(plane::UsageOutcome::Failed {
        what: "node metrics",
        why: "did not parse".to_string(),
    }) {
        k10s_provider::UsageOutcome::Failed(why) => assert_eq!(why, "did not parse"),
        other => panic!("{other:?}"),
    }
    match usage_outcome(plane::UsageOutcome::Absent {
        why: "metrics-server is not installed".to_string(),
    }) {
        k10s_provider::UsageOutcome::Absent(why) => {
            assert_eq!(why, "metrics-server is not installed");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn policy_harbor_and_mesh_absence_hide_the_pane() {
    assert!(matches!(
        policy_outcome(Fetched::Ok(
            k10s_data::integrations::policy::Inventory::default()
        )),
        k10s_provider::TableOutcome::Absent
    ));
    assert!(matches!(
        harbor_outcome(Fetched::Ok(
            k10s_data::integrations::harbor::Inventory::default()
        )),
        k10s_provider::TableOutcome::Absent
    ));
    assert!(matches!(
        mesh_outcome(k10s_data::integrations::mesh::MeshInventory::default()),
        k10s_provider::TableOutcome::Absent
    ));
    assert!(matches!(
        grafana_outcome(Fetched::Ok(k10s_data::read::GrafanaCatalog {
            dashboards: Vec::new(),
            extra_hits: Vec::new(),
            truncated: false,
            browser_base: None,
            browser_service: None,
            served: false,
            errors: Vec::new(),
        })),
        k10s_provider::GrafanaOutcome::Absent
    ));
    assert!(matches!(
        prom_outcome(Fetched::Ok(None)),
        k10s_provider::PromOutcome::Absent
    ));
    assert!(matches!(
        loki_outcome(Fetched::Ok(None)),
        k10s_provider::LokiOutcome::Absent
    ));
    assert!(matches!(
        trace_outcome(Fetched::Ok(None)),
        k10s_provider::TraceOutcome::Absent
    ));
}

#[test]
fn a_helm_rollback_report_keeps_the_not_helm_rollback_note() {
    let report = k10s_data::integrations::helm_reveal::RollbackReport {
        note: k10s_data::integrations::helm_reveal::NOT_HELM_ROLLBACK,
        documents: Vec::new(),
    };
    match helm_rollback_outcome(Fetched::Ok(report)) {
        k10s_provider::HelmRollbackOutcome::Report { note, lines } => {
            assert_eq!(
                note,
                k10s_data::integrations::helm_reveal::NOT_HELM_ROLLBACK
            );
            assert_eq!(
                lines[0],
                k10s_data::integrations::helm_reveal::NOT_HELM_ROLLBACK
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_rollback_names_the_document_that_may_have_landed_and_the_ones_not_sent() {
    use k10s_data::integrations::helm_reveal::DocumentRollback;
    let report = k10s_data::integrations::helm_reveal::RollbackReport {
        note: k10s_data::integrations::helm_reveal::NOT_HELM_ROLLBACK,
        documents: vec![
            DocumentRollback::Applied {
                name: "web".to_string(),
                kind: "Deployment".to_string(),
                outcome: k10s_data::apply::ApplyOutcome::Unconfirmed {
                    why: "no answer within 35 s".to_string(),
                },
            },
            DocumentRollback::Skipped {
                name: "settings".to_string(),
                kind: "ConfigMap".to_string(),
                why: "not sent after an earlier document went unconfirmed".to_string(),
            },
        ],
    };
    match helm_rollback_outcome(Fetched::Ok(report)) {
        k10s_provider::HelmRollbackOutcome::Report { lines, .. } => assert_eq!(
            lines[2..],
            [
                "Deployment web: it may have landed: no answer within 35 s",
                "ConfigMap settings: skipped (not sent after an earlier document went unconfirmed)",
            ]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn grafana_queries_cross_as_panel_rows_and_unsupported_keep_a_browser_url() {
    let catalog = k10s_data::read::GrafanaCatalog {
        errors: Vec::new(),
        dashboards: vec![k10s_data::integrations::grafana::Dashboard {
            uid: "node".to_string(),
            title: "Node".to_string(),
            panels: vec![k10s_data::integrations::grafana::Panel {
                id: 2,
                title: "CPU".to_string(),
                kind: k10s_data::integrations::grafana::PanelKind::Timeseries,
                unit: String::new(),
                reducer: "lastNotNull".into(),
                min_interval: String::new(),
                unsupported: None,
                queries: vec![k10s_data::integrations::grafana::PanelQuery {
                    ref_id: "A".to_string(),
                    expr: "rate(cpu[1m])".to_string(),
                    dialect: k10s_data::integrations::grafana::QueryDialect::PromQL,
                    datasource: None,
                    min_step: String::new(),
                    unsupported: None,
                }],
                transformed: false,
            }],
            truncated: false,
        }],
        extra_hits: Vec::new(),
        truncated: false,
        browser_base: Some("http://grafana.mon.svc:3000".to_string()),
        browser_service: None,
        served: true,
    };
    match grafana_outcome(Fetched::Ok(catalog)) {
        k10s_provider::GrafanaOutcome::Catalog { panels, .. } => {
            assert_eq!(panels[0].expr, "rate(cpu[1m])");
            assert_eq!(
                panels[0].browser_url.as_deref(),
                Some("http://grafana.mon.svc:3000/d/node?viewPanel=2")
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn grafana_catalog_failures_survive_projection_instead_of_appearing_absent() {
    let errors = vec!["Provisioned dashboards: configmaps temporarily unavailable".to_string()];
    for served in [false, true] {
        let catalog = k10s_data::read::GrafanaCatalog {
            dashboards: Vec::new(),
            extra_hits: Vec::new(),
            truncated: false,
            browser_base: None,
            browser_service: None,
            served,
            errors: errors.clone(),
        };
        match grafana_outcome(Fetched::Ok(catalog)) {
            k10s_provider::GrafanaOutcome::Catalog {
                panels, notices, ..
            } => {
                assert!(panels.is_empty());
                assert_eq!(notices, errors);
            }
            other => panic!("a named catalog source failed: {other:?}"),
        }
    }
}

#[test]
fn an_empty_trace_reply_remains_distinct_from_an_absent_store() {
    let trace = k10s_data::integrations::traces::Trace {
        trace_id: String::new(),
        source: "Tempo / observe/tempo:3200".into(),
        spans: Vec::new(),
    };
    assert_eq!(
        trace_outcome(Fetched::Ok(Some(trace))),
        k10s_provider::TraceOutcome::Empty {
            source: "Tempo / observe/tempo:3200".into()
        }
    );
    let trace = k10s_data::integrations::traces::Trace {
        trace_id: "returned-id".into(),
        source: "Jaeger / observe/jaeger:16686".into(),
        spans: Vec::new(),
    };
    assert!(
        matches!(trace_outcome(Fetched::Ok(Some(trace))), k10s_provider::TraceOutcome::Trace { trace_id, source, spans } if trace_id=="returned-id" && source=="Jaeger / observe/jaeger:16686" && spans.is_empty())
    );
}

#[test]
fn successful_trace_projection_keeps_its_backend_source_and_span_identity() {
    let mut trace = k10s_data::integrations::traces::parse(br#"{
        "resourceSpans":[{"resource":{"attributes":[{"key":"k8s.pod.uid","value":{"stringValue":"current-uid"}}]},
        "scopeSpans":[{"spans":[{"traceId":"0123456789abcdef0123456789abcdef","spanId":"0123456789abcdef",
        "startTimeUnixNano":"1000","endTimeUnixNano":"2000"}]}]}]
    }"#).expect("a decoded trace");
    trace.source = "Tempo / observe/tempo:3200".into();
    let k10s_provider::TraceOutcome::Trace {
        trace_id,
        source,
        spans,
    } = trace_outcome(Fetched::Ok(Some(trace)))
    else {
        panic!("successful trace data crosses the provider");
    };
    assert_eq!(trace_id, "0123456789abcdef0123456789abcdef");
    assert_eq!(source, "Tempo / observe/tempo:3200");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].pod_uid, "current-uid");
    assert_eq!((spans[0].start_us, spans[0].duration_us), (1, 1));
}

#[test]
fn failed_denied_and_absent_traces_keep_their_state_without_a_success_source() {
    assert_eq!(
        trace_outcome(Fetched::Failed {
            what: "tempo",
            why: "trace storage is unavailable".into()
        }),
        k10s_provider::TraceOutcome::Failed("trace storage is unavailable".into())
    );
    assert_eq!(
        trace_outcome(Fetched::Denied { what: "tempo" }),
        k10s_provider::TraceOutcome::Denied("tempo")
    );
    assert_eq!(
        trace_outcome(Fetched::Ok(None)),
        k10s_provider::TraceOutcome::Absent
    );
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
        k10s_provider::Detail::Events(rows) => {
            assert_eq!(rows[0].when, "2m");
            assert_eq!(rows[0].kind, "Warning");
            assert_eq!(rows[0].reason, "BackOff");
            assert_eq!(rows[0].count, 12);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn policy_findings_sharing_a_resource_get_distinct_row_uids() {
    let finding = |policy: &str, uid: &str| k10s_data::integrations::policy::Finding {
        policy: policy.to_string(),
        result: "fail".to_string(),
        severity: k10s_core::Severity::Warn,
        resource_name: "api".to_string(),
        resource_kind: "Deployment".to_string(),
        resource_uid: uid.to_string(),
    };
    let inventory = k10s_data::integrations::policy::Inventory {
        served: true,
        reports: vec![k10s_data::integrations::policy::Report {
            namespace: "prod".to_string(),
            name: "polr-api".to_string(),
            results: vec![
                finding("require-limits", "uid-1"),
                finding("disallow-latest", "uid-1"),
                finding("require-probes", ""),
                finding("require-labels", ""),
            ],
        }],
        truncated: false,
        partly_denied: false,
    };
    match policy_outcome(Fetched::Ok(inventory)) {
        k10s_provider::TableOutcome::Table(page) => {
            let mut uids: Vec<&str> = page.rows.iter().map(|row| row.uid.as_str()).collect();
            uids.sort_unstable();
            let before = uids.len();
            uids.dedup();
            assert_eq!(uids.len(), before, "every finding keeps its own row uid");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_harbor_project_without_fetched_repos_counts_them_under_repository_not_artifacts() {
    let inventory = k10s_data::integrations::harbor::Inventory {
        served: true,
        projects: vec![k10s_data::integrations::harbor::Project {
            name: "library".to_string(),
            public: true,
            repo_count: 7,
            repositories: Vec::new(),
        }],
        truncated: false,
        unreadable: 0,
    };
    match harbor_outcome(Fetched::Ok(inventory)) {
        k10s_provider::TableOutcome::Table(page) => {
            // Columns are Project, Visibility, Repository, Artifacts, Scan.
            assert_eq!(page.rows[0].cells[2], "(7 repositories)");
            assert_eq!(page.rows[0].cells[3], "");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_harbor_repo_scan_cell_reports_the_worst_artifact_not_the_first() {
    let scan = |severity: &str, mapped: k10s_core::Severity, critical: u32, total: u32| {
        k10s_data::integrations::harbor::ArtifactScan {
            digest: format!("sha256:{severity}"),
            tags: Vec::new(),
            scan: Some(k10s_data::integrations::harbor::ScanOverview {
                status: "Success".to_string(),
                severity: severity.to_string(),
                mapped,
                total,
                fixable: 0,
                critical,
                high: 0,
                medium: 0,
                low: 0,
            }),
        }
    };
    let inventory = k10s_data::integrations::harbor::Inventory {
        served: true,
        projects: vec![k10s_data::integrations::harbor::Project {
            name: "library".to_string(),
            public: false,
            repo_count: 1,
            repositories: vec![k10s_data::integrations::harbor::Repository {
                name: "api".to_string(),
                artifact_count: 2,
                pull_count: 0,
                artifacts: vec![
                    scan("Low", k10s_core::Severity::Warn, 0, 3),
                    scan("Critical", k10s_core::Severity::Err, 2, 9),
                ],
            }],
        }],
        truncated: false,
        unreadable: 0,
    };
    match harbor_outcome(Fetched::Ok(inventory)) {
        k10s_provider::TableOutcome::Table(page) => {
            assert_eq!(page.rows[0].cells[4], "Critical (9)");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unreadable_harbor_rows_mark_the_listing_as_partial() {
    let inventory = k10s_data::integrations::harbor::Inventory {
        served: true,
        projects: vec![k10s_data::integrations::harbor::Project {
            name: "library".to_string(),
            public: true,
            repo_count: 1,
            repositories: Vec::new(),
        }],
        truncated: false,
        unreadable: 2,
    };
    match harbor_outcome(Fetched::Ok(inventory)) {
        k10s_provider::TableOutcome::Table(page) => assert!(page.truncated),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_denied_mesh_group_stays_visible_next_to_the_one_that_answered() {
    use k10s_data::integrations::mesh::{GroupState, MeshInventory, MeshKind, MeshObject};

    let object = MeshObject {
        kind: MeshKind::VirtualService,
        namespace: "prod".to_string(),
        name: "api".to_string(),
        hosts: vec!["api.prod".to_string()],
        destinations: Vec::new(),
        gateways: Vec::new(),
    };

    // Istio answered with rows; the linkerd denial makes the page partial.
    match mesh_outcome(MeshInventory {
        istio: GroupState::Served,
        linkerd: GroupState::Denied,
        objects: vec![object],
        truncated: false,
    }) {
        k10s_provider::TableOutcome::Table(page) => assert!(page.truncated),
        other => panic!("{other:?}"),
    }

    // With nothing to show, the sibling's denial is the whole answer.
    assert!(matches!(
        mesh_outcome(MeshInventory {
            istio: GroupState::Served,
            linkerd: GroupState::Denied,
            objects: Vec::new(),
            truncated: false,
        }),
        k10s_provider::TableOutcome::Denied("mesh")
    ));
    match mesh_outcome(MeshInventory {
        istio: GroupState::Served,
        linkerd: GroupState::Failed {
            why: "timed out".to_string(),
        },
        objects: Vec::new(),
        truncated: false,
    }) {
        k10s_provider::TableOutcome::Failed(why) => assert_eq!(why, "timed out"),
        other => panic!("{other:?}"),
    }

    // Both groups served and simply empty stays an ordinary empty table.
    match mesh_outcome(MeshInventory {
        istio: GroupState::Served,
        linkerd: GroupState::Served,
        objects: Vec::new(),
        truncated: false,
    }) {
        k10s_provider::TableOutcome::Table(page) => assert!(!page.truncated),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_alert_join_uses_the_chosen_fingerprint_and_keeps_both_labels() {
    let alerts = k10s_data::integrations::alertmanager::parse_alerts(
        br#"[
        {"fingerprint":"other", "labels":{"namespace":"wrong", "pod":"same-name"}},
        {"fingerprint":"chosen", "labels":{"namespace":"prod", "pod":"same-name"}}
    ]"#,
    )
    .expect("alerts");
    assert_eq!(
        alert_pod_outcome("chosen", Fetched::Ok(Some(alerts))),
        k10s_provider::AlertPodOutcome::Pod {
            namespace: "prod".into(),
            pod: "same-name".into()
        }
    );
    assert_eq!(
        alert_pod_outcome("chosen", Fetched::Ok(None)),
        k10s_provider::AlertPodOutcome::Absent
    );
    assert_eq!(
        alert_pod_outcome(
            "chosen",
            Fetched::Denied {
                what: "alertmanager"
            }
        ),
        k10s_provider::AlertPodOutcome::Denied("alertmanager")
    );
    assert_eq!(
        alert_pod_outcome(
            "chosen",
            Fetched::Failed {
                what: "alertmanager",
                why: "backend unavailable".into()
            }
        ),
        k10s_provider::AlertPodOutcome::Failed("backend unavailable".into())
    );
    for labels in [
        serde_json::json!({"namespace":"prod"}),
        serde_json::json!({"pod":"same-name"}),
        serde_json::json!({}),
    ] {
        let alerts = k10s_data::integrations::alertmanager::parse_alerts(
            serde_json::json!([{"fingerprint":"chosen", "labels":labels}])
                .to_string()
                .as_bytes(),
        )
        .expect("alert");
        assert!(
            matches!(alert_pod_outcome("chosen", Fetched::Ok(Some(alerts))), k10s_provider::AlertPodOutcome::Failed(why) if why.contains("both namespace and pod"))
        );
    }
    assert!(
        matches!(alert_pod_outcome("gone", Fetched::Ok(Some(Default::default()))), k10s_provider::AlertPodOutcome::Failed(why) if why.contains("no longer"))
    );
}

#[test]
fn the_omitted_alerts_row_names_no_pod() {
    let mut alerts = k10s_data::integrations::alertmanager::parse_alerts(
        br#"[{"fingerprint":"fdfff9b4ee24368c", "labels":{"namespace":"prod", "pod":"api-0"}}]"#,
    )
    .expect("alert");
    alerts.dropped = 2;
    let k10s_provider::TableOutcome::Table(page) = optional_table_outcome(Fetched::Ok(
        k10s_data::integrations::alertmanager::table_page(Some(&alerts)),
    )) else {
        panic!("Alertmanager answered");
    };
    assert_eq!(
        omitted_notice_answer(&page.rows[0].uid),
        Some(k10s_provider::AlertPodOutcome::Failed(
            "this row counts alerts that are not shown; it names no pod".into()
        ))
    );
    assert_eq!(omitted_notice_answer(&page.rows[1].uid), None);
}

#[test]
fn the_silence_adapter_preserves_matchers_interval_and_destination() {
    use k10s_provider::{AlertMatcher, AlertmanagerEndpoint, SilenceRequest};
    let start = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_789_182_000);
    let request = SilenceRequest {
        endpoint: AlertmanagerEndpoint {
            namespace: "monitoring".into(),
            service: "reviewed".into(),
            port: 9093,
        },
        matchers: vec![
            AlertMatcher {
                name: "namespace".into(),
                value: "prod".into(),
                is_regex: false,
                is_equal: true,
            },
            AlertMatcher {
                name: "pod".into(),
                value: "api.v2-7d4f".into(),
                is_regex: false,
                is_equal: true,
            },
        ],
        window: start..start + std::time::Duration::from_secs(3600),
        created_by: "k10s".into(),
        comment: "Investigating this pod.".into(),
    };
    let spec = silence_spec(&request).expect("a valid interval");
    assert_eq!(
        spec.matchers,
        vec![
            k10s_data::integrations::alertmanager::Matcher {
                name: "namespace".into(),
                value: "prod".into(),
                is_regex: false,
                is_equal: true
            },
            k10s_data::integrations::alertmanager::Matcher {
                name: "pod".into(),
                value: "api.v2-7d4f".into(),
                is_regex: false,
                is_equal: true
            },
        ]
    );
    assert_eq!(spec.starts_at, "2026-09-12T03:00:00Z");
    assert_eq!(spec.ends_at, "2026-09-12T04:00:00Z");
    assert_eq!(spec.created_by, request.created_by);
    assert_eq!(spec.comment, request.comment);
    assert_eq!(
        alertmanager_bound(&request.endpoint).transport,
        k10s_data::reach::Transport::Proxy {
            namespace: "monitoring".into(),
            service: "reviewed".into(),
            port: 9093,
        }
    );
}
