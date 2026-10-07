//! Strict Helm metadata requests and payload actions that send nothing.

use crate::*;
use k10s_data::integrations::{helm, helm_reveal};
use k10s_data::read::{Fetched, Reader};

const LIST: &str = "/api/v1/namespaces/prod/secrets?&fieldSelector=type%3Dhelm.sh%2Frelease.v1&labelSelector=owner%3Dhelm&limit=200";
const ACCEPT: &str = "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1";

async fn connected(script: &Script) -> (Sync, EventReceiver) {
    connected_with(script, script.client()).await
}

async fn connected_with(script: &Script, client: kube::Client) -> (Sync, EventReceiver) {
    script_discovery_with_secret(script, api_resource("Secret", "secrets", true));
    script_rules_review(script);
    script_access_reviews(script, true, 32);
    script_lists(script);
    let (tx, rx) = crossbeam_channel::unbounded();
    let sync = sync_from(
        Clients {
            sync: client.clone(),
            interactive: client,
        },
        "prod",
        &options(),
        tx,
        Arc::new(IngestMetrics::default()),
    )
    .await
    .expect("scripted cluster sync");
    (sync, rx)
}

fn revision(
    secret: &str,
    namespace: &str,
    release: &str,
    version: &str,
    status: &str,
) -> serde_json::Value {
    serde_json::json!({
        "apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadata",
        "metadata":{"name":secret,"namespace":namespace,"uid":format!("uid-{secret}"),
            "labels":{"owner":"helm","name":release,"version":version,"status":status},
            "annotations":{"ignored":"not retained"}}
    })
}

fn page(items: Vec<serde_json::Value>, token: &str) -> String {
    serde_json::json!({"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList",
        "metadata":{"continue":token},"items":items})
    .to_string()
}

fn route(script: &Script, path: &str, status: u16, body: impl Into<String>) {
    script.route_accepting("GET", &path.replacen("?&", "?", 1), ACCEPT, status, body);
}

fn assert_lists(rows: &[Seen], paths: &[&str]) {
    assert_eq!(rows.len(), paths.len(), "{rows:?}");
    for (row, path) in rows.iter().zip(paths) {
        assert_eq!(row.method, "GET");
        assert_eq!(row.path, *path);
        assert_eq!(row.accept, ACCEPT);
        assert_eq!(row.content_type, "application/json");
        assert!(row.body.is_empty(), "{row:?}");
    }
}

async fn fetch(reader: &Reader, namespace: Option<&str>) -> Fetched<helm::Releases> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    reader.fetch_releases(namespace.map(str::to_string), move |answer| {
        let _ = tx.send(answer);
    });
    rx.await.expect("inventory callback")
}

fn failed(answer: Fetched<helm::Releases>) -> String {
    let Fetched::Failed { what, why } = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(what, "helm releases");
    why
}

#[tokio::test(start_paused = true)]
async fn release_inventory_uses_only_metadata_and_actual_labels_on_every_page() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let second = format!("{LIST}&continue=page-2%2F%2B%3D");
    route(
        &script,
        LIST,
        200,
        page(
            vec![
                revision("unrelated-spelling", "prod", "ingress", "1", "superseded"),
                revision("also-unrelated", "prod", "ingress", "2", "deployed"),
            ],
            "page-2/+=",
        ),
    );
    route(
        &script,
        &second,
        200,
        page(
            vec![
                revision("third", "prod", "api", "7", "future-status"),
                revision("broken", "prod", "broken", "not-a-version", "deployed"),
                serde_json::json!({"metadata":{"name":"missing-labels","namespace":"prod"}}),
            ],
            "",
        ),
    );
    let before = script.seen().len();
    let answer = fetch(&sync.reader, Some("prod")).await;
    let Fetched::Ok(releases) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(
        releases
            .releases
            .iter()
            .map(|r| (r.namespace.as_str(), r.name.as_str()))
            .collect::<Vec<_>>(),
        [("prod", "api"), ("prod", "ingress")]
    );
    assert_eq!(
        releases.releases[1]
            .revisions
            .iter()
            .map(|r| r.revision)
            .collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(
        releases.releases[1]
            .current()
            .expect("recorded deployed revision")
            .revision,
        2
    );
    assert_eq!(releases.releases[0].revisions[0].status, "future-status");
    assert_eq!(releases.unreadable, 2);
    assert!(!releases.truncated);
    for release in &releases.releases {
        for revision in &release.revisions {
            for field in [
                &revision.chart,
                &revision.chart_version,
                &revision.app_version,
                &revision.updated,
                &revision.description,
            ] {
                assert!(field.is_empty(), "payload-only fields stay unavailable");
            }
        }
    }
    let table = helm::table_page(&releases);
    assert!(
        table
            .rows
            .iter()
            .filter(|row| row.namespace.is_some())
            .all(|row| row.cells[4] == "unavailable (metadata only)")
    );
    assert!(!format!("{releases:?}").contains("not retained"));
    assert_lists(&after_sync(&script, before), &[LIST, &second]);
}

