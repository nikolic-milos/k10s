//! PromQL over the API-server service proxy: instant and range GET
//! land on the Prometheus HTTP paths, and a named token never rides that proxy.

use crate::*;

use k10s_data::integrations::prom::{query, query_range};
use k10s_data::reach::{Bound, FoundService, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;

const INSTANT_JSON: &str = r#"{"status":"success","data":{"resultType":"vector","result":[
    {"metric":{"__name__":"up","job":"prometheus"},"value":[1700000000,"1"]}
]}}"#;

const RANGE_JSON: &str = r#"{"status":"success","data":{"resultType":"matrix","result":[
    {"metric":{"__name__":"up","job":"prometheus"},
     "values":[[1700000000,"1"],[1700000015,"1"]]}
]}}"#;

fn prometheus_bound(auth: ToolAuth, transport: Transport) -> Bound {
    Bound {
        kind: ToolKind::Prometheus,
        found: Some(FoundService {
            kind: ToolKind::Prometheus,
            namespace: "monitoring".into(),
            name: "prometheus".into(),
            port: 9090,
            port_name: None,
        }),
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
fn an_instant_query_is_a_get_through_the_service_proxy() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query?",
        200,
        INSTANT_JSON,
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        query(
            &script.client(),
            &prometheus_bound(ToolAuth::Anonymous, proxy()),
            "up",
            None,
        )
        .await
    });
    let Fetched::Ok(result) = outcome else {
        panic!("the instant query must resolve: {outcome:?}");
    };
    assert_eq!(result.series.len(), 1);
    assert_eq!(result.series[0].points, vec![(1_700_000_000_000, 1.0)]);

    let hits = script.requests_for("/proxy/api/v1/query");
    assert_eq!(hits.len(), 1, "one GET, nothing else: {hits:?}");
    assert_eq!(hits[0].method, "GET");
    assert!(
        hits[0].path.starts_with(
            "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query?"
        ),
        "the ask is the service proxy, not a node proxy: {}",
        hits[0].path
    );
    assert!(
        hits[0].path.contains("query=up"),
        "the PromQL is a query parameter: {}",
        hits[0].path
    );
    assert!(
        !hits[0].path.contains("query_range"),
        "instant is /query, not /query_range: {}",
        hits[0].path
    );

    drop(runtime);
}

#[test]
fn a_range_query_gets_urlencoded_query_start_end_step() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?",
        200,
        RANGE_JSON,
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        query_range(
            &script.client(),
            &prometheus_bound(ToolAuth::Anonymous, proxy()),
            r#"{job="api"}"#,
            1_700_000_000.0,
            1_700_000_900.0,
            "15s",
        )
        .await
    });
    let Fetched::Ok(result) = outcome else {
        panic!("the range query must resolve: {outcome:?}");
    };
    assert_eq!(result.series[0].points.len(), 2);

    let hits = script.requests_for("/proxy/api/v1/query_range");
    assert_eq!(hits.len(), 1, "one GET: {hits:?}");
    assert_eq!(hits[0].method, "GET");
    assert_eq!(hits[0].content_type, "");
    assert_eq!(hits[0].accept, "application/json");
    assert!(hits[0].body.is_empty());
    assert!(
        hits[0].path.ends_with("/proxy/api/v1/query_range")
            || hits[0].path.contains("/proxy/api/v1/query_range?"),
        "the path is Prometheus query_range: {}",
        hits[0].path
    );
    assert!(
        hits[0].path.contains("query=%7Bjob%3D%22api%22%7D"),
        "the expression is the encoded query parameter: {}",
        hits[0].path
    );
    assert!(
        hits[0].path.contains("start=1700000000"),
        "start is unix seconds: {}",
        hits[0].path
    );
    assert!(
        hits[0].path.contains("end=1700000900"),
        "end is unix seconds: {}",
        hits[0].path
    );
    assert!(
        hits[0].path.contains("step=15s"),
        "step is an encoded query parameter: {}",
        hits[0].path
    );

    assert!(hits[0].path.contains("limit=256"));
    drop(runtime);
}

#[test]
fn a_named_token_never_rides_the_service_proxy() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query?",
        200,
        INSTANT_JSON,
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        query(
            &script.client(),
            &prometheus_bound(ToolAuth::NamedToken("prom-token".into()), proxy()),
            "up",
            None,
        )
        .await
    });
    let Fetched::Failed { why, .. } = outcome else {
        panic!("a token on the proxy must not be sent: {outcome:?}");
    };
    assert!(why.contains("proxy"), "{why}");
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "refusing means the request is not issued: {:?}",
        script.seen()
    );

    drop(runtime);
}

