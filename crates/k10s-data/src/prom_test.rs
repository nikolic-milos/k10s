use super::*;

fn vector_json(series: &str) -> String {
    format!(r#"{{"status":"success","data":{{"resultType":"vector","result":[{series}]}}}}"#)
}

fn matrix_json(series: &str) -> String {
    format!(r#"{{"status":"success","data":{{"resultType":"matrix","result":[{series}]}}}}"#)
}

fn bound(kind: ToolKind, auth: ToolAuth, transport: Transport) -> Bound {
    Bound {
        kind,
        found: None,
        transport,
        auth,
    }
}

fn proxy() -> Transport {
    Transport::Proxy {
        namespace: "monitoring".into(),
        service: "prometheus".into(),
        port: 9090,
    }
}

#[test]
fn a_vector_instant_becomes_labels_and_millisecond_points() {
    let json = vector_json(
        r#"{"metric":{"job":"prometheus","__name__":"up","instance":"a:9090"},
            "value":[1700000000,"1"]}"#,
    );
    let result = parse_response(json.as_bytes()).expect("vector JSON");
    assert_eq!(result.result_type, ResultType::Vector);
    assert_eq!(result.series.len(), 1);
    assert!(
        !result.truncated && result.dropped_series == 0,
        "one honest series is kept whole"
    );
    assert_eq!(
        result.series[0].labels,
        vec![
            ("__name__".into(), "up".into()),
            ("instance".into(), "a:9090".into()),
            ("job".into(), "prometheus".into()),
        ],
        "labels are sorted so a chart can key on them without guessing order"
    );
    assert_eq!(result.series[0].points, vec![(1_700_000_000_000, 1.0)]);
    let samples: Vec<Sample> = result.series[0].samples().collect();
    assert_eq!(
        samples,
        vec![Sample {
            t_ms: 1_700_000_000_000,
            value: 1.0
        }]
    );
}

#[test]
fn a_matrix_keeps_each_finite_point() {
    let json = matrix_json(
        r#"{"metric":{"__name__":"up"},
            "values":[[1700000000,"1"],[1700000015,"2.5"]]}"#,
    );
    let result = parse_response(json.as_bytes()).expect("matrix JSON");
    assert_eq!(result.result_type, ResultType::Matrix);
    assert_eq!(
        result.series[0].points,
        vec![(1_700_000_000_000, 1.0), (1_700_000_015_000, 2.5)]
    );
}

#[test]
fn nan_and_inf_points_are_dropped() {
    let json = matrix_json(
        r#"{"metric":{"__name__":"x"},
            "values":[[1,"1"],[2,"NaN"],[3,"+Inf"],[4,"-Inf"],[5,"2"]]}"#,
    );
    let result = parse_response(json.as_bytes()).expect("matrix JSON");
    assert_eq!(
        result.series[0].points,
        vec![(1000, 1.0), (5000, 2.0)],
        "NaN and Inf are not numbers a chart can stamp"
    );
}

#[test]
fn a_scalar_is_one_unlabelled_series() {
    let json = r#"{"status":"success","data":{"resultType":"scalar","result":[1700000000,"3"]}}"#;
    let result = parse_response(json.as_bytes()).expect("scalar JSON");
    assert_eq!(result.result_type, ResultType::Scalar);
    assert!(result.series[0].labels.is_empty());
    assert_eq!(result.series[0].points, vec![(1_700_000_000_000, 3.0)]);
}

#[test]
fn an_error_status_is_a_failure_with_prometheus_text() {
    let json = r#"{"status":"error","errorType":"bad_data","error":"invalid parameter"}"#;
    match parse_response(json.as_bytes()) {
        Err(why) => {
            assert!(why.contains("bad_data"), "{why}");
            assert!(why.contains("invalid parameter"), "{why}");
        }
        other => panic!("an error status is not a series: {other:?}"),
    }
}

#[test]
fn a_native_histogram_result_is_refused() {
    let json = r#"{"status":"success","data":{"resultType":"histogram","result":[]}}"#;
    match parse_response(json.as_bytes()) {
        Err(why) => assert!(why.contains("native histograms"), "{why}"),
        other => panic!("histograms are out of scope: {other:?}"),
    }
}

