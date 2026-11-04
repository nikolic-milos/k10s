//! Managed port-forwards: a bounded registry with a lifecycle, over a seam.
//!
//! The registry owns what a person sees -- open, list, close, at most
//! [`MAX_FORWARDS`] at once, a local-port collision refused by name, a dying
//! forward left visible as a labelled `Dead` state rather than silently
//! vanishing. It never touches a socket: the actual byte-moving lives behind
//! the [`Forwarder`] trait, which is what makes every lifecycle rule testable
//! with no network. The production implementation, [`KubeForwarder`], binds a
//! local listener on 127.0.0.1 and drives each accepted connection through
//! kube's WebSocket port-forward. A loopback test with a pending client proves
//! active connection cancellation; live-cluster tests prove the HTTP upgrade
//! and byte transfer that the scripted API server cannot speak.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Client;
use kube::api::{Api, ListParams};

use crate::read::{Fetched, classify};

// More simultaneous forwards than anyone manages by hand; the bound exists
// so a scripted caller cannot grow the registry without limit.
pub const MAX_FORWARDS: usize = 32;
// Connections multiplex over one forward (a browser opens several); past
// this, new connections are refused rather than queued without bound.
pub const MAX_CONNECTIONS_PER_FORWARD: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardSpec {
    pub namespace: String,
    pub pod: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub expected_service: Option<Arc<k10s_core::ServiceIdentity>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardState {
    Opening,
    Active,
    // The labelled terminal state: the row stays listed with its reason
    // until the user closes it, because a forward that disappears silently
    // reads as one that still works.
    Dead { why: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRow {
    pub id: u64,
    pub spec: ForwardSpec,
    pub state: ForwardState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardStart {
    Created(ForwardRow),
    Reused(ForwardRow),
}

/// One pending browser handoff. The last discarded reservation closes a row
/// unless a handoff or an explicit managed open has retained it.
#[must_use]
pub struct ForwardReservation {
    row: ForwardRow,
    registry: ForwardRegistry,
    pending: bool,
}

impl ForwardReservation {
    pub fn row(&self) -> &ForwardRow {
        &self.row
    }

    pub fn keep(mut self) {
        self.registry.release_reservation(self.row.id, true);
        self.pending = false;
    }
}

impl std::fmt::Debug for ForwardReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardReservation")
            .field("row", &self.row)
            .finish()
    }
}

impl Drop for ForwardReservation {
    fn drop(&mut self) {
        if self.pending {
            self.registry.release_reservation(self.row.id, false);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardTerminal {
    Dead(String),
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardEvent {
    Ready,
    Dead(String),
}

// Dropping the guard cancels the forward; the implementation decides what
// that means (the kube one closes the listener and every connection task).
pub struct ForwardGuard(Option<Box<dyn FnOnce() + Send>>);

impl ForwardGuard {
    pub fn new(cancel: impl FnOnce() + Send + 'static) -> ForwardGuard {
        ForwardGuard(Some(Box::new(cancel)))
    }

    pub fn noop() -> ForwardGuard {
        ForwardGuard(None)
    }
}

impl Drop for ForwardGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel();
        }
    }
}

/// Cancels an unfinished browser-forward resolution without closing other forwards.
#[must_use]
pub struct ForwardOpenStop {
    cancelled: Arc<AtomicBool>,
    task: tokio::task::AbortHandle,
}

impl ForwardOpenStop {
    pub(crate) fn new(cancelled: Arc<AtomicBool>, task: tokio::task::AbortHandle) -> Self {
        Self { cancelled, task }
    }
}

impl Drop for ForwardOpenStop {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.task.abort();
    }
}

// The seam between lifecycle and bytes. `on_event` may fire from any thread
// (or synchronously from `start`); the registry treats it as the only truth
// about the forward's health.
pub trait Forwarder: Send + Sync {
    fn start(
        &self,
        spec: &ForwardSpec,
        on_event: Box<dyn Fn(ForwardEvent) + Send + Sync>,
    ) -> ForwardGuard;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    PortInUse { local_port: u16, held_by: String },
    Full { max: usize },
    IdsExhausted,
    Closed,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::PortInUse {
                local_port,
                held_by,
            } => write!(
                f,
                "local port {local_port} is already forwarding to {held_by}; close that forward first"
            ),
            OpenError::Full { max } => {
                write!(f, "{max} forwards are already open; close one first")
            }
            OpenError::IdsExhausted => write!(f, "forward identifiers are exhausted"),
            OpenError::Closed => write!(f, "this forward was closed while it was opening"),
        }
    }
}

// What a person points at: a pod row (forward its first valid TCP port) or a
// service row (resolve through the selector to a pod, and through targetPort
// to the pod's port). The local port mirrors what was asked for -- the pod's
// port or the service's port -- and a collision is the registry's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub namespace: String,
    pub name: String,
    pub service: bool,
    pub service_port: Option<u16>,
    pub expected_service: Option<Arc<k10s_core::ServiceIdentity>>,
}

// How many pods a service resolution will look at to find a running one.
const SERVICE_POD_SCAN: u32 = 10;

