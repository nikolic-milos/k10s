//! Exec into a container, behind a transport seam.
//!
//! The terminal never sees kube: it writes keystrokes and resizes into an
//! [`ExecSession`] and receives raw bytes and labelled terminal states from
//! an [`ExecTransport`]. The split exists because the real transport is a
//! WebSocket upgrade (`AttachedProcess`) the scripted API server cannot
//! speak: everything above the seam -- grid state, input encoding, resize --
//! is tested against a fake transport in `k10s-shell`, while the kube
//! implementation below stays thin plumbing, proven only against a live
//! cluster. Input is bounded by a fixed queue; a session that ends, is
//! denied, or fails says so through its event callback, never by going
//! silent.

use futures::SinkExt;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, AttachParams, TerminalSize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::read::{Fetched, classify};

// Keystrokes and resizes waiting for the WebSocket; a full queue drops the
// oldest-pending writes rather than growing, and a human cannot type past it.
const INPUT_QUEUE: usize = 256;
// One read's worth of remote output handed to the callback at a time.
const OUTPUT_CHUNK: usize = 8 * 1024;

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
                _ = &mut cancel => return,
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
                    _ = &mut cancel => return,
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
                        // cancel, so this arm is belt and braces.
                        None => return,
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
