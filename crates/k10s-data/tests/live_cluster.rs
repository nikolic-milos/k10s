//! The live half of §6.7's matrix, for the write path.
//!
//! Everything in `scripted_apiserver.rs` is proven against a `tower` service
//! that *is* the API server, and that server is honest about what it cannot be:
//! it answers with whatever a test scripted, so it can prove that a dry-run
//! apply sends the right method, media type, query and bytes -- and it cannot
//! prove that a real API server *does* with those bytes what the code says it
//! does. Two claims in `apply.rs` depend entirely on that difference:
//!
//! 1. that `metadata.resourceVersion` left in an apply body is an
//!    optimistic-lock precondition, which is why the payload prune removes it;
//! 2. that a field-manager conflict names the manager in the first quoted run
//!    of its cause message, which is where `manager_of` reads it from.
//!
//! Both are checked here against a real one, along with the shape of a refusal
//! of the document itself and an RBAC denial on a write. Two of these tests
//! exist because running them was the only way to learn what they now assert: a
//! stale resourceVersion comes back as a 409 with **no** causes, which is a
//! different state from a field-manager conflict and cannot be forced; and a
//! misspelled field in an apply is refused by the field manager with a **500**
//! naming it, not by strict validation with a 400.
//!
//! Ignored by default: the unit suites keep the no-network discipline and this
//! one is a network test. It needs a cluster and a kubeconfig naming two
//! identities, one of which may not patch:
//!
//! ```text
//! KUBECONFIG=/path/to/kubeconfig cargo test -p k10s-data --test live_cluster -- --ignored --nocapture
//! ```
//!
//! `K10S_LIVE_NAMESPACE` (default `g2`) must already hold a ConfigMap named
//! `settings` created by a *client-side* `kubectl apply` -- so that it carries a
//! `last-applied-configuration` for the three-way base -- and a Deployment named
//! `web`. `K10S_LIVE_READER_CONTEXT` (default `reader@k10s-lab`) must name a
//! context whose account can read and cannot patch.

use std::time::Duration;

use k10s_core::KindId;
use k10s_data::apply::{ApplyOutcome, ApplyRequest};
use k10s_data::describe::DescribeRequest;
use k10s_data::read::{Fetched, KindRow, Reader};
use k10s_data::{DEFAULT_EVENT_SINK_CAPACITY, Options, Sync};

fn namespace() -> String {
    std::env::var("K10S_LIVE_NAMESPACE").unwrap_or_else(|_| "g2".to_string())
}

fn reader_context() -> String {
    std::env::var("K10S_LIVE_READER_CONTEXT").unwrap_or_else(|_| "reader@k10s-lab".to_string())
}

// The production cold start, not a shortcut around it: this is the same call the
// binary makes, so connect, discovery and the RBAC probe are under test too.
fn connect(context: Option<&str>) -> (k10s_data::DataPlane, Sync) {
    assert!(
        std::env::var_os("KUBECONFIG").is_some(),
        "this test needs KUBECONFIG to name a real cluster; see the module comment"
    );
    let (sink, _drain) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
    // The drain is dropped, so the bounded sink fills and backpressures; the
    // write path does not read it and the sync only needs it to exist.
    std::mem::forget(_drain);
    let plane = k10s_data::spawn(sink).expect("a runtime");
    let options = Options {
        context: context.map(str::to_string),
        probe_namespaces: vec![namespace()],
        sync_timeout: Duration::from_secs(30),
    };
    let sync = plane.sync(&options).expect("the live cluster syncs");
    (plane, sync)
}

fn kind(reader: &Reader, display: &str) -> KindRow {
    reader
        .kinds()
        .into_iter()
        .find(|row| row.display == display)
        .unwrap_or_else(|| panic!("the cluster serves {display}"))
}

fn wait<T: Send + 'static>(rx: &std::sync::mpsc::Receiver<T>) -> T {
    rx.recv_timeout(Duration::from_secs(30))
        .expect("a reply within the budget")
}

