//! Ownership, proxy methods, source binding and correlation on the wire.
use crate::*;
use k10s_core::Role;
use k10s_data::integrations::{grafana, loki, otel, prom, traces};
use k10s_data::reach::{Bound, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;
use k10s_data::{describe::DescribeRequest, discover::KindTarget, telemetry};

const METADATA: &str = "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1";
fn targets() -> Vec<KindTarget> {
    use k8s_openapi::api::{apps::v1::Deployment, core::v1::Pod};
    use kube::api::ApiResource;
    [
        (KindId::POD, ApiResource::erase::<Pod>(&()), Role::Instance),
        (
            KindId::DEPLOYMENT,
            ApiResource::erase::<Deployment>(&()),
            Role::Owner,
        ),
    ]
    .into_iter()
    .map(|(id, resource, role)| KindTarget {
        id,
        resource,
        role,
        namespaced: true,
        listable: true,
        watchable: true,
        patchable: true,
        status_subresource: true,
    })
    .collect()
}
fn request(kind: KindId) -> DescribeRequest {
    DescribeRequest {
        kind,
        namespace: Some("prod".into()),
        name: "api".into(),
        uid: "selected-uid".into(),
    }
}
fn meta(name: &str, uid: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({"metadata":{"name":name,"namespace":"prod","uid":uid,"ownerReferences":[{"apiVersion":"apps/v1","kind":"ReplicaSet","name":"same-label","uid":owner,"controller":true}]}})
}
fn deployment(script: &Script) {
    script.route("GET", "/apis/apps/v1/namespaces/prod/deployments/api", 200,
        r#"{"metadata":{"name":"api","namespace":"prod","uid":"selected-uid"},"spec":{"selector":{"matchLabels":{"app":"api"}}}}"#);
}
fn proxy(kind: ToolKind, service: &str, port: u16) -> Bound {
    Bound {
        kind,
        found: None,
        auth: ToolAuth::Anonymous,
        transport: Transport::Proxy {
            namespace: "observe".into(),
            service: service.into(),
            port,
        },
    }
}

#[test]
fn a_workload_without_replicas_has_an_empty_scope_from_null_metadata_items() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    deployment(&script);
    for path in [
        "/apis/apps/v1/namespaces/prod/replicasets?",
        "/api/v1/namespaces/prod/pods?",
    ] {
        script.route_accepting("GET", path, METADATA, 200,
            r#"{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList","metadata":{"resourceVersion":"12"},"items":null}"#);
    }
    let result = runtime.block_on(telemetry::resolve_scope(
        &script.client(),
        &targets(),
        &request(KindId::DEPLOYMENT),
    ));
    let Fetched::Ok(scope) = result else {
        panic!("a served empty scope: {result:?}");
    };
    assert!(scope.pods.is_empty());
    assert_eq!(scope.resource_uid, "selected-uid");
    let seen = script.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(
        seen[0].path,
        "/apis/apps/v1/namespaces/prod/deployments/api"
    );
    assert_eq!(seen[0].accept, "application/json");
    for row in &seen {
        assert_eq!(row.method, "GET");
        assert_eq!(row.content_type, "");
        assert!(row.body.is_empty());
    }
    for row in &seen[1..] {
        assert_eq!(row.accept, METADATA);
        assert!(row.path.contains("labelSelector=app%3Dapi"));
        assert!(row.path.contains("limit=65"));
    }
}

#[test]
fn deployment_scope_follows_controller_uids_and_refuses_nearby_names() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    deployment(&script);
    script.route_accepting("GET", "/apis/apps/v1/namespaces/prod/replicasets?", METADATA, 200,
        serde_json::json!({"items":[meta("api-rs", "owned-rs", "selected-uid"), meta("api-extra-rs", "other-rs", "another-deployment")]}).to_string());
    script.route_accepting("GET", "/api/v1/namespaces/prod/pods?", METADATA, 200,
        serde_json::json!({"items":[meta("api-1", "owned-pod", "owned-rs"), meta("api-11", "near-pod", "other-rs"), meta("api-2", "foreign-pod", "another-owner")]}).to_string());
    let result = runtime.block_on(telemetry::resolve_scope(
        &script.client(),
        &targets(),
        &request(KindId::DEPLOYMENT),
    ));
    let Fetched::Ok(scope) = result else {
        panic!("{result:?}")
    };
    assert_eq!(scope.pods.len(), 1);
    assert_eq!(scope.pods[0].name, "api-1");
    assert_eq!(scope.pods[0].uid, "owned-pod");
    let seen = script.seen();
    assert_eq!(seen.len(), 3);
    for row in &seen {
        assert_eq!(row.method, "GET");
        assert_eq!(row.content_type, "");
        assert!(row.body.is_empty());
    }
    assert_eq!(
        seen[0].path,
        "/apis/apps/v1/namespaces/prod/deployments/api"
    );
    assert_eq!(seen[0].accept, "application/json");
    for row in &seen[1..] {
        assert_eq!(row.accept, METADATA);
        assert!(row.path.contains("labelSelector=app%3Dapi"), "{}", row.path);
        assert!(row.path.contains("limit=65"));
    }
}

#[test]
fn recreation_denial_and_partial_ownership_do_not_produce_scope() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api",
        200,
        r#"{"metadata":{"namespace":"prod","uid":"replacement"},"spec":{"nodeName":"n"}}"#,
    );
    let result = runtime.block_on(telemetry::resolve_scope(
        &script.client(),
        &targets(),
        &request(KindId::POD),
    ));
    assert!(matches!(result, Fetched::Failed { why, .. } if why.contains("recreated")));
    assert_eq!(script.seen().len(), 1);
    let script = Script::default();
    deployment(&script);
    script.route(
        "GET",
        "/apis/apps/v1/namespaces/prod/replicasets?",
        403,
        r#"{"message":"replicasets are forbidden"}"#,
    );
    assert!(matches!(
        runtime.block_on(telemetry::resolve_scope(
            &script.client(),
            &targets(),
            &request(KindId::DEPLOYMENT)
        )),
        Fetched::Denied {
            what: "replicasets"
        }
    ));
    assert!(script.requests_for("/pods?").is_empty());
    let script = Script::default();
    deployment(&script);
    script.route(
        "GET",
        "/apis/apps/v1/namespaces/prod/replicasets?",
        200,
        r#"{"metadata":{"continue":"next"},"items":[]}"#,
    );
    assert!(
        matches!(runtime.block_on(telemetry::resolve_scope(&script.client(), &targets(), &request(KindId::DEPLOYMENT))), Fetched::Failed { why, .. } if why.contains("partial ownership"))
    );
    assert!(script.requests_for("/pods?").is_empty());
}

