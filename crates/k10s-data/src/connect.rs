use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kube::client::AuthError;
use kube::client::oidc_errors::{Error as OidcError, IdTokenError, RefreshError};
use kube::config::{KubeConfigOptions, Kubeconfig, KubeconfigError};
use kube::{Client, Config};

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
        assert!(credential_is_fresh(None, 1_000_000));
        assert!(credential_is_fresh(Some(1_000_000), 900_000));
        assert!(
            !credential_is_fresh(Some(1_000_000), 999_990),
            "10s of validity must not be handed out"
        );
        assert!(!credential_is_fresh(Some(1_000_000), 1_000_001));
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

    const SENTINEL: &str = "eyJhbGciOiJSUzI1NiJ9.sentinel-do-not-log";

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
            (
                r#"KUBERNETES_EXEC_INFO="{\"kind\":\"ExecCredential\"}" "aws" "eks""#,
                Some("aws"),
            ),
            (r#"MESSAGE="two words" "aws""#, Some("aws")),
            (r#"QUOTE="\"" "aws""#, Some("aws")),
            (r#"WINDIR="C:\\Users\\" "aws""#, Some("aws")),
            (r#"EMPTY="" "aws""#, Some("aws")),
            ("", None),
            (r#"TOKEN="unterminated"#, None),
            ("env -u ONLY", None),
        ] {
            assert_eq!(exec_program(rendered), want, "in {rendered}");
        }
    }

    #[test]
    fn the_legacy_gcp_command_line_gives_back_nothing_or_kubeconfig_text() {
        assert_eq!(
            exec_program("/usr/lib/google-cloud-sdk/bin/gcloud config config-helper --format=json"),
            None
        );
        assert_eq!(
            exec_program(r#"/bin/gcloud config "helper mode""#),
            Some("helper mode")
        );
    }

    #[test]
    fn a_kubeconfig_that_will_not_parse_is_located_not_quoted() {
        let yaml = format!(
            "apiVersion: v1\nkind: Config\nclusters: []\nusers:\n- name: u\n  user:\n    token: {SENTINEL}\n    exec: 3\n"
        );
        let err = Kubeconfig::from_yaml(&yaml).expect_err("`exec: 3` is not an ExecConfig");
        assert!(
            err.to_string().contains(SENTINEL),
            "kube's own message must be the thing that leaks, or this test proves nothing"
        );

        let said = describe(&err as &dyn std::error::Error);
        assert!(!said.contains(SENTINEL), "token survived: {said}");
        assert!(
            said.contains("line 8"),
            "the position is what is left: {said}"
        );
    }

    #[test]
    fn a_position_is_read_out_of_both_shapes_serde_saphyr_writes_one_in() {
        assert_eq!(
            yaml_location("line 7 column 11: unexpected event"),
            Some("line 7, column 11".to_string())
        );
        assert_eq!(
            yaml_location(r#"invalid type: string "tok" at line 7, column 11"#),
            Some("line 7, column 11".to_string())
        );
        assert_eq!(yaml_location(r#"invalid type: string "tok""#), None);
        assert_eq!(yaml_location("no anchor on line 7 of it"), None);
    }

    #[test]
    fn a_kubeconfig_error_this_module_quotes_still_names_the_file_under_it() {
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
        let said = describe(&AuthError::AuthExec(SENTINEL.to_string()) as &dyn std::error::Error);
        assert!(!said.contains(SENTINEL), "{said}");
        assert!(said.contains("withheld"), "{said}");
    }

    #[test]
    fn an_oidc_failure_that_is_only_a_mechanism_is_named_and_not_withheld() {
        let unconfigured = AuthError::Oidc(OidcError::RefreshInit(
            kube::client::oidc_errors::RefreshInitError::MissingField("refresh-token"),
        ));
        let said = describe(&unconfigured as &dyn std::error::Error);
        assert!(said.contains("refresh-token"), "{said}");

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

        let said = describe(&Verbose("é".repeat(10_000)) as &dyn std::error::Error);
        assert_eq!(said.chars().count(), MAX_REASON_CHARS + 3);
    }
}
