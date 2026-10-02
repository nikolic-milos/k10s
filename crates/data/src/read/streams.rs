//! Log follows, exec sessions, port forwards and usage polling.

use super::{Fetched, Reader};
use crate::exec::{ExecEvent, ExecRequest, ExecSession};
use crate::exec::{ExecTransport, KubeExecTransport};
use crate::forward::{ForwardRegistry, ForwardRequest, ForwardRow};
use crate::logs::{LogChunk, LogRequest, LogStop};
use crate::metrics::{UsageOutcome, UsageRequest, UsageStop};
use crate::{forward, logs, metrics};

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
