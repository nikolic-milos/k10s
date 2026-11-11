//! The production interactive layer with an observable scripted response body.

use super::*;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::{Stream as _, StreamExt as _};
use http_body_util::{BodyExt as _, BodyStream, StreamBody};
use kube::Client;
use kube::client::ClientBuilder;
use tokio::sync::{Notify, oneshot};

use crate::read::Fetched;
use crate::telemetry::{RequestMetrics, Scheduler};

const PATH: &str = "/apis/apps/v1/namespaces/k10s-observe-pilot/deployments/pilot-producer";

#[derive(Debug)]
struct Seen {
    method: http::Method,
    path: String,
    accept: String,
    content_type: String,
    body: Vec<u8>,
}

#[derive(Clone, Default)]
struct Script(Arc<Mutex<Vec<Seen>>>);

impl Script {
    fn assert_get(&self) {
        let seen = self.0.lock().expect("script lock");
        assert_eq!(seen.len(), 1, "{seen:?}");
        let row = &seen[0];
        assert_eq!(row.method, http::Method::GET);
        assert_eq!(row.path, PATH);
        assert_eq!(row.accept, "application/json");
        assert_eq!(row.content_type, "");
        assert!(row.body.is_empty(), "{row:?}");
    }
}

#[derive(Default)]
struct Probe {
    polls: AtomicUsize,
    bytes: AtomicUsize,
    dropped: AtomicBool,
    changed: Notify,
}

struct BodyDrop(Arc<Probe>);

impl Drop for BodyDrop {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
        self.0.changed.notify_one();
    }
}

fn observed(body: Box<DynBody>, probe: Arc<Probe>) -> Box<DynBody> {
    let mut body = body;
    let dropped = BodyDrop(probe.clone());
    let frames = futures::stream::poll_fn(move |cx| {
        let _ = &dropped;
        probe.polls.fetch_add(1, Ordering::SeqCst);
        let frame = Pin::new(body.as_mut()).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &frame
            && let Some(data) = frame.data_ref()
        {
            probe.bytes.fetch_add(data.len(), Ordering::SeqCst);
        }
        probe.changed.notify_one();
        frame
    });
    Box::new(StreamBody::new(frames))
}

fn scripted_body(chunks: Vec<Vec<u8>>, hold_open: bool, probe: Arc<Probe>) -> Box<DynBody> {
    let frames = futures::stream::iter(chunks)
        .flat_map(|chunk| BodyStream::new(Body::from(chunk)))
        .map(|frame| frame.map_err(BoxError::from));
    let mut frames = Box::pin(frames);
    let frames = futures::stream::poll_fn(move |cx| match frames.as_mut().poll_next(cx) {
        Poll::Ready(None) if hold_open => Poll::Pending,
        frame => frame,
    });
    observed(Box::new(StreamBody::new(frames)), probe)
}

