//! A selected Service port, listener failure and kubelet upgrade against Grafana.
//! Numeric and named targetPort values each reach the existing Grafana Pod.
//! Dashboard navigation and restricted-account denial require separate proofs.

use super::*;
use k8s_openapi::api::core::v1::{Pod, Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::{apis::meta::v1::ObjectMeta, util::intstr::IntOrString};
use k10s_data::forward::{
    ForwardOpenStop, ForwardRegistry, ForwardRequest, ForwardReservation, ForwardRow, ForwardState,
};
use k10s_data::reach::{self, Bound, ToolAuth, ToolKind, Transport};
use kube::api::{Api, DeleteParams, ListParams, PostParams, Preconditions};
use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const REQUEST_BUDGET: Duration = Duration::from_secs(30);
const CLEANUP_BUDGET: Duration = Duration::from_secs(30);
const CLOSE_BUDGET: Duration = Duration::from_secs(5);

struct OwnedService<'a> {
    runtime: &'a tokio::runtime::Runtime,
    api: Api<Service>,
    namespace: String,
    name: String,
    uid: Option<String>,
    pending: bool,
}

impl OwnedService<'_> {
    fn create(&mut self, service: &Service) -> Result<(), String> {
        self.pending = true;
        eprintln!(
            "Grafana forward fixture create intent: {}/{}",
            self.namespace, self.name
        );
        let result = self.runtime.block_on(async {
            tokio::time::timeout(
                REQUEST_BUDGET,
                self.api.create(&PostParams::default(), service),
            )
            .await
        });
        let created = match result {
            Ok(Ok(created)) => created,
            Ok(Err(kube::Error::Api(status))) if matches!(status.code, 401 | 403 | 409 | 422) => {
                self.pending = false;
                return Err(status.message);
            }
            Ok(Err(error)) => return Err(said(&error)),
            Err(_) => return Err("Service CREATE timed out; its outcome is unknown".into()),
        };
        let uid = created
            .metadata
            .uid
            .filter(|uid| !uid.is_empty())
            .ok_or("created Service did not return a UID")?;
        if created.metadata.name.as_deref() != Some(self.name.as_str())
            || created.metadata.namespace.as_deref() != Some(self.namespace.as_str())
        {
            return Err("created Service identity did not match the request".into());
        }
        self.uid = Some(uid);
        self.pending = false;
        Ok(())
    }

    fn remove(&mut self) -> Result<(), String> {
        let Some(uid) = self.uid.as_ref() else {
            return if self.pending {
                Err(format!(
                    "Service CREATE is unresolved at {}/{}; no UID was adopted",
                    self.namespace, self.name
                ))
            } else {
                Ok(())
            };
        };
        self.runtime.block_on(async {
            tokio::time::timeout(CLEANUP_BUDGET, async {
                let params = DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(uid.clone()),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                match self.api.delete(&self.name, &params).await {
                    Ok(_) => {}
                    Err(kube::Error::Api(status)) if matches!(status.code, 404 | 409) => {}
                    Err(error) => return Err(said(&error)),
                }
                loop {
                    let current = self
                        .api
                        .get_metadata_opt(&self.name)
                        .await
                        .map_err(|error| said(&error))?;
                    match current {
                        None => return Ok(()),
                        Some(current) => match current.metadata.uid.as_deref() {
                            Some(current_uid) if current_uid != uid.as_str() => return Ok(()),
                            Some(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                            None => return Err("Service cleanup returned no UID".into()),
                        },
                    }
                }
            })
            .await
            .map_err(|_| {
                format!(
                    "cleanup of {}/{} UID {uid} timed out",
                    self.namespace, self.name
                )
            })?
        })?;
        self.uid = None;
        Ok(())
    }
}

impl Drop for OwnedService<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("Grafana forward fixture cleanup: {error}");
        }
    }
}

struct OwnedForward {
    registry: ForwardRegistry,
    row: ForwardRow,
    reservation: Option<ForwardReservation>,
}

impl OwnedForward {
    fn close(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            drop(reservation);
        } else {
            assert!(self.registry.close(self.row.id));
        }
        assert!(self.registry.list().iter().all(|row| row.id != self.row.id));
    }
}

impl Drop for OwnedForward {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            drop(reservation);
        } else {
            self.registry.close(self.row.id);
        }
    }
}

