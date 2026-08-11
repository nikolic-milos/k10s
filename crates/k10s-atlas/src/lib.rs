//! The map engine: camera, culling, LOD, frame pacing, and the flight
//! harness. Domain-agnostic on purpose -- nothing in here knows what a pod is.
//!
//! `cull` is the counting oracle the painter is checked against; it must stay
//! independently written from any traversal that draws, because the agreement
//! of two independent walks is the correctness argument. Visible work is a
//! function of the viewport, not of scene size, and `visible_work_is_
//! independent_of_scene_size` holds that invariant. The `Flight` harness pins
//! a letterboxed `FLIGHT_VIEWPORT` so a recording is comparable across
//! machines, and stamps the real window and any mid-flight resizes as
//! provenance instead of trusting the environment.

pub mod camera;
pub mod cull;
#[cfg(test)]
mod cull_test;
pub mod curves;
pub mod flight;
pub mod lod;
pub mod motion;
pub mod pacing;
pub mod scene;
pub mod stats;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use camera::{Camera, MAX_ZOOM, MIN_ZOOM};
pub use cull::{CullStats, cull, walk_edges};
pub use flight::{
    CpuPercentiles, FLIGHT_VIEWPORT, Flight, FlightAnchors, FlightFrame, FlightResult, IdleResult,
    Percentiles, Segment, SegmentResult,
};
pub use lod::{LodPolicy, StageBlend, StageMachine};
pub use motion::{FLY_SECONDS, FlyTo, Motion, Step};
pub use pacing::FramePacer;
pub use scene::{
    BlockNode, CellNode, Edge, EdgeIndex, EdgeSegment, Endpoint, Level, Rect, RegionNode, Scene,
    Totals,
};
pub use stats::{
    CounterStats, DrawnCounts, FrameSpans, FrameStats, SegmentCounters, TextCacheCounts,
};