pub(crate) async fn resolve(client: &Client, request: &ForwardRequest) -> Fetched<ForwardSpec> {
    if let Some(expected) = &request.expected_service
        && (!request.service
            || request.namespace != expected.namespace
            || request.name != expected.name
            || request.service_port != Some(expected.port)
            || expected.port == 0
            || expected.uid.is_empty()
            || expected.resource_version.is_empty())
    {
        return Fetched::Failed {
            what: "service",
            why: "the forward request does not match its reviewed Service identity".into(),
        };
    }
    if !request.service {
        if request.service_port.is_some() {
            return Fetched::Failed {
                what: "port-forward",
                why: "a service port cannot be selected for a pod forward".to_string(),
            };
        }
        let api: Api<Pod> = Api::namespaced(client.clone(), &request.namespace);
        let pod = match api.get(&request.name).await {
            Ok(pod) => pod,
            Err(error) => return classify("pod", &error),
        };
        let phase = pod
            .status
            .as_ref()
            .and_then(|status| status.phase.as_deref())
            .filter(|phase| !phase.is_empty());
        if phase != Some("Running") {
            return Fetched::Failed {
                what: "port-forward",
                why: format!(
                    "pod {} has no confirmed Running phase (phase: {})",
                    request.name,
                    phase.unwrap_or("unknown")
                ),
            };
        }
        let Some(port) = first_container_port(&pod) else {
            return Fetched::Failed {
                what: "port-forward",
                why: format!(
                    "pod {} declares no valid TCP containerPort; forward its service or declare one",
                    request.name
                ),
            };
        };
        return Fetched::Ok(ForwardSpec {
            namespace: request.namespace.clone(),
            pod: request.name.clone(),
            local_port: port,
            remote_port: port,
            expected_service: None,
        });
    }

    let api: Api<Service> = Api::namespaced(client.clone(), &request.namespace);
    let service = match api.get(&request.name).await {
        Ok(service) => service,
        Err(error) => return classify("service", &error),
    };
    if let Some(expected) = &request.expected_service
        && (service.metadata.namespace.as_deref() != Some(expected.namespace.as_str())
            || service.metadata.name.as_deref() != Some(expected.name.as_str())
            || service.metadata.uid.as_deref() != Some(expected.uid.as_str())
            || service.metadata.resource_version.as_deref()
                != Some(expected.resource_version.as_str()))
    {
        return Fetched::Failed {
            what: "service",
            why: "the Service no longer matches the reviewed source; refresh the catalogue before forwarding".into(),
        };
    }
    let spec = service.spec.unwrap_or_default();
    let ports = spec.ports.as_deref().unwrap_or_default();
    let port = match request.service_port {
        Some(selected) if selected != 0 => {
            let port = ports
                .iter()
                .find(|port| port.port == i32::from(selected) && is_tcp(port.protocol.as_deref()));
            if port.is_none() && ports.iter().any(|port| port.port == i32::from(selected)) {
                return Fetched::Failed {
                    what: "port-forward",
                    why: format!(
                        "service {} selected port {selected} is not TCP",
                        request.name
                    ),
                };
            }
            port
        }
        Some(_) => {
            return Fetched::Failed {
                what: "port-forward",
                why: "the selected service port must be nonzero".to_string(),
            };
        }
        None => ports.iter().find(|port| is_tcp(port.protocol.as_deref())),
    };
    let Some(port) = port else {
        return Fetched::Failed {
            what: "port-forward",
            why: match request.service_port {
                Some(port) => format!(
                    "service {} no longer declares selected port {port}",
                    request.name
                ),
                None if ports.is_empty() => format!("service {} declares no ports", request.name),
                None => format!("service {} declares no TCP ports", request.name),
            },
        };
    };
    let local_port = match as_port(port.port, "the service port") {
        Ok(local_port) => local_port,
        Err(failed) => return failed,
    };
    enum TargetPort<'a> {
        Number(u16),
        Name(&'a str),
    }
    // Kubernetes ignores targetPort for a headless Service.
    let target_port = if spec.cluster_ip.as_deref() == Some("None") {
        TargetPort::Number(local_port)
    } else {
        match port.target_port.as_ref() {
            None | Some(IntOrString::Int(0)) => TargetPort::Number(local_port),
            Some(IntOrString::Int(target)) => match as_port(*target, "the target port") {
                Ok(target) => TargetPort::Number(target),
                Err(failed) => return failed,
            },
            Some(IntOrString::String(name)) => TargetPort::Name(name),
        }
    };
    let selector = spec.selector.unwrap_or_default();
    if selector.is_empty() {
        return Fetched::Failed {
            what: "port-forward",
            why: format!(
                "service {} has no selector, so this forward cannot choose a pod",
                request.name
            ),
        };
    }
    let label_selector = {
        let mut pairs: Vec<String> = selector.iter().map(|(k, v)| format!("{k}={v}")).collect();
        pairs.sort();
        pairs.join(",")
    };
    let pods: Api<Pod> = Api::namespaced(client.clone(), &request.namespace);
    let params = ListParams::default()
        .labels(&label_selector)
        .limit(SERVICE_POD_SCAN);
    let listed = match pods.list(&params).await {
        Ok(listed) => listed,
        Err(error) => return classify("pods", &error),
    };
    let Some(pod) = listed.items.iter().find(|pod| {
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref())
            == Some("Running")
    }) else {
        return Fetched::Failed {
            what: "port-forward",
            why: format!(
                "service {} has no Running pod among up to {SERVICE_POD_SCAN} selected pods",
                request.name
            ),
        };
    };
    let Some(pod_name) = pod.metadata.name.clone().filter(|name| !name.is_empty()) else {
        return Fetched::Failed {
            what: "port-forward",
            why: format!(
                "the pod service {} selects carries no name, so there is nothing to forward to",
                request.name
            ),
        };
    };
    let remote_port = match target_port {
        TargetPort::Number(port) => port,
        TargetPort::Name(name) => {
            let Some(found) = named_container_port(pod, name) else {
                return Fetched::Failed {
                    what: "port-forward",
                    why: format!(
                        "the selected pod declares no valid TCP container port named {name:?}, so the \
                         service's targetPort cannot be resolved"
                    ),
                };
            };
            found
        }
    };
    Fetched::Ok(ForwardSpec {
        namespace: request.namespace.clone(),
        pod: pod_name,
        local_port,
        remote_port,
        expected_service: request.expected_service.clone(),
    })
}

// A port outside u16 is a malformed object, labelled rather than wrapped.
fn as_port(value: i32, what: &str) -> Result<u16, Fetched<ForwardSpec>> {
    u16::try_from(value)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| Fetched::Failed {
            what: "port-forward",
            why: format!("{what} {value} is not a valid TCP port"),
        })
}

fn is_tcp(protocol: Option<&str>) -> bool {
    protocol.is_none_or(|protocol| protocol == "TCP")
}

fn first_container_port(pod: &Pod) -> Option<u16> {
    pod.spec.as_ref()?.containers.iter().find_map(|container| {
        container
            .ports
            .as_ref()?
            .iter()
            .filter(|port| is_tcp(port.protocol.as_deref()))
            .find_map(|port| {
                u16::try_from(port.container_port)
                    .ok()
                    .filter(|port| *port != 0)
            })
    })
}

fn named_container_port(pod: &Pod, name: &str) -> Option<u16> {
    pod.spec.as_ref()?.containers.iter().find_map(|container| {
        container.ports.as_ref()?.iter().find_map(|port| {
            (port.name.as_deref() == Some(name) && is_tcp(port.protocol.as_deref()))
                .then(|| {
                    u16::try_from(port.container_port)
                        .ok()
                        .filter(|port| *port != 0)
                })
                .flatten()
        })
    })
}

struct Entry {
    row: ForwardRow,
    guard: ForwardGuard,
    changed: Option<tokio::sync::watch::Sender<()>>,
    retained: bool,
    reservations: usize,
}