fn open_browser(reader: &Reader, request: ForwardRequest) -> (OwnedForward, ForwardOpenStop) {
    assert!(
        reader.forwards().list().is_empty(),
        "the isolated live-test Reader must have no forwards to reuse"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let registry = reader.forwards().clone();
    let stop = reader.open_forward_cancellable(request, move |answer| {
        let result = match answer {
            Fetched::Ok(reservation) => Ok(OwnedForward {
                registry,
                row: reservation.row().clone(),
                reservation: Some(reservation),
            }),
            other => Err(format!("the Service forward did not open: {other:?}")),
        };
        let _ = tx.send(result);
    });
    let owned = rx
        .recv_timeout(REQUEST_BUDGET)
        .expect("forward resolution callback")
        .expect("a new fixture forward");
    (owned, stop)
}

fn open_fixed(reader: &Reader, request: ForwardRequest) -> OwnedForward {
    assert!(
        reader.forwards().list().is_empty(),
        "the isolated Reader has no forwards"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let registry = reader.forwards().clone();
    reader.open_forward(request, move |answer| {
        let result = match answer {
            Fetched::Ok(row) => Ok(OwnedForward {
                registry,
                row,
                reservation: None,
            }),
            other => Err(format!("the fixed Service forward did not open: {other:?}")),
        };
        let _ = tx.send(result);
    });
    rx.recv_timeout(REQUEST_BUDGET)
        .expect("fixed resolution callback")
        .expect("a new fixed fixture forward")
}

fn ready(reader: &Reader, id: u64) -> Fetched<ForwardRow> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _wait = reader.wait_forward_ready(id, move |answer| {
        let _ = tx.send(answer);
    });
    rx.recv_timeout(REQUEST_BUDGET).expect("listener callback")
}

fn pod_target(pod: &Pod, target: &IntOrString) -> u16 {
    match target {
        IntOrString::Int(port) => u16::try_from(*port)
            .ok()
            .filter(|port| *port != 0)
            .expect("a valid nonzero target port"),
        IntOrString::String(name) => pod
            .spec
            .as_ref()
            .expect("the selected Pod spec")
            .containers
            .iter()
            .flat_map(|container| container.ports.iter().flatten())
            .filter(|port| {
                port.name.as_deref() == Some(name.as_str())
                    && port
                        .protocol
                        .as_deref()
                        .is_none_or(|protocol| protocol == "TCP")
            })
            .find_map(|port| {
                u16::try_from(port.container_port)
                    .ok()
                    .filter(|port| *port != 0)
            })
            .expect("the selected Pod declares Grafana's TCP target port"),
    }
}

fn listener_released(runtime: &tokio::runtime::Runtime, port: u16) {
    let deadline = Instant::now() + CLOSE_BUDGET;
    loop {
        match runtime.block_on(tokio::net::TcpListener::bind(("127.0.0.1", port))) {
            Ok(listener) => {
                assert_eq!(
                    listener
                        .local_addr()
                        .expect("released listener address")
                        .port(),
                    port
                );
                return;
            }
            Err(error) if error.kind() == ErrorKind::AddrInUse && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                panic!("the closed forward did not release loopback port {port}: {error}")
            }
        }
    }
}

fn observed_client(
    runtime: &tokio::runtime::Runtime,
    client: kube::Client,
    namespace: &str,
    pod: &str,
    port: u16,
    upgrades: Arc<Mutex<Vec<String>>>,
) -> kube::Client {
    let expected = format!("/api/v1/namespaces/{namespace}/pods/{pod}/portforward?&ports={port}");
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let client = client.clone();
        let expected = expected.clone();
        let upgrades = upgrades.clone();
        async move {
            if !request.uri().path().ends_with("/portforward") {
                return client.send(request).await;
            }
            assert_eq!(request.method(), http::Method::GET);
            assert_eq!(request.uri().path_and_query().unwrap().as_str(), expected);
            assert_eq!(request.headers()[http::header::CONNECTION], "Upgrade");
            assert_eq!(request.headers()[http::header::UPGRADE], "websocket");
            assert!(request.headers().get(http::header::ACCEPT).is_none());
            let (parts, body) = request.into_parts();
            let bytes = body.collect_bytes().await?;
            assert!(bytes.is_empty(), "the real port-forward GET has no body");
            let response = client
                .send(http::Request::from_parts(
                    parts,
                    kube::client::Body::from(bytes),
                ))
                .await?;
            assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);
            upgrades
                .lock()
                .expect("actual upgrade request log")
                .push(expected);
            Ok::<_, kube::Error>(response)
        }
    });
    runtime.block_on(async { kube::Client::new(service, namespace) })
}

