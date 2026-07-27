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

use kube::client::AuthError;
use kube::client::oidc_errors::{Error as OidcError, IdTokenError, RefreshError};
use kube::config::{KubeConfigOptions, Kubeconfig, KubeconfigError};
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

/// Flattens an error chain into one line, minus the parts of it that quote a
/// credential.
///
/// kube-rs wraps causes several deep and the outermost message is often the
/// least useful one ("failed to load kubeconfig"), so the chain is what a user
/// needs to see. Two of its leaves cannot be shown at all, and both of them are
/// the *ordinary* failure rather than a corner: an exec plugin that exits
/// non-zero (expired refresh token, no browser, offline) is reported with the
/// environment and argv it was given, and a hand-edited kubeconfig is reported
/// with a snippet of itself. §6.7 says secret values never reach crash output,
/// and this is the one place the whole crate could put them there.
///
/// Every wrapper above such a leaf interpolates its source with `{0}`, so the
/// outer message is no safer than the leaf it quotes — recognising a leaf
/// therefore replaces the whole line rather than one link of it. Little is lost
/// by that, because the context worth having (which file, which context) is
/// added by this module's own callers, not by kube.
///
/// The `'static` bound is what makes the downcasts legal. It is not a constraint
/// on callers, who all pass an error owned by a local.
pub(crate) fn describe(err: &(dyn std::error::Error + 'static)) -> String {
    let mut link = Some(err);
    while let Some(e) = link {
        if let Some(auth) = e.downcast_ref::<AuthError>() {
            return cap(describe_auth(auth));
        }
        if let Some(cfg) = e.downcast_ref::<KubeconfigError>() {
            return cap(describe_kubeconfig(cfg));
        }
        link = e.source();
    }
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
    cap(out)
}

/// What is safe to say about an auth failure.
///
/// Denies by default, unlike [`describe_kubeconfig`], because this enum's
/// payloads *are* the credential: a plugin's stdout, a parsed `ExecCredential`,
/// a provider's token response. A variant is quoted only once someone has read
/// it and found nothing in it but a path, an OS message or a fixed sentence, so
/// a variant kube adds later stays withheld until someone does. `Oidc` is the
/// one whose payload is worth taking apart rather than dropping whole, and
/// [`describe_oidc`] is where that is done.
fn describe_auth(err: &AuthError) -> String {
    match err {
        // Which plugin failed is the one fact here worth digging out, and the
        // only part of `cmd` that is not credential material.
        AuthError::AuthExecRun { cmd, status, .. } => match exec_program(cmd) {
            Some(program) => format!("exec credential plugin {program:?} failed with {status}"),
            None => format!("exec credential plugin failed with {status}"),
        },
        AuthError::AuthExecParse(_) => {
            "the exec credential plugin printed no usable ExecCredential".to_string()
        }
        // An id-token that expired and would not refresh is the ordinary auth
        // failure on a cluster behind an identity provider, not a corner, so
        // this is the one variant the deny-by-default arm cannot be allowed to
        // eat: withheld, it reads as "auth failed" and sends the user nowhere.
        AuthError::Oidc(oidc) => format!("OIDC: {}", describe_oidc(oidc)),
        AuthError::InvalidBasicAuth(_)
        | AuthError::InvalidBearerToken(_)
        | AuthError::UnrefreshableTokenResponse
        | AuthError::ExecPluginFailed
        | AuthError::MalformedTokenExpirationDate(_)
        | AuthError::AuthExecStart(_)
        | AuthError::AuthExecSerialize(_)
        | AuthError::ReadTokenFile(..)
        | AuthError::MissingCommand
        | AuthError::ExecMissingClusterInfo
        | AuthError::NoValidNativeRootCA(_) => err.to_string(),
        _ => "auth failed; the reason is withheld because it can quote the credential".to_string(),
    }
}

/// What is safe to say about an OIDC failure.
///
/// [`describe_auth`]'s allow list one level down, and for the same reason: kube
/// interpolates every link of this subtree into the one above it, so a single
/// leaf that quotes makes the whole line unshowable. Three of them do; the rest
/// are the mechanism — a kubeconfig field that is not there, a provider that
/// cannot be reached, a status code from the token endpoint — which for this
/// failure is the entire diagnosis.
fn describe_oidc(err: &OidcError) -> String {
    match err {
        // `missing field id-token`; and under `RefreshInit`, a `&'static str`
        // field name (which is how "the kubeconfig has no refresh token"
        // arrives), a fixed root-CA sentence, or an OpenSSL error stack.
        OidcError::IdTokenMissing | OidcError::RefreshInit(_) => err.to_string(),
        OidcError::Refresh(refresh) => match refresh {
            // Offline, no discovery document, a 401 from the token endpoint:
            // all mechanism, and quoted from `err` rather than from `refresh`
            // because "ID token expired and refreshing failed" is the half of
            // the sentence that tells the user what was being attempted.
            RefreshError::InvalidURI(_)
            | RefreshError::HyperError(_)
            | RefreshError::HyperUtilError(_)
            | RefreshError::InvalidMetadata(_)
            | RefreshError::RequestFailed(_)
            | RefreshError::HttpError(_)
            | RefreshError::AuthorizationFailure
            | RefreshError::NoIdTokenReceived => err.to_string(),
            // What is left is serde_json over the token response, which is
            // where the new id-token would have come from.
            _ => "the refresh failed; the provider's answer is withheld because it \
                  can quote the new token"
                .to_string(),
        },
        // A token that is not a JWT at all, or whose `exp` claim is out of
        // range: the first says nothing but that, and the second is a jiff
        // message over an integer, the same kind `MalformedTokenExpirationDate`
        // is already quoted for a level up.
        OidcError::IdToken(
            IdTokenError::InvalidFormat | IdTokenError::InvalidExpirationTimestamp(_),
        ) => err.to_string(),
        // What is left of `IdToken` is base64 and serde_json over the token's
        // own payload, and a variant kube adds later has not been read at all.
        _ => "the reason is withheld because it can quote the credential".to_string(),
    }
}

/// What is safe to say about a kubeconfig that would not load.
///
/// Quotes kube by default: every variant but one carries a path, a context name
/// or a fixed reason, which is exactly what a user needs. The exception is the
/// parse failure, whose payload is the file.
fn describe_kubeconfig(err: &KubeconfigError) -> String {
    match err {
        KubeconfigError::Parse(e) => match yaml_location(&e.to_string()) {
            Some(at) => format!("failed to parse kubeconfig YAML at {at}"),
            None => "failed to parse kubeconfig YAML".to_string(),
        },
        other => other.to_string(),
    }
}

/// The program name out of kube's rendering of a failed exec plugin.
///
/// The exec-credential path reports the plugin as `format!("{cmd:?}")` over the
/// `std::process::Command` it ran, and std's non-alternate `Debug` writes that as
/// `env -u DROPPED KEY="value" "program" "arg"` — so the values of every variable
/// kube set, including the `KUBERNETES_EXEC_INFO` it injects and whatever
/// `exec.env` holds, sit in front of the program, and the args sit behind it. The
/// program has to be walked to rather than trimmed to, because a value is itself
/// a quoted and escaped string: `KUBERNETES_EXEC_INFO` is JSON, so the second
/// quote in the rendering is already inside it.
///
/// The two other prefixes std can emit, `cd {cwd:?} && ` and an `[{program:?}] `
/// for an overridden argv[0], are not handled because `auth_exec` sets neither.
///
/// kube raises this same error from a second place, the legacy gcp
/// `auth-provider`'s `cmd-path`, where it renders `format!("{cmd} {params}")` over
/// two raw kubeconfig strings and quotes nothing. That shape is not handled: the
/// walk runs off the end and returns `None`, so the failure is reported without a
/// name. Reading its first word as the program is what handling it would take,
/// and that word is the program in this shape but the `env` of an `env -u KEY`
/// prefix in std's, with nothing in the bytes to tell them apart. `cmd-args` that
/// happen to contain a quoted token are the one case that comes back non-`None`,
/// with the wrong name — still only kubeconfig text, because in this shape the
/// credential is what the command prints and not what it is handed.
fn exec_program(rendered: &str) -> Option<&str> {
    let b = rendered.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' => i += 1,
            b'"' => return rendered.get(quoted(b, i)?)?.rsplit(['/', '\\']).next(),
            // `KEY="value"`, or a bare word from the `env -u KEY` prefix that
            // `exec.dropEnv` produces.
            _ => {
                let word = i + b[i..]
                    .iter()
                    .position(|c| *c == b' ')
                    .unwrap_or(b.len() - i);
                i = match b[i..word].iter().position(|c| *c == b'=') {
                    Some(eq) if b.get(i + eq + 1) == Some(&b'"') => quoted(b, i + eq + 1)?.end + 1,
                    _ => word,
                };
            }
        }
    }
    None
}

