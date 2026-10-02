use std::sync::Arc;

use crate::{KindId, State, ToolId};

/// One pod in a fully ordered batch scene.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedPod {
    pub uid: Arc<str>,
    pub name: Arc<str>,
    pub state: State,
}

/// One attachment in a fully ordered batch scene.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSat {
    pub uid: Arc<str>,
    pub name: Arc<str>,
    pub kind: KindId,
    pub detail: Arc<str>,
}

/// One workload and its already grouped children.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedWorkload {
    pub uid: Arc<str>,
    pub name: Arc<str>,
    pub kind: KindId,
    pub tool: ToolId,
    pub pods: Vec<PreparedPod>,
    pub sats: Vec<PreparedSat>,
    pub depends_on: Vec<Arc<str>>,
}

/// One namespace and its already grouped workloads.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedNamespace {
    pub uid: Arc<str>,
    pub name: Arc<str>,
    pub workloads: Vec<PreparedWorkload>,
}

/// An owned, hierarchy-preserving input for one atomic world replacement.
///
/// Live cluster ingestion remains an event stream. This representation is for
/// sources that already own a complete hierarchy, so crossing the world seam
/// does not require flattening and immediately rebuilding it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PreparedScene {
    pub namespaces: Vec<PreparedNamespace>,
    pub total_workloads: u32,
    pub total_pods: u32,
    pub total_sats: u32,
    pub total_edges: u32,
}
