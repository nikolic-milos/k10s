//! Tempo v1/v2 and Jaeger query JSON reduced to [`Trace`], including the
//! two refusals: an oversize body and a span list past [`MAX_SPANS`].
//! Protobuf OTLP is not a parse.

use super::*;

fn tempo_v2_json() -> &'static str {
    r#"{
      "trace": {
        "resourceSpans": [{
          "resource": {
            "attributes": [
              {"key": "service.name", "value": {"stringValue": "frontend"}},
              {"key": "token", "value": {"stringValue": "SUPERSECRET-ATTR"}}
            ]
          },
          "scopeSpans": [{
            "spans": [
              {
                "traceId": "5b8efff798038103d269b633813fc700",
                "spanId": "aaa19b7ec3c1b100",
                "name": "GET /api",
                "startTimeUnixNano": "1689969302000000000",
                "endTimeUnixNano": "1689970000000000000",
                "status": {"code": "STATUS_CODE_OK"},
                "events": [{"name": "SUPERSECRET-EVENT"}]
              },
              {
                "traceId": "5b8efff798038103d269b633813fc700",
                "spanId": "bbb19b7ec3c1b100",
                "parentSpanId": "aaa19b7ec3c1b100",
                "name": "db.query",
                "startTimeUnixNano": 1689969303000000000,
                "endTimeUnixNano": 1689969304000000000,
                "status": {"code": 2}
              }
            ]
          }]
        }]
      }
    }"#
}

fn tempo_v1_json() -> &'static str {
    r#"{
      "batches": [{
        "resource": {
          "attributes": [
            {"key": "service.name", "value": {"stringValue": "frontend"}}
          ]
        },
        "scopeSpans": [{
          "spans": [
            {
              "traceId": "5b8efff798038103d269b633813fc700",
              "spanId": "aaa19b7ec3c1b100",
              "name": "GET /api",
              "startTimeUnixNano": "1689969302000000000",
              "endTimeUnixNano": "1689970000000000000",
              "status": {"code": "STATUS_CODE_OK"}
            },
            {
              "traceId": "5b8efff798038103d269b633813fc700",
              "spanId": "bbb19b7ec3c1b100",
              "parentSpanId": "aaa19b7ec3c1b100",
              "name": "db.query",
              "startTimeUnixNano": 1689969303000000000,
              "endTimeUnixNano": 1689969304000000000,
              "status": {"code": 2}
            }
          ]
        }]
      }]
    }"#
}

fn jaeger_json() -> &'static str {
    r#"{
      "data": [{
        "traceID": "5b8efff798038103d269b633813fc700",
        "spans": [
          {
            "traceID": "5b8efff798038103d269b633813fc700",
            "spanID": "aaa19b7ec3c1b100",
            "operationName": "GET /api",
            "references": [],
            "startTime": 1689969302000000,
            "duration": 698000000,
            "tags": [
              {"key": "otel.status_code", "type": "string", "value": "ok"},
              {"key": "password", "type": "string", "value": "SUPERSECRET-TAG"}
            ],
            "processID": "p1",
            "logs": [{"timestamp": 1, "fields": [{"key": "event", "value": "SUPERSECRET-LOG"}]}]
          },
          {
            "traceID": "5b8efff798038103d269b633813fc700",
            "spanID": "bbb19b7ec3c1b100",
            "operationName": "db.query",
            "references": [{
              "refType": "CHILD_OF",
              "traceID": "5b8efff798038103d269b633813fc700",
              "spanID": "aaa19b7ec3c1b100"
            }],
            "startTime": 1689969303000000,
            "duration": 1000000,
            "tags": [{"key": "error", "type": "bool", "value": true}],
            "processID": "p1"
          }
        ],
        "processes": {
          "p1": {"serviceName": "frontend", "tags": []}
        }
      }],
      "total": 0,
      "limit": 0,
      "offset": 0,
      "errors": null
    }"#
}

