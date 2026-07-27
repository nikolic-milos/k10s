pub mod ingest;
pub mod layout;
pub mod model;
pub mod replay;

use std::sync::Arc;

use arc_swap::ArcSwap;

pub use ingest::{
    CONTROL_CAPACITY, Capability, DEFAULT_INTAKE_CAPACITY, DesyncReason, IngestEvent, Intake,
    IntakeStats, Op, Payload, ResourceEvent,
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
    pub rollup: Severity,
}

#[derive(Debug, Clone, Copy)]
pub struct WlExt {
    pub kind: KindId,
    pub tool: ToolId,
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
        assert_eq!(size_of::<PodNode>(), 40, "PodNode");
        assert_eq!(size_of::<SatNode>(), 56, "SatNode");
    }
}