#[tokio::test(start_paused = true)]
async fn cluster_inventory_keeps_complete_names_and_separates_namespaces() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let path = LIST.replace("/namespaces/prod", "");
    let name = "r".repeat(63);
    let namespace = "n".repeat(63);
    route(
        &script,
        &path,
        200,
        page(
            vec![
                revision(&"s".repeat(253), &namespace, &name, "1", "deployed"),
                revision("another", "prod", &name, "2", "failed"),
            ],
            "",
        ),
    );
    let before = script.seen().len();
    let answer = fetch(&sync.reader, None).await;
    let Fetched::Ok(releases) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(releases.releases.len(), 2);
    assert_eq!(releases.unreadable, 0);
    let table = helm::table_page(&releases);
    for row in &table.rows {
        assert_eq!(row.name, name);
        assert_eq!(
            row.uid,
            format!("{}/{}", row.namespace.as_deref().expect("namespace"), name)
        );
    }
    assert!(
        table
            .rows
            .iter()
            .any(|row| row.namespace.as_deref() == Some(namespace.as_str()))
    );
    assert_lists(&after_sync(&script, before), &[&path]);
}

#[tokio::test(start_paused = true)]
async fn metadata_denials_and_failures_keep_their_state_and_never_retry_full_secrets() {
    for status in [401, 403, 406, 500] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        route(
            &script,
            LIST,
            status,
            r#"{"message":"release metadata cannot be served"}"#,
        );
        script.route("GET", &LIST.replacen("?&", "?", 1), 200,
            r#"{"kind":"SecretList","apiVersion":"v1","items":[{"data":{"release":"PLANTED-full-secret-retry"}}]}"#);
        let before = script.seen().len();
        let answer = fetch(&sync.reader, Some("prod")).await;
        if matches!(status, 401 | 403) {
            assert_eq!(
                answer,
                Fetched::Denied {
                    what: "helm releases"
                }
            );
        } else {
            assert_eq!(failed(answer), "release metadata cannot be served");
        }
        assert_lists(&after_sync(&script, before), &[LIST]);
    }
}

#[tokio::test(start_paused = true)]
async fn unsupported_metadata_or_full_object_responses_refuse_without_fallback() {
    for body in [
        r#"{"apiVersion":"v1","kind":"SecretList","items":[]}"#,
        r#"{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList"}"#,
        r#"{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList","items":{}}"#,
        r#"{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList","items":[{"metadata":{},"data":{"release":"PLANTED-full-body"}}]}"#,
        r#"{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList","items":[{"metadata":{"labels":{"name":12}}}]}"#,
    ] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        route(&script, LIST, 200, body);
        route(&script, LIST, 200, page(Vec::new(), ""));
        let before = script.seen().len();
        let why = failed(fetch(&sync.reader, Some("prod")).await);
        assert!(why.contains("no full Secret inventory is used"), "{why}");
        assert!(!why.contains("PLANTED"));
        assert_lists(&after_sync(&script, before), &[LIST]);
    }
}

#[tokio::test(start_paused = true)]
async fn empty_arrays_and_null_metadata_items_are_successful_empty_inventories() {
    for items in ["[]", "null"] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        route(
            &script,
            LIST,
            200,
            format!(
                r#"{{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList","items":{items}}}"#
            ),
        );
        let before = script.seen().len();
        assert_eq!(
            fetch(&sync.reader, Some("prod")).await,
            Fetched::Ok(helm::Releases::default())
        );
        assert_lists(&after_sync(&script, before), &[LIST]);
    }
}

