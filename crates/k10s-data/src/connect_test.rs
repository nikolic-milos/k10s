//! How a connection is resolved -- precedence across `KUBECONFIG`, the default
//! path and in-cluster, context selection, and first-wins merging -- and what an
//! error is allowed to say. Half of these tests exist to prove a failure
//! carries no credential: an auth variant this module has not read is withheld
//! whole rather than quoted.

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
        cache: HashMap::new(),
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
fn a_listing_carries_the_name_the_server_and_the_namespace_and_nothing_else() {
    let cfg = parse(TWO_CONTEXTS);
    let listed = context_info(&cfg);
    assert_eq!(
        listed,
        vec![
            ContextInfo {
                name: "prod".to_string(),
                current: true,
                server: Some("https://prod.example:6443".to_string()),
                namespace: Some("payments".to_string()),
            },
            ContextInfo {
                name: "dev".to_string(),
                current: false,
                server: Some("https://dev.example:6443".to_string()),
                namespace: None,
            },
        ]
    );

    // The fixture's users carry tokens. What a screen is handed must not.
    let rendered = format!("{listed:?}");
    assert!(
        !rendered.contains("not-a-real-token"),
        "a credential reached the listing: {rendered}"
    );
}

#[test]
fn a_context_naming_a_cluster_the_file_does_not_declare_still_lists() {
    let cfg = parse(
        "apiVersion: v1\nkind: Config\ncontexts:\n- name: orphan\n  context:\n    cluster: gone\n    user: u\n",
    );
    assert_eq!(
        context_info(&cfg),
        vec![ContextInfo {
            name: "orphan".to_string(),
            current: false,
            server: None,
            namespace: None,
        }]
    );
}