fn expected_two_spans() -> Trace {
    Trace {
        trace_id: "5b8efff798038103d269b633813fc700".into(),
        spans: vec![
            Span {
                id: "aaa19b7ec3c1b100".into(),
                parent: String::new(),
                name: "GET /api".into(),
                service: "frontend".into(),
                start_us: 1_689_969_302_000_000,
                duration_us: 698_000_000,
                status: "ok".into(),
            },
            Span {
                id: "bbb19b7ec3c1b100".into(),
                parent: "aaa19b7ec3c1b100".into(),
                name: "db.query".into(),
                service: "frontend".into(),
                start_us: 1_689_969_303_000_000,
                duration_us: 1_000_000,
                status: "error".into(),
            },
        ],
    }
}

#[test]
fn a_tempo_v2_trace_becomes_spans_with_service_and_timing() {
    let trace = parse(tempo_v2_json().as_bytes()).expect("v2 json");
    assert_eq!(trace, expected_two_spans());
}

#[test]
fn a_tempo_v1_batches_document_reads_the_same_way() {
    let v1 = parse(tempo_v1_json().as_bytes()).expect("v1 json");
    let v2 = parse(tempo_v2_json().as_bytes()).expect("v2 json");
    assert_eq!(v1, v2);
    assert_eq!(v1, expected_two_spans());
}

#[test]
fn a_jaeger_query_document_joins_spans_to_processes() {
    let trace = parse(jaeger_json().as_bytes()).expect("jaeger json");
    assert_eq!(trace, expected_two_spans());
}

#[test]
fn auto_detect_reads_tempo_shape_before_jaeger_shape() {
    assert!(
        is_tempo_shape(&serde_json::from_str(tempo_v2_json()).unwrap()),
        "v2 is Tempo"
    );
    assert!(
        is_jaeger_shape(&serde_json::from_str(jaeger_json()).unwrap()),
        "query envelope is Jaeger"
    );
    assert_eq!(
        parse(tempo_v1_json().as_bytes()).unwrap().trace_id,
        parse(jaeger_json().as_bytes()).unwrap().trace_id
    );
}

#[test]
fn instrumentation_library_spans_are_walked() {
    let json = r#"{
      "batches": [{
        "resource": {"serviceName": "api"},
        "instrumentationLibrarySpans": [{
          "spans": [{
            "traceId": "ab",
            "spanId": "cd",
            "name": "work",
            "startTimeUnixNano": "1000000",
            "endTimeUnixNano": "2000000"
          }]
        }]
      }]
    }"#;
    let trace = parse(json.as_bytes()).expect("old otel field");
    assert_eq!(trace.trace_id, "ab");
    assert_eq!(trace.spans.len(), 1);
    assert_eq!(trace.spans[0].service, "api");
    assert_eq!(trace.spans[0].start_us, 1_000);
    assert_eq!(trace.spans[0].duration_us, 1_000);
}

#[test]
fn protobuf_json_ids_are_shown_as_hex() {
    let json = r#"{
      "trace": {"resourceSpans": [{
        "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "my.service"}}]},
        "scopeSpans": [{"spans": [{
          "traceId": "W47/95gDgQPSabYzgT/HAA==",
          "spanId": "7uGbfsPBsQA=",
          "name": "I am a span!",
          "kind": "SPAN_KIND_SERVER",
          "startTimeUnixNano": "1689969302000000000",
          "endTimeUnixNano": "1689970000000000000",
          "status": {}
        }]}]
      }]}
    }"#;
    let trace = parse(json.as_bytes()).expect("tempo docs v2");
    assert_eq!(trace.trace_id, "5b8efff798038103d269b633813fc700");
    assert_eq!(trace.spans[0].id, "eee19b7ec3c1b100");
    assert!(trace.spans[0].status.is_empty(), "unset status stays empty");
}

#[test]
fn span_attributes_and_events_do_not_survive() {
    let json = tempo_v2_json();
    assert!(
        json.contains("SUPERSECRET"),
        "the fixture has to contain what must not come out"
    );
    let trace = parse(json.as_bytes()).expect("decodes");
    let rendered = format!("{trace:?}");
    assert!(
        !rendered.contains("SUPERSECRET"),
        "attributes, events and logs are dropped at the boundary: {rendered}"
    );
}

#[test]
fn a_jaeger_error_tag_does_not_leak_into_the_returned_type_beyond_status() {
    let json = jaeger_json();
    assert!(json.contains("SUPERSECRET"));
    let trace = parse(json.as_bytes()).expect("decodes");
    let rendered = format!("{trace:?}");
    assert!(!rendered.contains("SUPERSECRET"), "{rendered}");
    assert_eq!(trace.spans[1].status, "error");
}