#[tokio::test(start_paused = true)]
async fn unreadable_metadata_entries_also_exhaust_the_inventory_ceiling() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    route(
        &script,
        LIST,
        200,
        page(vec![serde_json::json!({"metadata":{}}); 2_001], "unused"),
    );
    let before = script.seen().len();
    let answer = fetch(&sync.reader, Some("prod")).await;
    assert_eq!(
        answer,
        Fetched::Ok(helm::Releases {
            releases: Vec::new(),
            truncated: true,
            unreadable: 2_000
        })
    );
    assert_lists(&after_sync(&script, before), &[LIST]);
}

#[tokio::test(start_paused = true)]
async fn empty_pages_cannot_continue_past_the_metadata_page_ceiling() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let paths: Vec<_> = (0..11)
        .map(|i| {
            if i == 0 {
                LIST.to_string()
            } else {
                format!("{LIST}&continue=page-{i}")
            }
        })
        .collect();
    for (i, path) in paths.iter().enumerate() {
        route(
            &script,
            path,
            200,
            page(Vec::new(), &format!("page-{}", i + 1)),
        );
    }
    let before = script.seen().len();
    assert_eq!(
        failed(fetch(&sync.reader, Some("prod")).await),
        "Helm releases exceed 10 metadata pages; incomplete inventory is refused"
    );
    assert_lists(
        &after_sync(&script, before),
        &paths[..10].iter().map(String::as_str).collect::<Vec<_>>(),
    );
}

#[tokio::test(start_paused = true)]
async fn repeated_or_oversized_continuation_tokens_refuse_before_another_read() {
    for repeated in [false, true] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        let token = if repeated {
            "next".to_string()
        } else {
            "x".repeat((16 << 10) + 1)
        };
        route(&script, LIST, 200, page(Vec::new(), &token));
        let second = format!("{LIST}&continue=next");
        if repeated {
            route(&script, &second, 200, page(Vec::new(), &token));
        }
        let before = script.seen().len();
        assert_eq!(
            failed(fetch(&sync.reader, Some("prod")).await),
            "Helm release metadata have an oversized or repeated continuation token; incomplete inventory is refused"
        );
        let expected = if repeated {
            vec![LIST, second.as_str()]
        } else {
            vec![LIST]
        };
        assert_lists(&after_sync(&script, before), &expected);
    }
}

#[tokio::test(start_paused = true)]
async fn nonconsecutive_continuation_tokens_refuse_before_repeating_a_page() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let second = format!("{LIST}&continue=A");
    let third = format!("{LIST}&continue=B");
    route(&script, LIST, 200, page(Vec::new(), "A"));
    route(&script, &second, 200, page(Vec::new(), "B"));
    route(&script, &third, 200, page(Vec::new(), "A"));
    let before = script.seen().len();
    assert_eq!(
        failed(fetch(&sync.reader, Some("prod")).await),
        "Helm release metadata have an oversized or repeated continuation token; incomplete inventory is refused"
    );
    assert_lists(&after_sync(&script, before), &[LIST, &second, &third]);
}

#[tokio::test(start_paused = true)]
async fn metadata_pages_share_the_reply_byte_budget() {
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let mut first = page(Vec::new(), "next");
    first.push_str(&" ".repeat((4 << 20) - first.len()));
    route(&script, LIST, 200, first);
    let second = format!("{LIST}&continue=next");
    route(&script, &second, 200, " ".repeat((4 << 20) + 1));
    let before = script.seen().len();
    let why = failed(fetch(&sync.reader, Some("prod")).await);
    assert_eq!(
        why,
        "helm releases answered with more than 4194304 bytes; the body is refused"
    );
    assert_lists(&after_sync(&script, before), &[LIST, &second]);
}

#[tokio::test(start_paused = true)]
async fn oversized_metadata_and_error_bodies_are_refused_on_the_first_read() {
    for (status, bytes) in [(200, (8 << 20) + 1), (500, (32 << 10) + 1)] {
        let script = Script::default();
        let (sync, _events) = connected(&script).await;
        route(&script, LIST, status, "x".repeat(bytes));
        let before = script.seen().len();
        let why = failed(fetch(&sync.reader, Some("prod")).await);
        assert!(why.contains("bytes; the body is refused"), "{why}");
        assert_lists(&after_sync(&script, before), &[LIST]);
    }
}

