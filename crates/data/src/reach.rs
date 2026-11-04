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

use std::future::Future;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, LazyLock};
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
/// Cluster Secrets are metadata only. Credentials here come from the user's
/// tool settings; this enum has no route to a cluster Secret payload.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum ToolAuth {
    #[default]
    Anonymous,
    /// A token the user typed into settings, already in this process because
    /// they named it. Never filled from a Secret list.
    NamedToken(String),
}

impl std::fmt::Debug for ToolAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anonymous => f.write_str("Anonymous"),
            Self::NamedToken(_) => f.debug_tuple("NamedToken").field(&"[redacted]").finish(),
        }
    }
}

/// A URL and optional token the user named for one tool. Empty means "find it".
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ToolOverride {
    pub url: Option<String>,
    pub auth: ToolAuth,
}

impl std::fmt::Debug for ToolOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolOverride")
            .field("url", &self.url.as_ref().map(|_| "[redacted]"))
            .field("auth", &self.auth)
            .finish()
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnusableService {
    pub(crate) kind: ToolKind,
    pub(crate) namespace: String,
    pub(crate) name: String,
}

impl UnusableService {
    pub(crate) fn why(&self) -> String {
        format!(
            "{} Service {}/{} has no usable TCP port",
            self.kind.as_str(),
            self.namespace,
            self.name
        )
    }

    pub(crate) fn summary(&self, count: usize) -> String {
        format!(
            "{}; matching Services without usable TCP ports: {count}",
            self.why()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServiceCandidate {
    Usable(FoundService),
    Unusable(UnusableService),
}

impl ServiceCandidate {
    fn name(&self) -> (&str, &str) {
        match self {
            Self::Usable(found) => (&found.namespace, &found.name),
            Self::Unusable(found) => (&found.namespace, &found.name),
        }
    }
}

pub(crate) fn unusable_candidates(
    candidates: &[ServiceCandidate],
) -> Option<(&UnusableService, usize)> {
    let mut unusable = candidates.iter().filter_map(|candidate| match candidate {
        ServiceCandidate::Unusable(service) => Some(service),
        ServiceCandidate::Usable(_) => None,
    });
    let first = unusable.next()?;
    Some((first, 1 + unusable.count()))
}

/// How bytes actually move once a tool is bound.
#[derive(Clone, PartialEq, Eq)]
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

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proxy {
                namespace,
                service,
                port,
            } => f
                .debug_struct("Proxy")
                .field("namespace", namespace)
                .field("service", service)
                .field("port", port)
                .finish(),
            Self::NeedsForward {
                namespace,
                name,
                port,
            } => f
                .debug_struct("NeedsForward")
                .field("namespace", namespace)
                .field("name", name)
                .field("port", port)
                .finish(),
            Self::Url { .. } => f.debug_struct("Url").field("base", &"[redacted]").finish(),
        }
    }
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
            Transport::Url { base } => url_source(base),
        }
    }
}

fn url_source(base: &str) -> String {
    let Ok(uri) = settings_uri(base) else {
        return "configured tool URL [redacted]".to_string();
    };
    let scheme = uri.scheme_str().expect("a settings URL has a scheme");
    let authority = uri.authority().expect("a settings URL has an authority");
    if matches!(uri.path(), "" | "/") && uri.query().is_none() {
        format!("{scheme}://{authority}")
    } else {
        format!("{scheme}://{authority}/[path or query redacted]")
    }
}

/// Why a visible tool still has no section.
#[derive(Clone, PartialEq, Eq)]
pub struct Unbound {
    pub kind: ToolKind,
    pub found: Option<FoundService>,
    pub why: String,
    /// Open in the system browser. Never an iframe, never a webview.
    pub browser_url: Option<String>,
}

