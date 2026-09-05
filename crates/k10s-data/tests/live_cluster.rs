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
//! Note `--test-threads=1`, which is not optional: the field-manager conflict
//! test leaves a manager named `rival` owning `.data.retries` on the shared
//! ConfigMap, and in parallel that reaches the staleness test as a conflict
//! instead of as staleness.
//!
//! `live_fixtures.sh`, beside this file, creates everything below against
//! whatever `KUBECONFIG` names. Prefer it to following this list by hand -- the
//! list used to be shorter than the suite, and each thing missing from it fails
//! an assertion a long way from the fixture that was absent.
//!
//! In `K10S_LIVE_NAMESPACE` (default `g2`):
//!
//! - a ConfigMap `settings` from a *client-side* `kubectl apply`, so it carries a
//!   `last-applied-configuration` for the three-way base, labelled
//!   `team: platform` and holding a `greeting: hello` key -- the label proves
//!   that something which is not apply bookkeeping survives into the edited
//!   document, and the key is read back out of the base;
//! - a Deployment `web`;
//! - a Secret `api-token` whose value is `super-secret-value`, for the route
//!   where a value is in the object itself;
//! - a Secret `declared-token` from a client-side apply with
//!   `plaintext-in-the-annotation` as its value, for the subtler route: an
//!   annotation is `ObjectMeta`, so the declared value survives the
//!   metadata-only fetch that makes the first route safe;
//! - a CRD `widgets.k10s.test` with `additionalPrinterColumns`, and one
//!   `Widget` named `sprocket` with `size: 7` and `flavour: vanilla`;
//! - a Deployment `usage-probe` whose container declares requests (10m/16Mi)
//!   and limits (100m/64Mi), the four numbers the usage tests assert against;
//! - a ClusterRole `k10s-reader-nometrics` bound to ServiceAccount
//!   `nometrics` -- everything the reader may read except `metrics.k8s.io`,
//!   for the denial that must arrive as a label.
//!
//! `K10S_LIVE_READER_CONTEXT` (default `reader@k10s-lab`) must name a context
//! whose account can read and cannot patch. `K10S_LIVE_NOMETRICS_CONTEXT`
//! (default `nometrics@k10s-lab`) must name one wired to that ServiceAccount.
//!
//! Three of these tests need a cluster with a *kubelet*, and are skipped rather
//! than failed where there is none: a standalone API server can serve every
//! other row here, but port-forward, exec, and log follow terminate at a node.
//! Set `K10S_LIVE_KUBELET=1` when the cluster has one, and make sure the
//! `web` Deployment's pod is actually Running -- a Deployment that no kubelet
//! ever scheduled satisfies the other tests and none of those.
//!
//! The usage tests also need a kubelet, and split on one more axis:
//! `K10S_LIVE_METRICS_SERVER=1` runs the metrics-server row, `=0` runs the
//! kubelet-fallback row (kill metrics-server first -- on k3s,
//! `kubectl -n kube-system scale deploy/metrics-server --replicas=0` leaves
//! the APIService registered and unanswering, which is the 503 half of the
//! fallback decision; the scripted suite covers the 404 half), and unset
//! skips both. Run the suite once per side to close the matrix.
//!
//! Both client-side applies must stay client-side. Server-side apply writes no
//! `last-applied-configuration`, so a fixture created with `--server-side`
//! quietly turns two three-way comparisons into two-way ones that still pass.
//!
//! Helm, Argo, Flux, overlays, and day-2 live in `live_adapters.rs`. That file
//! is the other half of the write path: apply is a document, day-2 is a named
//! click, and this suite still deletes fixtures through kube so a failed
//! assertion cannot leave a half-applied day-2 behind its own cleanup.

use std::time::Duration;

use k10s_core::KindId;
use k10s_data::apply::{ApplyOutcome, ApplyRequest};
use k10s_data::describe::DescribeRequest;
use k10s_data::metrics::{
    Bytes, Millicores, UsageOutcome, UsageRequest, UsageSample, UsageSource, UsageStop, UsageTarget,
};
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
        kubeconfig: None,
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

