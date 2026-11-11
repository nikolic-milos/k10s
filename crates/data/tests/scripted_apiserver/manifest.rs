//! The write path against the scripted server: an editable manifest from one
//! get, the last-applied annotation as a diff base, and the dry run and apply
//! that follow -- conflicts named per field and manager, a strict rejection
//! naming the field the server would have dropped, a denial that never
//! reaches the server at all, and an unconfirmed real apply told apart from
//! a failed dry run.

use crate::*;

#[test]
fn a_manifest_renders_editable_yaml_from_one_get() {
    use k10s_core::KindId;
    use k10s_data::describe::DescribeRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        200,
        pod_json("api-1", "uid-pod-1", false),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_manifest(
        DescribeRequest {
            kind: KindId::POD,
            namespace: Some("prod".to_string()),
            name: "api-1".to_string(),
            uid: "uid-pod-1".to_string(),
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Ok(manifest) = wait(&rx) else {
        panic!("the manifest must resolve");
    };
    assert_eq!(manifest.title, "api-1.yaml");
    assert_eq!(manifest.api_version, "v1");
    assert_eq!(manifest.kind, "Pod");
    let lines: Vec<&str> = manifest.yaml.lines().collect();
    assert_eq!(lines[0], "apiVersion: v1");
    assert_eq!(lines[1], "kind: Pod");
    assert_eq!(lines[2], "metadata:");
    assert!(manifest.yaml.contains("image: nginx"));
    assert!(
        manifest.yaml.trim_end().ends_with("phase: Running"),
        "status renders last: {}",
        manifest.yaml
    );
    let fetches = script.requests_for("/api/v1/namespaces/prod/pods/api-1");
    assert_eq!(fetches.len(), 1, "one GET builds the manifest: {fetches:?}");

    drop(runtime);
}
#[test]
fn a_secret_manifest_is_structurally_metadata_only_and_says_so() {
    use k10s_core::KindId;
    use k10s_data::describe::DescribeRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route_accepting(
        "GET",
        "/api/v1/namespaces/prod/secrets/api-token",
        "as=PartialObjectMetadata",
        200,
        r#"{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
            "metadata":{"name":"api-token","namespace":"prod","uid":"uid-sec"}}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_manifest(
        DescribeRequest {
            kind: KindId::SECRET,
            namespace: Some("prod".to_string()),
            name: "api-token".to_string(),
            uid: "uid-sec".to_string(),
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Ok(manifest) = wait(&rx) else {
        panic!("the secret manifest must resolve");
    };
    assert!(
        manifest.yaml.starts_with("# values withheld"),
        "the projection is announced: {}",
        manifest.yaml
    );
    assert!(manifest.yaml.contains("kind: Secret"));
    assert!(
        !manifest
            .yaml
            .lines()
            .any(|line| line.starts_with("data:") || line.starts_with("stringData:")),
        "no value field ever renders: {}",
        manifest.yaml
    );
    let fetches = script.requests_for("/api/v1/namespaces/prod/secrets/api-token");
    assert_eq!(fetches.len(), 1);
    assert!(
        fetches[0].accept.contains("PartialObjectMetadata"),
        "the wire itself is metadata-only: {}",
        fetches[0].accept
    );

    drop(runtime);
}
// Every dry run reads the object with it; these are the reads, one per dry
// run, each consumed once.
fn route_reads(script: &Script, dry_runs: usize) {
    for _ in 0..dry_runs {
        script.route(
            "GET",
            "/api/v1/namespaces/prod/pods/api-1",
            200,
            pod_json("api-1", "uid-pod-1", false),
        );
    }
}
// Exactly the reads `route_reads` scripted were made and answered. Without them
// the harness answers 404, which a dry run takes for an absent object, so a test
// would quietly review a creation instead of the object it names.
fn assert_reads_used(script: &Script, dry_runs: usize) {
    assert_eq!(
        script.answered("GET", "/api/v1/namespaces/prod/pods/api-1"),
        (dry_runs, dry_runs),
        "every dry run read the object from its scripted route"
    );
}

fn patches(script: &Script) -> Vec<Seen> {
    script
        .requests_for("/pods/api-1")
        .into_iter()
        .filter(|seen| seen.method == "PATCH")
        .collect()
}

#[test]
fn an_unreadable_2xx_confirms_a_real_apply_but_cannot_be_reviewed() {
    use k10s_data::apply::ApplyOutcome;
    for body in [
        b"stored".to_vec(),
        b"{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"uid\":\"uid-pod-1\"},\"data\":{\"value\":\"\xff\"}}".to_vec(),
    ] {
        for dry_run in [false, true] {
            let script = Script::default();
            script_discovery(&script);
            script_rules_review(&script);
            script_access_reviews(&script, true, 32);
            script_lists(&script);
            route_reads(&script, usize::from(dry_run));
            script.route_bytes(
                "PATCH",
                "/api/v1/namespaces/prod/pods/api-1?",
                200,
                body.clone(),
            );
            let runtime = runtime();
            let (sync, _live) = sync_on(&runtime, &script);
            let yaml = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: api-1\n";
            let (tx, rx) = std::sync::mpsc::channel();
            sync.reader
                .apply(apply_request(yaml, dry_run, false), move |outcome| {
                    let _ = tx.send(outcome);
                });
            match wait(&rx) {
                ApplyOutcome::Applied(applied) if !dry_run => assert_eq!(applied.uid, None),
                ApplyOutcome::Unrendered(unrendered) if dry_run => assert!(
                    unrendered.why.starts_with("the answer is not JSON:"),
                    "{}",
                    unrendered.why
                ),
                other => panic!("2xx confirms the write; only a dry run needs its body: {other:?}"),
            }
            let writes = patches(&script);
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].method, "PATCH");
            let dry = if dry_run { "dryRun=All&" } else { "" };
            assert_eq!(
                writes[0].path,
                format!(
                    "/api/v1/namespaces/prod/pods/api-1?&{dry}fieldManager=k10s&fieldValidation=Strict"
                )
            );
            assert_eq!(writes[0].accept, "application/json");
            assert_eq!(writes[0].content_type, "application/apply-patch+yaml");
            assert_eq!(writes[0].body, yaml);
            assert_reads_used(&script, usize::from(dry_run));
            let reads = script
                .requests_for("/api/v1/namespaces/prod/pods/api-1")
                .into_iter()
                .filter(|sent| sent.method == "GET")
                .collect::<Vec<_>>();
            assert_eq!(reads.len(), usize::from(dry_run));
            if let Some(read) = reads.first() {
                assert_eq!(read.path, "/api/v1/namespaces/prod/pods/api-1");
                assert_eq!(read.accept, "");
                assert!(read.body.is_empty());
            }
        }
    }
}

fn apply_request(yaml: &str, dry_run: bool, force: bool) -> k10s_data::apply::ApplyRequest {
    k10s_data::apply::ApplyRequest {
        kind: KindId::POD,
        namespace: Some("prod".to_string()),
        name: "api-1".to_string(),
        yaml: yaml.to_string(),
        dry_run,
        force,
    }
}
#[test]
fn a_manifest_carries_the_last_applied_configuration_as_its_diff_base() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        200,
        pod_with_last_applied(),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_manifest(pod_request(), move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(manifest) = wait(&rx) else {
        panic!("the manifest must resolve");
    };

    assert!(
        !manifest.yaml.contains("last-applied-configuration"),
        "apply bookkeeping is not part of the document being edited: {}",
        manifest.yaml
    );
    assert!(
        !manifest.yaml.contains("managedFields"),
        "nor is the field-manager ledger: {}",
        manifest.yaml
    );
    assert!(
        manifest.yaml.contains("team: platform"),
        "an annotation that is not bookkeeping stays: {}",
        manifest.yaml
    );
    assert!(manifest.yaml.contains("image: nginx:1.27"));

    let base = manifest.last_applied.expect("the annotation was there");
    assert!(
        base.contains("image: nginx:1.26"),
        "the base is what was declared, not what is live: {base}"
    );
    assert!(
        base.starts_with("apiVersion: v1\nkind: Pod\n"),
        "the base renders through the same emitter, so the two are comparable: {base}"
    );
    assert!(
        manifest.patchable && manifest.status_subresource,
        "discovery said pods take a patch and have a status subresource"
    );
    // Whose object the text is, from the response that produced it. An apply's
    // answer carries the same field, and comparing the two is what tells an
    // update from a recreation -- server-side apply creates what is absent.
    assert_eq!(manifest.uid.as_deref(), Some("uid-pod-1"));

    drop(runtime);
}
#[test]
fn an_object_with_no_last_applied_annotation_has_no_base_rather_than_an_invented_one() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        200,
        pod_json("api-1", "uid-pod-1", false),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_manifest(pod_request(), move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(manifest) = wait(&rx) else {
        panic!("the manifest must resolve");
    };
    assert_eq!(manifest.last_applied, None);

    drop(runtime);
}
#[test]
fn a_dry_run_apply_sends_the_buffer_verbatim_and_answers_with_what_would_be_stored() {
    use k10s_data::apply::ApplyOutcome;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    route_reads(&script, 1);
    script.route(
        "PATCH",
        "/api/v1/namespaces/prod/pods/api-1?",
        200,
        pod_with_last_applied(),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let sent = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: api-1\nspec:\n  containers:\n    - image: nginx:1.28\n      name: app\n";
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request(sent, true, false), move |outcome| {
            let _ = tx.send(outcome);
        });
    let ApplyOutcome::DryRun(applied) = wait(&rx) else {
        panic!("the dry run must resolve");
    };
    assert!(
        matches!(applied.live, k10s_data::apply::Live::Read { .. }),
        "the scripted read is the left side, not an absent object: {:?}",
        applied.live
    );
    assert!(
        applied.yaml.starts_with("apiVersion: v1\nkind: Pod\n"),
        "the server's answer renders like the document the editor opened: {}",
        applied.yaml
    );
    assert!(
        !applied.yaml.contains("managedFields"),
        "including the ledger the apply just wrote: {}",
        applied.yaml
    );
    // Which object the server answered about. A reviewer holding the uid the
    // document was read at can tell an update from a recreation with no second
    // round trip, and this is the field that makes that possible.
    assert_eq!(applied.uid.as_deref(), Some("uid-pod-1"));

    let writes = patches(&script);
    assert_eq!(
        writes.len(),
        1,
        "one dry run is one PATCH after its read: {writes:?}"
    );
    let write = &writes[0];
    assert_eq!(write.method, "PATCH");
    assert_eq!(
        write.content_type, "application/apply-patch+yaml",
        "the media type whose point is that the bytes may be YAML"
    );
    assert_eq!(
        write.body, sent,
        "the bytes on the wire are the buffer's own"
    );
    assert!(
        write.path.contains("dryRun=All"),
        "a dry run says so on the query: {}",
        write.path
    );
    assert!(
        write.path.contains("fieldManager=k10s"),
        "and names us as the manager: {}",
        write.path
    );
    assert!(
        write.path.contains("fieldValidation=Strict"),
        "and asks the server to reject rather than drop unknown fields: {}",
        write.path
    );
    assert!(
        !write.path.contains("force"),
        "nothing is forced until a conflict names what would be taken: {}",
        write.path
    );

    assert_reads_used(&script, 1);
    drop(runtime);
}
#[test]
fn a_conflict_names_fields_before_a_forced_preview_and_real_apply() {
    use k10s_data::apply::ApplyOutcome;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    // The first dry run conflicts. A forced preview precedes the real write.
    route_reads(&script, 2);
    script.route(
        "PATCH",
        "/api/v1/namespaces/prod/pods/api-1?",
        409,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":409,"reason":"Conflict",
            "message":"Apply failed with 1 conflict: conflict with \"kubectl\" using v1",
            "details":{"causes":[{"reason":"FieldManagerConflict",
              "message":"conflict with \"kubectl\" using v1",
              "field":".spec.containers[name=\"app\"].image"}]}}"#,
    );
    for _ in 0..2 {
        script.route(
            "PATCH",
            "/api/v1/namespaces/prod/pods/api-1?",
            200,
            pod_json("api-1", "uid-pod-1", false),
        );
    }

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let sent = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: api-1\n";
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request(sent, true, false), move |outcome| {
            let _ = tx.send(outcome);
        });
    let ApplyOutcome::Conflict {
        message,
        causes,
        truncated,
    } = wait(&rx)
    else {
        panic!("a 409 is a conflict, not a failure");
    };
    assert!(!truncated);
    assert!(message.contains("Apply failed with 1 conflict"));
    assert_eq!(causes.len(), 1);
    assert_eq!(causes[0].field, ".spec.containers[name=\"app\"].image");
    assert_eq!(causes[0].manager, "kubectl");

    for dry_run in [true, false] {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader
            .apply(apply_request(sent, dry_run, true), move |outcome| {
                let _ = tx.send(outcome);
            });
        let outcome = wait(&rx);
        if dry_run {
            assert!(
                matches!(&outcome, ApplyOutcome::DryRun(dry) if matches!(dry.live, k10s_data::apply::Live::Read { .. })),
                "{outcome:?}"
            );
        } else {
            assert!(matches!(outcome, ApplyOutcome::Applied(_)), "{outcome:?}");
        }
    }

    let writes = patches(&script);
    assert_eq!(writes.len(), 3);
    let queries = [
        "dryRun=All&fieldManager=k10s&fieldValidation=Strict",
        "dryRun=All&force=true&fieldManager=k10s&fieldValidation=Strict",
        "force=true&fieldManager=k10s&fieldValidation=Strict",
    ];
    for (write, query) in writes.iter().zip(queries) {
        assert_eq!(write.method, "PATCH");
        assert_eq!(
            write.path,
            format!("/api/v1/namespaces/prod/pods/api-1?&{query}")
        );
        assert_eq!(write.accept, "application/json");
        assert_eq!(write.content_type, "application/apply-patch+yaml");
        assert_eq!(write.body, sent);
    }

    assert_reads_used(&script, 2);
    drop(runtime);
}
// A write the server accepted, whose echo the editor's emitter will not render:
// a custom resource with `x-kubernetes-preserve-unknown-fields` can nest past
// the 64 levels the emitter allows, and nothing between the buffer and the wire
// caps depth. Reported as a failure, that told the user the cluster was
// unchanged while it held the object.
#[test]
fn an_answer_too_deep_to_render_is_still_a_write_that_happened() {
    use k10s_data::apply::ApplyOutcome;

    let mut deep = serde_json::json!("leaf");
    for _ in 0..70 {
        deep = serde_json::json!({ "nest": deep });
    }
    let body = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "api-1", "namespace": "prod", "uid": "uid-pod-1" },
        "spec": deep,
    })
    .to_string();

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    route_reads(&script, 1);
    script.route("PATCH", "/api/v1/namespaces/prod/pods/api-1?", 200, &body);
    script.route("PATCH", "/api/v1/namespaces/prod/pods/api-1?", 200, &body);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request("kind: Pod\n", false, false), move |outcome| {
            let _ = tx.send(outcome);
        });
    // A real apply's answer is not rendered, so its depth cannot turn a stored
    // write into anything but one.
    let ApplyOutcome::Applied(applied) = wait(&rx) else {
        panic!("the object is stored, and the outcome says so");
    };
    assert_eq!(applied.uid.as_deref(), Some("uid-pod-1"));

    // The same answer to a dry run is what a person would review, and it
    // cannot be shown: the state says which cap it hit.
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request("kind: Pod\n", true, false), move |outcome| {
            let _ = tx.send(outcome);
        });
    let ApplyOutcome::Unrendered(unrendered) = wait(&rx) else {
        panic!("a dry run whose answer will not render says so");
    };
    assert!(
        unrendered.why.contains("nests deeper"),
        "and it says which cap it hit: {}",
        unrendered.why
    );

    assert_reads_used(&script, 1);
    drop(runtime);
}
#[test]
fn a_strict_rejection_names_the_field_the_server_would_have_dropped() {
    use k10s_data::apply::ApplyOutcome;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    route_reads(&script, 1);
    script.route(
        "PATCH",
        "/api/v1/namespaces/prod/pods/api-1?",
        400,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":400,"reason":"BadRequest",
            "message":"strict decoding error",
            "details":{"causes":[{"reason":"FieldValueInvalid",
              "message":"unknown field \"spec.containerz\"","field":""}]}}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request("kind: Pod\n", true, false), move |outcome| {
            let _ = tx.send(outcome);
        });
    let ApplyOutcome::Rejected { message, causes } = wait(&rx) else {
        panic!("a 400 is a rejection");
    };
    assert!(message.contains("strict decoding error"));
    assert_eq!(
        causes,
        vec!["unknown field \"spec.containerz\"".to_string()]
    );

    assert_reads_used(&script, 1);
    drop(runtime);
}
#[test]
fn a_denied_apply_is_a_denial_and_a_kind_with_no_patch_verb_never_reaches_the_wire() {
    use k10s_data::apply::ApplyOutcome;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "PATCH",
        "/api/v1/namespaces/prod/pods/api-1?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"pods is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request("kind: Pod\n", false, false), move |outcome| {
            let _ = tx.send(outcome);
        });
    let ApplyOutcome::Denied { what, why } = wait(&rx) else {
        panic!("a 403 on a write is a denial");
    };
    assert_eq!(what, "apply");
    assert!(
        why.contains("pods is forbidden"),
        "and it keeps what the server said: {why}"
    );

    // The Secret in this server's discovery has no patch verb, which is not a
    // permission problem and must not be reported as one.
    let (tx, rx) = std::sync::mpsc::channel();
    let secret = k10s_data::apply::ApplyRequest {
        kind: KindId::SECRET,
        namespace: Some("prod".to_string()),
        name: "api-token".to_string(),
        yaml: "kind: Secret\n".to_string(),
        dry_run: true,
        force: false,
    };
    sync.reader.apply(secret, move |outcome| {
        let _ = tx.send(outcome);
    });
    let ApplyOutcome::Failed { why } = wait(&rx) else {
        panic!("an unpatchable kind is a labelled failure, not a denial");
    };
    assert!(
        why.contains("without a patch verb"),
        "the reason names the server's own contract: {why}"
    );
    assert!(
        script.requests_for("/secrets/api-token").is_empty(),
        "and nothing was sent"
    );

    drop(runtime);
}

