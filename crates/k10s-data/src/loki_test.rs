//! Parsing Loki's query JSON through its caps, and the form a range or
//! instant query puts on the wire: direction is backward, a limit is always
//! named, and crossing a cap is counted rather than dropped.

use super::*;
use crate::read::Fetched;

fn body(result_type: &str, result: &str) -> String {
    format!(r#"{{"status":"success","data":{{"resultType":"{result_type}","result":{result}}}}}"#)
}

fn streams(result: &str) -> String {
    body("streams", result)
}

fn stream(app: &str, lines: &[(&str, &str)]) -> String {
    let values: Vec<String> = lines
        .iter()
        .map(|(ts, line)| format!(r#"["{ts}","{line}"]"#))
        .collect();
    format!(
        r#"{{"stream":{{"app":"{app}","namespace":"prod"}},"values":[{}]}}"#,
        values.join(",")
    )
}

#[test]
fn a_stream_keeps_its_labels_and_nanosecond_lines() {
    let json = streams(&format!(
        "[{}]",
        stream(
            "api",
            &[
                ("1594382401000000000", "hello"),
                ("1594382402000000000", "world")
            ]
        )
    ));
    let logs = parse(json.as_bytes()).expect("streams parse");
    assert!(!logs.truncated);
    assert_eq!(logs.streams.len(), 1);
    assert_eq!(
        logs.streams[0].labels,
        vec![
            ("app".to_string(), "api".to_string()),
            ("namespace".to_string(), "prod".to_string()),
        ]
    );
    assert_eq!(logs.streams[0].lines[0].ts_ns, 1_594_382_401_000_000_000);
    assert_eq!(logs.streams[0].lines[0].line, "hello");
    assert_eq!(logs.streams[0].lines[1].line, "world");
}

#[test]
fn structured_metadata_on_a_value_is_ignored_and_the_line_is_kept() {
    let json =
        streams(r#"[{"stream":{"app":"api"},"values":[["10","kept",{"detected_level":"info"}]]}]"#);
    let logs = parse(json.as_bytes()).expect("a third element is metadata, not a reason to drop");
    assert_eq!(logs.streams[0].lines[0].line, "kept");
    assert_eq!(logs.dropped_lines, 0);
}

#[test]
fn streams_past_sixteen_are_counted_not_silently_dropped() {
    let items: Vec<String> = (0..=MAX_STREAMS)
        .map(|i| stream(&format!("s{i}"), &[("1", "line")]))
        .collect();
    let logs = parse(streams(&format!("[{}]", items.join(","))).as_bytes()).expect("parses");
    assert!(logs.truncated, "the cap must be visible");
    assert_eq!(logs.streams.len(), MAX_STREAMS);
    assert_eq!(logs.dropped_streams, 1);
    assert_eq!(
        logs.dropped_lines, 1,
        "the seventeenth stream's line is counted with it"
    );
}

#[test]
fn lines_past_two_thousand_are_counted_not_silently_dropped() {
    let values: Vec<String> = (0..=MAX_LINES)
        .map(|i| format!(r#"["{i}","line {i}"]"#))
        .collect();
    let json = streams(&format!(
        r#"[{{"stream":{{"app":"api"}},"values":[{}]}}]"#,
        values.join(",")
    ));
    let logs = parse(json.as_bytes()).expect("parses");
    assert!(logs.truncated);
    assert_eq!(logs.streams.len(), 1);
    assert_eq!(logs.streams[0].lines.len(), MAX_LINES);
    assert_eq!(logs.dropped_lines, 1);
    assert_eq!(logs.dropped_streams, 0, "the stream itself was kept");
}

#[test]
fn exhausting_the_line_cap_drops_later_streams_with_a_count() {
    let values: Vec<String> = (0..MAX_LINES)
        .map(|i| format!(r#"["{i}","line {i}"]"#))
        .collect();
    let first = format!(
        r#"{{"stream":{{"app":"first"}},"values":[{}]}}"#,
        values.join(",")
    );
    let second = stream("second", &[("1", "a"), ("2", "b"), ("3", "c")]);
    let logs = parse(streams(&format!("[{first},{second}]")).as_bytes()).expect("parses");
    assert!(logs.truncated);
    assert_eq!(logs.streams.len(), 1);
    assert_eq!(logs.streams[0].lines.len(), MAX_LINES);
    assert_eq!(logs.dropped_streams, 1);
    assert_eq!(logs.dropped_lines, 3);
}

#[test]
fn a_line_longer_than_eight_kibibytes_is_clipped_and_the_cap_is_stated() {
    let huge = "x".repeat(MAX_LINE_BYTES + 8);
    let json = streams(&format!(
        r#"[{{"stream":{{"app":"api"}},"values":[["1","{huge}"]]}}]"#
    ));
    let logs = parse(json.as_bytes()).expect("parses");
    assert!(logs.truncated);
    assert_eq!(logs.clipped_lines, 1);
    assert_eq!(logs.dropped_lines, 0, "the line is kept, clipped");
    let line = &logs.streams[0].lines[0].line;
    assert!(line.ends_with('\u{2026}'), "clipped looks clipped");
    assert!(
        line.len() <= MAX_LINE_BYTES + '\u{2026}'.len_utf8(),
        "the ellipsis is the only overshoot: {} bytes",
        line.len()
    );
}

#[test]
fn an_oversize_body_is_refused_not_parsed() {
    let huge = vec![b'x'; MAX_BODY_BYTES + 1];
    let err = parse(&huge).expect_err("refused");
    assert!(
        err.contains(&MAX_BODY_BYTES.to_string()),
        "the cap is named: {err}"
    );
}

#[test]
fn a_metric_result_is_refused_rather_than_read_as_empty_logs() {
    let json = body(
        "matrix",
        r#"[{"metric":{"app":"api"},"values":[[1,"0.2"]]}]"#,
    );
    let err = parse(json.as_bytes()).expect_err("metrics are not streams");
    assert!(err.contains("matrix"), "{err}");
    assert!(
        !err.to_ascii_lowercase().contains("empty"),
        "an empty panel would look like no logs: {err}"
    );
}

#[test]
fn an_error_status_is_the_reason_not_an_empty_panel() {
    let json = r#"{"status":"error","errorType":"bad_data","error":"parse error at line 1"}"#;
    let err = parse(json.as_bytes()).expect_err("error status");
    assert_eq!(err, "parse error at line 1");
}

#[test]
fn an_empty_stream_list_is_empty_not_a_failure() {
    let logs = parse(streams("[]").as_bytes()).expect("empty is a result");
    assert!(logs.streams.is_empty());
    assert!(!logs.truncated);
}

#[test]
fn a_malformed_value_is_counted_rather_than_skipped() {
    let json =
        streams(r#"[{"stream":{"app":"api"},"values":[["not-a-ts","x"],["2","ok"],["3"]]}]"#);
    let logs = parse(json.as_bytes()).expect("the honest line carries the parse");
    assert!(logs.truncated);
    assert_eq!(logs.streams[0].lines.len(), 1);
    assert_eq!(logs.streams[0].lines[0].line, "ok");
    assert_eq!(logs.dropped_lines, 2);
}

#[test]
fn query_range_form_asks_for_backward_and_a_limit() {
    let form = range_form(&RangeQuery {
        query: r#"{app="api"}"#.into(),
        start_ns: 100,
        end_ns: 200,
        limit: 50,
    })
    .expect("valid");
    assert_eq!(
        form,
        "query=%7Bapp%3D%22api%22%7D&start=100&end=200&limit=50&direction=BACKWARD"
    );
}

#[test]
fn a_limit_above_the_cap_is_asked_as_the_cap() {
    let form = range_form(&RangeQuery {
        query: "up".into(),
        start_ns: 1,
        end_ns: 2,
        limit: 9_000,
    })
    .expect("valid");
    assert!(form.contains("limit=2000"), "{form}");
    let zero = range_form(&RangeQuery {
        query: "up".into(),
        start_ns: 1,
        end_ns: 2,
        limit: 0,
    })
    .expect("zero means the cap");
    assert!(zero.contains("limit=2000"), "{zero}");
}

#[test]
fn an_empty_query_is_refused_before_it_is_sent() {
    let err = range_form(&RangeQuery {
        query: "  ".into(),
        start_ns: 1,
        end_ns: 2,
        limit: 10,
    })
    .expect_err("empty");
    assert_eq!(err, "the LogQL query is empty");
}

#[test]
fn a_range_that_starts_after_it_ends_is_refused() {
    let err = range_form(&RangeQuery {
        query: "up".into(),
        start_ns: 20,
        end_ns: 10,
        limit: 10,
    })
    .expect_err("inverted");
    assert!(err.contains("starts after"), "{err}");
}

#[test]
fn an_instant_query_omits_time_until_one_is_named() {
    let open = instant_form(&InstantQuery {
        query: "up".into(),
        time_ns: None,
        limit: 10,
    })
    .expect("valid");
    assert_eq!(open, "query=up&limit=10&direction=BACKWARD");
    assert!(!open.contains("time="), "{open}");
    let stamped = instant_form(&InstantQuery {
        query: "up".into(),
        time_ns: Some(99),
        limit: 10,
    })
    .expect("valid");
    assert!(stamped.contains("time=99"), "{stamped}");
}

#[test]
fn a_query_longer_than_eight_kibibytes_is_refused_not_truncated() {
    let err = instant_form(&InstantQuery {
        query: "a".repeat(MAX_QUERY_BYTES + 1),
        time_ns: None,
        limit: 10,
    })
    .expect_err("refused");
    assert!(err.contains(&MAX_QUERY_BYTES.to_string()), "{err}");
}

#[test]
fn a_denied_fetch_stays_denied_rather_than_becoming_an_empty_log_panel() {
    assert!(matches!(
        finish(Fetched::Denied { what: "loki" }),
        Fetched::Denied { what: "loki" }
    ));
    let failed = finish(Fetched::Failed {
        what: "loki",
        why: "Loki did not answer within 4 seconds".into(),
    });
    assert!(
        matches!(failed, Fetched::Failed { what: "loki", .. }),
        "{failed:?}"
    );
}

#[test]
fn finish_parses_a_successful_body() {
    let json = streams(&format!("[{}]", stream("api", &[("1", "hi")])));
    match finish(Fetched::Ok(json.into_bytes())) {
        Fetched::Ok(logs) => assert_eq!(logs.streams[0].lines[0].line, "hi"),
        other => panic!("expected logs, got {other:?}"),
    }
}