fn table(reader: &Reader, kind: KindId) -> k10s_data::browse::TablePage {
    let (tx, rx) = std::sync::mpsc::channel();
    reader.fetch_table(kind, None, move |outcome| {
        let _ = tx.send(outcome);
    });
    match wait(&rx) {
        Fetched::Ok(page) => page,
        other => panic!("the table must resolve: {other:?}"),
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

// A test that needs an object gone without going through day-2's confirm gate
// asks kube directly. That keeps cleanup off the path under test: apply is the
// document write, day-2 is the named click, and this file is about apply.
fn delete_if_present(name: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let client = kube::Client::try_default()
            .await
            .expect("a client from KUBECONFIG");
        let request =
            kube::api::Request::new(format!("/api/v1/namespaces/{}/configmaps", namespace()))
                .delete(name, &kube::api::DeleteParams::default())
                .expect("a delete request");
        // A 404 is the state this asks for, so it is not a failure.
        let _ = client.request::<serde_json::Value>(request).await;
    });
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

// A server-side apply *creates* what is absent, so a press on a document whose
// object was deleted between the read and the press brings it back instead of
// failing -- `kubectl apply`'s behaviour, and not what the person pressing the
// key is thinking about. The uid is the whole evidence, and only a real server
// can produce this: nothing scripted mints one.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn an_apply_after_a_delete_creates_a_new_object_rather_than_updating_the_old_one() {
    let (_plane, sync) = connect(None);
    let configmaps = kind(&sync.reader, "configmaps");
    let name = "k10s-recreate-probe";
    let document = format!(
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {name}\n  namespace: {}\ndata:\n  probe: \"1\"\n",
        namespace()
    );

    // Nothing server-owned is in this document, so the prune is a no-op on it and
    // the bytes go as written.
    delete_if_present(name);
    let outcome = apply(
        &sync.reader,
        request(configmaps.id, name, document.clone(), false, false),
    );
    let ApplyOutcome::Applied(created) = outcome else {
        panic!("an apply of an absent object creates it: {outcome:?}");
    };
    let first = created.uid.expect("a stored object has a uid");
    assert_eq!(
        manifest(&sync.reader, configmaps.id, name).uid.as_deref(),
        Some(first.as_str()),
        "the read and the apply agree about which object this is, which is what \
         makes a disagreement mean something"
    );

    delete_if_present(name);
    let outcome = apply(
        &sync.reader,
        request(configmaps.id, name, document, false, false),
    );
    let ApplyOutcome::Applied(again) = outcome else {
        panic!("the apply recreates it rather than refusing: {outcome:?}");
    };
    let second = again.uid.expect("the new object has a uid too");
    assert_ne!(
        first, second,
        "the object was deleted, so this apply created a new one -- which is the \
         difference a review has to be able to state"
    );
    delete_if_present(name);
}

// A kind this binary has never heard of, which is the whole open-model claim.
//
// §5.1 says arbitrary kinds including CRDs flow through unchanged and §2 says the
// open model makes that true by construction, and both were argued from the
// shape of the code rather than from a server that had been asked. A CRD is the
// one case where every layer has to cooperate without a compiled-in name:
// discovery has to find it over `/apis`, the browser has to list it through
// server-side printing it cannot predict the columns of, and the write path has
// to prune and dry-run an object whose schema arrived at runtime.
//
// The printer columns are the sharp part. `additionalPrinterColumns` is the CRD
// author's choice, so a client that quietly fell back to its own idea of how to
// render a row would still produce a table -- just not this one. Asserting the
// CRD's own column names is what tells the two apart.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_custom_resource_is_discovered_listed_and_applied_like_any_other_kind() {
    let (_plane, sync) = connect(None);

    let widgets = kind(&sync.reader, "widgets.k10s.test");
    assert!(widgets.namespaced, "the fixture CRD is namespaced");
    assert!(
        widgets.patchable,
        "a served CRD takes a patch, so the write path applies to it"
    );

    let page = table(&sync.reader, widgets.id);
    let columns: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(
        columns.iter().any(|name| name.eq_ignore_ascii_case("size"))
            && columns
                .iter()
                .any(|name| name.eq_ignore_ascii_case("flavour")),
        "the CRD's own additionalPrinterColumns reach the browser, rather than a \
         fallback rendering that would also have produced a table: {columns:?}"
    );
    let row = page
        .rows
        .iter()
        .find(|row| row.name == "sprocket")
        .expect("the fixture widget is listed");
    assert!(
        row.cells.iter().any(|cell| cell == "7"),
        "and the row carries the value the column named: {:?}",
        row.cells
    );
    assert!(!row.uid.is_empty(), "a listed row is identified by uid");

    // The write path, on a kind whose schema this binary learned at runtime.
    let live = manifest(&sync.reader, widgets.id, "sprocket");
    assert!(
        live.yaml.contains("kind: Widget"),
        "the document is the custom kind:\n{}",
        live.yaml
    );
    let built = payload(&live.yaml, live.status_subresource);
    for owned in ["metadata.resourceVersion", "metadata.uid"] {
        assert!(
            built.pruned.iter().any(|field| field == owned),
            "the prune is kind-agnostic and must remove {owned}: {:?}",
            built.pruned
        );
    }
    let outcome = apply(
        &sync.reader,
        request(
            widgets.id,
            "sprocket",
            sent(&built).to_string(),
            true,
            false,
        ),
    );
    let ApplyOutcome::Applied(applied) = &outcome else {
        panic!("a dry run against a custom resource resolves: {outcome:?}");
    };
    assert!(applied.dry_run, "nothing was stored");
    assert!(
        applied.yaml.contains("flavour: vanilla"),
        "and the server's answer is the object it would keep:\n{}",
        applied.yaml
    );
}

