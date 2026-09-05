use super::*;
use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn labels(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn service(
    name: &str,
    namespace: &str,
    label_pairs: &[(&str, &str)],
    ports: &[(i32, &str)],
) -> Service {
    Service {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            labels: Some(labels(label_pairs)),
            ..ObjectMeta::default()
        },
        spec: Some(ServiceSpec {
            ports: Some(
                ports
                    .iter()
                    .map(|(port, port_name)| ServicePort {
                        port: *port,
                        name: Some((*port_name).into()),
                        ..ServicePort::default()
                    })
                    .collect(),
            ),
            ..ServiceSpec::default()
        }),
        status: None,
    }
}

#[test]
fn grafana_matches_on_name_and_on_the_app_label() {
    let by_name = service("grafana", "monitoring", &[], &[(3000, "http")]);
    let hit = match_service(ToolKind::Grafana, &by_name).expect("name");
    assert_eq!(hit.port, 3000);
    assert_eq!(hit.namespace, "monitoring");

    let by_label = service(
        "kube-prom-grafana",
        "monitoring",
        &[("app.kubernetes.io/name", "grafana")],
        &[(80, "http")],
    );
    let hit = match_service(ToolKind::Grafana, &by_label).expect("label");
    assert_eq!(
        hit.port, 80,
        "when 3000 is not on the Service, the named http port is still a bind"
    );
}

#[test]
fn a_frontend_named_api_is_not_grafana() {
    let svc = service("api", "prod", &[("app", "checkout")], &[(80, "http")]);
    assert_eq!(match_service(ToolKind::Grafana, &svc), None);
    assert_eq!(match_service(ToolKind::Prometheus, &svc), None);
}

#[test]
fn prometheus_prefers_9090_even_when_another_port_is_first() {
    let svc = service(
        "prometheus-operated",
        "monitoring",
        &[("app.kubernetes.io/name", "prometheus")],
        &[(8080, "admin"), (9090, "web")],
    );
    let hit = match_service(ToolKind::Prometheus, &svc).expect("prom");
    assert_eq!(hit.port, 9090);
    assert_eq!(hit.port_name.as_deref(), Some("web"));
}

#[test]
fn loki_and_tempo_and_harbor_have_their_own_ports() {
    let loki = service("loki", "logging", &[], &[(3100, "http")]);
    assert_eq!(match_service(ToolKind::Loki, &loki).unwrap().port, 3100);
    let tempo = service("tempo", "tracing", &[], &[(3200, "http")]);
    assert_eq!(match_service(ToolKind::Tempo, &tempo).unwrap().port, 3200);
    let harbor = service("harbor-core", "harbor", &[], &[(80, "http")]);
    assert_eq!(match_service(ToolKind::Harbor, &harbor).unwrap().port, 80);
    let alertmanager = service("alertmanager-operated", "monitoring", &[], &[(9093, "web")]);
    assert_eq!(
        match_service(ToolKind::Alertmanager, &alertmanager)
            .unwrap()
            .port,
        9093
    );
}

#[test]
fn alertmanager_binds_9093_by_number_when_no_port_name_helps() {
    let alertmanager = service(
        "alertmanager",
        "monitoring",
        &[],
        &[(8080, "admin"), (9093, "metrics")],
    );
    let hit = match_service(ToolKind::Alertmanager, &alertmanager).expect("alertmanager");
    assert_eq!(
        hit.port, 9093,
        "the well-known number, not list position, picks Alertmanager's port"
    );
}

#[test]
fn a_collector_bind_prefers_the_ports_health_can_actually_answer_on() {
    // otel::health() refuses the OTLP receiver (4318) and internal metrics
    // (8888); a standard collector Service exposing them next to the
    // health_check extension must bind 13133 or discovery can never answer
    // health.
    let svc = service(
        "otel-collector",
        "observability",
        &[],
        &[
            (4317, "otlp-grpc"),
            (4318, "otlp-http"),
            (8888, "metrics"),
            (13133, "health-check"),
        ],
    );
    let hit = match_service(ToolKind::OtelCollector, &svc).expect("collector");
    assert_eq!(
        hit.port, 13133,
        "the data-plane ports are refused by otel::health(); they must not outrank 13133"
    );
    let bound = Bound {
        kind: ToolKind::OtelCollector,
        found: Some(hit.clone()),
        transport: Transport::Proxy {
            namespace: hit.namespace,
            service: hit.name,
            port: hit.port,
        },
        auth: ToolAuth::Anonymous,
    };
    assert!(
        crate::otel::health_path(&bound).is_ok(),
        "the discovered bind must be a port health_path accepts"
    );

    let zpages_only = service(
        "otel-collector",
        "observability",
        &[],
        &[(4318, "otlp-http"), (55679, "zpages")],
    );
    let hit = match_service(ToolKind::OtelCollector, &zpages_only).expect("collector");
    assert_eq!(
        hit.port, 55679,
        "zpages outranks OTLP HTTP for the same reason"
    );
}

