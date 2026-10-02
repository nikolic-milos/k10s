use super::*;
use std::sync::{Arc, Mutex};

use kube::client::Body;

fn virtualservice_list_json() -> &'static str {
    r#"{
      "apiVersion": "networking.istio.io/v1",
      "kind": "VirtualServiceList",
      "items": [
        {
          "apiVersion": "networking.istio.io/v1",
          "kind": "VirtualService",
          "metadata": {"name": "reviews", "namespace": "prod"},
          "spec": {
            "hosts": ["reviews"],
            "gateways": ["prod/edge"],
            "http": [
              {
                "route": [
                  {"destination": {"host": "reviews"}}
                ]
              }
            ]
          }
        }
      ]
    }"#
}

fn serviceprofile_list_json() -> &'static str {
    r#"{
      "apiVersion": "linkerd.io/v1alpha2",
      "kind": "ServiceProfileList",
      "items": [
        {
          "apiVersion": "linkerd.io/v1alpha2",
          "kind": "ServiceProfile",
          "metadata": {"name": "web.prod.svc.cluster.local", "namespace": "prod"},
          "spec": {"routes": [{"name": "GET /api"}]}
        }
      ]
    }"#
}

fn config_dump_json() -> &'static str {
    r#"{
      "configs": [
        {
          "@type": "type.googleapis.com/envoy.admin.v3.ListenersConfigDump",
          "static_listeners": [{"listener": {"name": "virtualInbound"}}],
          "dynamic_listeners": [{"name": "0.0.0.0_80"}]
        },
        {
          "@type": "type.googleapis.com/envoy.admin.v3.ClustersConfigDump",
          "static_clusters": [{"cluster": {"name": "prometheus_stats"}}],
          "dynamic_active_clusters": [
            {"cluster": {"name": "outbound|80||reviews.default.svc.cluster.local"}},
            {"cluster": {"name": "inbound|80||"}}
          ]
        }
      ]
    }"#
}

fn istio_group_json() -> &'static str {
    r#"{
      "kind": "APIGroup",
      "apiVersion": "v1",
      "name": "networking.istio.io",
      "versions": [{"groupVersion": "networking.istio.io/v1", "version": "v1"}],
      "preferredVersion": {"groupVersion": "networking.istio.io/v1", "version": "v1"}
    }"#
}

fn status_404() -> &'static str {
    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"the server could not find the requested resource"}"#
}

fn empty_list() -> &'static str {
    r#"{"kind":"List","apiVersion":"v1","metadata":{},"items":[]}"#
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

struct Script {
    routes: Vec<(String, u16, String)>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Script {
    fn new(routes: &[(&str, u16, &str)]) -> Script {
        Script {
            routes: routes
                .iter()
                .map(|(path, status, body)| (path.to_string(), *status, body.to_string()))
                .collect(),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn client(&self) -> kube::Client {
        let routes = self.routes.clone();
        let seen = self.seen.clone();
        kube::Client::new(
            tower::service_fn(move |req: http::Request<Body>| {
                let routes = routes.clone();
                let seen = seen.clone();
                async move {
                    let path = req.uri().path().to_string();
                    seen.lock().expect("seen").push(path.clone());
                    let hit = routes
                        .iter()
                        .filter(|(prefix, _, _)| {
                            path == *prefix || path.starts_with(&format!("{prefix}/"))
                        })
                        .max_by_key(|(prefix, _, _)| prefix.len());
                    let (status, body) = match hit {
                        Some((_, status, body)) => (*status, body.clone()),
                        None => (404, status_404().to_string()),
                    };
                    let response = http::Response::builder()
                        .status(status)
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.into_bytes()))
                        .expect("response");
                    Ok::<_, tower::BoxError>(response)
                }
            }),
            "default",
        )
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }
}

#[test]
fn a_404_on_the_istio_group_is_served_false() {
    assert_eq!(after_group(&api_error(404)), GroupState::Absent);
    assert!(!GroupState::Absent.is_served());
    assert!(matches!(after_group(&api_error(403)), GroupState::Denied));
    assert!(matches!(
        after_group(&api_error(500)),
        GroupState::Failed { .. }
    ));
}

#[test]
fn a_virtualservice_fixture_is_declared_reach_from_policy() {
    let objects = parse_list(
        MeshKind::VirtualService,
        virtualservice_list_json().as_bytes(),
    )
    .expect("json");
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].name, "reviews");
    assert_eq!(objects[0].destinations, ["reviews"]);
    let edges = declared_from(&objects);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].from, "prod/edge");
    assert_eq!(edges[0].to, "reviews");
    assert_eq!(edges[0].because.kind, "VirtualService");
    assert_eq!(edges[0].because.group, ISTIO_GROUP);
}

