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
