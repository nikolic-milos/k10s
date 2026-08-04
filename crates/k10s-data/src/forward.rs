//! Managed port-forwards: a bounded registry with a lifecycle, over a seam.
//!
//! The registry owns what a person sees -- open, list, close, at most
//! [`MAX_FORWARDS`] at once, a local-port collision refused by name, a dying
//! forward left visible as a labelled `Dead` state rather than silently
//! vanishing. It never touches a socket: the actual byte-moving lives behind
//! the [`Forwarder`] trait, which is what makes every lifecycle rule testable
//! with no network. The production implementation, [`KubeForwarder`], binds a
//! local listener on 127.0.0.1 and drives each accepted connection through
//! kube's WebSocket port-forward; the scripted API server cannot speak an
//! HTTP upgrade, so that implementation is exercised only against a live
//! cluster and is kept deliberately thin.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
        }
    }
}

// What a person points at: a pod row (forward its first declared port) or a
// service row (resolve through the selector to a pod, and through targetPort
// to the pod's port). The local port mirrors what was asked for -- the pod's
// port or the service's port -- and a collision is the registry's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub namespace: String,
    pub name: String,
    pub service: bool,
}

// How many pods a service resolution will look at to find a running one.
const SERVICE_POD_SCAN: u32 = 10;

