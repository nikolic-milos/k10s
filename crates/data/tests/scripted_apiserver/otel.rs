//! Bounded metadata-only Collector inventory and named process health.

use crate::*;
use k10s_data::integrations::otel::{self, KindSet, health};
use k10s_data::reach::{Bound, FoundService, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;

const METADATA: &str = "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1";
const GROUP_PATH: &str = "/apis/opentelemetry.io";
const LIST_PATH: &str = "/apis/opentelemetry.io/v1beta1/opentelemetrycollectors?";

fn assert_inventory_requests(script: &Script) {
    for request in script.requests_for("/apis/opentelemetry.io") {
        assert_eq!(request.method, "GET");
        assert_eq!(request.content_type, "");
        assert!(request.body.is_empty());
        if request.path == GROUP_PATH {
            assert_eq!(request.accept, "application/json");
        } else {
            assert_eq!(request.accept, METADATA);
            assert!(request.path.contains("opentelemetrycollectors?"));
            assert!(request.path.contains("limit=200"));
        }
    }
    assert!(script.requests_for("secrets").is_empty());
}

fn page(next: &str) -> String {
    serde_json::json!({
        "apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList",
        "metadata":{"continue":next},"items":null
    })
    .to_string()
}

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn otel_group() -> String {
    r#"{"kind":"APIGroup","name":"opentelemetry.io",
        "versions":[{"groupVersion":"opentelemetry.io/v1beta1","version":"v1beta1"}],
        "preferredVersion":{"groupVersion":"opentelemetry.io/v1beta1","version":"v1beta1"}}"#
        .to_string()
}

fn collector_item() -> serde_json::Value {
    serde_json::json!({"apiVersion":"meta.k8s.io/v1", "kind":"PartialObjectMetadata", "metadata":{"name":"simplest","namespace":"observability"}})
}

fn bound_on(port: u16, port_name: Option<&str>) -> Bound {
    Bound {
        kind: ToolKind::OtelCollector,
        found: Some(FoundService {
            kind: ToolKind::OtelCollector,
            namespace: "observability".into(),
            name: "otel-collector".into(),
            port,
            port_name: port_name.map(str::to_string),
        }),
        transport: Transport::Proxy {
            namespace: "observability".into(),
            service: "otel-collector".into(),
            port,
        },
        auth: ToolAuth::Anonymous,
    }
}

#[test]
fn a_404_otel_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { otel::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.collectors, KindSet::NotServed));
    assert!(
        otel::table_page(&inventory).is_none(),
        "not served is no table"
    );
    assert!(
        script.requests_for("opentelemetrycollectors").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert_inventory_requests(&script);
    drop(runtime);
}

#[test]
fn a_403_otel_group_is_denied_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/opentelemetry.io",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { otel::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.collectors, KindSet::Denied));
    assert!(
        inventory.collectors.served(),
        "403 is Denied, not served: false"
    );
    let page = otel::table_page(&inventory).expect("Denied is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("access denied"), "{text}");
    assert_inventory_requests(&script);
    drop(runtime);
}

#[test]
fn a_401_otel_group_is_also_a_labelled_denial() {
    let script = Script::default();
    script.route("GET", GROUP_PATH, 401, status(401, "Unauthorized"));
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert!(matches!(answer, Fetched::Ok(inventory) if inventory.collectors == KindSet::Denied));
    assert_inventory_requests(&script);
    assert_eq!(script.seen().len(), 1);
}

#[test]
fn a_missing_preferred_version_falls_back_with_only_metadata_accepts() {
    let script = Script::default();
    script.route(
        "GET",
        GROUP_PATH,
        200,
        r#"{
        "preferredVersion":{"version":"v1"},
        "versions":[{"version":"v1"},{"version":"v1beta1"}]
    }"#,
    );
    let missing = "/apis/opentelemetry.io/v1/opentelemetrycollectors?";
    script.route("GET", missing, 404, status(404, "NotFound"));
    script.route("GET", LIST_PATH, 200, page(""));
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert!(
        matches!(answer, Fetched::Ok(inventory) if matches!(inventory.collectors, KindSet::Served { truncated: false, .. }))
    );
    let requests = script.requests_for("opentelemetrycollectors");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].path.starts_with(missing));
    assert!(requests[1].path.starts_with(LIST_PATH));
    assert_inventory_requests(&script);
}

#[test]
fn unsupported_metadata_never_falls_back_to_a_full_collector_list() {
    let script = Script::default();
    script.route("GET", GROUP_PATH, 200, otel_group());
    script.route(
        "GET",
        LIST_PATH,
        406,
        r#"{"message":"metadata representation unavailable"}"#,
    );
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert_eq!(
        answer,
        Fetched::Failed {
            what: "otel collectors",
            why: "metadata representation unavailable".into()
        }
    );
    assert_eq!(script.requests_for("opentelemetrycollectors").len(), 1);
    assert_inventory_requests(&script);
}

