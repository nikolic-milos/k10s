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

fn jaeger_response(trace_id: &str) -> serde_json::Value {
    serde_json::json!({"data":[{"traceID":trace_id,"spans":[
        {"traceID":trace_id,"spanID":"0123456789abcdef","startTime":1,"duration":1},
        {"traceID":trace_id,"spanID":"fedcba9876543210","startTime":2,"duration":1,
         "references":[{"refType":"CHILD_OF","traceID":trace_id,"spanID":"0123456789abcdef"}]}
    ]}]})
}

fn jaeger_bound() -> Bound {
    Bound {
        kind: ToolKind::Jaeger,
        transport: Transport::Proxy {
            namespace: "observe".into(),
            service: "jaeger".into(),
            port: 16686,
        },
        ..bound()
    }
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
fn malformed_tempo_timestamps_are_named_without_a_compatibility_read() {
    for field in ["startTimeUnixNano", "endTimeUnixNano"] {
        for value in [
            serde_json::json!(-1),
            serde_json::json!("invalid"),
            serde_json::json!(true),
            serde_json::json!(1.5),
            serde_json::json!("18446744073709551616"),
        ] {
            let script = Script::default();
            let mut body = response();
            body["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0][field] = value;
            script.route("GET", V2, 200, body.to_string());
            script.route("GET", V1, 200, response().to_string());
            let answer =
                runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
            assert!(
                matches!(answer, Fetched::Failed { what:"tempo", why } if why.contains(field) && why.contains("unsigned 64-bit"))
            );
            assert_gets(&script, &[V2]);
        }
    }
}

#[test]
fn invalid_first_span_ids_refuse_the_whole_trace_without_compatibility_reads() {
    for field in ["traceId", "spanId", "parentSpanId"] {
        for invalid in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!(7)),
            Some(serde_json::json!("")),
            Some(serde_json::json!("aa")),
            Some(serde_json::json!("0".repeat(if field == "traceId" {
                32
            } else {
                16
            }))),
        ] {
            if field == "parentSpanId"
                && (invalid.as_ref().is_none_or(|value| value.is_null())
                    || invalid
                        .as_ref()
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|id| id.is_empty() || id == "0000000000000000"))
            {
                continue;
            }
            let script = Script::default();
            let mut body = response();
            let spans = body["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array_mut()
                .unwrap();
            spans.push(spans[0].clone());
            spans[1]["spanId"] = serde_json::json!("fedcba9876543210");
            spans[0].as_object_mut().unwrap().remove(field);
            if let Some(invalid) = invalid {
                spans[0][field] = invalid;
            }
            script.route("GET", V2, 200, body.to_string());
            script.route("GET", V1, 200, response().to_string());
            let answer =
                runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
            assert!(
                matches!(&answer, Fetched::Failed {what:"tempo",why} if why.contains(field) && why.contains("identifier")),
                "{answer:?}"
            );
            assert_gets(&script, &[V2]);
        }
    }
}

