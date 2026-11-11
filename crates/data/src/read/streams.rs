//! Log follows, exec sessions, port forwards and usage polling.

use super::{Fetched, Reader};
use crate::exec::{ExecEvent, ExecRequest, ExecSession};
use crate::exec::{ExecTransport, KubeExecTransport};
use crate::forward::{ForwardRegistry, ForwardRequest, ForwardReservation, ForwardRow};
use crate::logs::{LogChunk, LogRequest, LogStop};
use crate::metrics::{UsageOutcome, UsageRequest, UsageStop};
use crate::{forward, logs, metrics};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

impl Reader {
    pub fn follow_log(
        &self,
        request: LogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        logs::follow(&self.handle, self.client.clone(), request, on_chunk)
    }

    // The managed forward registry: list and close are synchronous local
    // state; opening goes through `open_forward` because the target has to
    // be resolved on the cluster first.
    pub fn forwards(&self) -> &ForwardRegistry {
        &self.forwards
    }

    // Resolution only -- what pod and which ports a request means -- with no
    // listener bound; this is the half a scripted API server can prove.
    pub fn resolve_forward(
        &self,
        request: ForwardRequest,
        reply: impl FnOnce(Fetched<forward::ForwardSpec>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(forward::resolve(&client, &request).await);
        });
    }

    // Resolve, then register: the registry's answer (collision, cap) comes
    // back as a labelled failure like everything else.
    pub fn open_forward(
        &self,
        request: ForwardRequest,
        reply: impl FnOnce(Fetched<ForwardRow>) + Send + 'static,
    ) {
        let client = self.client.clone();
        let registry = self.forwards.clone();
        self.handle.spawn(async move {
            match forward::resolve(&client, &request).await {
                Fetched::Ok(spec) => reply(match registry.open(spec) {
                    Ok(row) => Fetched::Ok(row),
                    Err(error) => Fetched::Failed {
                        what: "port-forward",
                        why: error.to_string(),
                    },
                }),
                Fetched::Denied { what } => reply(Fetched::Denied { what }),
                Fetched::Failed { what, why } => reply(Fetched::Failed { what, why }),
            }
        });
    }

    /// Resolve within four seconds, then reserve an automatic local listener for the browser.
    /// A reply already being delivered may run after cancellation; discarding
    /// its reservation releases this caller's claim without closing other users.
    pub fn open_forward_cancellable(
        &self,
        request: ForwardRequest,
        reply: impl FnOnce(Fetched<ForwardReservation>) + Send + 'static,
    ) -> forward::ForwardOpenStop {
        let client = self.client.clone();
        let registry = self.forwards.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = cancelled.clone();
        let task = self.handle.spawn(async move {
            open_cancellable(
                &registry,
                forward::resolve(&client, &request),
                &task_cancelled,
                reply,
            )
            .await;
        });
        forward::ForwardOpenStop::new(cancelled, task.abort_handle())
    }

    /// Wait for the local listener of a registered forward. Aborting this wait
    /// leaves the forward in the registry; closing it is a separate action.
    pub fn wait_forward_ready(
        &self,
        id: u64,
        reply: impl FnOnce(Fetched<ForwardRow>) + Send + 'static,
    ) -> tokio::task::AbortHandle {
        let registry = self.forwards.clone();
        self.handle
            .spawn(async move { reply(registry.wait_ready(id).await) })
            .abort_handle()
    }

    /// Wait without polling for a forward to die or be closed.
    pub fn wait_forward_terminal(
        &self,
        id: u64,
        reply: impl FnOnce(forward::ForwardTerminal) + Send + 'static,
    ) -> tokio::task::AbortHandle {
        let registry = self.forwards.clone();
        self.handle
            .spawn(async move { reply(registry.wait_terminal(id).await) })
            .abort_handle()
    }

    // An interactive exec over the kube transport; the returned session
    // carries input and resize, and dropping it terminates the remote shell.
    pub fn start_exec(
        &self,
        request: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession> {
        KubeExecTransport::new(self.client.clone(), self.handle.clone()).start(request, on_event)
    }

    // Live usage for a pod or a workload, re-fetched on the request's own
    // cadence until the guard drops; every answer is a labelled
    // [`UsageOutcome`], and a tick that repeats the last one is not
    // re-delivered. Denied and Absent end the poll themselves.
    pub fn poll_usage(
        &self,
        request: UsageRequest,
        on_update: Box<dyn Fn(UsageOutcome) + Send + Sync>,
    ) -> UsageStop {
        metrics::poll(
            &self.handle,
            self.client.clone(),
            self.targets.clone(),
            request,
            on_update,
        )
    }

    // One merged follow over the pods the workload's selector matches; the
    // returned guard cancels every underlying follow.
    pub fn follow_workload_logs(
        &self,
        request: logs::WorkloadLogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        let Some(target) = self.target(request.kind) else {
            on_chunk(LogChunk::Failed {
                what: "workload logs",
                why: "this kind is not served by the connected cluster".to_string(),
            });
            return LogStop::noop();
        };
        logs::follow_workload(&self.handle, self.client.clone(), target, request, on_chunk)
    }
}

