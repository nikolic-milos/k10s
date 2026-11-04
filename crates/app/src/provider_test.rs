//! The translation layer between the data plane's answers and the shell's
//! outcomes, driven with hand-built values. What these pin is preservation:
//! a denial keeps its label, a failure keeps its reason, and the fields that
//! decide behaviour downstream -- an apply's uid, a conflict's truncation, a
//! table's continue token -- cross the seam untouched.

use super::*;

#[test]
fn prometheus_annotation_omissions_cross_the_provider_without_copying_messages() {
    use k10s_data::integrations::prom::{MAX_ANNOTATIONS_PER_LEVEL, parse_response};
    use k10s_provider::PromOutcome;

    let body = serde_json::json!({
        "status": "success",
        "data": {"resultType": "vector", "result": []},
        "warnings": vec!["warning from the server"; MAX_ANNOTATIONS_PER_LEVEL + 1],
        "infos": vec!["info from the server"; MAX_ANNOTATIONS_PER_LEVEL + 2],
    })
    .to_string();
    let result = parse_response(body.as_bytes()).expect("numeric Prometheus response");
    let warning_pointer = result.warnings[0].as_ptr();
    let info_pointer = result.infos[0].as_ptr();
    let PromOutcome::Series {
        warnings,
        infos,
        warnings_omitted,
        infos_omitted,
        ..
    } = prom_outcome(Fetched::Ok(Some(result)))
    else {
        panic!("successful numeric result")
    };
    assert_eq!(warnings.len(), MAX_ANNOTATIONS_PER_LEVEL);
    assert_eq!(infos.len(), MAX_ANNOTATIONS_PER_LEVEL);
    assert_eq!(warnings_omitted, 1);
    assert_eq!(infos_omitted, 2);
    assert_eq!(warnings[0].as_ptr(), warning_pointer);
    assert_eq!(infos[0].as_ptr(), info_pointer);
}

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
            expected_service: None,
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
            expected_service: None,
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
            query_source: None,
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
            source: k10s_data::integrations::grafana::DashboardSource::Grafana,
            uid: "node".to_string(),
            title: "Node".to_string(),
            panels: vec![k10s_data::integrations::grafana::Panel {
                id: Some(2),
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
        browser_service: Some(("monitoring".into(), "grafana".into(), 3000)),
        query_source: Some(test_grafana_service(3000)),
        served: true,
    };
    let mut browser_only = catalog.clone();
    browser_only.browser_service = None;
    let browser_url = "https://grafana.test/d/private-path?token=private-query#private-fragment";
    browser_only.browser_base = Some(browser_url.to_string());
    let diagnostic = format!("{browser_only:?}");
    for secret in ["private-path", "private-query", "private-fragment"] {
        assert!(!diagnostic.contains(secret));
    }
    let browser_outcome = grafana_outcome(Fetched::Ok(browser_only));
    let diagnostic = format!("{browser_outcome:?}");
    for secret in ["private-path", "private-query", "private-fragment"] {
        assert!(!diagnostic.contains(secret));
    }
    let k10s_provider::GrafanaOutcome::Catalog { panels, .. } = browser_outcome else {
        panic!("the browser-only catalog keeps its panel");
    };
    assert_eq!(panels[0].browser_url.as_deref(), Some(browser_url));
    match grafana_outcome(Fetched::Ok(catalog)) {
        k10s_provider::GrafanaOutcome::Catalog { panels, .. } => {
            assert_eq!(panels[0].expr, "rate(cpu[1m])");
            assert_eq!(
                panels[0].browser_forward,
                Some(k10s_provider::ForwardRequest {
                    namespace: "monitoring".into(),
                    name: "grafana".into(),
                    service: true,
                    service_port: Some(3000),
                    expected_service: Some(test_grafana_service(3000)),
                })
            );
            assert_eq!(
                panels[0].browser_url.as_deref(),
                Some("http://grafana.mon.svc:3000/d/node?viewPanel=2")
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn grafana_unfetched_dashboard_and_real_zero_panel_keep_distinct_routes() {
    use k10s_data::integrations::grafana::{
        CatalogHit, CatalogHitReason, Dashboard, Panel, PanelKind, SearchHit,
    };

    let catalog = k10s_data::read::GrafanaCatalog {
        dashboards: vec![Dashboard {
            source: k10s_data::integrations::grafana::DashboardSource::Grafana,
            uid: "with-zero".into(),
            title: "Known dashboard".into(),
            panels: vec![Panel {
                id: Some(0),
                title: "Known zero panel".into(),
                kind: PanelKind::Timeseries,
                queries: Vec::new(),
                transformed: false,
                unit: String::new(),
                reducer: String::new(),
                min_interval: String::new(),
                unsupported: None,
            }],
            truncated: false,
        }],
        extra_hits: vec![CatalogHit {
            search: SearchHit {
                uid: "not-fetched".into(),
                title: "Unfetched dashboard".into(),
                folder_title: String::new(),
                kind: "dash-db".into(),
            },
            reason: CatalogHitReason::FetchLimit,
        }],
        truncated: true,
        browser_base: Some("http://grafana.test/prefix/".into()),
        browser_service: Some(("monitoring".into(), "grafana".into(), 3333)),
        query_source: Some(test_grafana_service(3333)),
        served: true,
        errors: Vec::new(),
    };
    let k10s_provider::GrafanaOutcome::Catalog { panels, .. } =
        grafana_outcome(Fetched::Ok(catalog))
    else {
        panic!("the catalog must preserve both rows");
    };
    assert_eq!(panels.len(), 2);
    assert_eq!(
        panels[0].identity,
        k10s_provider::GrafanaRowIdentity::Panel {
            id: Some(0),
            position: 0,
        }
    );
    assert_eq!(
        panels[0].browser_url.as_deref(),
        Some("http://grafana.test/prefix/d/with-zero?viewPanel=0")
    );
    assert_eq!(
        panels[0].unsupported.as_deref(),
        Some("No readable Grafana query targets")
    );
    assert_eq!(
        panels[1].identity,
        k10s_provider::GrafanaRowIdentity::Dashboard
    );
    assert_eq!(panels[1].dashboard_title, "Unfetched dashboard");
    assert_eq!(panels[1].title, "Dashboard");
    assert_eq!(
        panels[1].browser_url.as_deref(),
        Some("http://grafana.test/prefix/d/not-fetched")
    );
    assert_eq!(
        panels[1].unsupported.as_deref(),
        Some("dashboard was not fetched at the catalog limit")
    );
    let expected_forward = k10s_provider::ForwardRequest {
        namespace: "monitoring".into(),
        name: "grafana".into(),
        service: true,
        service_port: Some(3333),
        expected_service: Some(test_grafana_service(3333)),
    };
    assert!(
        panels
            .iter()
            .all(|panel| panel.browser_forward.as_ref() == Some(&expected_forward))
    );
}

#[test]
fn grafana_unreadable_search_hits_keep_browser_routes_and_catalog_reasons() {
    use k10s_data::integrations::grafana::{CatalogHit, CatalogHitReason, SearchHit};
    use k10s_provider::{GrafanaOutcome, GrafanaRowIdentity};

    for (reason, state, notice) in [
        (
            CatalogHitReason::Denied,
            "Grafana dashboard read denied; see catalog details",
            "Grafana dashboard locked: access denied for this account",
        ),
        (
            CatalogHitReason::Failed,
            "Grafana dashboard read failed; see catalog details",
            "Grafana dashboard locked: dashboard store unavailable",
        ),
    ] {
        let catalog = k10s_data::read::GrafanaCatalog {
            dashboards: Vec::new(),
            extra_hits: vec![CatalogHit {
                search: SearchHit {
                    uid: "locked".into(),
                    title: "Locked dashboard".into(),
                    folder_title: String::new(),
                    kind: "dash-db".into(),
                },
                reason,
            }],
            truncated: false,
            browser_base: Some("http://grafana.test".into()),
            browser_service: Some(("monitoring".into(), "grafana".into(), 3000)),
            query_source: Some(test_grafana_service(3000)),
            served: true,
            errors: vec![notice.into()],
        };
        let GrafanaOutcome::Catalog {
            panels, notices, ..
        } = grafana_outcome(Fetched::Ok(catalog))
        else {
            panic!("the search hit must remain browseable");
        };
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].identity, GrafanaRowIdentity::Dashboard);
        assert_eq!(
            panels[0].browser_url.as_deref(),
            Some("http://grafana.test/d/locked")
        );
        assert_eq!(
            panels[0]
                .browser_forward
                .as_ref()
                .and_then(|r| r.service_port),
            Some(3000)
        );
        assert_eq!(panels[0].unsupported.as_deref(), Some(state));
        assert_eq!(notices, [notice]);
    }
}

fn test_grafana_service(port: u16) -> std::sync::Arc<k10s_core::ServiceIdentity> {
    std::sync::Arc::new(k10s_core::ServiceIdentity {
        namespace: "monitoring".into(),
        name: "grafana".into(),
        port,
        uid: "uid-grafana".into(),
        resource_version: "42".into(),
    })
}

fn dashboard_with_query_rows(
    source: k10s_data::integrations::grafana::DashboardSource,
    rows: usize,
    truncated: bool,
) -> k10s_data::integrations::grafana::Dashboard {
    use k10s_data::integrations::grafana::{
        Dashboard, MAX_QUERIES_PER_PANEL, Panel, PanelKind, PanelQuery, QueryDialect,
    };

    let panels = (0..rows)
        .step_by(MAX_QUERIES_PER_PANEL)
        .map(|start| Panel {
            id: None,
            title: "Panel".into(),
            kind: PanelKind::Timeseries,
            queries: (0..(rows - start).min(MAX_QUERIES_PER_PANEL))
                .map(|target| PanelQuery {
                    ref_id: format!("Q{target}"),
                    expr: "up".into(),
                    dialect: QueryDialect::PromQL,
                    datasource: None,
                    min_step: String::new(),
                    unsupported: None,
                })
                .collect(),
            transformed: false,
            unit: String::new(),
            reducer: String::new(),
            min_interval: String::new(),
            unsupported: None,
        })
        .collect();
    Dashboard {
        source,
        uid: "pilot".into(),
        title: "Pilot".into(),
        panels,
        truncated,
    }
}

#[test]
fn grafana_projection_caps_the_ordered_prefix_inside_a_panel() {
    use k10s_data::integrations::grafana::{DashboardSource, MAX_QUERIES_PER_PANEL};
    use k10s_provider::{GrafanaOutcome, GrafanaRowIdentity};

    let limit = k10s_shell::table::MAX_ROWS;
    for (rows, source_truncated) in [(limit - 1, false), (limit, true), (limit + 1, false)] {
        let catalog = k10s_data::read::GrafanaCatalog {
            dashboards: vec![dashboard_with_query_rows(
                DashboardSource::Grafana,
                rows,
                source_truncated,
            )],
            extra_hits: Vec::new(),
            truncated: false,
            browser_base: None,
            browser_service: None,
            query_source: None,
            served: true,
            errors: Vec::new(),
        };
        let GrafanaOutcome::Catalog {
            panels,
            omitted,
            truncated,
            ..
        } = grafana_outcome(Fetched::Ok(catalog))
        else {
            panic!("the catalog must retain the decoded rows");
        };
        let retained = rows.min(limit);
        assert_eq!(panels.len(), retained);
        assert_eq!(omitted, rows - retained);
        assert_eq!(truncated, source_truncated);
        let last = panels.last().expect("the retained prefix is nonempty");
        assert_eq!(
            last.identity,
            GrafanaRowIdentity::Panel {
                id: None,
                position: (retained - 1) / MAX_QUERIES_PER_PANEL,
            }
        );
        assert_eq!(
            last.ref_id,
            format!("Q{}", (retained - 1) % MAX_QUERIES_PER_PANEL)
        );
    }
}

#[test]
fn grafana_projection_counts_later_sources_and_search_hits() {
    use k10s_data::integrations::grafana::{
        CatalogHit, CatalogHitReason, DashboardSource, MAX_QUERIES_PER_PANEL, Panel, PanelKind,
        SearchHit,
    };
    use k10s_provider::{GrafanaOutcome, GrafanaRowIdentity, GrafanaRowSource};

    let empty_panel = || Panel {
        id: None,
        title: "No targets".into(),
        kind: PanelKind::Unsupported,
        queries: Vec::new(),
        transformed: false,
        unit: String::new(),
        reducer: String::new(),
        min_interval: String::new(),
        unsupported: None,
    };
    let limit = k10s_shell::table::MAX_ROWS;
    let mut grafana = dashboard_with_query_rows(DashboardSource::Grafana, limit + 4, false);
    grafana.panels.push(empty_panel());
    let mut configmap = dashboard_with_query_rows(DashboardSource::ConfigMap, 0, true);
    configmap.panels.push(empty_panel());
    let unknown = dashboard_with_query_rows(DashboardSource::Unknown, 2, false);
    let hits = (0..2)
        .map(|index| CatalogHit {
            search: SearchHit {
                uid: format!("hit-{index}"),
                title: "Search result".into(),
                folder_title: String::new(),
                kind: "dash-db".into(),
            },
            reason: CatalogHitReason::FetchLimit,
        })
        .collect();
    let catalog = k10s_data::read::GrafanaCatalog {
        dashboards: vec![configmap, unknown, grafana],
        extra_hits: hits,
        truncated: false,
        browser_base: None,
        browser_service: None,
        query_source: None,
        served: true,
        errors: vec!["An upstream catalog notice".into()],
    };
    let GrafanaOutcome::Catalog {
        panels,
        omitted,
        truncated,
        notices,
    } = grafana_outcome(Fetched::Ok(catalog))
    else {
        panic!("the catalog must retain the decoded rows");
    };
    assert_eq!(panels.len(), limit);
    assert_eq!(omitted, 10);
    assert!(truncated);
    assert_eq!(notices, ["An upstream catalog notice"]);
    assert!(
        panels
            .iter()
            .all(|row| row.source == GrafanaRowSource::Grafana)
    );
    assert_eq!(
        panels.last().unwrap().identity,
        GrafanaRowIdentity::Panel {
            id: None,
            position: (limit - 1) / MAX_QUERIES_PER_PANEL,
        }
    );
    assert_eq!(
        panels.last().unwrap().ref_id,
        format!("Q{}", (limit - 1) % MAX_QUERIES_PER_PANEL)
    );
}

#[test]
fn grafana_projection_keeps_the_search_hit_prefix_at_the_limit() {
    use k10s_data::integrations::grafana::{
        CatalogHit, CatalogHitReason, DashboardSource, SearchHit,
    };
    use k10s_provider::{GrafanaOutcome, GrafanaRowIdentity};

    let limit = k10s_shell::table::MAX_ROWS;
    let hits = (0..4)
        .map(|index| CatalogHit {
            search: SearchHit {
                uid: format!("hit-{index}"),
                title: "Search result".into(),
                folder_title: String::new(),
                kind: "dash-db".into(),
            },
            reason: CatalogHitReason::Denied,
        })
        .collect();
    let catalog = k10s_data::read::GrafanaCatalog {
        dashboards: vec![dashboard_with_query_rows(
            DashboardSource::Grafana,
            limit - 2,
            false,
        )],
        extra_hits: hits,
        truncated: false,
        browser_base: None,
        browser_service: None,
        query_source: None,
        served: true,
        errors: Vec::new(),
    };
    let GrafanaOutcome::Catalog {
        panels,
        omitted,
        truncated,
        ..
    } = grafana_outcome(Fetched::Ok(catalog))
    else {
        panic!("the search hits must remain represented");
    };
    assert_eq!(panels.len(), limit);
    assert_eq!(omitted, 2);
    assert!(!truncated);
    assert_eq!(panels[limit - 2].identity, GrafanaRowIdentity::Dashboard);
    assert_eq!(panels[limit - 2].dashboard_uid, "hit-0");
    assert_eq!(panels[limit - 1].identity, GrafanaRowIdentity::Dashboard);
    assert_eq!(panels[limit - 1].dashboard_uid, "hit-1");
}

#[test]
fn grafana_missing_panel_ids_and_repeated_targets_keep_local_row_positions() {
    use k10s_data::integrations::grafana::{Dashboard, Panel, PanelKind, PanelQuery, QueryDialect};
    use k10s_provider::GrafanaRowIdentity;

    let query = |expr: &str| PanelQuery {
        ref_id: "A".into(),
        expr: expr.into(),
        dialect: QueryDialect::PromQL,
        datasource: None,
        min_step: String::new(),
        unsupported: None,
    };
    let panel = |title: &str, queries| Panel {
        id: None,
        title: title.into(),
        kind: PanelKind::Timeseries,
        queries,
        transformed: false,
        unit: String::new(),
        reducer: String::new(),
        min_interval: String::new(),
        unsupported: None,
    };
    let catalog = k10s_data::read::GrafanaCatalog {
        dashboards: vec![Dashboard {
            source: k10s_data::integrations::grafana::DashboardSource::Grafana,
            uid: "pilot".into(),
            title: "Pilot dashboard".into(),
            panels: vec![
                panel("First", vec![query("up"), query("down")]),
                panel("Second", vec![query("ready")]),
            ],
            truncated: false,
        }],
        extra_hits: Vec::new(),
        truncated: false,
        browser_base: Some("http://grafana.test".into()),
        browser_service: None,
        query_source: None,
        served: true,
        errors: Vec::new(),
    };
    let k10s_provider::GrafanaOutcome::Catalog { panels, .. } =
        grafana_outcome(Fetched::Ok(catalog))
    else {
        panic!("the catalog must preserve all targets");
    };
    assert_eq!(panels.len(), 3);
    for (position, panel) in [0, 0, 1].into_iter().zip(&panels) {
        assert_eq!(
            panel.identity,
            GrafanaRowIdentity::Panel { id: None, position }
        );
        assert_eq!(panel.ref_id, "A");
        assert_eq!(
            panel.browser_url.as_deref(),
            Some("http://grafana.test/d/pilot")
        );
    }
    assert_eq!(panels[0].expr, "up");
    assert_eq!(panels[1].expr, "down");
    assert_eq!(panels[2].expr, "ready");
}

#[test]
fn grafana_rows_label_unsupported_target_dialects() {
    use k10s_data::integrations::grafana::{Dashboard, Panel, PanelKind, PanelQuery, QueryDialect};

    let panel = |dialect, unsupported| Panel {
        id: Some(1),
        title: "Panel".into(),
        kind: PanelKind::Timeseries,
        queries: vec![PanelQuery {
            ref_id: "A".into(),
            expr: "query".into(),
            dialect,
            datasource: None,
            min_step: String::new(),
            unsupported,
        }],
        transformed: false,
        unit: String::new(),
        reducer: String::new(),
        min_interval: String::new(),
        unsupported: None,
    };
    let catalog = k10s_data::read::GrafanaCatalog {
        dashboards: vec![Dashboard {
            source: k10s_data::integrations::grafana::DashboardSource::Grafana,
            uid: "pilot".into(),
            title: "Pilot".into(),
            panels: vec![
                panel(QueryDialect::PromQL, None),
                panel(QueryDialect::LogQL, None),
                panel(QueryDialect::TraceQL, None),
                panel(QueryDialect::Unknown, None),
                panel(QueryDialect::LogQL, Some("specific target failure".into())),
            ],
            truncated: false,
        }],
        extra_hits: Vec::new(),
        truncated: false,
        browser_base: None,
        browser_service: None,
        query_source: None,
        served: true,
        errors: Vec::new(),
    };
    let k10s_provider::GrafanaOutcome::Catalog { panels, .. } =
        grafana_outcome(Fetched::Ok(catalog))
    else {
        panic!("the catalog must retain its panel targets");
    };
    let reasons: Vec<_> = panels
        .iter()
        .map(|panel| panel.unsupported.as_deref())
        .collect();
    assert_eq!(
        reasons,
        [
            None,
            Some(
                "Grafana LogQL panel targets are unsupported; use a raw LogQL query or open in Grafana"
            ),
            Some("Grafana TraceQL panel targets are unsupported; open in Grafana"),
            Some("Grafana panel datasource query dialect is unknown"),
            Some("specific target failure"),
        ]
    );
}

#[test]
fn grafana_panel_rows_keep_source_and_only_link_verified_panels() {
    use k10s_data::integrations::grafana::{
        Dashboard, DashboardSource, Panel, PanelKind, PanelQuery, QueryDialect,
    };
    use k10s_provider::GrafanaRowSource;

    let dashboard = |source, uid: &str, id, expr: &str| Dashboard {
        source,
        uid: uid.into(),
        title: "Pilot".into(),
        panels: vec![Panel {
            id,
            title: "CPU".into(),
            kind: PanelKind::Timeseries,
            queries: vec![PanelQuery {
                ref_id: "A".into(),
                expr: expr.into(),
                dialect: QueryDialect::PromQL,
                datasource: None,
                min_step: String::new(),
                unsupported: None,
            }],
            transformed: false,
            unit: String::new(),
            reducer: String::new(),
            min_interval: String::new(),
            unsupported: None,
        }],
        truncated: false,
    };
    let catalog = k10s_data::read::GrafanaCatalog {
        dashboards: vec![
            dashboard(DashboardSource::ConfigMap, "same", Some(9), "file_query"),
            dashboard(DashboardSource::Grafana, "same", Some(5), "live_query"),
            dashboard(DashboardSource::ConfigMap, "", Some(7), "uidless_query"),
            dashboard(DashboardSource::Unknown, "other", Some(3), "unknown_query"),
        ],
        extra_hits: Vec::new(),
        truncated: false,
        browser_base: Some("http://grafana.test/prefix".into()),
        browser_service: Some(("monitoring".into(), "grafana".into(), 3000)),
        query_source: Some(test_grafana_service(3000)),
        served: true,
        errors: Vec::new(),
    };
    let mut browser_only = catalog.clone();
    let configured = "https://grafana.test/custom?orgId=1#panel";
    browser_only.browser_base = Some(configured.into());
    browser_only.browser_service = None;
    let k10s_provider::GrafanaOutcome::Catalog { panels, .. } =
        grafana_outcome(Fetched::Ok(catalog))
    else {
        panic!("the source rows must remain available");
    };
    assert_eq!(panels.len(), 4);
    assert_eq!(panels[0].source, GrafanaRowSource::Grafana);
    assert_eq!(panels[0].expr, "live_query");
    assert_eq!(
        panels[0].browser_url.as_deref(),
        Some("http://grafana.test/prefix/d/same?viewPanel=5")
    );
    assert_eq!(panels[1].source, GrafanaRowSource::ConfigMap);
    assert_eq!(panels[1].expr, "file_query");
    assert_eq!(
        panels[1].browser_url.as_deref(),
        Some("http://grafana.test/prefix/d/same")
    );
    assert_eq!(panels[2].source, GrafanaRowSource::ConfigMap);
    assert_eq!(panels[2].dashboard_uid, "");
    assert_eq!(
        panels[2].browser_url.as_deref(),
        Some("http://grafana.test/prefix")
    );
    assert_eq!(
        panels[2]
            .browser_forward
            .as_ref()
            .and_then(|r| r.service_port),
        Some(3000)
    );
    assert_eq!(panels[3].source, GrafanaRowSource::Unknown);
    assert_eq!(
        panels[3].browser_url.as_deref(),
        Some("http://grafana.test/prefix/d/other")
    );

    let k10s_provider::GrafanaOutcome::Catalog { panels, .. } =
        grafana_outcome(Fetched::Ok(browser_only))
    else {
        panic!("the configured browser destination must remain available");
    };
    assert!(
        panels
            .iter()
            .all(|panel| panel.browser_url.as_deref() == Some(configured))
    );
    assert!(panels.iter().all(|panel| panel.browser_forward.is_none()));
}

#[test]
fn grafana_unimplemented_time_and_field_semantics_keep_browser_routes() {
    use k10s_data::integrations::grafana::{DashboardSource, parse_dashboard};
    use serde_json::json;

    for (mut panel, reason) in [
        (
            json!({"type":"timeseries","timeFrom":"5m"}),
            "unsupported Grafana panel relative time override",
        ),
        (
            json!({"type":"timeseries","timeShift":"1d"}),
            "unsupported Grafana panel time shift",
        ),
        (
            json!({
                "type":"stat",
                "options":{"reduceOptions":{"fields":"/^requests$/","calcs":["lastNotNull"]}}
            }),
            "unsupported Grafana stat field selection",
        ),
    ] {
        panel["id"] = json!(7);
        panel["title"] = json!("Requests");
        panel["datasource"] = json!({"type":"prometheus","uid":"prom"});
        panel["targets"] = json!([{"refId":"A","expr":"up"}]);
        let document = json!({"uid":"requests","title":"Service","panels":[panel]});
        let dashboard = parse_dashboard(&serde_json::to_vec(&document).unwrap()).unwrap();
        for (source, browser_url) in [
            (
                DashboardSource::Grafana,
                "http://grafana.test/d/requests?viewPanel=7",
            ),
            (DashboardSource::ConfigMap, "http://grafana.test/d/requests"),
        ] {
            let mut dashboard = dashboard.clone();
            dashboard.source = source;
            let catalog = k10s_data::read::GrafanaCatalog {
                dashboards: vec![dashboard],
                extra_hits: Vec::new(),
                truncated: false,
                browser_base: Some("http://grafana.test".into()),
                browser_service: Some(("monitoring".into(), "grafana".into(), 3000)),
                query_source: Some(test_grafana_service(3000)),
                served: true,
                errors: Vec::new(),
            };
            let k10s_provider::GrafanaOutcome::Catalog { panels, .. } =
                grafana_outcome(Fetched::Ok(catalog))
            else {
                panic!("unsupported semantics must keep the dashboard available");
            };
            assert_eq!(panels.len(), 1);
            assert_eq!(panels[0].unsupported.as_deref(), Some(reason));
            assert_eq!(panels[0].expr, "up");
            assert_eq!(panels[0].datasource_uid.as_deref(), Some("prom"));
            assert_eq!(panels[0].browser_url.as_deref(), Some(browser_url));
            assert_eq!(
                panels[0].browser_forward,
                Some(k10s_provider::ForwardRequest {
                    namespace: "monitoring".into(),
                    name: "grafana".into(),
                    service: true,
                    service_port: Some(3000),
                    expected_service: Some(test_grafana_service(3000)),
                })
            );
        }
    }
}

#[test]
fn grafana_catalog_reports_source_and_dashboard_truncation() {
    use k10s_data::integrations::grafana::Dashboard;

    for (source_truncated, dashboard_truncated) in [(false, false), (true, false), (false, true)] {
        let catalog = k10s_data::read::GrafanaCatalog {
            dashboards: vec![Dashboard {
                source: k10s_data::integrations::grafana::DashboardSource::Grafana,
                uid: "pilot".into(),
                title: "Pilot".into(),
                panels: Vec::new(),
                truncated: dashboard_truncated,
            }],
            extra_hits: Vec::new(),
            truncated: source_truncated,
            browser_base: None,
            browser_service: None,
            query_source: None,
            served: true,
            errors: Vec::new(),
        };
        let k10s_provider::GrafanaOutcome::Catalog { truncated, .. } =
            grafana_outcome(Fetched::Ok(catalog))
        else {
            panic!("the catalog must retain the dashboard result");
        };
        assert_eq!(truncated, source_truncated || dashboard_truncated);
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
            query_source: None,
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
fn grafana_multiple_denial_labels_survive_projection() {
    let what = "provisioned dashboard configmaps and Grafana search";
    assert_eq!(
        grafana_outcome(Fetched::Denied { what }),
        k10s_provider::GrafanaOutcome::Denied(what)
    );
}

#[test]
fn grafana_multiple_source_failures_survive_projection() {
    let why = "provisioned dashboards: store unavailable; Grafana search: access denied";
    assert_eq!(
        grafana_outcome(Fetched::Failed {
            what: "grafana",
            why: why.to_string(),
        }),
        k10s_provider::GrafanaOutcome::Failed(why.to_string())
    );
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

#[test]
fn categorized_log_entries_preserve_all_trace_links_and_current_uid() {
    let values: Vec<_> = (1_u64..=90).map(|index| serde_json::json!([
        index.to_string(), "line", {
            "structuredMetadata":{"trace_id":format!("{index:032X}"),"k8s_pod_uid":"current"},
            "parsed":{"trace_id":"ffffffffffffffffffffffffffffffff","k8s_pod_uid":"parsed"}
        }
    ])).collect();
    let body = serde_json::json!({"status":"success","data":{"resultType":"streams","result":[{
        "stream":{"k8s_namespace_name":"prod","k8s_pod_name":"api","k8s_pod_uid":"retired"}, "values":values
    }]}}).to_string();
    let logs = k10s_data::integrations::loki::parse(body.as_bytes()).unwrap();
    assert_eq!(logs.streams.len(), 1);
    assert!(!logs.truncated);
    let k10s_provider::LokiOutcome::Logs { entries, .. } = loki_outcome(Fetched::Ok(Some(logs)))
    else {
        panic!("categorized logs must remain entries")
    };
    assert_eq!(entries.len(), 90);
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry.timestamp_ns, (90 - index) as u64);
        assert_eq!(entry.trace_id, Some(format!("{:032x}", 90 - index)));
        assert_eq!(entry.attribute("k8s_pod_uid"), Some("current"));
    }
}

#[test]
fn parsed_only_log_fields_do_not_create_correlation_identity() {
    for metadata in [
        serde_json::json!({"parsed":{
            "trace_id":"0123456789abcdef0123456789abcdef",
            "span_id":"0123456789abcdef","k8s_pod_uid":"parsed"
        }}),
        serde_json::json!({"structuredMetadata":{},"parsed":{
            "trace_id":"0123456789abcdef0123456789abcdef",
            "span_id":"0123456789abcdef","k8s_pod_uid":"parsed"
        }}),
    ] {
        let body = serde_json::json!({"status":"success","data":{"resultType":"streams","result":[{
            "stream":{"k8s_namespace_name":"prod","k8s_pod_name":"api"},
            "values":[["1","line",metadata]]
        }]}})
        .to_string();
        let logs = k10s_data::integrations::loki::parse(body.as_bytes()).unwrap();
        let k10s_provider::LokiOutcome::Logs { entries, .. } =
            loki_outcome(Fetched::Ok(Some(logs)))
        else {
            panic!("parsed-only fields must not hide the log line")
        };
        assert_eq!(entries.len(), 1);
        assert!(entries[0].metadata.is_empty());
        assert_eq!(entries[0].trace_id, None);
        for key in ["trace_id", "span_id", "k8s_pod_uid"] {
            assert_eq!(entries[0].attribute(key), None);
        }
    }
}

#[test]
fn grafana_rows_share_the_verified_source_and_never_create_an_unpinned_browser_request() {
    use k10s_data::integrations::grafana::{
        CatalogHit, CatalogHitReason, DashboardSource, SearchHit,
    };
    use k10s_provider::{GrafanaOutcome, GrafanaRowSource};
    use std::sync::Arc;

    let source = test_grafana_service(3000);
    let mut catalog = k10s_data::read::GrafanaCatalog {
        dashboards: vec![dashboard_with_query_rows(
            DashboardSource::ConfigMap,
            2,
            false,
        )],
        extra_hits: vec![CatalogHit {
            search: SearchHit {
                uid: "search-only".into(),
                title: "Search only".into(),
                folder_title: String::new(),
                kind: "dash-db".into(),
            },
            reason: CatalogHitReason::FetchLimit,
        }],
        truncated: false,
        browser_base: Some("http://grafana.test".into()),
        browser_service: Some((source.namespace.clone(), source.name.clone(), source.port)),
        query_source: Some(source.clone()),
        served: true,
        errors: Vec::new(),
    };
    let GrafanaOutcome::Catalog { panels, .. } = grafana_outcome(Fetched::Ok(catalog.clone()))
    else {
        panic!("the source catalogue must project");
    };
    assert_eq!(panels.len(), 3);
    assert_eq!(panels[0].source, GrafanaRowSource::ConfigMap);
    assert_eq!(panels[1].source, GrafanaRowSource::ConfigMap);
    assert_eq!(panels[2].source, GrafanaRowSource::Grafana);
    for panel in &panels {
        assert!(Arc::ptr_eq(panel.query_source.as_ref().unwrap(), &source));
        let request = panel
            .browser_forward
            .as_ref()
            .expect("the verified browser source");
        assert!(Arc::ptr_eq(
            request.expected_service.as_ref().unwrap(),
            &source
        ));
    }
    catalog.query_source = None;
    let GrafanaOutcome::Catalog { panels, .. } = grafana_outcome(Fetched::Ok(catalog.clone()))
    else {
        panic!("unverified definitions stay readable");
    };
    assert_eq!(panels.len(), 3);
    assert!(
        panels
            .iter()
            .all(|panel| panel.query_source.is_none() && panel.browser_forward.is_none())
    );
    assert!(panels.iter().all(|panel| panel.browser_url.is_some()));
    catalog.query_source = Some(source);
    catalog.browser_service.as_mut().unwrap().1 = "another-grafana".into();
    let GrafanaOutcome::Catalog { panels, .. } = grafana_outcome(Fetched::Ok(catalog)) else {
        panic!("mismatched browser routing keeps the catalogue readable");
    };
    assert!(panels.iter().all(|panel| panel.browser_forward.is_none()));
}
