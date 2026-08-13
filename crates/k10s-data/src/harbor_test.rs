//! Harbor inventory through a bound tool: 404 is absence, https is a labelled
//! hole, and a scan joins to pods by digest without claiming a Cosign verify.

use super::*;
use crate::oci::{digest_index, parse_image_id};
use crate::reach::{Bound, ToolAuth, ToolKind, ToolReach, Transport, Unbound};
use crate::read::Fetched;
use k8s_openapi::api::core::v1::{ContainerStatus, Pod, PodStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::client::Body;
use std::task::{Context, Poll};
use tower::Service;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

fn projects_json() -> &'static str {
    r#"[{"name":"library","repo_count":1,"metadata":{"public":"true"}}]"#
}

fn repos_json() -> &'static str {
    r#"[{"name":"library/nginx","artifact_count":2,"pull_count":9}]"#
}

fn artifacts_json() -> String {
    format!(
        r#"[{{
          "digest": "{DIGEST}",
          "tags": [{{"name": "1.27"}}],
          "scan_overview": {{
            "application/vnd.security.vulnerability.report; version=1.1": {{
              "scan_status": "Success",
              "severity": "High",
              "summary": {{
                "total": 4,
                "fixable": 2,
                "summary": {{"Critical": 1, "High": 1, "Medium": 1, "Low": 1}}
              }}
            }}
          }}
        }}]"#
    )
}

#[test]
fn projects_and_repositories_parse_and_clip_to_inventory_fields() {
    let projects = parse_projects(projects_json().as_bytes()).expect("projects");
    assert_eq!(projects[0].name, "library");
    assert!(projects[0].public);
    let repos = parse_repositories("library", repos_json().as_bytes()).expect("repos");
    assert_eq!(repos[0].name, "nginx");
    assert_eq!(repos[0].artifact_count, 2);
}

#[test]
fn scan_overview_maps_harbor_severity_onto_the_core_scale() {
    let artifacts = parse_artifacts(artifacts_json().as_bytes()).expect("artifacts");
    let scan = artifacts[0].scan.as_ref().expect("scan");
    assert_eq!(scan.severity, "High");
    assert_eq!(scan.mapped, k10s_core::Severity::Err);
    assert_eq!(scan.total, 4);
    assert_eq!(scan.critical, 1);
}

#[test]
fn a_404_inventory_renders_as_absence_not_an_empty_catalog() {
    let lines = render(&Inventory {
        served: false,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.contains("not serving its API"), "{text}");
    assert!(!text.contains("no projects this account can see"), "{text}");
}

#[test]
fn findings_rank_by_severity_then_by_how_many_pods_run_the_digest() {
    let parsed = parse_image_id(&format!("docker-pullable://library/nginx@{DIGEST}"));
    assert_eq!(parsed.digest.as_deref(), Some(DIGEST));
    let pod = Pod {
        metadata: ObjectMeta {
            name: Some("web-0".into()),
            namespace: Some("prod".into()),
            ..ObjectMeta::default()
        },
        spec: None,
        status: Some(PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "nginx".into(),
                image: "library/nginx:1.27".into(),
                image_id: format!("docker-pullable://library/nginx@{DIGEST}"),
                ..ContainerStatus::default()
            }]),
            ..PodStatus::default()
        }),
    };
    let index = digest_index(&[pod]);
    let artifacts = parse_artifacts(artifacts_json().as_bytes()).expect("artifacts");
    let inventory = Inventory {
        served: true,
        projects: vec![Project {
            name: "library".into(),
            public: true,
            repo_count: 1,
            repositories: vec![Repository {
                name: "nginx".into(),
                artifact_count: 1,
                pull_count: 9,
                artifacts,
            }],
        }],
        truncated: false,
        unreadable: 0,
    };
    let findings = join_scans(&inventory, &index);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].pods, ["prod/web-0"]);
    let text = render_findings(&findings).join("\n");
    assert!(text.contains("High"), "{text}");
    assert!(text.contains("prod/web-0"), "{text}");
}

