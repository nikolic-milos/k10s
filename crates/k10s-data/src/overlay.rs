//! Overlay stamps from already-fetched inventories. Paint never calls this.
//!
//! First paint of the cluster does not wait on these functions. A missing
//! adapter is an empty frame with a note, not a hole in the scene. Marks are
//! capped; a prefix of a dump is not the dump, so truncation is named.

use k10s_core::Severity;

use crate::argo;
use crate::flux;
use crate::mesh;
use crate::netpol;
use crate::policy;
use crate::prom;

/// One card's overlay. `uid` joins to a published scene object when present;
/// `namespace` plus `name` is the fallback a snapshot can still resolve.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamp {
    pub uid: String,
    pub namespace: String,
    pub name: String,
    pub tint: Option<Severity>,
    pub samples: Vec<(i64, f64)>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub stamps: Vec<Stamp>,
    pub truncated: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sync,
    Metrics,
    Policy,
    MeshDeclared,
    MeshObserved,
}

impl Frame {
    fn empty(note: impl Into<String>) -> Frame {
        Frame {
            stamps: Vec::new(),
            truncated: false,
            note: Some(note.into()),
        }
    }

    fn of(stamps: Vec<Stamp>, truncated: bool, note: Option<String>) -> Frame {
        let (stamps, capped) = cap(stamps);
        Frame {
            stamps,
            truncated: truncated || capped,
            note,
        }
    }
}

/// Cards the map will stamp. Past this, the rest is named as truncated.
pub const MAX_MARKS: usize = 512;

/// Samples a sparkline keeps after extrema-preserving downsample.
pub const SPARK_POINTS: usize = 32;

/// Default PromQL for the metrics overlay when Grafana has not named one.
/// Cadvisor CPU, summed per pod, so one series joins one cell.
pub const CPU_EXPR: &str = r#"sum by (namespace, pod) (rate(container_cpu_usage_seconds_total{container!="",container!="POD"}[5m]))"#;

/// Istio request rate by source and destination. Cadvisor CPU is not a mesh
/// observation and must not be substituted here.
pub const MESH_EXPR: &str = r#"sum by (source_workload, destination_workload, destination_service, namespace) (rate(istio_requests_total[5m]))"#;

/// Hubble flow rate already sitting in Prometheus. Hubble's own API, relay,
/// and ports are never scraped; this is a PromQL name only.
pub const HUBBLE_EXPR: &str =
    r#"sum by (source, destination) (rate(hubble_flows_processed_total[5m]))"#;

/// Linkerd proxy response rate already sitting in Prometheus.
pub const LINKERD_EXPR: &str = r#"sum by (client, dst, authority) (rate(response_total[5m]))"#;

pub const RANGE_SECS: f64 = 15.0 * 60.0;
pub const STEP: &str = "30s";

fn cap(mut stamps: Vec<Stamp>) -> (Vec<Stamp>, bool) {
    let truncated = stamps.len() > MAX_MARKS;
    stamps.truncate(MAX_MARKS);
    (stamps, truncated)
}

fn downsample(points: &[(i64, f64)]) -> Vec<(i64, f64)> {
    let samples: Vec<Point> = points
        .iter()
        .copied()
        .map(|(t_ms, value)| Point { t_ms, value })
        .collect();
    downsample_points(&samples)
}

struct Point {
    t_ms: i64,
    value: f64,
}

