use std::sync::Arc;

use crate::ClusterSpec;
use k10s_core::{
    Capability, IngestEvent, KindId, Op, Payload, PreparedNamespace, PreparedPod, PreparedSat,
    PreparedScene, PreparedWorkload, ResourceEvent,
};

fn indexed_uid(prefix: &str, mut index: usize) -> Arc<str> {
    // Four prefix bytes plus twenty decimal digits cover every 64-bit usize.
    // Formatting into this stack buffer avoids allocating a temporary String
    // immediately before the final Arc allocation -- one avoided allocation
    // for nearly every object in a generated scene.
    let mut bytes = [0u8; 24];
    let mut cursor = bytes.len();
    loop {
        cursor -= 1;
        bytes[cursor] = b'0' + (index % 10) as u8;
        index /= 10;
        if index == 0 {
            break;
        }
    }
    let digits = bytes.len() - cursor;
    bytes.copy_within(cursor.., prefix.len());
    bytes[..prefix.len()].copy_from_slice(prefix.as_bytes());
    let uid = std::str::from_utf8(&bytes[..prefix.len() + digits])
        .expect("a fixed ASCII prefix and decimal digits are UTF-8");
    Arc::from(uid)
}

pub fn scope_uid(ns: usize) -> Arc<str> {
    indexed_uid("ns-", ns)
}

pub fn owner_uid(wl: usize) -> Arc<str> {
    indexed_uid("wl-", wl)
}

pub fn instance_uid(pod: usize) -> Arc<str> {
    indexed_uid("pod-", pod)
}

pub fn attachment_uid(sat: usize) -> Arc<str> {
    indexed_uid("sat-", sat)
}

pub fn snapshot(spec: &ClusterSpec, with_attachments: bool) -> Vec<IngestEvent> {
    let mut out = Vec::new();
    emit_snapshot(spec, with_attachments, &mut out);
    out
}

struct SnapshotPlan {
    ns_first_wl: Vec<usize>,
    cross_deps: Vec<(u32, u32)>,
}

impl SnapshotPlan {
    fn new(spec: &ClusterSpec) -> Self {
        let mut ns_first_wl = Vec::with_capacity(spec.namespaces.len());
        let mut workloads = 0usize;
        for ns in &spec.namespaces {
            ns_first_wl.push(workloads);
            workloads += ns.workloads.len();
        }

        let mut cross_deps = spec.cross_deps.clone();
        // Stable within each source so the event and prepared contracts retain
        // generation order. There are at most 64 generated cross edges; a
        // sorted sparse list avoids constructing one Vec header per workload
        // (124k empty vectors in the million-object platform scene).
        cross_deps.sort_by_key(|&(from, _)| from);
        Self {
            ns_first_wl,
            cross_deps,
        }
    }

    fn dependencies(&self, namespace: usize, workload: usize, local: &[u32]) -> Vec<Arc<str>> {
        let workload = workload as u32;
        let start = self
            .cross_deps
            .partition_point(|&(from, _)| from < workload);
        let end = self.cross_deps[start..].partition_point(|&(from, _)| from == workload) + start;
        let mut dependencies = Vec::with_capacity(local.len() + end - start);
        dependencies.extend(
            local
                .iter()
                .map(|&target| owner_uid(self.ns_first_wl[namespace] + target as usize)),
        );
        dependencies.extend(
            self.cross_deps[start..end]
                .iter()
                .map(|&(_, target)| owner_uid(target as usize)),
        );
        dependencies
    }
}