#[derive(Default)]
struct Entries {
    next_id: u64,
    entries: Vec<Entry>,
}

#[derive(Clone)]
pub struct ForwardRegistry {
    forwarder: Arc<dyn Forwarder>,
    inner: Arc<Mutex<Entries>>,
}

impl ForwardRegistry {
    pub fn new(forwarder: Arc<dyn Forwarder>) -> ForwardRegistry {
        ForwardRegistry {
            forwarder,
            inner: Arc::new(Mutex::new(Entries::default())),
        }
    }

    pub fn open(&self, spec: ForwardSpec) -> Result<ForwardRow, OpenError> {
        self.open_inner(spec, false, true).map(|(row, _)| row)
    }

    /// Reuse a live row only if it names the exact resolved target.
    pub fn open_or_reuse(&self, spec: ForwardSpec) -> Result<ForwardStart, OpenError> {
        self.open_inner(spec, true, true).map(|(row, created)| {
            if created {
                ForwardStart::Created(row)
            } else {
                ForwardStart::Reused(row)
            }
        })
    }

    /// Acquire a pending claim while choosing or reusing the exact target.
    /// Another cancelled browser request cannot close this claim's listener.
    pub fn reserve(&self, spec: ForwardSpec) -> Result<ForwardReservation, OpenError> {
        self.open_inner(spec, true, false)
            .map(|(row, _)| ForwardReservation {
                row,
                registry: self.clone(),
                pending: true,
            })
    }

    fn open_inner(
        &self,
        spec: ForwardSpec,
        reuse: bool,
        retained: bool,
    ) -> Result<(ForwardRow, bool), OpenError> {
        let id;
        {
            let mut inner = self.inner.lock().expect("forward registry lock");
            if !reuse && inner.entries.len() >= MAX_FORWARDS {
                return Err(OpenError::Full { max: MAX_FORWARDS });
            }
            // A dead forward no longer binds its port, so only live rows
            // collide.
            if let Some(holder) = inner.entries.iter_mut().find(|e| {
                e.row.spec.local_port == spec.local_port
                    && !matches!(e.row.state, ForwardState::Dead { .. })
            }) {
                if reuse && holder.row.spec == spec {
                    if retained {
                        holder.retained = true;
                    } else {
                        holder.reservations += 1;
                    }
                    return Ok((holder.row.clone(), false));
                }
                return Err(OpenError::PortInUse {
                    local_port: spec.local_port,
                    held_by: format!("{}/{}", holder.row.spec.namespace, holder.row.spec.pod),
                });
            }
            if inner.entries.len() >= MAX_FORWARDS {
                return Err(OpenError::Full { max: MAX_FORWARDS });
            }
            id = inner.next_id;
            inner.next_id = inner
                .next_id
                .checked_add(1)
                .ok_or(OpenError::IdsExhausted)?;
            inner.entries.push(Entry {
                row: ForwardRow {
                    id,
                    spec: spec.clone(),
                    state: ForwardState::Opening,
                },
                guard: ForwardGuard::noop(),
                changed: None,
                retained,
                reservations: usize::from(!retained),
            });
        }
        // The lock is released around `start`: the forwarder may report
        // synchronously, and its report needs the entry it is about.
        let guard = self.forwarder.start(&spec, {
            let inner = Arc::downgrade(&self.inner);
            Box::new(move |event| {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let changed = {
                    let mut inner = inner.lock().expect("forward registry lock");
                    if let Some(entry) = inner.entries.iter_mut().find(|e| e.row.id == id) {
                        match event {
                            ForwardEvent::Ready => {
                                if entry.row.state == ForwardState::Opening {
                                    entry.row.state = ForwardState::Active;
                                }
                            }
                            ForwardEvent::Dead(why) => entry.row.state = ForwardState::Dead { why },
                        }
                        entry.changed.clone()
                    } else {
                        None
                    }
                };
                if let Some(changed) = changed {
                    changed.send_replace(());
                }
            })
        });
        let mut inner = self.inner.lock().expect("forward registry lock");
        match inner.entries.iter_mut().find(|e| e.row.id == id) {
            Some(entry) => {
                entry.guard = guard;
                Ok((entry.row.clone(), true))
            }
            // Closed in the window between the two locks: cancel what was
            // just started by dropping its guard.
            None => Err(OpenError::Closed),
        }
    }

    fn release_reservation(&self, id: u64, keep: bool) {
        let removed = {
            let mut inner = self.inner.lock().expect("forward registry lock");
            let Some(index) = inner.entries.iter().position(|entry| entry.row.id == id) else {
                return;
            };
            let entry = &mut inner.entries[index];
            entry.retained |= keep;
            entry.reservations -= 1;
            if entry.reservations == 0 && !entry.retained {
                Some(inner.entries.remove(index))
            } else {
                None
            }
        };
        // Cancellation and waiter notification may re-enter the registry.
        drop(removed);
    }

    pub fn list(&self) -> Vec<ForwardRow> {
        self.inner
            .lock()
            .expect("forward registry lock")
            .entries
            .iter()
            .map(|e| e.row.clone())
            .collect()
    }

    /// A point-in-time check of one local listener's registry state.
    pub fn is_active(&self, id: u64) -> bool {
        self.inner
            .lock()
            .expect("forward registry lock")
            .entries
            .iter()
            .any(|entry| entry.row.id == id && entry.row.state == ForwardState::Active)
    }

    /// Wait for a bound local listener. This does not establish a kubelet
    /// connection or prove that the remote service answers.
    pub async fn wait_ready(&self, id: u64) -> Fetched<ForwardRow> {
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(4);
        match tokio::time::timeout(DEADLINE, self.wait_for_listener(id)).await {
            Ok(Ok(row)) => Fetched::Ok(row),
            Ok(Err(why)) => Fetched::Failed {
                what: "port-forward",
                why,
            },
            Err(_) => Fetched::Failed {
                what: "port-forward",
                why: "the local forward listener did not open within 4 seconds".to_string(),
            },
        }
    }