fn downsample_points(samples: &[Point]) -> Vec<(i64, f64)> {
    const MAX: usize = SPARK_POINTS;
    if samples.len() <= MAX {
        return samples
            .iter()
            .filter(|sample| sample.value.is_finite())
            .map(|sample| (sample.t_ms, sample.value))
            .collect();
    }
    let mut finite = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        if sample.value.is_finite() {
            finite.push((index, sample.t_ms, sample.value));
        }
    }
    if finite.len() <= MAX {
        return finite
            .into_iter()
            .map(|(_, t_ms, value)| (t_ms, value))
            .collect();
    }
    if finite.len() < 2 {
        return Vec::new();
    }
    let first = finite[0];
    let last = finite[finite.len() - 1];
    if MAX == 2 {
        return vec![(first.1, first.2), (last.1, last.2)];
    }
    let bucket_count = (MAX - 2) / 2;
    if bucket_count == 0 {
        return vec![(first.1, first.2), (last.1, last.2)];
    }
    #[derive(Clone, Copy, Default)]
    struct Extrema {
        min: Option<(usize, i64, f64)>,
        max: Option<(usize, i64, f64)>,
    }
    let interior = finite.len() - 2;
    let mut buckets = vec![Extrema::default(); bucket_count];
    for (ordinal, &(index, t_ms, value)) in finite.iter().skip(1).take(interior).enumerate() {
        let bucket = (ordinal * bucket_count / interior).min(bucket_count - 1);
        let extrema = &mut buckets[bucket];
        if extrema.min.is_none_or(|(_, _, current)| value < current) {
            extrema.min = Some((index, t_ms, value));
        }
        if extrema.max.is_none_or(|(_, _, current)| value > current) {
            extrema.max = Some((index, t_ms, value));
        }
    }
    let mut kept = Vec::with_capacity(MAX);
    kept.push((first.1, first.2));
    for extrema in buckets {
        match (extrema.min, extrema.max) {
            (Some(min), Some(max)) if min.0 < max.0 => {
                kept.push((min.1, min.2));
                kept.push((max.1, max.2));
            }
            (Some(min), Some(max)) if min.0 > max.0 => {
                kept.push((max.1, max.2));
                kept.push((min.1, min.2));
            }
            (Some(point), _) | (_, Some(point)) => kept.push((point.1, point.2)),
            (None, None) => {}
        }
    }
    kept.push((last.1, last.2));
    kept
}

fn word_tint(word: &str) -> Option<Severity> {
    match word.trim().to_ascii_lowercase().as_str() {
        "synced" | "healthy" | "true" | "ready" | "current" | "pass" => Some(Severity::Ok),
        "outofsync" | "out-of-sync" | "progressing" | "suspended" | "missing" | "false"
        | "degraded" | "warn" | "warning" => Some(Severity::Warn),
        "error" | "err" | "failed" | "unknown" => Some(Severity::Err),
        "" => None,
        _ => Some(Severity::Unknown),
    }
}

/// GitOps desired versus live, from Argo Applications and their recorded objects.
pub fn from_argo(inventory: &argo::Inventory) -> Frame {
    if !inventory.served {
        return Frame::empty("Argo CD is not served by this cluster");
    }
    let mut stamps = Vec::new();
    for app in &inventory.applications {
        if let Some(tint) = word_tint(&app.sync).or_else(|| word_tint(&app.health)) {
            stamps.push(Stamp {
                uid: app.uid.clone(),
                namespace: app.namespace.clone(),
                name: app.name.clone(),
                tint: Some(tint),
                samples: Vec::new(),
                label: Some(app.sync.clone()).filter(|word| !word.is_empty()),
            });
        }
        for resource in &app.resources {
            let Some(tint) = word_tint(&resource.sync).or_else(|| word_tint(&resource.health))
            else {
                continue;
            };
            stamps.push(Stamp {
                uid: resource.uid.clone(),
                namespace: resource.namespace.clone(),
                name: resource.name.clone(),
                tint: Some(tint),
                samples: Vec::new(),
                label: Some(resource.sync.clone()).filter(|word| !word.is_empty()),
            });
        }
    }
    let truncated = inventory.truncated;
    if stamps.is_empty() {
        return Frame::empty("no Argo Applications to colour");
    }
    Frame::of(stamps, truncated, None)
}

/// Flux Ready condition, per object the controllers already published.
pub fn from_flux(inventory: &flux::Inventory) -> Frame {
    if !inventory.served() {
        return Frame::empty("Flux is not served by this cluster");
    }
    let mut stamps = Vec::new();
    let mut truncated = false;
    for set in [
        &inventory.git_repositories,
        &inventory.oci_repositories,
        &inventory.kustomizations,
        &inventory.helm_releases,
    ] {
        match set {
            flux::KindSet::Served {
                items,
                truncated: more,
                ..
            } => {
                truncated |= *more;
                for item in items {
                    let tint = if item.suspended {
                        Some(Severity::Warn)
                    } else {
                        word_tint(&item.ready)
                    };
                    let Some(tint) = tint else {
                        continue;
                    };
                    stamps.push(Stamp {
                        uid: item.uid.clone(),
                        namespace: item.namespace.clone(),
                        name: item.name.clone(),
                        tint: Some(tint),
                        samples: Vec::new(),
                        label: Some(item.ready.clone()).filter(|word| !word.is_empty()),
                    });
                }
            }
            flux::KindSet::Denied | flux::KindSet::NotServed => {}
        }
    }
    if stamps.is_empty() {
        return Frame::empty("no Flux objects to colour");
    }
    Frame::of(stamps, truncated, None)
}

