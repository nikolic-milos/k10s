//! The open model and the ingestion contract every other crate meets at.
//!
//! Kinds, tools, and reasons are interned dense ids (`KindId`, `ToolId`,
//! `ReasonId`) resolved through a `Catalog`, never strings in hot paths. The
//! scene is four levels of role (scope, owner, instance, satellite) held in
//! flat vectors inside `SceneSnapshot`; a published snapshot is immutable, so
//! a reader holding one must never observe a mutation. Ingestion is an event
//! stream (`IngestEvent`), not a snapshot type, and `Intake` bounds it on both
//! axes -- it coalesces by object uid and degrades to a labelled `Desync`
//! resync instead of blocking or growing.

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
pub use k10s_atlas::{
    BlockNode, CellNode, Edge, EdgeIndex, Endpoint, Level, Rect, RegionNode, Scene, Totals,
};
pub use model::{
    BUILTIN_KIND_COUNT, BUILTIN_KINDS, BUILTIN_REASON_COUNT, BUILTIN_REASONS, BUILTIN_TOOL_COUNT,
    BUILTIN_TOOLS, Catalog, KindEntry, KindId, KindInfo, ReasonId, ReasonInfo, Role, Severity,
    State, ToolId, ToolInfo, kind_role, kind_short, reason_severity,
};
pub use replay::RecordedStream;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NsExt {
    pub unhealthy_frac: f32,
    pub rollup: Severity,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WlExt {
    pub kind: KindId,
    pub tool: ToolId,
    pub rollup: Severity,
    pub ns: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PodExt {
    pub state: State,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SatExt {
    pub kind: KindId,
    pub detail: Arc<str>,
}

pub type NsNode = RegionNode<NsExt>;
pub type WorkloadNode = BlockNode<WlExt>;
pub type PodNode = CellNode<PodExt>;
pub type SatNode = CellNode<SatExt>;

pub type EdgeInst = Edge;

pub type SceneData = Scene<NsExt, WlExt, PodExt, SatExt>;

// Opaque per-slot identities, parallel to the scene's node vectors. The
// engine below never reads them: they exist so a consumer holding a snapshot
// can say what a slot *is* -- selection, data requests -- across publishes,
// where slot reuse would otherwise let a bare index silently change meaning.
// Tombstoned slots hold the empty string.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SceneIds {
    pub regions: Vec<Arc<str>>,
    pub blocks: Vec<Arc<str>>,
    pub cells: Vec<Arc<str>>,
    pub sats: Vec<Arc<str>>,
}

// The scene the engine draws plus the identity the model layer needs, one
// value so both swap atomically under the same Arc. Identity lives here and
// not on `Scene` deliberately: the engine's hot type stays engine-only, and
// the ids cost one reference bump per snapshot clone.
#[derive(Debug, Clone, Default)]
pub struct SceneSnapshot {
    pub scene: SceneData,
    pub ids: Arc<SceneIds>,
}

impl std::ops::Deref for SceneSnapshot {
    type Target = SceneData;

    fn deref(&self) -> &SceneData {
        &self.scene
    }
}

impl std::ops::DerefMut for SceneSnapshot {
    fn deref_mut(&mut self) -> &mut SceneData {
        &mut self.scene
    }
}

pub type SharedScene = Arc<ArcSwap<SceneSnapshot>>;

pub fn new_shared_scene() -> SharedScene {
    Arc::new(ArcSwap::from_pointee(SceneSnapshot::default()))
}

#[derive(Debug, Clone)]
pub enum WorldCtrl {
    SetChurn(bool),
    /// Flips per second the synthetic churn is allowed to spend. Set after
    /// spawn because the scene's provenance is chosen on screen now: a world
    /// that was still empty when it started must be able to learn that what
    /// arrived is a real cluster, where inventing pod transitions would be a
    /// lie, or the generator, where they are the point.
    SetChurnRate(f32),
    /// Replace the whole scene with one built from this stream: what a cluster
    /// or a starmap chosen on screen sends.
    ///
    /// The stream travels *with* the instruction rather than down the event
    /// channel behind it, for two reasons. A scene arriving all at once has to
    /// be laid out the way the command line's scenes are, by the batch layout --
    /// the incremental one exists for the namespace that appears at runtime, and
    /// placing two hundred of them one after another produces a strip. And a
    /// reset sent alongside the events it replaces would race them: control and
    /// events are separate channels read at different points in a tick, so the
    /// old scene would be re-applied on top of the new one from whatever was
    /// still queued. Carrying the stream makes the replacement one act.
    Rebuild(Vec<IngestEvent>),
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