impl std::fmt::Debug for Unbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unbound")
            .field("kind", &self.kind)
            .field("found", &self.found)
            .field("why", &self.why)
            .field(
                "browser_url",
                &self.browser_url.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
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

/// Whether this Service is that tool and advertises a usable TCP port.
pub fn match_service(kind: ToolKind, svc: &Service) -> Option<FoundService> {
    match service_candidate(kind, svc)? {
        ServiceCandidate::Usable(found) => Some(found),
        ServiceCandidate::Unusable(_) => None,
    }
}

fn service_candidate(kind: ToolKind, svc: &Service) -> Option<ServiceCandidate> {
    let spec = fingerprint(kind);
    let name = svc.metadata.name.as_deref()?;
    if !name_or_labels_match(name, svc.metadata.labels.as_ref(), spec) {
        return None;
    }
    let namespace = svc.metadata.namespace.as_deref().unwrap_or("").to_string();
    let name = name.to_string();
    Some(
        match pick_port(svc.spec.as_ref().and_then(|s| s.ports.as_deref()), spec) {
            Some(port) => ServiceCandidate::Usable(FoundService {
                kind,
                namespace,
                name,
                port: port.port,
                port_name: port.name,
            }),
            None => ServiceCandidate::Unusable(UnusableService {
                kind,
                namespace,
                name,
            }),
        },
    )
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
    let tcp_port = |port: &ServicePort| {
        if !matches!(port.protocol.as_deref(), None | Some("TCP")) {
            return None;
        }
        let number = u16::try_from(port.port).ok()?;
        (number != 0).then_some(number)
    };
    for want in spec.ports {
        if let Some(found) = ports.iter().find(|p| tcp_port(p) == Some(*want)) {
            return Some(PickedPort {
                port: *want,
                name: found.name.clone(),
            });
        }
    }
    for want in spec.port_names {
        if let Some(found) = ports.iter().find(|p| {
            tcp_port(p).is_some()
                && p.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(want))
        }) {
            return Some(PickedPort {
                port: tcp_port(found)?,
                name: found.name.clone(),
            });
        }
    }
    let first = ports.iter().find(|p| tcp_port(p).is_some())?;
    Some(PickedPort {
        port: tcp_port(first)?,
        name: first.name.clone(),
    })
}