fn manifest(reader: &Reader, kind: KindId, name: &str) -> k10s_data::manifest::Manifest {
    let (tx, rx) = std::sync::mpsc::channel();
    reader.fetch_manifest(
        DescribeRequest {
            kind,
            namespace: Some(namespace()),
            name: name.to_string(),
            uid: String::new(),
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    match wait(&rx) {
        Fetched::Ok(manifest) => manifest,
        other => panic!("{name} must resolve: {other:?}"),
    }
}

fn apply(reader: &Reader, request: ApplyRequest) -> ApplyOutcome {
    let (tx, rx) = std::sync::mpsc::channel();
    reader.apply(request, move |outcome| {
        let _ = tx.send(outcome);
    });
    wait(&rx)
}

fn request(kind: KindId, name: &str, yaml: String, dry_run: bool, force: bool) -> ApplyRequest {
    ApplyRequest {
        kind,
        namespace: Some(namespace()),
        name: name.to_string(),
        yaml,
        dry_run,
        force,
    }
}

// The payload the editor would send, built by the same pure prune the shell
// uses, so what this test puts on the wire is what a person would.
fn payload(yaml: &str, status_subresource: bool) -> k10s_edit::Payload {
    let rope = k10s_edit::Rope::from(yaml);
    let mut syntax = k10s_edit::Syntax::yaml();
    syntax.reparse(&rope);
    k10s_edit::apply::payload(&rope, &syntax, 0, status_subresource)
}

// The bytes are only reachable through `sendable`, which is the point: a
// payload the prune refused hands back its reasons instead, and nothing in this
// file may put those bytes on a real cluster.
fn sent(payload: &k10s_edit::Payload) -> &str {
    payload
        .sendable()
        .expect("these fixtures prune cleanly; a blocked payload is never sent")
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_client_side_applied_object_yields_a_three_way_base_and_a_sendable_payload() {
    let (_plane, sync) = connect(None);
    let configmaps = kind(&sync.reader, "configmaps");
    assert!(
        configmaps.patchable,
        "a real server serves configmaps with a patch verb"
    );

    let live = manifest(&sync.reader, configmaps.id, "settings");
    assert!(
        !live.yaml.contains("last-applied-configuration"),
        "apply bookkeeping is not part of the document being edited:\n{}",
        live.yaml
    );
    assert!(
        !live.yaml.contains("managedFields"),
        "nor is the field-manager ledger:\n{}",
        live.yaml
    );
    assert!(
        live.yaml.contains("team: platform"),
        "an annotation or label that is not bookkeeping stays:\n{}",
        live.yaml
    );
    assert!(
        !live.status_subresource,
        "a ConfigMap has no status subresource, so an apply may carry status"
    );

    let base = live
        .last_applied
        .clone()
        .expect("the fixture was created by a client-side apply, so the annotation is there");
    assert!(
        base.starts_with("apiVersion: v1\nkind: ConfigMap\n"),
        "the base renders through the same emitter as the live object:\n{base}"
    );
    assert!(base.contains("greeting: hello"), "{base}");

    // The three-way diff of an untouched buffer against its own live object is
    // empty, whatever the base says: that is the property that makes "no
    // differences" trustworthy on a real object rather than a fixture.
    let unchanged = k10s_edit::three_way(k10s_edit::Sides {
        base: Some(&base),
        live: &live.yaml,
        buffer: &live.yaml,
    });
    assert_eq!(
        unchanged.verdict(),
        k10s_edit::Verdict::Agreed,
        "an unedited buffer differs from live nowhere; counts were {:?}",
        unchanged.counts
    );
    assert!(!unchanged.two_way, "a base was found, so this is three-way");

    let built = payload(&live.yaml, live.status_subresource);
    for owned in ["metadata.resourceVersion", "metadata.uid"] {
        assert!(
            built.pruned.iter().any(|field| field == owned),
            "a live object carries {owned} and the prune must remove it: {:?}",
            built.pruned
        );
    }
    assert!(
        built.kept.is_empty(),
        "nothing was left behind unremovable: {:?}",
        built.kept
    );

    let outcome = apply(
        &sync.reader,
        request(
            configmaps.id,
            "settings",
            sent(&built).to_string(),
            true,
            false,
        ),
    );
    let ApplyOutcome::Applied(applied) = outcome else {
        panic!("a real server accepts the pruned payload as a dry run: {outcome:?}");
    };
    assert!(applied.dry_run);
    assert!(
        applied.yaml.contains("greeting: hello"),
        "the server echoes what it would store:\n{}",
        applied.yaml
    );
}

// The claim the scripted server cannot check: the prune removes
// `metadata.resourceVersion` because leaving it in makes every apply an
// optimistic-lock precondition. If a real server ignores it instead, the doc
// comment in `apply.rs` is wrong and this test says so.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_stale_resource_version_in_the_body_is_a_precondition_the_prune_exists_to_avoid() {
    let (_plane, sync) = connect(None);
    let configmaps = kind(&sync.reader, "configmaps");
    let live = manifest(&sync.reader, configmaps.id, "settings");

    let unpruned = live.yaml.clone();
    assert!(
        unpruned.contains("resourceVersion:"),
        "the fetched document carries one"
    );
    let stale = unpruned.replace(&resource_version(&unpruned), "1");
    assert_ne!(stale, unpruned, "the version was replaced with a stale one");

    let outcome = apply(
        &sync.reader,
        request(configmaps.id, "settings", stale, true, false),
    );
    let ApplyOutcome::Stale { message } = &outcome else {
        panic!(
            "the prune's stated reason requires a real server to refuse a stale \
             resourceVersion, and to refuse it as staleness rather than as a \
             forceable conflict; it answered {outcome:?} instead, so the comment in \
             k10s-data/src/apply.rs is wrong and must be corrected"
        );
    };
    println!("a stale resourceVersion in an apply body is refused: {message}");
    assert!(
        message.contains("has been modified"),
        "and says why in the server's own words: {message}"
    );

    // And the pruned payload of the very same object goes through, which is what
    // makes the prune the fix rather than a workaround.
    let built = payload(&live.yaml, live.status_subresource);
    let outcome = apply(
        &sync.reader,
        request(
            configmaps.id,
            "settings",
            sent(&built).to_string(),
            true,
            false,
        ),
    );
    assert!(
        matches!(outcome, ApplyOutcome::Applied(_)),
        "the pruned payload is accepted: {outcome:?}"
    );
}

fn resource_version(yaml: &str) -> String {
    yaml.lines()
        .find_map(|line| line.trim().strip_prefix("resourceVersion: "))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("the document carries a resourceVersion")
}

// A real field-manager conflict, produced by a second manager taking a field,
// and the message shape `manager_of` reads the manager out of.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_second_field_manager_produces_a_conflict_naming_it_and_force_takes_the_field() {
    let (plane, sync) = connect(None);
    let configmaps = kind(&sync.reader, "configmaps");

    // k10s takes ownership of the field first, by applying it as it stands.
    let live = manifest(&sync.reader, configmaps.id, "settings");
    let built = payload(&live.yaml, live.status_subresource);
    let outcome = apply(
        &sync.reader,
        request(
            configmaps.id,
            "settings",
            sent(&built).to_string(),
            false,
            false,
        ),
    );
    assert!(
        matches!(outcome, ApplyOutcome::Applied(_)),
        "the first apply establishes k10s as a manager: {outcome:?}"
    );

    // A rival manager takes the same field, forcing it away from k10s.
    rival_apply(&plane, &namespace(), "9");

    let live = manifest(&sync.reader, configmaps.id, "settings");
    let contested = live.yaml.replace("retries: \"9\"", "retries: \"5\"");
    assert_ne!(
        contested, live.yaml,
        "the buffer changes the contested field"
    );
    let built = payload(&contested, live.status_subresource);
    let outcome = apply(
        &sync.reader,
        request(
            configmaps.id,
            "settings",
            sent(&built).to_string(),
            false,
            false,
        ),
    );
    let ApplyOutcome::Conflict {
        message, causes, ..
    } = &outcome
    else {
        panic!("a contested field is a conflict, not {outcome:?}");
    };
    println!("live conflict message: {message}");
    println!("live conflict causes:  {causes:?}");
    assert!(!causes.is_empty(), "the server named its causes");
    assert!(
        causes.iter().any(|cause| cause.field.contains("retries")),
        "a cause names the contested field: {causes:?}"
    );
    assert!(
        causes.iter().any(|cause| cause.manager == "rival"),
        "and the manager holding it, parsed out of the message: {causes:?}"
    );

    // Forcing takes it, which is the only thing that gets past a conflict.
    let outcome = apply(
        &sync.reader,
        request(
            configmaps.id,
            "settings",
            sent(&built).to_string(),
            false,
            true,
        ),
    );
    assert!(
        matches!(outcome, ApplyOutcome::Applied(_)),
        "force takes the field: {outcome:?}"
    );
    let after = manifest(&sync.reader, configmaps.id, "settings");
    assert!(
        after.yaml.contains("retries: \"5\""),
        "and the value is the one that was applied:\n{}",
        after.yaml
    );
}

// A second field manager, spelled out here because `FIELD_MANAGER` is a
// constant: the point is that the name on the wire is not k10s.
fn rival_apply(plane: &k10s_data::DataPlane, namespace: &str, retries: &str) {
    let body = format!(
        "{{\"apiVersion\":\"v1\",\"kind\":\"ConfigMap\",\"metadata\":{{\"name\":\"settings\"}},\"data\":{{\"retries\":\"{retries}\"}}}}"
    );
    let path =
        format!("/api/v1/namespaces/{namespace}/configmaps/settings?fieldManager=rival&force=true");
    plane.runtime().block_on(async move {
        let client = kube::Client::try_default()
            .await
            .expect("a client from KUBECONFIG");
        let request = http::Request::patch(path)
            .header(http::header::CONTENT_TYPE, "application/apply-patch+yaml")
            .header(http::header::ACCEPT, "application/json")
            .body(body.into_bytes())
            .expect("a request");
        client
            .request::<serde_json::Value>(request)
            .await
            .expect("the rival apply lands");
    });
}

// Strict field validation, which the code asks for so a misspelling is a
// labelled refusal rather than a field silently dropped from a manifest someone
// believes they applied.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn an_unknown_field_is_refused_by_name_rather_than_dropped() {
    // Not by strict validation, and not with a 4xx: the field manager cannot
    // build a typed patch and answers 500. What has to hold is that the field is
    // named in a sentence a person can act on, rather than dropped silently or
    // buried inside a flattened error chain.
    let (_plane, sync) = connect(None);
    let configmaps = kind(&sync.reader, "configmaps");
    let live = manifest(&sync.reader, configmaps.id, "settings");
    let built = payload(&live.yaml, live.status_subresource);
    let misspelled = format!("{}dataz:\n  oops: yes\n", sent(&built));

    let outcome = apply(
        &sync.reader,
        request(configmaps.id, "settings", misspelled, true, false),
    );
    let (labelled, detail) = match &outcome {
        ApplyOutcome::Rejected { message, causes } => (message.clone(), causes.join("; ")),
        ApplyOutcome::Failed { why, .. } => (why.clone(), String::new()),
        other => {
            panic!("an unknown field must be refused, never dropped; the server answered {other:?}")
        }
    };
    println!("live refusal of an unknown field: {labelled} {detail}");
    assert!(
        labelled.contains("dataz") || detail.contains("dataz"),
        "the refusal names the field: {labelled} {detail}"
    );
    assert!(
        !labelled.starts_with("ApiError"),
        "and it is the server's own sentence, not a flattened error chain: {labelled}"
    );
}