#[test]
fn an_oversize_body_is_refused_not_truncated() {
    let huge = vec![b'x'; MAX_BODY_BYTES + 1];
    match parse(&huge) {
        Err(TraceError::TooLarge { bytes }) => assert_eq!(bytes, MAX_BODY_BYTES + 1),
        other => panic!("{other:?}"),
    }
}

fn otel_spans_document(n: usize) -> String {
    let one = r#"{"spanId":"aa","name":"x","startTimeUnixNano":"1000","endTimeUnixNano":"2000"}"#;
    let mut spans = String::new();
    for i in 0..n {
        if i > 0 {
            spans.push(',');
        }
        spans.push_str(one);
    }
    format!(r#"{{"resourceSpans":[{{"scopeSpans":[{{"spans":[{spans}]}}]}}]}}"#)
}

#[test]
fn a_trace_with_too_many_spans_is_refused() {
    match parse(otel_spans_document(MAX_SPANS + 1).as_bytes()) {
        Err(TraceError::TooManySpans) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn exactly_the_span_cap_is_kept() {
    let trace = parse(otel_spans_document(MAX_SPANS).as_bytes()).expect("at the cap");
    assert_eq!(trace.spans.len(), MAX_SPANS);
}

#[test]
fn protobuf_otlp_is_refused_unless_it_is_already_json() {
    match parse(&[0x0a, 0x10, 0xff, 0xfe, 0x00]) {
        Err(TraceError::Protobuf) => {}
        other => panic!("{other:?}"),
    }
    match parse(b"[1,2,3]") {
        Err(TraceError::NotATrace) => {}
        other => panic!("a JSON array is not a trace object: {other:?}"),
    }
    match parse(b"{\"unrelated\":1}") {
        Err(TraceError::NotATrace) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn jaeger_errors_without_data_are_a_reason_not_an_empty_trace() {
    let json = r#"{"data":[],"errors":[{"code":404,"msg":"trace not found"}]}"#;
    match parse(json.as_bytes()) {
        Err(TraceError::Rejected(why)) => assert!(why.contains("trace not found"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_empty_tempo_v2_document_is_an_empty_trace() {
    let json = r#"{"trace":{"resourceSpans":[]}}"#;
    let trace = parse(json.as_bytes()).expect("v2 not-found is 200 with empty");
    assert!(trace.spans.is_empty());
    assert!(trace.trace_id.is_empty());
}

#[test]
fn one_enormous_name_is_clipped_where_it_is_carried() {
    let huge = "n".repeat(6 << 10);
    let json = format!(
        r#"{{"resourceSpans":[{{"scopeSpans":[{{"spans":[{{"spanId":"aa","name":"{huge}"}}]}}]}}]}}"#
    );
    let trace = parse(json.as_bytes()).expect("legal body");
    assert!(
        trace.spans[0].name.chars().count() <= MAX_FIELD_CHARS,
        "{} chars",
        trace.spans[0].name.chars().count()
    );
}

#[test]
fn tempo_tries_v2_then_v1_and_jaeger_uses_the_query_path() {
    match lookup_paths(ToolKind::Tempo, "5b8efff798038103d269b633813fc700") {
        Fetched::Ok(paths) => assert_eq!(
            paths,
            [
                "api/v2/traces/5b8efff798038103d269b633813fc700",
                "api/traces/5b8efff798038103d269b633813fc700",
            ]
        ),
        other => panic!("{other:?}"),
    }
    match lookup_paths(ToolKind::Jaeger, "abc") {
        Fetched::Ok(paths) => assert_eq!(paths, ["api/traces/abc"]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_non_hex_id_is_not_sent() {
    match lookup_paths(ToolKind::Tempo, "not a trace/id") {
        Fetched::Failed { why, .. } => assert!(why.contains("hex"), "{why}"),
        other => panic!("{other:?}"),
    }
    match lookup_paths(ToolKind::Tempo, "") {
        Fetched::Failed { .. } => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn grafana_is_not_a_trace_store() {
    match lookup_paths(ToolKind::Grafana, "ab") {
        Fetched::Failed { why, .. } => assert!(why.contains("Grafana"), "{why}"),
        other => panic!("{other:?}"),
    }
}
