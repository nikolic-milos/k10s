//! Live log following, bounded end to end.
//!
//! A follow is a task on the data plane's runtime reading the pod's log
//! stream line by line and handing each line to a callback the caller
//! supplied; the render thread never blocks on the cluster. The feed is
//! bounded by construction -- a line is capped in bytes here and retention is
//! the consumer's ring -- and every terminal state arrives as a labelled
//! chunk (`Ended`, `Denied`, `Failed`), never as silence. Dropping the
//! returned [`LogStop`] cancels the task at the next await point, including
//! while the connection is still being opened.
//!
//! A workload follow merges the follows of the pods its label selector
//! matches -- at most [`MAX_MERGED_PODS`] of them -- into the same callback,
//! each line carrying the pod's name after the kubelet timestamp. One guard
//! cancels every underlying follow; a pod's own ending becomes a marked line
//! in the feed, and the merged feed ends only when the last pod's does.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::AsyncBufReadExt;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, GetParams, ListParams, LogParams, Request};

use crate::discover::KindTarget;
use crate::read::{Fetched, classify, collection_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    pub previous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogChunk {
    Lines(Vec<String>),
    Ended { why: &'static str },
    Denied { what: &'static str },
    Failed { what: &'static str, why: String },
}

pub struct LogStop {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

impl LogStop {
    pub(crate) fn noop() -> LogStop {
        LogStop { cancel: None }
    }
}

impl Drop for LogStop {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

const FOLLOW_TAIL_LINES: i64 = 500;
const MAX_LINE_BYTES: usize = 8 * 1024;

// A workload fanning out wider than this follows its first sixteen pods and
// says so in the feed; every merged follow costs one open connection per pod.
pub const MAX_MERGED_PODS: usize = 16;

pub(crate) fn follow(
    handle: &tokio::runtime::Handle,
    client: Client,
    request: LogRequest,
    on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
) -> LogStop {
    let (cancel_tx, mut cancel) = tokio::sync::oneshot::channel::<()>();
    handle.spawn(async move {
        let api: Api<Pod> = Api::namespaced(client, &request.namespace);
        let params = LogParams {
            follow: true,
            tail_lines: Some(FOLLOW_TAIL_LINES),
            timestamps: true,
            previous: request.previous,
            container: request.container.clone(),
            ..LogParams::default()
        };
        let opened = tokio::select! {
            _ = &mut cancel => {
                on_chunk(LogChunk::Ended { why: "stopped" });
                return;
            }
            opened = api.log_stream(&request.pod, &params) => opened,
        };
        let mut reader = match opened {
            Ok(stream) => Box::pin(stream),
            Err(error) => {
                on_chunk(match classify::<()>("logs", &error) {
                    Fetched::Denied { what } => LogChunk::Denied { what },
                    Fetched::Failed { what, why } => LogChunk::Failed { what, why },
                    Fetched::Ok(()) => unreachable!("classify never returns Ok"),
                });
                return;
            }
        };
        let mut line = String::new();
        loop {
            line.clear();
            let read = tokio::select! {
                _ = &mut cancel => {
                    on_chunk(LogChunk::Ended { why: "stopped" });
                    return;
                }
                read = reader.read_line(&mut line) => read,
            };
            match read {
                Ok(0) => {
                    on_chunk(LogChunk::Ended {
                        why: "the stream ended",
                    });
                    return;
                }
                Ok(_) => {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    on_chunk(LogChunk::Lines(vec![capped(&line)]));
                }
                Err(error) => {
                    on_chunk(LogChunk::Failed {
                        what: "logs",
                        why: error.to_string(),
                    });
                    return;
                }
            }
        }
    });
    LogStop {
        cancel: Some(cancel_tx),
    }
}

fn capped(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_string();
    }
    let mut cut = MAX_LINE_BYTES;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\u{2026}", &line[..cut])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadLogRequest {
    pub namespace: String,
    pub kind: k10s_core::KindId,
    pub name: String,
}

pub(crate) fn follow_workload(
    handle: &tokio::runtime::Handle,
    client: Client,
    target: KindTarget,
    request: WorkloadLogRequest,
    on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
) -> LogStop {
    let (cancel_tx, mut cancel) = tokio::sync::oneshot::channel::<()>();
    handle.spawn(async move {
        let on_chunk: Arc<dyn Fn(LogChunk) + Send + Sync> = Arc::from(on_chunk);

        let workload = tokio::select! {
            _ = &mut cancel => {
                on_chunk(LogChunk::Ended { why: "stopped" });
                return;
            }
            fetched = fetch_workload(&client, &target, &request) => fetched,
        };
        let selector = match workload {
            Ok(object) => match selector_string(&object) {
                Ok(selector) => selector,
                Err(why) => {
                    on_chunk(LogChunk::Failed {
                        what: "workload logs",
                        why: why.to_string(),
                    });
                    return;
                }
            },
            Err(error) => {
                on_chunk(forward_terminal(classify::<()>("workload", &error)));
                return;
            }
        };

        let listed = tokio::select! {
            _ = &mut cancel => {
                on_chunk(LogChunk::Ended { why: "stopped" });
                return;
            }
            listed = matching_pods(&client, &request.namespace, &selector) => listed,
        };
        let (pods, more_exist) = match listed {
            Ok(pods) => clamp_pods(pods),
            Err(error) => {
                on_chunk(forward_terminal(classify::<()>("pods", &error)));
                return;
            }
        };
        if pods.is_empty() {
            on_chunk(LogChunk::Ended {
                why: "no pods match this workload's selector",
            });
            return;
        }
        if more_exist {
            on_chunk(LogChunk::Lines(vec![format!(
                "<following the first {MAX_MERGED_PODS} pods; more match and are not followed>"
            )]));
        }

        let live = Arc::new(AtomicUsize::new(pods.len()));
        let handle = tokio::runtime::Handle::current();
        let mut stops: Vec<LogStop> = Vec::with_capacity(pods.len());
        for pod in pods {
            let on_chunk = on_chunk.clone();
            let live = live.clone();
            let child_request = LogRequest {
                namespace: request.namespace.clone(),
                pod: pod.clone(),
                container: None,
                previous: false,
            };
            stops.push(follow(
                &handle,
                client.clone(),
                child_request,
                Box::new(move |chunk| match chunk {
                    LogChunk::Lines(lines) => {
                        on_chunk(LogChunk::Lines(
                            lines.iter().map(|line| merged_line(&pod, line)).collect(),
                        ));
                    }
                    terminal => {
                        // One pod ending is a fact in the feed, not the end
                        // of the feed; the merge ends when the last one does.
                        on_chunk(LogChunk::Lines(vec![format!(
                            "{pod} <{}>",
                            terminal_text(&terminal)
                        )]));
                        if live.fetch_sub(1, Ordering::AcqRel) == 1 {
                            on_chunk(LogChunk::Ended {
                                why: "every pod follow ended",
                            });
                        }
                    }
                }),
            ));
        }
        // Hold the child guards until the merged guard drops: releasing them
        // cancels every follow, opening included.
        let _ = cancel.await;
        drop(stops);
    });
    LogStop {
        cancel: Some(cancel_tx),
    }
}

async fn fetch_workload(
    client: &Client,
    target: &KindTarget,
    request: &WorkloadLogRequest,
) -> Result<serde_json::Value, kube::Error> {
    let http_request = Request::new(collection_path(target, Some(&request.namespace)))
        .get(&request.name, &GetParams::default())
        .map_err(kube::Error::BuildRequest)?;
    client.request::<serde_json::Value>(http_request).await
}

async fn matching_pods(
    client: &Client,
    namespace: &str,
    selector: &str,
) -> Result<Vec<String>, kube::Error> {
    let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let params = ListParams::default()
        .labels(selector)
        .limit(MAX_MERGED_PODS as u32 + 1);
    let list = api.list(&params).await?;
    Ok(list
        .items
        .into_iter()
        .filter_map(|pod| pod.metadata.name)
        .collect())
}

fn forward_terminal(fetched: Fetched<()>) -> LogChunk {
    match fetched {
        Fetched::Denied { what } => LogChunk::Denied { what },
        Fetched::Failed { what, why } => LogChunk::Failed { what, why },
        Fetched::Ok(()) => unreachable!("classify never returns Ok"),
    }
}

fn terminal_text(chunk: &LogChunk) -> String {
    match chunk {
        LogChunk::Ended { why } => format!("log follow ended: {why}"),
        LogChunk::Denied { what } => format!("{what}: access denied for this account"),
        LogChunk::Failed { what, why } => format!("{what} failed: {why}"),
        LogChunk::Lines(_) => unreachable!("lines are not terminal"),
    }
}

// The pod's `spec.selector` as a label-selector expression the list endpoint
// accepts. Deterministic: keys sorted within each half.
pub(crate) fn selector_string(workload: &serde_json::Value) -> Result<String, &'static str> {
    let Some(selector) = workload.get("spec").and_then(|s| s.get("selector")) else {
        return Err("this workload declares no pod selector, so its pods cannot be found");
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(labels) = selector.get("matchLabels").and_then(|m| m.as_object()) {
        let mut keys: Vec<&String> = labels.keys().collect();
        keys.sort();
        for key in keys {
            let value = labels[key].as_str().unwrap_or_default();
            parts.push(format!("{key}={value}"));
        }
    }
    for expression in selector
        .get("matchExpressions")
        .and_then(|e| e.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let key = expression.get("key").and_then(|k| k.as_str()).unwrap_or("");
        let values: Vec<&str> = expression
            .get("values")
            .and_then(|v| v.as_array())
            .map(|v| v.iter().filter_map(|s| s.as_str()).collect())
            .unwrap_or_default();
        match expression
            .get("operator")
            .and_then(|o| o.as_str())
            .unwrap_or("")
        {
            "In" => parts.push(format!("{key} in ({})", values.join(","))),
            "NotIn" => parts.push(format!("{key} notin ({})", values.join(","))),
            "Exists" => parts.push(key.to_string()),
            "DoesNotExist" => parts.push(format!("!{key}")),
            _ => return Err("this workload's selector uses an operator k10s does not translate"),
        }
    }
    if parts.is_empty() {
        return Err("this workload's pod selector is empty, so its pods cannot be found");
    }
    Ok(parts.join(","))
}

fn clamp_pods(mut pods: Vec<String>) -> (Vec<String>, bool) {
    let more = pods.len() > MAX_MERGED_PODS;
    pods.truncate(MAX_MERGED_PODS);
    (pods, more)
}

// The pod's name joins the line after the kubelet timestamp, so the shell's
// timestamp handling keeps working and a stripped line still says which pod
// spoke: "2026-...Z api-1 ready".
fn merged_line(pod: &str, line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() > 11
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[10] == b'T'
        && let Some(space) = line.find(' ')
    {
        return format!("{} {pod} {}", &line[..space], &line[space + 1..]);
    }
    format!("{pod} {line}")
}

pub(crate) async fn fetch_containers(
    client: &Client,
    namespace: &str,
    pod: &str,
) -> Fetched<Vec<String>> {
    let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    match api.get(pod).await {
        Ok(pod) => {
            let spec = pod.spec.unwrap_or_default();
            let mut names: Vec<String> = spec.containers.into_iter().map(|c| c.name).collect();
            names.extend(
                spec.init_containers
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| c.name),
            );
            names.extend(
                spec.ephemeral_containers
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| c.name),
            );
            Fetched::Ok(names)
        }
        Err(error) => classify("containers", &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_capped_at_a_char_boundary_not_mid_codepoint() {
        assert_eq!(capped("short"), "short");
        let long = format!("{}é", "x".repeat(MAX_LINE_BYTES - 1));
        let out = capped(&long);
        assert!(out.ends_with('\u{2026}'));
        assert!(out.len() <= MAX_LINE_BYTES + '\u{2026}'.len_utf8());
        assert!(!out.contains('é'), "the split codepoint is dropped whole");
    }

    #[test]
    fn a_selector_translates_match_labels_and_expressions_deterministically() {
        let selector = |json: serde_json::Value| {
            selector_string(&serde_json::json!({"spec": {"selector": json}}))
        };
        assert_eq!(
            selector(serde_json::json!({"matchLabels": {"tier": "web", "app": "api"}})),
            Ok("app=api,tier=web".to_string()),
            "keys sort, whatever order serde kept"
        );
        assert_eq!(
            selector(serde_json::json!({
                "matchLabels": {"app": "api"},
                "matchExpressions": [
                    {"key": "env", "operator": "In", "values": ["prod", "canary"]},
                    {"key": "batch", "operator": "DoesNotExist"},
                    {"key": "owned", "operator": "Exists"},
                    {"key": "tier", "operator": "NotIn", "values": ["debug"]}
                ]
            })),
            Ok("app=api,env in (prod,canary),!batch,owned,tier notin (debug)".to_string())
        );
        assert!(
            selector(serde_json::json!({
                "matchExpressions": [{"key": "x", "operator": "Gt", "values": ["1"]}]
            }))
            .is_err(),
            "an untranslatable operator refuses rather than guesses"
        );
        assert!(
            selector(serde_json::json!({})).is_err(),
            "empty is labelled"
        );
        assert!(
            selector_string(&serde_json::json!({"spec": {}})).is_err(),
            "absent is labelled"
        );
    }

    #[test]
    fn the_merged_pod_set_is_clamped_and_the_overflow_is_reported() {
        let names = |n: usize| (0..n).map(|i| format!("pod-{i}")).collect::<Vec<_>>();
        let (kept, more) = clamp_pods(names(3));
        assert_eq!(kept.len(), 3);
        assert!(!more);
        let (kept, more) = clamp_pods(names(MAX_MERGED_PODS + 1));
        assert_eq!(kept.len(), MAX_MERGED_PODS);
        assert!(more, "the overflow becomes a line in the feed");
    }

    #[test]
    fn a_merged_line_carries_the_pod_name_after_the_kubelet_timestamp() {
        assert_eq!(
            merged_line("api-1", "2026-08-02T05:00:01Z ready"),
            "2026-08-02T05:00:01Z api-1 ready"
        );
        assert_eq!(
            merged_line("api-1", "no timestamp here"),
            "api-1 no timestamp here",
            "a line without the timestamp still names its pod"
        );
        assert_eq!(merged_line("api-1", ""), "api-1 ");
    }
}