// An apply and its dry run are the same request with one parameter between
// them, and both take the interactive client: a confirmed write never retries
// and never waits without bound, and the dry run decides it.
#[test]
fn an_apply_and_its_dry_run_take_the_interactive_client() {
    use k10s_data::apply::ApplyOutcome;

    let sync = Script::default();
    script_discovery(&sync);
    script_rules_review(&sync);
    script_access_reviews(&sync, true, 32);
    script_lists(&sync);
    let interactive = Script::default();
    let pod = r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"api-1","namespace":"prod","uid":"uid-pod"}}"#;
    interactive.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, pod);
    interactive.route("PATCH", "/api/v1/namespaces/prod/pods/api-1?", 200, pod);
    interactive.route("PATCH", "/api/v1/namespaces/prod/pods/api-1?", 200, pod);

    let runtime = runtime();
    let (data, _live) = sync_on_split(&runtime, &sync, &interactive);
    let before = sync.seen().len();
    for dry_run in [true, false] {
        let (tx, rx) = std::sync::mpsc::channel();
        data.reader.apply(
            apply_request("kind: Pod\n", dry_run, false),
            move |outcome| {
                let _ = tx.send(outcome);
            },
        );
        let outcome = wait(&rx);
        if dry_run {
            assert!(
                matches!(&outcome, ApplyOutcome::DryRun(dry) if matches!(dry.live, k10s_data::apply::Live::Read { .. })),
                "the read on the interactive client is the left side: {outcome:?}"
            );
        } else {
            assert!(matches!(outcome, ApplyOutcome::Applied(_)), "{outcome:?}");
        }
    }
    // The dry run's read decides the review, so it takes the same client.
    let sent = interactive.seen();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[0].method, "GET");
    assert!(sent[1].path.contains("dryRun=All"), "{:?}", sent[1].path);
    assert!(!sent[2].path.contains("dryRun"), "{:?}", sent[2].path);
    let stray = after_sync(&sync, before);
    assert!(stray.is_empty(), "nothing on the sync client: {stray:?}");
    drop(runtime);
}