/// List Services and keep the ones that match a fingerprint. Cluster-wide,
/// paged, capped: a tool hunt is not a watch and must not grow without bound.
/// If any retained match has no usable TCP port, the list fails and names one
/// such Service rather than silently omitting it.
pub async fn find_services(client: &Client, kind: ToolKind) -> Fetched<Vec<FoundService>> {
    match discover_services(client).await {
        Fetched::Ok(services) => {
            let candidates = service_candidates(kind, &services);
            if let Some((first, count)) = unusable_candidates(&candidates) {
                return Fetched::Failed {
                    what: "services",
                    why: format!("{}; no partial source list returned", first.summary(count)),
                };
            }
            Fetched::Ok(
                candidates
                    .into_iter()
                    .filter_map(|candidate| match candidate {
                        ServiceCandidate::Usable(found) => Some(found),
                        ServiceCandidate::Unusable(_) => None,
                    })
                    .collect(),
            )
        }
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

pub(crate) fn service_candidates(kind: ToolKind, services: &[Service]) -> Vec<ServiceCandidate> {
    let mut found: Vec<_> = services
        .iter()
        .filter_map(|service| {
            service_candidate(kind, service).map(|found| {
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
    found.sort_by(|a, b| a.name().cmp(&b.name()));
    found
}

/// Bind one tool: settings URL, then an in-cluster Service via proxy, then a
/// forward. Absent if nothing matched and the user named no URL.
pub async fn bind(client: &Client, kind: ToolKind, settings: &ReachSettings) -> ToolReach {
    let over = settings.for_kind(kind);
    if let Some(url) = over.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return bind_url(kind, url, over.auth.clone());
    }

    let found = match discover_services(client).await {
        Fetched::Ok(services) => service_candidates(kind, &services),
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
    found: Vec<ServiceCandidate>,
) -> ToolReach {
    if found.len() > 1 {
        let mut why = format!(
            "{} matches {} Services; name a source URL in settings before querying",
            kind.as_str(),
            found.len()
        );
        if let Some((service, count)) = unusable_candidates(&found) {
            why.push_str("; ");
            why.push_str(&service.summary(count));
        }
        return ToolReach::Unbound(Unbound {
            kind,
            found: None,
            browser_url: None,
            why,
        });
    }
    let Some(candidate) = found.into_iter().next() else {
        return ToolReach::Absent { kind };
    };
    let service = match candidate {
        ServiceCandidate::Usable(service) => service,
        ServiceCandidate::Unusable(service) => {
            return ToolReach::Unbound(Unbound {
                kind,
                found: None,
                why: service.why(),
                browser_url: None,
            });
        }
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
    if trimmed.len() > MAX_URL_BYTES
        || trimmed
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return ToolReach::Unbound(Unbound {
            kind,
            found: None,
            why: "the settings URL is too long or malformed".into(),
            browser_url: None,
        });
    }
    let validation = trimmed
        .split_once('#')
        .map_or(trimmed, |(prefix, _)| prefix);
    let uri = match settings_uri(validation) {
        Ok(uri) => uri,
        Err(why) => {
            return ToolReach::Unbound(Unbound {
                kind,
                found: None,
                why: why.into(),
                browser_url: None,
            });
        }
    };
    if uri.scheme_str() == Some("https") {
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
    if trimmed.contains('#') || uri.query().is_some() {
        return ToolReach::Unbound(Unbound {
            kind, found: None,
            why: "the settings HTTP base URL must not contain a query or fragment; use a tool URL prefix".into(),
            browser_url: None,
        });
    }
    if let Err(why) = authorization(&auth) {
        return ToolReach::Unbound(Unbound {
            kind,
            found: None,
            why: why.into(),
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

/// GET a path through the API proxy or a settings HTTP URL.
/// A port-forward remains the shell's job; settings HTTPS opens in the browser.
/// The body is capped; oversize is Failed, not truncated.
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
        Transport::Url { base } => {
            plaintext_http_get_with_status(
                bound.kind.slug(),
                base,
                rest,
                &bound.auth,
                categorized_loki_query(bound.kind, rest),
            )
            .await
        }
    }
}

fn categorized_loki_query(kind: ToolKind, rest: &str) -> bool {
    kind == ToolKind::Loki
        && matches!(
            rest.trim_start_matches('/').split('?').next(),
            Some("loki/api/v1/query") | Some("loki/api/v1/query_range")
        )
}

async fn proxy_get_with_status(
    client: &Client,
    found: &FoundService,
    rest: &str,
) -> (Option<http::StatusCode>, Fetched<Vec<u8>>) {
    let path = proxy_path(found, rest);
    let mut request = http::Request::get(&path).header(http::header::ACCEPT, "application/json");
    if categorized_loki_query(found.kind, rest) {
        request = request.header(
            http::HeaderName::from_static("x-loki-response-encoding-flags"),
            http::HeaderValue::from_static("categorize-labels"),
        );
    }
    let request = match request.body(Vec::new()) {
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
/// Partial responses are refused without exposing a fallback status.
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
        if status == http::StatusCode::PARTIAL_CONTENT {
            return Fetched::Failed {
                what,
                why: format!("{what} returned 206 Partial Content; partial responses are refused"),
            };
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
        let sentence = http_failure_sentence(&bytes, status);
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

fn http_failure_sentence(bytes: &[u8], status: http::StatusCode) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|value| {
            ["message", "error"].into_iter().find_map(|field| {
                value
                    .get(field)
                    .and_then(serde_json::Value::as_str)
                    .filter(|message| !message.trim().is_empty())
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| {
            let text = String::from_utf8_lossy(bytes);
            if text.trim().is_empty() {
                status.to_string()
            } else {
                text.trim().to_string()
            }
        })
}

const MAX_URL_BYTES: usize = u16::MAX as usize - 1;
const MAX_HTTP_BUFFER_BYTES: usize = 32 << 10;
const MAX_HTTP_HEADER_CONTENT_BYTES: usize = 32 << 10;
const MAX_HTTP_ERROR_BYTES: usize = 32 << 10;
// A system resolver already running in the blocking pool cannot be aborted.
// Its permit must outlive a cancelled request and stay with the lookup itself.
// Four is a process-wide safety bound for direct URL DNS, not a tuned value.
const MAX_DIRECT_DNS_LOOKUPS: usize = 4;

static DIRECT_DNS_SLOTS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_DIRECT_DNS_LOOKUPS)));

#[derive(Clone)]
struct DirectResolver {
    slots: Arc<tokio::sync::Semaphore>,
}

type DnsAnswer = std::io::Result<std::vec::IntoIter<SocketAddr>>;

enum DirectLookup {
    Running(tokio::task::JoinHandle<DnsAnswer>),
    AtCapacity,
}

impl Future for DirectLookup {
    type Output = DnsAnswer;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match &mut *self {
            Self::Running(task) => std::pin::Pin::new(task)
                .poll(cx)
                .map(|answer| match answer {
                    Ok(answer) => answer,
                    Err(_) => Err(std::io::Error::other("the direct DNS lookup worker failed")),
                }),
            Self::AtCapacity => std::task::Poll::Ready(Err(std::io::Error::other(
                "direct DNS lookup capacity is full; try again shortly",
            ))),
        }
    }
}

impl Drop for DirectLookup {
    fn drop(&mut self) {
        if let Self::Running(task) = self {
            task.abort();
        }
    }
}

fn bounded_dns_lookup<F>(slots: Arc<tokio::sync::Semaphore>, lookup: F) -> DirectLookup
where
    F: FnOnce() -> DnsAnswer + Send + 'static,
{
    let Ok(permit) = slots.try_acquire_owned() else {
        return DirectLookup::AtCapacity;
    };
    DirectLookup::Running(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        lookup()
    }))
}

impl tower::Service<hyper_util::client::legacy::connect::dns::Name> for DirectResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = DirectLookup;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: hyper_util::client::legacy::connect::dns::Name) -> Self::Future {
        let slots = self.slots.clone();
        bounded_dns_lookup(slots, move || (name.as_str(), 0).to_socket_addrs())
    }
}

fn header_content_within_limit(headers: &http::HeaderMap) -> bool {
    headers
        .iter()
        .try_fold(MAX_HTTP_HEADER_CONTENT_BYTES, |remaining, (name, value)| {
            remaining
                .checked_sub(name.as_str().len())?
                .checked_sub(value.as_bytes().len())
        })
        .is_some()
}

fn identity_content_encoding(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .all(|value| {
            value.to_str().is_ok_and(|value| {
                value.split(',').all(|coding| {
                    let coding = coding.trim_matches([' ', '\t']);
                    coding.is_empty() || coding.eq_ignore_ascii_case("identity")
                })
            })
        })
}

fn settings_uri(value: &str) -> Result<http::Uri, &'static str> {
    if value.len() > MAX_URL_BYTES {
        return Err("the settings URL is too long");
    }
    if value.contains('#') {
        return Err("the settings URL must not contain a fragment");
    }
    let uri = http::Uri::try_from(value).map_err(|_| "the settings URL is malformed")?;
    let authority = uri.authority().ok_or("the settings URL has no host")?;
    if authority.as_str().contains('@') {
        return Err("tool URLs must not contain credentials; use named authentication");
    }
    if authority.host().is_empty() {
        return Err("the settings URL has no host");
    }
    let host = authority.host();
    if host.contains(['[', ']']) {
        let literal = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .ok_or("the settings URL has stray host brackets")?;
        if literal.contains('%') {
            return Err("settings URL IPv6 zone identifiers are unsupported");
        }
        if literal.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("the settings URL bracketed host must be an IPv6 address");
        }
    }
    let suffix = &authority.as_str()[host.len()..];
    if !suffix.is_empty() {
        let port = suffix
            .strip_prefix(':')
            .ok_or("the settings URL authority is malformed")?;
        if port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().is_err()
        {
            return Err("the settings URL has an invalid explicit port");
        }
    }
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err("the settings URL must use http:// or https://");
    }
    Ok(uri)
}

fn authorization(auth: &ToolAuth) -> Result<Option<http::HeaderValue>, &'static str> {
    let ToolAuth::NamedToken(token) = auth else {
        return Ok(None);
    };
    let unpadded = token.trim_end_matches('=');
    if token.len() > MAX_HTTP_BUFFER_BYTES - "Bearer ".len()
        || unpadded.is_empty()
        || !unpadded.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
    {
        return Err("the named tool token is not a valid bounded Bearer token");
    }
    let mut header = http::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "the named tool token is not a valid HTTP header")?;
    header.set_sensitive(true);
    Ok(Some(header))
}

