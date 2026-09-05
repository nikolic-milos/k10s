//! Parsing imageID, docker config, and OCI referrers: the digest map is the
//! join key, credentials never land on the config type, and a missing bind is
//! a why rather than an empty Cosign list.

use super::*;
use crate::reach::{Bound, Scratch, ToolAuth, ToolKind, ToolReach, Transport, Unbound};
use crate::read::Fetched;
use k8s_openapi::api::core::v1::{ContainerStatus, Pod, PodSpec, PodStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::client::Body;
use std::task::{Context, Poll};
use tower::Service;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn pod(name: &str, namespace: &str, image_id: &str, image: &str) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            image_pull_secrets: Some(vec![k8s_openapi::api::core::v1::LocalObjectReference {
                name: "registry-creds".into(),
            }]),
            ..PodSpec::default()
        }),
        status: Some(PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "app".into(),
                image: image.into(),
                image_id: image_id.into(),
                ..ContainerStatus::default()
            }]),
            ..PodStatus::default()
        }),
    }
}

#[test]
fn a_docker_pullable_image_id_yields_repository_and_digest() {
    let parsed = parse_image_id(&format!("docker-pullable://registry.k8s.io/pause@{DIGEST}"));
    assert_eq!(parsed.runtime.as_deref(), Some("docker-pullable"));
    assert_eq!(parsed.repository.as_deref(), Some("registry.k8s.io/pause"));
    assert_eq!(parsed.digest.as_deref(), Some(DIGEST));
}

#[test]
fn a_containerd_id_that_is_only_a_digest_still_maps() {
    let parsed = parse_image_id(&format!("containerd://{DIGEST}"));
    assert_eq!(parsed.runtime.as_deref(), Some("containerd"));
    assert_eq!(parsed.repository, None);
    assert_eq!(parsed.digest.as_deref(), Some(DIGEST));
}

#[test]
fn a_bare_name_at_digest_without_a_runtime_prefix_parses() {
    let parsed = parse_image_id(&format!("ghcr.io/acme/api:1.2@{DIGEST}"));
    assert_eq!(parsed.runtime, None);
    assert_eq!(parsed.repository.as_deref(), Some("ghcr.io/acme/api:1.2"));
    assert_eq!(parsed.digest.as_deref(), Some(DIGEST));
}

#[test]
fn an_empty_or_short_image_id_is_not_a_digest() {
    assert_eq!(parse_image_id("").digest, None);
    assert_eq!(parse_image_id("docker://sha256:dead").digest, None);
    assert_eq!(parse_image_id("nginx:1.27").digest, None);
}

#[test]
fn digest_index_groups_pods_and_counts_those_without_a_digest() {
    let pods = [
        pod(
            "api-0",
            "prod",
            &format!("docker-pullable://ghcr.io/acme/api@{DIGEST}"),
            "ghcr.io/acme/api:1",
        ),
        pod(
            "api-1",
            "prod",
            &format!("containerd://{DIGEST}"),
            "ghcr.io/acme/api:1",
        ),
        pod(
            "web",
            "prod",
            &format!("docker-pullable://ghcr.io/acme/web@{OTHER}"),
            "ghcr.io/acme/web:2",
        ),
        pod("pending", "prod", "", "nginx"),
    ];
    let index = digest_index(&pods);
    assert_eq!(index.without_digest, 1);
    assert_eq!(index.by_digest.get(DIGEST).map(Vec::len), Some(2));
    assert_eq!(index.by_digest.get(OTHER).map(Vec::len), Some(1));
    let names: Vec<&str> = index.by_digest[DIGEST]
        .iter()
        .map(|r| r.pod.as_str())
        .collect();
    assert_eq!(names, ["api-0", "api-1"]);
    let text = render_index(&index).join("\n");
    assert!(text.contains("2 digests, 3 running containers"), "{text}");
    assert!(!text.contains("pending"), "{text}");
}

#[test]
fn pull_secret_names_are_metadata_and_do_not_read_a_secret() {
    let names = pull_secret_names(&pod("api", "prod", "", "nginx"));
    assert_eq!(names, ["registry-creds"]);
}