// A real apply answered 504, or 500 the way the lab's server answered a write
// it had stored (C1/lab/after-storage), or whose connection is lost, may have
// landed. Its dry run stored nothing either way. Each press is one request;
// nothing here sends it again.
#[test]
fn a_real_apply_with_no_verdict_is_unconfirmed_and_its_dry_run_failed() {
    use k10s_data::apply::ApplyOutcome;

    const TIMEOUT: &str = "Timeout: request did not complete within requested timeout";
    const STORED: &str = "conversion webhook for closeout.k10s.test/v1, Kind=Widget failed: this webhook refuses every conversion to v2";
    let status = format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","message":"{TIMEOUT}","reason":"Timeout","code":504}}"#
    );
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    let pod = "/api/v1/namespaces/prod/pods/api-1?";
    route_reads(&script, 1);
    script.route("PATCH", pod, 504, status.clone());
    script.route("PATCH", pod, 504, status);
    script.route(
        "PATCH",
        pod,
        500,
        format!(r#"{{"kind":"Status","apiVersion":"v1","metadata":{{}},"status":"Failure","message":"{STORED}","code":500}}"#),
    );
    script.route_lost("PATCH", pod);
    script.route_unsent("PATCH", pod);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let apply = |dry_run| {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.apply(
            apply_request("kind: Pod\n", dry_run, false),
            move |outcome| {
                let _ = tx.send(outcome);
            },
        );
        wait(&rx)
    };
    assert_eq!(
        apply(true),
        ApplyOutcome::Failed {
            why: TIMEOUT.to_string()
        }
    );
    assert_eq!(
        apply(false),
        ApplyOutcome::Unconfirmed {
            why: TIMEOUT.to_string()
        }
    );
    assert_eq!(
        apply(false),
        ApplyOutcome::Unconfirmed {
            why: STORED.to_string()
        }
    );
    let ApplyOutcome::Unconfirmed { why } = apply(false) else {
        panic!("a real apply whose connection is lost may have landed");
    };
    assert!(why.contains("the scripted connection was lost"), "{why}");
    // A real apply whose credential failed never left the process: nothing
    // may have landed, and it is its own outcome.
    let ApplyOutcome::NotSent { why } = apply(false) else {
        panic!("a real apply that never left is not sent, not unconfirmed");
    };
    assert!(!why.is_empty(), "it carries its reason");

    let sent = patches(&script);
    assert_eq!(sent.len(), 5, "{sent:?}");
    for (seen, dry_run) in sent.iter().zip([true, false, false, false, false]) {
        assert_eq!(seen.method, "PATCH");
        assert_eq!(seen.path.contains("dryRun=All"), dry_run, "{}", seen.path);
        assert_eq!(seen.accept, "application/json");
        assert_eq!(seen.content_type, "application/apply-patch+yaml");
        assert_eq!(seen.body, "kind: Pod\n");
    }
    assert_reads_used(&script, 1);
    drop(runtime);
}