#[test]
fn a_mismatched_pod_name_cannot_inherit_the_selected_uid() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api",
        200,
        r#"{"metadata":{"name":"other","namespace":"prod","uid":"selected-uid"}}"#,
    );
    let result = runtime.block_on(telemetry::resolve_scope(
        &script.client(),
        &targets(),
        &request(KindId::POD),
    ));
    assert!(matches!(result, Fetched::Failed { why, .. } if why.contains("identity")));
    let seen = script.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, "/api/v1/namespaces/prod/pods/api");
    assert_eq!(seen[0].accept, "application/json");
    assert_eq!(seen[0].content_type, "");
    assert!(seen[0].body.is_empty());
}

#[test]
fn grafana_uid_and_ref_id_select_the_actual_prometheus_proxy() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route("GET", "/api/v1/namespaces/observe/services/grafana:3000/proxy/api/datasources/uid/prom-main", 200,
        r#"{"uid":"prom-main","type":"prometheus","url":"http://elsewhere:9090","jsonData":{"timeInterval":"15s"}}"#);
    script.route("GET", "/api/v1/namespaces/observe/services/grafana:3000/proxy/api/datasources/proxy/uid/prom-main/api/v1/query_range?", 200,
        r#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{"pod":"api-1"},"values":[[1,"2"],[16,"NaN"],[31,"4"]]}]},"warnings":["partial storage"],"infos":["annotation"]}"#);
    let target = grafana::PanelQuery {
        ref_id: "B".into(),
        expr: r#"rate(requests_total{namespace="$namespace",pod=~"$pod"}[$__rate_interval])"#
            .into(),
        dialect: grafana::QueryDialect::PromQL,
        datasource: Some("prom-main".into()),
        min_step: "30s".into(),
        unsupported: None,
    };
    let vars = grafana::Variables {
        namespace: "prod".into(),
        pods: vec!["api-1".into()],
        node: None,
        start_ms: 1000,
        end_ms: 31000,
        step_ms: 15000,
        min_interval: String::new(),
    };
    let result = runtime.block_on(grafana::query_target(
        &script.client(),
        &proxy(ToolKind::Grafana, "grafana", 3000),
        &target,
        &vars,
    ));
    let Fetched::Ok(result) = result else {
        panic!("{result:?}")
    };
    assert_eq!(result.series[0].points, [(1000, 2.0), (31000, 4.0)]);
    assert_eq!(result.series[0].gaps, [16000]);
    assert_eq!(result.warnings, ["partial storage"]);
    assert_eq!(result.infos, ["annotation"]);
    assert_eq!(result.step_ms, 30000);
    assert!(result.source.contains("prom-main") && result.source.contains("B"));
    let seen = script.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].accept, "application/json");
    assert!(seen[0].body.is_empty());
    assert_eq!(seen[1].method, "GET");
    assert_eq!(seen[1].accept, "application/json");
    assert_eq!(seen[1].content_type, "");
    assert!(seen[1].body.is_empty());
    assert!(
        seen[1].path.contains("start=1")
            && seen[1].path.contains("end=31")
            && seen[1].path.contains("step=30s")
            && seen[1].path.contains("limit=256")
    );
    assert!(seen[1].path.contains("120s") && seen[1].path.contains("api-1"));
    assert!(!seen[1].path.contains("$"));
}

