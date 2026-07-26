//! The Kubernetes data plane.
//!
//! Still a stub, but no longer a *divergent* one: it speaks
//! [`k10s_core::IngestEvent`], the same contract the generator implements and the
//! world consumes. This crate used to define its own `ObjEvent` over a closed
//! `ObjKind` enum that could not name a CRD, which nothing consumed, and it
//! dropped the sender on the floor.
//!
//! What remains for the real cluster phase: kubeconfig and contexts,
//! exec-credential plugins, discovery over `/apis`, watch-based reflectors with
//! `resourceVersion`, bookmarks and 410 recovery, and the RBAC capability probe.
//! The shapes those must produce are already pinned by the scenarios in
//! `k10s_core::replay`, so they can be built against tests rather than against a
//! live cluster.

use crossbeam_channel::Sender;
use k10s_core::IngestEvent;

/// Where a producer sends what it learns. Bounded queueing and coalescing are the
/// consumer's job, via `k10s_core::Intake`.
pub type EventSink = Sender<IngestEvent>;

pub struct DataPlane {
    runtime: tokio::runtime::Runtime,
    events: EventSink,
}

impl DataPlane {
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// The sink watchers publish into. Held rather than dropped, so wiring a
    /// reflector up is an addition here rather than a change of shape.
    pub fn events(&self) -> &EventSink {
        &self.events
    }
}

pub fn spawn(events: EventSink) -> std::io::Result<DataPlane> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("k10s-data")
        .enable_all()
        .build()?;
    Ok(DataPlane { runtime, events })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::{Intake, replay};

    #[test]
    fn the_sink_carries_contract_events_to_an_intake() {
        // The regression this guards: the sender used to be discarded, so nothing a
        // producer said could reach a consumer. Now the types alone connect them.
        let (tx, rx) = crossbeam_channel::unbounded();
        let plane = spawn(tx).expect("build the runtime");

        for event in replay::initial_sync().events {
            plane.events().send(event).expect("sink is live");
        }

        let mut intake = Intake::new();
        while let Ok(event) = rx.try_recv() {
            intake.push(event);
        }
        let drained = intake.drain();
        assert_eq!(
            drained
                .iter()
                .filter(|e| matches!(e, IngestEvent::Resource(_)))
                .count(),
            4
        );
    }
}