fn interactive_client(
    body: Box<DynBody>,
    status: u16,
    deadline: Duration,
    headers_after: Duration,
) -> (Client, Script) {
    let script = Script::default();
    let seen = script.clone();
    let body = Arc::new(Mutex::new(Some(body)));
    let service = tower::service_fn(move |request: http::Request<Body>| {
        let body = body
            .lock()
            .expect("response lock")
            .take()
            .expect("one request");
        let seen = seen.clone();
        async move {
            let (parts, request_body) = request.into_parts();
            let request_body = request_body.collect().await?.to_bytes().to_vec();
            seen.0.lock().expect("script lock").push(Seen {
                method: parts.method,
                path: parts.uri.to_string(),
                accept: parts
                    .headers
                    .get(http::header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
                content_type: parts
                    .headers
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
                body: request_body,
            });
            if !headers_after.is_zero() {
                tokio::time::sleep(headers_after).await;
            }
            Ok::<_, BoxError>(
                http::Response::builder()
                    .status(status)
                    .body(body)
                    .expect("scripted response"),
            )
        }
    });
    (
        ClientBuilder::new(Deadline(deadline).layer(service), "default").build(),
        script,
    )
}

fn request() -> http::Request<Vec<u8>> {
    http::Request::get(PATH)
        .header(http::header::ACCEPT, "application/json")
        .body(Vec::new())
        .expect("selected resource GET")
}

async fn fetch(
    scheduler: &Arc<Scheduler>,
    client: Client,
    cap: usize,
) -> (Option<http::StatusCode>, Fetched<Vec<u8>>, RequestMetrics) {
    let (tx, rx) = oneshot::channel();
    let _stop = scheduler.submit(
        &tokio::runtime::Handle::current(),
        async move {
            Fetched::Ok(
                crate::reach::bounded_request_with_status(
                    &client,
                    "selected resource",
                    request(),
                    cap,
                )
                .await,
            )
        },
        move |answer| {
            let counts = crate::telemetry::request_metrics().expect("admitted request metrics");
            let _ = tx.send((answer, counts));
        },
    );
    let (answer, counts) = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .expect("bounded reader finishes before the held-open body")
        .expect("request callback");
    match answer {
        Fetched::Ok((status, answer)) => (status, answer, counts),
        other => panic!("admitted request: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn an_interactive_body_stops_at_the_callers_cap() {
    let probe = Arc::new(Probe::default());
    let body = scripted_body(
        vec![vec![b'a'; 4], vec![b'b'], vec![b'c'; 64]],
        true,
        probe.clone(),
    );
    let (client, script) = interactive_client(body, 200, Duration::from_secs(35), Duration::ZERO);
    let (status, answer, counts) = fetch(&Arc::new(Scheduler::default()), client, 4).await;
    assert_eq!(status, None, "an oversized body is not a completed status");
    assert_eq!(
        answer,
        Fetched::Failed {
            what: "selected resource",
            why: "selected resource answered with more than 4 bytes; the body is refused".into(),
        }
    );
    assert_eq!(counts.requests, 1);
    assert_eq!(counts.bytes, 5);
    assert_eq!(probe.polls.load(Ordering::SeqCst), 2);
    assert_eq!(probe.bytes.load(Ordering::SeqCst), 5);
    assert!(probe.dropped.load(Ordering::SeqCst));
    script.assert_get();
}

#[tokio::test(start_paused = true)]
async fn an_interactive_denial_drops_its_body_without_polling_it() {
    for code in [401, 403] {
        let probe = Arc::new(Probe::default());
        let body = scripted_body(vec![b"unread refusal".to_vec()], true, probe.clone());
        let (client, script) =
            interactive_client(body, code, Duration::from_secs(35), Duration::ZERO);
        let (status, answer, counts) = fetch(&Arc::new(Scheduler::default()), client, 4).await;
        assert_eq!(
            status,
            Some(http::StatusCode::from_u16(code).expect("status"))
        );
        assert_eq!(
            answer,
            Fetched::Denied {
                what: "selected resource"
            }
        );
        assert_eq!(counts.requests, 1);
        assert_eq!(counts.bytes, 0);
        assert_eq!(probe.polls.load(Ordering::SeqCst), 0);
        assert_eq!(probe.bytes.load(Ordering::SeqCst), 0);
        assert!(probe.dropped.load(Ordering::SeqCst));
        script.assert_get();
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_an_interactive_body_releases_it_and_admission() {
    let probe = Arc::new(Probe::default());
    let body = scripted_body(vec![vec![b'a']], true, probe.clone());
    let (client, script) = interactive_client(body, 200, Duration::from_secs(35), Duration::ZERO);
    let scheduler = Arc::new(Scheduler::default());
    let (tx, rx) = oneshot::channel();
    let stop = scheduler.submit(
        &tokio::runtime::Handle::current(),
        async move { crate::reach::bounded_request(&client, "selected resource", request()).await },
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while probe.polls.load(Ordering::SeqCst) < 2 {
            probe.changed.notified().await;
        }
    })
    .await
    .expect("first frame consumed and next frame pending");
    assert_eq!(probe.bytes.load(Ordering::SeqCst), 1);
    assert_eq!(scheduler.counts().active, 1);
    assert_eq!(scheduler.counts().admitted, 1);
    drop(stop);
    tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .expect("cancelled task drops its callback")
        .expect_err("cancellation must not publish an outcome");
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert_eq!(scheduler.counts().active, 0);
    assert_eq!(scheduler.counts().admitted, 0);
    script.assert_get();

    let recovery = Arc::new(Probe::default());
    let body = scripted_body(vec![b"ok".to_vec()], false, recovery.clone());
    let (client, script) = interactive_client(body, 200, Duration::from_secs(35), Duration::ZERO);
    let (status, answer, counts) = fetch(&scheduler, client, 4).await;
    assert_eq!(status, Some(http::StatusCode::OK));
    assert_eq!(answer, Fetched::Ok(b"ok".to_vec()));
    assert_eq!(counts.requests, 1);
    assert_eq!(counts.bytes, 2);
    assert!(recovery.dropped.load(Ordering::SeqCst));
    script.assert_get();
}

#[tokio::test(start_paused = true)]
async fn headers_and_a_dripping_body_share_one_absolute_deadline() {
    let probe = Arc::new(Probe::default());
    let frames = futures::stream::unfold((), |()| async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut chunk = Body::from(vec![b'a']);
        Some((
            chunk
                .frame()
                .await
                .expect("one frame")
                .map_err(BoxError::from),
            (),
        ))
    });
    let body: Box<DynBody> = Box::new(Box::pin(StreamBody::new(frames)));
    let body = observed(body, probe.clone());
    let (client, script) = interactive_client(
        body,
        200,
        Duration::from_millis(100),
        Duration::from_millis(40),
    );
    let started = tokio::time::Instant::now();
    let (status, answer, counts) = fetch(&Arc::new(Scheduler::default()), client, 4).await;
    assert_eq!(status, None);
    assert_eq!(
        answer,
        Fetched::Failed {
            what: "selected resource",
            why: "no answer within 100 ms".into(),
        }
    );
    assert_eq!(started.elapsed(), Duration::from_millis(100));
    assert_eq!(counts.requests, 1);
    assert_eq!(counts.bytes, 1);
    assert_eq!(probe.bytes.load(Ordering::SeqCst), 1);
    assert!(probe.dropped.load(Ordering::SeqCst));
    script.assert_get();
}

#[tokio::test(start_paused = true)]
async fn a_body_expires_at_the_original_instant_and_then_ends() {
    let probe = Arc::new(Probe::default());
    let body = scripted_body(vec![b"late".to_vec()], true, probe.clone());
    let (client, script) = interactive_client(
        body,
        200,
        Duration::from_millis(100),
        Duration::from_millis(40),
    );
    let response = client
        .send(request().map(Body::from))
        .await
        .expect("headers");
    assert_eq!(probe.polls.load(Ordering::SeqCst), 0);
    let mut body = response.into_body();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let error = body
        .frame()
        .await
        .expect("one timeout error")
        .expect_err("expired body");
    assert_eq!(crate::connect::describe(&error), "no answer within 100 ms");
    assert!(body.frame().await.is_none(), "timeout ends the stream");
    assert_eq!(probe.polls.load(Ordering::SeqCst), 0);
    assert!(probe.dropped.load(Ordering::SeqCst));
    script.assert_get();
}

#[tokio::test(start_paused = true)]
async fn a_completed_body_keeps_its_data_and_trailers_and_releases_the_inner() {
    let probe = Arc::new(Probe::default());
    let mut trailers = http::HeaderMap::new();
    trailers.insert("x-result", http::HeaderValue::from_static("complete"));
    let body = Body::from(b"ok".to_vec())
        .map_err(BoxError::from)
        .with_trailers(async move { Some(Ok(trailers)) });
    let body: Box<DynBody> = Box::new(Box::pin(body));
    let body = observed(body, probe.clone());
    let (client, script) = interactive_client(body, 200, Duration::from_secs(35), Duration::ZERO);
    let response = client
        .send(request().map(Body::from))
        .await
        .expect("headers");
    assert_eq!(probe.polls.load(Ordering::SeqCst), 0);
    let mut body = response.into_body();
    let data = body
        .frame()
        .await
        .expect("data frame")
        .expect("data")
        .into_data()
        .expect("data frame");
    assert_eq!(data.as_ref(), b"ok");
    let trailers = body
        .frame()
        .await
        .expect("trailer frame")
        .expect("trailers")
        .into_trailers()
        .expect("trailer frame");
    assert_eq!(trailers.get("x-result").expect("kept trailer"), "complete");
    assert!(body.frame().await.is_none());
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(body.frame().await.is_none(), "completed body stays ended");
    script.assert_get();
}

#[tokio::test(start_paused = true)]
async fn a_body_error_keeps_its_sentence_and_releases_the_inner() {
    let probe = Arc::new(Probe::default());
    let frames = BodyStream::new(Body::from(b"ok".to_vec()))
        .map(|frame| frame.map_err(BoxError::from))
        .chain(futures::stream::once(std::future::ready(Err(
            BoxError::from("scripted body was lost"),
        ))));
    let body = observed(Box::new(StreamBody::new(frames)), probe.clone());
    let (client, script) = interactive_client(body, 200, Duration::from_secs(35), Duration::ZERO);
    let response = client
        .send(request().map(Body::from))
        .await
        .expect("headers");
    let mut body = response.into_body();
    assert_eq!(
        body.frame()
            .await
            .expect("data")
            .expect("data frame")
            .into_data()
            .expect("data")
            .as_ref(),
        b"ok"
    );
    let error = body
        .frame()
        .await
        .expect("one body error")
        .expect_err("body was lost");
    assert_eq!(crate::connect::describe(&error), "scripted body was lost");
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(body.frame().await.is_none(), "body error ends the stream");
    script.assert_get();
}

#[tokio::test(start_paused = true)]
async fn an_already_empty_response_remains_complete_after_its_headers() {
    let body: Box<DynBody> = Box::new(Body::empty().map_err(BoxError::from));
    let (client, script) = interactive_client(
        body,
        204,
        Duration::from_millis(100),
        Duration::from_millis(40),
    );
    let response = client
        .send(request().map(Body::from))
        .await
        .expect("complete empty response");
    assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
    let mut body = response.into_body();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        body.frame().await.is_none(),
        "empty response already completed within its deadline"
    );
    script.assert_get();
}
