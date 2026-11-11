//! Delayed successful real Grafana replies prove client behavior without storage access.

use super::*;
use http_body_util::{BodyExt, Limited};
use k10s_data::reach::{self, ToolKind, ToolReach, Transport};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

struct Witness {
    path_and_query: String,
    status: u16,
    bytes: usize,
}

pub(super) struct Delay {
    path: String,
    max_replies: usize,
    armed: watch::Sender<bool>,
    witnesses: Mutex<Vec<Witness>>,
    active: AtomicUsize,
}

impl Delay {
    pub(super) fn arm(&self) {
        assert!(self.witnesses.lock().expect("Grafana witnesses").is_empty());
        assert!(!self.armed.send_replace(true));
    }

    pub(super) fn release(&self) {
        self.armed.send_replace(false);
    }

    pub(super) fn completed(&self) -> usize {
        self.witnesses.lock().expect("Grafana witnesses").len()
    }

    pub(super) fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub(super) fn witnesses(&self) -> Vec<(String, u16, usize)> {
        self.witnesses
            .lock()
            .expect("Grafana witnesses")
            .iter()
            .map(|witness| {
                (
                    witness.path_and_query.clone(),
                    witness.status,
                    witness.bytes,
                )
            })
            .collect()
    }
}

struct Active<'a>(&'a AtomicUsize);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(super) fn client(
    runtime: &tokio::runtime::Runtime,
    underlying: kube::Client,
    rest: &str,
    max_replies: usize,
) -> (kube::Client, Arc<Delay>) {
    assert!((1..=6).contains(&max_replies));
    let bound = runtime.block_on(reach::bind(
        &underlying,
        ToolKind::Grafana,
        &ReachSettings::default(),
    ));
    let ToolReach::Bound(bound) = bound else {
        panic!("the actual Grafana must bind before delay: {bound:?}");
    };
    let Transport::Proxy {
        namespace,
        service,
        port,
    } = bound.transport
    else {
        panic!("this live proof requires the real API-server Grafana proxy");
    };
    let delay = Arc::new(Delay {
        path: format!("/api/v1/namespaces/{namespace}/services/{service}:{port}/proxy/{rest}"),
        max_replies,
        armed: watch::channel(false).0,
        witnesses: Mutex::new(Vec::with_capacity(max_replies)),
        active: AtomicUsize::new(0),
    });
    let intercepted = delay.clone();
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let underlying = underlying.clone();
        let delay = intercepted.clone();
        async move {
            if request.uri().path() != delay.path {
                return underlying.send(request).await;
            }
            let mut release = delay.armed.subscribe();
            let armed = *release.borrow_and_update();
            if !armed {
                return underlying.send(request).await;
            }
            assert_eq!(request.method(), http::Method::GET);
            assert_eq!(request.headers()[http::header::ACCEPT], "application/json");
            assert!(request.headers().get(http::header::CONTENT_TYPE).is_none());
            if delay.path.ends_with("/api/search") {
                assert_eq!(request.uri().query(), Some("type=dash-db&limit=100"));
            }
            let path_and_query = request
                .uri()
                .path_and_query()
                .expect("Grafana target")
                .to_string();
            let (parts, body) = request.into_parts();
            let body = Limited::new(body, 0)
                .collect()
                .await
                .map_err(kube::Error::Service)?
                .to_bytes();
            assert!(body.is_empty(), "Grafana GETs have no body");
            let response = underlying
                .send(http::Request::from_parts(parts, body.into()))
                .await?;
            assert!(
                response.status().is_success(),
                "the actual Grafana reply must succeed before delay"
            );
            let (parts, body) = response.into_parts();
            let body = Limited::new(body, reach::MAX_BODY_BYTES)
                .collect()
                .await
                .map_err(kube::Error::Service)?
                .to_bytes();
            assert!(
                !body.is_empty(),
                "the complete actual Grafana body must exist before delay"
            );
            {
                let mut witnesses = delay.witnesses.lock().expect("Grafana witnesses");
                assert!(
                    witnesses.len() < delay.max_replies,
                    "the bounded delay must receive no extra requests"
                );
                delay.active.fetch_add(1, Ordering::Relaxed);
                witnesses.push(Witness {
                    path_and_query,
                    status: parts.status.as_u16(),
                    bytes: body.len(),
                });
            }
            let _active = Active(&delay.active);
            tokio::time::timeout(Duration::from_secs(60), release.changed())
                .await
                .expect("the test must cancel or release its delayed response within one minute")
                .expect("the owned Grafana release channel stays present");
            assert!(!*release.borrow(), "the Grafana delay was released");
            Ok::<_, kube::Error>(http::Response::from_parts(parts, body.into()))
        }
    });
    (
        runtime.block_on(async { kube::Client::new(service, namespace) }),
        delay,
    )
}