/// The span between the quotes of the `Debug`-rendered string opening at `at`.
fn quoted(b: &[u8], at: usize) -> Option<std::ops::Range<usize>> {
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(at + 1..i),
            _ => i += 1,
        }
    }
    None
}

/// The line and column of a serde-saphyr error, and nothing else from it.
///
/// Its `Display` renders a rustc-style snippet of the document it was parsing,
/// and for a kubeconfig that document holds `token:` and `client-key-data:`. The
/// message above the snippet is no safer, because a scalar that failed to
/// deserialize is quoted into it, so two integers are the whole of what can be
/// shown — and they are what sends the user to the right line anyway. Read back
/// out of the rendering, in both the shapes serde-saphyr writes it (`line 7
/// column 11` titling a snippet, `at line 7, column 11` without one), because
/// `serde_saphyr` is kube's dependency and not ours.
fn yaml_location(rendered: &str) -> Option<String> {
    let (line, rest) = digits(rendered.split_once("line ")?.1)?;
    let (column, _) = digits(
        rest.trim_start_matches(',')
            .trim_start()
            .strip_prefix("column ")?,
    )?;
    Some(format!("line {line}, column {column}"))
}

/// The leading run of digits, and what follows it.
fn digits(s: &str) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    (end > 0).then(|| s.split_at(end))
}

