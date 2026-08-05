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
//!
//! A cluster is chosen on screen now rather than on the command line, so the
//! provider a view was built with is no longer the provider it must keep.
//! [`ProviderSlot`] is the one place it lives: every view clones the slot, and
//! adopting a connection re-points all of them at once. And because the connect
//! itself happens off the UI thread, what crosses back is a [`Connection`] --
//! `Send`, carrying only what may be shown, with the `Rc` built at the far end.
//! [`LaunchProvider`] is that seam: kubeconfig contexts, a connect, and the
//! generated starmap, all answered the same way `ReadProvider` answers.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

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
        // Which object this text is, from the response that produced it. An
        // apply's answer carries the same field, and a server-side apply creates
        // what is absent, so comparing the two is what tells an update from a
        // recreation. Absent means the server sent none, which is neither
        // answer.
        uid: Option<String>,
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
        // Which object the server answered about, so a review can say whether the
        // press updated the object it was opened from or landed on a different
        // one. None is "the answer carried no identity", never "the same".
        uid: Option<String>,
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
    // Helm's stored releases, as a document. Rendered on the far side of the seam
    // like a describe is, for the same reason: one deterministic rendering a test
    // can gate, and a shell that holds no release payload of its own.
    fn fetch_releases(&self, reply: Reply<DocOutcome>);
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

    fn fetch_releases(&self, reply: Reply<DocOutcome>) {
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

/// The one place the workspace's provider lives.
///
/// Every view clones this rather than the provider behind it, so adopting a
/// connection after the window is open re-points the whole workspace at once.
/// Without it a cluster chosen from the launch screen would reach the views
/// opened *after* the choice and nothing that was already on screen, and a
/// cluster replaced would leave open views holding a data plane that had since
/// been retired.
pub struct ProviderSlot(RefCell<Rc<dyn ReadProvider>>);

impl ProviderSlot {
    pub fn new(provider: Rc<dyn ReadProvider>) -> ProviderSlot {
        ProviderSlot(RefCell::new(provider))
    }

    pub fn empty() -> ProviderSlot {
        ProviderSlot::new(Rc::new(NullProvider))
    }

    pub fn set(&self, provider: Rc<dyn ReadProvider>) {
        *self.0.borrow_mut() = provider;
    }

    // The borrow ends before the call, never during it: [`NullProvider`]
    // answers synchronously, so a delegate that held the borrow across the
    // call would make any reply that reached back here a panic instead of an
    // answer. An `Rc` clone is the price and it is not a real one.
    fn get(&self) -> Rc<dyn ReadProvider> {
        self.0.borrow().clone()
    }
}

impl ReadProvider for ProviderSlot {
    fn fetch_events(&self, namespace: &str, name: &str, reply: Reply<Detail>) {
        self.get().fetch_events(namespace, name, reply);
    }

    fn fetch_log_tail(&self, namespace: &str, pod: &str, reply: Reply<Detail>) {
        self.get().fetch_log_tail(namespace, pod, reply);
    }

    fn kinds(&self) -> Vec<KindRow> {
        self.get().kinds()
    }

    fn fetch_table(
        &self,
        kind: KindId,
        continue_token: Option<String>,
        reply: Reply<TableOutcome>,
    ) {
        self.get().fetch_table(kind, continue_token, reply);
    }

    fn fetch_node_table(&self, reply: Reply<TableOutcome>) {
        self.get().fetch_node_table(reply);
    }

    fn fetch_describe(&self, request: &DescribeRequest, reply: Reply<DocOutcome>) {
        self.get().fetch_describe(request, reply);
    }

    fn fetch_releases(&self, reply: Reply<DocOutcome>) {
        self.get().fetch_releases(reply);
    }

    fn fetch_manifest(&self, request: &DescribeRequest, reply: Reply<ManifestOutcome>) {
        self.get().fetch_manifest(request, reply);
    }

    fn apply(&self, request: &ApplyRequest, reply: Reply<ApplyOutcome>) {
        self.get().apply(request, reply);
    }

    fn fetch_schema_catalog(&self, reply: Reply<SchemaCatalogOutcome>) {
        self.get().fetch_schema_catalog(reply);
    }

    fn fetch_schema_document(&self, url: &str, reply: Reply<SchemaTextOutcome>) {
        self.get().fetch_schema_document(url, reply);
    }

    fn fetch_crd_schemas(&self, reply: Reply<SchemaTextOutcome>) {
        self.get().fetch_crd_schemas(reply);
    }

    fn fetch_containers(&self, namespace: &str, pod: &str, reply: Reply<ContainersOutcome>) {
        self.get().fetch_containers(namespace, pod, reply);
    }

    fn follow_log(
        &self,
        request: &LogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        self.get().follow_log(request, on_chunk)
    }

    fn follow_workload_logs(
        &self,
        request: &WorkloadLogRequest,
        on_chunk: Box<dyn Fn(LogChunk) + Send + Sync>,
    ) -> LogStop {
        self.get().follow_workload_logs(request, on_chunk)
    }

    fn open_forward(&self, request: &ForwardRequest, reply: Reply<ForwardOutcome>) {
        self.get().open_forward(request, reply);
    }

    fn list_forwards(&self) -> Vec<ForwardRow> {
        self.get().list_forwards()
    }

    fn close_forward(&self, id: u64) -> bool {
        self.get().close_forward(id)
    }

    fn start_exec(
        &self,
        request: &ExecRequest,
        on_event: Box<dyn Fn(ExecEvent) + Send + Sync>,
    ) -> Box<dyn ExecSession> {
        self.get().start_exec(request, on_event)
    }
}

/// One context a kubeconfig declares, reduced to what may be shown.
///
/// A kubeconfig holds credentials. `client-certificate-data`, `client-key-data`,
/// `token` and `password` are the credential itself rather than a path to one,
/// and an exec plugin's argument vector routinely carries an account or a
/// project. This struct is where that is decided rather than remembered: the
/// only fields that exist are the ones safe to render, so nothing downstream
/// can leak by forgetting to filter and nothing upstream can leak by adding a
/// field to a struct that already travelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRow {
    pub name: String,
    /// This source's own `current-context`.
    pub current: bool,
    /// The cluster's API server URL.
    pub server: Option<String>,
    /// The namespace the context defaults to, when it sets one.
    pub namespace: Option<String>,
}

