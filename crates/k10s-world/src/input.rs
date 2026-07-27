use std::collections::HashMap;
use std::sync::Arc;

use k10s_core::{IngestEvent, KindId, Op, Payload, State, ToolId};

#[derive(Debug, Clone)]
pub struct PodInput {
    pub name: Arc<str>,
    pub state: State,
}

#[derive(Debug, Clone)]
pub struct SatInput {
    pub name: Arc<str>,
    pub kind: KindId,
    pub detail: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct WlInput {
    pub uid: Arc<str>,
    pub name: Arc<str>,
    pub kind: KindId,
    pub tool: ToolId,
    pub pods: Vec<PodInput>,
    pub sats: Vec<SatInput>,
    pub depends_on: Vec<Arc<str>>,
}

#[derive(Debug, Clone)]
pub struct NsInput {
    pub name: Arc<str>,
    pub workloads: Vec<WlInput>,
}

#[derive(Debug, Default, Clone)]
pub struct ClusterInput {
    pub namespaces: Vec<NsInput>,
    pub total_workloads: u32,
    pub total_pods: u32,
    pub total_sats: u32,
    pub total_edges: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FoldStats {
    pub orphaned: u64,
    pub ignored_updates: u64,
}

pub fn fold(events: &[IngestEvent]) -> (ClusterInput, FoldStats) {
    let mut input = ClusterInput::default();
    let mut stats = FoldStats::default();
    let mut scope_of: HashMap<Arc<str>, usize> = HashMap::new();
    let mut owner_of: HashMap<Arc<str>, (usize, usize)> = HashMap::new();

    for event in events {
        let IngestEvent::Resource(r) = event else {
            continue;
        };
        if r.op != Op::Added {
            stats.ignored_updates += 1;
            continue;
        }
        match &r.payload {
            Payload::Scope => {
                scope_of.insert(r.uid.clone(), input.namespaces.len());
                input.namespaces.push(NsInput {
                    name: r.name.clone(),
                    workloads: Vec::new(),
                });
            }
            Payload::Owner {
                kind,
                tool,
                depends_on,
            } => {
                let Some(&ni) = r.parent.as_ref().and_then(|p| scope_of.get(p)) else {
                    stats.orphaned += 1;
                    continue;
                };
                let wi = input.namespaces[ni].workloads.len();
                owner_of.insert(r.uid.clone(), (ni, wi));
                input.namespaces[ni].workloads.push(WlInput {
                    uid: r.uid.clone(),
                    name: r.name.clone(),
                    kind: *kind,
                    tool: *tool,
                    pods: Vec::new(),
                    sats: Vec::new(),
                    depends_on: depends_on.clone(),
                });
                input.total_workloads += 1;
                input.total_edges += depends_on.len() as u32;
            }
            Payload::Instance { state } => {
                let Some(&(ni, wi)) = r.parent.as_ref().and_then(|p| owner_of.get(p)) else {
                    stats.orphaned += 1;
                    continue;
                };
                input.namespaces[ni].workloads[wi].pods.push(PodInput {
                    name: r.name.clone(),
                    state: *state,
                });
                input.total_pods += 1;
            }
            Payload::Attached { kind, detail } => {
                let Some(&(ni, wi)) = r.parent.as_ref().and_then(|p| owner_of.get(p)) else {
                    stats.orphaned += 1;
                    continue;
                };
                input.namespaces[ni].workloads[wi].sats.push(SatInput {
                    name: r.name.clone(),
                    kind: *kind,
                    detail: detail.clone(),
                });
                input.total_sats += 1;
            }
        }
    }
    (input, stats)
}

impl ClusterInput {
    pub fn owner_indices(&self) -> HashMap<Arc<str>, u32> {
        let mut out = HashMap::with_capacity(self.total_workloads as usize);
        let mut i = 0u32;
        for ns in &self.namespaces {
            for wl in &ns.workloads {
                out.insert(wl.uid.clone(), i);
                i += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10s_core::replay;

    #[test]
    fn a_small_initial_sync_folds_into_the_shape_it_described() {
        let (input, stats) = fold(&replay::initial_sync().events);
        assert_eq!(stats, FoldStats::default(), "nothing should be unplaceable");
        assert_eq!(input.namespaces.len(), 1);
        assert_eq!(&*input.namespaces[0].name, "prod");
        assert_eq!(input.total_workloads, 1);
        assert_eq!(input.total_pods, 2);
        assert_eq!(input.namespaces[0].workloads[0].pods.len(), 2);
        assert_eq!(&*input.namespaces[0].workloads[0].name, "api");
    }

    #[test]
    fn an_orphan_is_counted_rather_than_silently_dropped() {
        let events = vec![replay::instance(
            "pod-x",
            "prod",
            "wl-missing",
            State::OK,
            Op::Added,
        )];
        let (input, stats) = fold(&events);
        assert_eq!(stats.orphaned, 1);
        assert_eq!(input.total_pods, 0);
    }

    #[test]
    fn updates_are_counted_as_unhandled_by_this_pass() {
        let (_, stats) = fold(&replay::churn(5).events);
        assert_eq!(stats.ignored_updates, 5);
    }

    #[test]
    fn owner_indices_follow_namespace_then_workload_order() {
        let mut s = replay::initial_sync();
        s.push(replay::owner(
            "wl-two",
            "prod",
            "second",
            KindId::JOB,
            Op::Added,
        ));
        let (input, _) = fold(&s.events);
        let idx = input.owner_indices();
        assert_eq!(idx.get(&Arc::<str>::from("wl-api")).copied(), Some(0));
        assert_eq!(idx.get(&Arc::<str>::from("wl-two")).copied(), Some(1));
        assert_eq!(idx.len(), input.total_workloads as usize);
    }
}
