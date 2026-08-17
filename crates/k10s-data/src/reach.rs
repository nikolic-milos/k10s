//! Reach an in-cluster tool without installing anything.
//!
//! Discovery is label, name, and port. Binding prefers the API-server
//! service proxy, then a port-forward the shell already knows how to open,
//! then a URL the user named in settings. Auth is never a Secret scraped in
//! the background: anonymous, a token the user named, or one field they
//! explicitly revealed. A tool the cluster does not run is [`ToolReach::Absent`]
//! and must stay invisible. A tool that is there but will not bind is
//! [`ToolReach::Unbound`]: a labelled hole, with an optional system-browser
//! URL, never a blank panel.
//!
//! First paint never waits on this module. A section that wants a tool asks
//! after the cluster is already on screen.
//!
//! A named Grafana (or Prometheus) token is a credential *for that tool*,
//! not for the API server. The kube client's Authorization header is already
//! spoken for, so a token never rides the service proxy -- that path is
//! anonymous-to-the-tool on purpose. A token binds through port-forward or a
//! settings URL, where the header goes to Grafana rather than to kube.

use std::time::Duration;

use k8s_openapi::api::core::v1::{Service, ServicePort};
use kube::Client;
use kube::api::{Api, ListParams};

use crate::read::{Fetched, classify};

/// How long a proxy probe may sit before the tool is treated as not answering.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(4);

/// A whole tool answer, checked before parsing. Dashboards and query results
/// are larger than a kubelet metrics page; eight mebibytes still refuses a
/// compression bomb rather than expanding one.
pub const MAX_BODY_BYTES: usize = 8 << 20;

const PAGE_LIMIT: u32 = 200;
const MAX_SERVICES: usize = 2_000;

/// One ecosystem tool k10s knows how to find. Independent of [`k10s_core::ToolId`]:
/// that table is the map's vendor glyphs, and growing it re-lays theme arrays.
/// A tool we can reach but have no glyph for still binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKind {
    Grafana,
    Prometheus,
    Loki,
    Tempo,
    Jaeger,
    Mimir,
    Thanos,
    Harbor,
    OtelCollector,
    Alertmanager,
}

impl ToolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolKind::Grafana => "Grafana",
            ToolKind::Prometheus => "Prometheus",
            ToolKind::Loki => "Loki",
            ToolKind::Tempo => "Tempo",
            ToolKind::Jaeger => "Jaeger",
            ToolKind::Mimir => "Mimir",
            ToolKind::Thanos => "Thanos",
            ToolKind::Harbor => "Harbor",
            ToolKind::OtelCollector => "OpenTelemetry Collector",
            ToolKind::Alertmanager => "Alertmanager",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            ToolKind::Grafana => "grafana",
            ToolKind::Prometheus => "prometheus",
            ToolKind::Loki => "loki",
            ToolKind::Tempo => "tempo",
            ToolKind::Jaeger => "jaeger",
            ToolKind::Mimir => "mimir",
            ToolKind::Thanos => "thanos",
            ToolKind::Harbor => "harbor",
            ToolKind::OtelCollector => "otel-collector",
            ToolKind::Alertmanager => "alertmanager",
        }
    }
}

/// How this process is allowed to authenticate *to the tool*.
///
/// A Secret is not a variant. Reading one is [`crate::helm`]'s exception and
/// an explicit reveal; this enum has nowhere to put a cluster Secret name
/// that was never shown to a person.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolAuth {
    #[default]
    Anonymous,
    /// A token the user typed into settings, already in this process because
    /// they named it. Never filled from a Secret list.
    NamedToken(String),
}

/// A URL and optional token the user named for one tool. Empty means "find it".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolOverride {
    pub url: Option<String>,
    pub auth: ToolAuth,
}

/// Per-tool overrides keyed the same way settings will key them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReachSettings {
    pub grafana: ToolOverride,
    pub prometheus: ToolOverride,
    pub loki: ToolOverride,
    pub tempo: ToolOverride,
    pub jaeger: ToolOverride,
    pub mimir: ToolOverride,
    pub thanos: ToolOverride,
    pub harbor: ToolOverride,
    pub otel: ToolOverride,
    pub alertmanager: ToolOverride,
}

