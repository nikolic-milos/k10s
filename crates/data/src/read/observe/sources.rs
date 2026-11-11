//! Native source reuse belongs to one Reader and never retains query results.
//!
//! A snapshot expires after thirty seconds or an explicit probe. A newly
//! competing Service is observed at that boundary. Reused single candidates
//! verify their UID and resourceVersion before querying; a changed source
//! stays invalidated until an explicit probe. Catalogue rows retain their exact
//! Service identity, and failures retain discovery identities until an explicit
//! probe or verified refresh can replace them. Missing and ambiguous successors
//! require the same explicit refresh as changed Services.
//! Only identity text, labelled source failures and
//! successful probe flags survive discovery, bounded to sixteen KiB for each
//! of five kinds.

use k8s_openapi::api::core::v1::Service;
use k10s_core::ServiceIdentity;
use kube::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

use super::{ObserveTools, observe_tools};
use crate::reach::{
    self, Bound, FoundService, ServiceCandidate, ToolAuth, ToolKind, ToolOverride, ToolReach,
    Transport, Unbound,
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
    snapshot: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    current: Option<Snapshot>,
    invalidated: [bool; 5],
}

struct Snapshot {
    listed_at: Instant,
    sources: [Source; 5],
}

#[derive(Default)]
struct Source {
    matches: usize,
    identity: Option<Identity>,
    failure: Option<String>,
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

impl Source {
    fn unresolved_reason(&self, kind: ToolKind) -> Option<String> {
        if self.matches > 1 {
            let mut why = format!(
                "{} matches {} Services; source binding is unresolved",
                kind.as_str(),
                self.matches
            );
            if let Some(failure) = &self.failure {
                why.push_str("; ");
                why.push_str(failure);
            }
            Some(why)
        } else {
            self.failure.clone()
        }
    }
}

impl Snapshot {
    fn new(services: &[Service]) -> Result<Self, String> {
        let mut sources: [Source; 5] = Default::default();
        for (source, kind) in sources.iter_mut().zip(KINDS) {
            let mut found = reach::service_candidates(kind, services);
            source.matches = found.len();
            if found.len() != 1 {
                if let Some((service, count)) = reach::unusable_candidates(&found) {
                    if service.namespace.len().saturating_add(service.name.len())
                        > MAX_IDENTITY_BYTES
                    {
                        return Err(format!(
                            "{} source identity exceeds 16 KiB; no partial binding is chosen",
                            kind.as_str()
                        ));
                    }
                    let why = service.summary(count);
                    if why.len() > MAX_IDENTITY_BYTES {
                        return Err(format!(
                            "{} source identity exceeds 16 KiB; no partial binding is chosen",
                            kind.as_str()
                        ));
                    }
                    source.failure = Some(why);
                }
                continue;
            }
            let found = match found.pop().expect("one matching Service") {
                ServiceCandidate::Usable(found) => found,
                ServiceCandidate::Unusable(found) => {
                    if found.namespace.len().saturating_add(found.name.len()) > MAX_IDENTITY_BYTES {
                        return Err(format!(
                            "{} source identity exceeds 16 KiB; no partial binding is chosen",
                            kind.as_str()
                        ));
                    }
                    let why = found.why();
                    if why.len() > MAX_IDENTITY_BYTES {
                        return Err(format!(
                            "{} source identity exceeds 16 KiB; no partial binding is chosen",
                            kind.as_str()
                        ));
                    }
                    source.failure = Some(why);
                    continue;
                }
            };
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

    fn reset_ready(&mut self) {
        for source in &mut self.sources {
            source.ready = false;
        }
    }

    async fn bind(&mut self, client: &Client, kind: ToolKind) -> ToolReach {
        let source = &mut self.sources[index(kind)];
        if let Some(why) = source.unresolved_reason(kind) {
            return unbound(kind, why);
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
            vec![ServiceCandidate::Usable(identity.found.clone())],
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

    fn matches(&self, source: &ServiceIdentity) -> bool {
        self.usable()
            && self.found.namespace == source.namespace
            && self.found.name == source.name
            && self.found.port == source.port
            && self.uid == source.uid
            && self.version == source.resource_version
    }

    fn token(&self) -> Arc<ServiceIdentity> {
        Arc::new(ServiceIdentity {
            namespace: self.found.namespace.clone(),
            name: self.found.name.clone(),
            port: self.found.port,
            uid: self.uid.clone(),
            resource_version: self.version.clone(),
        })
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
        Self::bind_inner(&mut state, client, kind, None)
            .await
            .unwrap_or_else(|why| unbound(kind, why))
    }

    pub(super) async fn bind_catalog(
        &self,
        client: &Client,
    ) -> (ToolReach, Option<Arc<ServiceIdentity>>) {
        let mut state = self.snapshot.lock().await;
        let reach = Self::bind_inner(&mut state, client, ToolKind::Grafana, None)
            .await
            .unwrap_or_else(|why| unbound(ToolKind::Grafana, why));
        let identity = match &reach {
            ToolReach::Bound(bound) if matches!(bound.transport, Transport::Proxy { .. }) => state
                .current
                .as_ref()
                .and_then(|snapshot| snapshot.source(ToolKind::Grafana).identity.as_ref())
                .filter(|identity| {
                    identity.usable() && bound.found.as_ref() == Some(&identity.found)
                })
                .map(Identity::token),
            _ => None,
        };
        (reach, identity)
    }

    /// A local origin mismatch must leave a refreshed source available to its current rows.
    pub(super) async fn bind_panel(
        &self,
        client: &Client,
        source: &ServiceIdentity,
    ) -> Result<ToolReach, String> {
        let mut state = self.snapshot.lock().await;
        Self::bind_inner(&mut state, client, ToolKind::Grafana, Some(source)).await
    }

    pub(super) fn grafana_is_current(&self, source: &ServiceIdentity) -> bool {
        let Ok(state) = self.snapshot.try_lock() else {
            return false;
        };
        !state.invalidated[index(ToolKind::Grafana)]
            && state.current.as_ref().is_some_and(|snapshot| {
                let current = snapshot.source(ToolKind::Grafana);
                current.ready
                    && current
                        .identity
                        .as_ref()
                        .is_some_and(|identity| identity.matches(source))
            })
    }

    async fn bind_inner(
        state: &mut Cache,
        client: &Client,
        kind: ToolKind,
        expected: Option<&ServiceIdentity>,
    ) -> Result<ToolReach, String> {
        if state.invalidated[index(kind)] {
            return Ok(unbound(
                kind,
                format!(
                    "{} source changed; refresh sources before querying",
                    kind.as_str()
                ),
            ));
        }
        let previous = state
            .current
            .as_ref()
            .and_then(|snapshot| snapshot.source(kind).identity.as_ref());
        if let (Some(expected), Some(previous)) = (expected, previous)
            && !previous.matches(expected)
        {
            return Err(catalog_changed(expected));
        }
        let previous = previous.cloned();
        let mut refresh = state
            .current
            .as_ref()
            .is_none_or(|snapshot| snapshot.listed_at.elapsed() >= FRESH_FOR);
        if let Some(identity) = &previous {
            if !identity.usable() {
                refresh = true;
            } else {
                match identity.verify(client).await {
                    Fetched::Ok(true) => {}
                    Fetched::Ok(false) => {
                        state.invalidated[index(kind)] = true;
                        return Ok(changed(identity, None));
                    }
                    // LIST and services/proxy can be granted without get.
                    Fetched::Denied { .. } => refresh = true,
                    Fetched::Failed { why, .. } => {
                        if let Some(snapshot) = &mut state.current {
                            snapshot.sources[index(kind)].ready = false;
                        }
                        return Ok(unbound(kind, why));
                    }
                }
            }
        }
        if refresh {
            let services = match reach::discover_services(client).await {
                Fetched::Ok(services) => services,
                Fetched::Denied { what } => {
                    return Ok(unbound(
                        kind,
                        format!("{what}: access denied for this account"),
                    ));
                }
                Fetched::Failed { why, .. } => return Ok(unbound(kind, why)),
            };
            let snapshot = match Snapshot::new(&services) {
                Ok(snapshot) => snapshot,
                Err(why) => return Ok(unbound(kind, why)),
            };
            if let Some(previous) = &state.current {
                for (slot, (previous, current)) in
                    previous.sources.iter().zip(&snapshot.sources).enumerate()
                {
                    if let Some(identity) = &previous.identity
                        && identity.usable()
                        && !current
                            .identity
                            .as_ref()
                            .is_some_and(|current| identity.same_source(current))
                    {
                        state.invalidated[slot] = true;
                    }
                }
            }
            let replaced = previous
                .as_ref()
                .filter(|identity| identity.usable() && state.invalidated[index(kind)]);
            let failure = replaced.and_then(|_| snapshot.source(kind).unresolved_reason(kind));
            state.current = Some(snapshot);
            if let Some(identity) = replaced {
                return Ok(changed(identity, failure));
            }
        }
        let snapshot = state.current.as_mut().expect("source discovery succeeded");
        if let (Some(expected), Some(current)) = (expected, &snapshot.source(kind).identity)
            && !current.matches(expected)
        {
            return Err(catalog_changed(expected));
        }
        Ok(snapshot.bind(client, kind).await)
    }

    pub(super) async fn probe(&self, client: &Client) -> ObserveTools {
        let mut state = self.snapshot.lock().await;
        if let Some(snapshot) = &mut state.current {
            snapshot.reset_ready();
        }
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
        state.current = Some(snapshot);
        state.invalidated = [false; 5];
        observe_tools(prometheus, loki, trace)
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

fn changed(identity: &Identity, failure: Option<String>) -> ToolReach {
    let mut why = format!(
        "{} source Service {}/{} was recreated or changed; refresh sources before querying",
        identity.found.kind.as_str(),
        identity.found.namespace,
        identity.found.name,
    );
    if let Some(failure) = failure {
        why.push_str("; ");
        why.push_str(&failure);
    }
    unbound(identity.found.kind, why)
}

fn catalog_changed(source: &ServiceIdentity) -> String {
    format!(
        "Grafana source Service {}/{}:{} no longer matches this catalogue; refresh the catalogue before querying",
        source.namespace, source.name, source.port,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn service(name: &str, ports: Value, headless: bool) -> Service {
        serde_json::from_value(json!({
            "metadata": {
                "name": name,
                "namespace": "monitoring",
                "uid": format!("{name}-uid"),
                "resourceVersion": "1"
            },
            "spec": {
                "clusterIP": if headless { "None" } else { "10.0.0.1" },
                "ports": ports
            }
        }))
        .unwrap()
    }

    #[test]
    fn snapshot_retains_unusable_source_identities() {
        for ports in [json!([]), json!([{ "port": 3000, "protocol": "UDP" }])] {
            let snapshot = Snapshot::new(&[service("grafana", ports, false)]).unwrap();
            let source = snapshot.source(ToolKind::Grafana);
            assert_eq!(source.matches, 1);
            assert!(source.identity.is_none());
            assert_eq!(
                source.failure.as_deref(),
                Some("Grafana Service monitoring/grafana has no usable TCP port")
            );
        }
    }

    #[test]
    fn snapshot_does_not_select_the_only_usable_match() {
        let snapshot = Snapshot::new(&[
            service("a-grafana", json!([{ "port": 3000 }]), false),
            service(
                "b-grafana",
                json!([{ "port": 3000, "protocol": "UDP" }]),
                false,
            ),
        ])
        .unwrap();
        let source = snapshot.source(ToolKind::Grafana);
        assert_eq!(source.matches, 2);
        assert!(source.identity.is_none());
        assert_eq!(
            source.failure.as_deref(),
            Some(
                "Grafana Service monitoring/b-grafana has no usable TCP port; matching Services without usable TCP ports: 1"
            )
        );
    }

    #[test]
    fn snapshot_preserves_routed_preference_and_selects_the_tcp_port() {
        let snapshot = Snapshot::new(&[
            service("headless-grafana", json!([]), true),
            service(
                "grafana",
                json!([
                    { "name": "datagram", "port": 3000, "protocol": "UDP" },
                    { "name": "web", "port": 3000, "protocol": "TCP" }
                ]),
                false,
            ),
        ])
        .unwrap();
        let source = snapshot.source(ToolKind::Grafana);
        assert_eq!(source.matches, 1);
        assert!(source.failure.is_none());
        let identity = source.identity.as_ref().unwrap();
        assert_eq!(identity.found.name, "grafana");
        assert_eq!(identity.found.port, 3000);
        assert_eq!(identity.found.port_name.as_deref(), Some("web"));
    }
}