// The hard rule, against real data rather than a fixture. A Secret written by
// `kubectl apply` carries its own declared values inside
// `metadata.annotations` -- and annotations are part of `ObjectMeta`, so they
// come back from the metadata-only fetch that keeps a Secret's `data` out of the
// read path. The annotation is what a three-way diff uses as its base document,
// which is how a plaintext value reaches a panel.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_secrets_values_never_reach_a_document_by_any_of_the_three_routes() {
    let (_plane, sync) = connect(None);
    let secrets = kind(&sync.reader, "secrets");

    // Route one: the object itself.
    let plain = manifest(&sync.reader, secrets.id, "api-token");
    assert!(
        !plain.yaml.contains("super-secret-value")
            && !plain.yaml.contains("c3VwZXItc2VjcmV0LXZhbHVl"),
        "no value in the document:\n{}",
        plain.yaml
    );

    // Route two: the annotation the fetch cannot avoid retrieving.
    let declared = manifest(&sync.reader, secrets.id, "declared-token");
    assert!(
        !declared.yaml.contains("plaintext-in-the-annotation"),
        "no value in the object:\n{}",
        declared.yaml
    );
    let base = declared
        .last_applied
        .clone()
        .expect("the fixture was client-side applied, so it has a base");
    assert!(
        !base.contains("plaintext-in-the-annotation"),
        "and none in the base document a diff renders:\n{base}"
    );
    assert!(
        base.contains("name: declared-token"),
        "while the base is still a document worth diffing:\n{base}"
    );

    // Route three: the object the server echoes back from an apply, which is a
    // full object and not a metadata projection.
    if secrets.patchable {
        let built = payload(&declared.yaml, declared.status_subresource);
        let outcome = apply(
            &sync.reader,
            request(
                secrets.id,
                "declared-token",
                sent(&built).to_string(),
                true,
                false,
            ),
        );
        let ApplyOutcome::Applied(applied) = &outcome else {
            panic!("the dry run resolves: {outcome:?}");
        };
        assert!(
            !applied.yaml.contains("plaintext-in-the-annotation"),
            "and none in what the server would store:\n{}",
            applied.yaml
        );
        assert!(
            applied.yaml.starts_with("# values withheld"),
            "the note that says so leads it:\n{}",
            applied.yaml
        );
    }
}