    async fn wait_for_listener(&self, id: u64) -> Result<ForwardRow, String> {
        let mut changed = {
            let mut inner = self.inner.lock().expect("forward registry lock");
            let entry = inner
                .entries
                .iter_mut()
                .find(|entry| entry.row.id == id)
                .ok_or_else(|| "the forward was closed before its listener opened".to_string())?;
            match &entry.row.state {
                ForwardState::Active => return Ok(entry.row.clone()),
                ForwardState::Dead { why } => return Err(why.clone()),
                ForwardState::Opening => {}
            }
            entry
                .changed
                .get_or_insert_with(|| tokio::sync::watch::channel(()).0)
                .subscribe()
        };
        loop {
            {
                let inner = self.inner.lock().expect("forward registry lock");
                let row = inner
                    .entries
                    .iter()
                    .find(|entry| entry.row.id == id)
                    .map(|entry| &entry.row)
                    .ok_or_else(|| {
                        "the forward was closed before its listener opened".to_string()
                    })?;
                match &row.state {
                    ForwardState::Active => return Ok(row.clone()),
                    ForwardState::Dead { why } => return Err(why.clone()),
                    ForwardState::Opening => {}
                }
            }
            if changed.changed().await.is_err() {
                return Err("the forward was closed before its listener opened".to_string());
            }
        }
    }

    /// Observe a terminal state without polling an active listener.
    pub async fn wait_terminal(&self, id: u64) -> ForwardTerminal {
        let mut changed = {
            let mut inner = self.inner.lock().expect("forward registry lock");
            let Some(entry) = inner.entries.iter_mut().find(|entry| entry.row.id == id) else {
                return ForwardTerminal::Closed;
            };
            if let ForwardState::Dead { why } = &entry.row.state {
                return ForwardTerminal::Dead(why.clone());
            }
            entry
                .changed
                .get_or_insert_with(|| tokio::sync::watch::channel(()).0)
                .subscribe()
        };
        loop {
            {
                let inner = self.inner.lock().expect("forward registry lock");
                let Some(row) = inner
                    .entries
                    .iter()
                    .find(|entry| entry.row.id == id)
                    .map(|entry| &entry.row)
                else {
                    return ForwardTerminal::Closed;
                };
                if let ForwardState::Dead { why } = &row.state {
                    return ForwardTerminal::Dead(why.clone());
                }
            }
            if changed.changed().await.is_err() {
                return ForwardTerminal::Closed;
            }
        }
    }

    // True when the id named a forward; dropping its guard cancels it.
    pub fn close(&self, id: u64) -> bool {
        let removed = {
            let mut inner = self.inner.lock().expect("forward registry lock");
            inner
                .entries
                .iter()
                .position(|entry| entry.row.id == id)
                .map(|index| inner.entries.remove(index))
        };
        let found = removed.is_some();
        drop(removed);
        found
    }
}

// The production side of the seam: bind, accept, one kube port-forward per
// connection, and bytes copied both ways. Actual upgrades need a live cluster.
pub struct KubeForwarder {
    client: Client,
    handle: tokio::runtime::Handle,
}

// Dropping kube's handle detaches its pump, so the connection must abort it.
struct ForwardConnection(kube::api::Portforwarder);

impl Drop for ForwardConnection {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl KubeForwarder {
    pub(crate) fn new(client: Client, handle: tokio::runtime::Handle) -> KubeForwarder {
        KubeForwarder { client, handle }
    }

    fn start_with_listener(
        &self,
        spec: &ForwardSpec,
        on_event: Box<dyn Fn(ForwardEvent) + Send + Sync>,
        listener: impl std::future::Future<Output = std::io::Result<tokio::net::TcpListener>>
        + Send
        + 'static,
    ) -> ForwardGuard {
        let (cancel_tx, mut cancel) = tokio::sync::oneshot::channel::<()>();
        let client = self.client.clone();
        let spec = spec.clone();
        self.handle.spawn(async move {
            let on_event: Arc<dyn Fn(ForwardEvent) + Send + Sync> = Arc::from(on_event);
            let listener = match listener.await {
                Ok(listener) => listener,
                Err(error) => {
                    on_event(ForwardEvent::Dead(format!(
                        "cannot listen on 127.0.0.1:{}: {error}",
                        spec.local_port
                    )));
                    return;
                }
            };
            on_event(ForwardEvent::Ready);
            let api: Api<Pod> = Api::namespaced(client, &spec.namespace);
            let (dead_tx, mut dead) = tokio::sync::mpsc::channel::<String>(1);
            let mut connections: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
            loop {
                let accepted = tokio::select! {
                    biased;
                    _ = &mut cancel => return,
                    why = dead.recv() => {
                        on_event(ForwardEvent::Dead(
                            why.unwrap_or_else(|| "the forward stopped".to_string()),
                        ));
                        return;
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => continue,
                    accepted = listener.accept() => accepted,
                };
                let (mut local, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        on_event(ForwardEvent::Dead(format!("accept failed: {error}")));
                        return;
                    }
                };
                if connections.len() >= MAX_CONNECTIONS_PER_FORWARD {
                    // Refused, not queued: the bound is the contract.
                    continue;
                }
                let api = api.clone();
                let spec = spec.clone();
                let dead_tx = dead_tx.clone();
                connections.spawn(async move {
                    match api.portforward(&spec.pod, &[spec.remote_port]).await {
                        Ok(forwarded) => {
                            let mut forwarded = ForwardConnection(forwarded);
                            let mut upstream = forwarded
                                .0
                                .take_stream(spec.remote_port)
                                .expect("the requested port has a data stream");
                            let error = forwarded
                                .0
                                .take_error(spec.remote_port)
                                .expect("the requested port has an error channel");
                            let copy = tokio::io::copy_bidirectional(&mut local, &mut upstream);
                            tokio::pin!(copy);
                            tokio::select! {
                                biased;
                                why = error => {
                                    if let Some(why) = why {
                                        let _ = dead_tx.try_send(why);
                                    } else {
                                        let _ = copy.await;
                                    }
                                }
                                _ = &mut copy => {}
                            }
                        }
                        // The pod side refusing is the forward dying, not a
                        // connection hiccup: the next connection would fail
                        // the same way.
                        Err(error) => {
                            let _ = dead_tx
                                .try_send(crate::read::classify_text("port-forward", &error));
                        }
                    }
                });
            }
        });
        ForwardGuard::new(move || {
            let _ = cancel_tx.send(());
        })
    }
}

impl Forwarder for KubeForwarder {
    fn start(
        &self,
        spec: &ForwardSpec,
        on_event: Box<dyn Fn(ForwardEvent) + Send + Sync>,
    ) -> ForwardGuard {
        self.start_with_listener(
            spec,
            on_event,
            tokio::net::TcpListener::bind(("127.0.0.1", spec.local_port)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeState {
        started: Vec<ForwardSpec>,
        cancelled: Vec<ForwardSpec>,
        events: Vec<Box<dyn Fn(ForwardEvent) + Send + Sync>>,
    }

    #[derive(Clone, Default)]
    struct FakeForwarder {
        state: Arc<Mutex<FakeState>>,
    }

    impl FakeForwarder {
        fn started(&self) -> usize {
            self.state.lock().unwrap().started.len()
        }

        fn cancelled(&self) -> Vec<ForwardSpec> {
            self.state.lock().unwrap().cancelled.clone()
        }

        // Drives the seam from the test: the nth started forward reports.
        fn report(&self, at: usize, event: ForwardEvent) {
            let state = self.state.lock().unwrap();
            (state.events[at])(event);
        }
    }

    impl Forwarder for FakeForwarder {
        fn start(
            &self,
            spec: &ForwardSpec,
            on_event: Box<dyn Fn(ForwardEvent) + Send + Sync>,
        ) -> ForwardGuard {
            let mut state = self.state.lock().unwrap();
            state.started.push(spec.clone());
            state.events.push(on_event);
            let shared = self.state.clone();
            let spec = spec.clone();
            ForwardGuard::new(move || {
                shared.lock().unwrap().cancelled.push(spec);
            })
        }
    }

    /// A client that reports the port-forward upgrade request and never
    /// answers, so a connection task stays parked while awaiting the kubelet.
    fn hanging_client(reached: tokio::sync::mpsc::UnboundedSender<()>) -> Client {
        Client::new(
            tower::service_fn(move |_: http::Request<kube::client::Body>| {
                let _ = reached.send(());
                std::future::pending::<Result<http::Response<kube::client::Body>, tower::BoxError>>(
                )
            }),
            "prod",
        )
    }

    fn spec(pod: &str, local: u16, remote: u16) -> ForwardSpec {
        ForwardSpec {
            namespace: "prod".to_string(),
            pod: pod.to_string(),
            local_port: local,
            remote_port: remote,
            expected_service: None,
        }
    }

    #[test]
    fn open_list_and_close_run_the_lifecycle_and_close_cancels_the_bytes() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));

        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        assert_eq!(row.state, ForwardState::Opening);
        assert!(!registry.is_active(row.id));
        assert_eq!(fake.started(), 1);

        fake.report(0, ForwardEvent::Ready);
        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, ForwardState::Active);
        assert!(registry.is_active(row.id));
        assert_eq!(listed[0].spec.local_port, 8080);

