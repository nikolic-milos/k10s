//! Live pod and workload usage, polled while an inspector looks at it.
//!
//! Two read paths, one labelled outcome, nothing installed. `metrics.k8s.io`
//! answers when the cluster runs metrics-server; when that group is not
//! served -- or is registered but not answering -- usage falls back to each
//! involved node's kubelet through the API server's node proxy,
//! `/proxy/metrics/resource`, the Prometheus text endpoint every kubelet
//! serves. A 403 on either path is `Denied` and ends the poll: falling back
//! after a denial would route around an administrator's decision. A cluster
//! where neither path is served is `Absent { why }` and also ends the poll --
//! a kind the server does not serve is not retried. A response that will not
//! parse or exceeds its byte cap is `Failed`, and a `Failed` tick keeps
//! polling because the next one may recover. No variant ever carries a zero
//! the cluster did not report.
//!
//! The kubelet reports CPU as a cumulative counter stamped with its own
//! timestamp, so a rate needs two samples: the first fallback tick carries
//! memory only, and a counter without its timestamp never becomes a rate. A
//! tick whose counters have not advanced keeps the last computed rate rather
//! than inventing a fresher one. Requests and limits ride the same poll, read
//! from the pod specs themselves; a percentage is never stored here.
//!
//! Bounded end to end: a workload polls at most [`MAX_POLLED_PODS`] pods
//! (which also bounds the kubelets consulted), the kubelet's answer is capped
//! at [`MAX_KUBELET_BODY_BYTES`] before parsing and each line at
//! [`MAX_METRIC_LINE_BYTES`], and dropping the returned [`UsageStop`] ends
//! the poll at the next await point. Time is bounded too, because a live
//! aggregated API with a dead backend was observed holding a request open
//! forever where `kubectl` gets an instant 503: a source that has not
//! answered within [`CONSULT_DEADLINE`] is treated as not answering -- the
//! same class as 503, which is what the fallback exists for -- and a whole
//! tick is cut off at [`TICK_DEADLINE`] as a labelled failure rather than a
//! panel that never updates. This module refuses to stream, to discover
//! Prometheus, and to speak PromQL: it polls two bounded endpoints while a
//! panel is open, nothing more.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Container, Pod, PodSpec};
use k10s_core::KindId;
use kube::Client;
use kube::api::{Api, GetParams, ListParams, Request};
use serde::Deserialize;

use crate::discover::KindTarget;
use crate::logs::selector_string;
use crate::nodes::{effective_request, parse_bytes, parse_cpu_millis};
use crate::read::{Fetched, classify, collection_path};

/// The cadence the shipped inspector polls at. A caller owns its cadence --
/// tests tick faster -- but the product has exactly one.
pub const USAGE_POLL_INTERVAL: Duration = Duration::from_secs(5);

// A workload wider than this polls its first sixteen pods and says so in the
// sample; the same clamp bounds how many kubelets a fallback tick consults.
pub const MAX_POLLED_PODS: usize = 16;

const CONCURRENT_KUBELET_SCANS: usize = 4;

// How long one source gets to answer. Healthy metrics-servers and kubelets
// answer in well under a second; a source that is slower than this is not
// answering, and the poll must move on rather than hold the panel open-ended.
const CONSULT_DEADLINE: Duration = Duration::from_secs(4);

// The whole tick's ceiling, covering the pod resolution the consults sit on.
// Crossing it is a labelled failure and the next tick tries again.
const TICK_DEADLINE: Duration = Duration::from_secs(15);

// The kubelet's whole answer, checked before parsing. A node's resource
// metrics run a few lines per container; two mebibytes is hundreds of times
// the largest honest answer.
const MAX_KUBELET_BODY_BYTES: usize = 2 << 20;

// One exposition line: a metric name, a label set of DNS-shaped values, a
// float and a timestamp fit in a fraction of this. A longer line is not one
// of ours and is skipped unparsed.
const MAX_METRIC_LINE_BYTES: usize = 1024;

const METRICS_GROUP_PATH: &str = "/apis/metrics.k8s.io/v1beta1";

/// CPU in millicores. The unit is the type; `Display` renders for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Millicores(pub u64);

impl fmt::Display for Millicores {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 1000 {
            return write!(f, "{}m", self.0);
        }
        let cores = format!("{:.2}", self.0 as f64 / 1000.0);
        let cores = cores.trim_end_matches('0').trim_end_matches('.');
        if cores == "1" {
            write!(f, "1 core")
        } else {
            write!(f, "{cores} cores")
        }
    }
}

