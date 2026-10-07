//! Reach an in-cluster tool without installing anything.
//!
//! Discovery uses tool names, application identity labels, and ports. A
//! chart or release label does not identify the application a Service serves.
//! A configured URL takes precedence. Otherwise binding probes an identified
//! Service through the API-server proxy; a failed probe leaves a
//! [`Transport::NeedsForward`] candidate for the caller to open or label.
//! No matching Service and no override means [`ToolReach::Absent`], which
//! stays invisible. Blocked discovery or authentication is [`ToolReach::Unbound`],
//! with a reason and an optional browser URL. Auth is never a Secret scraped
//! in the background: it is anonymous, a token the user named, or one field
//! they explicitly revealed.
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

use futures::StreamExt;
use http_body_util::BodyExt;

use k8s_openapi::api::core::v1::{Service, ServicePort};
use kube::Client;
use kube::api::{ListParams, ObjectList, Request};

use crate::read::{Fetched, classify};

/// How long a proxy probe may sit before the tool is treated as not answering.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(4);

/// A whole tool answer, checked before parsing. Dashboards and query results
/// are larger than a kubelet metrics page; eight mebibytes still refuses a
/// compression bomb rather than expanding one.
pub const MAX_BODY_BYTES: usize = 8 << 20;

const PAGE_LIMIT: u32 = 200;
const MAX_SERVICES: usize = 2_000;
const MAX_SERVICE_PAGES: usize = 16;

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
/// A Secret is not a variant. Reading one is [`crate::integrations::helm`]'s exception and
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

