//! Connecting to an API server: which kubeconfig, which context, one client each.
//!
//! Everything kube-rs already does correctly is delegated to it: the KUBECONFIG
//! merge rules, exec-credential plugins, OIDC refresh, client certificates,
//! proxies and the in-cluster service-account layout all live in
//! [`kube::config`]. What is added here is the part kube-rs deliberately leaves
//! to the caller, and the part that has to work on a machine with no cluster:
//!
//! - **The source decision is a pure function.** [`plan`] turns an [`Env`] into
//!   an ordered list of [`Source`]s. `Config::infer` makes the same decision by
//!   catching an error from the kubeconfig path, which cannot be tested without
//!   arranging for that error; a function over an injected environment can.
//! - **Context selection is a pure function.** [`resolve_context`] answers
//!   "which context will actually be used, and what are the alternatives" from a
//!   parsed [`Kubeconfig`], so an unknown `--context` is a listed set of choices
//!   rather than a failed request.
//! - **A per-context client cache that honours credential expiry.** A client
//!   built from an exec plugin that returned a *client certificate* carries
//!   [`Client::valid_until`], and kube-rs documents that checking it is the
//!   caller's job: the TLS identity is baked into the connector and no auth
//!   layer will refresh it. Caching such a client forever is how a session dies
//!   an hour in.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};

/// Where credentials come from, decided before any I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Merge these files in order, first-wins, exactly as `kubectl` does.
    Kubeconfig(Vec<PathBuf>),
    /// A service account mounted into a pod.
    InCluster,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Kubeconfig(files) => {
                let names: Vec<String> = files
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>();
                write!(f, "kubeconfig {}", names.join(":"))
            }
            Source::InCluster => write!(f, "in-cluster service account"),
        }
    }
}

/// The environment inputs the source decision depends on.
///
/// Injected rather than read inside [`plan`] so a test can be a pod, or a
/// machine with three kubeconfigs, or a machine with nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `KUBECONFIG`, unsplit. Path separators are platform-specific, so
    /// splitting stays in [`plan`].
    pub kubeconfig: Option<OsString>,
    /// `~/.kube/config`, already existence-checked. `None` means it is not
    /// there, which is why [`plan`] itself needs no filesystem.
    pub default_kubeconfig: Option<PathBuf>,
    /// `KUBERNETES_SERVICE_HOST` is set *and* the service-account token is
    /// mounted. Both, because either alone is a half-configured pod.
    pub in_cluster: bool,
}

/// Where a mounted service account puts its token. Named here so [`Env`] can
/// check for it rather than discovering its absence through a failed request.
const IN_CLUSTER_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

