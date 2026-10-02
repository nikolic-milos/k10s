//! Helm releases read as release Secrets only, grouped newest first -- the one
//! place the Secret rule bends, held by what the list asks the server for --
//! and a stored revision applied back as the documents it holds, its Secret
//! documents skipped, with that write's whole request set pinned.

use crate::*;

// A Helm release payload as the release Secret stores it: gzip, then base64
// twice (once by Helm, once by the Secret's data encoding).
fn payload(name: &str, revision: u32, status: &str, manifest: &str) -> String {
    use base64::Engine;
    use std::io::Write;
    let json = serde_json::json!({
        "name": name,
        "namespace": "prod",
        "version": revision,
        "info": {
            "last_deployed": format!("2026-08-0{revision}T10:00:00Z"),
            "status": status,
            "description": "Upgrade complete",
            "notes": "NOTES-SECRET",
        },
        "chart": {
            "metadata": {
                "name": name,
                "version": format!("4.11.{revision}"),
                "appVersion": format!("1.11.{revision}"),
            },
            "values": {"password": "CHART-SECRET"},
        },
        "config": {"adminPassword": "USER-SECRET"},
        "manifest": manifest,
    })
    .to_string();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(json.as_bytes()).expect("gzip");
    let engine = base64::engine::general_purpose::STANDARD;
    engine.encode(engine.encode(encoder.finish().expect("gzip")))
}

fn release_secret(
    name: &str,
    release: &str,
    revision: u32,
    status: &str,
    manifest: &str,
) -> String {
    format!(
        r#"{{"metadata":{{"name":"{name}","namespace":"prod"}},
             "type":"helm.sh/release.v1",
             "data":{{"release":"{}"}}}}"#,
        payload(release, revision, status, manifest)
    )
}

const SECRET_MANIFEST: &str = "kind: Secret\ndata:\n  token: MANIFEST-SECRET\n";

// Helm's release state is one Secret per revision and nothing else, so an
// inventory is a list and a decode. What this pins is the half a scripted server
// can prove: that the *request* is narrowed to release Secrets on the wire, that
// paging is followed, that revisions group into releases newest first, and that
// a payload which will not decode is counted rather than dropped.
#[test]
fn helm_releases_are_listed_as_release_secrets_only_and_grouped_newest_first() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    // Namespaced on purpose. Routes match by path prefix and are single-shot,
    // and the initial sync lists every discovered kind -- secrets among them --
    // cluster-wide, so a route for the cluster-wide collection would answer the
    // sync and leave this fetch reading the page after it. The two pages below
    // share a prefix and are answered in registration order, which is the order
    // this fetch asks for them in.
    script.route(
        "GET",
        "/api/v1/namespaces/prod/secrets?",
        200,
        format!(
            r#"{{"kind":"SecretList","apiVersion":"v1",
                 "metadata":{{"continue":"page-2"}},
                 "items":[{},{}]}}"#,
            release_secret(
                "sh.helm.release.v1.ingress.v1",
                "ingress",
                1,
                "superseded",
                SECRET_MANIFEST
            ),
            release_secret(
                "sh.helm.release.v1.ingress.v2",
                "ingress",
                2,
                "deployed",
                SECRET_MANIFEST
            ),
        ),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/secrets?",
        200,
        format!(
            r#"{{"kind":"SecretList","apiVersion":"v1","metadata":{{}},
                 "items":[{},
                          {{"metadata":{{"name":"sh.helm.release.v1.broken.v1","namespace":"prod"}},
                            "type":"helm.sh/release.v1","data":{{"release":"not-a-payload"}}}},
                          {{"metadata":{{"name":"sh.helm.release.v1.empty.v1","namespace":"prod"}},
                            "type":"helm.sh/release.v1","data":{{}}}}]}}"#,
            release_secret(
                "sh.helm.release.v1.api.v7",
                "api",
                7,
                "failed",
                SECRET_MANIFEST
            ),
        ),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_releases(Some("prod".to_string()), move |fetched| {
            let _ = tx.send(fetched);
        });
    let outcome = wait(&rx);
    let Fetched::Ok(releases) = outcome else {
        panic!("the listing must resolve: {outcome:?}");
    };

    let names: Vec<(&str, &str)> = releases
        .releases
        .iter()
        .map(|release| (release.namespace.as_str(), release.name.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![("prod", "api"), ("prod", "ingress")],
        "one release per name, whatever its Secrets are called"
    );
    let ingress = &releases.releases[1];
    assert_eq!(
        ingress
            .revisions
            .iter()
            .map(|revision| revision.revision)
            .collect::<Vec<u32>>(),
        vec![2, 1],
        "newest first, so the running revision is the first element"
    );
    assert_eq!(
        ingress.current().expect("a current revision").status,
        "deployed"
    );
    assert_eq!(ingress.revisions[0].chart_version, "4.11.2");
    assert_eq!(
        releases.unreadable, 2,
        "a payload that will not decode and a Secret with none are counted, not dropped"
    );
    assert!(!releases.truncated);

    // The narrowing is on the wire, not in this process: no Secret that is not a
    // Helm release is ever transferred, which is what keeps this path inside the
    // rule that the read seam does not fetch Secret values.
    let lists = script.requests_for("/api/v1/namespaces/prod/secrets");
    assert_eq!(lists.len(), 2, "two pages, both asked for: {lists:?}");
    for request in &lists {
        assert!(
            request.path.contains("labelSelector=owner%3Dhelm"),
            "the label Helm writes: {}",
            request.path
        );
        assert!(
            request
                .path
                .contains("fieldSelector=type%3Dhelm.sh%2Frelease.v1"),
            "and its own Secret type: {}",
            request.path
        );
    }
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );

    // Nothing the payload carried beyond the inventory reaches a caller.
    let rendered = format!(
        "{releases:?}\n{}",
        k10s_data::helm::render(&releases).join("\n")
    );
    assert!(
        !rendered.contains("SECRET"),
        "the manifest, the values and the notes are dropped at the decode: {rendered}"
    );

    drop(runtime);
}

