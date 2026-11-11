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

/// Dropping a request signals cancellation. Admission remains occupied until
/// the runtime drops the request task, which may happen later.
#[must_use]
pub struct RequestStop(Option<tokio::task::AbortHandle>);

impl RequestStop {
    pub(crate) fn noop() -> Self {
        Self(None)
    }

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

/// Counts are sampled separately and do not form a single atomic snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScheduleCounts {
    pub admitted: usize,
    pub active: usize,
    pub peak_admitted: usize,
    pub peak_active: usize,
    pub rejected: usize,
}

pub(crate) struct Scheduler {
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
            active_slots: Arc::new(Semaphore::new(MAX_ACTIVE)),
            admitted: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak_admitted: AtomicUsize::new(0),
            peak_active: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
        }
    }
}

struct Admission(Arc<Scheduler>);

impl Drop for Admission {
    fn drop(&mut self) {
        self.0.admitted.fetch_sub(1, Ordering::Relaxed);
    }
}

// An unpolled cancelled task drops its captures in an unspecified order.
// Keep the admission last so its work and callback are dropped first.
struct Admitted<W, R> {
    work: W,
    reply: R,
    admission: Admission,
}

impl<W, R> Admitted<W, R> {
    fn into_parts(self) -> (Admission, W, R) {
        (self.admission, self.work, self.reply)
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
        self.submit_inner(handle, None, work, reply)
    }

    /// One deadline covers queue waiting and the complete admitted exchange.
    pub(crate) fn submit_until<T: Send + 'static>(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        deadline: tokio::time::Instant,
        work: impl Future<Output = Fetched<T>> + Send + 'static,
        reply: impl FnOnce(Fetched<T>) + Send + 'static,
    ) -> RequestStop {
        self.submit_inner(handle, Some(deadline), work, reply)
    }

    fn submit_inner<T: Send + 'static>(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        deadline: Option<tokio::time::Instant>,
        work: impl Future<Output = Fetched<T>> + Send + 'static,
        reply: impl FnOnce(Fetched<T>) + Send + 'static,
    ) -> RequestStop {
        let Ok(previous) =
            self.admitted
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    (count < MAX_ACTIVE + MAX_WAITING).then_some(count + 1)
                })
        else {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            reply(Fetched::Failed {
                what: "telemetry",
                why: "telemetry is busy: request capacity is full; try again shortly".to_string(),
            });
            return RequestStop::noop();
        };
        self.peak_admitted
            .fetch_max(previous + 1, Ordering::Relaxed);
        let request = Admitted {
            work,
            reply,
            admission: Admission(self.clone()),
        };
        let started = Instant::now();
        let task = handle.spawn(async move {
            let (admission, work, reply) = request.into_parts();
            let scheduler = &admission.0;
            let acquire = scheduler.active_slots.clone().acquire_owned();
            let _active_slot = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, acquire).await {
                    Ok(slot) => slot.expect("request scheduler remains open"),
                    Err(_) => {
                        reply(request_expired());
                        return;
                    }
                },
                None => acquire.await.expect("request scheduler remains open"),
            };
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                reply(request_expired());
                return;
            }
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
                        let outcome = match deadline {
                            Some(deadline) => tokio::time::timeout_at(deadline, work)
                                .await
                                .unwrap_or_else(|_| request_expired()),
                            None => work.await,
                        };
                        reply(outcome);
                    },
                )
                .await;
        });
        RequestStop(Some(task.abort_handle()))
    }
}

