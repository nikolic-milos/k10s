//! Tempo v1/v2 and Jaeger query JSON reduced to [`Trace`], including the
//! body/span bounds, exact resource identities and named malformed-field refusals.
//! ProtoJSON defaults remain legal; protobuf OTLP is not a parse.

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
        source: String::new(),
        spans: vec![
            Span {
                namespace: String::new(),
                pod: String::new(),
                pod_uid: String::new(),
                node: String::new(),
                id: "aaa19b7ec3c1b100".into(),
                parent: String::new(),
                name: "GET /api".into(),
                service: "frontend".into(),
                start_us: 1_689_969_302_000_000,
                duration_us: 698_000_000,
                status: "ok".into(),
            },
            Span {
                namespace: String::new(),
                pod: String::new(),
                pod_uid: String::new(),
                node: String::new(),
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
            "traceId": "5b8efff798038103d269b633813fc700",
            "spanId": "aaa19b7ec3c1b100",
            "name": "work",
            "startTimeUnixNano": "1000000",
            "endTimeUnixNano": "2000000"
          }]
        }]
      }]
    }"#;
    let trace = parse(json.as_bytes()).expect("old otel field");
    assert_eq!(trace.trace_id, "5b8efff798038103d269b633813fc700");
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
fn every_tempo_span_must_have_valid_ids_before_a_later_span_can_name_the_trace() {
    for (field, alias) in [("traceId", "trace_id"), ("spanId", "span_id")] {
        for key in [field, alias] {
            for invalid in [
                None,
                Some(Value::Null),
                Some(serde_json::json!(7)),
                Some(serde_json::json!("")),
                Some(serde_json::json!("raw ID is not encoded")),
                Some(serde_json::json!("aa")),
                Some(serde_json::json!("0".repeat(if field == "traceId" {
                    32
                } else {
                    16
                }))),
                Some(serde_json::json!("a".repeat(201))),
                Some(serde_json::json!(if field == "traceId" {
                    "AAAAAAAAAAAAAAAAAAAAAA=="
                } else {
                    "AAAAAAAAAAA="
                })),
            ] {
                let mut body: Value = serde_json::from_str(tempo_v2_json()).unwrap();
                let span = &mut body["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
                span.as_object_mut().unwrap().remove(field);
                if let Some(invalid) = invalid {
                    span[key] = invalid;
                }
                let error = parse(&serde_json::to_vec(&body).unwrap()).unwrap_err();
                assert!(
                    matches!(error, TraceError::InvalidField {field: named,..} if named == field),
                    "{key}: {error:?}"
                );
                assert!(error.to_string().contains(field));
                assert!(!error.to_string().contains("raw ID"));
            }
        }
    }
}

#[test]
fn tempo_ids_preserve_supported_hex_and_base64_forms_without_clipping() {
    for (trace_id, span_id) in [
        ("5B8EFFF798038103D269B633813FC700", "AAA19B7EC3C1B100"),
        ("W47/95gDgQPSabYzgT/HAA==", "qqGbfsPBsQA="),
        ("W47/95gDgQPSabYzgT/HAA", "qqGbfsPBsQA"),
        ("W47_95gDgQPSabYzgT_HAA==", "qqGbfsPBsQA="),
        ("W47_95gDgQPSabYzgT_HAA", "qqGbfsPBsQA"),
    ] {
        let mut body: Value = serde_json::from_str(tempo_v2_json()).unwrap();
        let span = &mut body["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        span["traceId"] = serde_json::json!(trace_id);
        span["spanId"] = serde_json::json!(span_id);
        let trace = parse(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(trace, expected_two_spans());
    }
}

#[test]
fn tempo_parent_ids_allow_root_defaults_and_refuse_invalid_nonempty_values() {
    for root in [
        None,
        Some(Value::Null),
        Some(serde_json::json!("")),
        Some(serde_json::json!("0000000000000000")),
        Some(serde_json::json!("AAAAAAAAAAA=")),
    ] {
        let mut body: Value = serde_json::from_str(tempo_v2_json()).unwrap();
        if let Some(root) = root {
            body["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["parentSpanId"] = root;
        }
        let trace = parse(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(trace.spans[0].parent.is_empty());
    }
    for key in ["parentSpanId", "parent_span_id"] {
        for invalid in [
            serde_json::json!(7),
            serde_json::json!("aa"),
            serde_json::json!("raw parent ID"),
            serde_json::json!("00"),
            serde_json::json!("a".repeat(201)),
            serde_json::json!("W47/95gDgQPSabYzgT/HAA=="),
        ] {
            let mut body: Value = serde_json::from_str(tempo_v2_json()).unwrap();
            let span = &mut body["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][1];
            span.as_object_mut().unwrap().remove("parentSpanId");
            span[key] = invalid;
            let error = parse(&serde_json::to_vec(&body).unwrap()).unwrap_err();
            assert!(
                matches!(
                    error,
                    TraceError::InvalidField {
                        field: "parentSpanId",
                        ..
                    }
                ),
                "{key}: {error:?}"
            );
        }
    }
    let mut body: Value = serde_json::from_str(tempo_v2_json()).unwrap();
    body["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][1]["parentSpanId"] =
        serde_json::json!("qqGbfsPBsQA=");
    assert_eq!(
        parse(&serde_json::to_vec(&body).unwrap()).unwrap(),
        expected_two_spans()
    );
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
    let mut spans = String::new();
    for i in 0..n {
        if i > 0 {
            spans.push(',');
        }
        spans.push_str(&format!(
            r#"{{"traceId":"5b8efff798038103d269b633813fc700","spanId":"{:016x}","name":"x","startTimeUnixNano":"1000","endTimeUnixNano":"2000"}}"#,
            i + 1,
        ));
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
fn a_malformed_tempo_span_container_is_not_an_empty_trace() {
    for json in [
        r#"{"trace":false}"#,
        r#"{"batches":{}}"#,
        r#"{"resourceSpans":false}"#,
        r#"{"resourceSpans":[null]}"#,
        r#"{"resourceSpans":[{"scopeSpans":false}]}"#,
        r#"{"resourceSpans":[{"scopeSpans":[null]}]}"#,
        r#"{"resourceSpans":[{"scopeSpans":[{"spans":false}]}]}"#,
        r#"{"resourceSpans":[{"scopeSpans":[{"spans":[null]}]}]}"#,
    ] {
        assert_eq!(parse(json.as_bytes()), Err(TraceError::NotATrace), "{json}");
    }
}

#[test]
fn protojson_empty_messages_and_null_lists_keep_the_empty_trace_state() {
    for json in [
        r#"{"trace":null}"#,
        r#"{"trace":{}}"#,
        r#"{"trace":{"resourceSpans":null}}"#,
        r#"{"resourceSpans":[{}]}"#,
        r#"{"resourceSpans":[{"scopeSpans":null}]}"#,
        r#"{"resourceSpans":[{"scopeSpans":[{"spans":null}]}]}"#,
    ] {
        let trace = parse(json.as_bytes()).unwrap();
        assert!(
            trace.trace_id.is_empty() && trace.spans.is_empty(),
            "{json}"
        );
        assert!(trace.source.is_empty());
    }
}

#[test]
fn invalid_tempo_timestamps_cannot_become_zero_or_a_rounded_integer() {
    for (field, alias) in [
        ("startTimeUnixNano", "start_time_unix_nano"),
        ("endTimeUnixNano", "end_time_unix_nano"),
    ] {
        for key in [field, alias] {
            for invalid in [
                serde_json::json!(-1),
                serde_json::json!("-1"),
                serde_json::json!("invalid"),
                serde_json::json!(""),
                serde_json::json!(true),
                serde_json::json!({}),
                serde_json::json!([]),
                serde_json::json!(1.5),
                serde_json::json!("1.5"),
                serde_json::json!("18446744073709551616"),
                serde_json::json!("18446744073709551615.1"),
                serde_json::json!("1e100"),
                serde_json::json!("NaN"),
            ] {
                let mut value: Value = serde_json::from_str(tempo_v2_json()).unwrap();
                let span = &mut value["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
                span.as_object_mut().unwrap().remove(field);
                span[key] = invalid;
                let error = parse(&serde_json::to_vec(&value).unwrap()).unwrap_err();
                assert!(
                    matches!(error, TraceError::InvalidField { field: named, .. } if named == field),
                    "{key}: {value}: {error:?}"
                );
                assert!(error.to_string().contains(field));
            }
        }
    }
}

#[test]
fn unquoted_decimal_and_exponent_timestamps_are_refused_before_rounding() {
    for (field, alias) in [
        ("startTimeUnixNano", "start_time_unix_nano"),
        ("endTimeUnixNano", "end_time_unix_nano"),
    ] {
        for key in [field, alias] {
            for numeric in [
                "1.00000000000000001",
                "9007199254740993.0",
                "9007199254740993e0",
                "18446744073709551615.0",
                "18446744073709551616",
                "18446744073709551616.0",
                "1000.0",
                "1e3",
                "0.0",
                "0e0",
            ] {
                let (start_key, start, end_key, end) = if field == "startTimeUnixNano" {
                    (key, numeric, "endTimeUnixNano", "18446744073709551615")
                } else {
                    ("startTimeUnixNano", "0", key, numeric)
                };
                let body = format!(
                    r#"{{"resourceSpans":[{{"scopeSpans":[{{"spans":[{{
                        "traceId":"5b8efff798038103d269b633813fc700",
                        "spanId":"aaa19b7ec3c1b100",
                        "{start_key}":{start},"{end_key}":{end}
                    }}]}}]}}]}}"#,
                );
                let error = parse(body.as_bytes()).unwrap_err();
                assert!(
                    matches!(error, TraceError::InvalidField { field: named, .. } if named == field),
                    "{key}: {numeric}: {error:?}"
                );
            }
        }
    }
}

