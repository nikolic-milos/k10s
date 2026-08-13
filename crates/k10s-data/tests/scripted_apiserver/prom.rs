//! PromQL over the API-server service proxy: instant GET and query_range POST
//! land on the Prometheus HTTP paths, and a named token never rides that proxy.

use crate::*;

use k10s_data::prom::{query, query_range};
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
fn a_range_query_posts_form_urlencoded_query_start_end_step() {
    let script = Script::default();
    script.route(
        "POST",
        "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range",
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
    assert_eq!(hits.len(), 1, "one POST: {hits:?}");
    assert_eq!(hits[0].method, "POST");
    assert_eq!(
        hits[0].content_type, "application/x-www-form-urlencoded",
        "query_range is a form, not JSON"
    );
    assert!(
        hits[0].path.ends_with("/proxy/api/v1/query_range")
            || hits[0].path.contains("/proxy/api/v1/query_range?"),
        "the path is Prometheus query_range: {}",
        hits[0].path
    );
    assert!(
        hits[0].body.contains("query=%7Bjob%3D%22api%22%7D"),
        "the expression is the form field Prometheus names query: {}",
        hits[0].body
    );
    assert!(
        hits[0].body.contains("start=1700000000"),
        "start is unix seconds: {}",
        hits[0].body
    );
    assert!(
        hits[0].body.contains("end=1700000900"),
        "end is unix seconds: {}",
        hits[0].body
    );
    assert!(
        hits[0].body.contains("step=15s"),
        "step rides the form: {}",
        hits[0].body
    );

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
