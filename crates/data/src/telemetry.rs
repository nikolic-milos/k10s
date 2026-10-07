//! Fixed-range investigations retain Kubernetes UID identity and bounded work.
//!
//! Scope follows controller references, including ReplicaSets and Jobs. A
//! selector narrows the metadata read but never establishes ownership. No
//! Secret, Collector configuration or pod container configuration is decoded.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::{
    Client,
    api::{GetParams, ListParams, Request},
};
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::describe::DescribeRequest;
use crate::discover::KindTarget;
use crate::read::Fetched;

pub const MAX_SCOPE_PODS: usize = 64;
pub const MAX_ACTIVE: usize = 2;
pub const MAX_WAITING: usize = 4;

/// Counters cover this admitted request, including source discovery and probes.
/// Network time includes server waiting and body transport, not just RTT.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestMetrics {
    pub queue_ns: u64,
    pub total_ns: u64,
    pub requests: usize,
    pub bytes: usize,
    pub network_ns: u64,
}
struct Meter {
    started: Instant,
    counts: Cell<RequestMetrics>,
}
tokio::task_local! { static METER: Meter; }
pub fn request_metrics() -> Option<RequestMetrics> {
    METER
        .try_with(|meter| RequestMetrics {
            total_ns: meter.started.elapsed().as_nanos() as u64,
            ..meter.counts.get()
        })
        .ok()
}
pub(crate) fn record_request(network_ns: u64, bytes: usize) {
    let _ = METER.try_with(|meter| {
        let mut counts = meter.counts.get();
        counts.requests += 1;
        counts.bytes += bytes;
        counts.network_ns += network_ns;
        meter.counts.set(counts);
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodIdentity {
    pub name: String,
    pub uid: String,
    pub node: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub namespace: String,
    pub resource_uid: String,
    pub pods: Vec<PodIdentity>,
}

/// Dropping a request releases its queue slot and aborts its in-flight I/O.
#[must_use]
pub struct RequestStop(Option<tokio::task::AbortHandle>);

impl RequestStop {
    pub fn detach(mut self) {
        self.0.take();
    }
}

impl Drop for RequestStop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScheduleCounts {
    pub admitted: usize,
    pub active: usize,
    pub peak_admitted: usize,
    pub peak_active: usize,
    pub rejected: usize,
}

pub(crate) struct Scheduler {
    slots: Arc<Semaphore>,
    active_slots: Arc<Semaphore>,
    admitted: AtomicUsize,
    active: AtomicUsize,
    peak_admitted: AtomicUsize,
    peak_active: AtomicUsize,
    rejected: AtomicUsize,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_ACTIVE + MAX_WAITING)),
            active_slots: Arc::new(Semaphore::new(MAX_ACTIVE)),
            admitted: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak_admitted: AtomicUsize::new(0),
            peak_active: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
        }
    }
}

struct CountGuard<'a>(&'a AtomicUsize);
impl Drop for CountGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Scheduler {
    pub(crate) fn counts(&self) -> ScheduleCounts {
        ScheduleCounts {
            admitted: self.admitted.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
            peak_admitted: self.peak_admitted.load(Ordering::Relaxed),
            peak_active: self.peak_active.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn submit<T: Send + 'static>(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        work: impl Future<Output = Fetched<T>> + Send + 'static,
        reply: impl FnOnce(Fetched<T>) + Send + 'static,
    ) -> RequestStop {
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            reply(Fetched::Failed {
                what: "telemetry",
                why: "telemetry is busy: two requests are active and four are waiting".to_string(),
            });
            return RequestStop(None);
        };
        let scheduler = self.clone();
        let started = Instant::now();
        let task = handle.spawn(async move {
            let _slot = slot;
            let admitted = scheduler.admitted.fetch_add(1, Ordering::Relaxed) + 1;
            scheduler
                .peak_admitted
                .fetch_max(admitted, Ordering::Relaxed);
            let _admitted = CountGuard(&scheduler.admitted);
            let _active_slot = scheduler
                .active_slots
                .clone()
                .acquire_owned()
                .await
                .expect("request scheduler remains open");
            let queue_ns = started.elapsed().as_nanos() as u64;
            let active = scheduler.active.fetch_add(1, Ordering::Relaxed) + 1;
            scheduler.peak_active.fetch_max(active, Ordering::Relaxed);
            let _active = CountGuard(&scheduler.active);
            METER
                .scope(
                    Meter {
                        started,
                        counts: Cell::new(RequestMetrics {
                            queue_ns,
                            ..Default::default()
                        }),
                    },
                    async move {
                        reply(work.await);
                    },
                )
                .await;
        });
        RequestStop(Some(task.abort_handle()))
    }
}