// Whether this cluster has a node that can run a container. Port-forward and
// exec are the only two things here that need one, so they ask rather than
// assume: a standalone API server serves every other row in this file, and a
// suite that failed on it would be punishing the wrong setup.
fn has_kubelet() -> bool {
    std::env::var("K10S_LIVE_KUBELET").is_ok_and(|value| value == "1")
}

fn running_pod(reader: &Reader, prefix: &str) -> String {
    let pods = kind(reader, "pods");
    let page = table(reader, pods.id);
    page.rows
        .iter()
        .find(|row| row.name.starts_with(prefix))
        .map(|row| row.name.clone())
        .unwrap_or_else(|| panic!("a pod named {prefix}* is running"))
}

// Exec, against a kubelet, for the first time.
//
// This transport is a WebSocket upgrade, which is exactly what the scripted API
// server in `scripted_apiserver.rs` cannot script -- `tower` serves requests and
// responses, and an upgrade stops being either. So everything below the seam has
// been unproven since it was written: the terminal was tested against a fake
// transport and the transport against nothing.
//
// What is asserted is deliberately the round trip and not the grid. The bytes a
// remote shell sends back are the one thing no fake can stand in for; how they
// are laid out is `alacritty_terminal`'s business and is already tested without
// a cluster.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn exec_reaches_a_container_and_brings_its_output_back() {
    if !has_kubelet() {
        eprintln!("skipped: set K10S_LIVE_KUBELET=1 on a cluster with a node");
        return;
    }
    let (_plane, sync) = connect(None);
    let pod = running_pod(&sync.reader, "web-");

    let (tx, rx) = std::sync::mpsc::channel();
    let session = sync.reader.start_exec(
        &k10s_data::exec::ExecRequest {
            namespace: namespace(),
            pod,
            container: None,
            command: vec!["/bin/sh".to_string()],
        },
        Box::new(move |event| {
            let _ = tx.send(event);
        }),
    );

    // A marker rather than a prompt: a shell's prompt depends on the image, and
    // an assertion on it would be an assertion about nginx's base layer.
    session.write(b"echo k10s-exec-marker\n");

    let mut seen = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(k10s_data::exec::ExecEvent::Output(bytes)) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if seen.contains("k10s-exec-marker") {
                    break;
                }
            }
            Ok(other) => panic!("the session ended before it answered: {other:?}"),
            Err(_) => continue,
        }
    }
    assert!(
        seen.contains("k10s-exec-marker"),
        "the container never answered through the upgrade; saw {seen:?}"
    );
    drop(session);
}

// Log follow, against a kubelet, for the first time.
//
// The stream is an HTTP body the API server proxies from the node, not an
// upgrade, but a standalone API server still cannot stand in for it: there is
// no container and nothing writing. The scripted suite proves the client
// labels an end, a denial, and a cancel; this is the first time the bytes
// themselves came from a process a kubelet is running.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_log_follow_carries_lines_the_kubelet_already_has() {
    if !has_kubelet() {
        eprintln!("skipped: set K10S_LIVE_KUBELET=1 on a cluster with a node");
        return;
    }
    let (_plane, sync) = connect(None);
    let pod = running_pod(&sync.reader, "web-");
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.follow_log(
        k10s_data::logs::LogRequest {
            namespace: namespace(),
            pod,
            container: None,
            previous: false,
        },
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );

    let mut seen = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(k10s_data::logs::LogChunk::Lines(batch)) => {
                for line in &batch {
                    seen.push_str(line);
                    seen.push('\n');
                }
                if seen.contains("start worker") {
                    break;
                }
            }
            Ok(k10s_data::logs::LogChunk::Denied { what }) => {
                panic!("the follow was denied: {what}")
            }
            Ok(k10s_data::logs::LogChunk::Failed { why, .. }) => {
                panic!("the follow failed: {why}")
            }
            Ok(k10s_data::logs::LogChunk::Ended { .. }) => break,
            Err(_) => continue,
        }
    }
    drop(stop);
    assert!(
        seen.contains("start worker"),
        "the kubelet never handed the container's log stream; saw {seen:?}"
    );
}

