//! Native source reuse belongs to one Reader and never retains query results.
//!
//! A snapshot expires after thirty seconds or an explicit probe. A newly
//! competing Service is observed at that boundary. Reused single candidates
//! verify their UID and resourceVersion before querying; a changed source
//! refuses the current query. Only identity text and successful probe flags
//! survive discovery, bounded to sixteen KiB for each of five kinds.

use k8s_openapi::api::core::v1::Service;
use kube::Client;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

use super::{ObserveTools, observe_tools};
use crate::reach::{
    self, Bound, FoundService, ToolAuth, ToolKind, ToolOverride, ToolReach, Transport, Unbound,
};
use crate::read::Fetched;

const FRESH_FOR: Duration = Duration::from_secs(30);
const MAX_IDENTITY_BYTES: usize = 16 << 10;
const KINDS: [ToolKind; 5] = [
    ToolKind::Prometheus,
    ToolKind::Loki,
    ToolKind::Tempo,
    ToolKind::Jaeger,
    ToolKind::Grafana,
];

#[derive(Default)]
pub(in crate::read) struct Sources {
    snapshot: Mutex<Option<Snapshot>>,
}

struct Snapshot {
    listed_at: Instant,
    sources: [Source; 5],
}

#[derive(Default)]
struct Source {
    matches: usize,
    identity: Option<Identity>,
    ready: bool,
}

#[derive(Clone)]
struct Identity {
    found: FoundService,
    uid: String,
    version: String,
}

#[derive(Deserialize)]
struct WireIdentity {
    metadata: WireMetadata,
}

#[derive(Deserialize)]
struct WireMetadata {
    name: String,
    namespace: String,
    uid: String,
    #[serde(rename = "resourceVersion")]
    version: String,
}

impl Snapshot {
    fn new(services: &[Service]) -> Result<Self, String> {
        let mut sources: [Source; 5] = Default::default();
        for (source, kind) in sources.iter_mut().zip(KINDS) {
            let mut found = reach::service_candidates(kind, services);
            source.matches = found.len();
            if found.len() != 1 {
                continue;
            }
            let found = found.pop().expect("one matching Service");
            let service = services
                .iter()
                .find(|service| {
                    service.metadata.name.as_deref() == Some(found.name.as_str())
                        && service.metadata.namespace.as_deref().unwrap_or("") == found.namespace
                })
                .expect("a candidate came from this Service list");
            let identity = Identity {
                found,
                uid: service.metadata.uid.clone().unwrap_or_default(),
                version: service
                    .metadata
                    .resource_version
                    .clone()
                    .unwrap_or_default(),
            };
            if identity.text_bytes() > MAX_IDENTITY_BYTES {
                return Err(format!(
                    "{} source identity exceeds 16 KiB; no partial binding is chosen",
                    kind.as_str()
                ));
            }
            source.identity = Some(identity);
        }
        Ok(Self {
            listed_at: Instant::now(),
            sources,
        })
    }

    fn source(&self, kind: ToolKind) -> &Source {
        &self.sources[index(kind)]
    }

    async fn bind(&mut self, client: &Client, kind: ToolKind) -> ToolReach {
        let source = &mut self.sources[index(kind)];
        if source.matches > 1 {
            return unbound(
                kind,
                format!(
                    "{} matches {} routed Services; source binding is unresolved",
                    kind.as_str(),
                    source.matches
                ),
            );
        }
        let Some(identity) = &source.identity else {
            return ToolReach::Absent { kind };
        };
        if source.ready {
            return ToolReach::Bound(identity.bound());
        }
        let answer = reach::bind_candidates(
            client,
            kind,
            &ToolOverride::default(),
            vec![identity.found.clone()],
        )
        .await;
        source.ready = identity.usable()
            && matches!(&answer, ToolReach::Bound(bound) if matches!(bound.transport, Transport::Proxy { .. }));
        answer
    }
}

impl Identity {
    fn usable(&self) -> bool {
        !self.uid.is_empty() && !self.version.is_empty()
    }

    fn text_bytes(&self) -> usize {
        self.found.namespace.len()
            + self.found.name.len()
            + self.found.port_name.as_ref().map_or(0, String::len)
            + self.uid.len()
            + self.version.len()
    }

    fn bound(&self) -> Bound {
        Bound {
            kind: self.found.kind,
            found: Some(self.found.clone()),
            transport: Transport::Proxy {
                namespace: self.found.namespace.clone(),
                service: self.found.name.clone(),
                port: self.found.port,
            },
            auth: ToolAuth::Anonymous,
        }
    }

    fn same_source(&self, other: &Self) -> bool {
        self.found == other.found && self.uid == other.uid && self.version == other.version
    }

