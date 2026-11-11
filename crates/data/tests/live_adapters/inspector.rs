//! Inspector identities and log refusals against disposable, UID-owned objects.

use super::*;
use k8s_openapi::api::core::v1::{Event, Namespace, Pod};
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use k10s_data::inspect::InspectDetail;
use kube::api::{DeleteParams, LogParams, PostParams, Preconditions};
use kube::{Api, Client};
use std::sync::Arc;

const IMAGE: &str = "python:3.14.7-alpine3.24@sha256:c6ead215bfd31f1e433d968853b7a769989117115b728874824e6c0a27cb96fc";

struct Fixture {
    runtime: tokio::runtime::Runtime,
    client: Client,
    namespace: String,
    uid: Option<String>,
}

impl Fixture {
    fn new() -> Self {
        assert!(std::env::var_os("KUBECONFIG").is_some());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a live fixture runtime");
        let (client, namespace) = runtime.block_on(async {
            let client = Client::try_default().await.expect("the admin client");
            let api: Api<Namespace> = Api::all(client.clone());
            let namespace: Namespace = serde_json::from_value(serde_json::json!({
                "apiVersion":"v1", "kind":"Namespace",
                "metadata":{
                    "generateName":"k10s-inspector-probe-",
                    "labels":{"k10s.dev/fixture":"inspector-probe"}
                }
            }))
            .expect("the owned namespace");
            let namespace = api
                .create(&PostParams::default(), &namespace)
                .await
                .expect("create only the generated namespace");
            (client, namespace)
        });
        Self {
            runtime,
            client,
            namespace: namespace.metadata.name.expect("the generated name"),
            uid: Some(namespace.metadata.uid.expect("the created UID")),
        }
    }

    fn sync(&self, client: Client) -> (Sync, crossbeam_channel::Receiver<k10s_core::IngestEvent>) {
        let (sink, events) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
        let sync = self.runtime.block_on(async {
            k10s_data::sync_from(
                k10s_data::connect::Clients {
                    sync: client.clone(),
                    interactive: client,
                },
                &self.namespace,
                &Options {
                    probe_namespaces: vec![self.namespace.clone()],
                    ..Default::default()
                },
                sink,
                Arc::new(k10s_data::IngestMetrics::default()),
            )
            .await
            .expect("the real fixture account syncs")
        });
        (sync, events)
    }

