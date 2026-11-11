use super::*;
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;

const FIXTURE_GUARD: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Wire {
    request: Vec<u8>,
    closed: bool,
}

struct Fixture {
    base: String,
    accepted: oneshot::Receiver<()>,
    response_sent: oneshot::Receiver<()>,
    task: Option<tokio::task::JoinHandle<Wire>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Fixture {
    async fn new(parts: Vec<(Duration, Vec<u8>)>, close_write: bool) -> Self {
        Self::new_at("127.0.0.1:0", parts, close_write).await
    }

    async fn new_at(address: &str, parts: Vec<(Duration, Vec<u8>)>, close_write: bool) -> Self {
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, accepted) = oneshot::channel();
        let (response_done, response_sent) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(count, 0, "the request must include complete headers");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 128 << 10, "fixture request limit");
            }
            let _ = sent.send(());
            for (delay, bytes) in parts {
                tokio::time::sleep(delay).await;
                if tokio::time::timeout(FIXTURE_GUARD, socket.write_all(&bytes))
                    .await
                    .expect("fixture write exceeded its guard")
                    .is_err()
                {
                    break;
                }
            }
            let _ = response_done.send(());
            if close_write {
                let _ = tokio::time::timeout(FIXTURE_GUARD, socket.shutdown())
                    .await
                    .expect("fixture shutdown exceeded its guard");
            }
            loop {
                match tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer)).await {
                    Ok(Ok(0)) | Ok(Err(_)) => {
                        return Wire {
                            request,
                            closed: true,
                        };
                    }
                    Ok(Ok(count)) => {
                        request.extend_from_slice(&buffer[..count]);
                        assert!(request.len() < 128 << 10, "fixture request limit");
                    }
                    Err(_) => {
                        return Wire {
                            request,
                            closed: false,
                        };
                    }
                }
            }
        });
        Self {
            base: format!("http://{address}"),
            accepted,
            response_sent,
            task: Some(task),
        }
    }

    async fn response(bytes: Vec<u8>, close_write: bool) -> Self {
        Self::new(vec![(Duration::ZERO, bytes)], close_write).await
    }

    async fn finish(mut self) -> Wire {
        let task = self.task.take().unwrap();
        let wire = bounded_fixture_join(task, FIXTURE_GUARD)
            .await
            .expect("fixture completion exceeded its guard");
        assert!(
            wire.closed,
            "the owned connection must close within the fixture guard"
        );
        wire
    }
}

async fn bounded_fixture_join(
    mut task: tokio::task::JoinHandle<Wire>,
    guard: Duration,
) -> Result<Wire, ()> {
    match tokio::time::timeout(guard, &mut task).await {
        Ok(result) => Ok(result.expect("fixture task failed")),
        Err(_) => {
            task.abort();
            let result = tokio::time::timeout(FIXTURE_GUARD, &mut task)
                .await
                .expect("aborted fixture did not stop within its guard");
            assert!(result.unwrap_err().is_cancelled());
            Err(())
        }
    }
}

type HttpResult = (Option<http::StatusCode>, Fetched<Vec<u8>>);

struct FetchTask(Option<tokio::task::JoinHandle<HttpResult>>);

impl Drop for FetchTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn get(
    base: &str,
    rest: &str,
    auth: &ToolAuth,
    categorize: bool,
    budget: Duration,
) -> (Option<http::StatusCode>, Fetched<Vec<u8>>) {
    let mut status = None;
    let answer = plaintext_http_get_until(
        "grafana",
        base,
        rest,
        auth,
        categorize,
        tokio::time::Instant::now() + budget,
        &mut status,
    )
    .await;
    (status, answer)
}

fn length_response(code: u16, bytes: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {code} Fixture\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    )
    .into_bytes();
    response.extend_from_slice(bytes);
    response
}

fn failed(answer: Fetched<Vec<u8>>) -> String {
    match answer {
        Fetched::Failed {
            what: "grafana",
            why,
        } => why,
        _ => panic!("expected a named transport failure"),
    }
}

