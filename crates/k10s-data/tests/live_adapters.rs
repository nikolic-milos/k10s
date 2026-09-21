//! Live coverage for the seams `live_cluster.rs` does not own.
//!
//! Apply, exec, logs, port-forward, and usage stay in that file. This one
//! drives Helm, Argo, Flux, overlays, describe, tables, observability, and day-2 against
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
    use k10s_data::grafana::QueryDialect;
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
            k10s_data::loki::RangeQuery {
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
    accept: impl Fn(&k10s_data::alertmanager::Alert) -> bool,
) -> k10s_data::alertmanager::Alert {
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
    use k10s_data::alertmanager::{self, Matcher, SilenceOutcome, SilenceSpec};
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
        let answer = wait(&rx);
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
    use k10s_data::alertmanager::{self, Matcher, SilenceOutcome, SilenceSpec};
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
    let answer = wait(&rx);
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
    assert_eq!(
        answer,
        SilenceOutcome::Denied {
            what: "alertmanager",
            why: "access denied for this account".into()
        }
    );
    println!("live silence denial in {context}: {answer:?}");
}