impl ReachSettings {
    pub fn for_kind(&self, kind: ToolKind) -> &ToolOverride {
        match kind {
            ToolKind::Grafana => &self.grafana,
            ToolKind::Prometheus => &self.prometheus,
            ToolKind::Loki => &self.loki,
            ToolKind::Tempo => &self.tempo,
            ToolKind::Jaeger => &self.jaeger,
            ToolKind::Mimir => &self.mimir,
            ToolKind::Thanos => &self.thanos,
            ToolKind::Harbor => &self.harbor,
            ToolKind::OtelCollector => &self.otel,
            ToolKind::Alertmanager => &self.alertmanager,
        }
    }
}

/// A Service that matched a fingerprint, with the port we would speak to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundService {
    pub kind: ToolKind,
    pub namespace: String,
    pub name: String,
    pub port: u16,
    pub port_name: Option<String>,
}

/// How bytes actually move once a tool is bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// API-server service proxy. Fetch goes through [`tool_get`].
    Proxy {
        namespace: String,
        service: String,
        port: u16,
    },
    /// The shell should open a forward to this Service and then speak HTTP
    /// to 127.0.0.1. Reach does not bind a listener: that is [`crate::forward`].
    NeedsForward {
        namespace: String,
        name: String,
        port: u16,
    },
    /// A URL the user named. `http` can be fetched here; `https` is a browser
    /// hole until a TLS client exists in the lock without a new package.
    Url { base: String },
}

/// A tool we can see and have a way to talk to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub kind: ToolKind,
    pub found: Option<FoundService>,
    pub transport: Transport,
    pub auth: ToolAuth,
}

/// Why a visible tool still has no section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unbound {
    pub kind: ToolKind,
    pub found: Option<FoundService>,
    pub why: String,
    /// Open in the system browser. Never an iframe, never a webview.
    pub browser_url: Option<String>,
}

/// One tool's reachability. [`ToolReach::Absent`] is the one that must not
/// produce a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolReach {
    Bound(Bound),
    Unbound(Unbound),
    Absent { kind: ToolKind },
}

impl ToolReach {
    pub fn kind(&self) -> ToolKind {
        match self {
            ToolReach::Bound(bound) => bound.kind,
            ToolReach::Unbound(unbound) => unbound.kind,
            ToolReach::Absent { kind } => *kind,
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, ToolReach::Absent { .. })
    }
}

struct Fingerprint {
    kind: ToolKind,
    names: &'static [&'static str],
    needles: &'static [&'static str],
    ports: &'static [u16],
    port_names: &'static [&'static str],
    probe_path: &'static str,
}

const CATALOG: &[Fingerprint] = &[
    Fingerprint {
        kind: ToolKind::Grafana,
        names: &["grafana", "grafana-server"],
        needles: &["grafana"],
        ports: &[3000],
        port_names: &["http", "grafana", "service"],
        probe_path: "api/health",
    },
    Fingerprint {
        kind: ToolKind::Prometheus,
        names: &["prometheus", "prometheus-server", "prometheus-operated"],
        needles: &["prometheus"],
        ports: &[9090],
        port_names: &["http", "web", "prometheus"],
        probe_path: "-/ready",
    },
    Fingerprint {
        kind: ToolKind::Loki,
        names: &["loki", "loki-gateway", "loki-query-frontend"],
        needles: &["loki"],
        ports: &[3100],
        port_names: &["http", "loki"],
        probe_path: "ready",
    },
    Fingerprint {
        kind: ToolKind::Tempo,
        names: &["tempo", "tempo-query-frontend"],
        needles: &["tempo"],
        ports: &[3200],
        port_names: &["http", "tempo"],
        probe_path: "ready",
    },
    Fingerprint {
        kind: ToolKind::Jaeger,
        names: &["jaeger", "jaeger-query", "jaeger-all-in-one"],
        needles: &["jaeger"],
        ports: &[16686],
        port_names: &["http-query", "query", "http"],
        probe_path: "",
    },
    Fingerprint {
        kind: ToolKind::Mimir,
        names: &[
            "mimir",
            "mimir-nginx",
            "mimir-gateway",
            "mimir-query-frontend",
        ],
        needles: &["mimir"],
        ports: &[8080, 9009],
        port_names: &["http", "mimir"],
        probe_path: "ready",
    },
    Fingerprint {
        kind: ToolKind::Thanos,
        names: &["thanos-query", "thanos-querier", "thanos"],
        needles: &["thanos"],
        ports: &[10902],
        port_names: &["http", "query"],
        probe_path: "-/ready",
    },
    Fingerprint {
        kind: ToolKind::Harbor,
        names: &["harbor", "harbor-core", "harbor-portal"],
        needles: &["harbor"],
        ports: &[80, 443, 8080],
        port_names: &["http", "https", "core"],
        probe_path: "api/v2.0/ping",
    },
    Fingerprint {
        kind: ToolKind::OtelCollector,
        names: &["otel-collector", "opentelemetry-collector", "otelcol"],
        needles: &["opentelemetry", "otel-collector", "otelcol"],
        // otel::health() is the only wire operation on a collector bind, and
        // it refuses the data-plane ports. The health_check extension (13133,
        // answers on "/") and zpages (55679) must outrank OTLP HTTP (4318)
        // and internal metrics (8888), which stay only so a Service without
        // the extensions still binds and health() can label the Failed why.
        ports: &[13133, 55679, 4318, 8888],
        port_names: &[
            "health-check",
            "healthcheck",
            "zpages",
            "otlp-http",
            "metrics",
            "http",
        ],
        probe_path: "",
    },
    Fingerprint {
        kind: ToolKind::Alertmanager,
        names: &["alertmanager", "alertmanager-operated", "alertmanager-main"],
        needles: &["alertmanager"],
        ports: &[9093],
        port_names: &["http", "web", "alertmanager"],
        probe_path: "-/ready",
    },
];

