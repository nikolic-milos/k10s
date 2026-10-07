//! The Kubernetes data plane: reads, apply, and named day-2 writes.
//!
//! Everything here reaches the cluster through the API server with an
//! ordinary kubeconfig: no operator, agent, CRD, or in-cluster footprint of
//! any kind, ever. Watches recover from 410 by relisting and reaping, RBAC is
//! probed upfront into a tri-state capability verdict so denial degrades into
//! a label rather than an empty map, and Secrets are projected to metadata
//! structurally -- a secret value must never reach a snapshot, a log line, or
//! an error message. Error text shown to a person is redaction-filtered
//! because exec credential plugins leak their environment through `Debug`.
//! The `kube::Client` seam is a `tower` service, which is what lets
//! `tests/scripted_apiserver.rs` be the API server. A connection hands out two
//! clients over one transport: a confirmed write, and every read that decides
//! it, goes through the interactive one, which never resends a request it has
//! written and answers within a deadline; watches, streams, follows and every
//! list outside a confirmed write stay on the sync one, which retries 429, 503
//! and 504 with backoff and waits as long as a stream needs.

pub mod apply;
pub mod assemble;
pub mod browse;
pub mod connect;
pub mod day2;
mod deadline;
pub mod describe;
pub mod discover;
pub mod exec;
pub mod forward;
pub mod inspect;
pub mod integrations;
pub mod logs;
pub mod manifest;
#[cfg(test)]
mod manifest_test;
pub mod mapping;
pub mod metrics;
pub mod netpol;
pub mod nodes;
pub mod openapi;
pub mod overlay;
mod projection;
pub mod pss;
pub mod rbac;
pub mod rbac_index;
pub mod reach;
pub mod read;
mod served;
pub mod telemetry;
pub mod watch;

mod runtime;

pub use runtime::{
    ClusterReport, DEFAULT_EVENT_SINK_CAPACITY, DataError, DataPlane, EventSink, IngestMetrics,
    MetricsSnapshot, Options, Sync, spawn, sync_from, verdicts,
};