fn request_expired<T>() -> Fetched<T> {
    Fetched::Failed {
        what: "telemetry",
        why: "the request did not finish before its deadline".to_string(),
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

    async fn wait_tasks_finished(handles: &[tokio::task::AbortHandle]) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while handles.iter().any(|handle| !handle.is_finished()) {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("request tasks have stopped");
    }

    #[tokio::test(start_paused = true)]
    async fn an_admitted_deadline_expires_without_polling_work() {
        let scheduler = Arc::new(Scheduler::default());
        let mut active = Vec::new();
        let mut started = Vec::new();
        for _ in 0..MAX_ACTIVE {
            let (tx, rx) = tokio::sync::oneshot::channel();
            active.push(scheduler.submit(
                &tokio::runtime::Handle::current(),
                async move {
                    let _ = tx.send(());
                    std::future::pending::<Fetched<()>>().await
                },
                |_| panic!("cancelled active work must not reply"),
            ));
            started.push(rx);
        }
        for signal in started {
            tokio::time::timeout(std::time::Duration::from_secs(1), signal)
                .await
                .expect("active work reaches the scheduler")
                .expect("active work starts");
        }
        assert_eq!(scheduler.counts().active, MAX_ACTIVE);
        let polled = Arc::new(AtomicUsize::new(0));
        let work_polled = polled.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let waiting = scheduler.submit_until(
            &tokio::runtime::Handle::current(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            async move {
                work_polled.fetch_add(1, Ordering::Relaxed);
                Fetched::Ok(())
            },
            move |answer| tx.send(answer).expect("queued deadline reply"),
        );
        assert_eq!(scheduler.counts().admitted, MAX_ACTIVE + 1);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert_eq!(rx.await.expect("deadline reply"), request_expired());
        let waiting_task = waiting.0.as_ref().expect("admitted task").clone();
        wait_tasks_finished(&[waiting_task]).await;
        assert_eq!(polled.load(Ordering::Relaxed), 0);
        assert_eq!(scheduler.counts().active, MAX_ACTIVE);
        assert_eq!(scheduler.counts().admitted, MAX_ACTIVE);
        let tasks: Vec<_> = active
            .iter()
            .map(|stop| stop.0.as_ref().expect("active task").clone())
            .collect();
        drop(active);
        wait_tasks_finished(&tasks).await;
        assert_eq!(scheduler.counts().admitted, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_guard_cancels_an_admitted_deadline_before_the_runtime_polls_it() {
        let scheduler = Arc::new(Scheduler::default());
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let stop = scheduler.submit_until(
            &tokio::runtime::Handle::current(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(35),
            async { panic!("cancelled work must not be polled") },
            move |answer: Fetched<()>| tx.send(answer).expect("no cancellation reply"),
        );
        let task = stop.0.as_ref().expect("admitted task").clone();
        drop(stop);
        wait_tasks_finished(&[task]).await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert_eq!(scheduler.counts().active, 0);
        assert_eq!(scheduler.counts().admitted, 0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _fresh = scheduler.submit_until(
            &tokio::runtime::Handle::current(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(35),
            async { Fetched::Ok(42) },
            move |answer| tx.send(answer).expect("fresh reply"),
        );
        assert_eq!(rx.await.expect("fresh capacity"), Fetched::Ok(42));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_unpolled_requests_can_temporarily_fill_capacity() {
        let scheduler = Arc::new(Scheduler::default());
        let handle = tokio::runtime::Handle::current();
        let mut completions = Vec::new();
        let mut tasks = Vec::new();
        for _ in 0..MAX_ACTIVE + MAX_WAITING {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let stop = scheduler.submit(
                &handle,
                async { panic!("cancelled work must not be polled") },
                move |_: Fetched<()>| {
                    let _ = tx.send(());
                    panic!("cancelled work must not reply");
                },
            );
            completions.push(rx);
            tasks.push(stop.0.as_ref().expect("admitted task").clone());
            drop(stop);
        }
        assert_eq!(scheduler.counts().active, 0);
        assert_eq!(scheduler.counts().admitted, MAX_ACTIVE + MAX_WAITING);

        let (tx, rx) = tokio::sync::oneshot::channel();
        let _rejected = scheduler.submit(&handle, async { Fetched::Ok(()) }, move |answer| {
            tx.send(answer).expect("capacity reply");
        });
        assert_eq!(
            rx.await.expect("capacity refusal"),
            Fetched::Failed {
                what: "telemetry",
                why: "telemetry is busy: request capacity is full; try again shortly".to_string(),
            }
        );

        for completion in completions {
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(2), completion)
                    .await
                    .expect("cancelled callback is dropped")
                    .is_err()
            );
        }
        wait_tasks_finished(&tasks).await;
        assert_eq!(scheduler.counts().admitted, 0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _fresh = scheduler.submit(&handle, async { Fetched::Ok(42) }, move |answer| {
            tx.send(answer).expect("fresh reply");
        });
        assert_eq!(rx.await.expect("fresh capacity"), Fetched::Ok(42));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_admission_counts_tasks_before_they_are_polled() {
        const ATTEMPTS: usize = 12;
        let scheduler = Arc::new(Scheduler::default());
        let handle = tokio::runtime::Handle::current();
        let barrier = Arc::new(std::sync::Barrier::new(ATTEMPTS + 1));
        let guards = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..ATTEMPTS)
                .map(|_| {
                    let scheduler = scheduler.clone();
                    let handle = handle.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let stop = scheduler.submit(
                            &handle,
                            std::future::pending::<Fetched<()>>(),
                            move |answer| {
                                let _ = tx.send(answer);
                            },
                        );
                        (stop, rx)
                    })
                })
                .collect();
            barrier.wait();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("admission thread"))
                .collect::<Vec<_>>()
        });
        let (guards, completions): (Vec<_>, Vec<_>) = guards.into_iter().unzip();
        let counts = scheduler.counts();
        assert_eq!(counts.admitted, MAX_ACTIVE + MAX_WAITING);
        assert_eq!(counts.peak_admitted, MAX_ACTIVE + MAX_WAITING);
        assert_eq!(counts.active, 0);
        assert_eq!(counts.rejected, ATTEMPTS - MAX_ACTIVE - MAX_WAITING);

        let tasks: Vec<_> = guards.iter().filter_map(|stop| stop.0.clone()).collect();
        drop(guards);
        assert_eq!(scheduler.counts().admitted, MAX_ACTIVE + MAX_WAITING);
        for completion in completions {
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), completion)
                .await
                .expect("request callback is settled");
            assert!(matches!(
                result,
                Err(_)
                    | Ok(Fetched::Failed {
                        what: "telemetry",
                        ..
                    })
            ));
        }
        wait_tasks_finished(&tasks).await;
        assert_eq!(scheduler.counts().admitted, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn admission_outlives_cancelled_work_and_callback() {
        struct Probe {
            scheduler: Arc<Scheduler>,
            sender: tokio::sync::mpsc::UnboundedSender<(usize, ScheduleCounts, usize)>,
            index: usize,
        }

        impl Drop for Probe {
            fn drop(&mut self) {
                let _ = self.sender.send((
                    self.index,
                    self.scheduler.counts(),
                    self.scheduler.active_slots.available_permits(),
                ));
            }
        }

        for poll_work in [false, true] {
            let scheduler = Arc::new(Scheduler::default());
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let work_probe = Probe {
                scheduler: scheduler.clone(),
                sender: tx.clone(),
                index: 0,
            };
            let reply_probe = Probe {
                scheduler: scheduler.clone(),
                sender: tx,
                index: 1,
            };
            let (started, work_started) = tokio::sync::oneshot::channel();
            let stop = scheduler.submit(
                &tokio::runtime::Handle::current(),
                async move {
                    let _probe = work_probe;
                    let _ = started.send(());
                    std::future::pending::<Fetched<()>>().await
                },
                move |_: Fetched<()>| {
                    let _probe = reply_probe;
                    panic!("cancelled work must not reply");
                },
            );
            if poll_work {
                tokio::time::timeout(std::time::Duration::from_secs(2), work_started)
                    .await
                    .expect("work is polled")
                    .expect("work started");
            }
            assert_eq!(scheduler.counts().admitted, 1);
            let task = stop.0.as_ref().expect("admitted task").clone();
            drop(stop);

            let mut observed = [false; 2];
            for _ in 0..observed.len() {
                let (index, counts, permits) =
                    tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                        .await
                        .expect("capture is dropped")
                        .expect("probe message");
                assert!(!observed[index]);
                observed[index] = true;
                assert_eq!(counts.admitted, 1);
                assert_eq!(counts.active, usize::from(poll_work));
                assert_eq!(permits, MAX_ACTIVE - usize::from(poll_work));
            }
            assert_eq!(observed, [true, true]);
            wait_tasks_finished(&[task]).await;
            assert_eq!(scheduler.counts().admitted, 0);
        }
    }

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
