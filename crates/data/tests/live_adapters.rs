//! Live coverage for the seams `live_cluster.rs` does not own.
//!
//! The editor's apply, exec, logs, port-forward, and usage stay in that
//! file. This one drives Helm, Argo, Flux, overlays, describe, tables,
//! observability, and day-2 against the same cluster and the same two extra
//! identities. Ignored by default for the same no-network reason.
//!
//! ```text
//! KUBECONFIG=/path/to/kubeconfig cargo test -p k10s-data --test live_adapters -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` is required: the day-2 tests share a throwaway
//! Deployment that a parallel scale races. Grafana lock fixtures also need
//! exclusive access to the shared database.
//!
//! Helm inventory reads release Secret metadata only. Payload reveal, diff and
//! stored-revision apply refuse without reading or modifying a release. Argo
//! and Flux are pinned as absence when their groups are not served and as a
//! table when served. Day-2 mutates only `day2-probe` in K10S_LIVE_NAMESPACE;
//! it does not scale `web` or cordon or drain the node.
//!
//! The decode-failure proof needs the lab's permissive CNPG fixture CRD. It
//! creates one generated Cluster with an invalid instances field and removes
//! it before checking the returned ecosystem table.
//! The Grafana failure proof needs the observability lab's monitoring release
//! and its monitoring-grafana PVC. A generated pod holds a SQLite lock without
//! changing data. It is deleted even when a check panics; run this suite alone.
//! Saturated telemetry proves client cancellation and concurrent Kubernetes
//! reads and watch delivery, not backend cancellation or native UI latency.
//!
//! The drain proof requires `live_fixtures.sh --with kwok`: it drains a fake
//! kwok node whose one pod sits under a PodDisruptionBudget with no disruption
//! allowed, and deletes only its own node and namespace even when a check panics.
//! Creation and readiness share one minute, and cleanup has one further minute.
//!
//! The observability tests require `live_fixtures.sh --with observability`.
//! The monitoring test compares a scraped namespace creation time with
//! Kubernetes metadata and reads dashboards through a healthy Grafana. The
//! telemetry tests read a trace id from the producer's real container log,
//! then retrieve that span from Tempo and the same line from Loki through
//! the production Reader. A backend with no ingested data cannot satisfy them.
//! The alert proof creates its own PrometheusRule, reads the firing alert,
//! silences its exact namespace and pod, and removes only what it created.

use std::time::Duration;

#[path = "live_adapters/collector_route.rs"]
mod collector_route;
#[path = "live_adapters/inspector.rs"]
mod inspector;
#[path = "live_adapters/trace_logs.rs"]
mod trace_logs;

use k10s_core::KindId;
use k10s_data::apply::{ApplyOutcome, ApplyRequest};
use k10s_data::day2::{Blast, Caps, Day2Call, Day2Outcome, DeleteRequest, ScaleRequest};
use k10s_data::describe::DescribeRequest;
use k10s_data::integrations::argo;
use k10s_data::integrations::flux;
use k10s_data::integrations::helm;
use k10s_data::overlay;
use k10s_data::reach::ReachSettings;
use k10s_data::read::{Fetched, KindRow, Reader};
use k10s_data::{DEFAULT_EVENT_SINK_CAPACITY, Options, Sync};

const PROBE: &str = "day2-probe";

fn namespace() -> String {
    std::env::var("K10S_LIVE_NAMESPACE").unwrap_or_else(|_| "g2".to_string())
}

fn reader_context() -> String {
    std::env::var("K10S_LIVE_READER_CONTEXT").unwrap_or_else(|_| "reader@k10s-lab".to_string())
}

fn connect(context: Option<&str>) -> (k10s_data::DataPlane, Sync) {
    assert!(
        std::env::var_os("KUBECONFIG").is_some(),
        "this test needs KUBECONFIG to name a real cluster; see the module comment"
    );
    let (sink, _drain) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
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

fn wait_interactive<T: Send + 'static>(rx: &std::sync::mpsc::Receiver<T>, requests: u32) -> T {
    let budget = k10s_data::connect::INTERACTIVE_DEADLINE * requests + Duration::from_secs(5);
    rx.recv_timeout(budget)
        .expect("an interactive reply within its request budgets")
}

fn kube_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

fn delete_probe() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = kube::Client::try_default()
            .await
            .expect("a client from KUBECONFIG");
        let request = kube::api::Request::new(format!(
            "/apis/apps/v1/namespaces/{}/deployments",
            namespace()
        ))
        .delete(PROBE, &kube::api::DeleteParams::default())
        .expect("a delete request");
        let _ = client.request::<serde_json::Value>(request).await;
    });
}

fn deploy_replicas(name: &str) -> Option<i32> {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = kube::Client::try_default()
            .await
            .expect("a client from KUBECONFIG");
        let request = kube::api::Request::new(format!(
            "/apis/apps/v1/namespaces/{}/deployments",
            namespace()
        ))
        .get(name, &kube::api::GetParams::default())
        .expect("a get request");
        match client.request::<serde_json::Value>(request).await {
            Ok(value) => value
                .pointer("/spec/replicas")
                .and_then(|value| value.as_i64())
                .map(|n| n as i32),
            Err(_) => None,
        }
    })
}

fn probe_replicas() -> Option<i32> {
    deploy_replicas(PROBE)
}

fn apply(reader: &Reader, request: ApplyRequest) -> ApplyOutcome {
    let requests = if request.dry_run { 2 } else { 1 };
    let (tx, rx) = std::sync::mpsc::channel();
    reader.apply(request, move |outcome| {
        let _ = tx.send(outcome);
    });
    wait_interactive(&rx, requests)
}

