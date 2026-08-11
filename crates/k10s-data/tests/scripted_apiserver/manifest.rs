//! The write path against the scripted server: an editable manifest from one
//! get, the last-applied annotation as a diff base, and the dry run and apply
//! that follow -- conflicts named per field and manager, a strict rejection
//! naming the field the server would have dropped, and a denial that never
//! reaches the server at all.

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
    let ApplyOutcome::Applied(applied) = wait(&rx) else {
        panic!("the dry run must resolve");
    };
    assert!(applied.dry_run);
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

    let writes = script.requests_for("/pods/api-1?");
    assert_eq!(writes.len(), 1, "one apply is one request: {writes:?}");
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

    drop(runtime);
}
#[test]
fn a_conflict_names_every_field_and_its_manager_and_only_forcing_asks_for_them() {
    use k10s_data::apply::ApplyOutcome;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    // Routes are single-shot in registration order, so the first apply
    // conflicts and the forced one succeeds -- the sequence a person walks
    // through.
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
    script.route(
        "PATCH",
        "/api/v1/namespaces/prod/pods/api-1?",
        200,
        pod_json("api-1", "uid-pod-1", false),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let sent = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: api-1\n";
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request(sent, false, false), move |outcome| {
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

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request(sent, false, true), move |outcome| {
            let _ = tx.send(outcome);
        });
    assert!(
        matches!(wait(&rx), ApplyOutcome::Applied(applied) if !applied.dry_run),
        "forcing takes the fields and stores the object"
    );

    let writes = script.requests_for("/pods/api-1?");
    assert_eq!(writes.len(), 2);
    assert!(!writes[0].path.contains("force"));
    assert!(
        writes[1].path.contains("force=true"),
        "only the second one forces: {}",
        writes[1].path
    );
    assert!(
        !writes[1].path.contains("dryRun"),
        "and it is not a dry run: {}",
        writes[1].path
    );

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
    script.route("PATCH", "/api/v1/namespaces/prod/pods/api-1?", 200, &body);
    script.route("PATCH", "/api/v1/namespaces/prod/pods/api-1?", 200, &body);

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request("kind: Pod\n", false, false), move |outcome| {
            let _ = tx.send(outcome);
        });
    let ApplyOutcome::Unrendered(unrendered) = wait(&rx) else {
        panic!("the object is stored; only the picture of it is missing");
    };
    assert!(!unrendered.dry_run);
    assert!(
        unrendered.why.contains("nests deeper"),
        "and it says which cap it hit: {}",
        unrendered.why
    );

    // The same answer to a dry run wrote nothing, and the state says so by
    // carrying the flag rather than by being a different variant.
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .apply(apply_request("kind: Pod\n", true, false), move |outcome| {
            let _ = tx.send(outcome);
        });
    let ApplyOutcome::Unrendered(unrendered) = wait(&rx) else {
        panic!("a dry run whose answer will not render is the same state");
    };
    assert!(unrendered.dry_run);

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