impl Bound {
    pub fn source(&self) -> String {
        match &self.transport {
            Transport::Proxy {
                namespace,
                service,
                port,
            } => format!("{namespace}/{service}:{port}"),
            Transport::NeedsForward {
                namespace,
                name,
                port,
            } => format!("{namespace}/{name}:{port} (forward required)"),
            Transport::Url { base } => base.clone(),
        }
    }
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
    let named = |value: &str| {
        let lower = value.to_ascii_lowercase();
        spec.names.iter().any(|want| {
            lower == *want
                || lower
                    .strip_suffix(want)
                    .is_some_and(|prefix| prefix.ends_with('-'))
        })
    };
    if named(name) {
        return true;
    }
    let Some(labels) = labels else {
        return false;
    };
    ["app.kubernetes.io/name", "app", "k8s-app"]
        .iter()
        .any(|key| {
            labels.get(*key).is_some_and(|value| {
                named(value)
                    || spec
                        .needles
                        .iter()
                        .any(|needle| value.eq_ignore_ascii_case(needle))
            })
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
    match discover_services(client).await {
        Fetched::Ok(services) => Fetched::Ok(service_candidates(kind, &services)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

pub(crate) async fn discover_services(client: &Client) -> Fetched<Vec<Service>> {
    let mut services = Vec::new();
    let mut token: Option<String> = None;
    let mut body_bytes = 0usize;
    for _ in 0..MAX_SERVICE_PAGES {
        if body_bytes == MAX_BODY_BYTES {
            return Fetched::Failed {
                what: "services",
                why: "tool discovery exceeds 8 MiB; no partial binding is chosen".to_string(),
            };
        }
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = token.as_deref() {
            params = params.continue_token(token);
        }
        let request = Request::new("/api/v1/services".to_string())
            .list(&params)
            .expect("service list parameters");
        let bytes = match bounded_request_with_status(
            client,
            "services",
            request,
            MAX_BODY_BYTES - body_bytes,
        )
        .await
        .1
        {
            Fetched::Ok(bytes) => bytes,
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { what, why } => return Fetched::Failed { what, why },
        };
        body_bytes += bytes.len();
        let page: ObjectList<Service> = match serde_json::from_slice(&bytes) {
            Ok(page) => page,
            Err(error) => {
                return Fetched::Failed {
                    what: "services",
                    why: format!("services returned an unsupported response: {error}"),
                };
            }
        };
        if services.len() + page.items.len() > MAX_SERVICES
            || services.len() + page.items.len() == MAX_SERVICES
                && page
                    .metadata
                    .continue_
                    .as_deref()
                    .is_some_and(|token| !token.is_empty())
        {
            return Fetched::Failed {
                what: "services",
                why: format!(
                    "tool discovery exceeds {MAX_SERVICES} Services; no partial binding is chosen"
                ),
            };
        }
        services.extend(page.items);
        let next = page.metadata.continue_.filter(|token| !token.is_empty());
        if next.is_none() {
            return Fetched::Ok(services);
        }
        if next == token {
            return Fetched::Failed {
                what: "services",
                why: "tool discovery repeated a continuation token; no partial binding is chosen"
                    .to_string(),
            };
        }
        token = next;
    }
    Fetched::Failed {
        what: "services",
        why: format!(
            "tool discovery exceeds {MAX_SERVICE_PAGES} pages; no partial binding is chosen"
        ),
    }
}

pub(crate) fn service_candidates(kind: ToolKind, services: &[Service]) -> Vec<FoundService> {
    let mut found: Vec<_> = services
        .iter()
        .filter_map(|service| {
            match_service(kind, service).map(|found| {
                (
                    found,
                    service
                        .spec
                        .as_ref()
                        .and_then(|spec| spec.cluster_ip.as_deref())
                        == Some("None"),
                )
            })
        })
        .collect();
    // API-server routes use ordinary Services; headless controller discovery
    // is considered only when no routed endpoint exists.
    if found.iter().any(|(_, headless)| !headless) {
        found.retain(|(_, headless)| !headless);
    }
    let mut found: Vec<_> = found.into_iter().map(|(found, _)| found).collect();
    found.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    found
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

    bind_candidates(client, kind, over, found).await
}

pub(crate) async fn bind_candidates(
    client: &Client,
    kind: ToolKind,
    over: &ToolOverride,
    found: Vec<FoundService>,
) -> ToolReach {
    if found.len() > 1 {
        return ToolReach::Unbound(Unbound {
            kind,
            found: None,
            browser_url: None,
            why: format!(
                "{} matches {} routed Services; name a source URL in settings before querying",
                kind.as_str(),
                found.len()
            ),
        });
    }
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
        Probe::Denied(what) => ToolReach::Unbound(Unbound {
            kind,
            found: Some(service.clone()),
            why: format!(
                "{} proxy: {what} access denied for this account",
                kind.as_str(),
            ),
            browser_url: Some(browser_hint(&service)),
        }),
        Probe::Failed(why) => ToolReach::Unbound(Unbound {
            kind,
            found: Some(service),
            why,
            browser_url: None,
        }),
    }
}

fn bind_url(kind: ToolKind, url: &str, auth: ToolAuth) -> ToolReach {
    let trimmed = url.trim();
    if http::Uri::try_from(trimmed)
        .ok()
        .and_then(|uri| {
            uri.authority()
                .map(|authority| authority.as_str().contains('@'))
        })
        .unwrap_or(false)
    {
        return ToolReach::Unbound(Unbound {
            kind,
            found: None,
            why: "tool URLs must not contain credentials; use named authentication".into(),
            browser_url: None,
        });
    }
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
    Denied(&'static str),
    Failed(String),
}

async fn probe_proxy(client: &Client, found: &FoundService) -> Probe {
    let spec = fingerprint(found.kind);
    let path = proxy_path(found, spec.probe_path);
    let request = match http::Request::get(&path)
        .header(http::header::ACCEPT, "application/json")
        .body(Vec::new())
    {
        Ok(request) => request,
        Err(error) => return Probe::Failed(error.to_string()),
    };
    match bounded_request(client, found.kind.slug(), request).await {
        Fetched::Ok(_) => Probe::Ready,
        Fetched::Denied { what } => Probe::Denied(what),
        Fetched::Failed { why, .. } => Probe::Failed(why),
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
    tool_get_with_status(client, bound, rest).await.1
}

pub(crate) async fn tool_get_with_status(
    client: &Client,
    bound: &Bound,
    rest: &str,
) -> (Option<http::StatusCode>, Fetched<Vec<u8>>) {
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
            proxy_get_with_status(client, &found, rest).await
        }
        Transport::NeedsForward { .. } => (
            None,
            Fetched::Failed {
                what: bound.kind.slug(),
                why: format!(
                    "{} needs a port-forward before it can be queried; open one from the forwards panel",
                    bound.kind.as_str()
                ),
            },
        ),
        Transport::Url { base } => plaintext_http_get_with_status(base, rest, &bound.auth).await,
    }
}

async fn proxy_get_with_status(
    client: &Client,
    found: &FoundService,
    rest: &str,
) -> (Option<http::StatusCode>, Fetched<Vec<u8>>) {
    let path = proxy_path(found, rest);
    let request = match http::Request::get(&path)
        .header(http::header::ACCEPT, "application/json")
        .body(Vec::new())
    {
        Ok(request) => request,
        Err(error) => {
            return (
                None,
                Fetched::Failed {
                    what: found.kind.slug(),
                    why: error.to_string(),
                },
            );
        }
    };
    bounded_request_with_status(client, found.kind.slug(), request, MAX_BODY_BYTES).await
}

/// POST form or JSON through the service proxy. The content type is the caller's.
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
        .header(http::header::ACCEPT, "application/json")
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
    bounded_request(client, bound.kind.slug(), request).await
}

pub(crate) async fn bounded_request(
    client: &Client,
    what: &'static str,
    request: http::Request<Vec<u8>>,
) -> Fetched<Vec<u8>> {
    bounded_request_with_status(client, what, request, MAX_BODY_BYTES)
        .await
        .1
}

/// Absence and version fallback statuses require a complete bounded body.
/// Denied statuses are returned without reading the body.
pub(crate) async fn bounded_request_with_status(
    client: &Client,
    what: &'static str,
    request: http::Request<Vec<u8>>,
    body_limit: usize,
) -> (Option<http::StatusCode>, Fetched<Vec<u8>>) {
    let started = std::time::Instant::now();
    let mut response_status = None;
    let mut received_bytes = 0usize;
    let exchange = async {
        let response = match client.send(request.map(kube::client::Body::from)).await {
            Ok(response) => response,
            Err(error) => return classify(what, &error),
        };
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            response_status = Some(status);
            return Fetched::Denied { what };
        }
        let cap = if status.is_success() {
            body_limit
        } else {
            body_limit.min(32 << 10)
        };
        let mut body = response.into_body().into_data_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    return Fetched::Failed {
                        what,
                        why: crate::connect::describe(&error),
                    };
                }
            };
            received_bytes += chunk.len();
            if chunk.len() > cap.saturating_sub(bytes.len()) {
                return Fetched::Failed {
                    what,
                    why: format!(
                        "{} answered with more than {cap} bytes; the body is refused",
                        what
                    ),
                };
            }
            bytes.extend_from_slice(&chunk);
        }
        response_status = Some(status);
        if status.is_success() {
            return Fetched::Ok(bytes);
        }
        let text = String::from_utf8_lossy(&bytes);
        let sentence = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .or_else(|| value.get("error"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| {
                if text.trim().is_empty() {
                    status.to_string()
                } else {
                    text.trim().to_string()
                }
            });
        Fetched::Failed {
            what,
            why: sentence,
        }
    };
    let outcome = tokio::time::timeout(PROBE_DEADLINE, exchange)
        .await
        .unwrap_or_else(|_| Fetched::Failed {
            what,
            why: format!("{what} did not answer within 4 seconds"),
        });
    crate::telemetry::record_request(started.elapsed().as_nanos() as u64, received_bytes);
    (response_status, outcome)
}