fn day2(reader: &Reader, kind: KindId, call: Day2Call) -> Day2Outcome {
    let requests = match &call {
        Day2Call::Drain(request) if request.confirm => 2 + k10s_data::day2::MAX_DRAIN_PODS as u32,
        _ => 1,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    reader.day2(kind, call, move |outcome| {
        let _ = tx.send(outcome);
    });
    wait_interactive(&rx, requests)
}

fn scale_call(name: &str, current: i32, replicas: i32, confirm: bool) -> Day2Call {
    Day2Call::Scale(ScaleRequest {
        namespace: Some(namespace()),
        name: name.to_string(),
        current,
        replicas,
        confirm,
        caps: Caps::default(),
    })
}

fn ensure_probe(reader: &Reader, deployments: KindId) {
    delete_probe();
    let ns = namespace();
    let yaml = format!(
        r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: {PROBE}
  namespace: {ns}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: {PROBE}
  template:
    metadata:
      labels:
        app: {PROBE}
    spec:
      containers:
      - name: probe
        image: busybox:1.36
        command: ["sh", "-c", "sleep 3600"]
"#
    );
    let outcome = apply(
        reader,
        ApplyRequest {
            kind: deployments,
            namespace: Some(namespace()),
            name: PROBE.to_string(),
            yaml,
            dry_run: false,
            force: false,
        },
    );
    assert!(
        matches!(outcome, ApplyOutcome::Applied(_)),
        "the throwaway deploy must apply: {outcome:?}"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if probe_replicas() == Some(1) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the throwaway deploy never became readable");
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn helm_lists_stored_releases_and_never_a_secret_value() {
    let (_plane, sync) = connect(None);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_releases(None, move |fetched| {
        let _ = tx.send(fetched);
    });
    let Fetched::Ok(releases) = wait(&rx) else {
        panic!("Helm inventory must resolve on a cluster that serves Secrets");
    };
    assert!(
        !releases.releases.is_empty(),
        "this suite needs at least one stored Helm release (k3s Traefik is enough)"
    );
    assert_eq!(
        releases.unreadable, 0,
        "unusable release metadata must not become an empty inventory"
    );

    let names: Vec<_> = releases
        .releases
        .iter()
        .map(|release| (release.namespace.as_str(), release.name.as_str()))
        .collect();
    assert!(
        !names.iter().any(|(_, name)| *name == "api-token"
            || *name == "declared-token"
            || *name == "settings"),
        "an Opaque Secret is not a Helm release: {names:?}"
    );

    let leaked = format!("{releases:?}");
    let needles = [
        "super-secret-value",
        "plaintext-in-the-annotation",
        ROLLBACK_CANARY,
    ];
    for needle in needles {
        assert!(
            !leaked.contains(needle),
            "a Secret value reached the Helm inventory Debug"
        );
    }

    assert!(
        releases
            .releases
            .iter()
            .any(|release| release.namespace == "kube-system" && release.name == "traefik"),
        "the current lab's recorded Traefik release must be visible"
    );
    for release in &releases.releases {
        for revision in &release.revisions {
            for field in [
                &revision.chart,
                &revision.chart_version,
                &revision.app_version,
                &revision.updated,
                &revision.description,
            ] {
                assert!(
                    field.is_empty(),
                    "payload-only release fields are unavailable"
                );
            }
        }
    }

    let page = helm::table_page(&releases);
    assert_eq!(
        page.columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["Name", "Namespace", "Revision", "Status", "Chart"]
    );
    assert_eq!(page.rows.len(), releases.releases.len());
    for row in &page.rows {
        assert_eq!(row.cells.len(), 5, "{row:?}");
        assert_eq!(row.cells[4], "unavailable (metadata only)");
        for cell in &row.cells {
            assert!(
                !cell.contains("apiVersion:") && !cell.contains("kind:"),
                "a table cell carried a manifest: {cell:?}"
            );
            assert!(
                !needles.iter().any(|needle| cell.contains(needle)),
                "a table cell carried a Secret value"
            );
        }
        assert!(
            row.cells[2].chars().all(|c| c.is_ascii_digit()),
            "Revision is Helm's recorded number: {:?}",
            row.cells[2]
        );
    }
}

const ROLLBACK_RELEASE: &str = "rollback-probe";
const ROLLBACK_CANARY: &str = "PLANTED-rollback-canary";

fn admin_request(request: http::Request<Vec<u8>>) -> Result<serde_json::Value, kube::Error> {
    kube_runtime().block_on(async {
        let client = kube::Client::try_default().await?;
        client.request::<serde_json::Value>(request).await
    })
}

fn assert_payload_refusal<T>(answer: Fetched<T>, applied: bool) {
    let Fetched::Failed { what, why } = answer else {
        panic!("Helm payload actions require an explicit policy refusal");
    };
    assert_eq!(what, "helm revision");
    assert_eq!(
        why,
        if applied {
            format!(
                "nothing was applied: {}",
                k10s_data::integrations::helm_reveal::PAYLOAD_UNAVAILABLE
            )
        } else {
            k10s_data::integrations::helm_reveal::PAYLOAD_UNAVAILABLE.to_string()
        }
    );
}

fn assert_reader_payload_refusals(context: Option<&str>) {
    let (_plane, sync) = connect(context);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.reveal_helm_revision(
        Some(namespace()),
        ROLLBACK_RELEASE.into(),
        1,
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    assert_payload_refusal(wait(&rx), false);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.diff_helm_revisions(
        Some(namespace()),
        ROLLBACK_RELEASE.into(),
        1,
        2,
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    assert_payload_refusal(wait(&rx), false);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.rollback_helm_revision(
        Some(namespace()),
        ROLLBACK_RELEASE.into(),
        1,
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    assert_payload_refusal(wait(&rx), true);
}

#[test]
#[ignore = "needs the live lab; payload routes must send no requests"]
fn public_helm_payload_actions_send_no_requests_on_a_real_client() {
    use base64::Engine as _;
    use k10s_data::integrations::helm_reveal;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    assert!(
        std::env::var_os("KUBECONFIG").is_some(),
        "KUBECONFIG must name the live lab"
    );
    let runtime = kube_runtime();
    runtime.block_on(async {
        let actual = kube::Client::try_default()
            .await
            .expect("the current lab account");
        let mut catalog = k10s_core::Catalog::new();
        let discovered = k10s_data::discover::discover(&actual, &mut catalog)
            .await
            .expect("current lab discovery");
        assert!(discovered.targets.iter().any(|target| {
            target.kind() == "Secret" && target.listable
        }));
        assert!(discovered.targets.iter().any(|target| {
            target.kind() == "ConfigMap" && target.patchable
        }));
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let guarded = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let actual = actual.clone();
            let seen = seen.clone();
            let unsafe_secret = request.uri().path().contains("/secrets")
                && !request.headers().get(http::header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|accept| accept.contains("PartialObjectMetadata"));
            let refused = request.method() != http::Method::GET || unsafe_secret;
            async move {
                seen.fetch_add(1, Ordering::Relaxed);
                if refused {
                    return Err(kube::Error::Service(std::io::Error::other(
                        "unexpected Helm payload request refused before transport",
                    ).into()));
                }
                actual.send(request).await
            }
        });
        let client = kube::Client::new(guarded, namespace());
        for ns in [None, Some(namespace())] {
            let answer = helm_reveal::reveal_revision(
                &client, &discovered.targets, ns.as_deref(), ROLLBACK_RELEASE, 1,
            ).await;
            assert_payload_refusal(answer, false);
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(
            r#"{"name":"rollback-probe","namespace":"g2","version":1,"manifest":"apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: rollback-probe\ndata:\n  proof: caller-owned\n"}"#,
        );
        let owned = helm_reveal::reveal_payload(&encoded)
            .expect("caller-owned pure decoder fixture");
        let report = helm_reveal::rollback(&client, &discovered.targets, &owned).await;
        assert!(matches!(&report.documents[..],
            [helm_reveal::DocumentRollback::Skipped {why,..}]
                if why == helm_reveal::PAYLOAD_UNAVAILABLE), "{report:?}");
        assert_eq!(requests.load(Ordering::Relaxed), 0,
            "payload actions dispatch no reads or writes");
        println!("Helm public payload actions dispatched zero requests; discovery confirmed Secret LIST and ConfigMap PATCH are served");
    });
}

#[test]
#[ignore = "needs the live lab; release payloads remain unavailable for administrators"]
fn administrator_helm_payload_actions_report_the_metadata_only_refusal() {
    assert_reader_payload_refusals(None);
}

#[test]
#[ignore = "needs reader@k10s-lab; release payloads remain unavailable for restricted accounts"]
fn restricted_helm_payload_actions_report_the_metadata_only_refusal() {
    assert_reader_payload_refusals(Some(&reader_context()));
}

#[test]
#[ignore = "needs the live lab's permissive CNPG fixture CRD; see the module comment"]
fn an_unreadable_live_resource_survives_into_the_ecosystem_table() {
    use k10s_data::integrations::cnpg;
    use kube::api::{ApiResource, DynamicObject, GroupVersionKind};

    let (_plane, sync) = connect(None);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_cnpg(move |answer| {
        let _ = tx.send(answer);
    });
    let Fetched::Ok(before) = wait(&rx) else {
        panic!("the lab's CNPG inventory can be read");
    };
    assert!(
        matches!(before.clusters, cnpg::KindSet::Served { unreadable: 0, .. }),
        "the initial CNPG fixtures must decode: {before:?}"
    );
    let before = cnpg::table_page(&before).expect("the CNPG fixture kind is served");
    assert!(
        !before.rows.is_empty(),
        "the lab has a readable CNPG fixture"
    );

    let runtime = kube_runtime();
    let client = runtime
        .block_on(kube::Client::try_default())
        .expect("the fixture client");
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "postgresql.cnpg.io",
        "v1",
        "Cluster",
    ));
    let api: kube::Api<DynamicObject> = kube::Api::namespaced_with(client, &namespace(), &resource);
    let fixture: DynamicObject = serde_json::from_value(serde_json::json!({
        "apiVersion": "postgresql.cnpg.io/v1",
        "kind": "Cluster",
        "metadata": {"generateName": "k10s-unreadable-"},
        "spec": {"instances": "invalid-fixture-value"},
    }))
    .expect("the fixture object");
    let created = runtime
        .block_on(api.create(&kube::api::PostParams::default(), &fixture))
        .expect("the permissive lab CRD accepts the malformed fixture");
    let name = created
        .metadata
        .name
        .expect("the server assigns the fixture's name");

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_ecosystem(move |answer| {
        let _ = tx.send(answer);
    });
    let answer = rx.recv_timeout(Duration::from_secs(30));
    runtime
        .block_on(api.delete(&name, &kube::api::DeleteParams::default()))
        .expect("the generated fixture is removed before asserting the reply");
    assert!(
        runtime
            .block_on(api.get_opt(&name))
            .expect("the cleanup can be read back")
            .is_none()
    );

    let family = answer
        .expect("the ecosystem reply arrives within the budget")
        .into_iter()
        .find(|family| family.id == "cnpg")
        .expect("the ecosystem includes CNPG");
    let Fetched::Ok(Some(page)) = family.answer else {
        panic!(
            "the served family remains a labelled table: {:?}",
            family.answer
        );
    };
    assert_eq!(page.rows.len(), before.rows.len() + 1, "{page:?}");
    let notice = page
        .rows
        .iter()
        .find(|row| row.uid == "unreadable:Cluster")
        .expect("the failed decode has a visible row");
    assert_eq!(notice.cells[0], "Cluster");
    assert_eq!(notice.cells[3], "1 object could not be decoded");
    assert_eq!(notice.namespace, None);
    for row in before.rows {
        assert!(
            page.rows.contains(&row),
            "the readable row keeps its identity and cells: {row:?}"
        );
    }
}

async fn with_grafana_storage_lock<T>(
    client: &kube::Client,
    check: impl std::future::Future<Output = T>,
) -> T {
    use futures::FutureExt;
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{AttachParams, DeleteParams, PostParams, Preconditions};
    use kube::runtime::wait::{await_condition, conditions};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let pods: kube::Api<Pod> = kube::Api::namespaced(client.clone(), "observability");
    let fixture: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"generateName": "k10s-grafana-lock-"},
            "spec": {
                "restartPolicy": "Never", "activeDeadlineSeconds": 120,
                "securityContext": {
                    "runAsUser": 472, "runAsGroup": 472, "fsGroup": 472, "runAsNonRoot": true,
                },
                "containers": [{
                    "name": "lock",
                    "image": "python:3.14.7-alpine3.24@sha256:c6ead215bfd31f1e433d968853b7a769989117115b728874824e6c0a27cb96fc",
                    "command": ["python", "-c", "import time; time.sleep(120)"],
                    "resources": {
                        "requests": {"cpu": "10m", "memory": "32Mi"},
                        "limits": {"memory": "96Mi"},
                    },
                    "securityContext": {
                        "allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]},
                    },
                    "volumeMounts": [{"name": "storage", "mountPath": "/var/lib/grafana"}],
                }],
                "volumes": [{
                    "name": "storage", "persistentVolumeClaim": {"claimName": "monitoring-grafana"},
                }],
            },
    })).expect("the lock fixture");
    let created = tokio::time::timeout(
        Duration::from_secs(60),
        pods.create(&PostParams::default(), &fixture),
    )
    .await
    .expect("the lock pod create timed out; a write may have landed without a returned UID")
    .expect("the lock pod");
    let name = created.metadata.name.expect("the generated name");
    let uid = created.metadata.uid.expect("the generated UID");

    let checked = std::panic::AssertUnwindSafe(async {
        tokio::time::timeout(
            Duration::from_secs(60),
            await_condition(pods.clone(), &name, conditions::is_pod_running()),
        )
        .await
        .expect("the lock pod starts within a minute")
        .expect("the pod can be watched");
        let code = r#"
import sqlite3
import sys

database = sqlite3.connect("file:/var/lib/grafana/grafana.db?mode=rw", uri=True)
database.execute("BEGIN EXCLUSIVE")
print("locked", flush=True)
sys.stdin.readline()
database.rollback()
print("released", flush=True)
"#;
        let mut lock = tokio::time::timeout(
            Duration::from_secs(10),
            pods.exec(
                &name,
                ["python", "-u", "-c", code],
                &AttachParams::default()
                    .stdin(true)
                    .stdout(true)
                    .stderr(true),
            ),
        )
        .await
        .expect("the lock process attaches within 10 seconds")
        .expect("the lock process attaches");
        let status = lock.take_status().expect("the remote exit status");
        let mut input = lock.stdin().expect("the release channel");
        let mut output = BufReader::new(lock.stdout().expect("the lock markers")).lines();
        let marker = tokio::time::timeout(Duration::from_secs(10), output.next_line())
            .await
            .expect("the database can be locked")
            .expect("the lock marker can be read");
        assert_eq!(marker.as_deref(), Some("locked"));

        let checked = std::panic::AssertUnwindSafe(check).catch_unwind().await;

        tokio::time::timeout(Duration::from_secs(10), input.write_all(b"release\n"))
            .await
            .expect("the database lock release channel answers within 10 seconds")
            .expect("release the database lock");
        let marker = tokio::time::timeout(Duration::from_secs(10), output.next_line())
            .await
            .expect("the lock releases")
            .expect("the release marker can be read");
        assert_eq!(marker.as_deref(), Some("released"));
        let status = tokio::time::timeout(Duration::from_secs(10), status)
            .await
            .expect("the lock process exits")
            .expect("the exit status arrives");
        assert_eq!(status.status.as_deref(), Some("Success"), "{status:?}");
        tokio::time::timeout(Duration::from_secs(10), lock.join())
            .await
            .expect("the lock process ends")
            .expect("the lock connection closes");
        checked.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
    .catch_unwind()
    .await;

    tokio::time::timeout(Duration::from_secs(15), async {
        match pods
            .delete(
                &name,
                &DeleteParams {
                    grace_period_seconds: Some(0),
                    preconditions: Some(Preconditions {
                        uid: Some(uid.clone()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => panic!("generated lock pod {name} UID {uid} cleanup refused: {error}"),
        }
        await_condition(pods, &name, conditions::is_deleted(&uid))
            .await
            .expect("the lock pod cleanup can be watched");
    })
    .await
    .expect("the generated lock pod is removed within 15 seconds");
    checked.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

#[test]
#[ignore = "needs the observability lab's Grafana and PVC; see the module comment"]
fn a_busy_live_grafana_catalog_reports_its_timeout_and_recovers() {
    use k10s_data::reach::{self, ToolKind, ToolReach};

    let (_plane, sync) = connect(None);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_grafana_catalog(move |answer| {
        let _ = tx.send(answer);
    });
    let Fetched::Ok(before) = wait(&rx) else {
        panic!("the lab's Grafana catalog must initially answer");
    };
    assert!(before.served && !before.dashboards.is_empty());
    assert!(
        before.errors.is_empty(),
        "the initial catalog must be complete: {:?}",
        before.errors
    );

    kube_runtime().block_on(async {
        let mut config = kube::Config::infer().await.expect("the fixture kubeconfig");
        config.default_retry = false;
        let client = kube::Client::try_from(config).expect("the fixture client");
        let (reached, answer) = with_grafana_storage_lock(&client, async {
            let reached = reach::bind(&client, ToolKind::Grafana, &ReachSettings::default()).await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            sync.reader.fetch_grafana_catalog(move |answer| {
                let _ = tx.send(answer);
            });
            let answer = tokio::time::timeout(Duration::from_secs(15), rx).await;
            (reached, answer)
        })
        .await;
        assert!(
            matches!(reached, ToolReach::Bound(_)),
            "health still answers while storage is locked: {reached:?}"
        );
        let answer = answer
            .expect("the catalog reports within the budget")
            .expect("the callback arrives");
        let Fetched::Ok(partial) = answer else {
            panic!("healthy provisioned dashboards remain visible beside the timeout: {answer:?}");
        };
        assert!(partial.served && !partial.dashboards.is_empty());
        assert_eq!(
            partial.errors,
            ["Grafana search: grafana did not answer within 4 seconds"]
        );
        for dashboard in &partial.dashboards {
            assert!(before.dashboards.contains(dashboard), "{dashboard:?}");
        }
    });

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_grafana_catalog(move |answer| {
        let _ = tx.send(answer);
    });
    assert_eq!(
        wait(&rx),
        Fetched::Ok(before),
        "the catalog recovers after releasing the lock"
    );
}

#[test]
#[ignore = "needs the owned pilot, Grafana and PVC; run the live suite serially"]
fn stalled_telemetry_cancels_at_capacity_while_kubernetes_reads_and_watches_progress() {
    use futures::FutureExt;
    use k8s_openapi::api::{
        apps::v1::Deployment,
        core::v1::{Namespace, Pod},
    };
    use k10s_core::{Capability, IngestEvent, Op};
    use k10s_data::integrations::{grafana, prom};
    use kube::api::{DeleteParams, ListParams, PostParams, Preconditions};
    use kube::runtime::wait::{await_condition, conditions};
    use std::time::Instant;

    const NS: &str = "k10s-observe-pilot";
    const PREFIX: &str = "k10s-telemetry-progress-";

    assert!(
        std::env::var_os("KUBECONFIG").is_some(),
        "the real lab kubeconfig is required"
    );
    let runtime = kube_runtime();
    let (client, pod, uid, node) = runtime.block_on(async {
        let mut config = kube::Config::infer().await.expect("the fixture kubeconfig");
        config.default_retry = false;
        let client = kube::Client::try_from(config).expect("the admin client");
        let namespaces: kube::Api<Namespace> = kube::Api::all(client.clone());
        let namespace = tokio::time::timeout(Duration::from_secs(15), namespaces.get_metadata(NS))
            .await
            .expect("the owned namespace read answers within 15 seconds")
            .expect("the owned pilot namespace");
        assert_eq!(
            namespace
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("k10s.dev/fixture"))
                .map(String::as_str),
            Some("observe-pilot")
        );
        let pods: kube::Api<Pod> = kube::Api::namespaced(client.clone(), NS);
        let page = tokio::time::timeout(
            Duration::from_secs(15),
            pods.list_metadata(
                &ListParams::default()
                    .labels("app.kubernetes.io/name=pilot-producer")
                    .limit(2),
            ),
        )
        .await
        .expect("the producer census answers within 15 seconds")
        .expect("the current producer metadata");
        assert_eq!(page.items.len(), 1, "one current producer is required");
        let item = &page.items[0];
        let name = item.metadata.name.clone().expect("pod name");
        let uid = item.metadata.uid.clone().expect("pod UID");
        let current = tokio::time::timeout(Duration::from_secs(15), pods.get(&name))
            .await
            .expect("the current producer read answers within 15 seconds")
            .expect("the current producer");
        assert_eq!(
            current.metadata.uid.as_ref(),
            Some(&uid),
            "the producer was recreated during setup"
        );
        let node = current
            .spec
            .and_then(|spec| spec.node_name)
            .expect("the actual producer node");
        (client, name, uid, node)
    });

    let (sink, events) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
    let plane = k10s_data::spawn(sink).expect("the production data runtime");
    let (watch_tx, mut watch_rx) = tokio::sync::mpsc::channel(16);
    let drain = std::thread::spawn(move || {
        for event in events {
            let keep = match &event {
                IngestEvent::Resource(event) => {
                    event.kind == KindId::DEPLOYMENT
                        && event.namespace.as_ref() == NS
                        && event.name.starts_with(PREFIX)
                }
                IngestEvent::Synced { kind } | IngestEvent::Desync { kind, .. } => {
                    *kind == KindId::DEPLOYMENT
                }
                IngestEvent::Capability { .. } => false,
            };
            if keep {
                let _ = watch_tx.blocking_send((Instant::now(), event));
            }
        }
    });
    let sync = plane
        .sync(&Options {
            context: None,
            kubeconfig: None,
            probe_namespaces: vec![NS.into()],
            sync_timeout: Duration::from_secs(30),
        })
        .expect("the real pilot sync");
    let deployments = kind(&sync.reader, "deployments.apps");
    assert_eq!(deployments.id, KindId::DEPLOYMENT);
    assert_eq!(
        deployments.verdict,
        Some(Capability::Watchable),
        "{}",
        sync.report.summary()
    );
    assert!(
        !sync
            .report
            .desyncs
            .iter()
            .any(|(kind, _)| *kind == KindId::DEPLOYMENT),
        "{:?}",
        sync.report
    );
    runtime.block_on(async {
        if !sync.events.iter().any(
            |event| matches!(event, IngestEvent::Synced { kind } if *kind == KindId::DEPLOYMENT),
        ) {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    match watch_rx
                        .recv()
                        .await
                        .expect("the Deployment watch stays open")
                        .1
                    {
                        IngestEvent::Synced { .. } => break,
                        IngestEvent::Desync { reason, .. } => {
                            panic!("Deployment watch did not settle: {reason:?}")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("the Deployment watch settles before the stall");
        }
    });

    let (tx, rx) = std::sync::mpsc::channel();
    let catalog_stop = sync.reader.observe_grafana(move |answer| {
        let _ = tx.send(answer);
    });
    let catalog = match wait(&rx) {
        Fetched::Ok(catalog) => catalog,
        other => panic!("the owned pilot dashboard must answer: {other:?}"),
    };
    assert!(
        catalog.served && !catalog.truncated && catalog.errors.is_empty(),
        "{catalog:?}"
    );
    let dashboard = catalog
        .dashboards
        .iter()
        .find(|dashboard| dashboard.uid == "k10s-observe-pilot-native")
        .expect("the actual owned pilot dashboard");
    let panel = dashboard
        .panels
        .iter()
        .find(|panel| {
            panel.unsupported.is_none()
                && !panel.transformed
                && panel.queries.len() == 1
                && panel.queries[0].dialect == grafana::QueryDialect::PromQL
        })
        .expect("one supported actual pilot panel");
    let target = panel.queries[0].clone();
    assert_eq!(target.datasource.as_deref(), Some("prometheus"));
    let end_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_millis() as i64;
    let variables = grafana::Variables {
        namespace: NS.into(),
        pods: vec![pod.clone()],
        node: Some(node),
        start_ms: end_ms - 300_000,
        end_ms,
        step_ms: 15_000,
        min_interval: panel.min_interval.clone(),
    };
    let current_panel = |answer: Fetched<Option<prom::QueryResult>>| {
        let result = match answer {
            Fetched::Ok(Some(result)) => result,
            other => panic!("the real current-UID panel must return samples: {other:?}"),
        };
        assert!(
            !result.truncated && result.warnings.is_empty() && result.infos.is_empty(),
            "{result:?}"
        );
        assert!(!result.series.is_empty(), "{result:?}");
        assert!(
            result.series.iter().all(|series| {
                [
                    ("namespace", NS),
                    ("pod", pod.as_str()),
                    ("uid", uid.as_str()),
                ]
                .iter()
                .all(|(key, value)| {
                    series
                        .labels
                        .iter()
                        .any(|(name, label)| name.as_str() == *key && label.as_str() == *value)
                })
            }),
            "{result:?}"
        );
        assert!(
            result.series.iter().any(|series| series
                .points
                .iter()
                .any(|(_, value)| value.is_finite() && *value > 0.0)),
            "{result:?}"
        );
        result
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let warm_stop = sync
        .reader
        .observe_panel(target.clone(), variables.clone(), move |answer| {
            let _ = tx.send(answer);
        });
    let before = current_panel(wait(&rx));
    drop(warm_stop);
    drop(catalog_stop);

    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while sync.reader.observe_schedule_counts().admitted != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("warm requests release their scheduler slots");
        with_grafana_storage_lock(&client, async {
            let stalled_at = Instant::now();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(6);
            let mut stops = Vec::with_capacity(6);
            for request in 0..6 {
                let tx = reply_tx.clone();
                stops.push(sync.reader.observe_panel(target.clone(), variables.clone(), move |answer| {
                    let _ = tx.try_send((request, answer));
                }));
            }
            drop(reply_tx);
            tokio::time::timeout_at(deadline, async {
                loop {
                    let counts = sync.reader.observe_schedule_counts();
                    if counts.active == 2 && counts.admitted == 6 { break; }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }).await.expect("two requests run while four wait against locked Grafana");
            let saturated = sync.reader.observe_schedule_counts();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _rejected = sync.reader.observe_panel(target.clone(), variables.clone(), move |answer| { let _ = tx.send(answer); });
            let rejected = tokio::time::timeout_at(deadline, rx).await.expect("capacity refusal is immediate").expect("the refusal callback");
            assert_eq!(rejected, Fetched::Failed {
                what: "telemetry",
                why: "telemetry is busy: two requests are active and four are waiting".into(),
            });

            let api: kube::Api<Deployment> = kube::Api::namespaced(client.clone(), NS);
            let probe_label = format!("{}-{end_ms}", std::process::id());
            let probe_selector = format!("k10s.dev/probe={probe_label}");
            let fixture: Deployment = serde_json::from_value(serde_json::json!({
                "apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": {"generateName": PREFIX, "labels": {"k10s.dev/fixture": "observe-pilot"}},
                "spec": {
                    "replicas": 0,
                    "selector": {"matchLabels": {"k10s.dev/probe": probe_label}},
                    "template": {
                        "metadata": {"labels": {"k10s.dev/probe": probe_label}},
                        "spec": {
                            "automountServiceAccountToken": false,
                            "containers": [{"name": "never-started", "image": "python:3.14.7-alpine3.24"}]
                        }
                    }
                }
            })).expect("the zero-replica progress fixture");
            let metrics_before = plane.metrics();
            let created_at = Instant::now();
            let created = tokio::time::timeout_at(deadline, api.create(&PostParams::default(), &fixture))
                .await.expect("progress Deployment create timed out; a write may have landed without a returned UID")
                .expect("create only the generated progress Deployment");
            let name = created.metadata.name.expect("generated Deployment name");
            let probe_uid = created.metadata.uid.expect("generated Deployment UID");
            let checked = std::panic::AssertUnwindSafe(async {
                assert_eq!(created.spec.as_ref().and_then(|spec| spec.replicas), Some(0));
                let (tx, rx) = tokio::sync::oneshot::channel();
                let read_started = Instant::now();
                sync.reader.fetch_manifest(DescribeRequest {
                    kind: deployments.id, namespace: Some(NS.into()), name: name.clone(), uid: probe_uid.clone(),
                }, move |answer| { let _ = tx.send((answer, Instant::now())); });
                let delivered_at = tokio::time::timeout_at(deadline, async {
                    loop {
                        let (at, event) = watch_rx.recv().await.expect("the production watch remains open");
                        match event {
                            IngestEvent::Resource(event) if event.uid.as_ref() == probe_uid.as_str() && event.op == Op::Added => {
                                assert_eq!(event.namespace.as_ref(), NS);
                                assert_eq!(event.name.as_ref(), name.as_str());
                                break at;
                            }
                            IngestEvent::Desync { reason, .. } => panic!("Deployment watch stalled with {reason:?}"),
                            _ => {}
                        }
                    }
                }).await.expect("the exact created UID reaches the watch while telemetry remains stalled");
                let (read, read_at) = tokio::time::timeout_at(deadline, rx).await
                    .expect("an ordinary Kubernetes read progresses during saturation").expect("the ordinary read callback");
                match read {
                    Fetched::Ok(manifest) => assert_eq!(manifest.uid.as_deref(), Some(probe_uid.as_str())),
                    other => panic!("the owned zero-replica Deployment read failed: {other:?}"),
                }
                let still_stalled = sync.reader.observe_schedule_counts();
                assert_eq!(still_stalled.active, 2, "{still_stalled:?}");
                assert_eq!(still_stalled.admitted, 6, "{still_stalled:?}");
                let premature = reply_rx.try_recv();
                assert!(matches!(premature, Err(tokio::sync::mpsc::error::TryRecvError::Empty)), "locked requests must not finish before cancellation: {premature:?}");

                let cancelled_at = Instant::now();
                drop(stops);
                tokio::time::timeout_at(deadline, async {
                    loop {
                        let counts = sync.reader.observe_schedule_counts();
                        if counts.active == 0 && counts.admitted == 0 { break; }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }).await.expect("cancellation releases active and waiting capacity before backend timeout");
                let released_at = Instant::now();
                let cancelled_reply = tokio::time::timeout_at(deadline, reply_rx.recv()).await
                    .expect("all cancelled callback senders are dropped");
                assert!(cancelled_reply.is_none(), "cancelled requests published a reply: {cancelled_reply:?}");
                let pods: kube::Api<Pod> = kube::Api::namespaced(client.clone(), NS);
                let spawned = tokio::time::timeout(Duration::from_secs(5),
                    pods.list_metadata(&ListParams::default().labels(&probe_selector).limit(1)))
                    .await.expect("the zero-replica Pod census answers within five seconds")
                    .expect("the generated probe Pod census");
                assert!(spawned.items.is_empty(), "the zero-replica probe must not create Pods: {:?}", spawned.items);
                assert!(spawned.metadata.continue_.as_deref().is_none_or(str::is_empty));
                let counts = sync.reader.observe_schedule_counts();
                assert_eq!(counts.peak_active, 2);
                assert_eq!(counts.peak_admitted, 6);
                assert_eq!(counts.rejected, 1);
                let metrics_after = plane.metrics();
                assert!(metrics_after.forwarded > metrics_before.forwarded, "{metrics_before:?} -> {metrics_after:?}");
                println!("live telemetry client cancellation and data-plane progress: producer={NS}/{pod} UID={uid}, probe={name} UID={probe_uid}, saturated={saturated:?}, recovered={counts:?}, watch={:?}, ordinary_read={:?}, cancellation={:?}, stall_total={:?}, ingest={metrics_before:?}->{metrics_after:?}; server cancellation, unfinished wire bytes and GPUI responsiveness are not measured",
                    delivered_at.duration_since(created_at), read_at.duration_since(read_started), released_at.duration_since(cancelled_at), stalled_at.elapsed());
            }).catch_unwind().await;
            tokio::time::timeout(Duration::from_secs(15), async {
                match api.delete(&name, &DeleteParams {
                    preconditions: Some(Preconditions {uid: Some(probe_uid.clone()), ..Default::default()}),
                    propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
                    ..Default::default()
                }).await {
                    Ok(_) => {},
                    Err(kube::Error::Api(error)) if error.code == 404 => {},
                    Err(error) => panic!("generated Deployment {name} UID {probe_uid} cleanup refused: {error}"),
                }
                await_condition(api, &name, conditions::is_deleted(&probe_uid)).await.expect("the progress fixture cleanup can be watched");
            }).await.expect("the owned progress Deployment is removed within 15 seconds");
            checked.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        }).await;
    });

    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.observe_panel(target, variables, move |answer| {
        let _ = tx.send(answer);
    });
    let after = current_panel(wait(&rx));
    runtime.block_on(async {
        let pods: kube::Api<Pod> = kube::Api::namespaced(client, NS);
        let current = tokio::time::timeout(Duration::from_secs(15), pods.get_metadata(&pod))
            .await
            .expect("the recovered producer identity read answers within 15 seconds")
            .expect("the producer still exists after recovery");
        assert_eq!(
            current.metadata.uid.as_ref(),
            Some(&uid),
            "recovery must use the same current producer UID"
        );
    });
    assert_eq!(
        (after.source, after.expr, after.step_ms),
        (before.source, before.expr, before.step_ms)
    );
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while sync.reader.observe_schedule_counts().admitted != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the successful recovery query releases its slot");
    });
    drop(_stop);
    drop(sync);
    drop(watch_rx);
    drop(plane);
    drain
        .join()
        .expect("the production event receiver drains and closes");
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn argo_and_flux_are_absent_when_the_groups_are_not_served() {
    let (_plane, sync) = connect(None);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_argo(move |fetched| {
        let _ = tx.send(fetched);
    });
    let Fetched::Ok(argo) = wait(&rx) else {
        panic!("Argo absence is Ok, not a failure");
    };
    if argo.served {
        assert!(
            argo::table_page(&argo).is_some(),
            "a served Argo group is a table, even when it has no Applications"
        );
    } else {
        assert!(
            argo::table_page(&argo).is_none(),
            "an unserved Argo group must not open an empty pane"
        );
    }

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_flux(move |fetched| {
        let _ = tx.send(fetched);
    });
    let Fetched::Ok(flux) = wait(&rx) else {
        panic!("Flux absence is Ok, not a failure");
    };
    if flux.served() {
        assert!(
            flux::table_page(&flux).is_some(),
            "a served Flux group is a table, even when it has no objects"
        );
    } else {
        assert!(
            flux::table_page(&flux).is_none(),
            "an unserved Flux group must not open an empty pane"
        );
    }
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn overlays_degrade_to_a_note_when_the_adapter_is_not_on_the_cluster() {
    let (_plane, sync) = connect(None);
    let settings = ReachSettings::default();
    for kind in [
        overlay::Kind::Sync,
        overlay::Kind::Metrics,
        overlay::Kind::Policy,
        overlay::Kind::MeshDeclared,
        overlay::Kind::MeshObserved,
    ] {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader
            .fetch_overlay(kind, settings.clone(), move |fetched| {
                let _ = tx.send(fetched);
            });
        match wait(&rx) {
            Fetched::Ok(frame) => {
                println!(
                    "{kind:?}: note={:?} stamps={}",
                    frame.note,
                    frame.stamps.len()
                );
            }
            Fetched::Denied { what } => panic!("{kind:?} arrived as Denied({what})"),
            Fetched::Failed { what, why } => {
                panic!("{kind:?} must not fail when the adapter is missing: {what}: {why}")
            }
        }
    }
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn describe_and_table_read_the_g2_fixtures() {
    let (_plane, sync) = connect(None);
    let deployments = kind(&sync.reader, "deployments.apps");

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_describe(
        DescribeRequest {
            kind: deployments.id,
            namespace: Some(namespace()),
            name: "web".to_string(),
            uid: String::new(),
        },
        move |fetched| {
            let _ = tx.send(fetched);
        },
    );
    let Fetched::Ok(described) = wait(&rx) else {
        panic!("describe web must resolve");
    };
    assert!(
        described.title.contains("web"),
        "describe titles the object: {}",
        described.title
    );
    assert!(
        !described.lines.is_empty(),
        "a live describe is not an empty document"
    );
    let text = described.lines.join("\n");
    assert!(
        !text.contains("super-secret-value") && !text.contains("plaintext-in-the-annotation"),
        "a Deployment describe carried a Secret value"
    );

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_table(deployments.id, None, move |fetched| {
            let _ = tx.send(fetched);
        });
    let Fetched::Ok(page) = wait(&rx) else {
        panic!("the deployments table must resolve");
    };
    let names: Vec<_> = page.rows.iter().map(|row| row.name.as_str()).collect();
    for expected in ["web", "usage-probe", "forward-probe"] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn day2_scale_waits_for_confirm_then_writes_and_restores() {
    let (_plane, sync) = connect(None);
    let deployments = kind(&sync.reader, "deployments.apps");
    ensure_probe(&sync.reader, deployments.id);

    let first = day2(&sync.reader, deployments.id, scale_call(PROBE, 1, 2, false));
    match first {
        Day2Outcome::NeedsConfirm {
            blast: Blast::Replicas { from: 1, to: 2 },
            ..
        } => {}
        other => panic!("the first press must not touch the wire: {other:?}"),
    }
    assert_eq!(
        probe_replicas(),
        Some(1),
        "confirm=false left a write on the cluster"
    );

    let applied = day2(&sync.reader, deployments.id, scale_call(PROBE, 1, 2, true));
    assert!(
        matches!(applied, Day2Outcome::Applied(_)),
        "the confirmed scale must apply: {applied:?}"
    );
    assert_eq!(probe_replicas(), Some(2));

    let restored = day2(&sync.reader, deployments.id, scale_call(PROBE, 2, 1, true));
    assert!(
        matches!(restored, Day2Outcome::Applied(_)),
        "the restore scale must apply: {restored:?}"
    );
    assert_eq!(probe_replicas(), Some(1));

    let ask = day2(
        &sync.reader,
        deployments.id,
        Day2Call::Delete(DeleteRequest {
            namespace: Some(namespace()),
            name: PROBE.to_string(),
            grace_period_seconds: Some(0),
            confirm: false,
            caps: Caps::default(),
        }),
    );
    assert!(
        matches!(ask, Day2Outcome::NeedsConfirm { .. }),
        "delete without confirm must not remove the object: {ask:?}"
    );
    assert_eq!(probe_replicas(), Some(1));

    let gone = day2(
        &sync.reader,
        deployments.id,
        Day2Call::Delete(DeleteRequest {
            namespace: Some(namespace()),
            name: PROBE.to_string(),
            grace_period_seconds: Some(0),
            confirm: true,
            caps: Caps::default(),
        }),
    );
    assert!(
        matches!(gone, Day2Outcome::Applied(_)),
        "the confirmed delete must apply: {gone:?}"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if probe_replicas().is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the throwaway deploy was still on the cluster after a confirmed delete");
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_reader_cannot_scale_and_a_configmap_is_not_a_scale_target() {
    let (_plane, sync) = connect(Some(&reader_context()));
    let deployments = kind(&sync.reader, "deployments.apps");
    let outcome = day2(&sync.reader, deployments.id, scale_call("web", 1, 2, true));
    match outcome {
        Day2Outcome::Denied { what, .. } => assert_eq!(what, "scale"),
        other => panic!("a reader scale must be Denied, not {other:?}"),
    }
    assert_eq!(
        deploy_replicas("web"),
        Some(1),
        "a denied scale must leave web at one replica"
    );

    let (_plane, admin) = connect(None);
    let configmaps = kind(&admin.reader, "configmaps");
    let outcome = day2(
        &admin.reader,
        configmaps.id,
        scale_call("settings", 1, 2, true),
    );
    match outcome {
        Day2Outcome::Failed { why } => {
            assert!(
                why.contains("ConfigMap") || why.contains("configmap") || why.contains("scale"),
                "scale of a ConfigMap must name why, not look like a write: {why}"
            );
        }
        other => panic!("scaling a ConfigMap must fail before the wire: {other:?}"),
    }
}

#[test]
#[ignore = "needs live_fixtures.sh --with observability"]
fn prometheus_reads_the_lab_namespace_and_grafana_serves_its_dashboards() {
    use k10s_data::integrations::grafana::QueryDialect;
    use k10s_data::reach::{ToolKind, ToolReach, Transport};

    let (_plane, sync) = connect(None);
    let namespace = namespace();
    let created = kube_runtime().block_on(async {
        let client = kube::Client::try_default().await.expect("a client");
        let namespaces: kube::Api<k8s_openapi::api::core::v1::Namespace> = kube::Api::all(client);
        namespaces
            .get(&namespace)
            .await
            .expect("the fixture namespace")
            .metadata
            .creation_timestamp
            .expect("the namespace creation time")
            .0
            .as_second()
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let end = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs_f64();
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.query_prometheus(
            format!("kube_namespace_created{{namespace={namespace:?}}}"),
            end - 60.0,
            end,
            "15s".into(),
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        let answer = wait(&rx);
        let Fetched::Ok(Some(result)) = answer else {
            panic!("Prometheus must answer through the provider's Reader: {answer:?}");
        };
        assert!(!result.truncated);
        assert_eq!(result.dropped_series, 0);
        if !result.series.is_empty() {
            for series in &result.series {
                assert!(
                    series
                        .labels
                        .contains(&("namespace".into(), namespace.clone()))
                );
                assert!(!series.points.is_empty());
                assert!(
                    series
                        .points
                        .iter()
                        .all(|(_, value)| *value == created as f64)
                );
            }
            println!("live kube-state-metrics series: {result:?}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no namespace metric was scraped"
        );
        std::thread::sleep(Duration::from_secs(2));
    }

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .bind_tool(ToolKind::Grafana, ReachSettings::default(), move |answer| {
            let _ = tx.send(answer);
        });
    let bound = wait(&rx);
    assert!(
        matches!(bound, ToolReach::Bound(ref ready) if matches!(ready.transport, Transport::Proxy { .. })),
        "Grafana must answer its health probe: {bound:?}"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_grafana_catalog(move |answer| {
        let _ = tx.send(answer);
    });
    let answer = wait(&rx);
    let Fetched::Ok(catalog) = answer else {
        panic!("the provisioned dashboards must be readable: {answer:?}");
    };
    assert!(catalog.served);
    assert!(
        catalog
            .dashboards
            .iter()
            .flat_map(|dashboard| &dashboard.panels)
            .flat_map(|panel| &panel.queries)
            .any(|query| query.dialect == QueryDialect::PromQL && query.expr.contains("kube_"))
    );
    println!(
        "live Grafana catalog: {} dashboards, {} extra titles, truncated={}",
        catalog.dashboards.len(),
        catalog.extra_hits.len(),
        catalog.truncated
    );
}

fn telemetry_probe_trace() -> (String, String) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{ListParams, LogParams};

    kube_runtime().block_on(async {
        let client = kube::Client::try_default()
            .await
            .expect("a client from KUBECONFIG");
        let pods: kube::Api<Pod> = kube::Api::namespaced(client, "observability");
        let page = pods
            .list(
                &ListParams::default()
                    .labels("app=k10s-telemetry-probe")
                    .fields("status.phase=Running"),
            )
            .await
            .expect("list the running telemetry producer");
        assert_eq!(
            page.items.len(),
            1,
            "one producer after the fixture rollout"
        );
        let pod = page.items[0]
            .metadata
            .name
            .clone()
            .expect("the pod has a name");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let logs = pods
                .logs(
                    &pod,
                    &LogParams {
                        container: Some("probe".to_string()),
                        tail_lines: Some(10),
                        ..LogParams::default()
                    },
                )
                .await
                .expect("read the producer's log");
            for line in logs.lines().rev() {
                let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if entry["message"] == "k10s telemetry probe"
                    && entry["namespace"] == "observability"
                    && entry["pod"] == pod
                    && let Some(trace_id) = entry["trace_id"].as_str()
                {
                    assert_eq!(trace_id.len(), 32);
                    assert!(trace_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
                    return (pod, trace_id.to_string());
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the producer never sent a trace; recent log: {logs}"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

#[test]
#[ignore = "needs live_fixtures.sh --with observability"]
fn tempo_returns_the_span_named_by_the_live_producer() {
    let (_plane, sync) = connect(None);
    let (pod, trace_id) = telemetry_probe_trace();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.lookup_trace(trace_id.clone(), move |answer| {
            let _ = tx.send(answer);
        });
        let answer = wait(&rx);
        if let Fetched::Ok(Some(trace)) = &answer {
            assert_eq!(trace.trace_id, trace_id);
            assert_eq!(trace.spans.len(), 1, "each emission is a fresh trace");
            let span = &trace.spans[0];
            assert_eq!(span.name, "k10s-live-probe");
            assert_eq!(span.service, "k10s-telemetry-probe");
            assert_eq!(span.duration_us, 1000);
            assert_eq!(span.status, "ok");
            assert!(span.parent.is_empty());
            println!("live trace from observability/{pod}: {trace:?}");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Tempo never returned trace {trace_id}: {answer:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
#[ignore = "needs live_fixtures.sh --with observability"]
fn loki_returns_the_producers_log_with_its_namespace_pod_and_trace_id() {
    let (_plane, sync) = connect(None);
    let (pod, trace_id) = telemetry_probe_trace();
    let start_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time after the epoch")
        .as_nanos() as u64
        - 300_000_000_000;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let end_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after the epoch")
            .as_nanos() as u64;
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.query_loki(
            k10s_data::integrations::loki::RangeQuery {
                query: format!("{{namespace=\"observability\",pod=\"{pod}\"}} |= \"{trace_id}\""),
                start_ns,
                end_ns,
                limit: 10,
            },
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        let answer = wait(&rx);
        if let Fetched::Ok(Some(logs)) = &answer {
            for stream in &logs.streams {
                for line in &stream.lines {
                    let entry: serde_json::Value = serde_json::from_str(&line.line)
                        .expect("the collector preserves the producer's JSON line");
                    if entry["trace_id"] == trace_id {
                        assert_eq!(entry["message"], "k10s telemetry probe");
                        assert_eq!(entry["namespace"], "observability");
                        assert_eq!(entry["pod"], pod);
                        assert!(
                            stream
                                .labels
                                .contains(&("namespace".to_string(), "observability".to_string()))
                        );
                        assert!(
                            stream
                                .labels
                                .iter()
                                .any(|(key, value)| key == "pod" && value == &pod)
                        );
                        assert!(!logs.truncated);
                        assert_eq!(
                            (logs.dropped_streams, logs.dropped_lines, logs.clipped_lines),
                            (0, 0, 0)
                        );
                        println!("live collected log: {line:?}");
                        return;
                    }
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Loki never returned the producer's trace marker {trace_id}: {answer:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn live_alertmanager(reader: &Reader) -> k10s_data::reach::Bound {
    let (tx, rx) = std::sync::mpsc::channel();
    reader.bind_tool(
        k10s_data::reach::ToolKind::Alertmanager,
        ReachSettings::default(),
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    match wait(&rx) {
        k10s_data::reach::ToolReach::Bound(bound) => {
            assert!(
                matches!(bound.transport, k10s_data::reach::Transport::Proxy { .. }),
                "the live write uses the service proxy"
            );
            bound
        }
        answer => panic!("the observability fixture must serve Alertmanager: {answer:?}"),
    }
}

fn await_live_alert(
    reader: &Reader,
    name: &str,
    what: &str,
    accept: impl Fn(&k10s_data::integrations::alertmanager::Alert) -> bool,
) -> k10s_data::integrations::alertmanager::Alert {
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        let (tx, rx) = std::sync::mpsc::channel();
        reader.fetch_alertmanager_alerts(move |answer| {
            let _ = tx.send(answer);
        });
        let answer = wait(&rx);
        if let Fetched::Ok(Some(alerts)) = &answer
            && let Some(alert) = alerts
                .items
                .iter()
                .find(|alert| alert.alertname == name && accept(alert))
        {
            return alert.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "alert {name} never became {what}: {answer:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
#[ignore = "needs live_fixtures.sh --with observability; fires and silences a real Prometheus rule"]
fn a_firing_live_alert_keeps_its_pod_join_and_is_silenced_by_exact_matchers() {
    use k10s_data::integrations::alertmanager::{self, Matcher, SilenceOutcome, SilenceSpec};
    use kube::api::{
        ApiResource, DeleteParams, DynamicObject, GroupVersionKind, PostParams, Preconditions,
    };
    use kube::runtime::wait::{await_condition, conditions};

    let (_plane, sync) = connect(None);
    let (pod, _) = telemetry_probe_trace();
    let bound = live_alertmanager(&sync.reader);
    let runtime = kube_runtime();
    let client = runtime
        .block_on(kube::Client::try_default())
        .expect("a client");
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "monitoring.coreos.com",
        "v1",
        "PrometheusRule",
    ));
    let rules: kube::Api<DynamicObject> =
        kube::Api::namespaced_with(client.clone(), "observability", &resource);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let alertname = format!("K10sLiveProbe{unique}");
    let rule: DynamicObject = serde_json::from_value(serde_json::json!({
        "apiVersion":"monitoring.coreos.com/v1", "kind":"PrometheusRule",
        "metadata":{"generateName":"k10s-alert-probe-", "namespace":"observability", "labels":{"release":"monitoring"}},
        "spec":{"groups":[{"name":"k10s.live", "rules":[{
            "alert":alertname, "expr":"vector(1)", "for":"0s",
            "labels":{"namespace":"observability", "pod":pod, "severity":"warning"},
            "annotations":{"summary":"k10s live alert and silence proof"}
        }]}]}
    })).expect("a rule");
    let created = runtime
        .block_on(rules.create(&PostParams::default(), &rule))
        .expect("create the live rule");
    let name = created.metadata.name.as_deref().expect("the rule name");
    let uid = created.metadata.uid.as_deref().expect("the rule UID");
    let comment = format!("k10s live silence for rule {uid}");
    let mut written_id = None;
    let mut expired = false;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let alert = await_live_alert(&sync.reader, &alertname, "active", |alert| {
            alert.state == "active"
        });
        assert_eq!(alert.namespace, "observability");
        assert_eq!(alert.pod, pod);
        assert!(alert.silenced_by.is_empty());
        println!("live firing alert from PrometheusRule observability/{name}: {alert:?}");
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let start = std::time::UNIX_EPOCH + Duration::from_secs(seconds);
        let spec = SilenceSpec::for_window(
            vec![
                Matcher {
                    name: "namespace".into(),
                    value: alert.namespace,
                    is_regex: false,
                    is_equal: true,
                },
                Matcher {
                    name: "pod".into(),
                    value: alert.pod,
                    is_regex: false,
                    is_equal: true,
                },
            ],
            start..start + Duration::from_secs(3600),
            "k10s".into(),
            comment.clone(),
        )
        .expect("a one-hour silence");
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader
            .create_silence(bound.clone(), spec.clone(), false, move |answer| {
                let _ = tx.send(answer);
            });
        assert!(matches!(wait(&rx), SilenceOutcome::NeedsConfirm { .. }));
        let Fetched::Ok(before) = runtime.block_on(alertmanager::fetch_silences(&client, &bound))
        else {
            panic!("read silences after review");
        };
        assert!(!before.truncated);
        assert_eq!(before.dropped, 0);
        assert!(
            before
                .items
                .iter()
                .all(|silence| silence.comment != comment),
            "review created no silence"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader
            .create_silence(bound.clone(), spec.clone(), true, move |answer| {
                let _ = tx.send(answer);
            });
        let answer = wait_interactive(&rx, 1);
        let SilenceOutcome::Applied { id, .. } = answer else {
            panic!("the confirmed silence must be written: {answer:?}");
        };
        written_id = Some(id.clone());
        let Fetched::Ok(after) = runtime.block_on(alertmanager::fetch_silences(&client, &bound))
        else {
            panic!("read the created silence");
        };
        let silence = after
            .items
            .iter()
            .find(|silence| silence.id == id)
            .expect("the returned ID is stored in Alertmanager");
        assert_eq!(silence.matchers, spec.matchers);
        assert_eq!(silence.matchers_dropped, 0);
        assert_eq!(silence.created_by, spec.created_by);
        assert_eq!(silence.comment, spec.comment);
        let timestamp = |value: &str| {
            value
                .parse::<k8s_openapi::jiff::Timestamp>()
                .expect("a UTC timestamp")
        };
        // Alertmanager moves a past start to its acceptance time. The reviewed
        // deadline stays exact; the silence cannot claim to cover the past.
        let stored_start = timestamp(&silence.starts_at);
        assert!(stored_start >= timestamp(&spec.starts_at));
        assert!(
            stored_start
                <= k8s_openapi::jiff::Timestamp::try_from(std::time::SystemTime::now())
                    .expect("the lab clock")
        );
        assert_eq!(timestamp(&silence.ends_at), timestamp(&spec.ends_at));
        println!("live silence read back by ID: {silence:?}");
        let suppressed = await_live_alert(
            &sync.reader,
            &alertname,
            "silenced by the returned ID",
            |alert| alert.silenced_by.contains(&id),
        );
        assert_eq!(suppressed.state, "suppressed");
        assert_eq!(suppressed.namespace, "observability");
        assert_eq!(suppressed.pod, pod);
        assert!(matches!(
            runtime.block_on(alertmanager::expire_silence(&client, &bound, &id, true)),
            SilenceOutcome::Applied { .. }
        ));
        expired = true;
        let resumed = await_live_alert(&sync.reader, &alertname, "active after expiry", |alert| {
            alert.state == "active" && !alert.silenced_by.contains(&id)
        });
        println!("live alert after expiring the test silence: {resumed:?}");
    }));

    // Clean up before propagating an assertion failure. The unique rule comment
    // also finds a write whose response was lost before its ID reached us.
    let silence_cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            if !expired {
                let Fetched::Ok(silences) = alertmanager::fetch_silences(&client, &bound).await
                else {
                    panic!("read silences for cleanup");
                };
                assert!(
                    !silences.truncated,
                    "cleanup must see the complete silence list"
                );
                for silence in silences.items.iter().filter(|silence| {
                    silence.comment == comment || written_id.as_ref() == Some(&silence.id)
                }) {
                    let answer =
                        alertmanager::expire_silence(&client, &bound, &silence.id, true).await;
                    assert!(
                        matches!(answer, SilenceOutcome::Applied { .. }),
                        "expire only the test silence: {answer:?}"
                    );
                }
            }
        });
    }));
    runtime.block_on(async {
        rules
            .delete(
                name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(uid.to_string()),
                        resource_version: None,
                    }),
                    ..DeleteParams::default()
                },
            )
            .await
            .expect("delete the test rule by UID");
        tokio::time::timeout(
            Duration::from_secs(30),
            await_condition(rules, name, conditions::is_deleted(uid)),
        )
        .await
        .expect("rule cleanup within the budget")
        .expect("rule deletion observed");
    });
    if let Err(panic) = outcome.and(silence_cleanup) {
        std::panic::resume_unwind(panic);
    }
}

#[test]
#[ignore = "needs the observability fixture and reader@k10s-lab"]
fn a_read_only_account_cannot_create_a_live_alertmanager_silence() {
    use k10s_data::integrations::alertmanager::{self, Matcher, SilenceOutcome, SilenceSpec};
    let (_admin_plane, admin) = connect(None);
    let (pod, _) = telemetry_probe_trace();
    let admin_bound = live_alertmanager(&admin.reader);
    let context = reader_context();
    let (_reader_plane, restricted) = connect(Some(&context));
    let bound = live_alertmanager(&restricted.reader);
    assert_eq!(bound.transport, admin_bound.transport);
    let now = std::time::SystemTime::now();
    let comment = format!(
        "k10s denied silence proof {}",
        now.duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let spec = SilenceSpec::for_window(
        vec![
            Matcher {
                name: "namespace".into(),
                value: "observability".into(),
                is_regex: false,
                is_equal: true,
            },
            Matcher {
                name: "pod".into(),
                value: pod,
                is_regex: false,
                is_equal: true,
            },
        ],
        now..now + Duration::from_secs(300),
        "k10s".into(),
        comment.clone(),
    )
    .expect("a valid silence");
    let (tx, rx) = std::sync::mpsc::channel();
    restricted
        .reader
        .create_silence(bound, spec, true, move |answer| {
            let _ = tx.send(answer);
        });
    let answer = wait_interactive(&rx, 1);
    let stored = kube_runtime().block_on(async {
        let client = kube::Client::try_default().await.expect("an admin client");
        let Fetched::Ok(silences) = alertmanager::fetch_silences(&client, &admin_bound).await
        else {
            panic!("read back silences as admin");
        };
        assert!(!silences.truncated);
        let ours: Vec<_> = silences
            .items
            .into_iter()
            .filter(|silence| silence.comment == comment)
            .collect();
        // If the lab role became writable, remove the unexpected write before
        // reporting that its denial fixture is no longer a denial fixture.
        for silence in &ours {
            assert!(matches!(
                alertmanager::expire_silence(&client, &admin_bound, &silence.id, true).await,
                SilenceOutcome::Applied { .. }
            ));
        }
        ours
    });
    assert!(stored.is_empty(), "a read-only account created a silence");
    let SilenceOutcome::Denied { what, why } = &answer else {
        panic!("the restricted account's silence was not denied: {answer:?}");
    };
    assert_eq!(*what, "alertmanager");
    // The chosen context's username may differ; its server denial must survive.
    assert!(why.contains("is forbidden: User \""), "{why}");
    assert!(
        why.contains("cannot create resource \"services/proxy\""),
        "{why}"
    );
    println!("live silence denial in {context}: {answer:?}");
}

const DRAIN_FIXTURE_BUDGET: Duration = Duration::from_secs(60);

async fn drain_before(
    deadline: tokio::time::Instant,
    request: impl std::future::Future<Output = Result<serde_json::Value, kube::Error>>,
) -> Result<serde_json::Value, kube::Error> {
    let expired = || {
        kube::Error::Service(Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the drain fixture exceeded its 60 s deadline",
        )))
    };
    if tokio::time::Instant::now() >= deadline {
        return Err(expired());
    }
    tokio::time::timeout_at(deadline, request)
        .await
        .map_err(|_| expired())?
}

fn drain_request(
    request: http::Request<Vec<u8>>,
    deadline: std::time::Instant,
) -> Result<serde_json::Value, kube::Error> {
    kube_runtime().block_on(drain_before(deadline.into(), async {
        let client = kube::Client::try_default().await?;
        client.request(request).await
    }))
}

fn drain_get(
    path: &str,
    name: &str,
    deadline: std::time::Instant,
) -> Result<serde_json::Value, kube::Error> {
    let request = kube::api::Request::new(path)
        .get(name, &kube::api::GetParams::default())
        .expect("a get request");
    drain_request(request, deadline)
}

fn drain_delete(
    path: &str,
    name: &str,
    uid: &str,
    deadline: std::time::Instant,
) -> Result<serde_json::Value, kube::Error> {
    let request = kube::api::Request::new(path)
        .delete(
            name,
            &kube::api::DeleteParams {
                grace_period_seconds: Some(0),
                preconditions: Some(kube::api::Preconditions {
                    uid: Some(uid.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .expect("a delete request");
    drain_request(request, deadline)
}

fn drain_pause(deadline: std::time::Instant) {
    std::thread::sleep(
        deadline
            .saturating_duration_since(std::time::Instant::now())
            .min(Duration::from_secs(1)),
    );
}

// Only successful root creates confer ownership. Namespaced objects are removed
// through that namespace, and every forced pod deletion pins the observed UID.
struct DrainProbe {
    name: String,
    node_uid: Option<String>,
    namespace_uid: Option<String>,
}

impl DrainProbe {
    fn empty(name: &str) -> Self {
        Self {
            name: name.to_string(),
            node_uid: None,
            namespace_uid: None,
        }
    }

    fn objects(&self) -> [(String, serde_json::Value); 4] {
        let name = &self.name;
        [
            (
                "/api/v1/nodes".to_string(),
                serde_json::json!({
                    "apiVersion": "v1", "kind": "Node",
                    "metadata": {"name": name,
                        "annotations": {"kwok.x-k8s.io/node": "fake"},
                        "labels": {"type": "kwok", "kubernetes.io/hostname": name}},
                    "spec": {"taints": [{"key": "kwok.x-k8s.io/node", "value": "fake", "effect": "NoExecute"}]},
                }),
            ),
            (
                "/api/v1/namespaces".to_string(),
                serde_json::json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name}}),
            ),
            (
                format!("/apis/apps/v1/namespaces/{name}/deployments"),
                serde_json::json!({
                    "apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": {"name": name},
                    "spec": {"replicas": 1,
                        "selector": {"matchLabels": {"app": name}},
                        "template": {"metadata": {"labels": {"app": name}},
                            "spec": {"nodeSelector": {"kubernetes.io/hostname": name},
                                "tolerations": [{"key": "kwok.x-k8s.io/node", "operator": "Exists", "effect": "NoExecute"}],
                                "containers": [{"name": "fake", "image": "fake-image"}]}}},
                }),
            ),
            (
                format!("/apis/policy/v1/namespaces/{name}/poddisruptionbudgets"),
                serde_json::json!({
                    "apiVersion": "policy/v1", "kind": "PodDisruptionBudget",
                    "metadata": {"name": name},
                    "spec": {"minAvailable": 1, "selector": {"matchLabels": {"app": name}}},
                }),
            ),
        ]
    }

    fn create_object(
        &mut self,
        path: &str,
        object: serde_json::Value,
        deadline: std::time::Instant,
    ) -> Result<(), kube::Error> {
        let request = kube::api::Request::new(path)
            .create(
                &kube::api::PostParams::default(),
                serde_json::to_vec(&object).expect("JSON"),
            )
            .expect("a create request");
        let created = drain_request(request, deadline)?;
        let uid = created["metadata"]["uid"]
            .as_str()
            .expect("a created object's UID")
            .to_string();
        match path {
            "/api/v1/nodes" => self.node_uid = Some(uid),
            "/api/v1/namespaces" => self.namespace_uid = Some(uid),
            _ => {}
        }
        Ok(())
    }

    fn create() -> Self {
        Self::try_create(&drain_probe_name("live"))
            .unwrap_or_else(|error| panic!("creating the drain probe: {}", said(&error)))
    }

    fn try_create(name: &str) -> Result<Self, kube::Error> {
        let mut probe = Self::empty(name);
        let deadline = std::time::Instant::now() + DRAIN_FIXTURE_BUDGET;
        for (path, object) in probe.objects() {
            probe.create_object(&path, object, deadline)?;
        }
        let budget = format!("/apis/policy/v1/namespaces/{name}/poddisruptionbudgets");
        loop {
            let pdb = drain_get(&budget, name, deadline)?;
            if pdb["status"]["currentHealthy"] == 1 && pdb["status"]["disruptionsAllowed"] == 0 {
                return Ok(probe);
            }
            drain_pause(deadline);
        }
    }

    fn cleanup(&mut self) -> Result<(), String> {
        let deadline = std::time::Instant::now() + DRAIN_FIXTURE_BUDGET;
        let mut namespace_uid = self.namespace_uid.take();
        let mut node_uid = self.node_uid.take();
        for (path, uid) in [
            ("/api/v1/namespaces", namespace_uid.as_deref()),
            ("/api/v1/nodes", node_uid.as_deref()),
        ] {
            if let Some(uid) = uid
                && let Err(error) = drain_delete(path, &self.name, uid, deadline)
                && !matches!(&error, kube::Error::Api(status) if status.code == 404 || status.code == 409)
            {
                eprintln!("deleting {path}/{}: {}", self.name, said(&error));
            }
        }
        while namespace_uid.is_some() || node_uid.is_some() {
            for (path, owned) in [
                ("/api/v1/namespaces", &mut namespace_uid),
                ("/api/v1/nodes", &mut node_uid),
            ] {
                let Some(uid) = owned.as_deref() else {
                    continue;
                };
                match drain_get(path, &self.name, deadline) {
                    Ok(object) if object["metadata"]["uid"].as_str() == Some(uid) => {}
                    Ok(_) => {
                        *owned = None;
                        continue;
                    }
                    Err(kube::Error::Api(status)) if status.code == 404 => {
                        *owned = None;
                        continue;
                    }
                    Err(error) => {
                        return Err(format!(
                            "waiting for {path}/{}: {}",
                            self.name,
                            said(&error)
                        ));
                    }
                }
                if path == "/api/v1/namespaces" {
                    self.remove_fake_pods(uid, deadline)?;
                }
            }
            if namespace_uid.is_some() || node_uid.is_some() {
                drain_pause(deadline);
            }
        }
        Ok(())
    }

    fn remove_fake_pods(
        &self,
        namespace_uid: &str,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        let path = format!("/api/v1/namespaces/{}/pods", self.name);
        let request = kube::api::Request::new(&path)
            .list(&kube::api::ListParams::default())
            .expect("a pod list request");
        let pods = match drain_request(request, deadline) {
            Ok(pods) => pods,
            Err(kube::Error::Api(status)) if status.code == 404 => return Ok(()),
            Err(error) => return Err(format!("listing the drain probe's pods: {}", said(&error))),
        };
        // Check after the list: a replacement namespace must never lend us its
        // pods. Individual UID preconditions also cover replacements after this read.
        match drain_get("/api/v1/namespaces", &self.name, deadline) {
            Ok(namespace) if namespace["metadata"]["uid"].as_str() == Some(namespace_uid) => {}
            Ok(_) => return Ok(()),
            Err(kube::Error::Api(status)) if status.code == 404 => return Ok(()),
            Err(error) => return Err(format!("checking the drain namespace: {}", said(&error))),
        }
        for pod in pods["items"].as_array().expect("a pod list") {
            let name = pod["metadata"]["name"].as_str().expect("a pod name");
            let uid = pod["metadata"]["uid"].as_str().expect("a pod UID");
            match drain_delete(&path, name, uid, deadline) {
                Ok(_) => {}
                Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => {}
                Err(error) => {
                    return Err(format!(
                        "deleting the drain probe's pod {name}: {}",
                        said(&error)
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Drop for DrainProbe {
    fn drop(&mut self) {
        if let Err(message) = self.cleanup() {
            if std::thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

fn drain_probe_name(case: &str) -> String {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("k10s-drain-{case}-{suffix}")
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_drain_fixture_preserves_a_colliding_node() {
    let mut existing = DrainProbe::empty(&drain_probe_name("node"));
    let (path, object) = existing.objects().into_iter().next().expect("the node");
    let deadline = std::time::Instant::now() + DRAIN_FIXTURE_BUDGET;
    existing
        .create_object(&path, object, deadline)
        .expect("the existing node");
    let error = DrainProbe::try_create(&existing.name)
        .err()
        .expect("the name collides");
    assert!(matches!(error, kube::Error::Api(status) if status.code == 409));
    let node = drain_get(&path, &existing.name, deadline).expect("the original node remains");
    assert_eq!(
        node["metadata"]["uid"].as_str(),
        existing.node_uid.as_deref()
    );
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_drain_fixture_removes_only_its_partial_creation() {
    let mut existing = DrainProbe::empty(&drain_probe_name("partial"));
    let (path, object) = existing
        .objects()
        .into_iter()
        .nth(1)
        .expect("the namespace");
    let deadline = std::time::Instant::now() + DRAIN_FIXTURE_BUDGET;
    existing
        .create_object(&path, object, deadline)
        .expect("the existing namespace");
    let error = DrainProbe::try_create(&existing.name)
        .err()
        .expect("the namespace collides");
    assert!(matches!(error, kube::Error::Api(status) if status.code == 409));
    let namespace =
        drain_get(&path, &existing.name, deadline).expect("the original namespace remains");
    assert_eq!(
        namespace["metadata"]["uid"].as_str(),
        existing.namespace_uid.as_deref()
    );
    assert!(
        matches!(drain_get("/api/v1/nodes", &existing.name, deadline),
        Err(kube::Error::Api(status)) if status.code == 404),
        "the partially created node was removed"
    );
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn a_drain_fixture_preserves_replacement_roots() {
    let mut original = DrainProbe::empty(&drain_probe_name("replacement"));
    let deadline = std::time::Instant::now() + DRAIN_FIXTURE_BUDGET;
    for (path, object) in original.objects().into_iter().take(2) {
        original
            .create_object(&path, object, deadline)
            .expect("the original roots");
    }
    let mut remover = DrainProbe {
        name: original.name.clone(),
        node_uid: original.node_uid.clone(),
        namespace_uid: original.namespace_uid.clone(),
    };
    remover
        .cleanup()
        .expect("remove the original roots before replacing them");
    let mut replacement = DrainProbe::empty(&original.name);
    let deadline = std::time::Instant::now() + DRAIN_FIXTURE_BUDGET;
    for (path, object) in replacement.objects().into_iter().take(2) {
        replacement
            .create_object(&path, object, deadline)
            .expect("the replacement roots");
    }
    let pods = format!("/api/v1/namespaces/{}/pods", replacement.name);
    replacement
        .create_object(
            &pods,
            serde_json::json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": "replacement"},
                "spec": {"nodeName": replacement.name,
                    "tolerations": [{"key": "kwok.x-k8s.io/node", "operator": "Exists", "effect": "NoExecute"}],
                    "containers": [{"name": "fake", "image": "fake-image"}]},
            }),
            deadline,
        )
        .expect("a pod in the replacement namespace");
    let pod = drain_get(&pods, "replacement", deadline).expect("the replacement pod");
    assert_ne!(original.node_uid, replacement.node_uid);
    assert_ne!(original.namespace_uid, replacement.namespace_uid);
    original
        .cleanup()
        .expect("old ownership releases replacement roots");
    for (path, uid) in [
        ("/api/v1/nodes", replacement.node_uid.as_deref()),
        ("/api/v1/namespaces", replacement.namespace_uid.as_deref()),
    ] {
        let object = drain_get(path, &replacement.name, deadline).expect("the replacement remains");
        assert_eq!(object["metadata"]["uid"].as_str(), uid);
    }
    let remaining = drain_get(&pods, "replacement", deadline).expect("the replacement pod remains");
    assert_eq!(remaining["metadata"]["uid"], pod["metadata"]["uid"]);
}

#[tokio::test(start_paused = true)]
async fn drain_fixture_deadline_includes_an_unanswered_request() {
    use kube::client::Body;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let service = tower::service_fn(move |_: http::Request<Body>| {
        let attempt = seen.fetch_add(1, Ordering::Relaxed);
        async move {
            if attempt == 0 {
                tokio::time::sleep(Duration::from_secs(20)).await;
                Ok::<_, std::convert::Infallible>(http::Response::new(Body::from(b"{}".to_vec())))
            } else {
                std::future::pending().await
            }
        }
    });
    let client = kube::Client::new(service, "default");
    let started = tokio::time::Instant::now();
    let deadline = started + DRAIN_FIXTURE_BUDGET;
    for attempt in 0..3 {
        let request = kube::api::Request::new("/api/v1/nodes")
            .get("k10s-drain-unanswered", &kube::api::GetParams::default())
            .expect("a get request");
        let result = drain_before(deadline, client.request(request)).await;
        if attempt == 0 {
            result.expect("the first request answers after 20 seconds");
        } else {
            assert!(matches!(result, Err(kube::Error::Service(error))
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)));
        }
    }
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(started.elapsed(), DRAIN_FIXTURE_BUDGET);
}

// A failed admin request in the server's words and code, rather than kube's
// rendering of the whole Status.
fn said(error: &kube::Error) -> String {
    match error {
        kube::Error::Api(status) => format!("{} ({})", status.message, status.code),
        other => other.to_string(),
    }
}

// A drain meeting a budget used to have each refused eviction resent by the
// client's retry: sixteen POSTs over six minutes for one pod on this lab. The
// drain must now name the budget in the server's words within five seconds.
// The scripted and socket tests count requests; this fixture measures latency.
#[test]
#[ignore = "needs a live cluster with kwok; see the module comment"]
fn a_drain_names_the_disruption_budget_promptly() {
    use k10s_data::day2::DrainRequest;
    let (_plane, sync) = connect(None);
    let probe = DrainProbe::create();
    let name = &probe.name;
    let started = std::time::Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.day2(
        KindId::NODE,
        Day2Call::Drain(DrainRequest {
            name: name.clone(),
            force: false,
            confirm: true,
            caps: Caps::default(),
        }),
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    // One pod list, one cordon and the fixture's single eviction.
    let outcome = wait_interactive(&rx, 3);
    let took = started.elapsed();
    println!("live drain answered in {took:?}");
    let Day2Outcome::Applied(applied) = outcome else {
        panic!("the drain reports: {outcome:?}");
    };
    assert_eq!(
        applied.summary,
        format!(
            "cordoned {name}; evicted 0 pods; 1 blocked by a PodDisruptionBudget (The \
             disruption budget {name} needs 1 healthy pods and has 1 currently)"
        )
    );
    assert!(
        took < Duration::from_secs(5),
        "one eviction, answered: {took:?}"
    );
    let request = kube::api::Request::new("/api/v1/nodes")
        .get(name, &kube::api::GetParams::default())
        .expect("a get request");
    let node = admin_request(request).expect("the node");
    assert_eq!(node["spec"]["unschedulable"], true, "the node is cordoned");
}

#[test]
#[ignore = "needs the owned k10s-observe-pilot Collector and producer fixture"]
fn a_service_forward_reads_the_pilot_through_a_named_prometheus_url() {
    use k10s_data::forward::{ForwardRegistry, ForwardRequest};
    use k10s_data::integrations::prom;
    use k10s_data::reach::{Bound, ToolAuth, ToolKind, Transport};
    struct OwnedForward(ForwardRegistry, u64);
    impl Drop for OwnedForward {
        fn drop(&mut self) {
            self.0.close(self.1);
        }
    }
    let (_plane, sync) = connect(None);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.open_forward(
        ForwardRequest {
            namespace: "observability".into(),
            name: "monitoring-kube-prometheus-prometheus".into(),
            service: true,
        },
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    let row = match wait(&rx) {
        Fetched::Ok(row) => row,
        other => panic!("the actual Prometheus Service forward: {other:?}"),
    };
    let _forward = OwnedForward(sync.reader.forwards().clone(), row.id);
    assert_eq!(row.spec.remote_port, 9090);
    let bound = Bound {
        kind: ToolKind::Prometheus,
        found: None,
        transport: Transport::Url {
            base: format!("http://127.0.0.1:{}", row.spec.local_port),
        },
        auth: ToolAuth::Anonymous,
    };
    let end = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_secs_f64();
    let result = kube_runtime().block_on(async {
        let client = kube::Client::try_default().await.expect("the live client");
        prom::query_range(
            &client,
            &bound,
            "k10s_pilot_requests_total{namespace=\"k10s-observe-pilot\"}",
            end - 300.0,
            end,
            "15s",
        )
        .await
    });
    let result = match result {
        Fetched::Ok(result) => result,
        other => panic!("the named URL range read through the kubelet: {other:?}"),
    };
    assert!(result.series.iter().any(|series| {
        series
            .points
            .iter()
            .any(|(_, value)| value.is_finite() && *value > 0.0)
    }));
    assert!(!result.truncated);
    println!(
        "named URL read through {} / {}: {} series",
        row.spec.namespace,
        row.spec.pod,
        result.series.len()
    );
}

#[test]
#[ignore = "needs the owned k10s-observe-pilot namespace; creates only a zero-replica probe"]
fn zero_replica_scope_and_recreation_use_real_metadata_and_controller_uids() {
    use k8s_openapi::api::{apps::v1::Deployment, core::v1::Namespace};
    use kube::api::{DeleteParams, PostParams, Preconditions};
    const NS: &str = "k10s-observe-pilot";
    struct OwnedDeployment<'a> {
        runtime: &'a tokio::runtime::Runtime,
        api: kube::Api<Deployment>,
        name: String,
        uid: Option<String>,
    }
    impl OwnedDeployment<'_> {
        fn remove(&mut self) -> Result<(), String> {
            let Some(uid) = &self.uid else {
                return Ok(());
            };
            let params = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(uid.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            self.runtime.block_on(async {
                self.api
                    .delete(&self.name, &params)
                    .await
                    .map_err(|error| said(&error))?;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let current = self
                            .api
                            .get_metadata_opt(&self.name)
                            .await
                            .map_err(|error| said(&error))?;
                        if current.as_ref().and_then(|item| item.metadata.uid.as_ref()) != Some(uid)
                        {
                            return Ok::<(), String>(());
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await
                .map_err(|_| format!("deletion of {} UID {uid} did not finish in 10s", self.name))?
            })?;
            self.uid = None;
            Ok(())
        }
    }
    impl Drop for OwnedDeployment<'_> {
        fn drop(&mut self) {
            if let Err(error) = self.remove() {
                eprintln!("owned scope probe cleanup refused: {error}");
            }
        }
    }
    let runtime = kube_runtime();
    let (api, item) = runtime.block_on(async {
        let client = kube::Client::try_default().await.expect("the admin client");
        let namespaces: kube::Api<Namespace> = kube::Api::all(client.clone());
        let namespace = namespaces.get_metadata(NS).await.expect("the owned namespace");
        assert_eq!(
            namespace.metadata.labels.as_ref().and_then(|labels| labels.get("k10s.dev/fixture")).map(String::as_str),
            Some("observe-pilot")
        );
        let api: kube::Api<Deployment> = kube::Api::namespaced(client, NS);
        let item: Deployment = serde_json::from_value(serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "generateName": "scope-zero-",
                "labels": {"k10s.dev/fixture": "observe-pilot"}
            },
            "spec": {
                "replicas": 0,
                "selector": {"matchLabels": {"k10s.dev/probe": "scope-zero"}},
                "template": {
                    "metadata": {"labels": {"k10s.dev/probe": "scope-zero"}},
                    "spec": {
                        "automountServiceAccountToken": false,
                        "containers": [{"name": "never-started", "image": "python:3.14.7-alpine3.24"}]
                    }
                }
            }
        })).expect("the zero-replica fixture");
        let created = api.create(&PostParams::default(), &item).await.expect("create only the probe");
        (api, created)
    });
    let name = item.metadata.name.clone().expect("generated name");
    let uid = item.metadata.uid.clone().expect("created UID");
    let mut guard = OwnedDeployment {
        runtime: &runtime,
        api,
        name: name.clone(),
        uid: Some(uid.clone()),
    };
    println!("owned zero-replica probe {NS}/{} UID={uid}", guard.name);
    let (_plane, sync) = connect(None);
    let request = DescribeRequest {
        kind: KindId::DEPLOYMENT,
        namespace: Some(NS.into()),
        name,
        uid: uid.clone(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync
        .reader
        .resolve_observe_scope(request.clone(), move |answer| {
            let _ = tx.send(answer);
        });
    let scope = match wait(&rx) {
        Fetched::Ok(scope) => scope,
        other => panic!("zero replicas are a complete empty scope: {other:?}"),
    };
    assert!(scope.pods.is_empty());
    assert_eq!(scope.resource_uid, uid);
    guard.remove().expect("delete only the original probe UID");
    let mut replacement = item;
    replacement.metadata.uid = None;
    replacement.metadata.resource_version = None;
    replacement.metadata.creation_timestamp = None;
    replacement.metadata.managed_fields = None;
    replacement.metadata.generate_name = None;
    replacement.status = None;
    let replacement = runtime
        .block_on(guard.api.create(&PostParams::default(), &replacement))
        .expect("recreate the same zero-replica name");
    let next_uid = replacement.metadata.uid.expect("replacement UID");
    guard.uid = Some(next_uid.clone());
    assert_ne!(uid, next_uid);
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.resolve_observe_scope(request, move |answer| {
        let _ = tx.send(answer);
    });
    assert!(matches!(wait(&rx),Fetched::Failed {why,..} if why.contains("recreated")));
    guard
        .remove()
        .expect("delete only the replacement probe UID");
    println!("same-name recreation refused old UID {uid}, current UID {next_uid}");
}

#[test]
#[ignore = "needs the owned k10s-observe-pilot Collector and producer fixture"]
fn restricted_accounts_read_current_pilot_metrics_and_grafana_panels() {
    use k8s_openapi::api::core::v1::Pod;
    use k10s_data::integrations::grafana;
    use kube::api::ListParams;
    const NS: &str = "k10s-observe-pilot";
    let (pod, uid) = kube_runtime().block_on(async {
        let client = kube::Client::try_default().await.expect("the admin client");
        let pods: kube::Api<Pod> = kube::Api::namespaced(client, NS);
        let page = pods
            .list_metadata(
                &ListParams::default()
                    .labels("app.kubernetes.io/name=pilot-producer")
                    .limit(2),
            )
            .await
            .expect("owned producer metadata");
        assert_eq!(page.items.len(), 1);
        let item = &page.items[0];
        (
            item.metadata.name.clone().expect("pod name"),
            item.metadata.uid.clone().expect("pod UID"),
        )
    });
    let nometrics = std::env::var("K10S_LIVE_NOMETRICS_CONTEXT")
        .unwrap_or_else(|_| "nometrics@k10s-lab".into());
    for context in [reader_context(), nometrics] {
        let (_plane, sync) = connect(Some(&context));
        let (tx, rx) = std::sync::mpsc::channel();
        let _scope_stop = sync.reader.resolve_observe_scope(
            DescribeRequest {
                kind: KindId::POD,
                namespace: Some(NS.into()),
                name: pod.clone(),
                uid: uid.clone(),
            },
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        let scope = match wait(&rx) {
            Fetched::Ok(scope) => scope,
            other => panic!("{context} selected identity: {other:?}"),
        };
        assert_eq!(scope.pods.len(), 1);
        assert_eq!(scope.pods[0].uid, uid);
        assert!(!scope.pods[0].node.is_empty());
        let end_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_millis() as i64;
        let start_ms = end_ms - 300_000;
        let expr =
            format!("k10s_pilot_requests_total{{namespace=\"{NS}\",pod=\"{pod}\",uid=\"{uid}\"}}");
        let (tx, rx) = std::sync::mpsc::channel();
        let _query_stop = sync.reader.observe_prometheus(
            expr,
            start_ms as f64 / 1000.0,
            end_ms as f64 / 1000.0,
            "15s".into(),
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        let result = match wait(&rx) {
            Fetched::Ok(Some(result)) => result,
            other => panic!("{context} metric read: {other:?}"),
        };
        assert_eq!(result.series.len(), 1);
        assert!(
            result.series[0]
                .labels
                .contains(&("uid".into(), uid.clone()))
        );
        assert!(
            result.series[0]
                .points
                .iter()
                .any(|(_, value)| value.is_finite() && *value > 0.0)
        );
        assert!(!result.truncated);
        let (tx, rx) = std::sync::mpsc::channel();
        let _catalog_stop = sync.reader.observe_grafana(move |answer| {
            let _ = tx.send(answer);
        });
        let catalog = match wait(&rx) {
            Fetched::Ok(catalog) => catalog,
            other => panic!("{context} dashboard read: {other:?}"),
        };
        let dashboard = catalog
            .dashboards
            .iter()
            .find(|dashboard| dashboard.uid == "k10s-observe-pilot-native")
            .expect("the owned dashboard");
        assert_eq!(dashboard.panels.len(), 2);
        for panel in &dashboard.panels {
            assert!(panel.unsupported.is_none());
            let target = panel.queries.first().expect("a supported target").clone();
            let variables = grafana::Variables {
                namespace: NS.into(),
                pods: vec![pod.clone()],
                node: Some(scope.pods[0].node.clone()),
                start_ms,
                end_ms,
                step_ms: 15_000,
                min_interval: panel.min_interval.clone(),
            };
            let (tx, rx) = std::sync::mpsc::channel();
            let _panel_stop = sync.reader.observe_panel(target, variables, move |answer| {
                let _ = tx.send(answer);
            });
            let result = match wait(&rx) {
                Fetched::Ok(Some(result)) => result,
                other => panic!("{context} panel {}: {other:?}", panel.title),
            };
            assert!(result.source.contains("datasource prometheus"));
            assert!(result.series.iter().any(|series| {
                series
                    .points
                    .iter()
                    .any(|(_, value)| value.is_finite() && *value > 0.0)
            }));
            assert!(!result.truncated);
            println!("{context} read panel {}: {}", panel.title, result.source);
        }
        assert_eq!(sync.reader.observe_schedule_counts().rejected, 0);
    }
}

#[test]
#[ignore = "needs the owned k10s-observe-pilot Collector and producer fixture"]
fn collector_pipeline_links_current_workload_metrics_logs_and_trace() {
    use k8s_openapi::api::{apps::v1::Deployment, core::v1::Pod};
    use k10s_data::integrations::{grafana, loki, otel};
    use kube::api::{ListParams, LogParams};
    const NS: &str = "k10s-observe-pilot";
    let (pod, uid, node, deployment_uid, marker) = kube_runtime().block_on(async {
        let client = kube::Client::try_default().await.expect("the live client");
        let pods: kube::Api<Pod> = kube::Api::namespaced(client.clone(), NS);
        let page = pods
            .list(
                &ListParams::default()
                    .labels("app.kubernetes.io/name=pilot-producer")
                    .limit(2),
            )
            .await
            .expect("owned producer discovery");
        assert_eq!(page.items.len(), 1);
        let item = &page.items[0];
        let pod = item.metadata.name.clone().expect("pod name");
        let uid = item.metadata.uid.clone().expect("pod UID");
        let node = item
            .spec
            .as_ref()
            .and_then(|spec| spec.node_name.clone())
            .expect("scheduled node");
        let logs = pods
            .logs(
                &pod,
                &LogParams {
                    container: Some("producer".into()),
                    tail_lines: Some(4),
                    ..Default::default()
                },
            )
            .await
            .expect("producer receipt");
        let marker = logs
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|marker| marker["event"] == "pilot.complete")
            .expect("an actual Collector emission");
        assert_eq!(marker["namespace"], NS);
        assert_eq!(marker["pod"], pod);
        assert_eq!(marker["uid"], uid);
        let deployments: kube::Api<Deployment> = kube::Api::namespaced(client, NS);
        let deployment_uid = deployments
            .get("pilot-producer")
            .await
            .expect("owned deployment")
            .metadata
            .uid
            .expect("deployment UID");
        (pod, uid, node, deployment_uid, marker)
    });
    let (_plane, sync) = connect(None);
    let request = DescribeRequest {
        kind: KindId::DEPLOYMENT,
        namespace: Some(NS.into()),
        name: "pilot-producer".into(),
        uid: deployment_uid,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.resolve_observe_scope(request, move |answer| {
        let _ = tx.send(answer);
    });
    let scope = match wait(&rx) {
        Fetched::Ok(scope) => scope,
        other => panic!("owned scope: {other:?}"),
    };
    assert_eq!(scope.pods.len(), 1);
    assert_eq!(scope.pods[0].name, pod);
    assert_eq!(scope.pods[0].uid, uid);
    let trace_id = marker["trace_id"]
        .as_str()
        .expect("emitted trace ID")
        .to_string();
    let emitted_count = marker["count"].as_u64().expect("counter receipt");
    assert_eq!(trace_id.len(), 32);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let expr =
        format!("k10s_pilot_requests_total{{namespace=\"{NS}\",pod=\"{pod}\",uid=\"{uid}\"}}");
    let mut attempts = 0;
    let (start_ms, end_ms) = loop {
        let end_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_millis() as i64;
        let start_ms = end_ms - 300_000;
        let (tx, rx) = std::sync::mpsc::channel();
        let _stop = sync.reader.observe_prometheus(
            expr.clone(),
            start_ms as f64 / 1000.0,
            end_ms as f64 / 1000.0,
            "15s".into(),
            move |answer| {
                let _ = tx.send((answer, k10s_data::telemetry::request_metrics()));
            },
        );
        let (answer, metrics) = wait(&rx);
        let result = match answer {
            Fetched::Ok(Some(result)) => result,
            other => {
                panic!("Collector metric query must succeed while awaiting its scrape: {other:?}")
            }
        };
        let metrics = metrics.expect("admitted request counters");
        if attempts == 0 {
            assert_eq!(metrics.requests, 3, "the first source binding is cold");
        } else {
            assert!(
                matches!(metrics.requests, 2 | 4),
                "a verified source hit has two requests; an expired snapshot adds discovery and probe: {metrics:?}"
            );
        }
        attempts += 1;
        assert!(metrics.bytes > 0);
        assert!(!result.truncated);
        println!("pilot metric polling attempt={attempts}, request={metrics:?}");
        if result.series.iter().any(|series| {
            series
                .points
                .iter()
                .any(|(_, value)| *value >= emitted_count as f64)
        }) {
            assert_eq!(result.series.len(), 1);
            assert!(
                result.series[0]
                    .labels
                    .iter()
                    .any(|(name, value)| name == "uid" && value == &uid)
            );
            println!(
                "pilot metric source={}, samples={}, counter>={emitted_count}, network/decode={:?}, request={metrics:?}",
                result.source,
                result.series[0].points.len(),
                result.timing
            );
            break (start_ms, end_ms);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Collector metrics never reached Prometheus: {result:?}"
        );
        std::thread::sleep(Duration::from_secs(3));
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.observe_grafana(move |answer| {
        let _ = tx.send(answer);
    });
    let catalog = match wait(&rx) {
        Fetched::Ok(catalog) => catalog,
        other => panic!("Grafana catalogue: {other:?}"),
    };
    let dashboard = catalog
        .dashboards
        .iter()
        .find(|dashboard| dashboard.uid == "k10s-observe-pilot-native")
        .expect("the actually provisioned pilot dashboard");
    assert_eq!(dashboard.panels.len(), 2);
    for panel in &dashboard.panels {
        assert!(panel.unsupported.is_none(), "{panel:?}");
        assert!(!panel.transformed);
        let target = panel.queries.first().expect("a dashboard target").clone();
        assert_eq!(target.datasource.as_deref(), Some("prometheus"));
        assert_eq!(target.ref_id, "A");
        let (tx, rx) = std::sync::mpsc::channel();
        let vars = grafana::Variables {
            namespace: NS.into(),
            pods: vec![pod.clone()],
            node: Some(node.clone()),
            start_ms,
            end_ms,
            step_ms: 15000,
            min_interval: panel.min_interval.clone(),
        };
        let _stop = sync.reader.observe_panel(target, vars, move |answer| {
            let _ = tx.send((answer, k10s_data::telemetry::request_metrics()));
        });
        let (answer, metrics) = wait(&rx);
        let result = match answer {
            Fetched::Ok(Some(result)) => result,
            other => panic!("bound native panel {}: {other:?}", panel.title),
        };
        assert!(
            !result.series.is_empty(),
            "{} must contain ingested samples",
            panel.title
        );
        assert!(result.series.iter().any(|series| {
            series
                .points
                .iter()
                .any(|(_, value)| value.is_finite() && *value > 0.0)
        }));
        assert!(!result.truncated);
        assert!(result.source.contains("datasource prometheus"));
        assert!(!result.expr.contains('$'));
        println!(
            "pilot panel {} unit={} reducer={} step={} expr={} request={metrics:?}",
            panel.title, panel.unit, panel.reducer, result.step_ms, result.expr
        );
    }
    let query = loki::RangeQuery {
        query: format!(
            "{{k8s_namespace_name=\"{NS}\",k8s_pod_name=\"{pod}\"}} | k8s_pod_uid=\"{uid}\" | trace_id=\"{trace_id}\""
        ),
        start_ns: start_ms as u64 * 1_000_000,
        end_ns: end_ms as u64 * 1_000_000,
        limit: 10,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.observe_loki(query, move |answer| {
        let _ = tx.send(answer);
    });
    let logs = match wait(&rx) {
        Fetched::Ok(Some(logs)) => logs,
        other => panic!("Collector OTLP log: {other:?}"),
    };
    let (stream, line) = logs
        .streams
        .iter()
        .flat_map(|stream| stream.lines.iter().map(move |line| (stream, line)))
        .find(|(stream, line)| {
            line.metadata
                .iter()
                .chain(&stream.labels)
                .any(|(key, value)| key == "trace_id" && value == &trace_id)
        })
        .expect("the emitted log trace ID in labels or structured metadata");
    assert!(
        line.metadata
            .iter()
            .chain(&stream.labels)
            .any(|(key, value)| key == "k8s_pod_uid" && value == &uid)
    );
    let body: serde_json::Value = serde_json::from_str(&line.line).expect("the producer body");
    assert_eq!(body, marker);
    assert!(!logs.truncated);
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.observe_trace(trace_id.clone(), move |answer| {
        let _ = tx.send(answer);
    });
    let trace = match wait(&rx) {
        Fetched::Ok(Some(trace)) => trace,
        other => panic!("Collector OTLP trace: {other:?}"),
    };
    assert_eq!(trace.trace_id, trace_id);
    assert_eq!(trace.spans.len(), 2);
    for span in &trace.spans {
        assert_eq!(span.namespace, NS);
        assert_eq!(span.pod, pod);
        assert_eq!(span.pod_uid, uid);
        assert_eq!(span.node, node);
        assert_eq!(span.service, "k10s-observe-pilot");
        assert!(span.duration_us > 0);
    }
    let root = trace
        .spans
        .iter()
        .find(|span| span.name == "pilot.investigation")
        .expect("instrumented operation");
    let child = trace
        .spans
        .iter()
        .find(|span| span.name == "pilot.hash")
        .expect("instrumented child");
    assert_eq!(child.parent, root.id);
    assert_eq!(marker["span_id"], root.id);
    let inventory = kube_runtime().block_on(async {
        let client = kube::Client::try_default().await.expect("client");
        otel::fetch_inventory(&client, Some(NS)).await
    });
    let inventory = match inventory {
        Fetched::Ok(inventory) => inventory,
        other => panic!("safe Collector inventory: {other:?}"),
    };
    assert_eq!(inventory.services.len(), 1);
    assert_eq!(inventory.services[0].name, "collector");
    assert_eq!(inventory.services[0].port, 13133);
    assert!(inventory.services[0].health.contains("Process extension"));
    println!(
        "pilot pipeline proven: {NS}/{pod} UID={uid}, trace={trace_id}, spans={}, log_time_ns={}, Collector={:?}",
        trace.spans.len(),
        line.ts_ns,
        inventory.services[0]
    );
    assert_eq!(sync.reader.observe_schedule_counts().rejected, 0);
}