#[test]
fn a_string_result_is_not_a_series() {
    let json = r#"{"status":"success","data":{"resultType":"string","result":[1,"up"]}}"#;
    match parse_response(json.as_bytes()) {
        Err(why) => assert!(why.contains("string result"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_oversize_body_is_refused_before_parsing() {
    let huge = vec![b'x'; MAX_BODY_BYTES + 1];
    match parse_response(&huge) {
        Err(why) => assert!(why.contains("bytes"), "{why}"),
        other => panic!("a bomb is hidden, not parsed: {other:?}"),
    }
}

#[test]
fn an_expression_past_eight_kib_is_not_sent() {
    let too_long = "a".repeat(MAX_EXPR_BYTES + 1);
    let why = refuse_expr(&too_long).expect("refused");
    assert!(why.contains(&format!("{MAX_EXPR_BYTES}")), "{why}");
    assert!(refuse_expr("up").is_none());
    assert!(refuse_expr("   ").unwrap().contains("empty"));
}

#[test]
fn series_past_256_are_counted_not_kept() {
    let items: Vec<String> = (0..MAX_SERIES + 3)
        .map(|i| format!(r#"{{"metric":{{"i":"{i}"}},"value":[1,"1"]}}"#))
        .collect();
    let json = vector_json(&items.join(","));
    let result = parse_response(json.as_bytes()).expect("vector JSON");
    assert_eq!(result.series.len(), MAX_SERIES);
    assert!(result.truncated);
    assert_eq!(
        result.dropped_series, 3,
        "the overflow is counted, not dropped silently"
    );
}

#[test]
fn a_series_past_5000_points_is_refused_not_truncated() {
    let values: Vec<String> = (0..MAX_POINTS_PER_SERIES)
        .map(|i| format!(r#"[{i},"1"]"#))
        .collect();
    let at_cap = matrix_json(&format!(
        r#"{{"metric":{{"__name__":"ok"}},"values":[{}]}}"#,
        values.join(",")
    ));
    let kept = parse_response(at_cap.as_bytes()).expect("5000 is the cap, not past it");
    assert_eq!(kept.series.len(), 1);
    assert_eq!(kept.series[0].points.len(), MAX_POINTS_PER_SERIES);
    assert!(!kept.truncated);

    let values: Vec<String> = (0..MAX_POINTS_PER_SERIES + 1)
        .map(|i| format!(r#"[{i},"1"]"#))
        .collect();
    let over = matrix_json(&format!(
        r#"{{"metric":{{"__name__":"dense"}},"values":[{}]}}"#,
        values.join(",")
    ));
    let result = parse_response(over.as_bytes()).expect("the envelope still parses");
    assert!(
        result.series.is_empty(),
        "a prefix of a range is not the range: {:?}",
        result.series
    );
    assert!(result.truncated);
    assert_eq!(result.dropped_series, 1);
}

#[test]
fn a_named_token_never_uses_the_service_proxy() {
    let token = bound(
        ToolKind::Prometheus,
        ToolAuth::NamedToken("prom-token".into()),
        proxy(),
    );
    let Fetched::Failed { why, .. } = refuse_bind(&token).expect("refused") else {
        panic!("a named token on the proxy must fail closed");
    };
    assert!(why.contains("proxy"), "{why}");
    assert!(why.contains("Authorization"), "{why}");

    let anonymous = bound(ToolKind::Prometheus, ToolAuth::Anonymous, proxy());
    assert!(
        refuse_bind(&anonymous).is_none(),
        "anonymous proxy is the path reach prefers"
    );

    let forwarded = bound(
        ToolKind::Prometheus,
        ToolAuth::NamedToken("prom-token".into()),
        Transport::NeedsForward {
            namespace: "monitoring".into(),
            name: "prometheus".into(),
            port: 9090,
        },
    );
    assert!(
        refuse_bind(&forwarded).is_none(),
        "a token on a forward is reach's bind; this module does not rewrite it"
    );
}

#[test]
fn mimir_and_thanos_speak_the_same_http_api() {
    for kind in [ToolKind::Prometheus, ToolKind::Mimir, ToolKind::Thanos] {
        let ok = bound(kind, ToolAuth::Anonymous, proxy());
        assert!(
            refuse_bind(&ok).is_none(),
            "{} shares /api/v1/query",
            kind.as_str()
        );
    }
    let grafana = bound(ToolKind::Grafana, ToolAuth::Anonymous, proxy());
    let Fetched::Failed { why, .. } = refuse_bind(&grafana).expect("refused") else {
        panic!("Grafana is not PromQL");
    };
    assert!(why.contains("Prometheus HTTP API"), "{why}");
}

#[test]
fn promql_selectors_are_form_encoded() {
    assert_eq!(encoded_value("up"), "up");
    assert_eq!(
        encoded_value(r#"{job="api"}"#),
        "%7Bjob%3D%22api%22%7D",
        "braces and quotes must not land raw in a URI"
    );
    assert_eq!(
        encoded_value("sum by (job) (up)"),
        "sum+by+%28job%29+%28up%29",
        "spaces become plus; parentheses are percent-encoded"
    );
}