fn redact_named_token(message: String, auth: &ToolAuth) -> String {
    match auth {
        ToolAuth::NamedToken(token) if !token.is_empty() && message.contains(token) => {
            message.replace(token, "[redacted]")
        }
        _ => message,
    }
}

fn settings_request(
    base: &str,
    rest: &str,
    auth: &ToolAuth,
    categorize_labels: bool,
) -> Result<http::Request<kube::client::Body>, &'static str> {
    let base_uri = settings_uri(base)?;
    if base_uri.scheme_str() != Some("http") {
        return Err("https settings URLs open in the system browser; they are not fetched here");
    }
    if base_uri.query().is_some() {
        return Err("the settings base URL must not contain a query; use a tool URL prefix");
    }
    let base = base.trim_end_matches('/');
    let rest = rest.trim_start_matches('/');
    if base
        .len()
        .checked_add(1)
        .and_then(|n| n.checked_add(rest.len()))
        .is_none_or(|n| n > MAX_URL_BYTES)
    {
        return Err("the settings request URL is too long");
    }
    let url = settings_uri(&join_url(base, rest))?;
    let mut request = http::Request::get(url)
        .header(http::header::ACCEPT, "application/json")
        .header(http::header::ACCEPT_ENCODING, "identity")
        .header(http::header::CONNECTION, "close");
    if categorize_labels {
        request = request.header("x-loki-response-encoding-flags", "categorize-labels");
    }
    if let Some(header) = authorization(auth)? {
        request = request.header(http::header::AUTHORIZATION, header);
    }
    request
        .body(kube::client::Body::empty())
        .map_err(|_| "the settings HTTP request is malformed")
}