impl Env {
    /// Reads the process environment. The only function in this module that
    /// touches it.
    pub fn from_process() -> Env {
        let default_kubeconfig = home_dir()
            .map(|home| home.join(".kube").join("config"))
            .filter(|p| p.is_file());
        Env {
            kubeconfig: std::env::var_os("KUBECONFIG"),
            default_kubeconfig,
            in_cluster: std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
                && Path::new(IN_CLUSTER_TOKEN).is_file(),
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(v) = std::env::var_os(key)
            && !v.is_empty()
        {
            return Some(PathBuf::from(v));
        }
    }
    None
}

/// The sources to try, in order.
///
/// `KUBECONFIG` beats `~/.kube/config` beats a mounted service account, which is
/// `kubectl`'s order. In-cluster stays in the list even when a kubeconfig is
/// present, because a pod that ships both should still start if the kubeconfig
/// turns out to be unusable.
///
/// An empty result means there is nothing to try, which is a clearer answer than
/// a connection error.
pub fn plan(env: &Env) -> Vec<Source> {
    let mut out = Vec::new();
    if let Some(raw) = &env.kubeconfig {
        // Empty entries are ignored per the merge rules, and a KUBECONFIG that
        // is entirely empty entries is the same as unset.
        let files: Vec<PathBuf> = std::env::split_paths(raw)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !files.is_empty() {
            out.push(Source::Kubeconfig(files));
        }
    }
    if out.is_empty()
        && let Some(path) = &env.default_kubeconfig
    {
        out.push(Source::Kubeconfig(vec![path.clone()]));
    }
    if env.in_cluster {
        out.push(Source::InCluster);
    }
    out
}

/// What went wrong before we ever reached the API server.
#[derive(Debug)]
pub enum ConnectError {
    /// Nothing to try: no `KUBECONFIG`, no `~/.kube/config`, no service account.
    NoSource,
    /// Every candidate source failed, with the reason for each.
    NoUsableSource(Vec<(Source, String)>),
    /// A context was asked for by name and the merged kubeconfig has no such
    /// context. Carries what it does have, because "did you mean" beats "no".
    UnknownContext {
        requested: String,
        available: Vec<String>,
    },
    /// No `--context` and no usable `current-context`.
    NoCurrentContext { available: Vec<String> },
    /// Building the client failed: bad CA, exec plugin returned nonsense, TLS
    /// stack refused the identity.
    Client(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NoSource => write!(
                f,
                "no kubeconfig: set KUBECONFIG, create ~/.kube/config, or run inside a cluster"
            ),
            ConnectError::NoUsableSource(tried) => {
                write!(f, "no usable kubeconfig")?;
                for (source, why) in tried {
                    write!(f, "\n  {source}: {why}")?;
                }
                Ok(())
            }
            ConnectError::UnknownContext {
                requested,
                available,
            } => {
                write!(f, "no context named {requested:?}")?;
                if available.is_empty() {
                    write!(f, "; the kubeconfig declares none")
                } else {
                    write!(f, "; available: {}", available.join(", "))
                }
            }
            ConnectError::NoCurrentContext { available } => {
                write!(f, "the kubeconfig sets no usable current-context")?;
                if !available.is_empty() {
                    write!(f, "; pick one of: {}", available.join(", "))?;
                }
                Ok(())
            }
            ConnectError::Client(why) => write!(f, "cannot build a client: {why}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Every context the merged kubeconfig declares, in file order.
pub fn contexts(cfg: &Kubeconfig) -> Vec<String> {
    cfg.contexts.iter().map(|c| c.name.clone()).collect()
}

/// Which context will be used, given an optional request.
///
/// Pure over a parsed kubeconfig, which is what makes the interesting cases
/// (a `current-context` naming a context that was merged away, a request for a
/// context that only exists in a file that lost the merge) testable from a YAML
/// string.
pub fn resolve_context(cfg: &Kubeconfig, requested: Option<&str>) -> Result<String, ConnectError> {
    let available = contexts(cfg);
    match requested {
        Some(name) => {
            if available.iter().any(|c| c == name) {
                Ok(name.to_string())
            } else {
                Err(ConnectError::UnknownContext {
                    requested: name.to_string(),
                    available,
                })
            }
        }
        None => match cfg.current_context.as_deref() {
            Some(name) if available.iter().any(|c| c == name) => Ok(name.to_string()),
            _ => Err(ConnectError::NoCurrentContext { available }),
        },
    }
}

/// How long before a credential's stated expiry a cached client is retired.
///
/// A watch opened with three seconds of validity left is a watch that dies
/// mid-stream, so the cache would rather pay for a rebuild than hand out a
/// client that is technically still valid.
const CREDENTIAL_SKEW_SECS: i64 = 30;

/// Whether a cached client may still be handed out.
///
/// `None` means the credential carries no expiry, which is the common case: a
/// bearer token, a static client certificate, or an exec plugin whose token is
/// refreshed by kube-rs's own auth layer. An expiry appears only when an exec
/// plugin returned a client certificate, where the identity is fixed in the TLS
/// connector and nothing can refresh it in place.
pub fn credential_is_fresh(expires_at: Option<i64>, now_secs: i64) -> bool {
    match expires_at {
        None => true,
        Some(deadline) => now_secs + CREDENTIAL_SKEW_SECS < deadline,
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A live client plus the facts about the connection worth reporting.
#[derive(Clone)]
pub struct Connection {
    pub client: Client,
    /// `None` for in-cluster, which has no contexts.
    pub context: Option<String>,
    pub cluster_url: String,
    /// The context's namespace, or `default`. Where a namespaced probe starts.
    pub default_namespace: String,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Client holds credentials; naming it without printing it is deliberate.
        f.debug_struct("Connection")
            .field("context", &self.context)
            .field("cluster_url", &self.cluster_url)
            .field("default_namespace", &self.default_namespace)
            .finish_non_exhaustive()
    }
}

struct Cached {
    connection: Connection,
    /// Unix seconds, from [`Client::valid_until`].
    expires_at: Option<i64>,
}

/// Holds the merged kubeconfig and one client per context.
///
/// Multi-cluster is a phase away, but the cache is the shape it needs: switching
/// context must not re-run an exec plugin, and re-entering a context must not
/// either.
pub struct Connector {
    source: Source,
    /// The merged kubeconfig. `None` for [`Source::InCluster`].
    kubeconfig: Option<Kubeconfig>,
    clients: HashMap<String, Cached>,
}

impl Connector {
    /// Loads and merges whichever source the environment points at.
    ///
    /// Reads files; runs no exec plugin and opens no connection. Those happen in
    /// [`Connector::connect`], per context.
    pub fn load(env: &Env) -> Result<Connector, ConnectError> {
        let candidates = plan(env);
        if candidates.is_empty() {
            return Err(ConnectError::NoSource);
        }
        let mut failures = Vec::new();
        for source in candidates {
            match &source {
                Source::Kubeconfig(files) => match merge_files(files) {
                    Ok(cfg) => {
                        return Ok(Connector {
                            source,
                            kubeconfig: Some(cfg),
                            clients: HashMap::new(),
                        });
                    }
                    Err(why) => failures.push((source, why)),
                },
                Source::InCluster => {
                    return Ok(Connector {
                        source,
                        kubeconfig: None,
                        clients: HashMap::new(),
                    });
                }
            }
        }
        Err(ConnectError::NoUsableSource(failures))
    }

    /// Builds a connector over an already-parsed kubeconfig, for tests and for
    /// a future settings-supplied config that never touched a file.
    pub fn from_kubeconfig(kubeconfig: Kubeconfig) -> Connector {
        Connector {
            source: Source::Kubeconfig(Vec::new()),
            kubeconfig: Some(kubeconfig),
            clients: HashMap::new(),
        }
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn kubeconfig(&self) -> Option<&Kubeconfig> {
        self.kubeconfig.as_ref()
    }

    /// Every context available, empty for in-cluster.
    pub fn contexts(&self) -> Vec<String> {
        self.kubeconfig.as_ref().map(contexts).unwrap_or_default()
    }

    /// Resolves a request against this connector's kubeconfig without connecting.
    pub fn resolve(&self, requested: Option<&str>) -> Result<Option<String>, ConnectError> {
        match &self.kubeconfig {
            Some(cfg) => resolve_context(cfg, requested).map(Some),
            // In-cluster has exactly one identity, so naming a context is a
            // mistake worth reporting rather than silently ignoring.
            None => match requested {
                None => Ok(None),
                Some(name) => Err(ConnectError::UnknownContext {
                    requested: name.to_string(),
                    available: Vec::new(),
                }),
            },
        }
    }

    /// A client for `context`, cached per context and rebuilt when its
    /// credential is close to expiry.
    pub async fn connect(&mut self, context: Option<&str>) -> Result<Connection, ConnectError> {
        let resolved = self.resolve(context)?;
        let key = resolved.clone().unwrap_or_default();
        if let Some(cached) = self.clients.get(&key)
            && credential_is_fresh(cached.expires_at, now_secs())
        {
            return Ok(cached.connection.clone());
        }

        let config = self.config_for(resolved.as_deref()).await?;
        let cluster_url = config.cluster_url.to_string();
        let default_namespace = config.default_namespace.clone();
        let client = Client::try_from(config)
            .map_err(|e| ConnectError::Client(describe(&e as &dyn std::error::Error)))?;
        let expires_at = client.valid_until().as_ref().map(|t| t.as_second());
        let connection = Connection {
            client,
            context: resolved,
            cluster_url,
            default_namespace,
        };
        self.clients.insert(
            key,
            Cached {
                connection: connection.clone(),
                expires_at,
            },
        );
        Ok(connection)
    }

    async fn config_for(&self, context: Option<&str>) -> Result<Config, ConnectError> {
        match &self.kubeconfig {
            Some(cfg) => {
                let options = KubeConfigOptions {
                    context: context.map(str::to_string),
                    cluster: None,
                    user: None,
                };
                let mut config = Config::from_custom_kubeconfig(cfg.clone(), &options)
                    .await
                    .map_err(|e| ConnectError::Client(describe(&e as &dyn std::error::Error)))?;
                // Debug overrides are what `Config::infer` applies and are how a
                // developer points at `kubectl proxy`; skipping them here would
                // make our path quietly different from every other kube-rs app.
                config.apply_debug_overrides();
                Ok(config)
            }
            None => {
                let mut config = Config::incluster()
                    .map_err(|e| ConnectError::Client(describe(&e as &dyn std::error::Error)))?;
                config.apply_debug_overrides();
                Ok(config)
            }
        }
    }
}

/// Merges kubeconfig files first-wins, which is the rule `kubectl` documents.
///
/// `Kubeconfig::from_env` does the same thing, but from the process
/// environment; going through the explicit path list keeps the file names for
/// the error message and lets a test point at a fixture directory.
fn merge_files(files: &[PathBuf]) -> Result<Kubeconfig, String> {
    let mut merged = Kubeconfig::default();
    for path in files {
        let one = Kubeconfig::read_from(path).map_err(|e| {
            format!(
                "{}: {}",
                path.display(),
                describe(&e as &dyn std::error::Error)
            )
        })?;
        merged = merged.merge(one).map_err(|e| {
            format!(
                "{}: {}",
                path.display(),
                describe(&e as &dyn std::error::Error)
            )
        })?;
    }
    Ok(merged)
}

/// Flattens an error chain into one line.
///
/// kube-rs wraps causes several deep and the outermost message is often the
/// least useful one ("failed to load kubeconfig"), so the chain is what a user
/// needs to see. No credential material passes through here: these are config
/// parse and TLS setup errors, whose payloads are paths and reasons.
pub(crate) fn describe(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        let text = e.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = e.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(kubeconfig: Option<&str>, default: Option<&str>, in_cluster: bool) -> Env {
        Env {
            kubeconfig: kubeconfig.map(OsString::from),
            default_kubeconfig: default.map(PathBuf::from),
            in_cluster,
        }
    }

    #[test]
    fn kubeconfig_beats_the_default_path_which_beats_in_cluster() {
        // kubectl's precedence. Getting this backwards inside a pod that also
        // ships a kubeconfig points the app at the wrong cluster, which is the
        // worst possible class of bug for a tool that can eventually write.
        assert_eq!(
            plan(&env(Some("/a/config"), Some("/home/u/.kube/config"), true)),
            vec![
                Source::Kubeconfig(vec![PathBuf::from("/a/config")]),
                Source::InCluster
            ]
        );
        assert_eq!(
            plan(&env(None, Some("/home/u/.kube/config"), true)),
            vec![
                Source::Kubeconfig(vec![PathBuf::from("/home/u/.kube/config")]),
                Source::InCluster
            ]
        );
        assert_eq!(plan(&env(None, None, true)), vec![Source::InCluster]);
        assert!(plan(&env(None, None, false)).is_empty());
    }

    #[test]
    fn kubeconfig_splits_on_the_platform_separator_and_ignores_empties() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let joined = format!("/a{sep}{sep}/b");
        assert_eq!(
            plan(&env(Some(&joined), None, false)),
            vec![Source::Kubeconfig(vec![
                PathBuf::from("/a"),
                PathBuf::from("/b")
            ])]
        );
        // All-empty is the same as unset: fall through rather than trying to
        // read a file called "".
        let empties = format!("{sep}{sep}");
        assert_eq!(
            plan(&env(Some(&empties), Some("/d/config"), false)),
            vec![Source::Kubeconfig(vec![PathBuf::from("/d/config")])]
        );
    }

    const TWO_CONTEXTS: &str = "\
apiVersion: v1
kind: Config
current-context: prod
clusters:
- name: prod-cluster
  cluster:
    server: https://prod.example:6443
- name: dev-cluster
  cluster:
    server: https://dev.example:6443
users:
- name: prod-user
  user:
    token: not-a-real-token
- name: dev-user
  user:
    token: not-a-real-token
contexts:
- name: prod
  context:
    cluster: prod-cluster
    user: prod-user
    namespace: payments
- name: dev
  context:
    cluster: dev-cluster
    user: dev-user
";

    fn parse(yaml: &str) -> Kubeconfig {
        Kubeconfig::from_yaml(yaml).expect("fixture parses")
    }

    #[test]
    fn context_selection_prefers_the_request_then_current_context() {
        let cfg = parse(TWO_CONTEXTS);
        assert_eq!(resolve_context(&cfg, None).unwrap(), "prod");
        assert_eq!(resolve_context(&cfg, Some("dev")).unwrap(), "dev");
        assert_eq!(contexts(&cfg), vec!["prod", "dev"]);
    }

    #[test]
    fn an_unknown_context_lists_the_real_ones() {
        // The failure mode this replaces: a typo becoming a connection timeout
        // against a cluster URL nobody asked for.
        let cfg = parse(TWO_CONTEXTS);
        let err = resolve_context(&cfg, Some("prd")).unwrap_err();
        match err {
            ConnectError::UnknownContext {
                requested,
                available,
            } => {
                assert_eq!(requested, "prd");
                assert_eq!(available, vec!["prod", "dev"]);
            }
            other => panic!("expected UnknownContext, got {other:?}"),
        }
        assert!(format!("{}", resolve_context(&cfg, Some("prd")).unwrap_err()).contains("dev"));
    }

    #[test]
    fn a_current_context_naming_nothing_is_an_error_not_a_default() {
        // Silently falling back to the first context would connect to a cluster
        // the user did not name.
        let cfg = parse(
            "apiVersion: v1\nkind: Config\ncurrent-context: gone\ncontexts:\n- name: here\n  context:\n    cluster: c\n    user: u\n",
        );
        assert!(matches!(
            resolve_context(&cfg, None),
            Err(ConnectError::NoCurrentContext { .. })
        ));
        let empty = parse("apiVersion: v1\nkind: Config\n");
        assert!(matches!(
            resolve_context(&empty, None),
            Err(ConnectError::NoCurrentContext { .. })
        ));
    }

    #[test]
    fn merging_is_first_wins_across_files() {
        // The rule that decides which cluster you talk to when two files
        // disagree, and the one people are most often surprised by.
        let first = parse(TWO_CONTEXTS);
        let second = parse(
            "apiVersion: v1\nkind: Config\ncurrent-context: dev\nclusters:\n- name: prod-cluster\n  cluster:\n    server: https://impostor.example:6443\ncontexts:\n- name: staging\n  context:\n    cluster: prod-cluster\n    user: prod-user\n",
        );
        let merged = first.merge(second).expect("merge");
        assert_eq!(
            merged.current_context.as_deref(),
            Some("prod"),
            "the first file's current-context must survive"
        );
        let prod = merged
            .clusters
            .iter()
            .find(|c| c.name == "prod-cluster")
            .and_then(|c| c.cluster.as_ref())
            .and_then(|c| c.server.clone())
            .expect("prod cluster");
        assert_eq!(prod, "https://prod.example:6443", "first file wins");
        // New keys from later files are still added.
        assert!(contexts(&merged).contains(&"staging".to_string()));
    }

    #[test]
    fn in_cluster_has_no_contexts_and_rejects_a_named_one() {
        let c = Connector {
            source: Source::InCluster,
            kubeconfig: None,
            clients: HashMap::new(),
        };
        assert!(c.contexts().is_empty());
        assert_eq!(c.resolve(None).unwrap(), None);
        assert!(matches!(
            c.resolve(Some("prod")),
            Err(ConnectError::UnknownContext { .. })
        ));
    }

    #[test]
    fn a_connector_over_a_parsed_kubeconfig_resolves_without_io() {
        let c = Connector::from_kubeconfig(parse(TWO_CONTEXTS));
        assert_eq!(c.resolve(None).unwrap().as_deref(), Some("prod"));
        assert_eq!(c.resolve(Some("dev")).unwrap().as_deref(), Some("dev"));
        assert!(c.kubeconfig().is_some());
    }

    #[test]
    fn a_credential_is_retired_before_it_expires_not_after() {
        // A watch handed a client with seconds left dies mid-stream, and the
        // resulting desync looks like a cluster problem.
        assert!(credential_is_fresh(None, 1_000_000));
        assert!(credential_is_fresh(Some(1_000_000), 900_000));
        assert!(
            !credential_is_fresh(Some(1_000_000), 999_990),
            "10s of validity must not be handed out"
        );
        assert!(!credential_is_fresh(Some(1_000_000), 1_000_001));
        // Exactly at the skew boundary counts as stale.
        assert!(!credential_is_fresh(
            Some(1_000_000),
            1_000_000 - CREDENTIAL_SKEW_SECS
        ));
    }

    #[test]
    fn error_chains_flatten_into_one_readable_line() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "no such file")
            }
        }
        impl std::error::Error for Inner {}
        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "cannot read kubeconfig")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        assert_eq!(
            describe(&Outer(Inner) as &dyn std::error::Error),
            "cannot read kubeconfig: no such file"
        );
    }

    #[test]
    fn no_source_says_what_to_do_about_it() {
        let text = ConnectError::NoSource.to_string();
        assert!(text.contains("KUBECONFIG"));
        assert!(text.contains("~/.kube/config"));
    }

    #[test]
    fn source_displays_the_files_it_would_merge() {
        let s = Source::Kubeconfig(vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert!(s.to_string().contains("/a"));
        assert!(s.to_string().contains("/b"));
        assert!(Source::InCluster.to_string().contains("service account"));
    }
}