fn fingerprint(kind: ToolKind) -> &'static Fingerprint {
    CATALOG
        .iter()
        .find(|item| item.kind == kind)
        .expect("every ToolKind has a fingerprint")
}

/// Whether this Service is that tool, and which port to speak to.
pub fn match_service(kind: ToolKind, svc: &Service) -> Option<FoundService> {
    let spec = fingerprint(kind);
    let name = svc.metadata.name.as_deref()?;
    let namespace = svc.metadata.namespace.as_deref().unwrap_or("").to_string();
    if !name_or_labels_match(name, svc.metadata.labels.as_ref(), spec) {
        return None;
    }
    let port = pick_port(svc.spec.as_ref().and_then(|s| s.ports.as_deref()), spec)?;
    Some(FoundService {
        kind,
        namespace,
        name: name.to_string(),
        port: port.port,
        port_name: port.name,
    })
}

fn name_or_labels_match(
    name: &str,
    labels: Option<&std::collections::BTreeMap<String, String>>,
    spec: &Fingerprint,
) -> bool {
    let lower = name.to_ascii_lowercase();
    if spec
        .names
        .iter()
        .any(|want| lower == *want || lower.contains(want))
    {
        return true;
    }
    let Some(labels) = labels else {
        return false;
    };
    labels.values().any(|value| {
        let lower = value.to_ascii_lowercase();
        spec.needles.iter().any(|needle| lower.contains(needle))
    })
}

struct PickedPort {
    port: u16,
    name: Option<String>,
}

fn pick_port(ports: Option<&[ServicePort]>, spec: &Fingerprint) -> Option<PickedPort> {
    let ports = ports.unwrap_or(&[]);
    if ports.is_empty() {
        let port = *spec.ports.first()?;
        return Some(PickedPort { port, name: None });
    }
    for want in spec.ports {
        if let Some(found) = ports.iter().find(|p| p.port == *want as i32) {
            return Some(PickedPort {
                port: found.port as u16,
                name: found.name.clone(),
            });
        }
    }
    for want in spec.port_names {
        if let Some(found) = ports.iter().find(|p| {
            p.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(want))
        }) {
            return Some(PickedPort {
                port: found.port as u16,
                name: found.name.clone(),
            });
        }
    }
    let first = ports.first()?;
    Some(PickedPort {
        port: first.port as u16,
        name: first.name.clone(),
    })
}

/// List Services and keep the ones that match a fingerprint. Cluster-wide,
/// paged, capped: a tool hunt is not a watch and must not grow without bound.
pub async fn find_services(client: &Client, kind: ToolKind) -> Fetched<Vec<FoundService>> {
    let api: Api<Service> = Api::all(client.clone());
    let mut found = Vec::new();
    let mut token: Option<String> = None;
    let mut scanned = 0usize;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = token.as_deref() {
            params = params.continue_token(token);
        }
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(error) => return classify("services", &error),
        };
        for svc in page.items {
            scanned += 1;
            if scanned > MAX_SERVICES {
                break;
            }
            if let Some(hit) = match_service(kind, &svc) {
                found.push(hit);
            }
        }
        token = page.metadata.continue_.filter(|s| !s.is_empty());
        if token.is_none() || scanned > MAX_SERVICES {
            break;
        }
    }
    found.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Fetched::Ok(found)
}