#[tokio::test]
async fn absent_harbor_is_served_false_so_the_section_stays_invisible() {
    match fetch(
        &unused_client(),
        &ToolReach::Absent {
            kind: ToolKind::Harbor,
        },
    )
    .await
    {
        Fetched::Ok(inventory) => assert!(!inventory.served),
        other => panic!("absent is served false, not {other:?}"),
    }
}

#[tokio::test]
async fn https_harbor_is_a_browser_hole_not_a_fetch() {
    let reach = ToolReach::Unbound(Unbound {
        kind: ToolKind::Harbor,
        found: None,
        why: "Harbor is named as an https URL; open it in the system browser, or name an http \
              URL or an in-cluster Service"
            .into(),
        browser_url: Some("https://harbor.example".into()),
    });
    match fetch(&unused_client(), &reach).await {
        Fetched::Failed { why, .. } => {
            assert!(why.contains("https://harbor.example"), "{why}");
        }
        other => panic!("https is a hole, not {other:?}"),
    }
}

async fn serve(routes: Vec<(String, u16, String)>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for _ in 0..16 {
            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await
            else {
                break;
            };
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("");
            let (status, body) = routes
                .iter()
                .filter(|(prefix, _, _)| path.starts_with(prefix.as_str()))
                .max_by_key(|(prefix, _, _)| prefix.len())
                .map(|(_, status, body)| (*status, body.clone()))
                .unwrap_or((404, String::new()));
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    port
}

fn bound_url(port: u16) -> ToolReach {
    ToolReach::Bound(Bound {
        kind: ToolKind::Harbor,
        found: None,
        transport: Transport::Url {
            base: format!("http://127.0.0.1:{port}"),
        },
        auth: ToolAuth::Anonymous,
    })
}

#[tokio::test]
async fn a_404_on_projects_means_harbor_is_not_served() {
    let port = serve(vec![("/api/v2.0/projects".into(), 404, String::new())]).await;
    match fetch(&unused_client(), &bound_url(port)).await {
        Fetched::Ok(inventory) => {
            assert!(!inventory.served, "{inventory:?}");
            let text = render(&inventory).join("\n");
            assert!(text.contains("not serving its API"), "{text}");
        }
        other => panic!("404 is served false, not {other:?}"),
    }
}

#[tokio::test]
async fn a_bound_http_harbor_lists_projects_repos_and_scan_overview() {
    let artifacts = artifacts_json();
    let port = serve(vec![
        ("/api/v2.0/projects".into(), 200, projects_json().into()),
        (
            "/api/v2.0/projects/library/repositories".into(),
            200,
            repos_json().into(),
        ),
        (
            "/api/v2.0/projects/library/repositories/nginx/artifacts".into(),
            200,
            artifacts,
        ),
    ])
    .await;
    match fetch(&unused_client(), &bound_url(port)).await {
        Fetched::Ok(inventory) => {
            assert!(inventory.served);
            assert_eq!(inventory.projects[0].name, "library");
            assert_eq!(inventory.projects[0].repositories[0].name, "nginx");
            let scan = inventory.projects[0].repositories[0].artifacts[0]
                .scan
                .as_ref()
                .expect("scan");
            assert_eq!(scan.severity, "High");
            let text = render(&inventory).join("\n");
            assert!(text.contains("1 project"), "{text}");
            assert!(text.contains("scan High"), "{text}");
        }
        other => panic!("bound http should fetch: {other:?}"),
    }
}

#[tokio::test]
async fn a_401_on_projects_is_denied_not_an_empty_catalog() {
    let port = serve(vec![("/api/v2.0/projects".into(), 401, "no".into())]).await;
    match fetch(&unused_client(), &bound_url(port)).await {
        Fetched::Denied { what } => assert_eq!(what, "harbor"),
        other => panic!("401 is Denied, not {other:?}"),
    }
}
