//! The GPUI painter for the Starmap.
//!
//! `frame::walk` is the one traversal, shared by the window painter and every
//! headless sink through `FrameSink`; in debug builds each painted frame is
//! re-derived by independent atlas and map admission walks and must agree
//! counter for counter. Painting is damage-driven -- zero paints at idle under
//! `--churn 0` is a gated invariant -- and the paint path must not allocate
//! per frame at steady state: labels come from a bounded shaped-text cache
//! that only serves settled frames, and the `bench-alloc` ratchet holds the
//! walk at zero allocations.

mod bench;
mod chrome;
mod frame;
mod hex;
mod lod;
#[cfg(test)]
mod oracle_test;
mod overlay;
mod paint;
mod pick;
mod present;
mod primitive;
mod selection;
mod status;
mod text;
mod view;

pub use bench::{BenchMeta, BenchReport};
pub use frame::FrameOpts;
pub use k10s_atlas::{Camera, CullStats, LodPolicy, StageBlend};
pub use lod::{cull, stage_for_zoom};
pub use overlay::{OverlayFrame, OverlayKind, OverlayMark};
pub use pick::{PickPath, path_rect, pick};
pub use present::PresentProbe;
pub use primitive::{MarkPrimitive, mark_primitive};
pub use view::{MapView, Picked};

gpui::actions!(
    k10s_map,
    [
        ToggleChurn,
        ToggleEdges,
        ToggleHud,
        ToggleLegend,
        CycleOverlay,
        FitView,
        ZoomIn,
        ZoomOut,
    ]
);

// The map's own commands, bound in its own context so the shell's letters
// never collide with them and a user keymap can rebind them by name.
pub fn keybindings() -> Vec<gpui::KeyBinding> {
    let map = Some("Map");
    vec![
        gpui::KeyBinding::new("c", ToggleChurn, map),
        gpui::KeyBinding::new("e", ToggleEdges, map),
        gpui::KeyBinding::new("g", ToggleLegend, map),
        gpui::KeyBinding::new("h", ToggleHud, map),
        gpui::KeyBinding::new("o", CycleOverlay, map),
        gpui::KeyBinding::new("f", FitView, map),
        gpui::KeyBinding::new("=", ZoomIn, map),
        gpui::KeyBinding::new("-", ZoomOut, map),
    ]
}

#[cfg(feature = "testing")]
pub mod testing {
    pub use crate::frame::{FramePaths, FrameSink, IconJob, LabelFace, LabelJob, PaintSink, walk};
}