/// Bind one tool: settings URL, then an in-cluster Service via proxy, then a
/// forward. Absent if nothing matched and the user named no URL.
pub async fn bind(client: &Client, kind: ToolKind, settings: &ReachSettings) -> ToolReach {
    let over = settings.for_kind(kind);
    if let Some(url) = over.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return bind_url(kind, url, over.auth.clone());
    }

    let found = match find_services(client, kind).await {
        Fetched::Ok(list) => list,
        Fetched::Denied { what } => {
            return ToolReach::Unbound(Unbound {
                kind,
                found: None,
                why: format!("{what}: access denied for this account"),
                browser_url: None,
            });
        }
        Fetched::Failed { why, .. } => {
            return ToolReach::Unbound(Unbound {
                kind,
                found: None,
                why,
                browser_url: None,
            });
        }
    };

    let Some(service) = found.into_iter().next() else {
        return ToolReach::Absent { kind };
    };

    if matches!(over.auth, ToolAuth::NamedToken(_)) {
        return ToolReach::Bound(Bound {
            kind,
            found: Some(service.clone()),
            transport: Transport::NeedsForward {
                namespace: service.namespace.clone(),
                name: service.name.clone(),
                port: service.port,
            },
            auth: over.auth.clone(),
        });
    }

    match probe_proxy(client, &service).await {
        Probe::Ready => ToolReach::Bound(Bound {
            kind,
            found: Some(service.clone()),
            transport: Transport::Proxy {
                namespace: service.namespace,
                service: service.name,
                port: service.port,
            },
            auth: ToolAuth::Anonymous,
        }),
        Probe::Unauthorized => ToolReach::Unbound(Unbound {
            kind,
            found: Some(service.clone()),
            why: format!(
                "{} answered through the API-server proxy but wants a token; name one in settings, \
                 or it can be reached by port-forward",
                kind.as_str()
            ),
            browser_url: Some(browser_hint(&service)),
        }),
        Probe::Failed => ToolReach::Bound(Bound {
            kind,
            found: Some(service.clone()),
            transport: Transport::NeedsForward {
                namespace: service.namespace,
                name: service.name,
                port: service.port,
            },
            auth: ToolAuth::Anonymous,
        }),
    }
}

fn bind_url(kind: ToolKind, url: &str, auth: ToolAuth) -> ToolReach {
    let trimmed = url.trim();
    if trimmed.starts_with("https://") {
        return ToolReach::Unbound(Unbound {
            kind,
            found: None,
            why: format!(
                "{} is named as an https URL; open it in the system browser, or name an http \
                 URL or an in-cluster Service",
                kind.as_str()
            ),
            browser_url: Some(trimmed.to_string()),
        });
    }
    if !trimmed.starts_with("http://") {
        return ToolReach::Unbound(Unbound {
            kind,
            found: None,
            why: format!(
                "{} settings URL must be http:// or https://, not {trimmed}",
                kind.as_str()
            ),
            browser_url: None,
        });
    }
    ToolReach::Bound(Bound {
        kind,
        found: None,
        transport: Transport::Url {
            base: trimmed.trim_end_matches('/').to_string(),
        },
        auth,
    })
}

fn browser_hint(found: &FoundService) -> String {
    format!(
        "http://{}.{}.svc:{}",
        found.name, found.namespace, found.port
    )
}

enum Probe {
    Ready,
    Unauthorized,
    Failed,
}

async fn probe_proxy(client: &Client, found: &FoundService) -> Probe {
    let spec = fingerprint(found.kind);
    let path = proxy_path(found, spec.probe_path);
    let request = match http::Request::get(&path).body(Vec::new()) {
        Ok(request) => request,
        Err(_) => return Probe::Failed,
    };
    match tokio::time::timeout(PROBE_DEADLINE, client.request_text(request)).await {
        Err(_) => Probe::Failed,
        Ok(Ok(text)) if text.len() > MAX_BODY_BYTES => Probe::Failed,
        Ok(Ok(_)) => Probe::Ready,
        Ok(Err(kube::Error::Api(response))) if matches!(response.code, 401 | 403) => {
            Probe::Unauthorized
        }
        Ok(Err(_)) => Probe::Failed,
    }
}