fn limited_range_body(count: usize, dense_first: bool, result_type: &str) -> String {
    let result = if result_type == "scalar" {
        serde_json::json!([1, "2"])
    } else {
        serde_json::Value::Array((0..count).map(|index| {
            let mut row = serde_json::json!({"metric":{"namespace":"prod","pod":format!("pod-{index}")}});
            if result_type == "vector" {
                row["value"] = serde_json::json!([1, "2"]);
            } else {
                let points = if dense_first && index == 0 { 5001 } else { 1 };
                row["values"] = serde_json::Value::Array((0..points).map(|n| serde_json::json!([n, "2"])).collect());
            }
            row
        }).collect())
    };
    serde_json::json!({"status":"success","data":{"resultType":result_type,"result":result}})
        .to_string()
}

fn assert_limited_query_request(script: &Script, path: &str) {
    let requests = script.seen();
    assert_eq!(
        requests.len(),
        1,
        "the limit notice cannot issue another query"
    );
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, path);
    assert_eq!(request.accept, "application/json");
    assert_eq!(request.content_type, "");
    assert!(request.body_collected);
    assert!(request.body.is_empty());
}

#[test]
fn range_limit_is_a_request_bound_possibility_independent_of_local_omission_counts() {
    let path = "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?query=up&start=0&end=900&step=30s&limit=256";
    for (count, dense, result_type) in [
        (255, false, "matrix"),
        (256, false, "matrix"),
        (257, false, "matrix"),
        (256, true, "matrix"),
        (256, false, "vector"),
        (1, false, "scalar"),
    ] {
        let script = Script::default();
        script.route(
            "GET",
            path,
            200,
            limited_range_body(count, dense, result_type),
        );
        let answer = runtime().block_on(async {
            query_range(
                &script.client(),
                &prometheus_bound(ToolAuth::Anonymous, proxy()),
                "up",
                0.0,
                900.0,
                "30s",
            )
            .await
        });
        let Fetched::Ok(result) = answer else {
            panic!("the bounded numeric response must parse")
        };
        let dropped = count.saturating_sub(256) + usize::from(dense);
        assert_eq!(result.dropped_series, dropped);
        assert_eq!(result.truncated, dropped != 0);
        assert_eq!(result.series.len(), count.min(256) - usize::from(dense));
        let expected = (result_type != "scalar" && count >= 256).then_some(256);
        assert_eq!(result.possible_server_limit, expected);
        assert_limited_query_request(&script, path);
    }
}

#[test]
fn generic_parsing_and_instant_queries_do_not_invent_a_server_request_limit() {
    for count in [256, 257] {
        let body = limited_range_body(count, false, "vector");
        let parsed = k10s_data::integrations::prom::parse_response(body.as_bytes()).unwrap();
        assert_eq!(parsed.possible_server_limit, None);
        let script = Script::default();
        let path = "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query?query=up&time=0";
        script.route("GET", path, 200, body);
        let answer = runtime().block_on(async {
            query(
                &script.client(),
                &prometheus_bound(ToolAuth::Anonymous, proxy()),
                "up",
                Some(0.0),
            )
            .await
        });
        let Fetched::Ok(result) = answer else {
            panic!("instant numeric response")
        };
        assert_eq!(result.possible_server_limit, None);
        assert_limited_query_request(&script, path);
    }
}

#[test]
fn annotation_overflow_preserves_data_or_server_failure_without_an_extra_request() {
    use k10s_data::integrations::prom::MAX_ANNOTATIONS_PER_LEVEL;

    let path = "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range?query=up&start=0&end=900&step=30s&limit=256";
    for error in [false, true] {
        let mut body: serde_json::Value = serde_json::from_str(RANGE_JSON).unwrap();
        body["warnings"] =
            serde_json::json!(vec!["complete warning"; MAX_ANNOTATIONS_PER_LEVEL + 2]);
        body["infos"] = serde_json::json!(vec!["complete info"; MAX_ANNOTATIONS_PER_LEVEL + 3]);
        if error {
            body["status"] = "error".into();
            body["errorType"] = "execution".into();
            body["error"] = "the server could not evaluate the requested range".into();
        }
        let script = Script::default();
        script.route("GET", path, 200, body.to_string());
        let answer = runtime().block_on(async {
            query_range(
                &script.client(),
                &prometheus_bound(ToolAuth::Anonymous, proxy()),
                "up",
                0.0,
                900.0,
                "30s",
            )
            .await
        });
        if error {
            assert!(
                matches!(answer, Fetched::Failed { what: "prometheus", ref why }
                if why == "execution: the server could not evaluate the requested range")
            );
        } else {
            let Fetched::Ok(result) = answer else {
                panic!("successful data and annotation counts")
            };
            assert_eq!(result.series.len(), 1);
            assert_eq!(result.warnings.len(), MAX_ANNOTATIONS_PER_LEVEL);
            assert_eq!(result.infos.len(), MAX_ANNOTATIONS_PER_LEVEL);
            assert_eq!(result.warnings_omitted, 2);
            assert_eq!(result.infos_omitted, 3);
        }
        assert_limited_query_request(&script, path);
    }
}