    async fn verify(&self, client: &Client) -> Fetched<bool> {
        let request = http::Request::get(format!(
            "/api/v1/namespaces/{}/services/{}",
            self.found.namespace, self.found.name
        ))
        .header(
            http::header::ACCEPT,
            "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1",
        )
        .body(Vec::new())
        .expect("a discovered Service path");
        match reach::bounded_request(client, "services", request).await {
            Fetched::Ok(bytes) => match serde_json::from_slice::<WireIdentity>(&bytes) {
                Ok(object) => Fetched::Ok(
                    object.metadata.name == self.found.name
                        && object.metadata.namespace == self.found.namespace
                        && object.metadata.uid == self.uid
                        && object.metadata.version == self.version,
                ),
                Err(error) => Fetched::Failed {
                    what: "services",
                    why: format!("Service identity returned an unsupported response: {error}"),
                },
            },
            Fetched::Denied { what } => Fetched::Denied { what },
            Fetched::Failed { what, why } => Fetched::Failed { what, why },
        }
    }
}

impl Sources {
    pub(super) async fn bind(&self, client: &Client, kind: ToolKind) -> ToolReach {
        let mut state = self.snapshot.lock().await;
        let previous = state
            .as_ref()
            .and_then(|snapshot| snapshot.source(kind).identity.clone());
        if let Some(identity) = &previous {
            if !identity.usable() {
                *state = None;
            } else {
                match identity.verify(client).await {
                    Fetched::Ok(true) => {}
                    Fetched::Ok(false) => {
                        *state = None;
                        return changed(identity);
                    }
                    // LIST and services/proxy can be granted without get.
                    Fetched::Denied { .. } => *state = None,
                    Fetched::Failed { why, .. } => {
                        *state = None;
                        return unbound(kind, why);
                    }
                }
            }
        }
        if state
            .as_ref()
            .is_some_and(|snapshot| snapshot.listed_at.elapsed() >= FRESH_FOR)
        {
            *state = None;
        }
        if state.is_none() {
            let services = match reach::discover_services(client).await {
                Fetched::Ok(services) => services,
                Fetched::Denied { what } => {
                    return unbound(kind, format!("{what}: access denied for this account"));
                }
                Fetched::Failed { why, .. } => return unbound(kind, why),
            };
            let snapshot = match Snapshot::new(&services) {
                Ok(snapshot) => snapshot,
                Err(why) => return unbound(kind, why),
            };
            if let Some(previous) = &previous
                && previous.usable()
                && let Some(current) = &snapshot.source(kind).identity
                && !previous.same_source(current)
            {
                return changed(previous);
            }
            *state = Some(snapshot);
        }
        state
            .as_mut()
            .expect("source discovery succeeded")
            .bind(client, kind)
            .await
    }

    pub(super) async fn probe(&self, client: &Client) -> ObserveTools {
        let mut state = self.snapshot.lock().await;
        *state = None;
        let services = match reach::discover_services(client).await {
            Fetched::Ok(services) => services,
            Fetched::Denied { what } => {
                return ObserveTools::blocked(format!("{what}: access denied for this account"));
            }
            Fetched::Failed { why, .. } => return ObserveTools::blocked(why),
        };
        let mut snapshot = match Snapshot::new(&services) {
            Ok(snapshot) => snapshot,
            Err(why) => return ObserveTools::blocked(why),
        };
        let prometheus = snapshot.bind(client, ToolKind::Prometheus).await;
        let loki = snapshot.bind(client, ToolKind::Loki).await;
        let tempo = snapshot.bind(client, ToolKind::Tempo).await;
        let trace = match tempo {
            ToolReach::Absent { .. } => snapshot.bind(client, ToolKind::Jaeger).await,
            tempo => tempo,
        };
        *state = Some(snapshot);
        observe_tools(prometheus, loki, trace)
    }

    pub(super) async fn complete<T>(&self, outcome: Fetched<T>) -> Fetched<T> {
        if !matches!(&outcome, Fetched::Ok(_)) {
            *self.snapshot.lock().await = None;
        }
        outcome
    }
}

fn index(kind: ToolKind) -> usize {
    KINDS
        .iter()
        .position(|candidate| *candidate == kind)
        .expect("native Observe source kind")
}

fn unbound(kind: ToolKind, why: String) -> ToolReach {
    ToolReach::Unbound(Unbound {
        kind,
        found: None,
        why,
        browser_url: None,
    })
}

fn changed(identity: &Identity) -> ToolReach {
    unbound(
        identity.found.kind,
        format!(
            "{} source Service {}/{} was recreated or changed; refresh sources before querying",
            identity.found.kind.as_str(),
            identity.found.namespace,
            identity.found.name,
        ),
    )
}
