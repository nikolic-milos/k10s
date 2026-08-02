//! The seam the shell reads the cluster through.
//!
//! The shell knows nothing about how detail is fetched: it hands a
//! [`ReadProvider`] a request and a boxed reply callback and gets a value
//! back on some other thread; each view bridges the reply onto the UI
//! executor itself. Every outcome is labelled -- denial, failure, absence --
//! because a panel that goes quiet is indistinguishable from a panel that is
//! lying. When no cluster is connected the [`NullProvider`] answers every
//! question with that fact, so the views degrade into the same labelled
//! states they use for RBAC denial.

use k10s_core::KindId;

pub type Reply<T> = Box<dyn FnOnce(T) + Send>;

#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub when: String,
    pub kind: String,
    pub reason: String,
    pub message: String,
    pub count: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Detail {
    Events(Vec<EventRow>),
    Log(Vec<String>),
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindRow {
    pub id: KindId,
    // "deployments.apps" -- the name a kubectl user would type.
    pub display: String,
    pub kind: String,
    pub namespaced: bool,
    // The probe said no: shown disabled with the reason, still openable, and
    // the table answers with the denial the server actually returns.
    pub forbidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumn {
    pub name: String,
    pub wide: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub cells: Vec<String>,
    pub name: String,
    pub namespace: Option<String>,
    pub uid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TablePage {
    pub columns: Vec<TableColumn>,
    pub rows: Vec<TableRow>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableOutcome {
    Table(TablePage),
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribeRequest {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
    pub uid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocOutcome {
    Doc { title: String, lines: Vec<String> },
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainersOutcome {
    Containers(Vec<String>),
    Denied(&'static str),
    Failed(String),
}

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
    Ended(String),
    Denied(&'static str),
    Failed(String),
}

// Dropping the guard cancels the follow; the adapter decides what that means.
pub struct LogStop(Option<Box<dyn FnOnce() + Send>>);

impl LogStop {
    pub fn new(stop: impl FnOnce() + Send + 'static) -> LogStop {
        LogStop(Some(Box::new(stop)))
    }

    pub fn noop() -> LogStop {
        LogStop(None)
    }
}

impl Drop for LogStop {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop();
        }
    }
}

pub trait ReadProvider {
    fn fetch_events(&self, namespace: &str, name: &str, reply: Reply<Detail>);
    fn fetch_log_tail(&self, namespace: &str, pod: &str, reply: Reply<Detail>);
    fn kinds(&self) -> Vec<KindRow>;
    fn fetch_table(&self, kind: KindId, reply: Reply<TableOutcome>);
    fn fetch_node_table(&self, reply: Reply<TableOutcome>);
    fn fetch_describe(&self, request: &DescribeRequest, reply: Reply<DocOutcome>);
    fn fetch_containers(&self, namespace: &str, pod: &str, reply: Reply<ContainersOutcome>);
    fn follow_log(
        &self,
        request: &LogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop;
}

pub struct NullProvider;

const NO_CLUSTER: &str = "no cluster connected; this view needs one";

impl ReadProvider for NullProvider {
    fn fetch_events(&self, _: &str, _: &str, reply: Reply<Detail>) {
        reply(Detail::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_log_tail(&self, _: &str, _: &str, reply: Reply<Detail>) {
        reply(Detail::Failed(NO_CLUSTER.to_string()));
    }

    fn kinds(&self) -> Vec<KindRow> {
        Vec::new()
    }

    fn fetch_table(&self, _: KindId, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_node_table(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_describe(&self, _: &DescribeRequest, reply: Reply<DocOutcome>) {
        reply(DocOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_containers(&self, _: &str, _: &str, reply: Reply<ContainersOutcome>) {
        reply(ContainersOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn follow_log(&self, _: &LogRequest, on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>) -> LogStop {
        on_chunk(LogChunk::Failed(NO_CLUSTER.to_string()));
        LogStop::noop()
    }
}