#[test]
fn listing_keeps_every_source_and_a_broken_one_keeps_its_place_with_the_reason() {
    let dir = std::env::temp_dir().join(format!("k10s-listing-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let good = dir.join("good.yaml");
    let bad = dir.join("bad.yaml");
    std::fs::write(&good, TWO_CONTEXTS).expect("write");
    std::fs::write(&bad, "apiVersion: v1\nkind: Config\nclusters: 3\n").expect("write");

    let listed = list(&Env {
        kubeconfig: Some(OsString::from(good.as_os_str())),
        default_kubeconfig: None,
        in_cluster: true,
    });
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(listed[0].contexts.len(), 2);
    assert_eq!(listed[0].failure, None);
    assert_eq!(
        listed[1].source,
        Source::InCluster,
        "an account with no contexts is still a source somebody can pick"
    );
    assert!(listed[1].contexts.is_empty());

    let broken = list_file(&bad);
    assert!(broken.contexts.is_empty());
    assert!(
        broken.failure.is_some_and(|why| why.contains("bad.yaml")),
        "a file that will not parse says which file it was"
    );

    let missing = list_file(&dir.join("nowhere.yaml"));
    assert!(missing.failure.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_credential_is_retired_before_it_expires_not_after() {
    assert!(credential_is_fresh(None, Some(1_000_000)));
    assert!(credential_is_fresh(Some(1_000_000), Some(900_000)));
    assert!(
        !credential_is_fresh(Some(1_000_000), Some(999_990)),
        "10s of validity must not be handed out"
    );
    assert!(!credential_is_fresh(Some(1_000_000), Some(1_000_001)));
    assert!(!credential_is_fresh(
        Some(1_000_000),
        Some(1_000_000 - CREDENTIAL_SKEW_SECS)
    ));
}

#[test]
fn a_clock_that_cannot_be_read_retires_every_credential() {
    assert!(
        !credential_is_fresh(Some(1_000_000), None),
        "a machine whose clock reads before the epoch must rebuild, not trust a cache"
    );
    assert!(
        !credential_is_fresh(None, None),
        "an undated credential is only fresh while there is a clock to say so"
    );
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
fn the_production_error_shape_reaches_the_auth_leaf_through_the_chain() {
    // kube-client 4.x never constructs kube::Error::Auth on the request
    // path: the auth layer is a tower middleware, so a failed credential
    // arrives as kube::Error::Service(BoxError) with the AuthError buried
    // behind however many layers the stack added. The redaction must be
    // reached through source(), not through the enum variant.
    #[derive(Debug)]
    struct MiddlewareWrap {
        source: AuthError,
    }
    impl std::fmt::Display for MiddlewareWrap {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "request dispatch failed: {}", self.source)
        }
    }
    impl std::error::Error for MiddlewareWrap {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    for err in [
        kube::Error::Service(Box::new(failed_exec_plugin())),
        kube::Error::Service(Box::new(MiddlewareWrap {
            source: failed_exec_plugin(),
        })),
    ] {
        let raw = &err as &dyn std::error::Error;
        assert!(
            raw.to_string().contains(SENTINEL)
                || raw
                    .source()
                    .is_some_and(|s| s.to_string().contains(SENTINEL)),
            "the raw chain must be the thing that leaks, or this proves nothing: {err}"
        );
        let said = describe(&err as &dyn std::error::Error);
        assert!(!said.contains(SENTINEL), "credential survived: {said}");
        assert!(
            !said.contains("KUBERNETES_EXEC_INFO"),
            "injected environment survived: {said}"
        );
        assert!(said.contains("gke-gcloud-auth-plugin"), "{said}");
    }
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
        serde_json::from_str::<u64>(&format!("\"{SENTINEL}\"")).expect_err("a string is not a u64")
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

// The two clients against a local listener that answers every request the
// same way and counts the requests it read. After `answers` replies it closes
// the connection, retries arrive on one kept-alive connection, and kube does
// not retry a transport failure, so no test waits out kube's backoff.
mod clients {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum Answer {
        // A Status carrying this code, whose message names the code.
        Status(u16),
        // The headers of a 200 with a body that never finishes.
        Stall,
        // Nothing at all.
        Silence,
        // These bytes to a connection's first request, and nothing to any
        // later one on it.
        FirstThenSilence(&'static str),
    }

    struct Listener {
        url: String,
        requests: Arc<AtomicUsize>,
        // Each request's Authorization value, in the order they were read.
        authorizations: Arc<std::sync::Mutex<Vec<String>>>,
    }

    async fn listen(answer: Answer, answers: usize) -> Listener {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let authorizations = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = authorizations.clone();
        tokio::spawn(async move {
            while counted.load(Ordering::SeqCst) < answers {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                if counted.load(Ordering::SeqCst) >= answers {
                    return;
                }
                let counted = counted.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut earlier = 0;
                    while let Some(head) = read_request(&mut socket).await {
                        if let Some(value) = head
                            .lines()
                            .find_map(|line| line.strip_prefix("authorization:"))
                        {
                            seen.lock()
                                .expect("not poisoned")
                                .push(value.trim().to_string());
                        }
                        let read = counted.fetch_add(1, Ordering::SeqCst) + 1;
                        if read > answers {
                            counted.fetch_sub(1, Ordering::SeqCst);
                            return;
                        }
                        let reply = match answer {
                            Answer::Status(code) => {
                                let body = format!(
                                    r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","message":"the listener answered {code}","code":{code}}}"#
                                );
                                format!(
                                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                                    body.len()
                                )
                            }
                            Answer::Stall => "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 64\r\n\r\n{".to_string(),
                            Answer::Silence => String::new(),
                            Answer::FirstThenSilence(bytes) if earlier == 0 => bytes.to_string(),
                            Answer::FirstThenSilence(_) => String::new(),
                        };
                        earlier += 1;
                        if socket.write_all(reply.as_bytes()).await.is_err() || read >= answers {
                            return;
                        }
                        if reply.is_empty() || matches!(answer, Answer::Stall) {
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                        }
                    }
                });
            }
        });
        Listener {
            url,
            requests,
            authorizations,
        }
    }

    // One request, head and body, off the socket; its head, lowercased, or
    // None once the peer is gone.
    async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<String> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buffer.len() < head_end + length {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        }
        Some(head)
    }

    fn request(method: &str) -> http::Request<Vec<u8>> {
        let body = if method == "GET" {
            Vec::new()
        } else {
            br#"{"apiVersion":"policy/v1","kind":"Eviction"}"#.to_vec()
        };
        http::Request::builder()
            .method(method)
            .uri("/api/v1/namespaces/g2/pods/p/eviction")
            .header("content-type", "application/json")
            .body(body)
            .expect("a request")
    }

    fn plain(url: &str) -> Config {
        Config::new(url.parse().expect("a URL"))
    }

    // What one request comes to on a client, and how many requests the
    // listener read for it. Bounded, so a missing deadline or a stray retry
    // fails the test rather than hanging it.
    async fn attempts(client: &Client, method: &str, listener: &Listener) -> (String, usize) {
        let answer = match tokio::time::timeout(
            Duration::from_secs(10),
            client.request_text(request(method)),
        )
        .await
        {
            Err(_) => "still waiting after 10 s".to_string(),
            Ok(Ok(text)) => text,
            Ok(Err(error)) => describe(&error as &(dyn std::error::Error + 'static)),
        };
        (answer, listener.requests.load(Ordering::SeqCst))
    }

    // A confirmed write is sent once, whatever throttling or unavailability
    // the server answers, and the server's sentence comes back; the sync
    // client still retries, so the split is pinned both ways.
    async fn interactive_once_sync_retries(config_for: impl Fn(&str) -> Config) {
        for code in [429, 503, 504] {
            for method in ["GET", "POST", "PATCH"] {
                let listener = listen(Answer::Status(code), usize::MAX).await;
                let clients =
                    clients_with(config_for(&listener.url), INTERACTIVE_DEADLINE).expect("clients");
                let (answer, sent) = attempts(&clients.interactive, method, &listener).await;
                assert_eq!(sent, 1, "{method} {code} was sent {sent} times");
                assert!(
                    answer.contains(&format!("the listener answered {code}")),
                    "the server's sentence: {answer}"
                );

                let listener = listen(Answer::Status(code), 2).await;
                let clients =
                    clients_with(config_for(&listener.url), INTERACTIVE_DEADLINE).expect("clients");
                let (_, sent) = attempts(&clients.sync, method, &listener).await;
                assert_eq!(sent, 2, "the sync client retries {method} {code}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_confirmed_write_is_sent_once_and_the_sync_client_still_retries() {
        interactive_once_sync_retries(plain).await;
    }

    // Silence and a body that stops half way both end at the deadline, in
    // words a person reads rather than kube's "ServiceError".
    async fn interactive_answers_within_its_deadline(config_for: impl Fn(&str) -> Config) {
        for answer in [Answer::Silence, Answer::Stall] {
            let listener = listen(answer, usize::MAX).await;
            let clients = clients_with(config_for(&listener.url), Duration::from_millis(200))
                .expect("clients");
            let started = Instant::now();
            let (said, _) = attempts(&clients.interactive, "PATCH", &listener).await;
            assert_eq!(said, "no answer within 200 ms", "{answer:?}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{answer:?} took {:?}",
                started.elapsed()
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_interactive_client_answers_within_its_deadline_body_included() {
        interactive_answers_within_its_deadline(plain).await;
    }

    #[test]
    fn no_answer_names_its_deadline_in_seconds_or_milliseconds() {
        assert_eq!(
            crate::deadline::NoAnswer(INTERACTIVE_DEADLINE).to_string(),
            "no answer within 35 s"
        );
        assert_eq!(
            crate::deadline::NoAnswer(Duration::from_millis(200)).to_string(),
            "no answer within 200 ms"
        );
    }

    // kube builds exec-plugin configs itself and would put its retry inside
    // the base both clients share; the same split has to hold there. The
    // plugin is `/bin/sh -c`, so the test needs a Unix; the branch it proves
    // is the same code on every platform. Nothing written by a test is
    // executed: a script written a moment before could still be open in a
    // child another test thread forked meanwhile, and running it would then
    // fail with "Text file busy".
    #[cfg(unix)]
    fn exec_plugin(runs: &std::path::Path) -> impl Fn(&str) -> Config {
        exec_plugin_printing(runs, r#"{"token":"t"}"#)
    }

    // The plugin counts its runs in `runs`; `$n` in `status` is the number of
    // earlier runs.
    #[cfg(unix)]
    fn exec_plugin_printing(runs: &std::path::Path, status: &str) -> impl Fn(&str) -> Config {
        exec_plugin_then(runs, status, "")
    }

    // As above, and from its fourth run, the first after the three a kube
    // build makes (the refresh a request makes for an expired credential), it
    // runs `later` first.
    #[cfg(unix)]
    fn exec_plugin_then(
        runs: &std::path::Path,
        status: &str,
        later: &str,
    ) -> impl Fn(&str) -> Config {
        let runs = runs.display();
        let later = if later.is_empty() {
            String::new()
        } else {
            format!("if [ \"$n\" -ge 3 ]; then {later}; fi; ")
        };
        let body = format!(
            "n=$(cat '{runs}' 2>/dev/null | wc -l); echo run >> '{runs}'; {later}echo '{{\"apiVersion\":\"client.authentication.k8s.io/v1\",\"kind\":\"ExecCredential\",\"status\":{}}}'",
            status.replace("$n", "'$n'")
        );
        move |url| {
            let mut config = plain(url);
            config.auth_info.exec = Some(kube::config::ExecConfig {
                api_version: Some("client.authentication.k8s.io/v1".into()),
                command: Some("/bin/sh".to_string()),
                args: Some(vec!["-c".to_string(), body.clone()]),
                env: None,
                drop_env: None,
                interactive_mode: None,
                provide_cluster_info: false,
                cluster: None,
                install_hint: None,
                other: Default::default(),
            });
            config
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exec_plugin_config_splits_and_bounds_the_same_way() {
        let dir = std::env::temp_dir().join(format!("k10s-exec-split-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        interactive_once_sync_retries(exec_plugin(&dir.join("runs"))).await;
        interactive_answers_within_its_deadline(exec_plugin(&dir.join("runs"))).await;
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // The retry sits above the base the auth layer is part of, so every
    // attempt the sync client makes is authorised again: a credential that
    // has already expired is fetched again for each attempt, and each attempt
    // carries its own token rather than a copy of the first one.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retried_request_is_authorised_again() {
        let dir = std::env::temp_dir().join(format!("k10s-exec-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let listener = listen(Answer::Status(503), 2).await;
        let config = exec_plugin_printing(
            &dir.join("runs"),
            r#"{"token":"t$n","expirationTimestamp":"2000-01-01T00:00:00Z"}"#,
        )(&listener.url);
        let clients = clients_from(config).expect("clients");
        let (_, sent) = attempts(&clients.sync, "POST", &listener).await;
        std::fs::remove_dir_all(&dir).expect("cleanup");
        let tokens = listener
            .authorizations
            .lock()
            .expect("not poisoned")
            .clone();
        assert_eq!(sent, 2, "the first attempt and its retry");
        assert_eq!(tokens.len(), 2, "{tokens:?}");
        assert_ne!(tokens[0], tokens[1], "each attempt authorised on its own");
    }

    // One connect is one kube build, which runs the plugin three times
    // (builder.rs: the auth layer, the TLS identity, the expiry). Two builds
    // would be six. This passes on the parent too; it guards the cost.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exec_plugin_runs_as_often_as_one_kube_build_runs_it() {
        let dir = std::env::temp_dir().join(format!("k10s-exec-runs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let runs = dir.join("runs");
        let listener = listen(Answer::Status(503), usize::MAX).await;
        clients_from(exec_plugin(&runs)(&listener.url)).expect("clients");
        let count = std::fs::read_to_string(&runs)
            .unwrap_or_default()
            .lines()
            .count();
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(count, 3);
    }

    fn token_kubeconfig(url: &str) -> String {
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: lab\nclusters:\n- name: lab\n  cluster:\n    server: {url}\ncontexts:\n- name: lab\n  context:\n    cluster: lab\n    user: lab\nusers:\n- name: lab\n  user:\n    token: t\n"
        )
    }

    // The connection a Connector hands out carries both clients, built the
    // same way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_carries_the_interactive_and_the_sync_client() {
        let listener = listen(Answer::Status(503), 3).await;
        let mut connector = Connector::from_kubeconfig(parse(&token_kubeconfig(&listener.url)));
        let connection = connector.connect(None).await.expect("a connection");
        let (_, interactive) = attempts(&connection.clients.interactive, "POST", &listener).await;
        assert_eq!(interactive, 1, "the interactive client's one attempt");
        let (_, both) = attempts(&connection.clients.sync, "POST", &listener).await;
        assert_eq!(
            both - interactive,
            2,
            "the sync client's first attempt and its retry"
        );
    }

    // The deadline a person waits on is the production one, through the
    // clients a Connector hands out: a server that stops answering is waited
    // on for exactly INTERACTIVE_DEADLINE, on a paused clock. A paused clock
    // jumps to the next timer whenever the runtime idles, and a connect still
    // in progress has its own, so the connection is made and answered first
    // and the clock paused only for the request the server leaves unanswered.
    #[tokio::test]
    async fn a_connection_waits_the_production_deadline_and_no_longer() {
        let listener = listen(
            Answer::FirstThenSilence("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}"),
            usize::MAX,
        )
        .await;
        let mut connector = Connector::from_kubeconfig(parse(&token_kubeconfig(&listener.url)));
        let connection = connector.connect(None).await.expect("a connection");
        connection
            .clients
            .interactive
            .request_text(request("GET"))
            .await
            .expect("the first request is answered");
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let answer = tokio::time::timeout(
            Duration::from_secs(60),
            connection
                .clients
                .interactive
                .request_text(request("PATCH")),
        )
        .await;
        let said = match answer {
            Err(_) => "still waiting after 60 s".to_string(),
            Ok(Ok(text)) => text,
            Ok(Err(error)) => describe(&error as &(dyn std::error::Error + 'static)),
        };
        assert_eq!(said, "no answer within 35 s");
        let waited = started.elapsed();
        assert!(
            waited >= INTERACTIVE_DEADLINE
                && waited < INTERACTIVE_DEADLINE + Duration::from_secs(1),
            "waited {waited:?}"
        );
    }

    // Connecting is inside the deadline: a TLS handshake the server accepts
    // the connection for and never answers is cut off like a silent answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_interactive_deadline_covers_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let url = format!("https://{}", listener.local_addr().expect("an address"));
        let held = tokio::spawn(async move {
            let mut sockets = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                sockets.push(socket);
            }
        });
        let clients = clients_with(plain(&url), Duration::from_millis(200)).expect("clients");
        let started = Instant::now();
        let said = match tokio::time::timeout(
            Duration::from_secs(10),
            clients.interactive.request_text(request("PATCH")),
        )
        .await
        {
            Err(_) => "still waiting after 10 s".to_string(),
            Ok(Ok(text)) => text,
            Ok(Err(error)) => describe(&error as &(dyn std::error::Error + 'static)),
        };
        held.abort();
        assert_eq!(said, "no answer within 200 ms");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }

    // The sync client carries watches and log streams, so it is never
    // bounded: a body still open past the interactive deadline keeps flowing
    // on it, on both branches that build the clients.
    async fn sync_streams_past_the_interactive_deadline(config_for: impl Fn(&str) -> Config) {
        let listener = listen(Answer::Stall, usize::MAX).await;
        let clients =
            clients_with(config_for(&listener.url), Duration::from_millis(200)).expect("clients");
        let body = clients
            .sync
            .request_stream(request("GET"))
            .await
            .expect("the sync client is not bounded");
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut first = [0u8; 1];
        futures::AsyncReadExt::read_exact(&mut Box::pin(body), &mut first)
            .await
            .expect("the body still flows past the deadline");
        assert_eq!(&first, b"{");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sync_client_streams_past_the_interactive_deadline() {
        sync_streams_past_the_interactive_deadline(plain).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sync_client_streams_past_the_interactive_deadline_with_an_exec_plugin() {
        let dir = std::env::temp_dir().join(format!("k10s-exec-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        sync_streams_past_the_interactive_deadline(exec_plugin(&dir.join("runs"))).await;
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // A credential refresh runs inside the request, so it counts against the
    // interactive deadline: a plugin that takes longer than the deadline on
    // the request's own refresh ends it at the deadline, and nothing reaches
    // the server.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_credential_refresh_counts_against_the_deadline() {
        let dir = std::env::temp_dir().join(format!("k10s-exec-slow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let listener = listen(Answer::Status(200), usize::MAX).await;
        let config = exec_plugin_then(
            &dir.join("runs"),
            r#"{"token":"t","expirationTimestamp":"2000-01-01T00:00:00Z"}"#,
            "sleep 2",
        )(&listener.url);
        let clients = clients_with(config, Duration::from_millis(300)).expect("clients");
        let started = Instant::now();
        let error = clients
            .interactive
            .request_text(request("POST"))
            .await
            .expect_err("the refresh outlasts the deadline");
        let took = started.elapsed();
        assert_eq!(
            describe(&error as &(dyn std::error::Error + 'static)),
            "no answer within 300 ms"
        );
        assert!(took < Duration::from_secs(1), "took {took:?}");
        // Past the plugin's own two seconds, so its credential has arrived and
        // been thrown away rather than not yet arrived.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert!(
            listener
                .authorizations
                .lock()
                .expect("not poisoned")
                .is_empty(),
            "nothing was sent, then or after the refresh finished"
        );
    }
}