        assert!(registry.close(row.id));
        assert!(!registry.is_active(row.id));
        assert!(registry.list().is_empty());
        assert_eq!(
            fake.cancelled(),
            vec![spec("api-1", 8080, 80)],
            "closing the row must cancel the forwarder's work"
        );
        assert!(!registry.close(row.id), "a second close finds nothing");
    }

    #[test]
    fn close_and_last_reservation_drop_cancel_outside_the_registry_lock() {
        struct CheckingForwarder {
            registry: Arc<Mutex<Option<std::sync::Weak<Mutex<Entries>>>>>,
        }
        impl Forwarder for CheckingForwarder {
            fn start(
                &self,
                _: &ForwardSpec,
                _: Box<dyn Fn(ForwardEvent) + Send + Sync>,
            ) -> ForwardGuard {
                let registry = self.registry.clone();
                ForwardGuard::new(move || {
                    let registry = registry.lock().unwrap();
                    let inner = registry.as_ref().unwrap().upgrade().unwrap();
                    assert!(
                        inner.try_lock().is_ok(),
                        "the guard may re-enter the registry"
                    );
                })
            }
        }
        let slot = Arc::new(Mutex::new(None));
        let registry = ForwardRegistry::new(Arc::new(CheckingForwarder {
            registry: slot.clone(),
        }));
        *slot.lock().unwrap() = Some(Arc::downgrade(&registry.inner));
        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        assert!(registry.close(row.id));
        let reservation = registry
            .reserve(spec("api-1", 8080, 80))
            .expect("reserves a listener");
        drop(reservation);
        assert!(registry.list().is_empty());
    }

    #[tokio::test]
    async fn listener_waiters_observe_ready_before_and_after_registration() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let first = registry.open(spec("api-1", 8080, 80)).expect("opens");
        let mut waiter = Box::pin(registry.wait_ready(first.id));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        fake.report(0, ForwardEvent::Ready);
        let Fetched::Ok(active) = waiter.await else {
            panic!("the registered waiter sees the listener");
        };
        assert_eq!(active.id, first.id);
        assert_eq!(active.state, ForwardState::Active);