#[test]
fn a_full_object_response_is_refused_without_another_list() {
    let script = Script::default();
    script.route("GET", GROUP_PATH, 200, otel_group());
    script.route(
        "GET",
        LIST_PATH,
        200,
        r#"{"kind":"OpenTelemetryCollectorList","items":[]}"#,
    );
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert!(
        matches!(answer, Fetched::Failed { why, .. } if why.contains("did not return PartialObjectMetadataList"))
    );
    assert_eq!(script.requests_for("opentelemetrycollectors").len(), 1);
    assert_inventory_requests(&script);
}

#[test]
fn group_and_list_failures_keep_the_servers_sentence() {
    for group in [false, true] {
        let script = Script::default();
        if !group {
            script.route("GET", GROUP_PATH, 200, otel_group());
        }
        script.route(
            "GET",
            if group { GROUP_PATH } else { LIST_PATH },
            500,
            r#"{"message":"Collector store is unavailable"}"#,
        );
        let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
        assert_eq!(
            answer,
            Fetched::Failed {
                what: "otel collectors",
                why: "Collector store is unavailable".into()
            }
        );
        assert_inventory_requests(&script);
    }
}

#[test]
fn a_later_404_never_turns_an_incomplete_metadata_list_into_absence() {
    let script = Script::default();
    script.route("GET", GROUP_PATH, 200, otel_group());
    script.route("GET", LIST_PATH, 200, page("next"));
    script.route(
        "GET",
        LIST_PATH,
        404,
        r#"{"message":"Collector list disappeared while paging"}"#,
    );
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert_eq!(
        answer,
        Fetched::Failed {
            what: "otel collectors",
            why: "Collector list disappeared while paging".into()
        }
    );
    let requests = script.requests_for("opentelemetrycollectors");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].path.contains("continue=next"));
    assert_inventory_requests(&script);
}

#[test]
fn repeated_empty_continuations_fail_instead_of_returning_a_quiet_inventory() {
    let script = Script::default();
    script.route("GET", GROUP_PATH, 200, otel_group());
    for _ in 0..2 {
        script.route("GET", LIST_PATH, 200, page("same"));
    }
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert!(
        matches!(answer, Fetched::Failed { why, .. } if why.contains("repeated a continuation token"))
    );
    let requests = script.requests_for("opentelemetrycollectors");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].path.contains("continue=same"));
    assert_inventory_requests(&script);
}

#[test]
fn empty_metadata_pages_and_version_fallback_share_one_exchange_ceiling() {
    let script = Script::default();
    script.route(
        "GET",
        GROUP_PATH,
        200,
        r#"{
        "preferredVersion":{"version":"v1"},
        "versions":[{"version":"v1"},{"version":"v1alpha1"},{"version":"v1beta1"}]
    }"#,
    );
    for version in ["v1", "v1alpha1"] {
        script.route(
            "GET",
            &format!("/apis/opentelemetry.io/{version}/opentelemetrycollectors?"),
            404,
            status(404, "NotFound"),
        );
    }
    for index in 0..14 {
        script.route("GET", LIST_PATH, 200, page(&format!("page-{index}")));
    }
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert!(matches!(answer, Fetched::Failed { why, .. } if why.contains("16 LIST exchanges")));
    assert_eq!(script.requests_for("opentelemetrycollectors").len(), 16);
    assert_inventory_requests(&script);
}

#[test]
fn group_and_metadata_bodies_are_refused_before_unbounded_decode() {
    for (group, bytes) in [
        (true, (64 << 10) + 1),
        (false, k10s_data::reach::MAX_BODY_BYTES + 1),
    ] {
        let script = Script::default();
        if !group {
            script.route("GET", GROUP_PATH, 200, otel_group());
        }
        script.route_bytes(
            "GET",
            if group { GROUP_PATH } else { LIST_PATH },
            200,
            vec![b' '; bytes],
        );
        let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
        assert!(
            matches!(answer, Fetched::Failed { why, .. } if why.contains("bytes") && why.contains("refused"))
        );
        assert_eq!(script.seen().len(), if group { 1 } else { 2 });
        assert_inventory_requests(&script);
    }
}

#[test]
fn oversized_error_bodies_cannot_masquerade_as_404_absence_or_fallback() {
    for group in [false, true] {
        let script = Script::default();
        if !group {
            script.route("GET", GROUP_PATH, 200, otel_group());
        }
        script.route_bytes(
            "GET",
            if group { GROUP_PATH } else { LIST_PATH },
            404,
            vec![b'x'; (32 << 10) + 1],
        );
        let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
        assert!(matches!(answer, Fetched::Failed { why, .. } if why.contains("32768 bytes")));
        assert_eq!(script.seen().len(), if group { 1 } else { 2 });
        assert_inventory_requests(&script);
    }
}

