//! What the world builds a scene from, folded out of an ingest stream.
//!
//! This exists so the world's input contract is the shared event stream rather
//! than any one producer's type. `build_world` used to take
//! `k10s_clustergen::ClusterSpec`, which made the generator a structural
//! dependency of the renderer's data path and left no seam for a real cluster to
//! attach to.
//!
//! Folding an ordered initial sync into a whole-cluster shape is the honest scope
//! for now: layout places every island in one pass, so it needs the full set.
//! Applying `Modified` and `Deleted` incrementally, without moving what is already
//! placed, is the next phase.

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
    /// Uids of other owners, resolved to indices once every owner is known.
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

/// Counters for what the fold could not place, so a producer bug shows up as a
/// number rather than as a quietly smaller cluster.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FoldStats {
    /// Events whose parent was unknown when they arrived. A conforming producer
    /// emits parents first, so this is nonzero only for a broken or partial stream.
    pub orphaned: u64,
    /// `Modified` and `Deleted` seen during an initial fold, which this pass has
    /// no incremental path for yet.
    pub ignored_updates: u64,
}

/// Folds an ordered initial sync into a whole-cluster shape.
///
/// Relies on the contract's guarantee that a parent arrives before its children,
/// which the generator's snapshot and any conforming relist both provide.
pub fn fold(events: &[IngestEvent]) -> (ClusterInput, FoldStats) {
    let mut input = ClusterInput::default();
    let mut stats = FoldStats::default();
    // uid -> namespace index, and uid -> (namespace, workload) index.
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
    /// Global workload index per uid, in the order the world assigns block
    /// indices, so dependency uids can become endpoints.
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
        // A child whose parent never arrived means a broken producer, and the fold
        // must make that visible instead of quietly shrinking the cluster.
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
        // Honest about scope: an initial fold has no incremental path, so a
        // Modified is tallied rather than half-applied.
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