fn assert_request(wire: &Wire, base: &str, path: &str, token: Option<&str>, categorize: bool) {
    let split = wire
        .request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap();
    assert!(
        wire.request[split + 4..].is_empty(),
        "GET body must be empty"
    );
    let headers = std::str::from_utf8(&wire.request[..split]).unwrap();
    let mut lines = headers.split("\r\n");
    assert_eq!(lines.next().unwrap(), format!("GET {path} HTTP/1.1"));
    let mut actual = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').unwrap();
        assert!(
            actual
                .insert(name.to_ascii_lowercase(), value.trim().to_string())
                .is_none(),
            "duplicate request header"
        );
    }
    if let Some(length) = actual.remove("content-length") {
        assert_eq!(length, "0");
    }
    let mut expected = BTreeMap::from([
        ("host".into(), base.strip_prefix("http://").unwrap().into()),
        ("accept".into(), "application/json".into()),
        ("accept-encoding".into(), "identity".into()),
        ("connection".into(), "close".into()),
    ]);
    if let Some(token) = token {
        expected.insert("authorization".into(), format!("Bearer {token}"));
    }
    if categorize {
        expected.insert(
            "x-loki-response-encoding-flags".into(),
            "categorize-labels".into(),
        );
    }
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn content_length_finishes_without_waiting_for_server_eof_and_sends_only_named_auth() {
    let fixture = Fixture::response(length_response(200, br#"{"ok":true}"#), false).await;
    let auth = ToolAuth::NamedToken("invented_token-._~+/==".into());
    let (status, answer) = get(
        &format!("{}/prefix", fixture.base),
        "/api/query?q=up%7B%7D",
        &auth,
        true,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(status, Some(http::StatusCode::OK));
    assert_eq!(answer, Fetched::Ok(br#"{"ok":true}"#.to_vec()));
    let base = fixture.base.clone();
    let wire = fixture.finish().await;
    assert_request(
        &wire,
        &base,
        "/prefix/api/query?q=up%7B%7D",
        Some("invented_token-._~+/=="),
        true,
    );
}

#[tokio::test]
async fn chunked_success_decodes_extensions_and_trailers_after_an_interim_response() {
    let fixture = Fixture::response(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2;fixture=1\r\nok\r\n1\r\n!\r\n0\r\nX-Fixture: retained\r\n\r\n".to_vec(), false).await;
    let (status, answer) = get(
        &fixture.base,
        "",
        &ToolAuth::Anonymous,
        false,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(status, Some(http::StatusCode::OK));
    assert_eq!(answer, Fetched::Ok(b"ok!".to_vec()));
    let base = fixture.base.clone();
    let wire = fixture.finish().await;
    assert_request(&wire, &base, "/", None, false);
}

#[tokio::test]
async fn unsolicited_partial_content_is_refused_without_a_fallback_status() {
    let fixture = Fixture::response(
        b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-1/100\r\nContent-Length: 2\r\n\r\n[]".to_vec(),
        false,
    )
    .await;
    let (status, answer) = get(
        &fixture.base,
        "api/search",
        &ToolAuth::Anonymous,
        false,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(status, None);
    assert!(failed(answer).contains("206 Partial Content"));
    let base = fixture.base.clone();
    let wire = fixture.finish().await;
    assert_request(&wire, &base, "/api/search", None, false);
}

#[tokio::test]
async fn partial_proxy_response_cannot_authorize_status_fallback() {
    let service = tower::service_fn(|_: http::Request<kube::client::Body>| async {
        Ok::<_, std::io::Error>(
            http::Response::builder()
                .status(http::StatusCode::PARTIAL_CONTENT)
                .body(kube::client::Body::from(b"[]".to_vec()))
                .unwrap(),
        )
    });
    let client = kube::Client::new(service, "default");
    let request = http::Request::get("/api/search").body(Vec::new()).unwrap();
    let (status, answer) =
        bounded_request_with_status(&client, "grafana", request, MAX_BODY_BYTES).await;
    assert_eq!(status, None);
    assert!(failed(answer).contains("206 Partial Content"));
}

#[tokio::test]
async fn close_delimited_success_is_complete_only_after_eof() {
    let fixture = Fixture::response(
        b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nplain".to_vec(),
        true,
    )
    .await;
    let (status, answer) = get(
        &fixture.base,
        "",
        &ToolAuth::Anonymous,
        false,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(status, Some(http::StatusCode::OK));
    assert_eq!(answer, Fetched::Ok(b"plain".to_vec()));
    fixture.finish().await;
}

#[tokio::test]
async fn complete_error_bodies_keep_long_server_sentences_for_both_framings() {
    let sentence = format!(
        "a complete server explanation: {} end.",
        "detail λ ".repeat(400)
    );
    for (code, key) in [(404, "message"), (500, "error")] {
        let bytes = serde_json::to_vec(&serde_json::json!({(key): &sentence})).unwrap();
        for chunked in [false, true] {
            let response = if chunked {
                let mut raw = format!(
                    "HTTP/1.1 {code} Fixture\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                    bytes.len()
                )
                .into_bytes();
                raw.extend_from_slice(&bytes);
                raw.extend_from_slice(b"\r\n0\r\n\r\n");
                raw
            } else {
                length_response(code, &bytes)
            };
            let fixture = Fixture::response(response, false).await;
            let (status, answer) = get(
                &fixture.base,
                "",
                &ToolAuth::Anonymous,
                false,
                Duration::from_secs(1),
            )
            .await;
            assert_eq!(status.unwrap().as_u16(), code);
            assert_eq!(failed(answer), sentence);
            fixture.finish().await;
        }
    }
}

#[tokio::test]
async fn plain_error_and_empty_error_keep_the_sentence_or_status() {
    for (bytes, expected) in [
        (
            b"  exact server sentence  ".as_slice(),
            "exact server sentence",
        ),
        (b"".as_slice(), "503 Service Unavailable"),
    ] {
        let fixture = Fixture::response(length_response(503, bytes), false).await;
        let (status, answer) = get(
            &fixture.base,
            "",
            &ToolAuth::Anonymous,
            false,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(status, Some(http::StatusCode::SERVICE_UNAVAILABLE));
        assert_eq!(failed(answer), expected);
        fixture.finish().await;
    }
}

#[tokio::test]
async fn denied_status_is_named_and_does_not_wait_for_or_collect_a_body() {
    for code in [401, 403] {
        let fixture = Fixture::response(
            format!("HTTP/1.1 {code} Denied\r\nContent-Encoding: gzip\r\nContent-Length: 99999999\r\n\r\n").into_bytes(),
            false,
        )
        .await;
        let (status, answer) = get(
            &fixture.base,
            "",
            &ToolAuth::Anonymous,
            false,
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(status.unwrap().as_u16(), code);
        assert_eq!(answer, Fetched::Denied { what: "grafana" });
        fixture.finish().await;
    }
}

#[tokio::test]
async fn content_encoding_is_checked_before_success_or_error_body_processing() {
    for (encoding, supported) in [
        (b"".as_slice(), true),
        (b"Content-Encoding: identity\r\n".as_slice(), true),
        (
            b"Content-Encoding: IDENTITY\r\nContent-Encoding: identity\r\n".as_slice(),
            true,
        ),
        (
            b"Content-Encoding: , identity, ,IDENTITY,\r\n".as_slice(),
            true,
        ),
        (b"Content-Encoding: gzip\r\n".as_slice(), false),
        (
            b"Content-Encoding: identity\r\nContent-Encoding: gzip\r\n".as_slice(),
            false,
        ),
        (b"Content-Encoding: identity, GZIP\r\n".as_slice(), false),
        (b"Content-Encoding: identity;q=1\r\n".as_slice(), false),
        (b"Content-Encoding: \xff\r\n".as_slice(), false),
    ] {
        for code in [200, 404, 500] {
            let mut response = format!("HTTP/1.1 {code} Fixture\r\n").into_bytes();
            response.extend_from_slice(encoding);
            response.extend_from_slice(b"Content-Length: 2\r\n\r\nok");
            let fixture = Fixture::response(response, false).await;
            let (status, answer) = get(
                &fixture.base,
                "",
                &ToolAuth::Anonymous,
                false,
                PROBE_DEADLINE,
            )
            .await;
            if supported {
                assert_eq!(status.unwrap().as_u16(), code);
                if code == 200 {
                    assert_eq!(answer, Fetched::Ok(b"ok".to_vec()));
                } else {
                    assert_eq!(failed(answer), "ok");
                }
            } else {
                assert_eq!(status, None);
                assert!(failed(answer).contains("unsupported Content-Encoding"));
            }
            let base = fixture.base.clone();
            let wire = fixture.finish().await;
            assert_request(&wire, &base, "/", None, false);
        }
    }
}

#[tokio::test]
async fn partial_content_length_and_chunked_answers_never_authorize_fallback() {
    for code in [200, 404, 500] {
        for framing in [
            "Content-Length: 8\r\n\r\nshort",
            "Transfer-Encoding: chunked\r\n\r\n8\r\nshort",
        ] {
            let fixture = Fixture::response(
                format!("HTTP/1.1 {code} Fixture\r\n{framing}").into_bytes(),
                true,
            )
            .await;
            let (status, answer) = get(
                &fixture.base,
                "",
                &ToolAuth::Anonymous,
                false,
                Duration::from_secs(1),
            )
            .await;
            assert_eq!(status, None);
            assert!(!failed(answer).is_empty());
            fixture.finish().await;
        }
    }
}

#[tokio::test]
async fn success_and_error_caps_refuse_declared_and_streamed_oversize_bodies() {
    for (code, cap) in [(200, MAX_BODY_BYTES), (404, MAX_HTTP_ERROR_BYTES)] {
        let declared = format!(
            "HTTP/1.1 {code} Fixture\r\nContent-Length: {}\r\n\r\n",
            cap + 1
        )
        .into_bytes();
        let mut streamed = format!(
            "HTTP/1.1 {code} Fixture\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            cap + 1
        )
        .into_bytes();
        streamed.resize(streamed.len() + cap + 1, b'x');
        streamed.extend_from_slice(b"\r\n0\r\n\r\n");
        for response in [declared, streamed] {
            let fixture = Fixture::response(response, false).await;
            let (status, answer) = get(
                &fixture.base,
                "",
                &ToolAuth::Anonymous,
                false,
                PROBE_DEADLINE,
            )
            .await;
            assert_eq!(status, None);
            assert!(failed(answer).contains("body is refused"));
            fixture.finish().await;
        }
    }
}

#[tokio::test]
async fn malformed_framing_is_a_failure_without_status() {
    for (framing, response) in [
        (
            "content length",
            b"HTTP/1.1 200 OK\r\nContent-Length: invalid\r\n\r\nx".to_vec(),
        ),
        (
            "chunked",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nnot-hex\r\n".to_vec(),
        ),
    ] {
        let fixture = Fixture::response(response, true).await;
        let (status, answer) = get(
            &fixture.base,
            "",
            &ToolAuth::Anonymous,
            false,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(status, None, "{framing}");
        assert!(!failed(answer).is_empty(), "{framing}");
        fixture.finish().await;
    }
}

#[test]
fn parsed_header_content_limit_counts_each_name_and_value() {
    let mut headers = http::HeaderMap::new();
    let name = http::header::HeaderName::from_static("x");
    let exact = vec![b'a'; MAX_HTTP_HEADER_CONTENT_BYTES - name.as_str().len()];
    headers.insert(name.clone(), http::HeaderValue::from_bytes(&exact).unwrap());
    assert!(header_content_within_limit(&headers));
    headers.append(name, http::HeaderValue::from_static(""));
    assert!(!header_content_within_limit(&headers));

    let mut binary = http::HeaderMap::new();
    binary.append("x", http::HeaderValue::from_bytes(&[0xff]).unwrap());
    binary.append("x", http::HeaderValue::from_static("value"));
    assert!(header_content_within_limit(&binary));
}

#[tokio::test]
async fn oversized_response_headers_are_failures_without_status() {
    for code in [200, 404] {
        let response = format!(
            "HTTP/1.1 {code} Fixture\r\nX-Fixture: {}\r\nContent-Length: 0\r\n\r\n",
            "x".repeat(MAX_HTTP_HEADER_CONTENT_BYTES + 1)
        )
        .into_bytes();
        let fixture = Fixture::response(response, false).await;
        let (status, answer) = get(
            &fixture.base,
            "",
            &ToolAuth::Anonymous,
            false,
            PROBE_DEADLINE,
        )
        .await;
        assert_eq!(status, None);
        assert!(!failed(answer).is_empty());
        fixture.finish().await;
    }
}

#[tokio::test]
async fn an_incomplete_oversized_header_is_refused_without_waiting_for_eof() {
    let mut response = b"HTTP/1.1 200 OK\r\nX-Fixture: ".to_vec();
    response.resize(response.len() + MAX_HTTP_BUFFER_BYTES * 2, b'x');
    let fixture = Fixture::response(response, false).await;
    let (status, answer) = get(
        &fixture.base,
        "",
        &ToolAuth::Anonymous,
        false,
        PROBE_DEADLINE,
    )
    .await;
    assert_eq!(status, None);
    assert!(failed(answer).contains("message head is too large"));
    fixture.finish().await;
}

#[tokio::test]
async fn redirect_is_returned_without_contacting_the_location_or_forwarding_the_token() {
    let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let location = destination.local_addr().unwrap();
    let fixture = Fixture::response(format!("HTTP/1.1 302 Found\r\nLocation: http://{location}/token-sink\r\nContent-Length: 11\r\n\r\nno redirect").into_bytes(), false).await;
    let (status, answer) = get(
        &fixture.base,
        "",
        &ToolAuth::NamedToken("invented-private-token".into()),
        false,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(status, Some(http::StatusCode::FOUND));
    assert_eq!(failed(answer), "no redirect");
    assert!(
        tokio::time::timeout(Duration::from_millis(30), destination.accept())
            .await
            .is_err()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn one_deadline_bounds_stalled_and_peer_delayed_response_sequences() {
    for parts in [
        vec![],
        vec![
            (
                Duration::from_millis(60),
                b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".to_vec(),
            ),
            (Duration::from_millis(100), b"x".to_vec()),
        ],
    ] {
        let fixture = Fixture::new(parts, false).await;
        let (status, answer) = get(
            &fixture.base,
            "",
            &ToolAuth::Anonymous,
            false,
            Duration::from_millis(120),
        )
        .await;
        assert_eq!(status, None);
        assert!(failed(answer).contains("did not answer"));
        fixture.finish().await;
    }
}

#[tokio::test]
async fn dropping_the_fetch_closes_peers_waiting_after_header_or_body_writes() {
    for response in [
        vec![],
        b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\nx".to_vec(),
    ] {
        let mut fixture = Fixture::response(response, false).await;
        let base = fixture.base.clone();
        let mut task = FetchTask(Some(tokio::spawn(async move {
            get(
                &base,
                "",
                &ToolAuth::Anonymous,
                false,
                Duration::from_secs(1),
            )
            .await
        })));
        tokio::time::timeout(FIXTURE_GUARD, &mut fixture.accepted)
            .await
            .expect("fixture request barrier exceeded its guard")
            .unwrap();
        tokio::time::timeout(FIXTURE_GUARD, &mut fixture.response_sent)
            .await
            .expect("fixture response barrier exceeded its guard")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut handle = task.0.take().unwrap();
        handle.abort();
        assert!(
            tokio::time::timeout(FIXTURE_GUARD, &mut handle)
                .await
                .expect("cancelled fetch exceeded its guard")
                .unwrap_err()
                .is_cancelled()
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn a_running_dns_lookup_keeps_its_slot_after_cancellation_or_timeout() {
    use std::sync::atomic::{AtomicBool, Ordering};

    for deadline in [false, true] {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = oneshot::channel();
        let (expire_tx, expire_rx) = oneshot::channel();
        let task_slots = slots.clone();
        let task = tokio::spawn(async move {
            let lookup = bounded_dns_lookup(task_slots, move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                let _ = finished_tx.send(());
                Ok(Vec::new().into_iter())
            });
            tokio::pin!(lookup);
            if deadline {
                tokio::select! {
                    _ = &mut lookup => panic!("the lookup must remain blocked"),
                    _ = expire_rx => {}
                }
                assert!(
                    tokio::time::timeout(Duration::ZERO, &mut lookup)
                        .await
                        .is_err()
                );
            } else {
                let _ = lookup.await;
                panic!("the blocked lookup must be cancelled");
            }
        });
        tokio::time::timeout(FIXTURE_GUARD, started_rx)
            .await
            .expect("lookup start exceeded its guard")
            .unwrap();
        if deadline {
            expire_tx.send(()).expect("the timeout trigger");
            tokio::time::timeout(FIXTURE_GUARD, task)
                .await
                .expect("timed-out request exceeded its guard")
                .expect("the timeout path completed");
        } else {
            task.abort();
            assert!(
                tokio::time::timeout(FIXTURE_GUARD, task)
                    .await
                    .expect("cancelled request exceeded its guard")
                    .unwrap_err()
                    .is_cancelled()
            );
        }

        let invoked = Arc::new(AtomicBool::new(false));
        for _ in 0..4 {
            let invoked = invoked.clone();
            let error = bounded_dns_lookup(slots.clone(), move || {
                invoked.store(true, Ordering::SeqCst);
                Ok(Vec::new().into_iter())
            })
            .await
            .expect_err("the running lookup retains its permit");
            assert!(error.to_string().contains("DNS lookup capacity is full"));
        }
        assert!(!invoked.load(Ordering::SeqCst));
        release_tx.send(()).expect("release the running lookup");
        tokio::time::timeout(FIXTURE_GUARD, finished_rx)
            .await
            .expect("lookup finish exceeded its guard")
            .unwrap();
        tokio::time::timeout(FIXTURE_GUARD, async {
            while slots.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the finished lookup releases its permit");
        let address = SocketAddr::from(([127, 0, 0, 1], 80));
        let found = bounded_dns_lookup(slots, move || Ok(vec![address].into_iter()))
            .await
            .expect("a later lookup is admitted")
            .collect::<Vec<_>>();
        assert_eq!(found, [address]);
    }
}

#[test]
fn cancelling_queued_and_never_polled_dns_lookups_does_not_run_them() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("the isolated lookup runtime");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = runtime.spawn_blocking(move || {
        let _ = started_tx.send(());
        let _ = release_rx.recv();
    });
    started_rx
        .recv_timeout(FIXTURE_GUARD)
        .expect("the blocking worker is occupied");
    let unpolled_slots = Arc::new(tokio::sync::Semaphore::new(1));
    let unpolled_invoked = Arc::new(AtomicBool::new(false));
    let lookup_invoked = unpolled_invoked.clone();
    let unpolled = {
        let _entered = runtime.enter();
        bounded_dns_lookup(unpolled_slots.clone(), move || {
            lookup_invoked.store(true, Ordering::SeqCst);
            Ok(Vec::new().into_iter())
        })
    };
    assert_eq!(unpolled_slots.available_permits(), 0);
    drop(unpolled);
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let invoked = Arc::new(AtomicBool::new(false));
    let task_invoked = invoked.clone();
    let task_slots = slots.clone();
    let task = runtime.spawn(async move {
        bounded_dns_lookup(task_slots, move || {
            task_invoked.store(true, Ordering::SeqCst);
            Ok(Vec::new().into_iter())
        })
        .await
    });
    runtime.block_on(async {
        tokio::time::timeout(FIXTURE_GUARD, async {
            while slots.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the queued lookup takes its permit");
    });
    task.abort();
    runtime.block_on(async {
        assert!(
            tokio::time::timeout(FIXTURE_GUARD, task)
                .await
                .expect("cancellation exceeded its guard")
                .unwrap_err()
                .is_cancelled()
        );
    });
    release_tx.send(()).expect("release the blocking worker");
    runtime.block_on(async {
        tokio::time::timeout(FIXTURE_GUARD, blocker)
            .await
            .expect("blocker exceeded its guard")
            .expect("the blocking worker exits");
        tokio::time::timeout(FIXTURE_GUARD, async {
            while slots.available_permits() != 1 || unpolled_slots.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the cancelled queued lookups release their permits");
    });
    assert!(!invoked.load(Ordering::SeqCst));
    assert!(!unpolled_invoked.load(Ordering::SeqCst));
}

#[tokio::test]
async fn resolver_capacity_failure_is_labelled_before_dns_starts() {
    let mut resolver = DirectResolver {
        slots: Arc::new(tokio::sync::Semaphore::new(0)),
    };
    let name = "dns.invalid".parse().expect("a resolver name");
    let error = tower::Service::call(&mut resolver, name)
        .await
        .expect_err("a full resolver refuses work");
    assert!(error.to_string().contains("DNS lookup capacity is full"));
}

#[tokio::test]
async fn numeric_loopback_bypasses_an_exhausted_direct_dns_resolver() {
    let fixture = Fixture::response(length_response(200, b"ok"), false).await;
    let connector =
        hyper_util::client::legacy::connect::HttpConnector::new_with_resolver(DirectResolver {
            slots: Arc::new(tokio::sync::Semaphore::new(0)),
        });
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build(connector);
    let request = settings_request(&fixture.base, "api/health", &ToolAuth::Anonymous, false)
        .expect("the numeric loopback request");
    let response = tokio::time::timeout(FIXTURE_GUARD, client.request(request))
        .await
        .expect("the loopback request deadline")
        .expect("an IP address needs no DNS slot");
    assert_eq!(response.status(), http::StatusCode::OK);
    let bytes = tokio::time::timeout(FIXTURE_GUARD, response.into_body().collect())
        .await
        .expect("the response body deadline")
        .expect("the response body")
        .to_bytes();
    assert_eq!(&bytes[..], b"ok");
    let base = fixture.base.clone();
    let wire = fixture.finish().await;
    assert_request(&wire, &base, "/api/health", None, false);
}

#[tokio::test]
async fn malformed_tokens_are_refused_before_a_connection_and_do_not_escape_in_errors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    for token in [
        "",
        "invented-secret\r\nX-Injected: yes",
        "invented-secret\n",
        "invented-secret\0",
        "invented secret",
        "invented:secret",
        "λ-token",
        "=",
        "a=b",
    ] {
        let (status, answer) = get(
            &base,
            "",
            &ToolAuth::NamedToken(token.into()),
            false,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(status, None);
        let why = failed(answer);
        assert!(
            !why.contains("invented-secret")
                && !why.contains("X-Injected")
                && !why.contains("λ-token")
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    let header = authorization(&ToolAuth::NamedToken("invented-secret".into()))
        .unwrap()
        .unwrap();
    assert!(
        !format!("{:?}", ToolAuth::NamedToken("invented-secret".into()))
            .contains("invented-secret")
    );
    assert!(header.is_sensitive());
    assert!(!format!("{header:?}").contains("invented-secret"));
}

#[test]
fn malformed_or_credentialed_settings_are_not_echoed_or_exposed_as_browser_urls() {
    for url in [
        "http://invented-secret@example.test",
        "https://invented-secret@example.test",
        "invalid:invented-secret\r\n",
        "http://example.test/prefix?token=invented-secret",
        "http://example.test/#invented-secret",
        "http://",
    ] {
        match bind_url(ToolKind::Grafana, url, ToolAuth::Anonymous) {
            ToolReach::Unbound(value) => {
                assert!(value.browser_url.is_none());
                assert!(!value.why.contains("invented-secret"));
            }
            _ => panic!("unsafe or malformed settings must be unbound"),
        }
    }
    assert!(settings_request("https://example.test", "", &ToolAuth::Anonymous, false).is_err());
    let huge = "x".repeat(MAX_URL_BYTES + 1);
    assert!(settings_uri(&huge).is_err());
    assert!(settings_request("http://example.test", &huge, &ToolAuth::Anonymous, false).is_err());
}

#[tokio::test]
async fn reflected_named_tokens_are_redacted_without_truncating_the_server_sentence() {
    let sentence = format!(
        "token invented-secret was refused; {} exact end",
        "detail ".repeat(100)
    );
    let fixture = Fixture::response(length_response(500, sentence.as_bytes()), false).await;
    let (_, answer) = get(
        &fixture.base,
        "",
        &ToolAuth::NamedToken("invented-secret".into()),
        false,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(
        failed(answer),
        sentence.replace("invented-secret", "[redacted]")
    );
    fixture.finish().await;
}

#[tokio::test]
async fn an_exact_error_cap_is_complete_and_more_than_one_hundred_headers_are_refused() {
    let bytes = vec![b'x'; MAX_HTTP_ERROR_BYTES];
    let fixture = Fixture::response(length_response(500, &bytes), false).await;
    let (status, answer) = get(
        &fixture.base,
        "",
        &ToolAuth::Anonymous,
        false,
        PROBE_DEADLINE,
    )
    .await;
    assert_eq!(status, Some(http::StatusCode::INTERNAL_SERVER_ERROR));
    assert_eq!(failed(answer).as_bytes(), bytes);
    fixture.finish().await;
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n".to_vec();
    for n in 0..100 {
        response.extend_from_slice(format!("X-Fixture-{n}: value\r\n").as_bytes());
    }
    response.extend_from_slice(b"\r\n");
    let fixture = Fixture::response(response, false).await;
    let (status, answer) = get(
        &fixture.base,
        "",
        &ToolAuth::Anonymous,
        false,
        PROBE_DEADLINE,
    )
    .await;
    assert_eq!(status, None);
    assert!(!failed(answer).is_empty());
    fixture.finish().await;
}

#[test]
fn valid_https_browser_links_keep_their_query_and_fragment_without_a_fetch() {
    let url = "https://example.test/d/view?orgId=1#panel";
    match bind_url(ToolKind::Grafana, url, ToolAuth::Anonymous) {
        ToolReach::Unbound(value) => assert_eq!(value.browser_url.as_deref(), Some(url)),
        _ => panic!("HTTPS remains a browser link"),
    }
}

#[test]
fn configured_urls_are_hidden_in_diagnostics_without_changing_requests() {
    let rejected = ToolOverride {
        url: Some("https://private-user@example.test/path".into()),
        auth: ToolAuth::NamedToken("private-token".into()),
    };
    let settings = ReachSettings {
        grafana: rejected,
        ..ReachSettings::default()
    };
    let diagnostic = format!("{settings:?}");
    assert!(!diagnostic.contains("private-user"));
    assert!(!diagnostic.contains("private-token"));

    let browser_url = "https://example.test/private-path?token=private-query#private-fragment";
    let reach = bind_url(ToolKind::Grafana, browser_url, ToolAuth::Anonymous);
    let diagnostic = format!("{reach:?}");
    for secret in ["private-path", "private-query", "private-fragment"] {
        assert!(!diagnostic.contains(secret));
    }
    let ToolReach::Unbound(unbound) = reach else {
        panic!("HTTPS remains a browser link");
    };
    assert_eq!(unbound.browser_url.as_deref(), Some(browser_url));

    let base = "http://example.test:8080/private-path";
    let ToolReach::Bound(bound) = bind_url(ToolKind::Prometheus, base, ToolAuth::Anonymous) else {
        panic!("HTTP URL is bound");
    };
    assert_eq!(
        bound.source(),
        "http://example.test:8080/[path or query redacted]"
    );
    assert!(!format!("{bound:?}").contains("private-path"));
    let request = settings_request(base, "api/health", &ToolAuth::Anonymous, false).unwrap();
    assert_eq!(request.uri().path(), "/private-path/api/health");
    assert_eq!(
        url_source("http://private-user@example.test/path"),
        "configured tool URL [redacted]"
    );
    assert_eq!(
        url_source("http://example.test:8080"),
        "http://example.test:8080"
    );
}

#[test]
fn an_oversized_https_fragment_is_refused_without_retaining_or_echoing_it() {
    let url = format!(
        "https://example.test/#invented-secret{}",
        "x".repeat(MAX_URL_BYTES)
    );
    match bind_url(ToolKind::Grafana, &url, ToolAuth::Anonymous) {
        ToolReach::Unbound(value) => {
            assert!(value.browser_url.is_none());
            assert!(!value.why.contains("invented-secret"));
        }
        _ => panic!("the full HTTPS value must obey the cap"),
    }
}

#[test]
fn token_absent_error_redaction_preserves_the_existing_allocation() {
    for auth in [
        ToolAuth::Anonymous,
        ToolAuth::NamedToken("invented-secret".into()),
    ] {
        let message = "a full server sentence without the token".to_string();
        let pointer = message.as_ptr();
        let capacity = message.capacity();
        let result = redact_named_token(message, &auth);
        assert_eq!(result, "a full server sentence without the token");
        assert_eq!(result.as_ptr(), pointer);
        assert_eq!(result.capacity(), capacity);
    }
}

#[tokio::test]
async fn a_stalled_fixture_join_aborts_its_task_before_reporting_failure() {
    struct Stopped(Option<oneshot::Sender<()>>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    let (started_tx, started_rx) = oneshot::channel();
    let (stopped_tx, stopped_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _stopped = Stopped(Some(stopped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
        Wire {
            request: Vec::new(),
            closed: false,
        }
    });
    tokio::time::timeout(FIXTURE_GUARD, started_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(bounded_fixture_join(task, Duration::ZERO).await.is_err());
    tokio::time::timeout(FIXTURE_GUARD, stopped_rx)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn the_exact_success_body_cap_is_accepted_without_waiting_for_eof() {
    let bytes = vec![b'x'; MAX_BODY_BYTES];
    let fixture = Fixture::response(length_response(200, &bytes), false).await;
    let (status, answer) = get(
        &fixture.base,
        "",
        &ToolAuth::Anonymous,
        false,
        PROBE_DEADLINE,
    )
    .await;
    assert_eq!(status, Some(http::StatusCode::OK));
    assert_eq!(answer, Fetched::Ok(bytes));
    fixture.finish().await;
}

#[tokio::test]
async fn an_unusable_message_field_falls_back_to_a_valid_error_sentence() {
    for message in [
        serde_json::Value::Null,
        serde_json::json!(7),
        serde_json::json!(false),
        serde_json::json!([]),
        serde_json::json!({}),
        serde_json::json!(""),
        serde_json::json!("   "),
    ] {
        let bytes =
            serde_json::to_vec(&serde_json::json!({"message": message, "error": "disk is full"}))
                .unwrap();
        let fixture = Fixture::response(length_response(500, &bytes), false).await;
        let (status, answer) = get(
            &fixture.base,
            "",
            &ToolAuth::Anonymous,
            false,
            PROBE_DEADLINE,
        )
        .await;
        assert_eq!(status, Some(http::StatusCode::INTERNAL_SERVER_ERROR));
        assert_eq!(failed(answer), "disk is full");
        fixture.finish().await;
    }
    assert_eq!(
        http_failure_sentence(
            br#"{"message":"keep the message","error":"later error"}"#,
            http::StatusCode::INTERNAL_SERVER_ERROR
        ),
        "keep the message"
    );
}

#[tokio::test]
async fn direct_exchanges_update_only_their_task_local_meter_including_a_refused_chunk() {
    let mut oversize = format!(
        "HTTP/1.1 500 Fixture\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
        MAX_HTTP_ERROR_BYTES + 1
    )
    .into_bytes();
    oversize.resize(oversize.len() + MAX_HTTP_ERROR_BYTES + 1, b'x');
    oversize.extend_from_slice(b"\r\n0\r\n\r\n");
    let cases = [
        (length_response(200, b"ok"), 2),
        (
            length_response(500, b"server sentence"),
            b"server sentence".len(),
        ),
        (
            b"HTTP/1.1 403 Denied\r\nContent-Length: 999999\r\n\r\n".to_vec(),
            0,
        ),
        (oversize, MAX_HTTP_ERROR_BYTES + 1),
    ];
    let scheduler = std::sync::Arc::new(crate::telemetry::Scheduler::default());
    for (response, expected_bytes) in cases {
        let fixture = Fixture::response(response, false).await;
        let base = fixture.base.clone();
        let (send, receive) = oneshot::channel();
        let _stop = scheduler.submit(
            &tokio::runtime::Handle::current(),
            async move {
                let _answer = get(&base, "", &ToolAuth::Anonymous, false, PROBE_DEADLINE).await;
                Fetched::Ok(crate::telemetry::request_metrics().expect("admitted task-local meter"))
            },
            move |answer| {
                let _ = send.send(answer);
            },
        );
        let answer = tokio::time::timeout(PROBE_DEADLINE + FIXTURE_GUARD, receive)
            .await
            .expect("metered request exceeded its guard")
            .unwrap();
        let Fetched::Ok(metrics) = answer else {
            panic!("the admitted work must return its meter")
        };
        assert_eq!(metrics.requests, 1);
        assert_eq!(metrics.bytes, expected_bytes);
        assert!(metrics.network_ns > 0 && metrics.network_ns <= metrics.total_ns);
        assert!(
            crate::telemetry::request_metrics().is_none(),
            "the caller is outside each admitted task"
        );
        fixture.finish().await;
    }
    let (send, receive) = oneshot::channel();
    let _stop = scheduler.submit(
        &tokio::runtime::Handle::current(),
        async {
            let (_, answer) = get(
                "not-a-url-invented",
                "",
                &ToolAuth::Anonymous,
                false,
                PROBE_DEADLINE,
            )
            .await;
            assert!(!failed(answer).is_empty());
            Fetched::Ok(crate::telemetry::request_metrics().expect("admitted validation meter"))
        },
        move |answer| {
            let _ = send.send(answer);
        },
    );
    let Fetched::Ok(metrics) = tokio::time::timeout(FIXTURE_GUARD, receive)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("validation result")
    };
    assert_eq!(metrics.requests, 0);
    assert_eq!(metrics.bytes, 0);
    assert_eq!(metrics.network_ns, 0);
}

#[test]
fn invalid_explicit_ports_and_bracket_hosts_cannot_construct_a_token_request() {
    let auth = ToolAuth::NamedToken("invented-secret".into());
    for base in [
        "http://127.0.0.1:65536",
        "http://127.0.0.1:bad",
        "http://127.0.0.1:",
        "http://127.0.0.1:+80",
        "http://[::1]:65536",
        "http://[::1]:bad",
        "http://[::1]:",
        "http://[example.test]",
        "http://example[bad]/",
        "http://example[::1]/",
        "http://example]/",
        "http://[127.0.0.1]",
        "http://[fe80::1%25fixture]",
    ] {
        let why = settings_request(base, "", &auth, false)
            .err()
            .expect("no request may be constructed");
        assert!(!why.contains("invented-secret") && !why.contains(base));
        match bind_url(ToolKind::Grafana, base, auth.clone()) {
            ToolReach::Unbound(value) => {
                assert!(value.browser_url.is_none());
                assert!(!value.why.contains("invented-secret"));
            }
            _ => panic!("invalid endpoint syntax must be refused before a client exists"),
        }
    }
    for base in [
        "http://127.0.0.1:80",
        "http://127.0.0.1:65535",
        "http://[::1]",
        "http://[::1]:80",
        "http://[2001:db8::1]:65535",
    ] {
        let request =
            settings_request(base, "api/health", &auth, false).expect("valid endpoint request");
        assert_eq!(
            request.uri().authority().unwrap().as_str(),
            base.strip_prefix("http://").unwrap()
        );
    }
}

#[tokio::test]
async fn valid_explicit_ipv6_routes_only_to_its_owned_loopback_fixture() {
    let fixture = Fixture::new_at(
        "[::1]:0",
        vec![(Duration::ZERO, length_response(200, b"ipv6"))],
        false,
    )
    .await;
    let (status, answer) = get(
        &fixture.base,
        "api/health",
        &ToolAuth::NamedToken("invented-token".into()),
        false,
        PROBE_DEADLINE,
    )
    .await;
    assert_eq!(status, Some(http::StatusCode::OK));
    assert_eq!(answer, Fetched::Ok(b"ipv6".to_vec()));
    let base = fixture.base.clone();
    let wire = fixture.finish().await;
    assert_request(&wire, &base, "/api/health", Some("invented-token"), false);
}