#[test]
fn wrong_datasource_never_gets_a_query() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observe/services/grafana:3000/proxy/api/datasources/uid/mixed",
        200,
        r#"{"uid":"mixed","type":"loki"}"#,
    );
    let target = grafana::PanelQuery {
        ref_id: "A".into(),
        expr: "up".into(),
        dialect: grafana::QueryDialect::PromQL,
        datasource: Some("mixed".into()),
        min_step: String::new(),
        unsupported: None,
    };
    let vars = grafana::Variables {
        namespace: "prod".into(),
        pods: vec![],
        node: None,
        start_ms: 1000,
        end_ms: 31000,
        step_ms: 15000,
        min_interval: String::new(),
    };
    assert!(matches!(
        runtime.block_on(grafana::query_target(
            &script.client(),
            &proxy(ToolKind::Grafana, "grafana", 3000),
            &target,
            &vars
        )),
        Fetched::Failed { .. }
    ));
    assert_eq!(script.seen().len(), 1);
}

#[test]
fn loki_uses_get_and_retains_only_correlation_metadata() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route("GET", "/api/v1/namespaces/observe/services/loki:3100/proxy/loki/api/v1/query_range?", 200,
        r#"{"status":"success","data":{"resultType":"streams","result":[{"stream":{"k8s_namespace_name":"prod","k8s_pod_name":"api-1"},"values":[["1700000000000000000","done",{"trace_id":"0123456789abcdef0123456789abcdef","k8s_pod_uid":"pod-uid","password":"unused"}]]}]}}"#);
    let query = loki::RangeQuery {
        query: r#"{k8s_namespace_name="prod",k8s_pod_name="api-1"}"#.into(),
        start_ns: 1700000000000000000,
        end_ns: 1700000001000000000,
        limit: 1,
    };
    let result = runtime.block_on(loki::query_range(
        &script.client(),
        &proxy(ToolKind::Loki, "loki", 3100),
        &query,
    ));
    let Fetched::Ok(result) = result else {
        panic!("{result:?}")
    };
    assert!(result.limit_reached);
    assert_eq!(result.streams[0].lines[0].ts_ns, query.start_ns);
    assert!(
        result.streams[0].lines[0]
            .metadata
            .iter()
            .any(|(key, value)| key == "k8s_pod_uid" && value == "pod-uid")
    );
    assert!(!format!("{result:?}").contains("password"));
    let seen = script.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].accept, "application/json");
    assert_eq!(seen[0].content_type, "");
    assert!(seen[0].body.is_empty());
    assert!(
        seen[0].path.contains("start=1700000000000000000")
            && seen[0].path.contains("end=1700000001000000000")
            && seen[0].path.contains("limit=1")
    );
}

#[test]
fn tempo_requests_json_and_refuses_a_different_trace_id() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route("GET", "/api/v1/namespaces/observe/services/tempo:3200/proxy/api/v2/traces/0123456789abcdef0123456789abcdef", 200,
        r#"{"batches":[{"scopeSpans":[{"spans":[{"traceId":"ffffffffffffffffffffffffffffffff","spanId":"0000000000000001","name":"other","startTimeUnixNano":"1000","endTimeUnixNano":"2000"}]}]}]}"#);
    let result = runtime.block_on(traces::lookup(
        &script.client(),
        &proxy(ToolKind::Tempo, "tempo", 3200),
        "0123456789abcdef0123456789abcdef",
    ));
    assert!(matches!(result, Fetched::Failed { why, .. } if why.contains("requested")));
    let seen = script.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].accept, "application/json");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].content_type, "");
    assert!(seen[0].body.is_empty());
}

