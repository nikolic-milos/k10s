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
        expected_service: row.spec.expected_service,
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

pub(super) fn forward_start_outcome(
    fetched: Fetched<k10s_data::forward::ForwardReservation>,
) -> k10s_provider::ForwardStartOutcome {
    match fetched {
        Fetched::Ok(reservation) => {
            let row = adapt_forward(reservation.row().clone());
            let lease = k10s_provider::ForwardLease::new(row, move |keep| {
                if keep {
                    reservation.keep();
                }
            });
            k10s_provider::ForwardStartOutcome::Started(lease)
        }
        Fetched::Denied { what } => k10s_provider::ForwardStartOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::ForwardStartOutcome::Failed(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_data::forward::{ForwardEvent, ForwardGuard, ForwardRegistry, ForwardSpec, Forwarder};
    use std::sync::Arc;

    struct IdleForwarder;
    impl Forwarder for IdleForwarder {
        fn start(
            &self,
            _: &ForwardSpec,
            _: Box<dyn Fn(ForwardEvent) + Send + Sync>,
        ) -> ForwardGuard {
            ForwardGuard::noop()
        }
    }

    #[test]
    fn a_discarded_lease_cannot_close_an_identical_id_in_a_replacement_provider() {
        let old = ForwardRegistry::new(Arc::new(IdleForwarder));
        let replacement = ForwardRegistry::new(Arc::new(IdleForwarder));
        let spec = ForwardSpec {
            namespace: "observability".into(),
            pod: "grafana".into(),
            local_port: 3000,
            remote_port: 3000,
            expected_service: None,
        };
        let old_reservation = old.reserve(spec.clone()).expect("old forward");
        let new_row = replacement.open(spec).expect("new forward");
        assert_eq!(old_reservation.row().id, new_row.id);
        let outcome = forward_start_outcome(Fetched::Ok(old_reservation));
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        assert!(tx.send(outcome).is_err());
        assert!(old.list().is_empty());
        assert_eq!(replacement.list(), vec![new_row]);
    }

    #[test]
    fn discarding_a_reused_forward_does_not_close_it() {
        let registry = ForwardRegistry::new(Arc::new(IdleForwarder));
        let spec = ForwardSpec {
            namespace: "observability".into(),
            pod: "grafana".into(),
            local_port: 3000,
            remote_port: 3000,
            expected_service: None,
        };
        let row = registry.open(spec.clone()).expect("existing forward");
        let reservation = registry.reserve(spec).expect("reuses retained forward");
        let outcome = forward_start_outcome(Fetched::Ok(reservation));
        drop(outcome);
        assert_eq!(registry.list(), vec![row]);
    }

    #[test]
    fn a_discarded_old_reply_cannot_close_the_newer_browser_handoff() {
        let registry = ForwardRegistry::new(Arc::new(IdleForwarder));
        let spec = ForwardSpec {
            namespace: "observability".into(),
            pod: "grafana".into(),
            local_port: 3000,
            remote_port: 3000,
            expected_service: None,
        };
        let old = registry.reserve(spec.clone()).expect("old request");
        let new = registry.reserve(spec).expect("new request shares listener");
        let row = new.row().clone();
        let old_outcome = forward_start_outcome(Fetched::Ok(old));
        let new_outcome = forward_start_outcome(Fetched::Ok(new));
        drop(old_outcome);
        assert_eq!(registry.list(), vec![row.clone()]);
        let k10s_provider::ForwardStartOutcome::Started(lease) = new_outcome else {
            panic!("the new request holds the listener");
        };
        lease.keep();
        assert_eq!(registry.list(), vec![row]);
    }

    #[test]
    fn a_forward_outcome_preserves_the_shared_reviewed_service_origin() {
        let source = Arc::new(k10s_core::ServiceIdentity {
            namespace: "monitoring".into(),
            name: "grafana".into(),
            port: 3000,
            uid: "grafana-a".into(),
            resource_version: "1".into(),
        });
        let answer = forward_outcome(Fetched::Ok(k10s_data::forward::ForwardRow {
            id: 7,
            spec: ForwardSpec {
                namespace: "monitoring".into(),
                pod: "grafana-pod".into(),
                local_port: 3000,
                remote_port: 3000,
                expected_service: Some(source.clone()),
            },
            state: k10s_data::forward::ForwardState::Active,
        }));
        let k10s_provider::ForwardOutcome::Opened(row) = answer else {
            panic!("the opened forward must project");
        };
        assert!(Arc::ptr_eq(row.expected_service.as_ref().unwrap(), &source));
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