#[tokio::test(start_paused = true)]
async fn a_metadata_header_or_body_stall_obeys_one_exchange_deadline() {
    use futures::StreamExt as _;
    for body_stall in [false, true] {
        let script = Script::default();
        let server = script.clone();
        let client = kube::Client::new(
            tower::service_fn(move |request: http::Request<Body>| {
                let stall = body_stall
                    && request
                        .uri()
                        .query()
                        .is_some_and(|query| query.contains("fieldSelector=type"));
                let mut server = server.clone();
                async move {
                    let (parts, body) = server.call(request).await?.into_parts();
                    let tail = futures::stream::poll_fn(move |_| {
                        if stall {
                            Poll::Pending
                        } else {
                            Poll::Ready(None)
                        }
                    });
                    let body = http_body_util::StreamBody::new(
                        http_body_util::BodyStream::new(body).chain(tail),
                    );
                    Ok::<_, tower::BoxError>(http::Response::from_parts(parts, body))
                }
            }),
            "prod",
        );
        let (sync, _events) = connected_with(&script, client).await;
        if body_stall {
            route(&script, LIST, 200, page(Vec::new(), ""));
        } else {
            script.route_hanging("GET", &LIST.replacen("?&", "?", 1));
        }
        let before = script.seen().len();
        let started = tokio::time::Instant::now();
        assert_eq!(
            failed(fetch(&sync.reader, Some("prod")).await),
            "helm releases did not answer within 4 seconds"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(4));
        assert_lists(&after_sync(&script, before), &[LIST]);
    }
}

fn assert_refusal<T>(answer: Fetched<T>, apply: bool) {
    let Fetched::Failed { what, why } = answer else {
        panic!("payload action must report an explicit policy refusal");
    };
    assert_eq!(what, "helm revision");
    assert_eq!(
        why,
        if apply {
            format!("nothing was applied: {}", helm_reveal::PAYLOAD_UNAVAILABLE)
        } else {
            helm_reveal::PAYLOAD_UNAVAILABLE.to_string()
        }
    );
}

fn payload_targets() -> Vec<k10s_data::discover::KindTarget> {
    let mut catalog = k10s_core::Catalog::new();
    [("Secret", "secrets"), ("ConfigMap", "configmaps")]
        .into_iter()
        .map(|(kind, plural)| {
            k10s_data::discover::intern(
                &mut catalog,
                kube::discovery::ApiResource {
                    group: String::new(),
                    version: "v1".into(),
                    api_version: "v1".into(),
                    kind: kind.into(),
                    plural: plural.into(),
                },
                &kube::discovery::ApiCapabilities {
                    scope: kube::discovery::Scope::Namespaced,
                    subresources: Vec::new(),
                    operations: vec!["get".into(), "list".into(), "patch".into()],
                },
            )
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn reader_and_public_payload_actions_refuse_before_any_read_or_write() {
    use base64::Engine as _;
    let script = Script::default();
    let (sync, _events) = connected(&script).await;
    let before = script.seen().len();
    let targets = payload_targets();
    for namespace in [None, Some("prod")] {
        assert_refusal(
            helm_reveal::reveal_revision(&script.client(), &targets, namespace, "api", 1).await,
            false,
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        sync.reader.reveal_helm_revision(
            namespace.map(str::to_string),
            "api".into(),
            1,
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        assert_refusal(rx.await.expect("reveal callback"), false);
        let (tx, rx) = tokio::sync::oneshot::channel();
        sync.reader.diff_helm_revisions(
            namespace.map(str::to_string),
            "api".into(),
            1,
            2,
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        assert_refusal(rx.await.expect("diff callback"), false);
        let (tx, rx) = tokio::sync::oneshot::channel();
        sync.reader.rollback_helm_revision(
            namespace.map(str::to_string),
            "api".into(),
            1,
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        assert_refusal(rx.await.expect("apply callback"), true);
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(r#"{"name":"api","namespace":"prod","version":1,"manifest":"apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: api\ndata:\n  proof: caller-owned\n"}"#);
    let owned = helm_reveal::reveal_payload(&encoded).expect("caller-owned pure payload");
    let report = helm_reveal::rollback(&script.client(), &targets, &owned).await;
    assert!(
        matches!(&report.documents[..], [helm_reveal::DocumentRollback::Skipped {name,why,..}] if name == "api" && why == helm_reveal::PAYLOAD_UNAVAILABLE),
        "{report:?}"
    );
    assert!(
        after_sync(&script, before).is_empty(),
        "no action read or write: {:?}",
        after_sync(&script, before)
    );
}