#[test]
fn oversized_proxy_bodies_and_native_histograms_are_labelled_failures() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route_bytes(
        "GET",
        "/api/v1/namespaces/observe/services/prometheus:9090/proxy/api/v1/query?",
        200,
        vec![b' '; k10s_data::reach::MAX_BODY_BYTES + 1],
    );
    assert!(
        matches!(runtime.block_on(prom::query(&script.client(), &proxy(ToolKind::Prometheus,"prometheus",9090),"up",None)), Fetched::Failed { why, .. } if why.contains("bytes"))
    );
    assert!(prom::parse_response(br#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{},"histograms":[[1,{}]]}]}}"#).unwrap_err().contains("histograms"));
    assert!(
        prom::parse_response(
            br#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{}}]}}"#
        )
        .is_err()
    );
}

#[test]
fn standalone_collector_discovery_needs_no_operator_config() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route("GET", "/api/v1/services?", 200,
        r#"{"apiVersion":"v1","kind":"ServiceList","items":[{"metadata":{"name":"otel-collector","namespace":"observe"},"spec":{"ports":[{"name":"health-check","port":13133}]}}]}"#);
    script.route(
        "GET",
        "/api/v1/namespaces/observe/services/otel-collector:13133/proxy/",
        200,
        "healthy",
    );
    let result = runtime.block_on(otel::fetch_inventory(&script.client(), None));
    let Fetched::Ok(result) = result else {
        panic!("{result:?}")
    };
    assert!(result.served());
    assert_eq!(result.services.len(), 1);
    assert!(result.services[0].health.contains("Process extension"));
    assert!(script.requests_for("opentelemetrycollectors").is_empty());
    let requests = script.requests_for("/proxy/");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].accept, "application/json");
    assert!(requests[0].body.is_empty());
}

#[test]
fn observe_discovery_failure_keeps_its_sentence_and_denial_keeps_its_resource() {
    for (code, expected) in [
        (500, "discovery store unavailable"),
        (403, "services: access denied"),
    ] {
        let runtime = runtime();
        let script = Script::default();
        script_discovery(&script);
        script_rules_review(&script);
        script_access_reviews(&script, true, 32);
        script_lists(&script);
        let (sync, _live) = sync_on(&runtime, &script);
        let before = script.seen().len();
        script.route("GET", "/api/v1/services?", code,
            format!(r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"message":"discovery store unavailable"}}"#));
        let (tx, rx) = std::sync::mpsc::channel();
        let _stop = sync.reader.observe_probe(move |reply| {
            tx.send(reply).expect("probe reply");
        });
        let Fetched::Ok(tools) = wait(&rx) else {
            panic!("an admitted probe must answer");
        };
        assert_eq!(tools.prometheus, k10s_data::read::Seen::Unbound);
        assert!(
            tools.errors.iter().any(|why| why.contains(expected)),
            "{tools:?}"
        );
        let requests: Vec<_> = after_sync(&script, before)
            .into_iter()
            .filter(|row| {
                row.path.starts_with("/api/v1/services?") && !row.path.contains("watch=true")
            })
            .collect();
        assert_eq!(requests.len(), 1, "discovery is shared by all three tools");
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].path.contains("limit=200"));
        assert_eq!(requests[0].accept, "");
        assert!(requests[0].body.is_empty());
    }
}

#[test]
fn collector_inventory_truncates_only_the_requested_namespace() {
    let runtime = runtime();
    let _entered = runtime.enter();
    let script = Script::default();
    script.route("GET", "/apis/opentelemetry.io", 404,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"no operator"}"#);
    let items: Vec<_> = (0..18).map(|index| serde_json::json!({
        "metadata":{"name":format!("otel-collector-{index}"),"namespace":if index == 0 { "observe" } else { "other" },"labels":{"app.kubernetes.io/name":"opentelemetry-collector"}},
        "spec":{"ports":[{"name":"health-check","port":13133}]}
    })).collect();
    script.route(
        "GET",
        "/api/v1/services?",
        200,
        serde_json::json!({"apiVersion":"v1","kind":"ServiceList","items":items}).to_string(),
    );
    let path = "/api/v1/namespaces/observe/services/otel-collector-0:13133/proxy/";
    script.route("GET", path, 200, "healthy");
    let Fetched::Ok(inventory) =
        runtime.block_on(otel::fetch_inventory(&script.client(), Some("observe")))
    else {
        panic!("the namespaced standalone inventory must resolve");
    };
    assert_eq!(inventory.services.len(), 1);
    assert!(!inventory.services_truncated);
    let requests = script.requests_for("/proxy/");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, path);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].accept, "application/json");
    assert!(requests[0].body.is_empty());
}