/// How much of an error text survives.
///
/// Not a redaction — a secret in the first few hundred characters lives through
/// it — but an error nobody here has read should not be able to put a whole
/// document in someone's scrollback.
const MAX_REASON_CHARS: usize = 400;

fn cap(text: String) -> String {
    match text.char_indices().nth(MAX_REASON_CHARS) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text,
    }
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

    /// Stands in for the bearer token an exec plugin hands back. Distinctive
    /// enough that finding it anywhere in a message is proof, not coincidence.
    const SENTINEL: &str = "eyJhbGciOiJSUzI1NiJ9.sentinel-do-not-log";

    /// The error kube-rs raises when an exec plugin exits non-zero, built the way
    /// `auth_exec` builds it: the `cmd` field is std's own rendering of the
    /// `Command`, not our guess at it, so the tests below also fail if std changes
    /// the shape [`exec_program`] reads. The status is the default one because
    /// `ExitStatus` has no portable constructor and nothing here reads it.
    fn failed_exec_plugin() -> AuthError {
        let mut cmd = std::process::Command::new("/opt/homebrew/bin/gke-gcloud-auth-plugin");
        cmd.args(["--credential", SENTINEL]);
        cmd.env(
            "KUBERNETES_EXEC_INFO",
            format!(r#"{{"kind":"ExecCredential","spec":{{"token":"{SENTINEL}"}}}}"#),
        );
        AuthError::AuthExecRun {
            cmd: format!("{cmd:?}"),
            status: std::process::ExitStatus::default(),
            out: std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: format!(r#"{{"status":{{"token":"{SENTINEL}"}}}}"#).into_bytes(),
                stderr: Vec::new(),
            },
        }
    }

    #[test]
    fn a_failed_exec_plugin_is_reported_without_its_credential() {
        // An expired refresh token is the ordinary way this fails, so the whole
        // message lands in the log of anyone who has ever been logged out.
        let err = kube::Error::Auth(failed_exec_plugin());
        assert!(
            err.to_string().contains(SENTINEL),
            "kube's own message must be the thing that leaks, or this test proves nothing"
        );

        let said = describe(&err as &dyn std::error::Error);
        assert!(!said.contains(SENTINEL), "credential survived: {said}");
        assert!(
            !said.contains("KUBERNETES_EXEC_INFO"),
            "injected environment survived: {said}"
        );
        // Withholding the reason is only tolerable if it still says which plugin,
        // and the basename is the part of the path that is not the user's home.
        assert!(said.contains("gke-gcloud-auth-plugin"), "{said}");
        assert!(!said.contains("/opt/homebrew"), "{said}");
    }

    #[test]
    fn the_plugin_name_is_found_behind_anything_std_puts_in_front_of_it() {
        for (rendered, want) in [
            (r#""kubelogin""#, Some("kubelogin")),
            (
                r#""/usr/local/bin/aws" "eks" "get-token" "--cluster-name" "prod""#,
                Some("aws"),
            ),
            (
                r#""C:\\Program Files\\bin\\aws.exe" "eks""#,
                Some("aws.exe"),
            ),
            (r#"AWS_PROFILE="prod" "aws" "eks""#, Some("aws")),
            (r#"env -u AWS_PROFILE "aws""#, Some("aws")),
            (r#"env -i PATH="/usr/bin" "aws""#, Some("aws")),
            // The normal case, not a corner: the injected value is JSON, so the
            // second and third quotes in the rendering are inside it.
            (
                r#"KUBERNETES_EXEC_INFO="{\"kind\":\"ExecCredential\"}" "aws" "eks""#,
                Some("aws"),
            ),
            // A value holding a space, a bare quote, a trailing backslash, or
            // nothing at all: each would end the value early if the escapes were
            // not honoured, and the token after it is credential material.
            (r#"MESSAGE="two words" "aws""#, Some("aws")),
            (r#"QUOTE="\"" "aws""#, Some("aws")),
            (r#"WINDIR="C:\\Users\\" "aws""#, Some("aws")),
            (r#"EMPTY="" "aws""#, Some("aws")),
            // Nothing recognisable: say nothing. An unterminated quote is the
            // shape that would otherwise hand back the value it opened.
            ("", None),
            (r#"TOKEN="unterminated"#, None),
            ("env -u ONLY", None),
        ] {
            assert_eq!(exec_program(rendered), want, "in {rendered}");
        }
    }

    #[test]
    fn the_legacy_gcp_command_line_gives_back_nothing_or_kubeconfig_text() {
        // kube's other `AuthExecRun` site renders `cmd-path` and the raw
        // `cmd-args` with a space between them and quotes neither, so nothing in
        // the bytes tells its program from the `env -u` prefix of the shape
        // above and the walk runs off the end of it.
        assert_eq!(
            exec_program("/usr/lib/google-cloud-sdk/bin/gcloud config config-helper --format=json"),
            None
        );
        // What does come back non-`None` is a quoted token anywhere in
        // `cmd-args`, and it is the wrong name. Pinned because that boundary is
        // what the doc claims: an arg out of `auth-provider.config` is not the
        // credential, which in this shape is what the command prints.
        assert_eq!(
            exec_program(r#"/bin/gcloud config "helper mode""#),
            Some("helper mode")
        );
    }

    #[test]
    fn a_kubeconfig_that_will_not_parse_is_located_not_quoted() {
        // serde-saphyr renders the neighbourhood of the failure, and in a
        // kubeconfig the neighbourhood of a user block is that user's token.
        let yaml = format!(
            "apiVersion: v1\nkind: Config\nclusters: []\nusers:\n- name: u\n  user:\n    token: {SENTINEL}\n    exec: 3\n"
        );
        let err = Kubeconfig::from_yaml(&yaml).expect_err("`exec: 3` is not an ExecConfig");
        assert!(
            err.to_string().contains(SENTINEL),
            "kube's own message must be the thing that leaks, or this test proves nothing"
        );

        // Unwrapped, which is how `merge_files` sees it: the downcast has to hit
        // on the first link as well as through a wrapper.
        let said = describe(&err as &dyn std::error::Error);
        assert!(!said.contains(SENTINEL), "token survived: {said}");
        assert!(
            said.contains("line 8"),
            "the position is what is left: {said}"
        );
    }

    #[test]
    fn a_position_is_read_out_of_both_shapes_serde_saphyr_writes_one_in() {
        // The title it puts above a snippet, and the suffix it uses when it has
        // no snippet to show.
        assert_eq!(
            yaml_location("line 7 column 11: unexpected event"),
            Some("line 7, column 11".to_string())
        );
        assert_eq!(
            yaml_location(r#"invalid type: string "tok" at line 7, column 11"#),
            Some("line 7, column 11".to_string())
        );
        // Half a position is not one, and the fallback for both is to say where
        // nothing: the only other thing in the message is the document.
        assert_eq!(yaml_location(r#"invalid type: string "tok""#), None);
        assert_eq!(yaml_location("no anchor on line 7 of it"), None);
    }

    #[test]
    fn a_kubeconfig_error_this_module_quotes_still_names_the_file_under_it() {
        // Every variant in the quoted arm interpolates its `#[source]` with
        // `{0}`, which is why one `to_string` is the whole chain and not its
        // first link. If kube ever stops doing that, this arm silently drops the
        // half of the message that says which file could not be read.
        let err = KubeconfigError::LoadClientKey(kube::config::LoadDataError::ReadFile(
            std::io::Error::from(std::io::ErrorKind::NotFound),
            PathBuf::from("/home/u/.kube/prod.key"),
        ));
        let said = describe(&err as &dyn std::error::Error);
        assert!(said.contains("failed to load client key"), "{said}");
        assert!(said.contains("prod.key"), "{said}");
    }

    #[test]
    fn an_auth_variant_this_module_has_not_read_is_withheld() {
        // `AuthExec` is kube's free-form string, built at a dozen sites, one of
        // which interpolates a command line. Deny-by-default is what stops a kube
        // upgrade from printing a payload nobody here has looked at.
        let said = describe(&AuthError::AuthExec(SENTINEL.to_string()) as &dyn std::error::Error);
        assert!(!said.contains(SENTINEL), "{said}");
        assert!(said.contains("withheld"), "{said}");
    }

    #[test]
    fn an_oidc_failure_that_is_only_a_mechanism_is_named_and_not_withheld() {
        // One from each subtree, and the first two are the OIDC support tickets
        // an enterprise cluster actually produces: nobody put a refresh token in
        // the kubeconfig, and the provider said no. Withholding these is what
        // leaves a user with "auth failed" and nowhere to go.
        let unconfigured = AuthError::Oidc(OidcError::RefreshInit(
            kube::client::oidc_errors::RefreshInitError::MissingField("refresh-token"),
        ));
        let said = describe(&unconfigured as &dyn std::error::Error);
        assert!(said.contains("refresh-token"), "{said}");

        // Wrapped this one, so the walk has to reach it through a link as well
        // as find it first, which is how a refresh failure on a live request
        // arrives.
        let refused = kube::Error::Auth(AuthError::Oidc(OidcError::Refresh(
            RefreshError::RequestFailed(http::StatusCode::UNAUTHORIZED),
        )));
        let said = describe(&refused as &dyn std::error::Error);
        assert!(said.contains("401"), "{said}");

        let not_a_jwt = AuthError::Oidc(OidcError::IdToken(IdTokenError::InvalidFormat));
        let said = describe(&not_a_jwt as &dyn std::error::Error);
        assert!(said.contains("not a valid JWT"), "{said}");
    }

    #[test]
    fn an_oidc_failure_that_would_quote_a_token_is_still_withheld() {
        // serde_json names the value it choked on, and for these two that value
        // is the id-token itself or the response carrying the next one.
        let quoting = || {
            serde_json::from_str::<u64>(&format!("\"{SENTINEL}\""))
                .expect_err("a string is not a u64")
        };
        for err in [
            AuthError::Oidc(OidcError::Refresh(RefreshError::InvalidTokenResponse(
                quoting(),
            ))),
            AuthError::Oidc(OidcError::IdToken(IdTokenError::InvalidJson(quoting()))),
        ] {
            assert!(
                err.to_string().contains(SENTINEL),
                "kube's own message must be the thing that leaks, or this test proves nothing"
            );
            let said = describe(&err as &dyn std::error::Error);
            assert!(!said.contains(SENTINEL), "token survived: {said}");
            assert!(said.contains("withheld"), "{said}");
        }
    }

    #[test]
    fn an_unread_error_cannot_put_a_document_in_the_scrollback() {
        #[derive(Debug)]
        struct Verbose(String);
        impl std::fmt::Display for Verbose {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for Verbose {}

        // Multibyte on purpose: the cap counts characters, and slicing on a byte
        // count would panic here rather than truncate.
        let said = describe(&Verbose("é".repeat(10_000)) as &dyn std::error::Error);
        assert_eq!(said.chars().count(), MAX_REASON_CHARS + 3);
    }
}