// A dry run is its read of the object and its PATCH, both on the interactive
// client: the read is the review's left side. A 404 is an object the apply
// would create, so the dry run still goes out; a read that is denied or gets
// no answer leaves nothing to compare with, and no PATCH is sent.
#[test]
fn a_dry_run_reads_the_object_with_it_and_sends_nothing_it_cannot_compare() {
    use k10s_data::apply::{ApplyOutcome, Live};

    let dry_run = |read: Option<(u16, String)>| {
        let script = Script::default();
        script_discovery(&script);
        script_rules_review(&script);
        script_access_reviews(&script, true, 32);
        script_lists(&script);
        match &read {
            Some((status, body)) => {
                script.route(
                    "GET",
                    "/api/v1/namespaces/prod/pods/api-1",
                    *status,
                    body.clone(),
                );
            }
            None => {
                script.route_lost("GET", "/api/v1/namespaces/prod/pods/api-1");
            }
        }
        script.route(
            "PATCH",
            "/api/v1/namespaces/prod/pods/api-1?",
            if matches!(read, Some((404, _))) {
                201
            } else {
                200
            },
            pod_json("api-1", "uid-pod-1", false),
        );
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader
            .apply(apply_request("kind: Pod\n", true, false), move |outcome| {
                let _ = tx.send(outcome);
            });
        let outcome = wait(&rx);
        let sent = script.requests_for("/pods/api-1");
        drop(runtime);
        (outcome, sent)
    };

    let (outcome, sent) = dry_run(Some((200, pod_json("api-1", "uid-pod-1", false))));
    let ApplyOutcome::DryRun(applied) = outcome else {
        panic!("the dry run resolves: {outcome:?}");
    };
    let Live::Read { yaml, uid } = applied.live else {
        panic!("a dry run carries its read: {:?}", applied.live);
    };
    assert!(yaml.starts_with("apiVersion: v1\nkind: Pod\n"), "{yaml}");
    assert_eq!(uid.as_deref(), Some("uid-pod-1"));
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0].method, "GET");
    assert_eq!(sent[0].path, "/api/v1/namespaces/prod/pods/api-1");
    assert_eq!(sent[0].accept, "");
    assert_eq!(sent[0].body, "");
    assert_eq!(sent[1].method, "PATCH");
    assert!(sent[1].path.contains("dryRun=All"), "{}", sent[1].path);

    let not_found = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"pods \"api-1\" not found","reason":"NotFound","code":404}"#;
    let (outcome, sent) = dry_run(Some((404, not_found.to_string())));
    assert!(
        matches!(&outcome, ApplyOutcome::DryRun(dry) if dry.live == Live::Absent),
        "{outcome:?}"
    );
    assert_eq!(
        sent.len(),
        2,
        "the dry run of a create still goes out: {sent:?}"
    );

    let forbidden = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"pods \"api-1\" is forbidden","reason":"Forbidden","code":403}"#;
    let (outcome, sent) = dry_run(Some((403, forbidden.to_string())));
    assert_eq!(
        outcome,
        ApplyOutcome::Denied {
            what: "read",
            why: "the dry run was not sent: pods \"api-1\" is forbidden".to_string(),
        }
    );
    assert_eq!(sent.len(), 1, "no PATCH after a denied read: {sent:?}");

    // A status on the read is the server's own sentence, not kube's rendering
    // of the whole Status.
    let unavailable = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"the server is currently unable to handle the request","reason":"ServiceUnavailable","code":503}"#;
    let (outcome, sent) = dry_run(Some((503, unavailable.to_string())));
    assert_eq!(
        outcome,
        ApplyOutcome::Failed {
            why: "the object could not be read, so the dry run was not sent: the server is \
                  currently unable to handle the request"
                .to_string(),
        }
    );
    assert_eq!(sent.len(), 1, "no PATCH after a failed read: {sent:?}");

    let (outcome, sent) = dry_run(None);
    let ApplyOutcome::Failed { why } = outcome else {
        panic!("a read with no answer fails the dry run: {outcome:?}");
    };
    assert!(
        why.starts_with("the object could not be read, so the dry run was not sent: "),
        "{why}"
    );
    assert_eq!(sent.len(), 1, "no PATCH after a lost read: {sent:?}");

    // A 401 is a denial like a 403, of the read, and an empty message names
    // its code.
    let unauthorized = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"","reason":"Unauthorized","code":401}"#;
    let (outcome, sent) = dry_run(Some((401, unauthorized.to_string())));
    assert_eq!(
        outcome,
        ApplyOutcome::Denied {
            what: "read",
            why: "the dry run was not sent: answered with status 401 and no message".to_string(),
        }
    );
    assert_eq!(
        sent.len(),
        1,
        "no PATCH after an unauthorised read: {sent:?}"
    );

    // An object the editor's emitter will not render (it caps nesting at 64
    // levels) leaves nothing to compare with, so the dry run is not sent.
    let deep = format!(
        r#"{{"apiVersion":"v1","kind":"Pod","metadata":{{"name":"api-1","namespace":"prod","uid":"uid-pod-1"}},"spec":{}1{}}}"#,
        r#"{"a":"#.repeat(70),
        "}".repeat(70)
    );
    let (outcome, sent) = dry_run(Some((200, deep)));
    let ApplyOutcome::Failed { why } = outcome else {
        panic!("an unrenderable read fails the dry run: {outcome:?}");
    };
    assert!(
        why.starts_with(
            "the object as it is now cannot be shown here, so the dry run was not sent: "
        ),
        "{why}"
    );
    assert_eq!(
        sent.len(),
        1,
        "no PATCH after an unrenderable read: {sent:?}"
    );
}