#[test]
fn protojson_timestamps_keep_omitted_null_and_explicit_zero_defaults() {
    for zero in [
        None,
        Some(Value::Null),
        Some(serde_json::json!(0)),
        Some(serde_json::json!("0")),
        Some(serde_json::json!("0.0e10")),
    ] {
        let mut value: Value = serde_json::from_str(tempo_v2_json()).unwrap();
        value["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"] = serde_json::json!([{
            "traceId":"5b8efff798038103d269b633813fc700", "spanId":"aaa19b7ec3c1b100"
        }]);
        let span = &mut value["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        if let Some(zero) = zero {
            span["startTimeUnixNano"] = zero.clone();
            span["endTimeUnixNano"] = zero;
        }
        let trace = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            (trace.spans[0].start_us, trace.spans[0].duration_us),
            (0, 0)
        );
        assert!(trace.source.is_empty());
    }
}

#[test]
fn unsigned_json_integer_timestamps_preserve_u64_precision() {
    let mut value: Value = serde_json::from_str(tempo_v2_json()).unwrap();
    let span = &mut value["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    span["startTimeUnixNano"] = serde_json::json!(u64::MAX - 1_000);
    span["endTimeUnixNano"] = serde_json::json!(u64::MAX);
    let trace = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(trace.spans[0].start_us, (u64::MAX - 1_000) / 1_000);
    assert_eq!(trace.spans[0].duration_us, 1);
}

#[test]
fn quoted_timestamps_preserve_u64_precision_before_conversion_to_microseconds() {
    let mut value: Value = serde_json::from_str(tempo_v2_json()).unwrap();
    let span = &mut value["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    span["startTimeUnixNano"] = serde_json::json!("18446744073709550615.0");
    span["endTimeUnixNano"] = serde_json::json!("184467440737095516150e-1");
    let trace = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(trace.spans[0].start_us, (u64::MAX - 1_000) / 1_000);
    assert_eq!(trace.spans[0].duration_us, 1);
}

#[test]
fn quoted_integral_protojson_exponents_and_zero_fractional_parts_keep_their_value() {
    for (start, end) in [
        (serde_json::json!("1000"), serde_json::json!("2e3")),
        (serde_json::json!("1e3"), serde_json::json!("2000.0")),
        (serde_json::json!("10000e-1"), serde_json::json!("2.000e+3")),
    ] {
        let mut value: Value = serde_json::from_str(tempo_v2_json()).unwrap();
        let span = &mut value["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        span["startTimeUnixNano"] = start;
        span["endTimeUnixNano"] = end;
        let trace = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            (trace.spans[0].start_us, trace.spans[0].duration_us),
            (1, 1)
        );
    }
}

#[test]
fn a_valid_numeric_tempo_span_that_ends_before_it_starts_is_refused() {
    let mut value: Value = serde_json::from_str(tempo_v2_json()).unwrap();
    let span = &mut value["trace"]["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    span["startTimeUnixNano"] = serde_json::json!("2000");
    span["endTimeUnixNano"] = serde_json::json!("1000");
    assert_eq!(
        parse(&serde_json::to_vec(&value).unwrap()),
        Err(TraceError::Rejected("a span ends before it starts".into()))
    );
}

#[test]
fn malformed_jaeger_trace_and_span_containers_are_named_refusals() {
    for (json, field) in [
        (r#"{"data":null}"#, "Jaeger data"),
        (r#"{"data":[null]}"#, "Jaeger data"),
        (r#"{"data":[{},{}]}"#, "Jaeger data"),
        (r#"{"data":[{}]}"#, "Jaeger spans"),
        (r#"{"data":[{"spans":null}]}"#, "Jaeger spans"),
        (r#"{"data":[{"spans":false}]}"#, "Jaeger spans"),
        (r#"{"data":[{"spans":[null]}]}"#, "Jaeger span"),
        (r#"{"data":[{"spans":["span"]}]}"#, "Jaeger span"),
        (r#"{"data":[{"spans":[{}]}]}"#, "Jaeger span traceID"),
    ] {
        let error = parse(json.as_bytes()).unwrap_err();
        assert!(
            matches!(error, TraceError::InvalidField { field: named, .. } if named == field),
            "{json}: {error:?}"
        );
        assert!(error.to_string().contains(field));
    }
}

#[test]
fn malformed_jaeger_timestamps_are_not_defaulted_or_coerced_from_strings() {
    for (key, field) in [
        ("startTime", "Jaeger startTime"),
        ("duration", "Jaeger duration"),
    ] {
        for invalid in [
            None,
            Some(Value::Null),
            Some(serde_json::json!(-1)),
            Some(serde_json::json!("1")),
            Some(serde_json::json!(true)),
            Some(serde_json::json!({})),
            Some(serde_json::json!([])),
            Some(serde_json::json!(1.0)),
            Some(serde_json::json!(1.5)),
        ] {
            let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
            let span = &mut value["data"][0]["spans"][0];
            span.as_object_mut().unwrap().remove(key);
            if let Some(invalid) = invalid {
                span[key] = invalid;
            }
            let error = parse(&serde_json::to_vec(&value).unwrap()).unwrap_err();
            assert!(
                matches!(error, TraceError::InvalidField { field: named, .. } if named == field),
                "{key}: {value}: {error:?}"
            );
        }
    }
}

#[test]
fn jaeger_spans_must_name_the_same_trace_and_a_hexadecimal_span_id() {
    for (key, field) in [
        ("traceID", "Jaeger span traceID"),
        ("spanID", "Jaeger spanID"),
    ] {
        for invalid in [
            None,
            Some(Value::Null),
            Some(serde_json::json!(7)),
            Some(serde_json::json!("invalid")),
            Some(serde_json::json!("")),
            Some(serde_json::json!("abc")),
            Some(serde_json::json!("a".repeat(15))),
            Some(serde_json::json!("a".repeat(17))),
            Some(serde_json::json!("a".repeat(33))),
            Some(serde_json::json!("a".repeat(201))),
            Some(serde_json::json!("0".repeat(16))),
            Some(serde_json::json!("0".repeat(32))),
            Some(serde_json::json!("W47/95gDgQPSabYzgT/HAA==")),
            Some(serde_json::json!("qqGbfsPBsQA=")),
        ] {
            let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
            let span = &mut value["data"][0]["spans"][0];
            span.as_object_mut().unwrap().remove(key);
            if let Some(invalid) = invalid {
                span[key] = invalid;
            }
            let error = parse(&serde_json::to_vec(&value).unwrap()).unwrap_err();
            assert!(
                matches!(error, TraceError::InvalidField { field: named, .. } if named == field),
                "{key}: {value}: {error:?}"
            );
        }
    }
    let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
    value["data"][0]["spans"][1]["traceID"] = serde_json::json!("ffffffffffffffffffffffffffffffff");
    assert_eq!(
        parse(&serde_json::to_vec(&value).unwrap()),
        Err(TraceError::Rejected(
            "Jaeger span traceID does not match the returned trace ID".into()
        ))
    );
}

#[test]
fn jaeger_trace_ids_preserve_both_documented_widths_and_span_ids_remain_64_bit() {
    for trace_id in ["5b8efff798038103", "5b8efff798038103d269b633813fc700"] {
        let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
        value["data"][0]["traceID"] = serde_json::json!(trace_id.to_ascii_uppercase());
        for span in value["data"][0]["spans"].as_array_mut().unwrap() {
            span["traceID"] = serde_json::json!(trace_id.to_ascii_uppercase());
        }
        value["data"][0]["spans"][1]["references"][0]["traceID"] =
            serde_json::json!(trace_id.to_ascii_uppercase());
        value["data"][0]["spans"][1]["references"][0]["spanID"] =
            serde_json::json!("AAA19B7EC3C1B100");
        let trace = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(trace.trace_id, trace_id);
        assert_eq!(trace.spans[1].parent, "aaa19b7ec3c1b100");
    }
    for key in ["traceID", "traceId"] {
        for invalid in [
            "abc",
            "0000000000000000",
            "00000000000000000000000000000000",
        ] {
            let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
            value["data"][0].as_object_mut().unwrap().remove("traceID");
            value["data"][0][key] = serde_json::json!(invalid);
            assert!(matches!(
                parse(&serde_json::to_vec(&value).unwrap()),
                Err(TraceError::InvalidField {
                    field: "Jaeger traceID",
                    ..
                })
            ));
        }
    }
    let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
    value["data"][0]["spans"][0]["spanID"] = serde_json::json!("5b8efff798038103d269b633813fc700");
    assert!(matches!(
        parse(&serde_json::to_vec(&value).unwrap()),
        Err(TraceError::InvalidField {
            field: "Jaeger spanID",
            ..
        })
    ));
}

#[test]
fn jaeger_hierarchical_parents_refuse_invalid_ids_and_cross_trace_references() {
    for invalid in [
        None,
        Some(Value::Null),
        Some(serde_json::json!(7)),
        Some(serde_json::json!("")),
        Some(serde_json::json!("abc")),
        Some(serde_json::json!("0000000000000000")),
        Some(serde_json::json!("5b8efff798038103d269b633813fc700")),
        Some(serde_json::json!("qqGbfsPBsQA=")),
        Some(serde_json::json!("a".repeat(201))),
    ] {
        let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
        let reference = &mut value["data"][0]["spans"][1]["references"][0];
        reference.as_object_mut().unwrap().remove("spanID");
        if let Some(invalid) = invalid {
            reference["spanID"] = invalid;
        }
        let answer = parse(&serde_json::to_vec(&value).unwrap());
        assert!(
            matches!(
                answer,
                Err(TraceError::InvalidField {
                    field: "Jaeger CHILD_OF spanID",
                    ..
                })
            ),
            "{answer:?}"
        );
    }
    for key in ["parentSpanID", "parentSpanId"] {
        let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
        value["data"][0]["spans"][1]["references"] = serde_json::json!([]);
        value["data"][0]["spans"][1][key] = serde_json::json!("AAA19B7EC3C1B100");
        assert_eq!(
            parse(&serde_json::to_vec(&value).unwrap()).unwrap().spans[1].parent,
            "aaa19b7ec3c1b100"
        );
        for invalid in [
            Value::Null,
            serde_json::json!(7),
            serde_json::json!("abc"),
            serde_json::json!("0000000000000000"),
            serde_json::json!("qqGbfsPBsQA="),
        ] {
            value["data"][0]["spans"][1][key] = invalid;
            let answer = parse(&serde_json::to_vec(&value).unwrap());
            assert!(
                matches!(
                    answer,
                    Err(TraceError::InvalidField {
                        field: "Jaeger parentSpanID",
                        ..
                    })
                ),
                "{answer:?}"
            );
        }
    }
    let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
    value["data"][0]["spans"][1]["references"][0]["traceID"] =
        serde_json::json!("ffffffffffffffffffffffffffffffff");
    assert_eq!(
        parse(&serde_json::to_vec(&value).unwrap()),
        Err(TraceError::Rejected(
            "Jaeger CHILD_OF refers to another trace; cross-trace parents are not supported".into()
        ))
    );
    for invalid in [
        None,
        Some(Value::Null),
        Some(serde_json::json!("0000000000000000")),
    ] {
        let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
        let reference = &mut value["data"][0]["spans"][1]["references"][0];
        reference.as_object_mut().unwrap().remove("traceID");
        if let Some(invalid) = invalid {
            reference["traceID"] = invalid;
        }
        assert!(matches!(
            parse(&serde_json::to_vec(&value).unwrap()),
            Err(TraceError::InvalidField {
                field: "Jaeger CHILD_OF traceID",
                ..
            })
        ));
    }
}

#[test]
fn jaeger_follows_from_is_not_an_ancestor_and_missing_reference_types_refuse() {
    let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
    value["data"][0]["spans"][1]["references"][0]["refType"] = serde_json::json!("FOLLOWS_FROM");
    let trace = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(trace.spans[1].parent.is_empty());
    for invalid in [
        None,
        Some(Value::Null),
        Some(serde_json::json!("")),
        Some(serde_json::json!("UNKNOWN")),
    ] {
        let reference = &mut value["data"][0]["spans"][1]["references"][0];
        reference.as_object_mut().unwrap().remove("refType");
        if let Some(invalid) = invalid {
            reference["refType"] = invalid;
        }
        assert!(matches!(
            parse(&serde_json::to_vec(&value).unwrap()),
            Err(TraceError::InvalidField {
                field: "Jaeger reference refType",
                ..
            })
        ));
    }
}

#[test]
fn jaeger_explicit_zero_timing_and_case_insensitive_hex_ids_are_retained() {
    let mut value: Value = serde_json::from_str(jaeger_json()).unwrap();
    let span = &mut value["data"][0]["spans"][0];
    span["startTime"] = serde_json::json!(0);
    span["duration"] = serde_json::json!(0);
    span["traceID"] = serde_json::json!("5B8EFFF798038103D269B633813FC700");
    let trace = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(
        (trace.spans[0].start_us, trace.spans[0].duration_us),
        (0, 0)
    );
    assert_eq!(trace.trace_id, "5b8efff798038103d269b633813fc700");
    assert!(trace.source.is_empty());
}

#[test]
fn tempo_completion_status_is_preserved_as_a_refusal_when_partial_or_unknown() {
    for status in [serde_json::json!("PARTIAL"), serde_json::json!(1)] {
        let mut body: Value = serde_json::from_str(tempo_v2_json()).unwrap();
        body["status"] = status;
        body["message"] = serde_json::json!("the trace exceeded its server size limit");
        assert_eq!(
            parse(&serde_json::to_vec(&body).unwrap()),
            Err(TraceError::Rejected(
                "the trace exceeded its server size limit".into()
            ))
        );
    }
    for status in [serde_json::json!("COMPLETE"), serde_json::json!(0)] {
        let mut body: Value = serde_json::from_str(tempo_v2_json()).unwrap();
        body["status"] = status;
        assert!(
            !parse(&serde_json::to_vec(&body).unwrap())
                .unwrap()
                .spans
                .is_empty()
        );
    }
    let mut body: Value = serde_json::from_str(tempo_v2_json()).unwrap();
    body["status"] = serde_json::json!("unknown");
    assert!(
        matches!(parse(&serde_json::to_vec(&body).unwrap()), Err(TraceError::Rejected(why)) if why.contains("unsupported trace completion status"))
    );
}

#[test]
fn one_enormous_name_is_clipped_where_it_is_carried() {
    let huge = "n".repeat(6 << 10);
    let json = format!(
        r#"{{"resourceSpans":[{{"scopeSpans":[{{"spans":[{{"traceId":"5b8efff798038103d269b633813fc700","spanId":"aaa19b7ec3c1b100","name":"{huge}"}}]}}]}}]}}"#
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

#[test]
fn kubernetes_resource_names_keep_their_complete_identity() {
    let name = "n".repeat(MAX_RESOURCE_IDENTITY_BYTES);
    let mut value: Value = serde_json::from_str(tempo_v2_json()).unwrap();
    let attributes = value["trace"]["resourceSpans"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap();
    for key in ["k8s.pod.name", "k8s.node.name"] {
        attributes.push(serde_json::json!({"key": key, "value": {"stringValue": name}}));
    }
    attributes
        .push(serde_json::json!({"key": "k8s.namespace.name", "value": {"stringValue": "pilot"}}));
    attributes.push(
        serde_json::json!({"key": "k8s.pod.uid", "value": {"stringValue": "current-pod-uid"}}),
    );
    let trace = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
    for span in &trace.spans {
        assert_eq!(span.pod, name);
        assert_eq!(span.node, name);
        assert_eq!(span.namespace, "pilot");
        assert_eq!(span.pod_uid, "current-pod-uid");
    }
    assert!(!format!("{trace:?}").contains("SUPERSECRET"));
}

#[test]
fn an_oversize_resource_identity_is_refused_instead_of_matching_a_prefix() {
    let mut value: Value = serde_json::from_str(tempo_v2_json()).unwrap();
    value["trace"]["resourceSpans"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "key": "k8s.pod.name",
            "value": {"stringValue": "n".repeat(MAX_RESOURCE_IDENTITY_BYTES + 1)}
        }));
    let error = parse(&serde_json::to_vec(&value).unwrap()).unwrap_err();
    assert_eq!(
        error,
        TraceError::ResourceIdentityTooLarge {
            field: "k8s.pod.name",
            bytes: MAX_RESOURCE_IDENTITY_BYTES + 1
        }
    );
    assert!(error.to_string().contains("k8s.pod.name"));
}