#[test]
fn a_linkerd_serviceprofile_fixture_is_declared_not_observed() {
    let objects = parse_list(
        MeshKind::ServiceProfile,
        serviceprofile_list_json().as_bytes(),
    )
    .expect("json");
    let edges = declared_from(&objects);
    assert_eq!(edges[0].to, "web.prod.svc.cluster.local");
    assert_eq!(edges[0].because.kind, "ServiceProfile");
    assert_eq!(edges[0].because.group, LINKERD_GROUP);
    assert!(
        observed_from_series(&[]).is_empty(),
        "a CR is policy; empty telemetry is not an observed edge"
    );
}

#[test]
fn destinationrule_gateway_and_sidecar_fixtures_declare_hosts() {
    let dr = parse_list(
        MeshKind::DestinationRule,
        br#"{"items":[{"metadata":{"name":"reviews","namespace":"prod"},"spec":{"host":"reviews"}}]}"#,
    )
    .unwrap();
    let gw = parse_list(
        MeshKind::Gateway,
        br#"{"items":[{"metadata":{"name":"edge","namespace":"istio-system"},"spec":{"servers":[{"hosts":["*.prod.example"]}]}}]}"#,
    )
    .unwrap();
    let sc = parse_list(
        MeshKind::Sidecar,
        br#"{"items":[{"metadata":{"name":"default","namespace":"prod"},"spec":{"egress":[{"hosts":["prod/reviews.prod.svc.cluster.local"]}]}}]}"#,
    )
    .unwrap();
    let edges = declared_from(&[dr[0].clone(), gw[0].clone(), sc[0].clone()]);
    assert!(
        edges
            .iter()
            .any(|e| e.because.kind == "DestinationRule" && e.to == "reviews")
    );
    assert!(
        edges
            .iter()
            .any(|e| e.because.kind == "Gateway" && e.to == "*.prod.example")
    );
    assert!(
        edges
            .iter()
            .any(|e| e.because.kind == "Sidecar" && e.to.contains("reviews"))
    );
}

#[test]
fn a_network_policy_edge_is_declared_reach() {
    let edge = declared_from_policy("frontend", "reviews", "prod", "allow-reviews");
    assert_eq!(edge.from, "frontend");
    assert_eq!(edge.to, "reviews");
    assert_eq!(edge.because.kind, "NetworkPolicy");
    assert_eq!(edge.because.group, "networking.k8s.io");
}

#[test]
fn hubble_series_labels_are_observed_reach_not_declared() {
    let series = [
        SeriesLabels {
            name: "hubble_flows_processed_total".into(),
            labels: vec![
                ("source".into(), "default/frontend".into()),
                ("destination".into(), "default/reviews".into()),
            ],
        },
        SeriesLabels {
            name: "istio_requests_total".into(),
            labels: vec![
                ("source_app".into(), "frontend".into()),
                (
                    "destination_service".into(),
                    "reviews.prod.svc.cluster.local".into(),
                ),
            ],
        },
    ];
    let edges = observed_from_series(&series);
    assert_eq!(edges.len(), 2);
    assert_eq!(edges[0].because.exporter, TelemetryExporter::Hubble);
    assert_eq!(edges[0].from, "default/frontend");
    assert_eq!(edges[0].to, "default/reviews");
    assert_eq!(edges[1].because.exporter, TelemetryExporter::Istio);
    assert_eq!(edges[0].because.metric, "hubble_flows_processed_total");
}

#[test]
fn linkerd_response_total_is_observed_reach_not_declared() {
    let series = [SeriesLabels {
        name: "response_total".into(),
        labels: vec![
            ("client".into(), "web".into()),
            ("dst".into(), "redis.prod.svc.cluster.local:6379".into()),
        ],
    }];
    let edges = observed_from_series(&series);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].because.exporter, TelemetryExporter::Linkerd);
    assert_eq!(edges[0].from, "web");
    assert_eq!(edges[0].to, "redis.prod.svc.cluster.local:6379");
    assert_eq!(edges[0].because.metric, "response_total");
}