/// PolicyReport findings keyed by resource uid. Pass and skip do not stamp.
/// Namespace and name stay on the stamp so a snapshot that lost the uid can
/// still join the same way Prometheus does.
pub fn from_policy_reports(inventory: &policy::Inventory) -> Frame {
    if !inventory.served {
        return Frame::empty("PolicyReport CRDs are not served by this cluster");
    }
    let stamps: Vec<Stamp> = inventory
        .resource_tints()
        .into_iter()
        .map(|mark| Stamp {
            uid: mark.uid,
            namespace: mark.namespace,
            name: mark.name,
            tint: Some(mark.tint),
            samples: Vec::new(),
            label: None,
        })
        .collect();
    let truncated = inventory.truncated;
    if stamps.is_empty() {
        return Frame {
            stamps,
            truncated,
            note: Some("no failing policy findings".into()),
        };
    }
    Frame::of(stamps, truncated, None)
}

/// NetworkPolicy isolation, not a traffic verdict. Isolated directions Warn;
/// both directions Err. A default-allow pod is not stamped.
pub fn from_netpol(inventory: &netpol::Inventory) -> Frame {
    let mut stamps = Vec::new();
    for pod in inventory.pods() {
        let posture = inventory.declared.pod_posture(pod, 0);
        let tint = match (posture.ingress.isolated, posture.egress.isolated) {
            (false, false) => continue,
            (true, true) => Severity::Err,
            _ => Severity::Warn,
        };
        let label = match (posture.ingress.isolated, posture.egress.isolated) {
            (true, true) => Some("isolated".to_string()),
            (true, false) => Some("ingress isolated".to_string()),
            (false, true) => Some("egress isolated".to_string()),
            (false, false) => None,
        };
        stamps.push(Stamp {
            uid: pod.uid.clone(),
            namespace: pod.namespace.clone(),
            name: pod.name.clone(),
            tint: Some(tint),
            samples: Vec::new(),
            label,
        });
    }
    let truncated = inventory.declared.truncated;
    if stamps.is_empty() {
        return Frame::empty("no NetworkPolicy isolation to colour");
    }
    Frame::of(stamps, truncated, None)
}

/// Prometheus matrix series joined later by namespace/pod labels.
pub fn from_prometheus(result: &prom::QueryResult) -> Frame {
    let mut stamps = Vec::new();
    for series in &result.series {
        let namespace = label(&series.labels, "namespace").unwrap_or_default();
        let name = label(&series.labels, "pod")
            .or_else(|| label(&series.labels, "exported_pod"))
            .unwrap_or_default();
        if namespace.is_empty() && name.is_empty() {
            continue;
        }
        stamps.push(Stamp {
            uid: String::new(),
            namespace,
            name,
            tint: None,
            samples: downsample(&series.points),
            label: None,
        });
    }
    Frame::of(
        stamps,
        result.truncated,
        result
            .truncated
            .then(|| format!("kept {MAX_MARKS} series; the rest were not stamped")),
    )
}

fn label(labels: &[(String, String)], name: &str) -> Option<String> {
    labels
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())
}

/// Declared mesh objects as named stamps. Observed mesh needs series in hand.
pub fn from_mesh_declared(inventory: &mesh::MeshInventory) -> Frame {
    if !inventory.present() {
        return Frame::empty("no Istio or Linkerd group is served");
    }
    let mut stamps = Vec::new();
    for object in &inventory.objects {
        stamps.push(Stamp {
            uid: String::new(),
            namespace: object.namespace.clone(),
            name: object.name.clone(),
            tint: Some(Severity::Ok),
            samples: Vec::new(),
            label: Some(object.kind.as_str().to_string()),
        });
    }
    if stamps.is_empty() {
        return Frame::empty("no mesh objects to colour");
    }
    Frame::of(stamps, inventory.truncated, None)
}

pub fn from_mesh_observed(edges: &[mesh::ObservedReach]) -> Frame {
    if edges.is_empty() {
        return Frame::empty("no Istio, Hubble, or Linkerd request series");
    }
    let mut stamps = Vec::new();
    for edge in edges {
        push_named(&mut stamps, &edge.to, Severity::Ok);
        push_named(&mut stamps, &edge.from, Severity::Warn);
    }
    Frame::of(stamps, false, None)
}