// The manifest Helm stores for the rollback-probe fixture chart, copied from
// `helm template` of it: a document per template, the Secret before the
// ConfigMap as Helm orders kinds. A change to the chart is made here too.
const ROLLBACK_MANIFEST: &str = "---\n# Source: rollback-probe/templates/secret.yaml\n\
    apiVersion: v1\nkind: Secret\nmetadata:\n  name: rollback-probe-token\ntype: Opaque\n\
    stringData:\n  canary: PLANTED-rollback-canary\n---\n\
    # Source: rollback-probe/templates/configmap.yaml\napiVersion: v1\nkind: ConfigMap\n\
    metadata:\n  name: rollback-probe\ndata:\n  proof: stored\n";

// A stored-manifest apply is a write, so its whole request set is pinned: one
// list for the revision and one server-side apply for each document that is
// not a Secret. The Secret must be patchable here, or it would be skipped as
// unpatchable and the secret-material rule would pass for the wrong reason.
#[test]
fn a_stored_manifest_apply_patches_each_document_and_skips_the_secret() {
    use k10s_data::apply::ApplyOutcome;
    use k10s_data::helm_reveal::DocumentRollback;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery_with_secret(&script, api_resource("Secret", "secrets", true));
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/secrets?",
        200,
        format!(
            r#"{{"kind":"SecretList","apiVersion":"v1","metadata":{{}},"items":[{}]}}"#,
            release_secret(
                "sh.helm.release.v1.rollback-probe.v1",
                "rollback-probe",
                1,
                "deployed",
                ROLLBACK_MANIFEST
            ),
        ),
    );
    script.route(
        "PATCH",
        "/api/v1/namespaces/prod/configmaps/rollback-probe?",
        200,
        r#"{"apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"rollback-probe","namespace":"prod","uid":"uid-cm"},
            "data":{"proof":"stored"}}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let before = script.seen().len();
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.rollback_helm_revision(
        Some("prod".to_string()),
        "rollback-probe".to_string(),
        1,
        move |fetched| {
            let _ = tx.send(fetched);
        },
    );
    let outcome = wait(&rx);
    let Fetched::Ok(report) = outcome else {
        panic!("the rollback must resolve: {outcome:?}");
    };

    // The sync's watches are opened as its lists settle and may still be
    // arriving; they stay open and are not this write's requests.
    let sent: Vec<_> = script.seen()[before..]
        .iter()
        .filter(|seen| !seen.path.contains("watch=true"))
        .cloned()
        .collect();
    assert_eq!(sent.len(), 2, "one list and one apply: {sent:?}");
    assert_eq!(sent[0].method, "GET");
    assert_eq!(
        sent[0].path,
        "/api/v1/namespaces/prod/secrets?&fieldSelector=type%3Dhelm.sh%2Frelease.v1\
         &labelSelector=owner%3Dhelm%2Cname%3Drollback-probe%2Cversion%3D1&limit=200"
    );
    assert_eq!(sent[0].accept, "", "kube's list sets no Accept header");
    assert_eq!(sent[1].method, "PATCH");
    assert_eq!(
        sent[1].path,
        "/api/v1/namespaces/prod/configmaps/rollback-probe?&fieldManager=k10s&fieldValidation=Strict"
    );
    assert_eq!(sent[1].accept, "application/json");
    assert_eq!(sent[1].content_type, "application/apply-patch+yaml");
    assert_eq!(
        sent[1].body,
        "# Source: rollback-probe/templates/configmap.yaml\napiVersion: v1\nkind: ConfigMap\n\
         metadata:\n  name: rollback-probe\ndata:\n  proof: stored\n"
    );

    assert_eq!(report.documents.len(), 2, "{report:?}");
    let DocumentRollback::Skipped { kind, name, why } = &report.documents[0] else {
        panic!("the Secret is skipped: {report:?}");
    };
    assert_eq!(
        (kind.as_str(), name.as_str()),
        ("Secret", "rollback-probe-token")
    );
    assert!(
        why.starts_with("a stored Secret document is not applied"),
        "{why}"
    );
    let DocumentRollback::Applied {
        kind,
        name,
        outcome,
    } = &report.documents[1]
    else {
        panic!("the ConfigMap is applied: {report:?}");
    };
    assert_eq!(
        (kind.as_str(), name.as_str()),
        ("ConfigMap", "rollback-probe")
    );
    assert!(matches!(outcome, ApplyOutcome::Applied(applied) if !applied.dry_run));
    assert!(
        !format!("{report:?}").contains("PLANTED-rollback-canary"),
        "a skipped Secret's values never reach the report"
    );

    drop(runtime);
}