#[test]
fn metadata_pages_share_the_total_body_budget() {
    let script = Script::default();
    script.route("GET", GROUP_PATH, 200, otel_group());
    let mut first = page("next");
    first.push_str(&" ".repeat((4 << 20) - first.len()));
    script.route("GET", LIST_PATH, 200, first);
    script.route_bytes("GET", LIST_PATH, 200, vec![b' '; (4 << 20) + 1]);
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert!(matches!(answer, Fetched::Failed { why, .. } if why.contains("4194304 bytes")));
    assert_eq!(script.requests_for("opentelemetrycollectors").len(), 2);
    assert_inventory_requests(&script);
}

#[test]
fn unreadable_items_also_consume_the_collector_count_ceiling() {
    let script = Script::default();
    script.route("GET", GROUP_PATH, 200, otel_group());
    script.route(
        "GET",
        LIST_PATH,
        200,
        serde_json::json!({
            "kind":"PartialObjectMetadataList","metadata":{"continue":"unused"},
            "items":vec![serde_json::json!({}); otel::MAX_COLLECTORS + 1]
        })
        .to_string(),
    );
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert!(
        matches!(answer, Fetched::Ok(inventory) if matches!(inventory.collectors, KindSet::Served { truncated: true, unreadable: 2_000, .. }))
    );
    assert_eq!(script.requests_for("opentelemetrycollectors").len(), 1);
    assert_inventory_requests(&script);
}

#[test]
fn invalid_discovery_versions_cannot_change_the_collection_route() {
    let script = Script::default();
    script.route(
        "GET",
        GROUP_PATH,
        200,
        r#"{"preferredVersion":{"version":"v1/../../other"}}"#,
    );
    let answer = runtime().block_on(async { otel::fetch(&script.client(), None).await });
    assert!(
        matches!(answer, Fetched::Failed { why, .. } if why.contains("unsupported API version"))
    );
    assert_eq!(script.seen().len(), 1);
    assert_inventory_requests(&script);
}

#[tokio::test(start_paused = true)]
async fn group_and_metadata_header_stalls_obey_the_exchange_deadline() {
    for group in [false, true] {
        let script = Script::default();
        if !group {
            script.route("GET", GROUP_PATH, 200, otel_group());
        }
        script.route_hanging("GET", if group { GROUP_PATH } else { LIST_PATH });
        let started = tokio::time::Instant::now();
        let answer = otel::fetch(&script.client(), None).await;
        assert!(
            matches!(answer, Fetched::Failed { why, .. } if why == "otel collectors did not answer within 4 seconds")
        );
        assert_eq!(started.elapsed(), Duration::from_secs(4));
        assert_inventory_requests(&script);
    }
}

#[tokio::test(start_paused = true)]
async fn group_and_metadata_body_stalls_obey_the_same_exchange_deadline() {
    use futures::StreamExt as _;
    for group in [false, true] {
        let script = Script::default();
        script.route("GET", GROUP_PATH, 200, otel_group());
        if !group {
            script.route("GET", LIST_PATH, 200, page(""));
        }
        let server = script.clone();
        let client = kube::Client::new(
            tower::service_fn(move |request: http::Request<Body>| {
                let stalled = if group {
                    request.uri().path() == GROUP_PATH
                } else {
                    request.uri().path().ends_with("opentelemetrycollectors")
                };
                let mut server = server.clone();
                async move {
                    let (parts, body) = server.call(request).await?.into_parts();
                    let tail = futures::stream::poll_fn(move |_| {
                        if stalled {
                            Poll::Pending
                        } else {
                            Poll::Ready(None)
                        }
                    });
                    let body = http_body_util::StreamBody::new(
                        http_body_util::BodyStream::new(body).chain(tail),
                    );
                    Ok::<_, tower::BoxError>(http::Response::from_parts(parts, body))
                }
            }),
            "default",
        );
        let started = tokio::time::Instant::now();
        let answer = otel::fetch(&client, None).await;
        assert!(
            matches!(answer, Fetched::Failed { why, .. } if why == "otel collectors did not answer within 4 seconds")
        );
        assert_eq!(started.elapsed(), Duration::from_secs(4));
        assert_inventory_requests(&script);
    }
}