async fn plaintext_http_get_with_status(
    what: &'static str,
    base: &str,
    rest: &str,
    auth: &ToolAuth,
    categorize_labels: bool,
) -> (Option<http::StatusCode>, Fetched<Vec<u8>>) {
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    let mut status = None;
    let outcome = plaintext_http_get_until(
        what,
        base,
        rest,
        auth,
        categorize_labels,
        deadline,
        &mut status,
    )
    .await;
    (status, outcome)
}

/// One deadline includes connection, request, headers and the complete body.
async fn plaintext_http_get_until(
    what: &'static str,
    base: &str,
    rest: &str,
    auth: &ToolAuth,
    categorize_labels: bool,
    deadline: tokio::time::Instant,
    response_status: &mut Option<http::StatusCode>,
) -> Fetched<Vec<u8>> {
    *response_status = None;
    let request = match settings_request(base, rest, auth, categorize_labels) {
        Ok(request) => request,
        Err(why) => {
            return Fetched::Failed {
                what,
                why: why.into(),
            };
        }
    };
    let started = std::time::Instant::now();
    let mut received_bytes = 0usize;
    let exchange = async {
        let connector =
            hyper_util::client::legacy::connect::HttpConnector::new_with_resolver(DirectResolver {
                slots: DIRECT_DNS_SLOTS.clone(),
            });
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_max_idle_per_host(0)
                .retry_canceled_requests(false)
                .http1_max_buf_size(MAX_HTTP_BUFFER_BYTES)
                .build(connector);
        let response = match client.request(request).await {
            Ok(response) => response,
            Err(error) => {
                return Fetched::Failed {
                    what,
                    why: redact_named_token(crate::connect::describe(&error), auth),
                };
            }
        };
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            *response_status = Some(status);
            return Fetched::Denied { what };
        }
        if status == http::StatusCode::PARTIAL_CONTENT {
            return Fetched::Failed {
                what,
                why: format!("{what} returned 206 Partial Content; partial responses are refused"),
            };
        }
        // A complete head can arrive in a read larger than Hyper's buffer target.
        if !header_content_within_limit(response.headers()) {
            return Fetched::Failed {
                what,
                why: format!(
                    "{what} response header names and values exceed {MAX_HTTP_HEADER_CONTENT_BYTES} bytes; the response is refused"
                ),
            };
        }
        if !identity_content_encoding(response.headers()) {
            return Fetched::Failed {
                what,
                why: format!(
                    "{what} returned unsupported Content-Encoding; the response is refused"
                ),
            };
        }
        let cap = if status.is_success() {
            MAX_BODY_BYTES
        } else {
            MAX_HTTP_ERROR_BYTES
        };
        let oversized = || Fetched::Failed {
            what,
            why: format!("{what} answered with more than {cap} bytes; the body is refused"),
        };
        if response
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > cap as u64)
        {
            return oversized();
        }
        let mut body = response.into_body().into_data_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    return Fetched::Failed {
                        what,
                        why: redact_named_token(crate::connect::describe(&error), auth),
                    };
                }
            };
            received_bytes += chunk.len();
            if chunk.len() > cap.saturating_sub(bytes.len()) {
                return oversized();
            }
            bytes.extend_from_slice(&chunk);
        }
        *response_status = Some(status);
        if status.is_success() {
            return Fetched::Ok(bytes);
        }
        let sentence = http_failure_sentence(&bytes, status);
        Fetched::Failed {
            what,
            why: redact_named_token(sentence, auth),
        }
    };
    let outcome = tokio::time::timeout_at(deadline, exchange)
        .await
        .unwrap_or_else(|_| Fetched::Failed {
            what,
            why: format!("{what} did not answer within 4 seconds"),
        });
    crate::telemetry::record_request(started.elapsed().as_nanos() as u64, received_bytes);
    outcome
}

fn join_url(base: &str, rest: &str) -> String {
    let rest = rest.trim_start_matches('/');
    format!("{}/{rest}", base.trim_end_matches('/'))
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
