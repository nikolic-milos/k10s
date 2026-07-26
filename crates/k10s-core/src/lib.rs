pub mod ingest;
pub mod layout;
pub mod model;
pub mod replay;

use std::sync::Arc;

use arc_swap::ArcSwap;

pub use ingest::{
    Capability, DEFAULT_INTAKE_CAPACITY, DesyncReason, IngestEvent, Intake, IntakeStats, Op,
    Payload, ResourceEvent,
};
pub use k10s_atlas::{BlockNode, CellNode, Edge, Endpoint, Level, Rect, RegionNode, Scene, Totals};
pub use model::{
    BUILTIN_KIND_COUNT, BUILTIN_KINDS, BUILTIN_REASON_COUNT, BUILTIN_REASONS, BUILTIN_TOOL_COUNT,
    BUILTIN_TOOLS, Catalog, KindEntry, KindId, KindInfo, ReasonId, ReasonInfo, Role, Severity,
    State, ToolId, ToolInfo, kind_role, kind_short, reason_severity,
};
pub use replay::RecordedStream;

#[derive(Debug, Clone, Copy)]
pub struct NsExt {
    pub unhealthy_frac: f32,
    /// The worst severity anywhere in this scope. Folded with
    /// [`Severity::rollup`], so it is order-free and cheap to maintain
    /// incrementally.
    pub rollup: Severity,
}

#[derive(Debug, Clone, Copy)]
pub struct WlExt {
    pub kind: KindId,
    pub tool: ToolId,
    /// The worst severity among this owner's instances. A rollup, not a
    /// [`State`]: many pods have many reasons, and picking one to stand for the
    /// workload would invent information. The reason lives on the instance.
    pub rollup: Severity,
    pub ns: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct PodExt {
    pub state: State,
}

#[derive(Debug, Clone)]
pub struct SatExt {
    pub kind: KindId,
    pub detail: Arc<str>,
}

pub type NsNode = RegionNode<NsExt>;
pub type WorkloadNode = BlockNode<WlExt>;
pub type PodNode = CellNode<PodExt>;
pub type SatNode = CellNode<SatExt>;

pub type EdgeInst = Edge;

pub type SceneSnapshot = Scene<NsExt, WlExt, PodExt, SatExt>;

pub type SharedScene = Arc<ArcSwap<SceneSnapshot>>;

pub fn new_shared_scene() -> SharedScene {
    Arc::new(ArcSwap::from_pointee(SceneSnapshot::default()))
}

#[derive(Debug, Clone, Copy)]
pub enum WorldCtrl {
    SetChurn(bool),
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame path walks these arrays per visible node, and the fan-out benches
    /// pad synthetic scenes to these strides to stand in for production. Pinning
    /// them means accidental growth shows up here rather than as an unexplained
    /// benchmark drift. Raise a number deliberately, with a measurement.
    #[test]
    fn node_extension_strides_are_pinned() {
        assert_eq!(size_of::<NsExt>(), 8, "NsExt");
        assert_eq!(size_of::<WlExt>(), 12, "WlExt");
        assert_eq!(size_of::<PodExt>(), 8, "PodExt");
        assert_eq!(size_of::<SatExt>(), 24, "SatExt");
        assert_eq!(size_of::<State>(), 8, "State");
        assert_eq!(size_of::<Severity>(), 1, "Severity is the rollup axis");

        assert_eq!(size_of::<NsNode>(), 56, "NsNode");
        assert_eq!(size_of::<WorkloadNode>(), 80, "WorkloadNode");
        // Unchanged from the closed-enum model: the old one-byte health sat in
        // seven bytes of tail padding, so the reason channel came for free.
        assert_eq!(size_of::<PodNode>(), 40, "PodNode");
        assert_eq!(size_of::<SatNode>(), 56, "SatNode");
    }
}
