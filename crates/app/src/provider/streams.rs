//! Translate stream and session events into provider outcomes.

use k10s_data::read::Fetched;

// The data plane's session behind the shell's trait: same shape, different
// crate, so the shell never links kube.
pub(super) struct ExecSessionAdapter(pub(super) Box<dyn k10s_data::exec::ExecSession>);

impl k10s_provider::ExecSession for ExecSessionAdapter {
    fn write(&self, bytes: &[u8]) {
        self.0.write(bytes);
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.0.resize(cols, rows);
    }
}

pub(super) fn adapt_forward(row: k10s_data::forward::ForwardRow) -> k10s_provider::ForwardRow {
    use k10s_data::forward::ForwardState;
    k10s_provider::ForwardRow {
        id: row.id,
        namespace: row.spec.namespace,
        pod: row.spec.pod,
        local_port: row.spec.local_port,
        remote_port: row.spec.remote_port,
        state: match row.state {
            ForwardState::Opening => k10s_provider::ForwardState::Opening,
            ForwardState::Active => k10s_provider::ForwardState::Active,
            ForwardState::Dead { why } => k10s_provider::ForwardState::Dead(why),
        },
    }
}

pub(super) fn adapt_chunk(chunk: k10s_data::logs::LogChunk) -> k10s_provider::LogChunk {
    use k10s_data::logs::LogChunk;
    match chunk {
        LogChunk::Lines(lines) => k10s_provider::LogChunk::Lines(lines),
        LogChunk::Ended { why } => k10s_provider::LogChunk::Ended(why.to_string()),
        LogChunk::Denied { what } => k10s_provider::LogChunk::Denied(what),
        LogChunk::Failed { why, .. } => k10s_provider::LogChunk::Failed(why),
    }
}

pub(super) fn usage_outcome(
    outcome: k10s_data::metrics::UsageOutcome,
) -> k10s_provider::UsageOutcome {
    use k10s_data::metrics::{UsageOutcome, UsageSource};
    match outcome {
        UsageOutcome::Usage(sample) => {
            k10s_provider::UsageOutcome::Usage(k10s_provider::UsageSample {
                cpu: sample.cpu.map(|cpu| k10s_provider::Millicores(cpu.0)),
                memory: sample.memory.map(|memory| k10s_provider::Bytes(memory.0)),
                cpu_request: sample
                    .cpu_request
                    .map(|cpu| k10s_provider::Millicores(cpu.0)),
                cpu_limit: sample.cpu_limit.map(|cpu| k10s_provider::Millicores(cpu.0)),
                memory_request: sample
                    .memory_request
                    .map(|memory| k10s_provider::Bytes(memory.0)),
                memory_limit: sample
                    .memory_limit
                    .map(|memory| k10s_provider::Bytes(memory.0)),
                source: match sample.source {
                    UsageSource::MetricsServer => k10s_provider::UsageSource::MetricsServer,
                    UsageSource::Kubelet => k10s_provider::UsageSource::Kubelet,
                },
                pods_measured: sample.pods_measured,
                pods_total: sample.pods_total,
                truncated: sample.truncated,
            })
        }
        UsageOutcome::Denied { what } => k10s_provider::UsageOutcome::Denied(what),
        UsageOutcome::Failed { why, .. } => k10s_provider::UsageOutcome::Failed(why),
        UsageOutcome::Absent { why } => k10s_provider::UsageOutcome::Absent(why),
    }
}

pub(super) fn forward_outcome(
    fetched: Fetched<k10s_data::forward::ForwardRow>,
) -> k10s_provider::ForwardOutcome {
    match fetched {
        Fetched::Ok(row) => k10s_provider::ForwardOutcome::Opened(adapt_forward(row)),
        Fetched::Denied { what } => k10s_provider::ForwardOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::ForwardOutcome::Failed(why),
    }
}

pub(super) fn exec_event(event: k10s_data::exec::ExecEvent) -> k10s_provider::ExecEvent {
    use k10s_data::exec::ExecEvent;
    match event {
        ExecEvent::Output(bytes) => k10s_provider::ExecEvent::Output(bytes),
        ExecEvent::Ended { why } => k10s_provider::ExecEvent::Ended(why),
        ExecEvent::Denied { what } => k10s_provider::ExecEvent::Denied(what),
        ExecEvent::Failed { why, .. } => k10s_provider::ExecEvent::Failed(why),
    }
}
