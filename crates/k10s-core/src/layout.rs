//! The spacing vocabulary of the hierarchical scene, in world units.
//!
//! Policy lives here so one number cannot drift between the crate that places
//! nodes and the crates that draw or measure them; the algorithms that consume
//! it live in `k10s-world`. Nothing here is per-run random: `SAT_JITTER_MAX`
//! bounds a displacement derived from a hash of the object's identity, so the
//! same scene lays out the same way on every run.
//!
//! Changing a constant is a visual change to every consumer at once, even
//! though no type changes with it.

pub const POD_SIZE: f32 = 10.0;
pub const POD_GAP: f32 = 4.0;
pub const POD_PITCH: f32 = POD_SIZE + POD_GAP;

pub const WL_PAD: f32 = 10.0;
pub const WL_HEADER: f32 = 16.0;
pub const WL_GAP: f32 = 26.0;

pub const NS_PAD: f32 = 36.0;
pub const NS_HEADER: f32 = 44.0;
pub const NS_GAP: f32 = 120.0;

pub const CARD_PAD: f32 = 10.0;
pub const CARD_HEADER: f32 = 26.0;

pub const SAT_SIZE: f32 = 18.0;
pub const SAT_RING0_GAP: f32 = 66.0;
pub const SAT_RING_GAP: f32 = 54.0;
pub const SAT_ARC_PITCH: f32 = 52.0;

pub const SAT_MARGIN: f32 = 26.0;
pub const SAT_JITTER_MAX: f32 = 18.0;

pub const HUB_GAP: f32 = 70.0;
pub const ISLAND_GAP_MIN: f32 = 420.0;
pub const ISLAND_GAP_FACTOR: f32 = 0.30;