// Port-forward, against a kubelet, for the first time.
//
// The same upgrade problem as exec, plus a listener: the forward opens a local
// socket and pumps it, so the only proof that works is to connect to that socket
// from outside the client and get the remote server's own bytes back. Anything
// short of that -- a row that says Active, a registry that lists it -- is the
// bookkeeping this repository already tests against a fake `Forwarder`.
#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_port_forward_carries_real_bytes_from_the_pod() {
    if !has_kubelet() {
        eprintln!("skipped: set K10S_LIVE_KUBELET=1 on a cluster with a node");
        return;
    }
    use std::io::{Read, Write};

    let (_plane, sync) = connect(None);
    // The high-port fixture, not `web`: k10s takes the local port from the
    // container's own, so a pod on 80 resolves to a local 80 no unprivileged
    // process may bind. That is a real limitation and it is recorded as one; it
    // is not what this test is about.
    let pod = running_pod(&sync.reader, "forward-probe-");

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.open_forward(
        k10s_data::forward::ForwardRequest {
            namespace: namespace(),
            name: pod.clone(),
            service: false,
        },
        move |fetched| {
            let _ = tx.send(fetched);
        },
    );
    let row = match wait(&rx) {
        Fetched::Ok(row) => row,
        other => panic!("the forward must open: {other:?}"),
    };
    assert_eq!(row.spec.pod, pod);
    assert_eq!(
        row.spec.remote_port, 18081,
        "the container declares one port and the spec took it"
    );

    // The listener is bound before the row is handed back, but the pump behind
    // it may still be connecting, so this retries rather than assuming.
    let address = format!("127.0.0.1:{}", row.spec.local_port);

    let mut answer = String::new();
    for attempt in 0..20 {
        std::thread::sleep(Duration::from_millis(250));
        let Ok(mut stream) = std::net::TcpStream::connect(&address) else {
            continue;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("a read timeout");
        if stream
            .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .is_err()
        {
            continue;
        }
        let mut buffer = Vec::new();
        let _ = stream.read_to_end(&mut buffer);
        answer = String::from_utf8_lossy(&buffer).to_string();
        if answer.contains("k10s-forward-probe") {
            break;
        }
        assert!(attempt < 19, "the forward never carried a reply");
    }
    assert!(
        answer.contains("k10s-forward-probe"),
        "the bytes are the ones that container serves and nothing else could \
         have: {answer:?}"
    );

    let listed = sync.reader.forwards().list();
    assert!(
        listed.iter().any(|open| open.id == row.id),
        "an open forward is listed while it is open"
    );
    assert!(sync.reader.forwards().close(row.id), "and closes by its id");
    assert!(
        !sync
            .reader
            .forwards()
            .list()
            .iter()
            .any(|open| open.id == row.id),
        "and is gone from the registry afterwards"
    );
}

// Which half of the usage matrix this cluster is: Some(true) has
// metrics-server, Some(false) had it killed, None skips both rows.
fn metrics_server() -> Option<bool> {
    std::env::var("K10S_LIVE_METRICS_SERVER")
        .ok()
        .map(|value| value == "1")
}

fn nometrics_context() -> String {
    std::env::var("K10S_LIVE_NOMETRICS_CONTEXT")
        .unwrap_or_else(|_| "nometrics@k10s-lab".to_string())
}

fn poll_usage(
    reader: &Reader,
    target: UsageTarget,
) -> (UsageStop, std::sync::mpsc::Receiver<UsageOutcome>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = reader.poll_usage(
        UsageRequest {
            namespace: namespace(),
            target,
            interval: Duration::from_secs(2),
        },
        Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }),
    );
    (stop, rx)
}

// Wait out the scrape lag: metrics-server takes tens of seconds to first
// serve a fresh pod, and the kubelet's counters advance on their own clock.
// Outcomes that do not match yet are kept for the panic message, because
// "which wrong thing kept arriving" is the diagnosis.
fn await_sample(
    rx: &std::sync::mpsc::Receiver<UsageOutcome>,
    budget: Duration,
    what: &str,
    accept: impl Fn(&UsageSample) -> bool,
) -> UsageSample {
    let deadline = std::time::Instant::now() + budget;
    let mut last: Option<UsageOutcome> = None;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(UsageOutcome::Usage(sample)) if accept(&sample) => return sample,
            Ok(outcome) => last = Some(outcome),
            Err(_) => {}
        }
    }
    panic!("{what} never arrived within the budget; last outcome: {last:?}");
}