#[test]
fn a_collector_without_the_extensions_still_binds_and_health_says_why() {
    // A Service that genuinely lacks health_check and zpages keeps its bind on
    // the OTLP port; health() then answers a labelled Failed, not Absent.
    let svc = service(
        "otel-collector",
        "observability",
        &[],
        &[(4317, "otlp-grpc"), (4318, "otlp-http")],
    );
    let hit = match_service(ToolKind::OtelCollector, &svc).expect("collector");
    assert_eq!(hit.port, 4318);
    let bound = Bound {
        kind: ToolKind::OtelCollector,
        found: Some(hit.clone()),
        transport: Transport::Proxy {
            namespace: hit.namespace,
            service: hit.name,
            port: hit.port,
        },
        auth: ToolAuth::Anonymous,
    };
    let why = crate::otel::health_path(&bound).expect_err("OTLP is not a health signal");
    assert!(why.contains("OTLP"), "{why}");
}

#[test]
fn proxy_path_uses_the_port_name_when_the_service_declared_one() {
    let found = FoundService {
        kind: ToolKind::Grafana,
        namespace: "monitoring".into(),
        name: "grafana".into(),
        port: 3000,
        port_name: Some("http".into()),
    };
    assert_eq!(
        proxy_path(&found, "api/health"),
        "/api/v1/namespaces/monitoring/services/grafana:http/proxy/api/health"
    );
    let numbered = FoundService {
        port_name: None,
        ..found
    };
    assert_eq!(
        proxy_path(&numbered, "/api/search"),
        "/api/v1/namespaces/monitoring/services/grafana:3000/proxy/api/search"
    );
}

#[test]
fn a_named_https_url_is_a_browser_hole_not_a_fetch() {
    let mut settings = ReachSettings::default();
    settings.grafana.url = Some("https://grafana.example".into());
    // bind() needs a client for the Service list; bind_url is the branch this
    // covers and is reached when the URL is set, before any list.
    let reach = bind_url(
        ToolKind::Grafana,
        "https://grafana.example",
        ToolAuth::Anonymous,
    );
    match reach {
        ToolReach::Unbound(unbound) => {
            assert_eq!(
                unbound.browser_url.as_deref(),
                Some("https://grafana.example")
            );
            assert!(unbound.why.contains("https"));
        }
        other => panic!("https is a hole, not {other:?}"),
    }
}

#[test]
fn an_http_settings_url_binds_without_a_cluster_service() {
    match bind_url(
        ToolKind::Prometheus,
        "http://127.0.0.1:9090",
        ToolAuth::NamedToken("user-named".into()),
    ) {
        ToolReach::Bound(bound) => {
            assert_eq!(
                bound.transport,
                Transport::Url {
                    base: "http://127.0.0.1:9090".into()
                }
            );
            assert_eq!(bound.auth, ToolAuth::NamedToken("user-named".into()));
        }
        other => panic!("http URL binds: {other:?}"),
    }
}

#[test]
fn a_named_token_never_chooses_the_service_proxy() {
    // The kube client's Authorization header is already the kubeconfig. A
    // Grafana token riding that request would either clobber kube auth or
    // leak the kube token into Grafana. Port-forward is the bind.
    let svc = service("grafana", "monitoring", &[], &[(3000, "http")]);
    let found = match_service(ToolKind::Grafana, &svc).unwrap();
    let bound = Bound {
        kind: ToolKind::Grafana,
        found: Some(found.clone()),
        transport: Transport::NeedsForward {
            namespace: found.namespace,
            name: found.name,
            port: found.port,
        },
        auth: ToolAuth::NamedToken("glsa_xxx".into()),
    };
    assert!(matches!(bound.transport, Transport::NeedsForward { .. }));
}

#[test]
fn join_and_split_http_cover_the_localhost_client() {
    assert_eq!(
        join_url("http://127.0.0.1:9090", "api/v1/query"),
        "http://127.0.0.1:9090/api/v1/query"
    );
    let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"success\"}";
    match split_http_body(raw) {
        Fetched::Ok(body) => assert_eq!(body, br#"{"status":"success"}"#),
        other => panic!("{other:?}"),
    }
    let denied = b"HTTP/1.1 401 Unauthorized\r\n\r\nno";
    assert!(matches!(
        split_http_body(denied),
        Fetched::Denied { what: "url" }
    ));
}

#[tokio::test]
async fn a_settings_url_dripping_bytes_is_cut_off_at_one_total_deadline() {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
        // Each byte lands well inside a per-read window; only a deadline on
        // the whole exchange stops this. The iteration cap keeps a failing
        // run from hanging the suite.
        for _ in 0..500 {
            if stream.write_all(b"x").is_err() {
                return;
            }
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    let fetched = plaintext_http_get_until(
        &format!("http://{addr}"),
        "",
        &ToolAuth::Anonymous,
        deadline,
    )
    .await;
    match fetched {
        Fetched::Failed { what: "url", why } => {
            assert!(why.contains("did not answer"), "{why}");
        }
        other => panic!("a drip past the deadline is Failed, not {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the fetch outlived its deadline: {:?}",
        started.elapsed()
    );
    server.join().unwrap();
}