#[derive(Clone, Copy, Debug)]
enum TargetPortForm {
    Numeric,
    Named,
}

#[test]
#[ignore = "needs the live Grafana fixture and permission to create and delete two Services"]
fn selected_grafana_service_port_forwards_real_bytes_and_reports_listener_collision() {
    for form in [TargetPortForm::Numeric, TargetPortForm::Named] {
        selected_grafana_target_port(form, false);
    }
}

#[test]
#[ignore = "needs the live Grafana fixture; creates UID-pinned Services on port80 and uses an automatic local listener"]
fn browser_service_port80_forwards_numeric_and_named_targets_on_an_owned_local_port() {
    for form in [TargetPortForm::Numeric, TargetPortForm::Named] {
        selected_grafana_target_port(form, true);
    }
}

fn selected_grafana_target_port(form: TargetPortForm, browser: bool) {
    assert!(
        std::env::var_os("KUBECONFIG").is_some(),
        "set KUBECONFIG to the lab cluster"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a live forward runtime");
    let client = runtime.block_on(async {
        tokio::time::timeout(REQUEST_BUDGET, kube::Client::try_default())
            .await
            .expect("the lab client deadline")
            .expect("the lab client")
    });
    let found = runtime.block_on(async {
        match tokio::time::timeout(
            REQUEST_BUDGET,
            reach::find_services(&client, ToolKind::Grafana),
        )
        .await
        {
            Ok(Fetched::Ok(mut services)) if services.len() == 1 => services.pop().unwrap(),
            other => panic!("one discovered Grafana Service is required: {other:?}"),
        }
    });
    let api: Api<Service> = Api::namespaced(client.clone(), &found.namespace);
    let source = runtime.block_on(async {
        tokio::time::timeout(REQUEST_BUDGET, api.get(&found.name))
            .await
            .expect("Grafana Service read deadline")
            .expect("the discovered Grafana Service")
    });
    let source_uid = source.metadata.uid.as_deref().expect("Grafana Service UID");
    let spec = source.spec.as_ref().expect("Grafana Service spec");
    let selector = spec
        .selector
        .clone()
        .filter(|selector| !selector.is_empty())
        .expect("Grafana Service selects its workload");
    let port = spec
        .ports
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|port| {
            port.port == i32::from(found.port)
                && port.name == found.port_name
                && port
                    .protocol
                    .as_deref()
                    .is_none_or(|protocol| protocol == "TCP")
        })
        .expect("the discovered Grafana TCP port is still declared");
    let source_target = if spec.cluster_ip.as_deref() == Some("None") {
        IntOrString::Int(i32::from(found.port))
    } else {
        match port.target_port.as_ref() {
            None | Some(IntOrString::Int(0)) => IntOrString::Int(i32::from(found.port)),
            Some(target) => target.clone(),
        }
    };
    let pods: Api<Pod> = Api::namespaced(client.clone(), &found.namespace);
    let label_selector = selector
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    let selected_pod = runtime.block_on(async {
        let listed = tokio::time::timeout(
            REQUEST_BUDGET,
            pods.list(&ListParams::default().labels(&label_selector).limit(10)),
        )
        .await
        .expect("Grafana Pod list deadline")
        .expect("the existing Grafana Pods");
        assert!(
            listed
                .metadata
                .continue_
                .as_deref()
                .is_none_or(str::is_empty),
            "the existing Grafana fixture must fit within the ten-Pod selection budget"
        );
        let mut running = listed.items.into_iter().filter(|pod| {
            pod.status
                .as_ref()
                .and_then(|status| status.phase.as_deref())
                == Some("Running")
        });
        let pod = running.next().expect("one existing Running Grafana Pod");
        assert!(
            running.next().is_none(),
            "the lab fixture must select exactly one Running Grafana Pod"
        );
        pod
    });
    let selected_pod_name = selected_pod
        .metadata
        .name
        .as_deref()
        .expect("Grafana Pod name");
    let selected_pod_uid = selected_pod
        .metadata
        .uid
        .as_deref()
        .expect("Grafana Pod UID");
    let remote_port = pod_target(&selected_pod, &source_target);
    let target = match form {
        TargetPortForm::Numeric => IntOrString::Int(i32::from(remote_port)),
        TargetPortForm::Named => IntOrString::String(
            selected_pod
                .spec
                .as_ref()
                .expect("Grafana Pod spec")
                .containers
                .iter()
                .flat_map(|container| container.ports.iter().flatten())
                .filter(|port| {
                    port.container_port == i32::from(remote_port)
                        && port
                            .protocol
                            .as_deref()
                            .is_none_or(|protocol| protocol == "TCP")
                })
                .find_map(|port| port.name.clone().filter(|name| !name.is_empty()))
                .expect("the existing Grafana TCP port must have a declared name"),
        ),
    };

    let reservation = if browser {
        None
    } else {
        Some(TcpListener::bind(("127.0.0.1", 0)).expect("an owned loopback listener"))
    };
    let selected = reservation
        .as_ref()
        .map(|listener| listener.local_addr().expect("its local address").port())
        .unwrap_or(80);
    let first = if selected == 18080 { 18081 } else { 18080 };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock")
        .as_nanos();
    let name = format!("k10s-forward-{stamp}-{}", std::process::id());
    let mut fixture = OwnedService {
        runtime: &runtime,
        api,
        namespace: found.namespace.clone(),
        name: name.clone(),
        uid: None,
        pending: false,
    };
    let service = Service {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(found.namespace.clone()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(selector.clone()),
            ports: Some(vec![
                ServicePort {
                    name: Some("decoy".into()),
                    port: first,
                    protocol: Some("TCP".into()),
                    target_port: Some(IntOrString::Int(1)),
                    ..Default::default()
                },
                ServicePort {
                    name: Some("grafana".into()),
                    port: i32::from(selected),
                    protocol: Some("TCP".into()),
                    target_port: Some(target.clone()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        status: None,
    };
    fixture
        .create(&service)
        .expect("create the owned multi-port Service");
    let fixture_uid = fixture.uid.clone().expect("the created Service UID");
    let created = runtime.block_on(async {
        tokio::time::timeout(REQUEST_BUDGET, fixture.api.get(&fixture.name))
            .await
            .expect("fixture Service read deadline")
            .expect("the created fixture Service")
    });
    assert_eq!(created.metadata.uid.as_deref(), Some(fixture_uid.as_str()));
    let fixture_version = created
        .metadata
        .resource_version
        .as_ref()
        .expect("the created Service resourceVersion")
        .clone();
    let created_spec = created.spec.as_ref().expect("the created Service spec");
    let created_ports = created_spec.ports.as_deref().expect("the created ports");
    assert_eq!(created_ports.len(), 2);
    assert_eq!(created_ports[0].port, first);
    assert_eq!(created_ports[1].port, i32::from(selected));
    assert_eq!(created_ports[1].target_port.as_ref(), Some(&target));
    assert_eq!(created_spec.selector.as_ref(), Some(&selector));
    let (sink, _events) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
    let upgrades = Arc::new(Mutex::new(Vec::new()));
    let observed = observed_client(
        &runtime,
        client.clone(),
        &found.namespace,
        selected_pod_name,
        remote_port,
        upgrades.clone(),
    );
    let sync = runtime.block_on(async {
        tokio::time::timeout(
            REQUEST_BUDGET,
            k10s_data::sync_from(
                k10s_data::connect::Clients {
                    sync: observed.clone(),
                    interactive: observed,
                },
                &found.namespace,
                &Options {
                    probe_namespaces: vec![found.namespace.clone()],
                    ..Default::default()
                },
                sink,
                Arc::new(k10s_data::IngestMetrics::default()),
            ),
        )
        .await
        .expect("cluster sync deadline")
        .expect("the lab cluster syncs")
    });
    let expected_service = Arc::new(k10s_core::ServiceIdentity {
        namespace: found.namespace.clone(),
        name: fixture.name.clone(),
        port: selected,
        uid: fixture_uid.clone(),
        resource_version: fixture_version.clone(),
    });
    let request = ForwardRequest {
        namespace: found.namespace.clone(),
        name,
        service: true,
        service_port: Some(selected),
        expected_service: Some(expected_service.clone()),
    };

    if let Some(reservation) = reservation {
        let address = reservation
            .local_addr()
            .expect("the owned occupied address");
        let mut collision = open_fixed(&sync.reader, request.clone());
        let Fetched::Failed { what, why } = ready(&sync.reader, collision.row.id) else {
            panic!("the owned listener must refuse a second bind");
        };
        assert_eq!(what, "port-forward");
        assert!(why.contains("cannot listen on 127.0.0.1"), "{why}");
        assert!(sync.reader.forwards().list().iter().any(|row| {
            row.id == collision.row.id && matches!(row.state, ForwardState::Dead { .. })
        }));
        let connected = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .expect("the test's original listener still accepts connections");
        assert_eq!(connected.peer_addr().unwrap(), address);
        drop(connected);
        collision.close();
        assert!(
            upgrades
                .lock()
                .expect("actual upgrade request log")
                .is_empty()
        );
        drop(reservation);
    }

    let (mut forward, forward_stop) = if browser {
        let (owned, stop) = open_browser(&sync.reader, request);
        (owned, Some(stop))
    } else {
        (open_fixed(&sync.reader, request), None)
    };
    let row = match ready(&sync.reader, forward.row.id) {
        Fetched::Ok(row) => row,
        other => panic!("the selected listener must become ready: {other:?}"),
    };
    assert_eq!(row.state, ForwardState::Active);
    if browser {
        assert_eq!(selected, 80);
        assert_ne!(row.spec.local_port, 0);
    } else {
        assert_eq!(row.spec.local_port, selected);
    }
    let local_port = row.spec.local_port;
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], local_port));
    assert!(Arc::ptr_eq(
        row.spec.expected_service.as_ref().unwrap(),
        &expected_service
    ));
    assert_eq!(row.spec.pod, selected_pod_name);
    let pod = runtime.block_on(async {
        tokio::time::timeout(REQUEST_BUDGET, pods.get(&row.spec.pod))
            .await
            .expect("selected Pod read deadline")
            .expect("the selected Grafana Pod")
    });
    assert_eq!(pod.metadata.uid.as_deref(), Some(selected_pod_uid));
    let labels = pod.metadata.labels.as_ref().expect("selected Pod labels");
    assert!(
        selector
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    );
    assert_eq!(
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref()),
        Some("Running")
    );
    assert_eq!(row.spec.remote_port, remote_port);
    assert_eq!(row.spec.remote_port, pod_target(&pod, &target));
    let bound = Bound {
        kind: ToolKind::Grafana,
        found: None,
        transport: Transport::Url {
            base: format!("http://127.0.0.1:{local_port}"),
        },
        auth: ToolAuth::Anonymous,
    };
    let bytes = match runtime.block_on(reach::tool_get(&client, &bound, "api/health")) {
        Fetched::Ok(bytes) => bytes,
        other => panic!("Grafana must answer through the kubelet upgrade: {other:?}"),
    };
    let health: serde_json::Value = serde_json::from_slice(&bytes).expect("Grafana health JSON");
    assert_eq!(health["database"], "ok");
    assert!(
        health["version"]
            .as_str()
            .is_some_and(|version| !version.is_empty())
    );
    assert!(sync.reader.forwards().is_active(row.id));
    assert!(
        !upgrades
            .lock()
            .expect("actual upgrade request log")
            .is_empty()
    );
    let completed = upgrades.lock().expect("actual upgrade request log").len();
    let mut connection = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("an owned connection before closing the forward");
    let deadline = Instant::now() + CLOSE_BUDGET;
    while upgrades.lock().expect("actual upgrade request log").len() == completed {
        assert!(
            Instant::now() < deadline,
            "the owned connection did not upgrade"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    connection
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("the held connection read deadline");
    let pending = connection
        .read(&mut [0])
        .expect_err("the upgraded idle connection must remain open before close");
    assert!(matches!(
        pending.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut
    ));
    connection
        .set_read_timeout(Some(CLOSE_BUDGET))
        .expect("the owned connection read deadline");
    forward.close();
    drop(forward_stop);
    match connection.read(&mut [0]) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("the closed forward retained the owned connection: {other:?}"),
    }
    drop(connection);
    listener_released(&runtime, local_port);
    let current = runtime.block_on(async {
        tokio::time::timeout(REQUEST_BUDGET, fixture.api.get(&fixture.name))
            .await
            .expect("fixture Service recheck deadline")
            .expect("the fixture Service remains present")
    });
    assert_eq!(current.metadata.uid.as_deref(), Some(fixture_uid.as_str()));
    assert_eq!(
        current.metadata.resource_version.as_deref(),
        Some(fixture_version.as_str())
    );
    fixture
        .remove()
        .expect("remove only the created Service UID");
    println!(
        "Grafana forward proven: targetPort={form:?} {target:?}, source={}/{} UID={source_uid}, fixture UID={fixture_uid}, selected={selected}, target={}/{}:{}, health bytes={}, successful upgrade paths={:?}",
        found.namespace,
        found.name,
        row.spec.namespace,
        row.spec.pod,
        row.spec.remote_port,
        bytes.len(),
        upgrades.lock().expect("actual upgrade request log")
    );
}