#[test]
fn generic_prometheus_series_are_not_observed_traffic() {
    let series = [SeriesLabels {
        name: "up".into(),
        labels: vec![
            ("source".into(), "node".into()),
            ("destination".into(), "prometheus".into()),
        ],
    }];
    assert!(
        observed_from_series(&series).is_empty(),
        "labels without Hubble or mesh metric names are not traffic"
    );
}

#[test]
fn a_config_dump_fixture_counts_listeners_and_clusters() {
    let summary = parse_config_dump(config_dump_json().as_bytes()).expect("dump");
    assert_eq!(summary.listeners, 2);
    assert_eq!(summary.clusters, 3);
}

#[test]
fn an_oversize_config_dump_is_refused_not_truncated() {
    let huge = vec![b'x'; MAX_MESH_BYTES + 1];
    match parse_config_dump(&huge) {
        Err(MeshError::TooLarge { bytes }) => assert_eq!(bytes, MAX_MESH_BYTES + 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn envoy_admin_only_accepts_loopback_http() {
    assert!(parse_loopback("http://127.0.0.1:15000").is_ok());
    assert!(parse_loopback("http://10.0.0.5:15000").is_err());
    assert!(parse_loopback("https://127.0.0.1:15000").is_err());
    assert!(parse_loopback("http://127.0.0.1").is_err());
}

#[test]
fn declared_and_observed_because_fields_are_different_types() {
    let declared = declared_from_policy("a", "b", "prod", "np");
    let observed = observed_from_series(&[SeriesLabels {
        name: "hubble_flows_processed_total".into(),
        labels: vec![
            ("source".into(), "a".into()),
            ("destination".into(), "b".into()),
        ],
    }]);
    assert_eq!(declared.from, observed[0].from);
    assert_eq!(declared.to, observed[0].to);
    assert_eq!(declared.because.kind, "NetworkPolicy");
    assert_eq!(observed[0].because.exporter, TelemetryExporter::Hubble);
}

#[tokio::test]
async fn inventory_treats_a_missing_istio_group_as_not_served() {
    let script = Script::new(&[
        ("/apis/networking.istio.io", 404, status_404()),
        ("/apis/linkerd.io", 404, status_404()),
    ]);
    let got = inventory(&script.client()).await;
    assert!(!got.istio.is_served());
    assert!(!got.linkerd.is_served());
    assert!(!got.present());
    assert!(got.objects.is_empty());
    assert!(
        !script
            .seen()
            .iter()
            .any(|path| path.contains("virtualservices")),
        "a missing group must not be listed as if the CRD were there: {:?}",
        script.seen()
    );
}

#[tokio::test]
async fn inventory_lists_virtualservices_when_the_group_is_served() {
    let script = Script::new(&[
        ("/apis/networking.istio.io", 200, istio_group_json()),
        (
            "/apis/networking.istio.io/v1/virtualservices",
            200,
            virtualservice_list_json(),
        ),
        (
            "/apis/networking.istio.io/v1/destinationrules",
            200,
            empty_list(),
        ),
        ("/apis/networking.istio.io/v1/gateways", 200, empty_list()),
        ("/apis/networking.istio.io/v1/sidecars", 200, empty_list()),
        ("/apis/linkerd.io", 404, status_404()),
    ]);
    let got = inventory(&script.client()).await;
    assert!(got.istio.is_served());
    assert!(!got.linkerd.is_served());
    assert_eq!(got.objects.len(), 1);
    assert_eq!(got.objects[0].kind, MeshKind::VirtualService);
    let listed = script
        .seen()
        .iter()
        .any(|path| path.contains("virtualservices"));
    assert!(
        listed,
        "listing goes through kube Request: {:?}",
        script.seen()
    );
}

#[tokio::test]
async fn envoy_admin_reads_a_forwarded_loopback_dump() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let dump = config_dump_json().to_string();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 1024];
        let _ = sock.read(&mut buf).await;
        let body = dump.as_bytes();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(header.as_bytes()).await;
        let _ = sock.write_all(body).await;
    });
    match envoy_admin(&format!("http://127.0.0.1:{port}")).await {
        Fetched::Ok(summary) => {
            assert_eq!(summary.listeners, 2);
            assert_eq!(summary.clusters, 3);
        }
        other => panic!("{other:?}"),
    }
}
