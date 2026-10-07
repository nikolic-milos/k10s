//! Completed 404 compatibility, complete trace identity and first-failure proof.

use crate::*;
use k10s_data::integrations::traces;
use k10s_data::reach::{Bound, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;

const ID: &str = "0123456789abcdef0123456789abcdef";
const V2: &str = "/api/v1/namespaces/observe/services/tempo:3200/proxy/api/v2/traces/0123456789abcdef0123456789abcdef";
const V1: &str = "/api/v1/namespaces/observe/services/tempo:3200/proxy/api/traces/0123456789abcdef0123456789abcdef";

fn bound() -> Bound {
    Bound {
        kind: ToolKind::Tempo,
        found: None,
        transport: Transport::Proxy {
            namespace: "observe".into(),
            service: "tempo".into(),
            port: 3200,
        },
        auth: ToolAuth::Anonymous,
    }
}

fn response() -> serde_json::Value {
    serde_json::json!({"trace":{"resourceSpans":[{"scopeSpans":[{"spans":[{
        "traceId":ID,"spanId":"0123456789abcdef","name":"request",
        "startTimeUnixNano":"1000","endTimeUnixNano":"2000"
    }]}]}]}})
}

fn assert_gets(script: &Script, paths: &[&str]) {
    let requests = script.seen();
    assert_eq!(requests.len(), paths.len(), "{requests:?}");
    for (request, path) in requests.iter().zip(paths) {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, *path);
        assert_eq!(request.accept, "application/json");
        assert_eq!(request.content_type, "");
        assert!(request.body.is_empty());
    }
}

