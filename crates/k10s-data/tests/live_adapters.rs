//! Live coverage for the seams `live_cluster.rs` does not own.
//!
//! Apply, exec, logs, port-forward, and usage stay in that file. This one
//! drives Helm, Argo, Flux, overlays, describe, tables, and day-2 against
//! the same cluster and the same two extra identities. Ignored by default
//! for the same no-network reason.
//!
//! ```text
//! KUBECONFIG=/path/to/kubeconfig cargo test -p k10s-data --test live_adapters -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` is required once a day-2 test is in the file: the
//! throwaway Deployment is shared cleanup, and a parallel scale races it.
//!
//! Helm reads the Secrets Helm already wrote. k3s stores Traefik that way;
//! this file does not install a chart. Argo and Flux are pinned as absence
//! when the groups are not served, and as a table when they are -- never as
//! an error. Day-2 mutates only `day2-probe` in `K10S_LIVE_NAMESPACE`. It
//! does not scale `web`, and it does not cordon or drain the node.
//! The decode-failure proof needs the lab's permissive CNPG fixture CRD. It
//! creates one generated Cluster with an invalid instances field and removes
//! it before checking the returned ecosystem table.
//! The Grafana failure proof needs the observability lab's monitoring release
//! and its monitoring-grafana PVC. A generated pod holds a SQLite lock without
//! changing data. It is deleted even when a check panics; run this suite alone.

use std::time::Duration;

use k10s_core::KindId;
use k10s_data::apply::{ApplyOutcome, ApplyRequest};
use k10s_data::argo;
use k10s_data::day2::{Blast, Caps, Day2Call, Day2Outcome, DeleteRequest, ScaleRequest};
use k10s_data::describe::DescribeRequest;
use k10s_data::flux;
use k10s_data::helm;
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
    let (tx, rx) = std::sync::mpsc::channel();
    reader.apply(request, move |outcome| {
        let _ = tx.send(outcome);
    });
    wait(&rx)
}

fn day2(reader: &Reader, kind: KindId, call: Day2Call) -> Day2Outcome {
    let (tx, rx) = std::sync::mpsc::channel();
    reader.day2(kind, call, move |outcome| {
        let _ = tx.send(outcome);
    });
    wait(&rx)
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
        "a stored release that will not decode is not an empty inventory"
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
    for needle in ["super-secret-value", "plaintext-in-the-annotation"] {
        assert!(
            !leaked.contains(needle),
            "a Secret value reached the Helm inventory Debug"
        );
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
        for cell in &row.cells {
            assert!(
                !cell.contains("apiVersion:") && !cell.contains("kind:"),
                "a table cell carried a manifest: {cell:?}"
            );
            assert!(
                !cell.contains("super-secret-value")
                    && !cell.contains("plaintext-in-the-annotation"),
                "a table cell carried a Secret value"
            );
        }
        assert!(
            row.cells[2].chars().all(|c| c.is_ascii_digit()),
            "Revision is the running number, not a payload: {:?}",
            row.cells[2]
        );
    }
}

#[test]
#[ignore = "needs the live lab's permissive CNPG fixture CRD; see the module comment"]
fn an_unreadable_live_resource_survives_into_the_ecosystem_table() {
    use k10s_data::cnpg;
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

#[test]
#[ignore = "needs the observability lab's Grafana and PVC; see the module comment"]
fn a_busy_live_grafana_catalog_reports_its_timeout_and_recovers() {
    use futures::FutureExt;
    use k8s_openapi::api::core::v1::Pod;
    use k10s_data::reach::{self, ToolKind, ToolReach};
    use kube::api::{AttachParams, DeleteParams, PostParams, Preconditions};
    use kube::runtime::wait::{await_condition, conditions};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (_plane, sync) = connect(None);
    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_grafana_catalog(move |answer| {
        let _ = tx.send(answer);
    });
    let Fetched::Ok(before) = wait(&rx) else {
        panic!("the lab's Grafana catalog must initially answer");
    };
    assert!(before.served && !before.dashboards.is_empty());

    kube_runtime().block_on(async {
        let client = kube::Client::try_default().await.expect("the fixture client");
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
        let created = pods.create(&PostParams::default(), &fixture).await.expect("the lock pod");
        let name = created.metadata.name.expect("the generated name");
        let uid = created.metadata.uid.expect("the generated UID");

        let checked = std::panic::AssertUnwindSafe(async {
            tokio::time::timeout(
                Duration::from_secs(60),
                await_condition(pods.clone(), &name, conditions::is_pod_running()),
            ).await.expect("the lock pod starts within a minute").expect("the pod can be watched");
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
            let mut lock = pods.exec(
                &name,
                ["python", "-u", "-c", code],
                &AttachParams::default().stdin(true).stdout(true).stderr(true),
            ).await.expect("the lock process attaches");
            let status = lock.take_status().expect("the remote exit status");
            let mut input = lock.stdin().expect("the release channel");
            let mut output = BufReader::new(lock.stdout().expect("the lock markers")).lines();
            let marker = tokio::time::timeout(Duration::from_secs(10), output.next_line())
                .await.expect("the database can be locked").expect("the lock marker can be read");
            assert_eq!(marker.as_deref(), Some("locked"));

            let reached = reach::bind(&client, ToolKind::Grafana, &ReachSettings::default()).await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            sync.reader.fetch_grafana_catalog(move |answer| {
                let _ = tx.send(answer);
            });
            let answer = tokio::time::timeout(Duration::from_secs(15), rx).await;

            input.write_all(b"release\n").await.expect("release the database lock");
            let marker = tokio::time::timeout(Duration::from_secs(10), output.next_line())
                .await.expect("the lock releases").expect("the release marker can be read");
            assert_eq!(marker.as_deref(), Some("released"));
            let status = tokio::time::timeout(Duration::from_secs(10), status)
                .await.expect("the lock process exits").expect("the exit status arrives");
            assert_eq!(status.status.as_deref(), Some("Success"), "{status:?}");
            tokio::time::timeout(Duration::from_secs(10), lock.join())
                .await.expect("the lock process ends").expect("the lock connection closes");
            (reached, answer)
        }).catch_unwind().await;

        pods.delete(&name, &DeleteParams {
            grace_period_seconds: Some(0),
            preconditions: Some(Preconditions {uid: Some(uid.clone()), ..Default::default()}),
            ..Default::default()
        }).await.expect("remove only the generated lock pod");
        tokio::time::timeout(
            Duration::from_secs(15),
            await_condition(pods, &name, conditions::is_deleted(&uid)),
        ).await.expect("the lock pod is removed").expect("the cleanup can be watched");

        let (reached, answer) = checked.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        assert!(matches!(reached, ToolReach::Bound(_)), "health still answers while storage is locked: {reached:?}");
        let answer = answer.expect("the catalog reports within the budget").expect("the callback arrives");
        assert!(matches!(&answer, Fetched::Failed {what: "grafana", why}
            if why == "Grafana did not answer within 4 seconds"), "{answer:?}");
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