// The four declared numbers on the usage-probe fixture, asserted exactly:
// they come from the pod spec, whichever source carried the usage.
fn assert_probe_bounds(sample: &UsageSample) {
    assert_eq!(sample.cpu_request, Some(Millicores(10)));
    assert_eq!(sample.cpu_limit, Some(Millicores(100)));
    assert_eq!(sample.memory_request, Some(Bytes(16 * 1024 * 1024)));
    assert_eq!(sample.memory_limit, Some(Bytes(64 * 1024 * 1024)));
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn pod_usage_renders_from_metrics_server_with_its_requests_and_limits() {
    if !has_kubelet() || metrics_server() != Some(true) {
        eprintln!(
            "skipped: set K10S_LIVE_KUBELET=1 and K10S_LIVE_METRICS_SERVER=1 \
             on a cluster that runs metrics-server"
        );
        return;
    }
    let (_plane, sync) = connect(None);
    let pod = running_pod(&sync.reader, "usage-probe-");

    let (_stop, rx) = poll_usage(&sync.reader, UsageTarget::Pod { name: pod });
    let sample = await_sample(
        &rx,
        Duration::from_secs(180),
        "a metrics-server sample",
        |sample| sample.source == UsageSource::MetricsServer && sample.memory.is_some(),
    );
    println!("live metrics-server sample: {sample:?}");
    assert!(
        sample.cpu.is_some(),
        "metrics-server reports both quantities for a running pod: {sample:?}"
    );
    assert_probe_bounds(&sample);
    assert_eq!((sample.pods_measured, sample.pods_total), (1, 1));
    assert!(!sample.truncated);

    // The same numbers through the workload door: the deployment's own
    // selector resolves the pod and the sums land on the same bounds.
    let deployments = kind(&sync.reader, "deployments.apps");
    let (_stop, rx) = poll_usage(
        &sync.reader,
        UsageTarget::Workload {
            kind: deployments.id,
            name: "usage-probe".to_string(),
        },
    );
    let sample = await_sample(
        &rx,
        Duration::from_secs(60),
        "a workload sample",
        |sample| sample.source == UsageSource::MetricsServer && sample.memory.is_some(),
    );
    println!("live workload sample: {sample:?}");
    assert_probe_bounds(&sample);
    assert_eq!((sample.pods_measured, sample.pods_total), (1, 1));
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn pod_usage_is_carried_by_the_kubelet_when_metrics_server_is_gone() {
    if !has_kubelet() || metrics_server() != Some(false) {
        eprintln!(
            "skipped: set K10S_LIVE_KUBELET=1 and K10S_LIVE_METRICS_SERVER=0 \
             on a cluster whose metrics-server was scaled away"
        );
        return;
    }
    let (_plane, sync) = connect(None);
    let pod = running_pod(&sync.reader, "usage-probe-");

    let (_stop, rx) = poll_usage(&sync.reader, UsageTarget::Pod { name: pod });
    let first = await_sample(
        &rx,
        Duration::from_secs(60),
        "a kubelet-carried sample",
        |sample| sample.source == UsageSource::Kubelet && sample.memory.is_some(),
    );
    println!("live kubelet sample (memory first): {first:?}");
    assert_probe_bounds(&first);

    // CPU needs the counter to advance under the kubelet's own timestamps;
    // one more scrape interval is enough, and the rate must arrive without
    // metrics-server ever answering.
    let with_rate = await_sample(
        &rx,
        Duration::from_secs(120),
        "a kubelet CPU rate",
        |sample| sample.source == UsageSource::Kubelet && sample.cpu.is_some(),
    );
    println!("live kubelet sample (with rate): {with_rate:?}");
    assert_probe_bounds(&with_rate);
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn denied_pod_metrics_is_a_labelled_denial_not_an_error_string() {
    if !has_kubelet() {
        eprintln!("skipped: set K10S_LIVE_KUBELET=1 on a cluster with a node");
        return;
    }
    let context = nometrics_context();
    let (_plane, sync) = connect(Some(&context));
    let pod = running_pod(&sync.reader, "usage-probe-");

    let (_stop, rx) = poll_usage(&sync.reader, UsageTarget::Pod { name: pod });
    let outcome = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the poll answers");
    assert_eq!(
        outcome,
        UsageOutcome::Denied {
            what: "pod metrics"
        },
        "a 403 on pod metrics is a labelled state, and the kubelet is not \
         asked to route around it"
    );
    // Denied ends the poll itself: the sender is gone, so the channel closes
    // instead of carrying a retry.
    assert!(
        rx.recv_timeout(Duration::from_secs(10)).is_err(),
        "a denial is not retried"
    );
}
