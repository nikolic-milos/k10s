//! The seam the shell reads the cluster through.
//!
//! The shell knows nothing about how detail is fetched: it hands a
//! [`ReadProvider`] a request and a boxed reply callback and gets a value
//! back on some other thread; each view bridges the reply onto the UI
//! executor itself. Every outcome is labelled -- denial, failure, absence --
//! because a panel that goes quiet is indistinguishable from a panel that is
//! lying. When no cluster is connected the [`NullProvider`] answers every
//! question with that fact, so the views degrade into the same labelled
//! states they use for RBAC denial. One method mutates, and it is shaped so
//! that the dry run and the apply are the same call: a conflict and a
//! validation refusal are labelled states carrying what the server said, not
//! error strings someone has to read twice.

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
    // Present exactly when the server offers a next page; handing it back to
    // `fetch_table` loads that page. The node table truncates without one.
    pub continue_token: Option<String>,
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

// One object as editable YAML text, with the identity the editor needs to
// resolve its schema. A Secret arrives structurally metadata-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestOutcome {
    Manifest {
        title: String,
        yaml: String,
        api_version: String,
        kind: String,
        // The object as it was last declared, rendered by the same emitter:
        // the base document of a three-way diff. Absent on anything no
        // client-side apply ever wrote, which a diff has to say rather than
        // invent.
        last_applied: Option<String>,
        // From discovery, not guessed: whether the server takes a patch here
        // at all, and whether an apply may carry a status block.
        patchable: bool,
        status_subresource: bool,
    },
    Denied(&'static str),
    Failed(String),
}

// A server-side apply. The same request with `dry_run` set asks what the server
// *would* store and changes nothing, which is what makes an apply reviewable
// before it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRequest {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
    // Already stripped of everything the server owns.
    pub yaml: String,
    pub dry_run: bool,
    // Take the fields another manager holds. Only ever set once a conflict has
    // named them.
    pub force: bool,
}

// One field another field manager owns, and who owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflicted {
    pub field: String,
    pub manager: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    // What the server would store, or did, rendered like the document the
    // editor opened so the two can be diffed line for line.
    Applied {
        yaml: String,
        dry_run: bool,
    },
    // The server took the request and answered; only rendering that answer
    // failed, because the emitter caps a document at 2 MiB and 64 levels. On a
    // real apply the object is already stored, so this is not a failure and
    // must never be shown as one: the cluster changed and the buffer did not.
    Unrendered {
        dry_run: bool,
        why: String,
    },
    Conflict {
        message: String,
        causes: Vec<Conflicted>,
        truncated: bool,
    },
    // The object moved since it was read. Forcing cannot help; it has to be read
    // again, which is why this is its own state and not a conflict with no
    // fields in it.
    Stale {
        message: String,
    },
    // The document itself was refused.
    Rejected {
        message: String,
        causes: Vec<String>,
    },
    // A write denial keeps the server's sentence: on a write a 403 is as often
    // an admission decision, which explains itself, as it is RBAC, which needs
    // no explaining.
    Denied {
        what: &'static str,
        why: String,
    },
    Failed(String),
}