/// Convert a synthetic cluster directly into the world's hierarchical batch
/// contract.
///
/// Consuming the spec makes the ownership boundary explicit and avoids the
/// event-per-object flatten/fold round trip. Real cluster snapshots still use
/// `snapshot`, because events are their native contract.
pub fn prepared(spec: ClusterSpec, with_attachments: bool) -> PreparedScene {
    let plan = SnapshotPlan::new(&spec);
    let mut workload_index = 0usize;
    let mut pod_index = 0usize;
    let mut sat_index = 0usize;
    let mut namespaces = Vec::with_capacity(spec.namespaces.len());

    for (namespace_index, namespace) in spec.namespaces.into_iter().enumerate() {
        let mut workloads = Vec::with_capacity(namespace.workloads.len());
        for workload in namespace.workloads {
            let depends_on = plan.dependencies(namespace_index, workload_index, &workload.deps);
            let pods = workload
                .pods
                .into_iter()
                .map(|pod| {
                    let prepared = PreparedPod {
                        uid: instance_uid(pod_index),
                        name: pod.name.into(),
                        state: pod.state,
                    };
                    pod_index += 1;
                    prepared
                })
                .collect();
            let sats = if with_attachments {
                workload
                    .sats
                    .into_iter()
                    .map(|sat| {
                        let prepared = PreparedSat {
                            uid: attachment_uid(sat_index),
                            name: sat.name.into(),
                            kind: sat.kind,
                            detail: sat.detail,
                        };
                        sat_index += 1;
                        prepared
                    })
                    .collect()
            } else {
                Vec::new()
            };
            workloads.push(PreparedWorkload {
                uid: owner_uid(workload_index),
                name: workload.name.into(),
                kind: workload.kind,
                tool: workload.tool,
                pods,
                sats,
                depends_on,
            });
            workload_index += 1;
        }
        namespaces.push(PreparedNamespace {
            uid: scope_uid(namespace_index),
            name: namespace.name.into(),
            workloads,
        });
    }

    PreparedScene {
        namespaces,
        total_workloads: spec.total_workloads,
        total_pods: spec.total_pods,
        total_sats: if with_attachments { spec.total_sats } else { 0 },
        total_edges: spec.total_edges,
    }
}