#[test]
fn protojson_default_zero_timestamps_still_need_only_one_trace_read() {
    let script = Script::default();
    let mut body = response();
    let span = body["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0]
        .as_object_mut()
        .unwrap();
    span.remove("startTimeUnixNano");
    span.insert("endTimeUnixNano".into(), serde_json::Value::Null);
    script.route("GET", V2, 200, body.to_string());
    let answer = runtime().block_on(async { traces::lookup(&script.client(), &bound(), ID).await });
    assert!(matches!(answer, Fetched::Ok(trace) if trace.trace_id == ID
        && trace.spans.len() == 1 && trace.spans[0].start_us == 0 && trace.spans[0].duration_us == 0));
    assert_gets(&script, &[V2]);
}

#[test]
fn malformed_and_foreign_jaeger_spans_do_not_become_a_successful_requested_trace() {
    let path = "/api/v1/namespaces/observe/services/jaeger:16686/proxy/api/traces/0123456789abcdef0123456789abcdef";
    let mut bound = bound();
    bound.kind = ToolKind::Jaeger;
    let Transport::Proxy { service, port, .. } = &mut bound.transport else {
        unreachable!()
    };
    *service = "jaeger".into();
    *port = 16686;
    for (spans, named) in [
        (serde_json::Value::Null, "Jaeger spans"),
        (serde_json::json!([null]), "Jaeger span"),
        (serde_json::json!([{}]), "Jaeger span traceID"),
        (
            serde_json::json!([{
                "traceID":ID,"spanID":"0123456789abcdef","startTime":-1,"duration":1
            }]),
            "Jaeger startTime",
        ),
        (
            serde_json::json!([{
                "traceID":ID,"spanID":"0123456789abcdef","startTime":1,"duration":"1"
            }]),
            "Jaeger duration",
        ),
        (
            serde_json::json!([
                {"traceID":ID,"spanID":"0123456789abcdef","startTime":1,"duration":1},
                {"traceID":"ffffffffffffffffffffffffffffffff","spanID":"fedcba9876543210","startTime":1,"duration":1}
            ]),
            "Jaeger span traceID",
        ),
    ] {
        let script = Script::default();
        script.route(
            "GET",
            path,
            200,
            serde_json::json!({"data":[{
                "traceID":ID,"spans":spans
            }]})
            .to_string(),
        );
        let answer =
            runtime().block_on(async { traces::lookup(&script.client(), &bound, ID).await });
        assert!(matches!(answer, Fetched::Failed { what:"jaeger", why } if why.contains(named)));
        assert_gets(&script, &[path]);
    }
}

#[test]
fn jaeger_both_trace_widths_keep_only_hierarchical_parents_on_one_exact_get() {
    for trace_id in ["0123456789abcdef", ID] {
        for parent_kind in ["CHILD_OF", "FOLLOWS_FROM", "direct"] {
            let script = Script::default();
            let path = format!(
                "/api/v1/namespaces/observe/services/jaeger:16686/proxy/api/traces/{trace_id}"
            );
            let mut body = jaeger_response(trace_id);
            let span = &mut body["data"][0]["spans"][1];
            if parent_kind == "direct" {
                span["references"] = serde_json::json!([]);
                span["parentSpanID"] = serde_json::json!("0123456789ABCDEF");
            } else {
                span["references"][0]["refType"] = serde_json::json!(parent_kind);
            }
            script.route("GET", &path, 200, body.to_string());
            let answer = runtime().block_on(async {
                traces::lookup(&script.client(), &jaeger_bound(), trace_id).await
            });
            let expected_parent = if parent_kind == "FOLLOWS_FROM" {
                ""
            } else {
                "0123456789abcdef"
            };
            assert!(
                matches!(&answer, Fetched::Ok(trace) if trace.trace_id == trace_id
                    && trace.spans.len() == 2 && trace.spans[1].parent == expected_parent
                    && trace.source.is_empty()),
                "{trace_id} {parent_kind}: {answer:?}"
            );
            assert_gets(&script, &[&path]);
        }
    }
}

#[test]
fn invalid_jaeger_ids_and_parent_references_refuse_after_one_exact_get() {
    let path = "/api/v1/namespaces/observe/services/jaeger:16686/proxy/api/traces/0123456789abcdef0123456789abcdef";
    for (target, key, invalid, named) in [
        (
            "trace",
            "traceID",
            serde_json::json!("0".repeat(32)),
            "Jaeger traceID",
        ),
        (
            "span",
            "traceID",
            serde_json::json!("0".repeat(16)),
            "Jaeger span traceID",
        ),
        (
            "span",
            "traceID",
            serde_json::json!("abc"),
            "Jaeger span traceID",
        ),
        (
            "span",
            "spanID",
            serde_json::json!("0".repeat(16)),
            "Jaeger spanID",
        ),
        ("span", "spanID", serde_json::json!(ID), "Jaeger spanID"),
        (
            "span",
            "spanID",
            serde_json::json!("qqGbfsPBsQA="),
            "Jaeger spanID",
        ),
        (
            "span",
            "parentSpanID",
            serde_json::json!("0".repeat(16)),
            "Jaeger parentSpanID",
        ),
        (
            "reference",
            "spanID",
            serde_json::Value::Null,
            "Jaeger CHILD_OF spanID",
        ),
        (
            "reference",
            "spanID",
            serde_json::json!(7),
            "Jaeger CHILD_OF spanID",
        ),
        (
            "reference",
            "spanID",
            serde_json::json!("abc"),
            "Jaeger CHILD_OF spanID",
        ),
        (
            "reference",
            "spanID",
            serde_json::json!("0".repeat(16)),
            "Jaeger CHILD_OF spanID",
        ),
        (
            "reference",
            "spanID",
            serde_json::json!("a".repeat(201)),
            "Jaeger CHILD_OF spanID",
        ),
        (
            "reference",
            "traceID",
            serde_json::json!("0".repeat(32)),
            "Jaeger CHILD_OF traceID",
        ),
        (
            "reference",
            "traceID",
            serde_json::json!("ffffffffffffffffffffffffffffffff"),
            "cross-trace parents are not supported",
        ),
        (
            "reference",
            "refType",
            serde_json::Value::Null,
            "Jaeger reference refType",
        ),
        (
            "span",
            "references",
            serde_json::json!({}),
            "Jaeger references",
        ),
    ] {
        let mut body = jaeger_response(ID);
        let target = match target {
            "trace" => &mut body["data"][0],
            "span" => &mut body["data"][0]["spans"][0],
            "reference" => &mut body["data"][0]["spans"][1]["references"][0],
            _ => unreachable!(),
        };
        target[key] = invalid;
        let script = Script::default();
        script.route("GET", path, 200, body.to_string());
        let answer = runtime()
            .block_on(async { traces::lookup(&script.client(), &jaeger_bound(), ID).await });
        assert!(
            matches!(&answer, Fetched::Failed { what:"jaeger", why } if why.contains(named)),
            "{key}: {answer:?}"
        );
        assert_gets(&script, &[path]);
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