// One group-version the cluster serves schemas for, and the hash-stamped
// URL its document lives at. The URL is server data; the provider refuses
// any that escapes /openapi/v3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSource {
    pub group_version: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCatalogOutcome {
    Catalog(Vec<SchemaSource>),
    Denied(&'static str),
    Failed(String),
}

// Raw JSON text: an OpenAPI v3 document or a CRD list. Parsing lives in the
// editor engine; the seam only moves bounded text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaTextOutcome {
    Text(String),
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

// Logs for a whole workload: the provider finds the pods (label selector or
// ownership, its choice) and merges their follows into one feed, each line
// naming its pod. The pod-count bound lives with the implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadLogRequest {
    pub namespace: String,
    pub kind: KindId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogChunk {
    Lines(Vec<String>),
    Ended(String),
    Denied(&'static str),
    Failed(String),
}

// A forward start request: a pod row forwards its first declared port, a
// service row resolves through its selector and targetPort. Port choice and
// bounds live with the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub namespace: String,
    pub name: String,
    pub service: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardState {
    Opening,
    Active,
    // Labelled and left visible until closed; a forward that vanishes reads
    // as one that still works.
    Dead(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRow {
    pub id: u64,
    pub namespace: String,
    pub pod: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub state: ForwardState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardOutcome {
    Opened(ForwardRow),
    Denied(&'static str),
    Failed(String),
}

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
    Ended(String),
    Denied(&'static str),
    Failed(String),
}

// The live half of an exec: keystrokes and resizes go down it, dropping it
// terminates the remote session.
pub trait ExecSession: Send {
    fn write(&self, bytes: &[u8]);
    fn resize(&self, cols: u16, rows: u16);
}

pub struct NullExecSession;

impl ExecSession for NullExecSession {
    fn write(&self, _: &[u8]) {}
    fn resize(&self, _: u16, _: u16) {}
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
    // `continue_token` is a token a previous page carried; None means the
    // first page.
    fn fetch_table(&self, kind: KindId, continue_token: Option<String>, reply: Reply<TableOutcome>);
    fn fetch_node_table(&self, reply: Reply<TableOutcome>);
    fn fetch_describe(&self, request: &DescribeRequest, reply: Reply<DocOutcome>);
    fn fetch_manifest(&self, request: &DescribeRequest, reply: Reply<ManifestOutcome>);
    // The one mutating method on the seam. Dry run and apply differ by one
    // field of the request, so a caller cannot reach the second without being
    // able to reach the first.
    fn apply(&self, request: &ApplyRequest, reply: Reply<ApplyOutcome>);
    fn fetch_schema_catalog(&self, reply: Reply<SchemaCatalogOutcome>);
    fn fetch_schema_document(&self, url: &str, reply: Reply<SchemaTextOutcome>);
    fn fetch_crd_schemas(&self, reply: Reply<SchemaTextOutcome>);
    fn fetch_containers(&self, namespace: &str, pod: &str, reply: Reply<ContainersOutcome>);
    fn follow_log(
        &self,
        request: &LogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop;
    fn follow_workload_logs(
        &self,
        request: &WorkloadLogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop;
    fn open_forward(&self, request: &ForwardRequest, reply: Reply<ForwardOutcome>);
    // Local registry state: synchronous, no cluster round trip.
    fn list_forwards(&self) -> Vec<ForwardRow>;
    fn close_forward(&self, id: u64) -> bool;
    fn start_exec(
        &self,
        request: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession>;
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

    fn fetch_table(&self, _: KindId, _: Option<String>, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_node_table(&self, reply: Reply<TableOutcome>) {
        reply(TableOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_describe(&self, _: &DescribeRequest, reply: Reply<DocOutcome>) {
        reply(DocOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_manifest(&self, _: &DescribeRequest, reply: Reply<ManifestOutcome>) {
        reply(ManifestOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn apply(&self, _: &ApplyRequest, reply: Reply<ApplyOutcome>) {
        reply(ApplyOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_schema_catalog(&self, reply: Reply<SchemaCatalogOutcome>) {
        reply(SchemaCatalogOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_schema_document(&self, _: &str, reply: Reply<SchemaTextOutcome>) {
        reply(SchemaTextOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_crd_schemas(&self, reply: Reply<SchemaTextOutcome>) {
        reply(SchemaTextOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn fetch_containers(&self, _: &str, _: &str, reply: Reply<ContainersOutcome>) {
        reply(ContainersOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn follow_log(&self, _: &LogRequest, on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>) -> LogStop {
        on_chunk(LogChunk::Failed(NO_CLUSTER.to_string()));
        LogStop::noop()
    }

    fn follow_workload_logs(
        &self,
        _: &WorkloadLogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        on_chunk(LogChunk::Failed(NO_CLUSTER.to_string()));
        LogStop::noop()
    }

    fn open_forward(&self, _: &ForwardRequest, reply: Reply<ForwardOutcome>) {
        reply(ForwardOutcome::Failed(NO_CLUSTER.to_string()));
    }

    fn list_forwards(&self) -> Vec<ForwardRow> {
        Vec::new()
    }

    fn close_forward(&self, _: u64) -> bool {
        false
    }

    fn start_exec(
        &self,
        _: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession> {
        on_event(ExecEvent::Failed(NO_CLUSTER.to_string()));
        Box::new(NullExecSession)
    }
}