pub fn proxy_path(found: &FoundService, rest: &str) -> String {
    let rest = rest.trim_start_matches('/');
    let port = found
        .port_name
        .as_deref()
        .map(|name| name.to_string())
        .unwrap_or_else(|| found.port.to_string());
    if rest.is_empty() {
        format!(
            "/api/v1/namespaces/{}/services/{}:{port}/proxy/",
            found.namespace, found.name
        )
    } else {
        format!(
            "/api/v1/namespaces/{}/services/{}:{port}/proxy/{rest}",
            found.namespace, found.name
        )
    }
}

/// GET a path on a bound tool. Proxy only: a forward is the shell's job and a
/// settings https URL is a browser hole. The body is capped; oversize is
/// Failed, not truncated.
pub async fn tool_get(client: &Client, bound: &Bound, rest: &str) -> Fetched<Vec<u8>> {
    match &bound.transport {
        Transport::Proxy {
            namespace,
            service,
            port,
        } => {
            let found = FoundService {
                kind: bound.kind,
                namespace: namespace.clone(),
                name: service.clone(),
                port: *port,
                port_name: None,
            };
            proxy_get(client, &found, rest).await
        }
        Transport::NeedsForward { .. } => Fetched::Failed {
            what: bound.kind.slug(),
            why: format!(
                "{} needs a port-forward before it can be queried; open one from the forwards panel",
                bound.kind.as_str()
            ),
        },
        Transport::Url { base } => plaintext_http_get(base, rest, &bound.auth).await,
    }
}

async fn proxy_get(client: &Client, found: &FoundService, rest: &str) -> Fetched<Vec<u8>> {
    let path = proxy_path(found, rest);
    let request = match http::Request::get(&path).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what: found.kind.slug(),
                why: error.to_string(),
            };
        }
    };
    match tokio::time::timeout(PROBE_DEADLINE, client.request_text(request)).await {
        Err(_) => Fetched::Failed {
            what: found.kind.slug(),
            why: format!("{} did not answer within 4 seconds", found.kind.as_str()),
        },
        Ok(Ok(text)) if text.len() > MAX_BODY_BYTES => Fetched::Failed {
            what: found.kind.slug(),
            why: format!(
                "{} answered with more than {MAX_BODY_BYTES} bytes; the body is hidden",
                found.kind.as_str()
            ),
        },
        Ok(Ok(text)) => Fetched::Ok(text.into_bytes()),
        Ok(Err(error)) => classify(found.kind.slug(), &error),
    }
}

/// POST form or JSON through the service proxy. Prometheus `query_range` is
/// a POST; the content type is the caller's.
pub async fn tool_post(
    client: &Client,
    bound: &Bound,
    rest: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Fetched<Vec<u8>> {
    let Transport::Proxy {
        namespace,
        service,
        port,
    } = &bound.transport
    else {
        return Fetched::Failed {
            what: bound.kind.slug(),
            why: format!(
                "{} is not bound through the API-server proxy; POST is only implemented there",
                bound.kind.as_str()
            ),
        };
    };
    if body.len() > MAX_BODY_BYTES {
        return Fetched::Failed {
            what: bound.kind.slug(),
            why: "the query itself exceeds 8 MiB; it is not sent".to_string(),
        };
    }
    let found = FoundService {
        kind: bound.kind,
        namespace: namespace.clone(),
        name: service.clone(),
        port: *port,
        port_name: None,
    };
    let path = proxy_path(&found, rest);
    let request = match http::Request::post(&path)
        .header(http::header::CONTENT_TYPE, content_type)
        .body(body)
    {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what: bound.kind.slug(),
                why: error.to_string(),
            };
        }
    };
    match tokio::time::timeout(PROBE_DEADLINE, client.request_text(request)).await {
        Err(_) => Fetched::Failed {
            what: bound.kind.slug(),
            why: format!("{} did not answer within 4 seconds", bound.kind.as_str()),
        },
        Ok(Ok(text)) if text.len() > MAX_BODY_BYTES => Fetched::Failed {
            what: bound.kind.slug(),
            why: format!(
                "{} answered with more than {MAX_BODY_BYTES} bytes; the body is hidden",
                found.kind.as_str()
            ),
        },
        Ok(Ok(text)) => Fetched::Ok(text.into_bytes()),
        Ok(Err(error)) => classify(found.kind.slug(), &error),
    }
}

async fn plaintext_http_get(base: &str, rest: &str, auth: &ToolAuth) -> Fetched<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    plaintext_http_get_until(base, rest, auth, deadline).await
}