pub fn emit_snapshot(spec: &ClusterSpec, with_attachments: bool, out: &mut Vec<IngestEvent>) {
    let mut wl_i = 0usize;
    let mut pod_i = 0usize;
    let mut sat_i = 0usize;

    let plan = SnapshotPlan::new(spec);

    for (ni, ns) in spec.namespaces.iter().enumerate() {
        let scope = scope_uid(ni);
        out.push(IngestEvent::Resource(ResourceEvent {
            kind: KindId::NAMESPACE,
            uid: scope.clone(),
            namespace: Arc::from(""),
            name: Arc::from(ns.name.as_str()),
            resource_version: 0,
            parent: None,
            op: Op::Added,
            payload: Payload::Scope,
        }));

        let ns_name: Arc<str> = Arc::from(ns.name.as_str());
        for wl in &ns.workloads {
            let owner = owner_uid(wl_i);
            let depends_on = plan.dependencies(ni, wl_i, &wl.deps);

            out.push(IngestEvent::Resource(ResourceEvent {
                kind: wl.kind,
                uid: owner.clone(),
                namespace: ns_name.clone(),
                name: Arc::from(wl.name.as_str()),
                resource_version: 0,
                parent: Some(scope.clone()),
                op: Op::Added,
                payload: Payload::Owner {
                    kind: wl.kind,
                    tool: wl.tool,
                    depends_on,
                },
            }));

            for pod in &wl.pods {
                out.push(IngestEvent::Resource(ResourceEvent {
                    kind: KindId::POD,
                    uid: instance_uid(pod_i),
                    namespace: ns_name.clone(),
                    name: Arc::from(pod.name.as_str()),
                    resource_version: 0,
                    parent: Some(owner.clone()),
                    op: Op::Added,
                    payload: Payload::Instance { state: pod.state },
                }));
                pod_i += 1;
            }

            if with_attachments {
                for sat in &wl.sats {
                    out.push(IngestEvent::Resource(ResourceEvent {
                        kind: sat.kind,
                        uid: attachment_uid(sat_i),
                        namespace: ns_name.clone(),
                        name: Arc::from(sat.name.as_str()),
                        resource_version: 0,
                        parent: Some(owner.clone()),
                        op: Op::Added,
                        payload: Payload::Attached {
                            kind: sat.kind,
                            detail: sat.detail.clone(),
                        },
                    }));
                    sat_i += 1;
                }
            }
            wl_i += 1;
        }
    }

    for kind in [
        KindId::NAMESPACE,
        KindId::DEPLOYMENT,
        KindId::STATEFUL_SET,
        KindId::DAEMON_SET,
        KindId::JOB,
        KindId::POD,
        KindId::VOLUME,
        KindId::SERVICE,
        KindId::CONFIG_MAP,
        KindId::SECRET,
    ] {
        out.push(IngestEvent::Capability {
            kind,
            verdict: Capability::Watchable,
        });
        out.push(IngestEvent::Synced { kind });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GenConfig, Scenario, generate};

    fn spec(seed: u64, objects: u32) -> ClusterSpec {
        generate(&GenConfig {
            seed,
            target_objects: objects,
            scenario: Scenario::Platform,
        })
    }

    #[test]
    fn indexed_uids_cover_zero_and_the_platform_maximum_without_heap_formatting() {
        assert_eq!(&*scope_uid(0), "ns-0");
        assert_eq!(&*owner_uid(42), "wl-42");
        assert_eq!(&*instance_uid(usize::MAX), format!("pod-{}", usize::MAX));
        assert_eq!(&*attachment_uid(7), "sat-7");
    }

    fn resources(events: &[IngestEvent]) -> Vec<&ResourceEvent> {
        events
            .iter()
            .filter_map(|e| match e {
                IngestEvent::Resource(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_snapshot_is_every_object_added_exactly_once() {
        let spec = spec(55, 8_000);
        let events = snapshot(&spec, true);
        let res = resources(&events);

        let expected = spec.namespaces.len()
            + spec.total_workloads as usize
            + spec.total_pods as usize
            + spec.total_sats as usize;
        assert_eq!(res.len(), expected, "object count");
        assert!(res.iter().all(|r| r.op == Op::Added), "a snapshot is Added");

        let mut uids: Vec<&str> = res.iter().map(|r| &*r.uid).collect();
        let before = uids.len();
        uids.sort_unstable();
        uids.dedup();
        assert_eq!(uids.len(), before, "uids must be unique across levels");
    }

    #[test]
    fn dense_mode_omits_attachments_at_the_source() {
        let spec = spec(55, 8_000);
        let dense = resources(&snapshot(&spec, false)).len();
        let spread = resources(&snapshot(&spec, true)).len();
        assert_eq!(spread - dense, spec.total_sats as usize);
        assert!(spec.total_sats > 0, "the fixture must exercise attachments");
    }

    #[test]
    fn a_parent_always_arrives_before_its_children() {
        let spec = spec(7, 4_000);
        let events = snapshot(&spec, true);
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for r in resources(&events) {
            if let Some(p) = &r.parent {
                assert!(
                    seen.contains(&**p),
                    "{} arrived before its parent {p}",
                    r.uid
                );
            }
            seen.insert(&r.uid);
        }
    }

    #[test]
    fn dependencies_resolve_to_real_owners_and_can_cross_namespaces() {
        let spec = spec(55, 20_000);
        let events = snapshot(&spec, true);
        let res = resources(&events);

        let ns_of: std::collections::HashMap<&str, &str> = res
            .iter()
            .filter(|r| matches!(r.payload, Payload::Owner { .. }))
            .map(|r| (&*r.uid, &*r.namespace))
            .collect();

        let mut total = 0usize;
        let mut crossing = 0usize;
        for r in &res {
            if let Payload::Owner { depends_on, .. } = &r.payload {
                for target in depends_on {
                    let target_ns = ns_of
                        .get(&**target)
                        .unwrap_or_else(|| panic!("{} depends on unknown {target}", r.uid));
                    total += 1;
                    if *target_ns != &*r.namespace {
                        crossing += 1;
                    }
                }
            }
        }
        assert_eq!(total, spec.total_edges as usize, "edge count");
        assert!(
            crossing > 0,
            "no dependency crossed a namespace, so the cross path is untested"
        );
        assert_eq!(crossing, spec.cross_deps.len());
    }

    #[test]
    fn every_kind_is_declared_synced_and_watchable() {
        let events = snapshot(&spec(1, 2_000), true);
        let synced: Vec<KindId> = events
            .iter()
            .filter_map(|e| match e {
                IngestEvent::Synced { kind } => Some(*kind),
                _ => None,
            })
            .collect();
        assert!(synced.contains(&KindId::POD));
        assert!(synced.contains(&KindId::NAMESPACE));
        assert!(synced.contains(&KindId::VOLUME));

        for r in resources(&events) {
            assert!(
                synced.contains(&r.kind),
                "{:?} appeared but was never declared synced",
                r.kind
            );
        }
        let caps = events
            .iter()
            .filter(|e| matches!(e, IngestEvent::Capability { .. }))
            .count();
        assert_eq!(caps, synced.len());
    }

    #[test]
    fn the_same_seed_streams_the_same_events() {
        let a = snapshot(&spec(9, 4_000), true);
        let b = snapshot(&spec(9, 4_000), true);
        assert_eq!(a, b);
    }
}