#[derive(Deserialize)]
struct Object {
    metadata: ObjectMeta,
    #[serde(default)]
    spec: Spec,
}

#[derive(Default, Deserialize)]
struct Spec {
    #[serde(default)]
    selector: Option<LabelSelector>,
    #[serde(default, rename = "nodeName")]
    node: String,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    metadata: PageMeta,
    #[serde(deserialize_with = "page_items")]
    items: Vec<Object>,
}

fn page_items<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<Object>, D::Error> {
    Ok(Option::<Vec<Object>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Default, Deserialize)]
struct PageMeta {
    #[serde(default, rename = "continue")]
    next: String,
}

async fn fetch<T: serde::de::DeserializeOwned>(
    client: &Client,
    request: http::Request<Vec<u8>>,
    what: &'static str,
) -> Fetched<T> {
    match crate::reach::bounded_request(client, what, request).await {
        Fetched::Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => Fetched::Ok(value),
            Err(error) => Fetched::Failed {
                what,
                why: format!("{what} returned an unsupported response: {error}"),
            },
        },
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

fn failure(why: impl Into<String>) -> Fetched<Scope> {
    Fetched::Failed {
        what: "workload scope",
        why: why.into(),
    }
}

fn controlled_by(meta: &ObjectMeta, owners: &BTreeSet<String>) -> bool {
    meta.owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|owner| owner.controller == Some(true) && owners.contains(&owner.uid))
}

fn selector_text(selector: &LabelSelector) -> Option<String> {
    let value = serde_json::json!({"spec":{"selector":selector}});
    crate::logs::selector_string(&value)
        .ok()
        .filter(|selector| !selector.is_empty())
}

async fn metadata_page(
    client: &Client,
    path: &str,
    selector: &str,
    what: &'static str,
) -> Fetched<Page> {
    let request = Request::new(path.to_string())
        .list(
            &ListParams::default()
                .labels(selector)
                .limit(MAX_SCOPE_PODS as u32 + 1),
        )
        .expect("fixed list parameters");
    let (mut parts, body) = request.into_parts();
    parts.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static(
            "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1",
        ),
    );
    fetch(client, http::Request::from_parts(parts, body), what).await
}

