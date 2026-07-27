use std::sync::Arc;

use k10s_core::{Capability, IngestEvent, KindId, Op, Payload, ResourceEvent};

use crate::ClusterSpec;

pub fn scope_uid(ns: usize) -> Arc<str> {
    format!("ns-{ns}").into()
}

pub fn owner_uid(wl: usize) -> Arc<str> {
    format!("wl-{wl}").into()
}

pub fn instance_uid(pod: usize) -> Arc<str> {
    format!("pod-{pod}").into()
}

pub fn attachment_uid(sat: usize) -> Arc<str> {
    format!("sat-{sat}").into()
}

pub fn snapshot(spec: &ClusterSpec, with_attachments: bool) -> Vec<IngestEvent> {
    let mut out = Vec::new();
    emit_snapshot(spec, with_attachments, &mut out);
    out
}

pub fn emit_snapshot(spec: &ClusterSpec, with_attachments: bool, out: &mut Vec<IngestEvent>) {
    let mut wl_i = 0usize;
    let mut pod_i = 0usize;
    let mut sat_i = 0usize;

    let mut ns_first_wl = Vec::with_capacity(spec.namespaces.len());
    let mut running = 0usize;
    for ns in &spec.namespaces {
        ns_first_wl.push(running);
        running += ns.workloads.len();
    }

    let mut cross_by_src: Vec<Vec<u32>> = vec![Vec::new(); running];
    for &(a, b) in &spec.cross_deps {
        if let Some(slot) = cross_by_src.get_mut(a as usize) {
            slot.push(b);
        }
    }

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
            let mut depends_on: Vec<Arc<str>> = wl
                .deps
                .iter()
                .map(|&d| owner_uid(ns_first_wl[ni] + d as usize))
                .collect();
            depends_on.extend(cross_by_src[wl_i].iter().map(|&b| owner_uid(b as usize)));

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
                            detail: Arc::from(sat.detail.as_str()),
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