        let second = registry.open(spec("api-2", 8081, 81)).expect("opens");
        fake.report(1, ForwardEvent::Ready);
        let Fetched::Ok(active) = registry.wait_ready(second.id).await else {
            panic!("a listener ready before registration is observed");
        };
        assert_eq!(active.id, second.id);
        assert_eq!(active.state, ForwardState::Active);
        assert!(
            registry
                .inner
                .lock()
                .unwrap()
                .entries
                .iter()
                .find(|entry| entry.row.id == second.id)
                .unwrap()
                .changed
                .is_none()
        );
    }

    #[tokio::test]
    async fn listener_notification_wakes_outside_the_registry_lock() {
        use std::future::Future;
        use std::task::{Context, Wake, Waker};

        struct CheckWake {
            registry: ForwardRegistry,
            called: AtomicBool,
        }
        impl Wake for CheckWake {
            fn wake(self: Arc<Self>) {
                assert!(self.registry.inner.try_lock().is_ok());
                self.called.store(true, Ordering::Release);
            }
        }
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        let signal = Arc::new(CheckWake {
            registry: registry.clone(),
            called: AtomicBool::new(false),
        });
        let waker = Waker::from(signal.clone());
        let mut context = Context::from_waker(&waker);
        let mut waiter = Box::pin(registry.wait_ready(row.id));
        assert!(waiter.as_mut().poll(&mut context).is_pending());
        fake.report(0, ForwardEvent::Ready);
        assert!(signal.called.load(Ordering::Acquire));
        assert!(matches!(waiter.await, Fetched::Ok(_)));
    }

    #[tokio::test]
    async fn listener_wait_fails_on_bind_failure_or_close() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let first = registry.open(spec("api-1", 8080, 80)).expect("opens");
        let mut waiter = Box::pin(registry.wait_ready(first.id));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        fake.report(0, ForwardEvent::Dead("address already in use".into()));
        assert_eq!(
            waiter.await,
            Fetched::Failed {
                what: "port-forward",
                why: "address already in use".into(),
            }
        );

        let second = registry.open(spec("api-2", 8081, 81)).expect("opens");
        let mut waiter = Box::pin(registry.wait_ready(second.id));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        assert!(registry.close(second.id));
        assert_eq!(
            waiter.await,
            Fetched::Failed {
                what: "port-forward",
                why: "the forward was closed before its listener opened".into(),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn listener_wait_has_a_deadline_and_can_be_cancelled_without_closing() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        let mut cancelled = Box::pin(registry.wait_ready(row.id));
        assert!(futures::poll!(cancelled.as_mut()).is_pending());
        drop(cancelled);
        assert_eq!(registry.list()[0].state, ForwardState::Opening);

        let mut expired = Box::pin(registry.wait_ready(row.id));
        assert!(futures::poll!(expired.as_mut()).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        assert_eq!(
            expired.await,
            Fetched::Failed {
                what: "port-forward",
                why: "the local forward listener did not open within 4 seconds".into(),
            }
        );
        assert_eq!(registry.list()[0].state, ForwardState::Opening);
        fake.report(0, ForwardEvent::Ready);
        assert!(matches!(registry.wait_ready(row.id).await, Fetched::Ok(_)));
    }

    #[tokio::test]
    async fn terminal_wait_observes_existing_and_later_failures() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let first = registry.open(spec("api-1", 8080, 80)).expect("opens");
        fake.report(0, ForwardEvent::Dead("bind failed".into()));
        assert_eq!(
            registry.wait_terminal(first.id).await,
            ForwardTerminal::Dead("bind failed".into())
        );
        assert!(registry.inner.lock().unwrap().entries[0].changed.is_none());

        let second = registry.open(spec("api-2", 8081, 81)).expect("opens");
        fake.report(1, ForwardEvent::Ready);
        let mut waiter = Box::pin(registry.wait_terminal(second.id));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        fake.report(1, ForwardEvent::Dead("kubelet denied the upgrade".into()));
        assert_eq!(
            waiter.await,
            ForwardTerminal::Dead("kubelet denied the upgrade".into())
        );
    }

    #[tokio::test]
    async fn terminal_wait_observes_close_and_can_be_cancelled() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake));
        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        let mut cancelled = Box::pin(registry.wait_terminal(row.id));
        assert!(futures::poll!(cancelled.as_mut()).is_pending());
        drop(cancelled);
        assert_eq!(
            registry.inner.lock().unwrap().entries[0]
                .changed
                .as_ref()
                .unwrap()
                .receiver_count(),
            0
        );
        assert_eq!(registry.list()[0].state, ForwardState::Opening);
        let mut waiter = Box::pin(registry.wait_terminal(row.id));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        assert!(registry.close(row.id));
        assert_eq!(waiter.await, ForwardTerminal::Closed);
        assert_eq!(
            registry.wait_terminal(row.id).await,
            ForwardTerminal::Closed
        );
    }

    #[test]
    fn the_last_registry_drop_cancels_with_retained_and_late_callbacks() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let remaining = registry.clone();
        let target = spec("api-1", 8080, 80);
        registry.open(target.clone()).expect("opens");

        drop(registry);
        assert!(
            fake.cancelled().is_empty(),
            "one remaining registry keeps its forward open"
        );
        fake.report(0, ForwardEvent::Ready);
        assert_eq!(remaining.list()[0].state, ForwardState::Active);

        drop(remaining);
        assert_eq!(
            fake.cancelled(),
            vec![target.clone()],
            "retained callbacks cannot keep the last registry's guard alive"
        );
        fake.report(0, ForwardEvent::Ready);
        fake.report(0, ForwardEvent::Dead("a late server sentence".into()));
        assert_eq!(
            fake.cancelled(),
            vec![target],
            "late callbacks return safely without another cancellation"
        );
    }

    #[test]
    fn a_local_port_collision_is_refused_and_names_the_holder() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        registry.open(spec("api-1", 8080, 80)).expect("opens");

        let refused = registry.open(spec("web-1", 8080, 3000));
        assert_eq!(
            refused,
            Err(OpenError::PortInUse {
                local_port: 8080,
                held_by: "prod/api-1".to_string(),
            })
        );
        assert_eq!(fake.started(), 1, "a refused open never reaches the seam");
        assert!(
            refused.unwrap_err().to_string().contains("prod/api-1"),
            "the message says who holds the port"
        );

        registry.open(spec("web-1", 8081, 3000)).expect("opens");
        assert_eq!(registry.list().len(), 2);
    }

    #[test]
    fn reuse_requires_the_exact_live_forward_spec() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let target = spec("api-1", 8080, 80);
        let row = registry.open(target.clone()).expect("opens");
        assert_eq!(
            registry.open_or_reuse(target.clone()),
            Ok(ForwardStart::Reused(row.clone()))
        );
        fake.report(0, ForwardEvent::Ready);
        let active = registry.list()[0].clone();
        assert_eq!(
            registry.open_or_reuse(target.clone()),
            Ok(ForwardStart::Reused(active))
        );
        assert!(matches!(
            registry.open_or_reuse(spec("different-pod", 8080, 80)),
            Err(OpenError::PortInUse { .. })
        ));
        assert!(matches!(
            registry.open_or_reuse(spec("api-1", 8080, 81)),
            Err(OpenError::PortInUse { .. })
        ));
        fake.report(0, ForwardEvent::Dead("listener failed".into()));
        let Ok(ForwardStart::Created(replacement)) = registry.open_or_reuse(target) else {
            panic!("a dead forward cannot be reused");
        };
        assert_ne!(replacement.id, row.id);
        assert_eq!(fake.started(), 2);
    }

    #[test]
    fn simultaneous_exact_requests_register_only_one_forward() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let registry = registry.clone();
            let barrier = barrier.clone();
            tasks.push(std::thread::spawn(move || {
                barrier.wait();
                registry.open_or_reuse(spec("api-1", 8080, 80))
            }));
        }
        barrier.wait();
        let first = tasks.remove(0).join().unwrap().expect("first request");
        let second = tasks.remove(0).join().unwrap().expect("second request");
        let (created, reused) = match (first, second) {
            (ForwardStart::Created(created), ForwardStart::Reused(reused))
            | (ForwardStart::Reused(reused), ForwardStart::Created(created)) => (created, reused),
            other => panic!("exact requests must share one row: {other:?}"),
        };
        assert_eq!(created.id, reused.id);
        assert_eq!(fake.started(), 1);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn discarding_one_pending_request_keeps_the_other_claim_alive() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let target = spec("api-1", 8080, 80);
        let first = registry.reserve(target.clone()).expect("first request");
        let second = registry.reserve(target.clone()).expect("second request");
        assert_eq!(first.row().id, second.row().id);
        let row = second.row().clone();
        assert_eq!(fake.started(), 1);

        drop(first);
        assert_eq!(registry.list(), vec![row]);
        assert!(fake.cancelled().is_empty());

        drop(second);
        assert!(registry.list().is_empty());
        assert_eq!(fake.cancelled(), vec![target]);
    }

    #[test]
    fn either_pending_request_can_retain_the_shared_forward() {
        for keep_first in [false, true] {
            let fake = FakeForwarder::default();
            let registry = ForwardRegistry::new(Arc::new(fake.clone()));
            let target = spec("api-1", 8080, 80);
            let first = registry.reserve(target.clone()).expect("first request");
            let second = registry.reserve(target.clone()).expect("second request");
            let row = first.row().clone();
            let (kept, discarded) = if keep_first {
                (first, second)
            } else {
                (second, first)
            };
            kept.keep();
            drop(discarded);
            assert_eq!(registry.list(), vec![row.clone()]);
            assert!(fake.cancelled().is_empty());
            assert!(registry.close(row.id));
            assert_eq!(fake.cancelled(), vec![target]);
        }
    }

    #[test]
    fn an_explicit_reuse_retains_a_pending_browser_forward() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let target = spec("api-1", 8080, 80);
        let reservation = registry.reserve(target.clone()).expect("browser request");
        let row = reservation.row().clone();
        assert_eq!(
            registry.open_or_reuse(target),
            Ok(ForwardStart::Reused(row.clone()))
        );
        drop(reservation);
        assert_eq!(registry.list(), vec![row]);
        assert!(fake.cancelled().is_empty());
    }

    #[test]
    fn discarding_a_reservation_leaves_an_existing_managed_forward_open() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let target = spec("api-1", 8080, 80);
        let row = registry.open(target.clone()).expect("managed forward");
        let reservation = registry.reserve(target).expect("browser request");
        assert_eq!(reservation.row(), &row);
        drop(reservation);
        assert_eq!(registry.list(), vec![row]);
        assert!(fake.cancelled().is_empty());
    }

    #[test]
    fn an_old_reservation_cannot_retain_or_close_a_replacement_row() {
        for keep_old in [false, true] {
            let fake = FakeForwarder::default();
            let registry = ForwardRegistry::new(Arc::new(fake.clone()));
            let target = spec("api-1", 8080, 80);
            let old = registry.reserve(target.clone()).expect("old request");
            assert!(registry.close(old.row().id));
            let replacement = registry.reserve(target.clone()).expect("replacement");
            assert_ne!(old.row().id, replacement.row().id);
            let row = replacement.row().clone();
            if keep_old {
                old.keep();
            } else {
                drop(old);
            }
            assert_eq!(registry.list(), vec![row]);
            assert_eq!(fake.cancelled(), vec![target.clone()]);
            drop(replacement);
            assert!(registry.list().is_empty());
            assert_eq!(fake.cancelled(), vec![target.clone(), target]);
        }
    }

    #[test]
    fn a_reservation_can_be_shared_before_the_original_start_returns() {
        struct PausedForwarder {
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl Forwarder for PausedForwarder {
            fn start(
                &self,
                _: &ForwardSpec,
                _: Box<dyn Fn(ForwardEvent) + Send + Sync>,
            ) -> ForwardGuard {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                ForwardGuard::noop()
            }
        }
        let (entered, start_entered) = std::sync::mpsc::channel();
        let (release, start_release) = std::sync::mpsc::channel();
        let registry = ForwardRegistry::new(Arc::new(PausedForwarder {
            entered,
            release: Mutex::new(start_release),
        }));
        let first = {
            let registry = registry.clone();
            std::thread::spawn(move || registry.reserve(spec("api-1", 8080, 80)))
        };
        start_entered.recv().unwrap();
        let second = registry
            .reserve(spec("api-1", 8080, 80))
            .expect("the inserted pending entry can be shared");
        let row = second.row().clone();
        release.send(()).unwrap();
        let first = first.join().unwrap().expect("the original start returns");
        assert_eq!(first.row(), &row);
        drop(first);
        assert_eq!(registry.list(), vec![row]);
        drop(second);
        assert!(registry.list().is_empty());
    }

    #[test]
    fn forward_identifiers_are_never_reused_after_exhaustion() {
        let registry = ForwardRegistry::new(Arc::new(FakeForwarder::default()));
        registry.inner.lock().unwrap().next_id = u64::MAX - 1;
        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        assert_eq!(row.id, u64::MAX - 1);
        assert!(registry.close(row.id));
        assert_eq!(
            registry.open(spec("api-2", 8081, 81)),
            Err(OpenError::IdsExhausted)
        );
    }

    #[test]
    fn a_forward_that_dies_stays_listed_as_a_labelled_state_and_frees_its_port() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        fake.report(0, ForwardEvent::Ready);
        fake.report(0, ForwardEvent::Dead("the pod is gone".to_string()));
        assert!(!registry.is_active(row.id));

        let listed = registry.list();
        assert_eq!(
            listed[0].state,
            ForwardState::Dead {
                why: "the pod is gone".to_string()
            },
            "death is shown, not hidden"
        );
        fake.report(0, ForwardEvent::Ready);
        assert!(
            matches!(registry.list()[0].state, ForwardState::Dead { .. }),
            "a late Ready cannot resurrect a dead forward"
        );

        registry
            .open(spec("api-2", 8080, 80))
            .expect("a dead forward no longer holds its local port");
        assert!(registry.close(row.id));
    }

    #[test]
    fn a_forwarder_that_fails_synchronously_is_dead_on_arrival_not_lost() {
        struct StillbornForwarder;
        impl Forwarder for StillbornForwarder {
            fn start(
                &self,
                _: &ForwardSpec,
                on_event: Box<dyn Fn(ForwardEvent) + Send + Sync>,
            ) -> ForwardGuard {
                on_event(ForwardEvent::Dead("cannot listen on 127.0.0.1:80".into()));
                ForwardGuard::noop()
            }
        }
        let registry = ForwardRegistry::new(Arc::new(StillbornForwarder));
        let row = registry.open(spec("api-1", 80, 80)).expect("registered");
        assert!(matches!(
            row.state,
            ForwardState::Dead { .. } | ForwardState::Opening
        ));
        assert!(
            matches!(registry.list()[0].state, ForwardState::Dead { .. }),
            "the synchronous death reached the row"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_a_forward_ends_the_connections_it_was_still_carrying() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("a bound local listener");
        let port = listener.local_addr().expect("a bound address").port();
        let (reached_tx, mut reached) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let forwarder = KubeForwarder::new(
            hanging_client(reached_tx),
            tokio::runtime::Handle::current(),
        );
        let guard = forwarder.start_with_listener(
            &spec("api-1", port, 80),
            Box::new(move |event| {
                let _ = events_tx.send(event);
            }),
            async move { Ok(listener) },
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("the listener binds"),
            Some(ForwardEvent::Ready)
        );

        let mut connection = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the forward is listening");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), reached.recv())
                .await
                .expect("the connection reaches the client and parks there"),
            Some(())
        );

        drop(guard);

        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connection.read(&mut byte),
        )
        .await
        .expect("closing a forward ends its connections rather than orphaning them");
        assert_eq!(
            read.ok(),
            Some(0),
            "the socket the copy task held is closed, not left open behind a cancelled guard"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forward_refuses_connections_past_its_bound_and_cancels_every_parked_connection() {
        use tokio::io::AsyncReadExt;

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("a bound local listener");
            let port = listener.local_addr().expect("a bound address").port();
            let (reached_tx, mut reached) = tokio::sync::mpsc::unbounded_channel();
            let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
            let forwarder = KubeForwarder::new(
                hanging_client(reached_tx),
                tokio::runtime::Handle::current(),
            );
            let guard = forwarder.start_with_listener(
                &spec("api-1", port, 80),
                Box::new(move |event| {
                    let _ = events_tx.send(event);
                }),
                async move { Ok(listener) },
            );
            assert_eq!(events.recv().await, Some(ForwardEvent::Ready));

            let mut connections = Vec::with_capacity(MAX_CONNECTIONS_PER_FORWARD);
            for _ in 0..MAX_CONNECTIONS_PER_FORWARD {
                let connection = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .expect("the forward accepts a connection within its bound");
                assert_eq!(
                    reached.recv().await,
                    Some(()),
                    "each accepted connection reaches the client and parks there"
                );
                connections.push(connection);
            }

            let mut refused = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("the local listener accepts the excess connection");
            let mut byte = [0u8; 1];
            let count = tokio::select! {
                biased;
                request = reached.recv() => {
                    panic!("the excess connection must not reach the client: {request:?}");
                }
                read = refused.read(&mut byte) => read.expect("the excess socket closes"),
            };
            assert_eq!(count, 0, "the excess connection is refused with EOF");
            assert!(matches!(
                reached.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));

            drop(guard);
            for mut connection in connections {
                assert_eq!(
                    connection
                        .read(&mut byte)
                        .await
                        .expect("the parked socket closes"),
                    0,
                    "cancellation closes every connection within the bound"
                );
            }
        })
        .await
        .expect("the connection bound and cancellation finish within their budget");
    }

    #[test]
    fn a_row_closed_while_it_opens_is_told_apart_from_a_full_registry() {
        #[derive(Default)]
        struct ClosingForwarder {
            registry: Mutex<Option<ForwardRegistry>>,
        }
        impl Forwarder for ClosingForwarder {
            fn start(
                &self,
                _: &ForwardSpec,
                _: Box<dyn Fn(ForwardEvent) + Send + Sync>,
            ) -> ForwardGuard {
                let registry =
                    self.registry.lock().expect("test lock").clone().expect(
                        "the registry releases its lock around start, so a close lands here",
                    );
                for row in registry.list() {
                    assert!(registry.close(row.id));
                }
                ForwardGuard::noop()
            }
        }

        let forwarder = Arc::new(ClosingForwarder::default());
        let registry = ForwardRegistry::new(forwarder.clone());
        *forwarder.registry.lock().expect("test lock") = Some(registry.clone());

        assert_eq!(
            registry.open(spec("api-1", 8080, 80)),
            Err(OpenError::Closed),
            "a registry with one row in it is not full, and saying so would \
             stop a caller retrying for the wrong reason"
        );
        assert!(registry.list().is_empty());
    }

    #[test]
    fn the_registry_is_bounded_and_says_so() {
        let registry = ForwardRegistry::new(Arc::new(FakeForwarder::default()));
        for i in 0..MAX_FORWARDS {
            registry
                .open(spec(&format!("pod-{i}"), 9000 + i as u16, 80))
                .expect("under the bound");
        }
        assert_eq!(
            registry.open(spec("one-too-many", 9999, 80)),
            Err(OpenError::Full { max: MAX_FORWARDS })
        );
        assert!(matches!(
            registry.open_or_reuse(spec("pod-0", 9000, 80)),
            Ok(ForwardStart::Reused(_))
        ));
        assert_eq!(
            registry.open(spec("pod-0", 9000, 80)),
            Err(OpenError::Full { max: MAX_FORWARDS })
        );
        assert_eq!(registry.list().len(), MAX_FORWARDS);
    }
    #[test]
    fn a_forward_from_another_service_origin_is_never_reused() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let source = Arc::new(k10s_core::ServiceIdentity {
            namespace: "prod".into(),
            name: "grafana".into(),
            port: 3000,
            uid: "grafana-a".into(),
            resource_version: "1".into(),
        });
        let mut original = spec("grafana-pod", 3000, 3000);
        original.expected_service = Some(source.clone());
        let opened = registry
            .open(original.clone())
            .expect("the original retained forward");
        let mut equivalent = original.clone();
        equivalent.expected_service = Some(Arc::new(source.as_ref().clone()));
        let reused = registry
            .reserve(equivalent)
            .expect("the same reviewed origin reuses its forward");
        assert_eq!(reused.row().id, opened.id);
        drop(reused);
        for identity in [
            k10s_core::ServiceIdentity {
                uid: "grafana-b".into(),
                ..source.as_ref().clone()
            },
            k10s_core::ServiceIdentity {
                resource_version: "2".into(),
                ..source.as_ref().clone()
            },
        ] {
            let mut changed = original.clone();
            changed.expected_service = Some(Arc::new(identity));
            assert!(matches!(
                registry.reserve(changed),
                Err(OpenError::PortInUse {
                    local_port: 3000,
                    ..
                })
            ));
            assert_eq!(registry.list(), vec![opened.clone()]);
        }
        assert_eq!(fake.started(), 1);
        assert!(registry.close(opened.id));
    }
}