#[test]
fn an_empty_metadata_projection_accepts_the_servers_null_items() {
    let script = Script::default();
    script.route("GET", "/apis/opentelemetry.io", 200, otel_group());
    script.route(
        "GET",
        "/apis/opentelemetry.io/v1beta1/namespaces/observability/opentelemetrycollectors?",
        200,
        r#"{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadataList","metadata":{"resourceVersion":"12"},"items":null}"#,
    );
    let runtime = runtime();
    let fetched =
        runtime.block_on(async { otel::fetch(&script.client(), Some("observability")).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served empty metadata list: {fetched:?}");
    };
    assert!(matches!(
        inventory.collectors,
        KindSet::Served { ref items, truncated: false, unreadable: 0 } if items.is_empty()
    ));
    let requests = script.requests_for("opentelemetrycollectors");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].path.contains("limit=200"));
    assert_eq!(
        requests[0].accept,
        "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1"
    );
    assert_eq!(requests[0].content_type, "");
    assert!(requests[0].body.is_empty());
}

#[test]
fn collectors_are_listed_from_the_crs_and_config_stays_off_the_row() {
    let script = Script::default();
    script.route("GET", "/apis/opentelemetry.io", 200, otel_group());
    script.route(
        "GET",
        "/apis/opentelemetry.io/v1beta1/opentelemetrycollectors?",
        200,
        serde_json::json!({
            "kind": "PartialObjectMetadataList",
            "items": [collector_item()]
        })
        .to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { otel::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    let collector = &inventory.collectors.items()[0];
    assert_eq!(collector.name, "simplest");
    assert_eq!(collector.mode, "Metadata only");
    assert_eq!(collector.ready, "Not fetched");
    assert_eq!(collector.replicas, None);
    let debug = format!("{collector:?}");
    assert!(
        !debug.contains("SECRET_EXPORTER_TOKEN_do_not_print"),
        "{debug}"
    );
    assert!(!debug.contains("exporters"), "{debug}");

    let list = script.requests_for("opentelemetrycollectors");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].method, "GET");
    assert_eq!(
        list[0].accept,
        "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1"
    );
    assert!(list[0].path.contains("limit=200"));
    assert_eq!(list[0].content_type, "");
    assert!(list[0].body.is_empty());
    drop(runtime);
}

#[test]
fn an_undecodable_item_is_a_labelled_table_row_not_a_silent_gap() {
    let script = Script::default();
    script.route("GET", "/apis/opentelemetry.io", 200, otel_group());
    script.route(
        "GET",
        "/apis/opentelemetry.io/v1beta1/opentelemetrycollectors?",
        200,
        serde_json::json!({
            "kind": "PartialObjectMetadataList",
            "items": [collector_item(), { "spec": { "mode": "sidecar" } }]
        })
        .to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { otel::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    assert!(matches!(
        inventory.collectors,
        KindSet::Served { unreadable: 1, .. }
    ));
    let page = otel::table_page(&inventory).expect("a served kind is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("1 collector could not be decoded and is not shown"),
        "a partial listing says what is missing and why: {text}"
    );
    drop(runtime);
}

#[test]
fn health_on_the_extension_port_is_a_get_of_the_well_known_path() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observability/services/otel-collector:13133/proxy/",
        200,
        "",
    );

    let runtime = runtime();
    let outcome = runtime
        .block_on(async { health(&script.client(), &bound_on(13133, Some("health"))).await });
    let Fetched::Ok(answered) = outcome else {
        panic!("health extension must resolve: {outcome:?}");
    };
    assert!(answered.path.is_empty(), "health_check is GET /");
    let hits = script.requests_for("/proxy/");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].method, "GET");
    drop(runtime);
}

#[test]
fn zpages_are_a_get_of_debug_servicez() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observability/services/otel-collector:55679/proxy/debug/servicez",
        200,
        "<html></html>",
    );

    let runtime = runtime();
    let outcome = runtime
        .block_on(async { health(&script.client(), &bound_on(55679, Some("zpages"))).await });
    let Fetched::Ok(answered) = outcome else {
        panic!("zpages must resolve: {outcome:?}");
    };
    assert_eq!(answered.path, "debug/servicez");
    let hits = script.requests_for("/proxy/debug/servicez");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].method, "GET");
    drop(runtime);
}

#[test]
fn a_metrics_bind_fails_closed_and_does_not_get() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observability/services/otel-collector:8888/proxy/",
        200,
        "# HELP up\n",
    );

    let runtime = runtime();
    let outcome = runtime
        .block_on(async { health(&script.client(), &bound_on(8888, Some("metrics"))).await });
    let Fetched::Failed { why, what } = outcome else {
        panic!("metrics is Failed, not a fake healthy: {outcome:?}");
    };
    assert_eq!(what, "otel-collector");
    assert!(why.contains("metrics"), "{why}");
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "a metrics bind must not be probed as health: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_on_health_is_denied() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/observability/services/otel-collector:13133/proxy/",
        403,
        status(403, "Forbidden"),
    );

    let runtime = runtime();
    let outcome =
        runtime.block_on(async { health(&script.client(), &bound_on(13133, None)).await });
    assert_eq!(
        outcome,
        Fetched::Denied {
            what: "otel-collector"
        }
    );
    drop(runtime);
}
