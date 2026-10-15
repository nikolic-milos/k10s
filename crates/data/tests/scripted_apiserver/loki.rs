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
    assert_eq!(seen[0].loki_encoding_flags, "categorize-labels");
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

#[test]
fn instant_queries_request_categorized_labels_after_one_exact_get() {
    use k10s_data::integrations::loki::{InstantQuery, query};
    const INSTANT: &str = "/api/v1/namespaces/observe/services/loki:3100/proxy/loki/api/v1/query?query=%7Bapp%3D%22api%22%7D&limit=10&direction=BACKWARD&time=2";
    let script = Script::default();
    script.route(
        "GET",
        INSTANT,
        200,
        r#"{"status":"success","data":{"resultType":"streams","result":[]}}"#,
    );
    let answer = runtime().block_on(async {
        query(
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
            &InstantQuery {
                query: r#"{app="api"}"#.into(),
                time_ns: Some(2),
                limit: 10,
            },
        )
        .await
    });
    assert!(matches!(answer, Fetched::Ok(_)), "{answer:?}");
    let seen = script.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, INSTANT);
    assert_eq!(seen[0].accept, "application/json");
    assert_eq!(seen[0].loki_encoding_flags, "categorize-labels");
    assert_eq!(seen[0].content_type, "");
    assert_eq!(seen[0].body, "");
}

#[test]
fn a_flat_label_named_structured_metadata_keeps_entry_identity_after_the_exact_get() {
    let (script, answer) = reply(
        r#"[{"stream":{"k8s_pod_uid":"retired"},"values":[["1","line",{
            "structuredMetadata":"ordinary-label",
            "trace_id":"0123456789abcdef0123456789abcdef",
            "k8s_pod_uid":"current",
            "parsed":{"trace_id":"ffffffffffffffffffffffffffffffff","span_id":"ffffffffffffffff","k8s_pod_uid":"parsed"}
        }]]}]"#,
    );
    let Fetched::Ok(logs) = answer else {
        panic!("a flat string label must not become a malformed category: {answer:?}");
    };
    assert_eq!(logs.streams.len(), 1);
    assert_eq!(logs.streams[0].lines.len(), 1);
    let metadata = &logs.streams[0].lines[0].metadata;
    assert_eq!(metadata.len(), 2);
    assert!(
        metadata
            .iter()
            .any(|(key, value)| key == "k8s_pod_uid" && value == "current")
    );
    assert!(
        metadata.iter().any(|(key, value)| {
            key == "trace_id" && value == "0123456789abcdef0123456789abcdef"
        })
    );
    assert!(!logs.truncated);
    assert_get(&script);
}

#[test]
fn malformed_nested_entry_identity_refuses_the_whole_reply() {
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
            let is_string = value.is_string();
            let result =
                serde_json::json!([{"stream":{"k8s_pod_uid":"fallback"},"values":[["1","line",{
                    "structuredMetadata": {(key):value}, "parsed": {(key):"parsed fallback"}
                }]]}])
                .to_string();
            let (script, answer) = reply(&result);
            let why = if is_string {
                format!("Loki entry metadata {key} exceeds 256 bytes; its identity is refused")
            } else {
                format!("Loki entry metadata {key} has a non-string identity value")
            };
            assert_eq!(answer, Fetched::Failed { what: "loki", why });
            assert_get(&script);
        }
    }
    for malformed in [serde_json::json!(null), serde_json::json!([])] {
        let result = serde_json::json!([{"stream":{"k8s_pod_uid":"fallback"},"values":[["1","line",{"structuredMetadata":malformed}]]}]).to_string();
        let (script, answer) = reply(&result);
        assert_eq!(
            answer,
            Fetched::Failed {
                what: "loki",
                why: "Loki entry structuredMetadata is not an object".into()
            }
        );
        assert_get(&script);
    }
}

#[test]
fn direct_range_and_instant_urls_send_the_exact_categorized_request() {
    use k10s_data::integrations::loki::{InstantQuery, query};
    use std::io::{Read as _, Write as _};
    for range in [true, false] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut connection = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the direct request did not arrive"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            connection
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut bytes = [0; 1024];
                let read = connection.read(&mut bytes).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&bytes[..read]);
                assert!(request.len() <= 16 * 1024);
            }
            let body = r#"{"status":"success","data":{"resultType":"streams","result":[]}}"#;
            write!(
                connection,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        let script = Script::default();
        let bound = Bound {
            kind: ToolKind::Loki,
            found: None,
            transport: Transport::Url {
                base: format!("http://{address}/named"),
            },
            auth: ToolAuth::Anonymous,
        };
        let answer = runtime().block_on(async {
            if range {
                query_range(
                    &script.client(),
                    &bound,
                    &RangeQuery {
                        query: r#"{app="api"}"#.into(),
                        start_ns: 1,
                        end_ns: 2,
                        limit: 10,
                    },
                )
                .await
            } else {
                query(
                    &script.client(),
                    &bound,
                    &InstantQuery {
                        query: r#"{app="api"}"#.into(),
                        time_ns: Some(2),
                        limit: 10,
                    },
                )
                .await
            }
        });
        assert!(matches!(answer, Fetched::Ok(_)), "{answer:?}");
        assert!(script.seen().is_empty());
        let request = server.join().unwrap();
        let path = if range {
            "query_range?query=%7Bapp%3D%22api%22%7D&start=1&end=2&limit=10&direction=BACKWARD"
        } else {
            "query?query=%7Bapp%3D%22api%22%7D&limit=10&direction=BACKWARD&time=2"
        };
        let expected = format!(
            "GET /named/loki/api/v1/{path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\nConnection: close\r\nX-Loki-Response-Encoding-Flags: categorize-labels\r\n\r\n"
        );
        assert_eq!(request, expected);
    }
}

#[test]
fn readiness_and_non_loki_reads_do_not_request_query_encoding() {
    for (kind, rest) in [
        (ToolKind::Loki, "ready"),
        (ToolKind::Prometheus, "loki/api/v1/query?query=up"),
    ] {
        let script = Script::default();
        let path = format!("/api/v1/namespaces/observe/services/loki:3100/proxy/{rest}");
        script.route("GET", &path, 200, "{}");
        let answer = runtime().block_on(async {
            k10s_data::reach::tool_get(
                &script.client(),
                &Bound {
                    kind,
                    found: None,
                    transport: Transport::Proxy {
                        namespace: "observe".into(),
                        service: "loki".into(),
                        port: 3100,
                    },
                    auth: ToolAuth::Anonymous,
                },
                rest,
            )
            .await
        });
        assert!(matches!(answer, Fetched::Ok(_)));
        let seen = script.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, path);
        assert_eq!(seen[0].accept, "application/json");
        assert_eq!(seen[0].content_type, "");
        assert_eq!(seen[0].body, "");
        assert_eq!(seen[0].loki_encoding_flags, "");
    }
}
