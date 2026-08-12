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
