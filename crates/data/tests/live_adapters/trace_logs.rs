//! Actual Collector spans select retained trace logs by recorded identity and time.

use super::*;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use k10s_data::integrations::{loki, traces};
use kube::api::{ListParams, LogParams};

const NS: &str = "k10s-observe-pilot";

fn producer_receipt() -> (String, String, serde_json::Value) {
    kube_runtime().block_on(async {
        let client = kube::Client::try_default().await.expect("the live client");
        let namespaces: kube::Api<Namespace> = kube::Api::all(client.clone());
        let namespace = namespaces
            .get_metadata(NS)
            .await
            .expect("the owned namespace");
        assert_eq!(
            namespace
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("k10s.dev/fixture"))
                .map(String::as_str),
            Some("observe-pilot")
        );
        let pods: kube::Api<Pod> = kube::Api::namespaced(client, NS);
        let page = pods
            .list_metadata(
                &ListParams::default()
                    .labels("k10s.dev/fixture=observe-pilot,app.kubernetes.io/name=pilot-producer")
                    .limit(2),
            )
            .await
            .expect("the owned producer identity");
        assert!(page.metadata.continue_.as_deref().is_none_or(str::is_empty));
        assert_eq!(page.items.len(), 1);
        let item = &page.items[0];
        let name = item.metadata.name.clone().expect("producer name");
        let uid = item.metadata.uid.clone().expect("producer UID");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let logs = pods
                .logs(
                    &name,
                    &LogParams {
                        container: Some("producer".into()),
                        tail_lines: Some(20),
                        limit_bytes: Some(64 * 1024),
                        ..Default::default()
                    },
                )
                .await
                .expect("the producer emission receipt");
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("epoch")
                .as_nanos();
            if let Some(marker) = logs
                .lines()
                .rev()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|marker| {
                    marker["event"] == "pilot.complete"
                        && marker["namespace"] == NS
                        && marker["pod"] == name
                        && marker["uid"] == uid
                        && marker["timestamp_ns"].as_u64().is_some_and(|time| {
                            now_ns.saturating_sub(time as u128) >= 2_000_000_000
                        })
                })
            {
                return (name, uid, marker);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the current producer never recorded a completed emission older than two seconds"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

fn span_logs(span: &traces::Span, trace_id: &str) -> loki::RangeQuery {
    let start_ms = span.start_us.saturating_sub(1_000_000) / 1_000;
    let end_ms = span
        .start_us
        .checked_add(span.duration_us)
        .and_then(|end| end.checked_add(1_000_000))
        .expect("the recorded span's padded end")
        .div_ceil(1_000);
    assert!(end_ms - start_ms <= 86_400_000);
    loki::RangeQuery {
        query: format!(
            "{{k8s_namespace_name=\"{}\",k8s_pod_name=\"{}\"}} | k8s_pod_uid=\"{}\" | trace_id=\"{trace_id}\"",
            span.namespace, span.pod, span.pod_uid
        ),
        start_ns: start_ms
            .checked_mul(1_000_000)
            .expect("the span start nanoseconds"),
        end_ns: end_ms
            .checked_mul(1_000_000)
            .expect("the span end nanoseconds"),
        limit: 200,
    }
}

fn logs(reader: &Reader, query: loki::RangeQuery) -> loki::Logs {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = reader.observe_loki(query, move |answer| {
        let _ = tx.send(answer);
    });
    match wait(&rx) {
        Fetched::Ok(Some(logs)) => logs,
        other => panic!("the recorded span's Loki query: {other:?}"),
    }
}

fn attribute<'a>(
    stream: &'a loki::LogStream,
    line: &'a loki::LogLine,
    key: &str,
) -> Option<&'a str> {
    line.metadata
        .iter()
        .chain(&stream.labels)
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn assert_trace_log(
    logs: &loki::Logs,
    query: &loki::RangeQuery,
    marker: &serde_json::Value,
    root_id: &str,
) {
    assert!(!logs.truncated);
    assert!(!logs.limit_reached);
    assert!(logs.source.contains("loki"));
    let matches: Vec<_> = logs
        .streams
        .iter()
        .flat_map(|stream| stream.lines.iter().map(move |line| (stream, line)))
        .collect();
    assert!(
        !matches.is_empty(),
        "the trace must have its retained producer log"
    );
    for (stream, line) in matches {
        assert!(line.ts_ns >= query.start_ns && line.ts_ns < query.end_ns);
        assert_eq!(
            line.ts_ns,
            marker["timestamp_ns"].as_u64().expect("emission timestamp")
        );
        assert_eq!(attribute(stream, line, "k8s_namespace_name"), Some(NS));
        assert_eq!(
            attribute(stream, line, "k8s_pod_name"),
            marker["pod"].as_str()
        );
        assert_eq!(
            attribute(stream, line, "k8s_pod_uid"),
            marker["uid"].as_str()
        );
        assert_eq!(
            attribute(stream, line, "trace_id"),
            marker["trace_id"].as_str()
        );
        assert_eq!(attribute(stream, line, "span_id"), Some(root_id));
        let body: serde_json::Value =
            serde_json::from_str(&line.line).expect("the actual producer body");
        assert_eq!(&body, marker);
    }
}

#[test]
#[ignore = "needs the owned Collector producer and current Loki/Tempo data; read-only"]
fn root_and_child_spans_find_their_trace_log_with_recorded_uid_and_padded_time() {
    let (pod, uid, marker) = producer_receipt();
    let trace_id = marker["trace_id"].as_str().expect("the emitted trace ID");
    let (_plane, sync) = connect(None);
    let (tx, rx) = std::sync::mpsc::channel();
    let _probe_stop = sync.reader.observe_loki_probe(move |answer| {
        let _ = tx.send(answer);
    });
    assert_eq!(wait(&rx), Fetched::Ok(k10s_data::read::Seen::Bound));
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync
        .reader
        .observe_trace(trace_id.to_string(), move |answer| {
            let _ = tx.send(answer);
        });
    let trace = match wait(&rx) {
        Fetched::Ok(Some(trace)) => trace,
        other => panic!("the actual Collector trace: {other:?}"),
    };
    assert_eq!(trace.trace_id, trace_id);
    assert!(trace.source.starts_with("Tempo / "));
    assert_eq!(trace.spans.len(), 2);
    let root = trace
        .spans
        .iter()
        .find(|span| span.name == "pilot.investigation")
        .expect("the emitted root span");
    let child = trace
        .spans
        .iter()
        .find(|span| span.name == "pilot.hash")
        .expect("the emitted child span");
    assert_eq!(child.parent, root.id);
    assert_eq!(marker["span_id"], root.id);
    for span in [root, child] {
        assert_eq!(span.namespace, NS);
        assert_eq!(span.pod, pod);
        assert_eq!(span.pod_uid, uid);
        assert_eq!(span.service, "k10s-observe-pilot");
        assert!(span.duration_us > 0);
        let query = span_logs(span, trace_id);
        assert_eq!(query.limit, 200);
        assert!(!query.query.contains("span_id"));
        let retained = logs(&sync.reader, query.clone());
        assert_trace_log(&retained, &query, &marker, &root.id);
        println!(
            "live trace-log origin span={} query={} start_ns={} end_ns={} source={}",
            span.id, query.query, query.start_ns, query.end_ns, retained.source
        );
    }

    for foreign in [
        traces::Span {
            pod_uid: format!("retired-{uid}"),
            ..root.clone()
        },
        traces::Span {
            namespace: "k10s-unrelated-trace-namespace".into(),
            ..root.clone()
        },
    ] {
        let query = span_logs(&foreign, trace_id);
        let retained = logs(&sync.reader, query);
        assert!(
            retained
                .streams
                .iter()
                .all(|stream| stream.lines.is_empty())
        );
        assert!(!retained.truncated);
    }
    let query = span_logs(root, trace_id);
    assert_trace_log(
        &logs(&sync.reader, query.clone()),
        &query,
        &marker,
        &root.id,
    );
    kube_runtime().block_on(async {
        let client = kube::Client::try_default()
            .await
            .expect("the final identity client");
        let pods: kube::Api<Pod> = kube::Api::namespaced(client, NS);
        let current = pods
            .get_metadata(&pod)
            .await
            .expect("the producer still exists");
        assert_eq!(current.metadata.uid.as_deref(), Some(uid.as_str()));
    });
    assert_eq!(sync.reader.observe_schedule_counts().rejected, 0);
}