/// Memory in bytes. The unit is the type; `Display` renders for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(pub u64);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const KI: f64 = 1024.0;
        let b = self.0 as f64;
        if b >= KI * KI * KI {
            write!(f, "{:.1}Gi", b / (KI * KI * KI))
        } else if b >= KI * KI {
            write!(f, "{:.0}Mi", b / (KI * KI))
        } else if b >= KI {
            write!(f, "{:.0}Ki", b / KI)
        } else {
            write!(f, "{}", self.0)
        }
    }
}

impl Millicores {
    // Usage is non-negative by nature; a negative quantity is a wire answer
    // this type refuses rather than wraps.
    fn parse(text: &str) -> Option<Millicores> {
        u64::try_from(parse_cpu_millis(text)?).ok().map(Millicores)
    }
}

impl Bytes {
    fn parse(text: &str) -> Option<Bytes> {
        u64::try_from(parse_bytes(text)?).ok().map(Bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRequest {
    pub namespace: String,
    pub target: UsageTarget,
    pub interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageTarget {
    Pod { name: String },
    Workload { kind: KindId, name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    MetricsServer,
    Kubelet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSample {
    // None is "not measured yet", never zero: the kubelet's first tick has no
    // CPU rate and a pod the source has not scraped has neither number.
    pub cpu: Option<Millicores>,
    pub memory: Option<Bytes>,
    // From the pod specs: the scheduler's effective request (unset counts as
    // zero inside a set that declares any), and a limit that is only a number
    // when every running container carries one -- one uncapped container
    // uncaps its pod, and one uncapped pod uncaps its workload.
    pub cpu_request: Option<Millicores>,
    pub cpu_limit: Option<Millicores>,
    pub memory_request: Option<Bytes>,
    pub memory_limit: Option<Bytes>,
    pub source: UsageSource,
    // How much of the target the numbers cover: a workload mid-rollout can
    // have pods no source has scraped yet, and the display must be able to
    // say "2 of 3 pods" rather than pass a partial sum off as the whole.
    pub pods_measured: usize,
    pub pods_total: usize,
    // More pods matched than MAX_POLLED_PODS; the sample covers the first
    // sixteen and this flag is how the display says so.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageOutcome {
    Usage(UsageSample),
    Denied { what: &'static str },
    Failed { what: &'static str, why: String },
    Absent { why: String },
}

/// Dropping this ends the poll at its next await point.
pub struct UsageStop {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for UsageStop {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

pub(crate) fn poll(
    handle: &tokio::runtime::Handle,
    client: Client,
    targets: Arc<[KindTarget]>,
    request: UsageRequest,
    on_update: Box<dyn Fn(UsageOutcome) + Send + Sync>,
) -> UsageStop {
    let (cancel_tx, mut cancel) = tokio::sync::oneshot::channel::<()>();
    handle.spawn(async move {
        let mut counters: CpuCounters = HashMap::new();
        let mut last: Option<UsageOutcome> = None;
        loop {
            let outcome = tokio::select! {
                _ = &mut cancel => return,
                bounded = tokio::time::timeout(
                    TICK_DEADLINE,
                    tick(&client, &targets, &request, &mut counters),
                ) => bounded.unwrap_or(UsageOutcome::Failed {
                    what: "usage",
                    why: "the cluster did not answer within 15 seconds".to_string(),
                }),
            };
            // Denied and Absent are answers about the cluster, not about the
            // moment: neither is retried, and the label stays on screen.
            let terminal = matches!(
                outcome,
                UsageOutcome::Denied { .. } | UsageOutcome::Absent { .. }
            );
            // A tick that says what the last one said is not news; an idle
            // panel must not repaint over it.
            if last.as_ref() != Some(&outcome) {
                on_update(outcome.clone());
                last = Some(outcome);
            }
            if terminal {
                return;
            }
            tokio::select! {
                _ = &mut cancel => return,
                _ = tokio::time::sleep(request.interval) => {}
            }
        }
    });
    UsageStop {
        cancel: Some(cancel_tx),
    }
}

// Last seen CPU counter per pod: cumulative seconds, the kubelet's timestamp
// for them, and the rate those two last produced -- kept so a tick whose
// counters have not advanced repeats the truth instead of inventing one.
type CpuCounters = HashMap<String, CpuCounter>;

struct CpuCounter {
    seconds: f64,
    stamp_ms: i64,
    rate: Option<Millicores>,
}

async fn tick(
    client: &Client,
    targets: &Arc<[KindTarget]>,
    request: &UsageRequest,
    counters: &mut CpuCounters,
) -> UsageOutcome {
    let resolved = match resolve_pods(client, targets, request).await {
        Ok(resolved) => resolved,
        Err(outcome) => return outcome,
    };

    let specs: Vec<PodSpec> = resolved
        .pods
        .iter()
        .map(|pod| pod.spec.clone().unwrap_or_default())
        .collect();
    let cpu_request = total_request(&specs, "cpu", parse_cpu_millis).map(Millicores);
    let memory_request = total_request(&specs, "memory", parse_bytes).map(Bytes);
    let cpu_limit = total_limit(&specs, "cpu", parse_cpu_millis).map(Millicores);
    let memory_limit = total_limit(&specs, "memory", parse_bytes).map(Bytes);

    let sample = |cpu, memory, source, measured| {
        UsageOutcome::Usage(UsageSample {
            cpu,
            memory,
            cpu_request,
            cpu_limit,
            memory_request,
            memory_limit,
            source,
            pods_measured: measured,
            pods_total: resolved.pods.len(),
            truncated: resolved.truncated,
        })
    };

    match metrics_server_usage(client, request, &resolved).await {
        MetricsAnswer::Measured { cpu, memory, pods } => {
            sample(cpu, memory, UsageSource::MetricsServer, pods)
        }
        MetricsAnswer::Denied => UsageOutcome::Denied {
            what: "pod metrics",
        },
        MetricsAnswer::Failed(why) => UsageOutcome::Failed {
            what: "pod metrics",
            why,
        },
        MetricsAnswer::NotServed => match kubelet_usage(client, &resolved.pods, counters).await {
            KubeletAnswer::Measured { cpu, memory, pods } => {
                sample(cpu, memory, UsageSource::Kubelet, pods)
            }
            KubeletAnswer::Denied => UsageOutcome::Denied {
                what: "node metrics",
            },
            KubeletAnswer::Failed(why) => UsageOutcome::Failed {
                what: "node metrics",
                why,
            },
            KubeletAnswer::NotServed => UsageOutcome::Absent {
                why: "metrics-server is not installed and the kubelet does not serve \
                      resource metrics; usage is hidden"
                    .to_string(),
            },
        },
    }
}

struct Resolved {
    pods: Vec<Pod>,
    truncated: bool,
    // The workload's own selector, verbatim; a pod target has none. Carried
    // so the metrics list asks the same question the pod list answered.
    selector: Option<String>,
}

// The pods the target names right now, clamped, with the overflow flagged.
// Terminated pods are excluded the same way the node table excludes them:
// they hold no requests and no source reports their usage.
async fn resolve_pods(
    client: &Client,
    targets: &Arc<[KindTarget]>,
    request: &UsageRequest,
) -> Result<Resolved, UsageOutcome> {
    let api: Api<Pod> = Api::namespaced(client.clone(), &request.namespace);
    match &request.target {
        UsageTarget::Pod { name } => match api.get(name).await {
            Ok(pod) => Ok(Resolved {
                pods: vec![pod],
                truncated: false,
                selector: None,
            }),
            Err(error) => Err(classified("pod", &error)),
        },
        UsageTarget::Workload { kind, name } => {
            let Some(target) = targets.iter().find(|t| t.id == *kind) else {
                return Err(UsageOutcome::Failed {
                    what: "usage",
                    why: "this kind is not served by the connected cluster".to_string(),
                });
            };
            let http_request = Request::new(collection_path(target, Some(&request.namespace)))
                .get(name, &GetParams::default())
                .map_err(|error| UsageOutcome::Failed {
                    what: "usage",
                    why: error.to_string(),
                })?;
            let workload: serde_json::Value = match client.request(http_request).await {
                Ok(value) => value,
                Err(error) => return Err(classified("workload", &error)),
            };
            let selector = selector_string(&workload).map_err(|why| UsageOutcome::Failed {
                what: "usage",
                why: why.to_string(),
            })?;
            let params = ListParams::default()
                .labels(&selector)
                .fields("status.phase!=Succeeded,status.phase!=Failed")
                .limit(MAX_POLLED_PODS as u32 + 1);
            match api.list(&params).await {
                Ok(list) => {
                    let mut pods = list.items;
                    let truncated = pods.len() > MAX_POLLED_PODS;
                    pods.truncate(MAX_POLLED_PODS);
                    Ok(Resolved {
                        pods,
                        truncated,
                        selector: Some(selector),
                    })
                }
                Err(error) => Err(classified("pods", &error)),
            }
        }
    }
}

fn classified(what: &'static str, error: &kube::Error) -> UsageOutcome {
    match classify::<()>(what, error) {
        Fetched::Denied { what } => UsageOutcome::Denied { what },
        Fetched::Failed { what, why } => UsageOutcome::Failed { what, why },
        Fetched::Ok(()) => unreachable!("classify never returns Ok"),
    }
}

// What metrics.k8s.io said, reduced to the four states the fallback decision
// needs. `NotServed` is the only one that consults the kubelet.
enum MetricsAnswer {
    Measured {
        cpu: Option<Millicores>,
        memory: Option<Bytes>,
        pods: usize,
    },
    NotServed,
    Denied,
    Failed(String),
}

// The fallback-order decision: 404 is "not installed", 503 is "registered but
// not answering" -- both fall through to the kubelet. 403 is an answer and is
// never routed around. Everything else is a failure of this path, not
// evidence about the other one.
fn after_metrics_api(error: &kube::Error) -> MetricsAnswer {
    if let kube::Error::Api(response) = error {
        if response.code == 403 {
            return MetricsAnswer::Denied;
        }
        if response.code == 404 || response.code == 503 {
            return MetricsAnswer::NotServed;
        }
    }
    MetricsAnswer::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

#[derive(Deserialize)]
struct WirePodMetricsList {
    #[serde(default)]
    items: Vec<WirePodMetrics>,
}

#[derive(Deserialize, Default)]
struct WirePodMetrics {
    #[serde(default)]
    metadata: WireName,
    #[serde(default)]
    containers: Vec<WireContainerMetrics>,
}

#[derive(Deserialize, Default)]
struct WireName {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct WireContainerMetrics {
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    cpu: String,
    #[serde(default)]
    memory: String,
}

async fn metrics_server_usage(
    client: &Client,
    request: &UsageRequest,
    resolved: &Resolved,
) -> MetricsAnswer {
    let path = format!(
        "/apis/metrics.k8s.io/v1beta1/namespaces/{}/pods",
        request.namespace
    );
    let items: Vec<WirePodMetrics> = match &request.target {
        UsageTarget::Pod { name } => {
            let http_request = match Request::new(path).get(name, &GetParams::default()) {
                Ok(request) => request,
                Err(error) => return MetricsAnswer::Failed(error.to_string()),
            };
            let answer = tokio::time::timeout(
                CONSULT_DEADLINE,
                client.request::<WirePodMetrics>(http_request),
            )
            .await;
            match answer {
                // Registered but not answering is the 503 class, deadline or
                // status code alike; the kubelet is what answers it.
                Err(_) => return MetricsAnswer::NotServed,
                Ok(Ok(metrics)) => vec![metrics],
                // A single object's 404 is ambiguous: the group may be absent,
                // or metrics-server may simply not have scraped this pod yet.
                // The group document tells the two apart.
                Ok(Err(kube::Error::Api(response))) if response.code == 404 => {
                    return match group_is_served(client).await {
                        GroupAnswer::Served => MetricsAnswer::Measured {
                            cpu: None,
                            memory: None,
                            pods: 0,
                        },
                        GroupAnswer::NotServed => MetricsAnswer::NotServed,
                        GroupAnswer::Denied => MetricsAnswer::Denied,
                        GroupAnswer::Failed(why) => MetricsAnswer::Failed(why),
                    };
                }
                Ok(Err(error)) => return after_metrics_api(&error),
            }
        }
        UsageTarget::Workload { .. } => {
            // The workload's own selector, so the metrics list asks the exact
            // question the pod list answered; the sum below still keys on the
            // resolved set so usage, requests and limits always describe the
            // same pods. A list of an absent group is an unambiguous 404.
            let selector = resolved.selector.as_deref().unwrap_or_default();
            let params = ListParams::default()
                .labels(selector)
                .limit(MAX_POLLED_PODS as u32 + 1);
            let http_request = match Request::new(path).list(&params) {
                Ok(request) => request,
                Err(error) => return MetricsAnswer::Failed(error.to_string()),
            };
            let answer = tokio::time::timeout(
                CONSULT_DEADLINE,
                client.request::<WirePodMetricsList>(http_request),
            )
            .await;
            match answer {
                Err(_) => return MetricsAnswer::NotServed,
                Ok(Ok(list)) => list.items,
                Ok(Err(error)) => return after_metrics_api(&error),
            }
        }
    };

    let mut cpu: Option<u64> = None;
    let mut memory: Option<u64> = None;
    let mut measured = 0usize;
    for pod in &resolved.pods {
        let name = pod.metadata.name.as_deref().unwrap_or_default();
        let Some(item) = items.iter().find(|item| item.metadata.name == name) else {
            continue;
        };
        let mut seen = false;
        for container in &item.containers {
            if let Some(Millicores(millis)) = Millicores::parse(&container.usage.cpu) {
                cpu = Some(cpu.unwrap_or(0).saturating_add(millis));
                seen = true;
            }
            if let Some(Bytes(bytes)) = Bytes::parse(&container.usage.memory) {
                memory = Some(memory.unwrap_or(0).saturating_add(bytes));
                seen = true;
            }
        }
        if seen {
            measured += 1;
        }
    }
    MetricsAnswer::Measured {
        cpu: cpu.map(Millicores),
        memory: memory.map(Bytes),
        pods: measured,
    }
}

enum GroupAnswer {
    Served,
    NotServed,
    Denied,
    Failed(String),
}

async fn group_is_served(client: &Client) -> GroupAnswer {
    let request = match http::Request::get(METRICS_GROUP_PATH).body(Vec::new()) {
        Ok(request) => request,
        Err(error) => return GroupAnswer::Failed(error.to_string()),
    };
    let answer = tokio::time::timeout(
        CONSULT_DEADLINE,
        client.request::<serde_json::Value>(request),
    )
    .await;
    match answer {
        Err(_) => GroupAnswer::NotServed,
        Ok(Ok(_)) => GroupAnswer::Served,
        Ok(Err(kube::Error::Api(response))) if response.code == 404 || response.code == 503 => {
            GroupAnswer::NotServed
        }
        Ok(Err(kube::Error::Api(response))) if response.code == 403 => GroupAnswer::Denied,
        Ok(Err(error)) => GroupAnswer::Failed(crate::connect::describe(
            &error as &(dyn std::error::Error + 'static),
        )),
    }
}

enum KubeletAnswer {
    Measured {
        cpu: Option<Millicores>,
        memory: Option<Bytes>,
        pods: usize,
    },
    NotServed,
    Denied,
    Failed(String),
}

async fn kubelet_usage(client: &Client, pods: &[Pod], counters: &mut CpuCounters) -> KubeletAnswer {
    let mut by_node: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for pod in pods {
        let Some(node) = pod.spec.as_ref().and_then(|spec| spec.node_name.as_deref()) else {
            // A pod no kubelet runs -- pending, unschedulable -- has no usage
            // to fetch, which the measured count reports.
            continue;
        };
        let name = pod.metadata.name.clone().unwrap_or_default();
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();
        by_node
            .entry(node.to_string())
            .or_default()
            .push((namespace, name));
    }
    if by_node.is_empty() {
        return KubeletAnswer::Measured {
            cpu: None,
            memory: None,
            pods: 0,
        };
    }

    // One node's answer, still tied to the pods that wanted it.
    type NodeAnswer = (Vec<(String, String)>, Result<String, NodeFetch>);
    let answers: Vec<NodeAnswer> =
        futures::stream::iter(by_node.into_iter().map(|(node, wanted)| {
            let client = client.clone();
            async move { (wanted, node_resource_metrics(&client, &node).await) }
        }))
        .buffered(CONCURRENT_KUBELET_SCANS)
        .collect()
        .await;

    let mut nodes_served = 0usize;
    let mut samples: HashMap<String, KubeletSample> = HashMap::new();
    for (wanted, answer) in answers {
        match answer {
            Ok(text) => {
                let parsed = match parse_resource_metrics(&text) {
                    Ok(parsed) => parsed,
                    Err(why) => return KubeletAnswer::Failed(why.to_string()),
                };
                nodes_served += 1;
                for (namespace, name) in wanted {
                    if let Some(sample) = parsed.get(&(namespace, name.clone())) {
                        samples.insert(name, sample.clone());
                    }
                }
            }
            Err(NodeFetch::Denied) => return KubeletAnswer::Denied,
            Err(NodeFetch::NotServed) => {}
            Err(NodeFetch::Failed(why)) => return KubeletAnswer::Failed(why),
        }
    }
    if nodes_served == 0 {
        return KubeletAnswer::NotServed;
    }

    let mut memory: Option<u64> = None;
    for sample in samples.values() {
        if let Some(bytes) = sample.memory_bytes {
            memory = Some(memory.unwrap_or(0).saturating_add(bytes));
        }
    }
    let cpu = advance_rates(counters, &samples);
    KubeletAnswer::Measured {
        cpu,
        memory: memory.map(Bytes),
        pods: samples.len(),
    }
}

// The rate decision, isolated so it can be tested and mutated on its own.
// Every pod that reported a counter must yield a rate before the sum is one:
// a partial sum would read as the whole workload using less. A counter that
// went backwards means the pod restarted; its history is discarded and it
// waits for its next sample like a new pod. A counter whose timestamp has
// not moved -- the kubelet refreshes its stats on its own clock, not on this
// poll's -- keeps the rate it last produced.
fn advance_rates(
    counters: &mut CpuCounters,
    samples: &HashMap<String, KubeletSample>,
) -> Option<Millicores> {
    counters.retain(|pod, _| samples.contains_key(pod));
    let mut total: u64 = 0;
    let mut complete = !samples.is_empty();
    for (pod, sample) in samples {
        let Some(stamp_ms) = sample.cpu_stamp_ms else {
            complete = false;
            continue;
        };
        let rate = match counters.get(pod) {
            Some(last) if sample.cpu_seconds < last.seconds => None,
            Some(last) if stamp_ms == last.stamp_ms => last.rate,
            Some(last) if stamp_ms > last.stamp_ms => {
                let elapsed = (stamp_ms - last.stamp_ms) as f64 / 1000.0;
                let millis = (sample.cpu_seconds - last.seconds) / elapsed * 1000.0;
                Some(Millicores(millis.round() as u64))
            }
            _ => None,
        };
        match rate {
            Some(Millicores(millis)) => total = total.saturating_add(millis),
            None => complete = false,
        }
        counters.insert(
            pod.clone(),
            CpuCounter {
                seconds: sample.cpu_seconds,
                stamp_ms,
                rate,
            },
        );
    }
    complete.then_some(Millicores(total))
}

#[derive(Clone)]
struct KubeletSample {
    cpu_seconds: f64,
    cpu_stamp_ms: Option<i64>,
    memory_bytes: Option<u64>,
}

enum NodeFetch {
    NotServed,
    Denied,
    Failed(String),
}

async fn node_resource_metrics(client: &Client, node: &str) -> Result<String, NodeFetch> {
    let path = format!("/api/v1/nodes/{node}/proxy/metrics/resource");
    let request = http::Request::get(path)
        .body(Vec::new())
        .map_err(|error| NodeFetch::Failed(error.to_string()))?;
    let answer = tokio::time::timeout(CONSULT_DEADLINE, client.request_text(request)).await;
    match answer {
        Err(_) => Err(NodeFetch::Failed(
            "the kubelet did not answer within 4 seconds; usage is hidden".to_string(),
        )),
        Ok(Ok(text)) if text.len() > MAX_KUBELET_BODY_BYTES => Err(NodeFetch::Failed(
            "the kubelet's resource metrics exceed 2 MiB; usage is hidden".to_string(),
        )),
        Ok(Ok(text)) => Ok(text),
        Ok(Err(kube::Error::Api(response))) if response.code == 404 => Err(NodeFetch::NotServed),
        Ok(Err(kube::Error::Api(response))) if response.code == 403 => Err(NodeFetch::Denied),
        Ok(Err(error)) => Err(NodeFetch::Failed(crate::connect::describe(
            &error as &(dyn std::error::Error + 'static),
        ))),
    }
}

// The bounded Prometheus text parser: exactly the two pod-level families the
// kubelet's resource endpoint serves, keyed by (namespace, pod). Lines it
// does not recognise are skipped; a body in which nothing is recognisable is
// refused whole, because "no pods" and "not Prometheus text" must not read
// the same. This is not a Prometheus client and must not grow into one.
fn parse_resource_metrics(
    text: &str,
) -> Result<HashMap<(String, String), KubeletSample>, &'static str> {
    let mut out: HashMap<(String, String), KubeletSample> = HashMap::new();
    let mut recognised = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.len() > MAX_METRIC_LINE_BYTES {
            continue;
        }
        if line.starts_with('#') {
            recognised = true;
            continue;
        }
        let Some(((family, labels), rest)) = split_series(line) else {
            continue;
        };
        let Some(value) = rest
            .split_ascii_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
        else {
            continue;
        };
        recognised = true;
        let cpu = family == "pod_cpu_usage_seconds_total";
        let memory = family == "pod_memory_working_set_bytes";
        if (!cpu && !memory) || !value.is_finite() || value < 0.0 {
            continue;
        }
        let (Some(namespace), Some(pod)) = (label_of(labels, "namespace"), label_of(labels, "pod"))
        else {
            continue;
        };
        let stamp_ms = rest
            .split_ascii_whitespace()
            .nth(1)
            .and_then(|t| t.parse::<i64>().ok());
        let entry = out
            .entry((namespace.to_string(), pod.to_string()))
            .or_insert(KubeletSample {
                cpu_seconds: 0.0,
                cpu_stamp_ms: None,
                memory_bytes: None,
            });
        if cpu {
            entry.cpu_seconds = value;
            entry.cpu_stamp_ms = stamp_ms;
        } else {
            entry.memory_bytes = Some(value.round() as u64);
        }
    }
    if !recognised {
        return Err("the kubelet's answer is not Prometheus text; usage is hidden");
    }
    Ok(out)
}

// "name{labels} value [timestamp]" -> ((name, labels), "value [timestamp]").
// A metric name is ASCII letters, digits, underscores and colons; anything
// else before the label brace is not exposition and the line is skipped.
fn split_series(line: &str) -> Option<((&str, &str), &str)> {
    let (family, labels, rest) = match line.split_once('{') {
        Some((family, tail)) => {
            let (labels, rest) = tail.split_once('}')?;
            (family, labels, rest.trim_start())
        }
        None => {
            let (family, rest) = line.split_once(' ')?;
            (family, "", rest.trim_start())
        }
    };
    let named = !family.is_empty()
        && family
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
    named.then_some(((family, labels), rest))
}

fn label_of<'a>(labels: &'a str, key: &str) -> Option<&'a str> {
    labels.split(',').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name.trim() == key).then(|| value.trim().trim_matches('"'))
    })
}

// The effective request over a set of pods. Unset counts as zero inside a set
// that declares any -- that is what the scheduler reserves -- but a set in
// which nothing declares the resource has no request at all, and None is that
// answer; zero would be an invented number.
fn total_request(specs: &[PodSpec], key: &str, parse: fn(&str) -> Option<i64>) -> Option<u64> {
    let declared = specs.iter().any(|spec| {
        all_running_containers(spec).any(|container| {
            container
                .resources
                .as_ref()
                .and_then(|resources| resources.requests.as_ref())
                .is_some_and(|requests| requests.contains_key(key))
        })
    });
    declared.then(|| {
        specs
            .iter()
            .map(|spec| u64::try_from(effective_request(spec, key, parse)).unwrap_or(0))
            .fold(0u64, u64::saturating_add)
    })
}

// A limit is a certainty, not a floor: one uncapped container uncaps its pod
// and one uncapped pod uncaps the set, so the sum is only a number when every
// running container carries one. Init containers that run to completion are
// excluded -- a limit that stopped binding when its container exited must not
// cap the number usage is compared against.
fn total_limit(specs: &[PodSpec], key: &str, parse: fn(&str) -> Option<i64>) -> Option<u64> {
    if specs.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    for spec in specs {
        for container in all_running_containers(spec) {
            let limit = container
                .resources
                .as_ref()
                .and_then(|resources| resources.limits.as_ref())
                .and_then(|limits| limits.get(key))
                .and_then(|quantity| parse(&quantity.0))?;
            total = total.saturating_add(u64::try_from(limit).unwrap_or(0));
        }
    }
    Some(total)
}

// App containers plus restartable init containers (sidecars): the set that is
// still running once the pod is, which is the set usage describes.
fn all_running_containers(spec: &PodSpec) -> impl Iterator<Item = &Container> {
    spec.containers.iter().chain(
        spec.init_containers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|container| container.restart_policy.as_deref() == Some("Always")),
    )
}

#[cfg(test)]
#[path = "metrics_test.rs"]
mod tests;