#[test]
fn a_completed_v2_404_allows_exactly_one_v1_compatibility_read() {
    let script = Script::default();
    script.route("GET", V2, 404, r#"{"message":"v2 route unavailable"}"#);
    script.route("GET", V1, 200, response().to_string());
    let answer = runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
    assert!(matches!(answer, Fetched::Ok(trace) if trace.trace_id == ID && trace.spans.len() == 1));
    assert_gets(&script, &[V2, V1]);
}

#[test]
fn a_v2_server_failure_never_changes_routes_or_loses_its_sentence() {
    for status in [400, 406, 429, 500, 503, 504] {
        let script = Script::default();
        script.route(
            "GET",
            V2,
            status,
            r#"{"message":"trace storage unavailable"}"#,
        );
        script.route("GET", V1, 200, response().to_string());
        let answer =
            runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
        assert_eq!(
            answer,
            Fetched::Failed {
                what: "tempo",
                why: "trace storage unavailable".into()
            }
        );
        assert_gets(&script, &[V2]);
    }
}

#[test]
fn trace_denial_does_not_retry_another_version() {
    for status in [401, 403] {
        let script = Script::default();
        script.route("GET", V2, status, r#"{"message":"trace read denied"}"#);
        script.route("GET", V1, 200, response().to_string());
        let answer =
            runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
        assert_eq!(answer, Fetched::Denied { what: "tempo" });
        assert_gets(&script, &[V2]);
    }
}

#[test]
fn malformed_and_protobuf_bodies_are_refused_without_compatibility_reads() {
    for body in [
        b"{invalid".to_vec(),
        b"<html>gateway error</html>".to_vec(),
        vec![0, 1, 2],
    ] {
        let script = Script::default();
        script.route_bytes("GET", V2, 200, body);
        script.route("GET", V1, 200, response().to_string());
        let answer =
            runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
        assert!(matches!(answer, Fetched::Failed { .. }));
        assert_gets(&script, &[V2]);
    }
}

#[test]
fn an_oversize_resource_identity_cannot_be_clipped_or_trigger_a_fallback() {
    let script = Script::default();
    let mut body = response();
    body["trace"]["resourceSpans"][0]["resource"] = serde_json::json!({
        "attributes": [{"key":"k8s.pod.name","value":{"stringValue":"n".repeat(254)}}]
    });
    script.route("GET", V2, 200, body.to_string());
    script.route("GET", V1, 200, response().to_string());
    let answer = runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
    assert!(
        matches!(answer, Fetched::Failed {why,..} if why.contains("k8s.pod.name") && why.contains("253"))
    );
    assert_gets(&script, &[V2]);
}

#[test]
fn oversized_success_and_404_bodies_cannot_trigger_another_trace_read() {
    for (status, bytes) in [
        (200, k10s_data::reach::MAX_BODY_BYTES + 1),
        (404, (32 << 10) + 1),
    ] {
        let script = Script::default();
        script.route_bytes("GET", V2, status, vec![b' '; bytes]);
        script.route("GET", V1, 200, response().to_string());
        let answer =
            runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
        assert!(
            matches!(answer, Fetched::Failed { why, .. } if why.contains("bytes") && why.contains("refused"))
        );
        assert_gets(&script, &[V2]);
    }
}

#[test]
fn a_partial_tempo_envelope_keeps_the_servers_message_without_v1_fallback() {
    for status in [serde_json::json!("PARTIAL"), serde_json::json!(1)] {
        let script = Script::default();
        let mut body = response();
        body["status"] = status;
        body["message"] = serde_json::json!(
            "Trace exceeds maximum size of 10 bytes, a partial trace is returned"
        );
        script.route("GET", V2, 200, body.to_string());
        script.route("GET", V1, 200, response().to_string());
        let answer =
            runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
        assert_eq!(
            answer,
            Fetched::Failed {
                what: "tempo",
                why: "Trace exceeds maximum size of 10 bytes, a partial trace is returned".into()
            }
        );
        assert_gets(&script, &[V2]);
    }
}

#[test]
fn an_empty_200_trace_has_no_invented_identity_and_needs_only_one_read() {
    let script = Script::default();
    script.route("GET", V2, 200, r#"{"trace":{"resourceSpans":[]}}"#);
    script.route("GET", V1, 200, response().to_string());
    let answer = runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
    assert!(
        matches!(answer, Fetched::Ok(trace) if trace.trace_id.is_empty() && trace.spans.is_empty())
    );
    assert_gets(&script, &[V2]);
}

#[test]
fn an_empty_jaeger_span_set_cannot_hide_a_nonempty_mismatched_trace_id() {
    let script = Script::default();
    let mut bound = bound();
    bound.kind = ToolKind::Jaeger;
    let Transport::Proxy { service, port, .. } = &mut bound.transport else {
        unreachable!()
    };
    *service = "jaeger".into();
    *port = 16686;
    let path = "/api/v1/namespaces/observe/services/jaeger:16686/proxy/api/traces/0123456789abcdef0123456789abcdef";
    script.route(
        "GET",
        path,
        200,
        r#"{"data":[{"traceID":"ffffffffffffffffffffffffffffffff","spans":[]}]}"#,
    );
    let answer = runtime().block_on(async { traces::lookup(&script.client(), &bound, ID).await });
    assert!(matches!(answer, Fetched::Failed { why, .. } if why.contains("requested trace ID")));
    assert_gets(&script, &[path]);
}

#[test]
fn an_oversized_resource_identity_is_refused_without_a_second_lookup() {
    let script = Script::default();
    let mut body = response();
    body["trace"]["resourceSpans"][0]["resource"] = serde_json::json!({
        "attributes":[{"key":"k8s.pod.uid","value":{"stringValue":"u".repeat(254)}}]
    });
    script.route("GET", V2, 200, body.to_string());
    script.route("GET", V1, 200, response().to_string());
    let answer = runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
    assert!(matches!(answer, Fetched::Failed { why, .. } if why.contains("identity cap is 253")));
    assert_gets(&script, &[V2]);
}

#[tokio::test(start_paused = true)]
async fn a_trace_header_stall_has_one_exchange_deadline_and_no_fallback() {
    let script = Script::default();
    script.route_hanging("GET", V2);
    script.route("GET", V1, 200, response().to_string());
    let started = tokio::time::Instant::now();
    let answer = traces::lookup(&script.client(), &bound(), ID).await;
    assert_eq!(
        answer,
        Fetched::Failed {
            what: "tempo",
            why: "tempo did not answer within 4 seconds".into()
        }
    );
    assert_eq!(started.elapsed(), Duration::from_secs(4));
    assert_gets(&script, &[V2]);
}

#[tokio::test(start_paused = true)]
async fn a_trace_body_stall_uses_the_same_deadline_without_a_fallback() {
    use futures::StreamExt as _;
    let script = Script::default();
    script.route("GET", V2, 200, response().to_string());
    script.route("GET", V1, 200, response().to_string());
    let server = script.clone();
    let client = kube::Client::new(
        tower::service_fn(move |request: http::Request<Body>| {
            let mut server = server.clone();
            async move {
                let (parts, body) = server.call(request).await?.into_parts();
                let body = http_body_util::StreamBody::new(
                    http_body_util::BodyStream::new(body).chain(futures::stream::pending()),
                );
                Ok::<_, tower::BoxError>(http::Response::from_parts(parts, body))
            }
        }),
        "default",
    );
    let started = tokio::time::Instant::now();
    let answer = traces::lookup(&client, &bound(), ID).await;
    assert_eq!(
        answer,
        Fetched::Failed {
            what: "tempo",
            why: "tempo did not answer within 4 seconds".into()
        }
    );
    assert_eq!(started.elapsed(), Duration::from_secs(4));
    assert_gets(&script, &[V2]);
}
