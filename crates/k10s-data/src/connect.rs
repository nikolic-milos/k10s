use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hyper_timeout::TimeoutConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use kube::client::AuthError;
use kube::client::oidc_errors::{Error as OidcError, IdTokenError, RefreshError};
use kube::client::retry::RetryPolicy;
use kube::client::{Body, ClientBuilder, ConfigExt, DynBody};
use kube::config::{KubeConfigOptions, Kubeconfig, KubeconfigError};
use kube::{Client, Config};
use tower::{BoxError, ServiceBuilder, ServiceExt as _};
use tower_http::ServiceExt as _;
use tower_http::decompression::DecompressionLayer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Kubeconfig(Vec<PathBuf>),
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    pub kubeconfig: Option<OsString>,
    pub default_kubeconfig: Option<PathBuf>,
    pub in_cluster: bool,
}

const IN_CLUSTER_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

impl Env {
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

pub fn plan(env: &Env) -> Vec<Source> {
    let mut out = Vec::new();
    if let Some(raw) = &env.kubeconfig {
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

#[derive(Debug)]
pub enum ConnectError {
    NoSource,
    NoUsableSource(Vec<(Source, String)>),
    UnknownContext {
        requested: String,
        available: Vec<String>,
    },
    NoCurrentContext {
        available: Vec<String>,
    },
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

pub fn contexts(cfg: &Kubeconfig) -> Vec<String> {
    cfg.contexts.iter().map(|c| c.name.clone()).collect()
}

/// One context, reduced to the fields that may be shown to somebody.
///
/// A kubeconfig is a credential store: `token`, `password`,
/// `client-certificate-data` and `client-key-data` are the credential itself
/// rather than a path to one, and an exec plugin's argument vector routinely
/// carries an account, a project or a cluster ARN. This struct is where that is
/// decided rather than remembered downstream -- the only fields it has are the
/// name a person picks by, the server they would be talking to, and the
/// namespace the context defaults to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextInfo {
    pub name: String,
    /// This kubeconfig's own `current-context`.
    pub current: bool,
    pub server: Option<String>,
    pub namespace: Option<String>,
}

/// One place a cluster could come from, and what it offers.
///
/// [`Connector::load`] returns the *first* source that works, which is the right
/// answer for connecting and the wrong one for choosing: somebody with a
/// `KUBECONFIG` and an in-cluster account needs to see both. So this lists every
/// candidate, and a source that will not read keeps its place with the reason
/// attached rather than disappearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub source: Source,
    pub contexts: Vec<ContextInfo>,
    /// Why this source produced nothing, when that is a failure rather than an
    /// empty file. Already redaction-filtered.
    pub failure: Option<String>,
}

/// Every source this process can see, in the order [`plan`] prefers them.
pub fn list(env: &Env) -> Vec<Listing> {
    plan(env)
        .into_iter()
        .map(|source| match &source {
            Source::Kubeconfig(files) => match merge_files(files) {
                Ok(cfg) => Listing {
                    source,
                    contexts: context_info(&cfg),
                    failure: None,
                },
                Err(why) => Listing {
                    source,
                    contexts: Vec::new(),
                    failure: Some(why),
                },
            },
            // A service account declares no contexts and needs none: it is
            // connectable with nothing named, which is what makes it a row
            // rather than an empty heading.
            Source::InCluster => Listing {
                source,
                contexts: Vec::new(),
                failure: None,
            },
        })
        .collect()
}

/// One named file, for a kubeconfig the environment does not point at.
pub fn list_file(path: &Path) -> Listing {
    let source = Source::Kubeconfig(vec![path.to_path_buf()]);
    match merge_files(std::slice::from_ref(&path.to_path_buf())) {
        Ok(cfg) => Listing {
            source,
            contexts: context_info(&cfg),
            failure: None,
        },
        Err(why) => Listing {
            source,
            contexts: Vec::new(),
            failure: Some(why),
        },
    }
}

fn context_info(cfg: &Kubeconfig) -> Vec<ContextInfo> {
    let server = |cluster: &str| {
        cfg.clusters
            .iter()
            .find(|named| named.name == cluster)
            .and_then(|named| named.cluster.as_ref())
            .and_then(|cluster| cluster.server.clone())
    };
    cfg.contexts
        .iter()
        .map(|named| ContextInfo {
            current: cfg.current_context.as_deref() == Some(named.name.as_str()),
            name: named.name.clone(),
            server: named
                .context
                .as_ref()
                .and_then(|context| server(&context.cluster)),
            namespace: named
                .context
                .as_ref()
                .and_then(|context| context.namespace.clone()),
        })
        .collect()
}

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

const CREDENTIAL_SKEW_SECS: i64 = 30;

/// The kube client, assembled here rather than by `Client::try_from`.
///
/// kube 4.2 builds its hyper client behind a private function, and the
/// connector it builds speaks HTTP/1.1 with TCP_NODELAY left off, hyper-util's
/// default. Measured on the labelled host against a local k3s on 2026-09-04:
/// the RBAC probe's 24 concurrent POSTs cost a flat 44 ms because Nagle held
/// each request body until the previous segment was acknowledged and Linux
/// delayed that acknowledgement by 40 ms; with the option on, 7 to 11 ms, and
/// three of eight cold starts finished the whole data plane in 19 ms. So the
/// same public layers kube itself stacks are stacked here in the same order,
/// with the one connector line kube does not offer. Two configs keep kube's
/// own path. A proxied one, because its tunnelling is not worth reproducing
/// for a rare case and a proxy hop makes the loopback stall this fixes
/// irrelevant anyway. And one that runs an exec plugin, because the expiry of
/// the identity that plugin hands back is computed behind a method kube does
/// not export, and `Connector` reconnects on exactly that expiry; a client
/// that never reported one would sit on a dead certificate.
fn client_from(config: Config) -> Result<Client, kube::Error> {
    if config.proxy_url.is_some() || config.auth_info.exec.is_some() {
        return Client::try_from(config);
    }
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    let https = config.rustls_https_connector_with_connector(http)?;
    let mut connector = TimeoutConnector::new(https);
    connector.set_connect_timeout(config.connect_timeout);
    connector.set_read_timeout(config.read_timeout);
    connector.set_write_timeout(config.write_timeout);
    let transport: hyper_util::client::legacy::Client<_, Body> =
        hyper_util::client::legacy::Builder::new(TokioExecutor::new()).build(connector);

    let stack = ServiceBuilder::new()
        .layer(config.base_uri_layer())
        .layer(
            DecompressionLayer::new()
                .no_br()
                .no_deflate()
                .no_zstd()
                .gzip(!config.disable_compression),
        )
        .into_inner();
    let service = ServiceBuilder::new()
        .layer(stack)
        .option_layer(
            config
                .default_retry
                .then_some(tower::retry::RetryLayer::new(RetryPolicy::server_retry())),
        )
        .option_layer(config.auth_layer()?)
        .layer(config.extra_headers_layer()?)
        .map_err(BoxError::from)
        .service(transport);
    Ok(ClientBuilder::new(
        service
            .map_response_body(|body| {
                Box::new(http_body_util::BodyExt::map_err(body, BoxError::from)) as Box<DynBody>
            })
            .boxed(),
        config.default_namespace,
    )
    .build())
}

pub fn credential_is_fresh(expires_at: Option<i64>, now_secs: Option<i64>) -> bool {
    match (expires_at, now_secs) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(deadline), Some(now)) => now + CREDENTIAL_SKEW_SECS < deadline,
    }
}

