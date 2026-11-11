use std::path::PathBuf;
use std::rc::Rc;

use crate::{ClusterProvider, Reply};

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
pub type ProviderFactory = Box<dyn FnOnce() -> Rc<dyn ClusterProvider> + Send>;

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
/// the implementation owns, exactly as [`ClusterProvider`] does, and the shell
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