/// Verify the selected incarnation before naming any telemetry query.
pub async fn resolve_scope(
    client: &Client,
    targets: &[KindTarget],
    request: &DescribeRequest,
) -> Fetched<Scope> {
    let Some(target) = targets.iter().find(|target| target.id == request.kind) else {
        return failure("the selected resource is not served");
    };
    if !matches!(
        target.kind(),
        "Pod" | "Deployment" | "ReplicaSet" | "StatefulSet" | "DaemonSet" | "Job"
    ) {
        return failure(format!(
            "{} telemetry scope is unsupported; select a pod or workload",
            target.kind()
        ));
    }
    let Some(namespace) = request.namespace.as_deref() else {
        return failure("telemetry scope requires a namespace");
    };
    if request.uid.is_empty() {
        return failure("the selection has no Kubernetes UID");
    }
    let path = crate::read::collection_path(target, Some(namespace));
    let mut get = Request::new(path)
        .get(&request.name, &GetParams::default())
        .expect("object GET");
    get.headers_mut().insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    let object: Object = match fetch(client, get, "selected resource").await {
        Fetched::Ok(object) => object,
        Fetched::Denied { what } => return Fetched::Denied { what },
        Fetched::Failed { what, why } => return Fetched::Failed { what, why },
    };
    if object.metadata.uid.as_deref() != Some(&request.uid)
        || object.metadata.namespace.as_deref() != Some(namespace)
        || object.metadata.name.as_deref() != Some(&request.name)
    {
        return failure(
            "the selected resource was recreated or changed identity; select its current namespace, name and UID",
        );
    }
    if target.kind() == "Pod" {
        return Fetched::Ok(Scope {
            namespace: namespace.to_string(),
            resource_uid: request.uid.clone(),
            pods: vec![PodIdentity {
                name: request.name.clone(),
                uid: request.uid.clone(),
                node: object.spec.node,
            }],
        });
    }
    let Some(selector) = object.spec.selector.as_ref().and_then(selector_text) else {
        return failure("the selected workload has no supported pod selector");
    };
    let mut owners = BTreeSet::from([request.uid.clone()]);
    if target.kind() == "Deployment" {
        let path = format!("/apis/apps/v1/namespaces/{namespace}/replicasets");
        let page = match metadata_page(client, &path, &selector, "replicasets").await {
            Fetched::Ok(page) => page,
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { what, why } => return Fetched::Failed { what, why },
        };
        if !page.metadata.next.is_empty() || page.items.len() > MAX_SCOPE_PODS {
            return failure("workload scope exceeds 64 ReplicaSets; no partial ownership is used");
        }
        for item in page.items {
            if item.metadata.namespace.as_deref() == Some(namespace)
                && item
                    .metadata
                    .owner_references
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|owner| owner.controller == Some(true) && owner.uid == request.uid)
                && let Some(uid) = item.metadata.uid
            {
                owners.insert(uid);
            }
        }
    }
    let path = format!("/api/v1/namespaces/{namespace}/pods");
    let page = match metadata_page(client, &path, &selector, "pods").await {
        Fetched::Ok(page) => page,
        Fetched::Denied { what } => return Fetched::Denied { what },
        Fetched::Failed { what, why } => return Fetched::Failed { what, why },
    };
    if !page.metadata.next.is_empty() || page.items.len() > MAX_SCOPE_PODS {
        return failure("workload scope exceeds 64 selected pods; no partial ownership is used");
    }
    let mut pods = Vec::new();
    for item in page.items {
        if item.metadata.namespace.as_deref() != Some(namespace)
            || !controlled_by(&item.metadata, &owners)
        {
            continue;
        }
        if let (Some(name), Some(uid)) = (item.metadata.name, item.metadata.uid) {
            pods.push(PodIdentity {
                name,
                uid,
                node: String::new(),
            });
        }
    }
    pods.sort_by(|a, b| a.name.cmp(&b.name));
    Fetched::Ok(Scope {
        namespace: namespace.to_string(),
        resource_uid: request.uid.clone(),
        pods,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_releases_both_queue_and_active_capacity() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let scheduler = Arc::new(Scheduler::default());
        let replies = Arc::new(AtomicUsize::new(0));
        let mut guards = Vec::new();
        for _ in 0..MAX_ACTIVE + MAX_WAITING {
            let replies = replies.clone();
            guards.push(scheduler.submit(
                runtime.handle(),
                std::future::pending::<Fetched<()>>(),
                move |_| {
                    replies.fetch_add(1, Ordering::Relaxed);
                },
            ));
        }
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while scheduler.counts().admitted != MAX_ACTIVE + MAX_WAITING
                    || scheduler.counts().active != MAX_ACTIVE
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("all admitted requests reach the scheduler");
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let _rejected =
            scheduler.submit(runtime.handle(), async { Fetched::Ok(()) }, move |answer| {
                tx.send(answer).expect("reply");
            });
        assert!(matches!(
            rx.recv().expect("capacity refusal"),
            Fetched::Failed {
                what: "telemetry",
                ..
            }
        ));
        drop(guards);
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while scheduler.counts().admitted != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("aborted work releases every slot");
        });
        assert_eq!(replies.load(Ordering::Relaxed), 0);
        let counts = scheduler.counts();
        assert_eq!(counts.active, 0);
        assert_eq!(counts.peak_active, 2);
        assert_eq!(counts.peak_admitted, 6);
        assert_eq!(counts.rejected, 1);
        let (tx, rx) = std::sync::mpsc::channel();
        let _guard = scheduler.submit(runtime.handle(), async { Fetched::Ok(42) }, move |answer| {
            tx.send((answer, request_metrics())).expect("reply");
        });
        let (answer, metrics) = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("reused capacity");
        assert!(matches!(answer, Fetched::Ok(42)));
        assert!(metrics.expect("per-request meter").queue_ns > 0);
        assert!(request_metrics().is_none());
    }
}