fn now_secs() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

#[derive(Clone)]
pub struct Connection {
    pub client: Client,
    pub context: Option<String>,
    pub cluster_url: String,
    pub default_namespace: String,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("context", &self.context)
            .field("cluster_url", &self.cluster_url)
            .field("default_namespace", &self.default_namespace)
            .finish_non_exhaustive()
    }
}

struct Cached {
    connection: Connection,
    expires_at: Option<i64>,
}

pub struct Connector {
    source: Source,
    kubeconfig: Option<Kubeconfig>,
    clients: HashMap<String, Cached>,
}

impl Connector {
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

    pub fn from_kubeconfig(kubeconfig: Kubeconfig) -> Connector {
        Connector {
            source: Source::Kubeconfig(Vec::new()),
            kubeconfig: Some(kubeconfig),
            clients: HashMap::new(),
        }
    }

    /// One named file, ignoring `KUBECONFIG` and the default path. What the
    /// launch screen connects through when somebody opened a kubeconfig this
    /// process would never have found on its own.
    pub fn from_file(path: &Path) -> Result<Connector, ConnectError> {
        let files = vec![path.to_path_buf()];
        match merge_files(&files) {
            Ok(cfg) => Ok(Connector {
                source: Source::Kubeconfig(files),
                kubeconfig: Some(cfg),
                clients: HashMap::new(),
            }),
            Err(why) => Err(ConnectError::NoUsableSource(vec![(
                Source::Kubeconfig(files),
                why,
            )])),
        }
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn kubeconfig(&self) -> Option<&Kubeconfig> {
        self.kubeconfig.as_ref()
    }

    pub fn contexts(&self) -> Vec<String> {
        self.kubeconfig.as_ref().map(contexts).unwrap_or_default()
    }

    pub fn resolve(&self, requested: Option<&str>) -> Result<Option<String>, ConnectError> {
        match &self.kubeconfig {
            Some(cfg) => resolve_context(cfg, requested).map(Some),
            None => match requested {
                None => Ok(None),
                Some(name) => Err(ConnectError::UnknownContext {
                    requested: name.to_string(),
                    available: Vec::new(),
                }),
            },
        }
    }

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
        let client = client_from(config)
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

fn describe_auth(err: &AuthError) -> String {
    match err {
        AuthError::AuthExecRun { cmd, status, .. } => match exec_program(cmd) {
            Some(program) => format!("exec credential plugin {program:?} failed with {status}"),
            None => format!("exec credential plugin failed with {status}"),
        },
        AuthError::AuthExecParse(_) => {
            "the exec credential plugin printed no usable ExecCredential".to_string()
        }
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

fn describe_oidc(err: &OidcError) -> String {
    match err {
        OidcError::IdTokenMissing | OidcError::RefreshInit(_) => err.to_string(),
        OidcError::Refresh(refresh) => match refresh {
            RefreshError::InvalidURI(_)
            | RefreshError::HyperError(_)
            | RefreshError::HyperUtilError(_)
            | RefreshError::InvalidMetadata(_)
            | RefreshError::RequestFailed(_)
            | RefreshError::HttpError(_)
            | RefreshError::AuthorizationFailure
            | RefreshError::NoIdTokenReceived => err.to_string(),
            _ => "the refresh failed; the provider's answer is withheld because it \
                  can quote the new token"
                .to_string(),
        },
        OidcError::IdToken(
            IdTokenError::InvalidFormat | IdTokenError::InvalidExpirationTimestamp(_),
        ) => err.to_string(),
        _ => "the reason is withheld because it can quote the credential".to_string(),
    }
}

fn describe_kubeconfig(err: &KubeconfigError) -> String {
    match err {
        KubeconfigError::Parse(e) => match yaml_location(&e.to_string()) {
            Some(at) => format!("failed to parse kubeconfig YAML at {at}"),
            None => "failed to parse kubeconfig YAML".to_string(),
        },
        other => other.to_string(),
    }
}

fn exec_program(rendered: &str) -> Option<&str> {
    let b = rendered.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' => i += 1,
            b'"' => return rendered.get(quoted(b, i)?)?.rsplit(['/', '\\']).next(),
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

fn yaml_location(rendered: &str) -> Option<String> {
    let (line, rest) = digits(rendered.split_once("line ")?.1)?;
    let (column, _) = digits(
        rest.trim_start_matches(',')
            .trim_start()
            .strip_prefix("column ")?,
    )?;
    Some(format!("line {line}, column {column}"))
}

fn digits(s: &str) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    (end > 0).then(|| s.split_at(end))
}

const MAX_REASON_CHARS: usize = 400;

fn cap(text: String) -> String {
    match text.char_indices().nth(MAX_REASON_CHARS) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text,
    }
}

#[cfg(test)]
#[path = "connect_test.rs"]
mod tests;