async fn open_cancellable(
    registry: &ForwardRegistry,
    resolve: impl std::future::Future<Output = Fetched<forward::ForwardSpec>>,
    cancelled: &AtomicBool,
    reply: impl FnOnce(Fetched<ForwardReservation>),
) {
    let result = match tokio::time::timeout(std::time::Duration::from_secs(4), resolve).await {
        Ok(result) => result,
        Err(_) => Fetched::Failed {
            what: "port-forward",
            why: "forward target resolution did not finish within 4 seconds".to_string(),
        },
    };
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    match result {
        Fetched::Ok(mut spec) => {
            spec.local_port = 0;
            match registry.reserve(spec) {
                Ok(reservation) => {
                    if !cancelled.load(Ordering::Acquire) {
                        reply(Fetched::Ok(reservation));
                    }
                }
                Err(error) => reply(Fetched::Failed {
                    what: "port-forward",
                    why: error.to_string(),
                }),
            }
        }
        Fetched::Denied { what } => reply(Fetched::Denied { what }),
        Fetched::Failed { what, why } => reply(Fetched::Failed { what, why }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward::{ForwardEvent, ForwardGuard, ForwardSpec, Forwarder};
    use std::sync::atomic::AtomicUsize;

    struct TestForwarder {
        starts: AtomicUsize,
        cancel_second: Option<Arc<AtomicBool>>,
    }

    impl Forwarder for TestForwarder {
        fn start(
            &self,
            _: &ForwardSpec,
            _: Box<dyn Fn(ForwardEvent) + Send + Sync>,
        ) -> ForwardGuard {
            if self.starts.fetch_add(1, Ordering::Relaxed) == 1
                && let Some(cancelled) = &self.cancel_second
            {
                cancelled.store(true, Ordering::Release);
            }
            ForwardGuard::noop()
        }
    }

    fn spec(pod: &str, port: u16) -> ForwardSpec {
        ForwardSpec {
            namespace: "prod".into(),
            pod: pod.into(),
            local_port: port,
            remote_port: port,
            expected_service: None,
        }
    }

    #[tokio::test]
    async fn browser_resolution_keeps_the_remote_port_and_requests_an_automatic_listener() {
        let registry = ForwardRegistry::new(Arc::new(TestForwarder {
            starts: AtomicUsize::new(0),
            cancel_second: None,
        }));
        let cancelled = AtomicBool::new(false);
        open_cancellable(
            &registry,
            std::future::ready(Fetched::Ok(spec("grafana", 80))),
            &cancelled,
            |result| {
                let Fetched::Ok(reservation) = result else {
                    panic!("the browser reservation");
                };
                assert_eq!(reservation.row().spec.local_port, 0);
                assert_eq!(reservation.row().spec.remote_port, 80);
                assert_eq!(reservation.row().spec.pod, "grafana");
                assert_eq!(reservation.row().spec.namespace, "prod");
            },
        )
        .await;
        assert!(registry.list().is_empty());
    }

    #[tokio::test]
    async fn cancellation_during_registration_closes_only_the_new_forward() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let registry = ForwardRegistry::new(Arc::new(TestForwarder {
            starts: AtomicUsize::new(0),
            cancel_second: Some(cancelled.clone()),
        }));
        let existing = registry.open(spec("existing", 8000)).expect("opens");
        let replied = Arc::new(AtomicBool::new(false));
        let reply_seen = replied.clone();
        open_cancellable(
            &registry,
            std::future::ready(Fetched::Ok(spec("new", 8001))),
            &cancelled,
            move |_| reply_seen.store(true, Ordering::Release),
        )
        .await;
        assert!(!replied.load(Ordering::Acquire));
        assert_eq!(registry.list(), vec![existing]);
    }

    #[tokio::test]
    async fn synchronous_reply_can_discard_its_new_forward() {
        let registry = ForwardRegistry::new(Arc::new(TestForwarder {
            starts: AtomicUsize::new(0),
            cancel_second: None,
        }));
        let existing = registry.open(spec("existing", 8000)).expect("opens");
        let owner = registry.clone();
        let cancelled = AtomicBool::new(false);
        open_cancellable(
            &registry,
            std::future::ready(Fetched::Ok(spec("new", 8001))),
            &cancelled,
            move |result| {
                let Fetched::Ok(reservation) = result else {
                    panic!("the new forward was registered");
                };
                let id = reservation.row().id;
                assert!(owner.list().iter().any(|row| row.id == id));
                drop(reservation);
                assert!(owner.list().iter().all(|row| row.id != id));
            },
        )
        .await;
        assert_eq!(registry.list(), vec![existing]);
    }

    #[tokio::test]
    async fn cancellation_and_discard_do_not_close_a_reused_forward() {
        let registry = ForwardRegistry::new(Arc::new(TestForwarder {
            starts: AtomicUsize::new(0),
            cancel_second: None,
        }));
        let target = spec("existing", 8000);
        let existing = registry.open(target.clone()).expect("opens");
        let cancelled = AtomicBool::new(false);
        open_cancellable(
            &registry,
            std::future::ready(Fetched::Ok(target.clone())),
            &cancelled,
            |result| {
                let Fetched::Ok(reservation) = result else {
                    panic!("the existing forward was reserved");
                };
                assert_eq!(reservation.row(), &existing);
            },
        )
        .await;
        assert_eq!(registry.list(), vec![existing.clone()]);

        let cancelled_during_resolution = AtomicBool::new(false);
        let resolve = async {
            cancelled_during_resolution.store(true, Ordering::Release);
            Fetched::Ok(target)
        };
        open_cancellable(&registry, resolve, &cancelled_during_resolution, |_| {
            panic!("a cancelled resolution does not reply");
        })
        .await;
        assert_eq!(registry.list(), vec![existing]);
    }

    #[tokio::test]
    async fn cancelling_pending_resolution_preserves_existing_forwards() {
        let registry = ForwardRegistry::new(Arc::new(TestForwarder {
            starts: AtomicUsize::new(0),
            cancel_second: None,
        }));
        let existing = registry.open(spec("existing", 8000)).expect("opens");
        let cancelled = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        let target = registry.clone();
        let task_cancelled = cancelled.clone();
        let task = tokio::spawn(async move {
            let resolve = async move {
                let _ = started_tx.send(());
                std::future::pending::<Fetched<ForwardSpec>>().await
            };
            open_cancellable(&target, resolve, &task_cancelled, move |result| {
                let _ = reply_tx.send(result);
            })
            .await;
        });
        let stop = forward::ForwardOpenStop::new(cancelled, task.abort_handle());
        started_rx.await.expect("resolution was polled");
        drop(stop);
        assert!(task.await.expect_err("the task was aborted").is_cancelled());
        assert!(matches!(
            reply_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert_eq!(registry.list(), vec![existing]);
    }

    #[tokio::test(start_paused = true)]
    async fn target_resolution_has_a_finite_deadline() {
        let registry = ForwardRegistry::new(Arc::new(TestForwarder {
            starts: AtomicUsize::new(0),
            cancel_second: None,
        }));
        let cancelled = AtomicBool::new(false);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut pending = Box::pin(open_cancellable(
            &registry,
            std::future::pending::<Fetched<ForwardSpec>>(),
            &cancelled,
            move |result| {
                let _ = tx.send(result);
            },
        ));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        pending.await;
        assert!(matches!(
            rx.await.expect("the deadline replies"),
            Fetched::Failed {
                what: "port-forward",
                why,
            } if why == "forward target resolution did not finish within 4 seconds"
        ));
        assert!(registry.list().is_empty());
    }
}