// The stored-manifest apply, its reveal included, takes the interactive
// client: the reveal decides what is written.
#[test]
fn a_stored_manifest_apply_takes_the_interactive_client_with_its_reveal() {
    use k10s_data::read::Fetched;

    let sync = Script::default();
    script_discovery_with_secret(&sync, api_resource("Secret", "secrets", true));
    script_rules_review(&sync);
    script_access_reviews(&sync, true, 32);
    script_lists(&sync);
    let interactive = Script::default();
    interactive.route(
        "GET",
        "/api/v1/namespaces/prod/secrets?",
        200,
        format!(
            r#"{{"kind":"SecretList","apiVersion":"v1","metadata":{{}},"items":[{}]}}"#,
            release_secret(
                "sh.helm.release.v1.rollback-probe.v1",
                "rollback-probe",
                1,
                "deployed",
                ROLLBACK_MANIFEST
            ),
        ),
    );
    interactive.route(
        "PATCH",
        "/api/v1/namespaces/prod/configmaps/rollback-probe?",
        200,
        r#"{"apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"rollback-probe","namespace":"prod","uid":"uid-cm"},
            "data":{"proof":"stored"}}"#,
    );

    let runtime = runtime();
    let (data, _live) = sync_on_split(&runtime, &sync, &interactive);
    let before = sync.seen().len();
    let (tx, rx) = std::sync::mpsc::channel();
    data.reader.rollback_helm_revision(
        Some("prod".to_string()),
        "rollback-probe".to_string(),
        1,
        move |fetched| {
            let _ = tx.send(fetched);
        },
    );
    let outcome = wait(&rx);
    assert!(matches!(outcome, Fetched::Ok(_)), "{outcome:?}");
    let sent: Vec<_> = interactive
        .seen()
        .into_iter()
        .map(|seen| seen.method)
        .collect();
    assert_eq!(sent, ["GET", "PATCH"]);
    let stray = after_sync(&sync, before);
    assert!(stray.is_empty(), "nothing on the sync client: {stray:?}");
    drop(runtime);
}