#[test]
fn docker_config_auths_are_hosts_without_the_password() {
    let json = br#"{
      "auths": {
        "https://index.docker.io/v1/": {"auth": "dXNlcjpwYXNz"},
        "ghcr.io": {"username": "u", "password": "SUPERSECRET"}
      },
      "credHelpers": {"gcr.io": "gcloud"},
      "credsStore": "desktop"
    }"#;
    assert!(
        std::str::from_utf8(json).unwrap().contains("SUPERSECRET"),
        "the fixture has to contain what must not come out"
    );
    let cfg = parse_docker_config(json).expect("parses");
    assert_eq!(cfg.creds_store.as_deref(), Some("desktop"));
    let ghcr = cfg
        .registries
        .iter()
        .find(|r| r.host == "ghcr.io")
        .expect("ghcr");
    assert!(ghcr.has_auth);
    let hub = cfg
        .registries
        .iter()
        .find(|r| r.host == "index.docker.io")
        .expect("hub");
    assert!(hub.has_auth);
    let gcr = cfg
        .registries
        .iter()
        .find(|r| r.host == "gcr.io")
        .expect("gcr helper");
    assert_eq!(gcr.helper.as_deref(), Some("gcloud"));
    assert!(!gcr.has_auth);
    let rendered = format!("{cfg:?}");
    assert!(
        !rendered.contains("SUPERSECRET") && !rendered.contains("dXNlcjpwYXNz"),
        "the auth blob does not survive on the type: {rendered}"
    );
}

#[test]
fn a_legacy_dockercfg_object_without_an_auths_wrapper_still_reads() {
    let json = br#"{"https://ghcr.io":{"auth":"dXNlcjpwYXNz"}}"#;
    let cfg = parse_docker_config(json).expect("legacy");
    assert_eq!(cfg.registries[0].host, "ghcr.io");
    assert!(cfg.registries[0].has_auth);
}

#[test]
fn an_image_pull_secret_is_parsed_only_from_revealed_scratch() {
    let json = br#"{"auths":{"ghcr.io":{"auth":"dXNlcjpwYXNz"}}}"#;
    let scratch = Scratch::from_bytes(json.to_vec());
    let cfg = parse_docker_config_from_scratch(&scratch).expect("scratch");
    assert_eq!(cfg.registries[0].host, "ghcr.io");
    let revealed = reveal_registry_auth(&scratch, "ghcr.io").expect("auth");
    assert_eq!(revealed.as_str().expect("utf8"), "user:pass");
    assert!(
        reveal_registry_auth(&scratch, "quay.io").is_none(),
        "a registry that is not in the config is not guessed"
    );
}

