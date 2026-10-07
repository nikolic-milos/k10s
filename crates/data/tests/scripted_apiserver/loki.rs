//! A valid quiet result and a malformed stream remain distinct on the wire.

use crate::*;
use k10s_data::integrations::loki::{Logs, RangeQuery, query_range};
use k10s_data::reach::{Bound, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;

const PATH: &str = "/api/v1/namespaces/observe/services/loki:3100/proxy/loki/api/v1/query_range?query=%7Bapp%3D%22api%22%7D&start=1&end=2&limit=10&direction=BACKWARD";

fn reply(result: &str) -> (Script, Fetched<Logs>) {
    let script = Script::default();
    script.route(
        "GET",
        PATH,
        200,
        format!(r#"{{"status":"success","data":{{"resultType":"streams","result":{result}}}}}"#),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async {
        query_range(
            &script.client(),
            &Bound {
                kind: ToolKind::Loki,
                found: None,
                transport: Transport::Proxy {
                    namespace: "observe".into(),
                    service: "loki".into(),
                    port: 3100,
                },
                auth: ToolAuth::Anonymous,
            },
            &RangeQuery {
                query: r#"{app="api"}"#.into(),
                start_ns: 1,
                end_ns: 2,
                limit: 10,
            },
        )
        .await
    });
    (script, fetched)
}

fn assert_get(script: &Script) {
    let seen = script.seen();
    assert_eq!(seen.len(), 1, "a parse outcome must not issue another read");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, PATH);
    assert_eq!(seen[0].accept, "application/json");
    assert_eq!(seen[0].content_type, "");
    assert_eq!(seen[0].body, "");
}

#[test]
fn malformed_stream_members_are_failures_after_one_exact_get() {
    for (result, why) in [
        (
            r#"[{}]"#,
            "Loki stream 0 has no object-valued stream member",
        ),
        (
            r#"[{"values":[]}]"#,
            "Loki stream 0 has no object-valued stream member",
        ),
        (
            r#"[{"stream":null,"values":[]}]"#,
            "Loki stream 0 has no object-valued stream member",
        ),
        (
            r#"[{"stream":[],"values":[]}]"#,
            "Loki stream 0 has no object-valued stream member",
        ),
        (
            r#"[{"stream":"api","values":[]}]"#,
            "Loki stream 0 has no object-valued stream member",
        ),
        (
            r#"[{"stream":{"namespace":10},"values":[]}]"#,
            "Loki stream 0 has a non-string label value",
        ),
        (
            r#"[{"stream":{}}]"#,
            "Loki stream 0 has no array-valued values member",
        ),
        (
            r#"[{"stream":{},"values":null}]"#,
            "Loki stream 0 has no array-valued values member",
        ),
        (
            r#"[{"stream":{},"values":{}}]"#,
            "Loki stream 0 has no array-valued values member",
        ),
        (
            r#"[{"stream":{},"values":"line"}]"#,
            "Loki stream 0 has no array-valued values member",
        ),
        (
            r#"[{"stream":{},"values":false}]"#,
            "Loki stream 0 has no array-valued values member",
        ),
    ] {
        let (script, fetched) = reply(result);
        assert!(
            matches!(fetched, Fetched::Failed { what: "loki", why: ref reason } if reason == why),
            "{fetched:?}"
        );
        assert_get(&script);
    }
}

#[test]
fn valid_empty_arrays_are_successful_after_one_exact_get() {
    for result in ["[]", r#"[{"stream":{},"values":[]}]"#] {
        let (script, fetched) = reply(result);
        let Fetched::Ok(logs) = fetched else {
            panic!("valid empty arrays must remain quiet data: {fetched:?}");
        };
        assert!(logs.streams.iter().all(|stream| stream.lines.is_empty()));
        assert!(!logs.truncated);
        assert!(!logs.limit_reached);
        assert_eq!(logs.dropped_streams, 0);
        assert_eq!(logs.dropped_lines, 0);
        assert_get(&script);
    }
}

#[test]
fn invalid_entry_identity_is_refused_after_one_exact_get_without_label_fallback() {
    for key in [
        "trace_id",
        "span_id",
        "k8s_namespace_name",
        "k8s_pod_name",
        "k8s_pod_uid",
        "k8s_node_name",
    ] {
        for value in [
            serde_json::json!(null),
            serde_json::json!(10),
            serde_json::json!("x".repeat(257)),
        ] {
            let string = value.is_string();
            let metadata =
                serde_json::Value::Object(serde_json::Map::from_iter([(key.into(), value)]));
            let result = serde_json::json!([{
                "stream": {
                    "k8s_namespace_name": "prod", "k8s_pod_name": "api", "k8s_pod_uid": "current",
                    "trace_id": "0123456789abcdef0123456789abcdef",
                },
                "values": [["1", "healthy earlier line"], ["2", "invalid identity line", metadata]],
            }])
            .to_string();
            let (script, fetched) = reply(&result);
            let why = if string {
                format!("Loki entry metadata {key} exceeds 256 bytes; its identity is refused")
            } else {
                format!("Loki entry metadata {key} has a non-string identity value")
            };
            assert_eq!(fetched, Fetched::Failed { what: "loki", why });
            assert_get(&script);
        }
    }
}

#[test]
fn valid_bounded_and_empty_entry_identity_survive_the_exact_get() {
    let boundary = format!("{}x", "界".repeat(85));
    assert_eq!(boundary.len(), 256);
    for metadata in [
        serde_json::json!({}),
        serde_json::json!({"k8s_pod_uid": ""}),
        serde_json::json!({"k8s_pod_uid": "retired"}),
        serde_json::json!({"k8s_pod_name": boundary}),
    ] {
        let result = serde_json::json!([{
            "stream": {"k8s_namespace_name": "prod", "k8s_pod_name": "api", "k8s_pod_uid": "current"},
            "values": [["1", "actual line", metadata]],
        }]).to_string();
        let (script, fetched) = reply(&result);
        let Fetched::Ok(logs) = fetched else {
            panic!("{fetched:?}");
        };
        assert_eq!(
            logs.streams[0].lines[0].metadata,
            metadata
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_string()))
                .collect::<Vec<_>>()
        );
        assert!(!logs.truncated);
        assert_get(&script);
    }
}