/// One deadline bounds the whole exchange -- connect, write, and every read --
/// so a server dripping bytes cannot hold the fetch open past it.
async fn plaintext_http_get_until(
    base: &str,
    rest: &str,
    auth: &ToolAuth,
    deadline: tokio::time::Instant,
) -> Fetched<Vec<u8>> {
    let Ok(url) = http::Uri::try_from(join_url(base, rest)) else {
        return Fetched::Failed {
            what: "url",
            why: format!("not a URL: {base}"),
        };
    };
    if url.scheme_str() != Some("http") {
        return Fetched::Failed {
            what: "url",
            why: "https settings URLs open in the system browser; they are not fetched here"
                .to_string(),
        };
    }
    let host = match url.host() {
        Some(host) => host.to_string(),
        None => {
            return Fetched::Failed {
                what: "url",
                why: "the settings URL has no host".to_string(),
            };
        }
    };
    let port = url.port_u16().unwrap_or(80);
    let path = url.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n"
    );
    if let ToolAuth::NamedToken(token) = auth {
        request.push_str("Authorization: Bearer ");
        request.push_str(token);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");

    let connect = tokio::time::timeout_at(
        deadline,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await;
    let mut stream = match connect {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return Fetched::Failed {
                what: "url",
                why: error.to_string(),
            };
        }
        Err(_) => {
            return Fetched::Failed {
                what: "url",
                why: format!("{host}:{port} did not accept a connection within 4 seconds"),
            };
        }
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    match tokio::time::timeout_at(deadline, stream.write_all(request.as_bytes())).await {
        Err(_) => {
            return Fetched::Failed {
                what: "url",
                why: "the settings URL did not answer within 4 seconds".to_string(),
            };
        }
        Ok(Err(error)) => {
            return Fetched::Failed {
                what: "url",
                why: error.to_string(),
            };
        }
        Ok(Ok(())) => {}
    }
    let mut buf = Vec::new();
    loop {
        if buf.len() > MAX_BODY_BYTES + 4096 {
            return Fetched::Failed {
                what: "url",
                why: format!("the answer exceeded {MAX_BODY_BYTES} bytes; it is hidden"),
            };
        }
        let mut chunk = [0u8; 8192];
        match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Err(_) => {
                return Fetched::Failed {
                    what: "url",
                    why: "the settings URL did not answer within 4 seconds".to_string(),
                };
            }
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(error)) => {
                return Fetched::Failed {
                    what: "url",
                    why: error.to_string(),
                };
            }
        }
    }
    split_http_body(&buf)
}

fn join_url(base: &str, rest: &str) -> String {
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        format!("{}/", base.trim_end_matches('/'))
    } else {
        format!("{}/{rest}", base.trim_end_matches('/'))
    }
}

fn split_http_body(raw: &[u8]) -> Fetched<Vec<u8>> {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Fetched::Failed {
            what: "url",
            why: "the settings URL answered with no HTTP header terminator".to_string(),
        };
    };
    let headers = &raw[..split];
    let body = &raw[split + 4..];
    let status = headers
        .split(|&b| b == b'\n')
        .next()
        .and_then(|line| {
            let line = std::str::from_utf8(line).ok()?.trim();
            line.split_whitespace().nth(1)?.parse::<u16>().ok()
        })
        .unwrap_or(0);
    if matches!(status, 401 | 403) {
        return Fetched::Denied { what: "url" };
    }
    if !(200..300).contains(&status) {
        return Fetched::Failed {
            what: "url",
            why: format!("the settings URL answered {status}"),
        };
    }
    if body.len() > MAX_BODY_BYTES {
        return Fetched::Failed {
            what: "url",
            why: format!("the answer exceeded {MAX_BODY_BYTES} bytes; it is hidden"),
        };
    }
    Fetched::Ok(body.to_vec())
}

/// Bytes that must not outlive the action that revealed them.
///
/// Zeroed on drop. The compiler may theoretically elide that store; there is
/// no `zeroize` crate in the lock, and adding one is a budget conversation.
/// Callers still must not clone this into a snapshot, a log, or a saved view.
#[derive(Debug)]
pub struct Scratch {
    bytes: Vec<u8>,
}

impl Scratch {
    pub fn from_bytes(bytes: Vec<u8>) -> Scratch {
        Scratch { bytes }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for byte in &mut self.bytes {
            *byte = 0;
        }
        self.bytes.clear();
    }
}

#[cfg(test)]
#[path = "reach_test.rs"]
mod tests;
