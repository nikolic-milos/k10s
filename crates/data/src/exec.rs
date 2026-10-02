//! Exec into a container, behind a transport seam.
//!
//! The terminal never sees kube: it writes keystrokes and resizes into an
//! [`ExecSession`] and receives raw bytes and labelled terminal states from
//! an [`ExecTransport`]. The split exists because the real transport is a
//! WebSocket upgrade (`AttachedProcess`) the scripted API server cannot
//! speak: everything above the seam -- grid state, input encoding, resize --
//! is tested against a fake transport in `k10s-shell`, while the kube
//! implementation below stays thin plumbing, proven only against a live
//! cluster (its cancel contract excepted, which a hanging transport proves
//! here). Input is bounded by a fixed queue; a session that ends, is denied,
//! fails, or is dropped says so through its event callback, never by going
//! silent.

use futures::SinkExt;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, AttachParams, TerminalSize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::read::{Fetched, classify};

// Keystrokes and resizes waiting for the WebSocket; a full queue drops the
// write being offered rather than growing or reordering what is already
// queued, and a human cannot type past it.
const INPUT_QUEUE: usize = 256;
// One read's worth of remote output handed to the callback at a time.
const OUTPUT_CHUNK: usize = 8 * 1024;
const STOPPED: &str = "stopped";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvent {
    Output(Vec<u8>),
    Ended { why: String },
    Denied { what: &'static str },
    Failed { what: &'static str, why: String },
}

// The live half the terminal holds: keystrokes down, size changes down,
// drop to terminate the remote session.
pub trait ExecSession: Send {
    fn write(&self, bytes: &[u8]);
    fn resize(&self, cols: u16, rows: u16);
}

pub trait ExecTransport: Send + Sync {
    fn start(
        &self,
        request: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession>;
}

enum SessionMsg {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

pub struct KubeExecTransport {
    client: Client,
    handle: tokio::runtime::Handle,
}

impl KubeExecTransport {
    pub(crate) fn new(client: Client, handle: tokio::runtime::Handle) -> KubeExecTransport {
        KubeExecTransport { client, handle }
    }
}

struct KubeExecSession {
    input: tokio::sync::mpsc::Sender<SessionMsg>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ExecSession for KubeExecSession {
    fn write(&self, bytes: &[u8]) {
        // try_send keeps the render thread from ever blocking; the queue is
        // deep enough that dropping means the session is already wedged.
        let _ = self.input.try_send(SessionMsg::Bytes(bytes.to_vec()));
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.input.try_send(SessionMsg::Resize { cols, rows });
    }
}

impl Drop for KubeExecSession {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

impl ExecTransport for KubeExecTransport {
    fn start(
        &self,
        request: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession> {
        let (input_tx, mut input) = tokio::sync::mpsc::channel::<SessionMsg>(INPUT_QUEUE);
        let (cancel_tx, mut cancel) = tokio::sync::oneshot::channel::<()>();
        let client = self.client.clone();
        let request = request.clone();
        self.handle.spawn(async move {
            let api: Api<Pod> = Api::namespaced(client, &request.namespace);
            let mut params = AttachParams::interactive_tty();
            params.container = request.container.clone();
            let attached = tokio::select! {
                _ = &mut cancel => {
                    on_event(ExecEvent::Ended { why: STOPPED.to_string() });
                    return;
                }
                attached = api.exec(&request.pod, request.command.clone(), &params) => attached,
            };
            let mut attached = match attached {
                Ok(attached) => attached,
                Err(error) => {
                    on_event(match classify::<()>("exec", &error) {
                        Fetched::Denied { what } => ExecEvent::Denied { what },
                        Fetched::Failed { what, why } => ExecEvent::Failed { what, why },
                        Fetched::Ok(()) => unreachable!("classify never returns Ok"),
                    });
                    return;
                }
            };
            let (Some(mut stdin), Some(mut stdout), Some(mut resize)) = (
                attached.stdin(),
                attached.stdout(),
                attached.terminal_size(),
            ) else {
                on_event(ExecEvent::Failed {
                    what: "exec",
                    why: "the attached process is missing its tty streams".to_string(),
                });
                return;
            };
            let mut buffer = vec![0u8; OUTPUT_CHUNK];
            loop {
                tokio::select! {
                    _ = &mut cancel => {
                        on_event(ExecEvent::Ended { why: STOPPED.to_string() });
                        return;
                    }
                    message = input.recv() => match message {
                        Some(SessionMsg::Bytes(bytes)) => {
                            if stdin.write_all(&bytes).await.is_err() {
                                on_event(ExecEvent::Ended {
                                    why: "the input stream closed".to_string(),
                                });
                                return;
                            }
                        }
                        Some(SessionMsg::Resize { cols, rows }) => {
                            let _ = resize
                                .send(TerminalSize { width: cols, height: rows })
                                .await;
                        }
                        // The session handle is gone; its Drop also fired
                        // cancel, so this arm is belt and braces and answers
                        // with the same terminal state that arm would.
                        None => {
                            on_event(ExecEvent::Ended { why: STOPPED.to_string() });
                            return;
                        }
                    },
                    read = stdout.read(&mut buffer) => match read {
                        Ok(0) => {
                            on_event(ExecEvent::Ended {
                                why: "the session ended".to_string(),
                            });
                            return;
                        }
                        Ok(n) => on_event(ExecEvent::Output(buffer[..n].to_vec())),
                        Err(error) => {
                            on_event(ExecEvent::Failed {
                                what: "exec",
                                why: error.to_string(),
                            });
                            return;
                        }
                    },
                }
            }
        });
        Box::new(KubeExecSession {
            input: input_tx,
            cancel: Some(cancel_tx),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport that accepts the upgrade request and then never answers,
    /// which is what a real API server holding an exec open looks like from
    /// here: the only thing that can end it is the caller.
    fn hanging_client() -> Client {
        Client::new(
            tower::service_fn(|_: http::Request<kube::client::Body>| {
                std::future::pending::<Result<http::Response<kube::client::Body>, tower::BoxError>>(
                )
            }),
            "prod",
        )
    }

    fn request() -> ExecRequest {
        ExecRequest {
            namespace: "prod".to_string(),
            pod: "api-1".to_string(),
            container: None,
            command: vec!["sh".to_string()],
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_session_while_it_is_still_attaching_ends_it_out_loud() {
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let transport = KubeExecTransport::new(hanging_client(), tokio::runtime::Handle::current());
        let session = transport.start(
            &request(),
            Box::new(move |event| {
                let _ = events.send(event);
            }),
        );

        drop(session);

        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), seen.recv())
            .await
            .expect("a cancelled session answers rather than going silent");
        assert_eq!(
            ended,
            Some(ExecEvent::Ended {
                why: STOPPED.to_string()
            }),
            "the terminal state is labelled the same way a stopped log follow is"
        );
    }
}