/// One place contexts were read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    /// What to call it on screen: a file path, or a sentence for an in-cluster
    /// service account, which declares no contexts and needs none.
    pub label: String,
    pub contexts: Vec<ContextRow>,
    /// An in-cluster account is connectable with no context named, which is
    /// what makes an empty `contexts` list a row here rather than a dead end.
    pub implicit: bool,
    /// Why this source offered nothing, when that is a failure and not an empty
    /// file. A source that will not read keeps its place with the reason
    /// attached: one bad file among several must not silently shorten the list.
    pub note: Option<String>,
}

/// Which kubeconfigs to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanRequest {
    /// Whatever this process can see: `KUBECONFIG`, else `~/.kube/config`, plus
    /// an in-cluster service account when one is mounted.
    Detected,
    /// One file the user pointed at.
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    /// Every source that could be read. An empty list means there was nothing
    /// to read, which is a different sentence from a file that would not parse
    /// and is worded differently on screen.
    Sources(Vec<ConfigSource>),
    Failed(String),
}

/// Which context, from which source. The source travels with the choice so a
/// context listed out of a file the user opened is connected through that file
/// rather than through whatever `KUBECONFIG` happens to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectRequest {
    pub source: ScanRequest,
    /// `None` names the source's implicit account.
    pub context: Option<String>,
}

/// Turns the parts of a connection that can cross a thread into the provider
/// that cannot. Called on the thread that will own the `Rc`.
pub type ProviderFactory = Box<dyn FnOnce() -> Rc<dyn ReadProvider> + Send>;

/// A connection that already succeeded, on its way back to the UI thread.
pub struct Connection {
    /// What the connection resolved to, for the label beside the state dot.
    /// `None` is an in-cluster account, which has no context name.
    pub context: Option<String>,
    /// One line: what synced, how much of it, how long it took.
    pub summary: String,
    /// Every way this connection is degraded -- a probe that could not run, a
    /// kind that is present but forbidden. Already redacted, because the
    /// reasons quote the server.
    pub notes: Vec<String>,
    pub provider: ProviderFactory,
}

pub enum ConnectOutcome {
    Connected(Connection),
    /// Refused, unreachable, or a credential that would not mint. The launch
    /// screen stays open and usable on this: an unreachable cluster is the
    /// case where a dead end would be worst.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemoOutcome {
    /// The generated scene is on its way into the world; the line says what
    /// was made.
    Started(String),
    Failed(String),
}

/// What the launch screen may ask of the world outside the shell.
///
/// Three questions, none of which may be a return value. Reading a kubeconfig
/// is file I/O on a path that can be a stalled network mount, connecting is a
/// round trip through a credential plugin, and generating a starmap is a
/// CPU-bound second -- so all three answer through a boxed reply from a thread
/// the implementation owns, exactly as [`ReadProvider`] does, and the shell
/// bridges the answer onto its own executor.
pub trait LaunchProvider {
    fn scan(&self, request: ScanRequest, reply: Reply<ScanOutcome>);
    fn connect(&self, request: ConnectRequest, reply: Reply<ConnectOutcome>);
    fn generate(&self, reply: Reply<DemoOutcome>);
}

/// The launch seam with nothing behind it: what a bench flight and any window
/// built without an application get. Every answer is the labelled absence, so
/// the screen says so instead of waiting forever.
pub struct NullLaunchProvider;

const NO_LAUNCH: &str = "this process was started without a kubeconfig service";

impl LaunchProvider for NullLaunchProvider {
    fn scan(&self, _: ScanRequest, reply: Reply<ScanOutcome>) {
        reply(ScanOutcome::Failed(NO_LAUNCH.to_string()));
    }

    fn connect(&self, _: ConnectRequest, reply: Reply<ConnectOutcome>) {
        reply(ConnectOutcome::Failed(NO_LAUNCH.to_string()));
    }

    fn generate(&self, reply: Reply<DemoOutcome>) {
        reply(DemoOutcome::Failed(NO_LAUNCH.to_string()));
    }
}