#[test]
fn a_dry_run_refuses_an_object_created_or_deleted_after_its_read() {
    use k10s_data::apply::ApplyOutcome;

    for (read_status, patch_status) in [(404, 200), (200, 201)] {
        for force in [false, true] {
            let script = Script::default();
            script_discovery(&script);
            script_rules_review(&script);
            script_access_reviews(&script, true, 32);
            script_lists(&script);
            let before = if read_status == 404 {
                r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"pods not found"}"#.to_string()
            } else {
                pod_json("api-1", "uid-pod-1", false)
            };
            script.route(
                "GET",
                "/api/v1/namespaces/prod/pods/api-1",
                read_status,
                before,
            );
            script.route(
                "PATCH",
                "/api/v1/namespaces/prod/pods/api-1?",
                patch_status,
                pod_json("api-1", "uid-new", false),
            );
            let runtime = runtime();
            let (sync, _live) = sync_on(&runtime, &script);
            let yaml = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: api-1\n";
            let (tx, rx) = std::sync::mpsc::channel();
            sync.reader
                .apply(apply_request(yaml, true, force), move |outcome| {
                    let _ = tx.send(outcome);
                });
            assert!(
                matches!(wait(&rx), ApplyOutcome::Failed { why } if why == "the object's existence changed between the read and the dry run; compare again")
            );
            let sent = script.requests_for("/pods/api-1");
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[0].method, "GET");
            assert_eq!(sent[0].path, "/api/v1/namespaces/prod/pods/api-1");
            assert_eq!(sent[0].accept, "");
            assert!(sent[0].body.is_empty());
            assert_eq!(sent[1].method, "PATCH");
            let forced = if force { "force=true&" } else { "" };
            assert_eq!(
                sent[1].path,
                format!(
                    "/api/v1/namespaces/prod/pods/api-1?&dryRun=All&{forced}fieldManager=k10s&fieldValidation=Strict"
                )
            );
            assert_eq!(sent[1].accept, "application/json");
            assert_eq!(sent[1].content_type, "application/apply-patch+yaml");
            assert_eq!(sent[1].body, yaml);
        }
    }
}