    fn remove(&mut self) -> Result<(), String> {
        let Some(uid) = &self.uid else {
            return Ok(());
        };
        let api: Api<Namespace> = Api::all(self.client.clone());
        self.runtime.block_on(async {
            let params = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(uid.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            match api.delete(&self.namespace, &params).await {
                Ok(_) => {}
                Err(kube::Error::Api(status)) if status.code == 404 => return Ok(()),
                Err(error) => return Err(said(&error)),
            }
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    let current = api
                        .get_metadata_opt(&self.namespace)
                        .await
                        .map_err(|error| said(&error))?;
                    if current.as_ref().and_then(|item| item.metadata.uid.as_ref()) != Some(uid) {
                        return Ok::<(), String>(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .map_err(|_| format!("cleanup of {} UID {uid} exceeded 60s", self.namespace))?
        })?;
        self.uid = None;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("owned Inspector fixture cleanup refused: {error}");
        }
    }
}

fn events(sync: &Sync, namespace: &str, uid: &str) -> Vec<k10s_data::inspect::EventLine> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.inspector.fetch_events(namespace, uid, move |detail| {
        let _ = tx.send(detail);
    });
    match wait_interactive(&rx, 1) {
        InspectDetail::Events(events) => events,
        other => panic!("the selected UID's event history: {other:?}"),
    }
}

fn tail(sync: &Sync, namespace: &str, name: &str, uid: &str) -> InspectDetail {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync
        .inspector
        .fetch_log_tail(namespace, &Arc::from(name), uid, move |detail| {
            let _ = tx.send(detail);
        });
    wait_interactive(&rx, 1)
}

fn nonempty_tail(sync: &Sync, namespace: &str, name: &str, uid: &str) -> InspectDetail {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the owned fixture must emit stdout within 30s"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let _stop =
            sync.inspector
                .fetch_log_tail(namespace, &Arc::from(name), uid, move |detail| {
                    let _ = tx.send(detail);
                });
        let detail = rx
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("the actual fixture tail must reply before the stdout deadline");
        match detail {
            InspectDetail::Log(ref log) if log.lines.is_empty() => {}
            InspectDetail::Log(_) => return detail,
            other => {
                panic!("the actual fixture tail must not fail while awaiting stdout: {other:?}")
            }
        }
        std::thread::sleep(
            Duration::from_millis(100)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
}

#[test]
#[ignore = "needs the lab; creates only a generated Inspector namespace"]
fn same_name_recreation_refuses_the_old_tail_and_keeps_uid_event_histories_separate() {
    let mut fixture = Fixture::new();
    let (sync, _events) = fixture.sync(fixture.client.clone());
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    let event_api: Api<Event> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    let mut uids = Vec::new();
    for (generation, message) in ["original Inspector object", "replacement Inspector object"]
        .into_iter()
        .enumerate()
    {
        let uid = fixture.runtime.block_on(async {
            pods.create(&PostParams::default(), &pod("same-name", false))
                .await
                .expect("create only the fixture Pod");
            let item = running_pod(&pods, "same-name", 0).await;
            let uid = item.metadata.uid.expect("the object UID");
            let event: Event = serde_json::from_value(serde_json::json!({
                "apiVersion":"v1", "kind":"Event",
                "metadata":{"generateName":"inspector-history-"},
                "involvedObject":{
                    "apiVersion":"v1", "kind":"Pod", "name":"same-name",
                    "namespace":fixture.namespace, "uid":uid
                },
                "message":message, "reason":"InspectorProbe", "type":"Normal"
            }))
            .expect("the generated event");
            event_api
                .create(&PostParams::default(), &event)
                .await
                .expect("create only the fixture event");
            uid
        });
        let InspectDetail::Log(log) = nonempty_tail(&sync, &fixture.namespace, "same-name", &uid)
        else {
            panic!("the actual selected Pod UID has current logs");
        };
        assert!(
            log.lines
                .iter()
                .any(|line| line.ends_with("current Inspector attempt")),
            "{log:?}"
        );
        assert!(!log.previous_unavailable);
        if generation == 0 {
            fixture.runtime.block_on(async {
                pods.delete(
                    "same-name",
                    &DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: Some(uid.clone()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .await
                .expect("remove only the original fixture Pod UID");
                tokio::time::timeout(Duration::from_secs(60), async {
                    while pods
                        .get_metadata_opt("same-name")
                        .await
                        .expect("deletion read")
                        .is_some()
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .expect("the original Pod is gone before recreating its name");
            });
        }
        uids.push(uid);
    }
    assert_ne!(uids[0], uids[1]);
    assert_eq!(
        tail(&sync, &fixture.namespace, "same-name", &uids[0]),
        InspectDetail::Failed {
            what: "logs",
            why: "Pod same-name no longer has the selected UID; its log tail is withheld"
                .to_string(),
        }
    );
    for (uid, expected, excluded) in [
        (
            &uids[0],
            "original Inspector object",
            "replacement Inspector object",
        ),
        (
            &uids[1],
            "replacement Inspector object",
            "original Inspector object",
        ),
    ] {
        let history = events(&sync, &fixture.namespace, uid);
        assert!(
            history.iter().any(|event| event.message == expected),
            "{history:?}"
        );
        assert!(
            !history.iter().any(|event| event.message == excluded),
            "{history:?}"
        );
    }
    fixture.remove().expect("restore the generated namespace");
}

fn pod(name: &str, restart_once: bool) -> Pod {
    let command = if restart_once {
        "from pathlib import Path; import sys,time; p=Path('/probe/started'); first=not p.exists(); p.touch(); print('previous Inspector attempt' if first else 'current Inspector attempt', flush=True); sys.exit(17) if first else time.sleep(3600)"
    } else {
        "import time; print('current Inspector attempt', flush=True); time.sleep(3600)"
    };
    serde_json::from_value(serde_json::json!({
        "apiVersion":"v1", "kind":"Pod", "metadata":{"name":name},
        "spec":{
            "restartPolicy":"Always", "automountServiceAccountToken":false,
            "terminationGracePeriodSeconds":0,
            "volumes":[{"name":"attempt", "emptyDir":{}}],
            "containers":[{
                "name":"probe", "image":IMAGE, "command":["python", "-u", "-c", command],
                "resources":{"requests":{"cpu":"10m", "memory":"16Mi"}, "limits":{"cpu":"100m", "memory":"64Mi"}},
                "volumeMounts":[{"name":"attempt", "mountPath":"/probe"}]
            }]
        }
    }))
    .expect("the nonprivileged log fixture")
}

async fn running_pod(api: &Api<Pod>, name: &str, restarts: i32) -> Pod {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let pod = api.get(name).await.expect("the owned Pod status");
            if pod
                .status
                .as_ref()
                .and_then(|status| status.container_statuses.as_ref())
                .is_some_and(|statuses| {
                    statuses.iter().any(|status| {
                        status.name == "probe"
                            && status.restart_count >= restarts
                            && status
                                .state
                                .as_ref()
                                .is_some_and(|state| state.running.is_some())
                    })
                })
            {
                return pod;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the owned Pod runs within 90s")
}

#[test]
#[ignore = "needs the lab kubelet; creates only a generated Inspector namespace"]
fn a_missing_previous_attempt_keeps_the_kubelets_exact_response_contract() {
    let mut fixture = Fixture::new();
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    let name = "never-restarted";
    let message = format!("previous terminated container \"probe\" in pod \"{name}\" not found");
    let uid = fixture.runtime.block_on(async {
        pods.create(&PostParams::default(), &pod(name, false))
            .await
            .expect("create only the never-restarted Pod");
        let current = running_pod(&pods, name, 0).await;
        let uid = current.metadata.uid.expect("the running Pod UID");
        let statuses = current
            .status
            .expect("the Pod status")
            .container_statuses
            .expect("the container status");
        assert_eq!(statuses[0].restart_count, 0);
        let error = pods
            .logs(
                name,
                &LogParams {
                    container: Some("probe".to_string()),
                    previous: true,
                    tail_lines: Some(200),
                    limit_bytes: Some(64 * 1024),
                    timestamps: true,
                    ..Default::default()
                },
            )
            .await
            .expect_err("the Pod has no previous container");
        let kube::Error::Api(status) = error else {
            panic!("the kubelet refusal is an API Status: {}", said(&error));
        };
        assert_eq!(status.code, 400);
        assert_eq!(status.reason, "BadRequest");
        assert_eq!(status.message.trim(), message);
        println!(
            "live missing-previous status={} reason={} message={}",
            status.code,
            status.reason,
            status.message.trim()
        );
        uid
    });
    let (sync, _events) = fixture.sync(fixture.client.clone());
    let InspectDetail::Log(log) = tail(&sync, &fixture.namespace, name, &uid) else {
        panic!("the never-restarted Pod has current logs");
    };
    assert!(
        log.lines
            .iter()
            .any(|line| line.ends_with("current Inspector attempt"))
    );
    assert!(log.note.is_none());
    assert!(!log.previous_unavailable);
    fixture.remove().expect("restore the generated namespace");
}

#[test]
#[ignore = "needs the lab kubelet and admin impersonation; creates only a generated Inspector namespace"]
fn a_real_previous_tail_stays_previous_and_log_denial_stays_denied() {
    let mut fixture = Fixture::new();
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    let name = "restarted-once";
    let (client, uid) = fixture.runtime.block_on(async {
        pods.create(&PostParams::default(), &pod(name, true))
            .await
            .expect("create only the restarted Pod");
        let current = running_pod(&pods, name, 1).await;
        let uid = current.metadata.uid.expect("the running Pod UID");
        assert_eq!(current.status.expect("status").container_statuses.expect("container status")[0].restart_count, 1);
        let user = format!("inspector-no-logs-{}", fixture.namespace);
        let roles: Api<Role> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
        let role: Role = serde_json::from_value(serde_json::json!({
            "apiVersion":"rbac.authorization.k8s.io/v1", "kind":"Role", "metadata":{"name":"metadata-reader"},
            "rules":[{"apiGroups":[""], "resources":["pods", "events"], "verbs":["get", "list", "watch"]}]
        })).expect("the log-denied role");
        roles.create(&PostParams::default(), &role).await.expect("create only the fixture role");
        let bindings: Api<RoleBinding> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
        let binding: RoleBinding = serde_json::from_value(serde_json::json!({
            "apiVersion":"rbac.authorization.k8s.io/v1", "kind":"RoleBinding", "metadata":{"name":"metadata-reader"},
            "roleRef":{"apiGroup":"rbac.authorization.k8s.io", "kind":"Role", "name":"metadata-reader"},
            "subjects":[{"apiGroup":"rbac.authorization.k8s.io", "kind":"User", "name":user}]
        })).expect("the fixture binding");
        bindings.create(&PostParams::default(), &binding).await.expect("create only the fixture binding");
        let mut config = kube::Config::infer().await.expect("the lab connection config");
        config.auth_info.impersonate = Some(user);
        config.auth_info.impersonate_groups = Some(vec!["system:authenticated".to_string()]);
        (Client::try_from(config).expect("the log-denied client"), uid)
    });
    let (admin, _admin_events) = fixture.sync(fixture.client.clone());
    let InspectDetail::Log(log) = tail(&admin, &fixture.namespace, name, &uid) else {
        panic!("the actual previous container output is available");
    };
    assert!(
        log.lines
            .iter()
            .any(|line| line.ends_with("previous Inspector attempt"))
    );
    assert!(
        !log.lines
            .iter()
            .any(|line| line.ends_with("current Inspector attempt"))
    );
    let note = log.note.expect("the previous tail names itself");
    assert!(!log.previous_unavailable);
    assert!(note.starts_with("previous instance of probe"), "{note}");
    assert!(note.contains("exit 17"), "{note}");
    let (restricted, _restricted_events) = fixture.sync(client);
    assert_eq!(
        tail(&restricted, &fixture.namespace, name, &uid),
        InspectDetail::Denied { what: "logs" }
    );
    fixture.remove().expect("restore the generated namespace");
}

#[test]
#[ignore = "needs the lab kubelet; creates only a generated Inspector namespace"]
fn timestamped_application_control_words_stay_current_or_previous_logs() {
    const APPLICATION: &str =
        "unable to retrieve container logs for containerd://application-message";

    let mut fixture = Fixture::new();
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    let (sync, _events) = fixture.sync(fixture.client.clone());
    for previous in [false, true] {
        let name = if previous {
            "previous-control-words"
        } else {
            "current-control-words"
        };
        let mut object = pod(name, previous);
        let command = object
            .spec
            .as_mut()
            .expect("the owned fixture spec")
            .containers[0]
            .command
            .as_mut()
            .expect("the fixture command");
        command[3] = format!("print('{APPLICATION}', flush=True); {}", command[3]);
        let uid = fixture.runtime.block_on(async {
            pods.create(&PostParams::default(), &object)
                .await
                .expect("create only the application control-word Pod");
            running_pod(&pods, name, i32::from(previous))
                .await
                .metadata
                .uid
                .expect("the running Pod UID")
        });
        let InspectDetail::Log(log) = nonempty_tail(&sync, &fixture.namespace, name, &uid) else {
            panic!("the actual kubelet timestamp distinguishes application control words");
        };
        assert!(
            log.lines.iter().any(|line| {
                line.split_once(' ').is_some_and(|(timestamp, message)| {
                    timestamp.parse::<k8s_openapi::jiff::Timestamp>().is_ok()
                        && message == APPLICATION
                })
            }),
            "{log:?}"
        );
        assert!(!log.previous_unavailable);
        assert_eq!(log.note.is_some(), previous);
    }
    fixture.remove().expect("restore the generated namespace");
}

#[test]
#[ignore = "needs the lab kubelet; delays an actual response inside a generated Inspector fixture"]
fn cancelling_a_real_tail_discards_a_delayed_reply_and_releases_shared_capacity() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut fixture = Fixture::new();
    let pods: Api<Pod> = Api::namespaced(fixture.client.clone(), &fixture.namespace);
    let name = "delayed-tail";
    let uid = fixture.runtime.block_on(async {
        pods.create(&PostParams::default(), &pod(name, false))
            .await
            .expect("create only the delayed-tail Pod");
        running_pod(&pods, name, 0)
            .await
            .metadata
            .uid
            .expect("the running Pod UID")
    });
    let identity_path = format!("/api/v1/namespaces/{}/pods/{name}", fixture.namespace);
    let log_path = format!("{identity_path}/log");
    let identities = Arc::new(AtomicUsize::new(0));
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let client = fixture.client.clone();
    let seen_identity = identities.clone();
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let client = client.clone();
        let seen_tx = seen_tx.clone();
        let is_log = request.uri().path() == log_path;
        let is_identity = request.uri().path() == identity_path;
        let identities = seen_identity.clone();
        async move {
            if is_identity {
                identities.fetch_add(1, Ordering::Relaxed);
            }
            let response = client.send(request).await?;
            if is_log {
                assert!(
                    response.status().is_success(),
                    "the actual kubelet tail resolves"
                );
                seen_tx.send(()).expect("the real request witness");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Ok::<_, kube::Error>(response)
        }
    });
    let delayed = fixture
        .runtime
        .block_on(async { Client::new(service, fixture.namespace.clone()) });
    let (sync, _events) = fixture.sync(delayed);
    let (tx, rx) = std::sync::mpsc::channel();
    let stop =
        sync.inspector
            .fetch_log_tail(&fixture.namespace, &Arc::from(name), &uid, move |detail| {
                tx.send(detail)
                    .expect("a cancelled delayed reply must not be published");
            });
    wait(&seen_rx);
    assert_eq!(sync.reader.observe_schedule_counts().active, 1);
    drop(stop);
    fixture.runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while sync.reader.observe_schedule_counts().admitted != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation releases the shared scheduler");
    });
    assert_eq!(sync.reader.observe_schedule_counts().active, 0);
    assert!(matches!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
    assert_eq!(
        identities.load(Ordering::Relaxed),
        1,
        "no final UID read follows the cancelled log request"
    );
    let _history = events(&sync, &fixture.namespace, &uid);
    fixture.remove().expect("restore the generated namespace");
}
