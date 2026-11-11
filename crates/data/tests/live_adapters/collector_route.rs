//! The focused Collector route reads an existing owned process endpoint safely.

use super::*;
use k8s_openapi::api::core::v1::{Namespace, Service};
use k10s_data::browse::TablePage;
use kube::{Api, Client};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const NS: &str = "k10s-observe-pilot";
const METADATA: &str = "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1";

fn collector_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a live Collector fixture runtime")
}

#[derive(Debug)]
struct Request {
    path: String,
    accept: String,
}

fn allowed(path: &str, accept: &str) -> bool {
    (path == "/apis/opentelemetry.io" && accept == "application/json")
        || (path.starts_with("/apis/opentelemetry.io/")
            && path.ends_with("/opentelemetrycollectors")
            && accept == METADATA)
        || (path == "/api/v1/services" && accept.is_empty())
        || (path.starts_with("/api/v1/namespaces/")
            && path.contains("/services/")
            && (path.ends_with("/proxy/") || path.ends_with("/proxy/debug/servicez"))
            && accept == "application/json")
}

fn owned_client(runtime: &tokio::runtime::Runtime) -> (Client, String) {
    assert!(std::env::var_os("KUBECONFIG").is_some());
    runtime.block_on(async {
        let client = Client::try_default().await.expect("the real lab client");
        let namespaces: Api<Namespace> = Api::all(client.clone());
        let namespace = namespaces
            .get_metadata(NS)
            .await
            .expect("the existing owned pilot namespace");
        assert_eq!(
            namespace
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("k10s.dev/fixture"))
                .map(String::as_str),
            Some("observe-pilot")
        );
        let services: Api<Service> = Api::namespaced(client.clone(), NS);
        let service = services
            .get_metadata("collector")
            .await
            .expect("the existing pilot Collector Service");
        assert_eq!(
            service
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("k10s.dev/fixture"))
                .map(String::as_str),
            Some("observe-pilot")
        );
        (
            client,
            service
                .metadata
                .uid
                .expect("the owned Collector Service UID"),
        )
    })
}

fn sync(
    runtime: &tokio::runtime::Runtime,
    client: Client,
    interactive: Client,
) -> (Sync, crossbeam_channel::Receiver<k10s_core::IngestEvent>) {
    let (tx, rx) = crossbeam_channel::bounded(DEFAULT_EVENT_SINK_CAPACITY);
    let sync = runtime.block_on(async {
        k10s_data::sync_from(
            k10s_data::connect::Clients {
                sync: client,
                interactive,
            },
            NS,
            &Options {
                probe_namespaces: vec![NS.to_string()],
                ..Default::default()
            },
            tx,
            Arc::new(k10s_data::IngestMetrics::default()),
        )
        .await
        .expect("the existing lab sync")
    });
    (sync, rx)
}

fn guarded_client(
    runtime: &tokio::runtime::Runtime,
    client: Client,
    requests: Arc<Mutex<Vec<Request>>>,
    delay: Option<std::sync::mpsc::Sender<()>>,
) -> Client {
    let delayed = Arc::new(AtomicBool::new(false));
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let client = client.clone();
        let requests = requests.clone();
        let delay = delay.clone();
        let delayed = delayed.clone();
        async move {
            let path = request.uri().path().to_string();
            let accept = request
                .headers()
                .get(http::header::ACCEPT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            assert_eq!(request.method(), http::Method::GET);
            assert!(request.headers().get(http::header::CONTENT_TYPE).is_none());
            assert!(
                allowed(&path, &accept),
                "unexpected Collector route refused before transport: {path}"
            );
            let (parts, body) = request.into_parts();
            let bytes = body.collect_bytes().await?;
            assert!(bytes.is_empty(), "Collector reads carry no request body");
            requests
                .lock()
                .expect("live Collector request lock")
                .push(Request {
                    path: path.clone(),
                    accept,
                });
            let response = client
                .send(http::Request::from_parts(
                    parts,
                    kube::client::Body::from(bytes),
                ))
                .await?;
            if path == format!("/api/v1/namespaces/{NS}/services/collector:13133/proxy/") {
                assert!(
                    response.status().is_success(),
                    "the actual owned process endpoint answers"
                );
                if let Some(delay) = delay
                    && !delayed.swap(true, Ordering::Relaxed)
                {
                    delay.send(()).expect("actual process-health witness");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            }
            Ok::<_, kube::Error>(response)
        }
    });
    runtime.block_on(async { Client::new(service, NS) })
}

fn fetch(reader: &Reader) -> TablePage {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = reader.fetch_collectors(move |answer| {
        tx.send(answer).expect("Collector table callback");
    });
    match wait_interactive(&rx, 1) {
        Fetched::Ok(Some(page)) => page,
        other => panic!("the actual Collector inventory and process endpoint: {other:?}"),
    }
}

#[test]
#[ignore = "needs the existing owned k10s-observe-pilot Collector; performs no mutations"]
fn focused_collectors_show_the_owned_process_endpoint_without_configuration_or_other_families() {
    let runtime = collector_runtime();
    let (client, uid) = owned_client(&runtime);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let interactive = guarded_client(&runtime, client.clone(), requests.clone(), None);
    let (sync, _events) = sync(&runtime, client.clone(), interactive);
    let page = fetch(&sync.reader);
    let row = page
        .rows
        .iter()
        .find(|row| row.uid == format!("service:{NS}/collector"))
        .expect("the actual owned Collector row");
    assert_eq!(row.name, "collector");
    assert_eq!(row.namespace.as_deref(), Some(NS));
    assert_eq!(row.cells[3], "13133");
    assert!(row.cells[4].starts_with("Process extension answered"));
    assert_eq!(row.cells[5], format!("{NS}/collector:13133"));
    let requests = requests.lock().expect("live Collector requests");
    assert_eq!(
        requests
            .iter()
            .filter(|row| row.path == "/apis/opentelemetry.io")
            .count(),
        1
    );
    assert!(
        requests
            .iter()
            .any(|row| row.path
                == format!("/api/v1/namespaces/{NS}/services/collector:13133/proxy/"))
    );
    assert!(requests.iter().all(|row| allowed(&row.path, &row.accept)));
    drop(requests);
    runtime.block_on(async {
        let services: Api<Service> = Api::namespaced(client, NS);
        assert_eq!(
            services
                .get_metadata("collector")
                .await
                .expect("the unchanged owned Service")
                .metadata
                .uid
                .as_deref(),
            Some(uid.as_str())
        );
    });
}

#[test]
#[ignore = "needs the existing owned Collector; delays its actual process-health response without mutations"]
fn cancelling_a_delayed_real_collector_reply_releases_capacity_without_publishing_a_table() {
    let runtime = collector_runtime();
    let (client, _uid) = owned_client(&runtime);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let interactive = guarded_client(&runtime, client.clone(), requests, Some(seen_tx));
    let (sync, _events) = sync(&runtime, client, interactive);
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.fetch_collectors(move |answer| {
        tx.send(answer)
            .expect("cancelled Collector work must not publish");
    });
    wait(&seen_rx);
    drop(stop);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while sync.reader.observe_schedule_counts().admitted != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation releases Collector admission");
    });
    assert_eq!(sync.reader.observe_schedule_counts().active, 0);
    assert!(matches!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
    let page = fetch(&sync.reader);
    let row = page
        .rows
        .iter()
        .find(|row| row.uid == format!("service:{NS}/collector"))
        .expect("fresh Collector read reuses released capacity");
    assert!(row.cells[4].starts_with("Process extension answered"));
}