#[test]
fn a_docker_config_file_is_parsed_from_bytes_the_caller_read() {
    let path = std::env::temp_dir().join(format!(
        "k10s-oci-docker-config-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::write(&path, br#"{"auths":{"ghcr.io":{"auth":"dXNlcjpwYXNz"}}}"#)
        .expect("write temp docker config");
    let bytes = std::fs::read(&path).expect("read");
    let _ = std::fs::remove_file(&path);
    let cfg = parse_docker_config(&bytes).expect("from caller bytes");
    assert_eq!(cfg.registries[0].host, "ghcr.io");
}

#[test]
fn an_oversize_docker_config_is_refused_not_truncated() {
    let huge = vec![b'x'; MAX_DOCKER_CONFIG_BYTES + 1];
    match parse_docker_config(&huge) {
        Err(DockerConfigError::TooLarge { bytes }) => {
            assert_eq!(bytes, MAX_DOCKER_CONFIG_BYTES + 1)
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn referrers_classify_cosign_and_sbom_without_a_verified_bit() {
    let json = format!(
        r#"{{
          "schemaVersion": 2,
          "mediaType": "application/vnd.oci.image.index.v1+json",
          "manifests": [
            {{
              "mediaType": "application/vnd.oci.image.manifest.v1+json",
              "digest": "{DIGEST}",
              "artifactType": "application/vnd.dev.cosign.artifact.sig.v1+json"
            }},
            {{
              "mediaType": "application/vnd.oci.image.manifest.v1+json",
              "digest": "{OTHER}",
              "artifactType": "application/spdx+json"
            }}
          ]
        }}"#
    );
    let items = parse_referrers(json.as_bytes()).expect("index");
    assert_eq!(items[0].kind, ReferrerKind::CosignSignature);
    assert_eq!(items[1].kind, ReferrerKind::Sbom);
    let rendered = render_referrers(&Referrers {
        repository: "library/nginx".into(),
        subject: DIGEST.into(),
        items,
        truncated: false,
    })
    .join("\n");
    assert!(rendered.contains("listed not verified"), "{rendered}");
    assert!(
        !rendered.contains("verified signature")
            && !rendered.to_ascii_lowercase().contains("valid"),
        "presence is not a verdict: {rendered}"
    );
}

#[test]
fn tags_and_manifest_summaries_parse_without_pulling_a_config_blob() {
    let tags = parse_tags(br#"{"name":"library/nginx","tags":["1.27","latest"]}"#).expect("tags");
    assert_eq!(tags.tags, ["1.27", "latest"]);
    let manifest = parse_manifest(
        br#"{
          "schemaVersion": 2,
          "mediaType": "application/vnd.oci.image.manifest.v1+json",
          "config": {"digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 1},
          "layers": [{}, {}]
        }"#,
    )
    .expect("manifest");
    assert_eq!(manifest.layers, 2);
    assert_eq!(manifest.config_digest, DIGEST);
}

#[derive(Clone)]
struct Unused;

impl Service<http::Request<Body>> for Unused {
    type Response = http::Response<Body>;
    type Error = tower::BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<Body>) -> Self::Future {
        Box::pin(async {
            Ok(http::Response::builder()
                .status(500)
                .body(Body::from(b"unused".to_vec()))
                .expect("response"))
        })
    }
}

fn unused_client() -> kube::Client {
    kube::Client::new(Unused, "default")
}

fn https_unbound() -> ToolReach {
    ToolReach::Unbound(Unbound {
        kind: ToolKind::Harbor,
        found: None,
        why: "Harbor is named as an https URL; open it in the system browser, or name an http \
              URL or an in-cluster Service"
            .into(),
        browser_url: Some("https://harbor.example".into()),
    })
}

#[tokio::test]
async fn https_referrers_are_a_failed_why_not_an_empty_unsigned_list() {
    match fetch_referrers(&unused_client(), &https_unbound(), "library/nginx", DIGEST).await {
        Fetched::Failed { why, .. } => {
            assert!(why.contains("not listed and not verified"), "{why}");
            assert!(why.contains("https://harbor.example"), "{why}");
        }
        other => panic!("https is a hole, not {other:?}"),
    }
}

#[tokio::test]
async fn a_missing_bind_does_not_claim_there_are_no_signatures() {
    match fetch_referrers(
        &unused_client(),
        &ToolReach::Absent {
            kind: ToolKind::Harbor,
        },
        "library/nginx",
        DIGEST,
    )
    .await
    {
        Fetched::Failed { why, .. } => {
            assert!(why.contains("cannot be fetched"), "{why}");
        }
        other => panic!("absent is not an empty referrer list: {other:?}"),
    }
}

#[tokio::test]
async fn plaintext_http_referrers_parse_and_do_not_claim_verification() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let body = format!(
        r#"{{"schemaVersion":2,"manifests":[{{"digest":"{DIGEST}","artifactType":"application/vnd.dev.cosign.artifact.sig.v1+json"}}]}}"#
    );
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    });

    let reach = ToolReach::Bound(Bound {
        kind: ToolKind::Harbor,
        found: None,
        transport: Transport::Url {
            base: format!("http://127.0.0.1:{port}"),
        },
        auth: ToolAuth::Anonymous,
    });
    match fetch_referrers(&unused_client(), &reach, "library/nginx", DIGEST).await {
        Fetched::Ok(referrers) => {
            assert_eq!(referrers.items.len(), 1);
            assert_eq!(referrers.items[0].kind, ReferrerKind::CosignSignature);
            let text = render_referrers(&referrers).join("\n");
            assert!(text.contains("not verified"), "{text}");
        }
        other => panic!("plaintext http should fetch: {other:?}"),
    }
}

#[tokio::test]
async fn a_404_on_referrers_says_the_api_is_missing_rather_than_unsigned() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 1024];
        let _ = stream.read(&mut buf).await;
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        let _ = stream.shutdown().await;
    });
    let reach = ToolReach::Bound(Bound {
        kind: ToolKind::Harbor,
        found: None,
        transport: Transport::Url {
            base: format!("http://127.0.0.1:{port}"),
        },
        auth: ToolAuth::Anonymous,
    });
    match fetch_referrers(&unused_client(), &reach, "library/nginx", DIGEST).await {
        Fetched::Failed { why, .. } => {
            assert!(why.contains("does not serve OCI referrers"), "{why}");
            assert!(why.contains("nothing was verified"), "{why}");
        }
        other => panic!("404 is not an empty list: {other:?}"),
    }
}