// The write half of the RBAC story: an account that may read and may not patch.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn an_account_without_the_patch_verb_gets_a_labelled_denial_not_an_error_string() {
    let context = reader_context();
    let (_plane, sync) = connect(Some(&context));
    let configmaps = kind(&sync.reader, "configmaps");
    assert!(
        configmaps.patchable,
        "the server still serves the verb; this account just may not use it, \
         and those are different labelled states"
    );

    let live = manifest(&sync.reader, configmaps.id, "settings");
    let built = payload(&live.yaml, live.status_subresource);
    let outcome = apply(
        &sync.reader,
        request(
            configmaps.id,
            "settings",
            sent(&built).to_string(),
            true,
            false,
        ),
    );
    let ApplyOutcome::Denied { what, why } = &outcome else {
        panic!("a 403 on a write is a denial, never a flattened error string: {outcome:?}");
    };
    assert_eq!(*what, "apply");
    println!("live write denial: {why}");
    assert!(
        why.contains("cannot patch") || why.contains("forbidden"),
        "and it carries the server's own explanation: {why}"
    );
}

// A kind that has a status subresource, so the prune takes status out; the
// server accepting the result is what says the prune did not remove intent.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_kind_with_a_status_subresource_applies_without_its_status() {
    let (_plane, sync) = connect(None);
    let deployments = kind(&sync.reader, "deployments.apps");
    let live = manifest(&sync.reader, deployments.id, "web");
    assert!(
        live.status_subresource,
        "discovery reports a status subresource for Deployment"
    );
    assert!(
        live.yaml.contains("status:"),
        "and the fetched document carries one:\n{}",
        live.yaml
    );

    let built = payload(&live.yaml, live.status_subresource);
    assert!(
        built.pruned.iter().any(|field| field == "status"),
        "which the prune removes: {:?}",
        built.pruned
    );
    assert!(
        !sent(&built).contains("\nstatus:"),
        "so the payload has no status block:\n{}",
        sent(&built)
    );

    let outcome = apply(
        &sync.reader,
        request(deployments.id, "web", sent(&built).to_string(), true, false),
    );
    let ApplyOutcome::Applied(applied) = outcome else {
        panic!("the server accepts a Deployment applied without status: {outcome:?}");
    };
    assert!(applied.dry_run);
    assert!(
        applied.yaml.contains("image: nginx:1.27"),
        "and the spec survives:\n{}",
        applied.yaml
    );
}