async fn plaintext_http_get_with_status(
    base: &str,
    rest: &str,
    auth: &ToolAuth,
) -> (Option<http::StatusCode>, Fetched<Vec<u8>>) {
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    let mut status = None;
    let outcome = plaintext_http_get_until(base, rest, auth, deadline, &mut status).await;
    (status, outcome)
}

/// One deadline bounds the whole exchange -- connect, write, and every read --
/// so a server dripping bytes cannot hold the fetch open past it.
async fn plaintext_http_get_until(
    base: &str,
    rest: &str,
    auth: &ToolAuth,
    deadline: tokio::time::Instant,
    response_status: &mut Option<http::StatusCode>,
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
    let (status, outcome) = split_http_body_with_status(&buf);
    *response_status = status;
    outcome
}

fn join_url(base: &str, rest: &str) -> String {
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        format!("{}/", base.trim_end_matches('/'))
    } else {
        format!("{}/{rest}", base.trim_end_matches('/'))
    }
}

#[cfg(test)]
fn split_http_body(raw: &[u8]) -> Fetched<Vec<u8>> {
    split_http_body_with_status(raw).1
}

fn split_http_body_with_status(raw: &[u8]) -> (Option<http::StatusCode>, Fetched<Vec<u8>>) {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (
            None,
            Fetched::Failed {
                what: "url",
                why: "the settings URL answered with no HTTP header terminator".to_string(),
            },
        );
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
    let response_status = (body.len() <= MAX_BODY_BYTES)
        .then(|| http::StatusCode::from_u16(status).ok())
        .flatten();
    if matches!(status, 401 | 403) {
        return (response_status, Fetched::Denied { what: "url" });
    }
    if !(200..300).contains(&status) {
        return (
            response_status,
            Fetched::Failed {
                what: "url",
                why: format!("the settings URL answered {status}"),
            },
        );
    }
    if body.len() > MAX_BODY_BYTES {
        return (
            None,
            Fetched::Failed {
                what: "url",
                why: format!("the answer exceeded {MAX_BODY_BYTES} bytes; it is hidden"),
            },
        );
    }
    (response_status, Fetched::Ok(body.to_vec()))
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