pub(crate) async fn resolve(client: &Client, request: &ForwardRequest) -> Fetched<ForwardSpec> {
    if !request.service {
        let api: Api<Pod> = Api::namespaced(client.clone(), &request.namespace);
        let pod = match api.get(&request.name).await {
            Ok(pod) => pod,
            Err(error) => return classify("pod", &error),
        };
        let Some(port) = first_container_port(&pod) else {
            return Fetched::Failed {
                what: "port-forward",
                why: format!(
                    "pod {} declares no containerPort; forward its service or declare one",
                    request.name
                ),
            };
        };
        return Fetched::Ok(ForwardSpec {
            namespace: request.namespace.clone(),
            pod: request.name.clone(),
            local_port: port,
            remote_port: port,
        });
    }

    let api: Api<Service> = Api::namespaced(client.clone(), &request.namespace);
    let service = match api.get(&request.name).await {
        Ok(service) => service,
        Err(error) => return classify("service", &error),
    };
    let spec = service.spec.unwrap_or_default();
    let Some(port) = spec.ports.as_ref().and_then(|ports| ports.first()) else {
        return Fetched::Failed {
            what: "port-forward",
            why: format!("service {} declares no ports", request.name),
        };
    };
    let selector = spec.selector.unwrap_or_default();
    if selector.is_empty() {
        return Fetched::Failed {
            what: "port-forward",
            why: format!(
                "service {} has no selector (headless or external), so there is no pod to \
                 forward to",
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
    let Some(pod) = listed
        .items
        .iter()
        .find(|pod| {
            pod.status
                .as_ref()
                .and_then(|status| status.phase.as_deref())
                == Some("Running")
        })
        .or_else(|| listed.items.first())
    else {
        return Fetched::Failed {
            what: "port-forward",
            why: format!("service {} selects no pods right now", request.name),
        };
    };
    let local_port = match as_port(port.port, "the service port") {
        Ok(local_port) => local_port,
        Err(failed) => return failed,
    };
    let remote_port = match port.target_port.as_ref() {
        None => local_port,
        Some(IntOrString::Int(target)) => match as_port(*target, "the target port") {
            Ok(remote_port) => remote_port,
            Err(failed) => return failed,
        },
        Some(IntOrString::String(name)) => {
            let Some(found) = named_container_port(pod, name) else {
                return Fetched::Failed {
                    what: "port-forward",
                    why: format!(
                        "the selected pod declares no container port named {name:?}, so the \
                         service's targetPort cannot be resolved"
                    ),
                };
            };
            found
        }
    };
    Fetched::Ok(ForwardSpec {
        namespace: request.namespace.clone(),
        pod: pod.metadata.name.clone().unwrap_or_default(),
        local_port,
        remote_port,
    })
}

// A port outside u16 is a malformed object, labelled rather than wrapped.
fn as_port(value: i32, what: &str) -> Result<u16, Fetched<ForwardSpec>> {
    u16::try_from(value).map_err(|_| Fetched::Failed {
        what: "port-forward",
        why: format!("{what} {value} is not a valid TCP port"),
    })
}

fn first_container_port(pod: &Pod) -> Option<u16> {
    pod.spec.as_ref()?.containers.iter().find_map(|container| {
        container
            .ports
            .as_ref()?
            .iter()
            .find_map(|port| u16::try_from(port.container_port).ok())
    })
}

fn named_container_port(pod: &Pod, name: &str) -> Option<u16> {
    pod.spec.as_ref()?.containers.iter().find_map(|container| {
        container.ports.as_ref()?.iter().find_map(|port| {
            (port.name.as_deref() == Some(name))
                .then(|| u16::try_from(port.container_port).ok())
                .flatten()
        })
    })
}

struct Entry {
    row: ForwardRow,
    guard: ForwardGuard,
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
        let id;
        {
            let mut inner = self.inner.lock().expect("forward registry lock");
            if inner.entries.len() >= MAX_FORWARDS {
                return Err(OpenError::Full { max: MAX_FORWARDS });
            }
            // A dead forward no longer binds its port, so only live rows
            // collide.
            if let Some(holder) = inner.entries.iter().find(|e| {
                e.row.spec.local_port == spec.local_port
                    && !matches!(e.row.state, ForwardState::Dead { .. })
            }) {
                return Err(OpenError::PortInUse {
                    local_port: spec.local_port,
                    held_by: format!("{}/{}", holder.row.spec.namespace, holder.row.spec.pod),
                });
            }
            id = inner.next_id;
            inner.next_id += 1;
            inner.entries.push(Entry {
                row: ForwardRow {
                    id,
                    spec: spec.clone(),
                    state: ForwardState::Opening,
                },
                guard: ForwardGuard::noop(),
            });
        }
        // The lock is released around `start`: the forwarder may report
        // synchronously, and its report needs the entry it is about.
        let guard = self.forwarder.start(&spec, {
            let inner = self.inner.clone();
            Box::new(move |event| {
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
                }
            })
        });
        let mut inner = self.inner.lock().expect("forward registry lock");
        match inner.entries.iter_mut().find(|e| e.row.id == id) {
            Some(entry) => {
                entry.guard = guard;
                Ok(entry.row.clone())
            }
            // Closed in the window between the two locks: cancel what was
            // just started by dropping its guard.
            None => Err(OpenError::Full { max: MAX_FORWARDS }),
        }
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

    // True when the id named a forward; dropping its guard cancels it.
    pub fn close(&self, id: u64) -> bool {
        let mut inner = self.inner.lock().expect("forward registry lock");
        let before = inner.entries.len();
        inner.entries.retain(|e| e.row.id != id);
        inner.entries.len() < before
    }
}

// The production side of the seam. Untestable against the scripted API
// server (port-forward is an HTTP upgrade); kept to plumbing: bind, accept,
// one kube port-forward per connection, bytes copied both ways.
pub struct KubeForwarder {
    client: Client,
    handle: tokio::runtime::Handle,
}

impl KubeForwarder {
    pub(crate) fn new(client: Client, handle: tokio::runtime::Handle) -> KubeForwarder {
        KubeForwarder { client, handle }
    }
}

impl Forwarder for KubeForwarder {
    fn start(
        &self,
        spec: &ForwardSpec,
        on_event: Box<dyn Fn(ForwardEvent) + Send + Sync>,
    ) -> ForwardGuard {
        let (cancel_tx, mut cancel) = tokio::sync::oneshot::channel::<()>();
        let client = self.client.clone();
        let spec = spec.clone();
        self.handle.spawn(async move {
            let on_event: Arc<dyn Fn(ForwardEvent) + Send + Sync> = Arc::from(on_event);
            let listener = match tokio::net::TcpListener::bind(("127.0.0.1", spec.local_port)).await
            {
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
            let live_connections = Arc::new(AtomicUsize::new(0));
            loop {
                let accepted = tokio::select! {
                    _ = &mut cancel => return,
                    why = dead.recv() => {
                        on_event(ForwardEvent::Dead(
                            why.unwrap_or_else(|| "the forward stopped".to_string()),
                        ));
                        return;
                    }
                    accepted = listener.accept() => accepted,
                };
                let (mut local, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        on_event(ForwardEvent::Dead(format!("accept failed: {error}")));
                        return;
                    }
                };
                if live_connections.load(Ordering::Acquire) >= MAX_CONNECTIONS_PER_FORWARD {
                    // Refused, not queued: the bound is the contract.
                    continue;
                }
                live_connections.fetch_add(1, Ordering::AcqRel);
                let api = api.clone();
                let spec = spec.clone();
                let dead_tx = dead_tx.clone();
                let live_connections = live_connections.clone();
                tokio::spawn(async move {
                    match api.portforward(&spec.pod, &[spec.remote_port]).await {
                        Ok(mut forwarded) => {
                            if let Some(mut upstream) = forwarded.take_stream(spec.remote_port) {
                                let _ =
                                    tokio::io::copy_bidirectional(&mut local, &mut upstream).await;
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
                    live_connections.fetch_sub(1, Ordering::AcqRel);
                });
            }
        });
        ForwardGuard::new(move || {
            let _ = cancel_tx.send(());
        })
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

    fn spec(pod: &str, local: u16, remote: u16) -> ForwardSpec {
        ForwardSpec {
            namespace: "prod".to_string(),
            pod: pod.to_string(),
            local_port: local,
            remote_port: remote,
        }
    }

    #[test]
    fn open_list_and_close_run_the_lifecycle_and_close_cancels_the_bytes() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));

        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        assert_eq!(row.state, ForwardState::Opening);
        assert_eq!(fake.started(), 1);

        fake.report(0, ForwardEvent::Ready);
        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, ForwardState::Active);
        assert_eq!(listed[0].spec.local_port, 8080);

        assert!(registry.close(row.id));
        assert!(registry.list().is_empty());
        assert_eq!(
            fake.cancelled(),
            vec![spec("api-1", 8080, 80)],
            "closing the row must cancel the forwarder's work"
        );
        assert!(!registry.close(row.id), "a second close finds nothing");
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
    fn a_forward_that_dies_stays_listed_as_a_labelled_state_and_frees_its_port() {
        let fake = FakeForwarder::default();
        let registry = ForwardRegistry::new(Arc::new(fake.clone()));
        let row = registry.open(spec("api-1", 8080, 80)).expect("opens");
        fake.report(0, ForwardEvent::Ready);
        fake.report(0, ForwardEvent::Dead("the pod is gone".to_string()));

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
        assert_eq!(registry.list().len(), MAX_FORWARDS);
    }
}