fn push_named(stamps: &mut Vec<Stamp>, named: &str, tint: Severity) {
    if named.is_empty() || named == "*" {
        return;
    }
    let (namespace, name) = match named.split_once('/') {
        Some((namespace, name)) => (namespace.to_string(), name.to_string()),
        None => (String::new(), named.to_string()),
    };
    if stamps
        .iter()
        .any(|stamp| stamp.namespace == namespace && stamp.name == name)
    {
        return;
    }
    stamps.push(Stamp {
        uid: String::new(),
        namespace,
        name,
        tint: Some(tint),
        samples: Vec::new(),
        label: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_downsample_keeps_a_spike_and_joins_by_pod() {
        let mut points: Vec<(i64, f64)> = (0..1_000).map(|t| (t, 1.0)).collect();
        points[400] = (400, 50.0);
        let result = prom::QueryResult {
            result_type: prom::ResultType::Matrix,
            series: vec![prom::Series {
                labels: vec![
                    ("namespace".into(), "prod".into()),
                    ("pod".into(), "api-0".into()),
                ],
                points,
            }],
            truncated: false,
            dropped_series: 0,
        };
        let frame = from_prometheus(&result);
        assert_eq!(frame.stamps.len(), 1);
        assert_eq!(frame.stamps[0].namespace, "prod");
        assert_eq!(frame.stamps[0].name, "api-0");
        assert!(frame.stamps[0].samples.len() <= SPARK_POINTS);
        assert!(
            frame.stamps[0]
                .samples
                .iter()
                .any(|(_, value)| *value == 50.0),
            "extrema survive downsample"
        );
    }

    #[test]
    fn overlay_marks_are_capped() {
        let stamps: Vec<_> = (0..MAX_MARKS + 8)
            .map(|i| Stamp {
                uid: format!("u{i}"),
                namespace: "ns".into(),
                name: format!("p{i}"),
                tint: Some(Severity::Warn),
                samples: Vec::new(),
                label: None,
            })
            .collect();
        let frame = Frame::of(stamps, false, None);
        assert_eq!(frame.stamps.len(), MAX_MARKS);
        assert!(frame.truncated);
    }

    #[test]
    fn hubble_and_linkerd_exprs_are_their_own_queries_not_cadvisor() {
        assert_ne!(MESH_EXPR, HUBBLE_EXPR);
        assert_ne!(MESH_EXPR, LINKERD_EXPR);
        assert_ne!(HUBBLE_EXPR, LINKERD_EXPR);
        assert_ne!(CPU_EXPR, MESH_EXPR);
        assert_ne!(CPU_EXPR, HUBBLE_EXPR);
        assert_ne!(CPU_EXPR, LINKERD_EXPR);
        assert!(MESH_EXPR.contains("istio_requests_total"));
        assert!(HUBBLE_EXPR.contains("hubble_flows_processed_total"));
        assert!(LINKERD_EXPR.contains("response_total"));
        for expr in [MESH_EXPR, HUBBLE_EXPR, LINKERD_EXPR] {
            assert!(
                !expr.contains("container_cpu"),
                "cadvisor CPU is not a mesh observation: {expr}"
            );
        }
    }

    #[test]
    fn policy_report_stamps_keep_the_resource_name() {
        let inventory = policy::Inventory {
            served: true,
            reports: vec![policy::Report {
                namespace: "prod".into(),
                name: "pods".into(),
                results: vec![policy::Finding {
                    policy: "require-labels".into(),
                    result: "fail".into(),
                    severity: Severity::Err,
                    resource_name: "api-0".into(),
                    resource_kind: "Pod".into(),
                    resource_uid: "uid-api".into(),
                }],
            }],
            truncated: false,
        };
        let frame = from_policy_reports(&inventory);
        assert_eq!(frame.stamps.len(), 1);
        assert_eq!(frame.stamps[0].uid, "uid-api");
        assert_eq!(frame.stamps[0].namespace, "prod");
        assert_eq!(frame.stamps[0].name, "api-0");
        assert_eq!(frame.stamps[0].tint, Some(Severity::Err));
    }

    #[test]
    fn cadvisor_cpu_is_not_an_observed_mesh_edge() {
        let labels = [crate::mesh::SeriesLabels {
            name: "container_cpu_usage_seconds_total".into(),
            labels: vec![
                ("namespace".into(), "prod".into()),
                ("pod".into(), "api-0".into()),
                ("source".into(), "api".into()),
                ("destination".into(), "db".into()),
            ],
        }];
        let frame = from_mesh_observed(&crate::mesh::observed_from_series(&labels));
        assert!(frame.stamps.is_empty());
        assert!(
            frame
                .note
                .as_deref()
                .is_some_and(|note| note.contains("no Istio")),
            "{:?}",
            frame.note
        );
    }
}
