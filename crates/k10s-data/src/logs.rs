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

use futures::AsyncBufReadExt;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, LogParams};

use crate::read::{Fetched, classify};

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

impl Drop for LogStop {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

const FOLLOW_TAIL_LINES: i64 = 500;
const MAX_LINE_BYTES: usize = 8 * 1024;

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
}
