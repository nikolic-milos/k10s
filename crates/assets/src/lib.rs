//! Embedded typography, brand artwork and glyphs, shared by every view.
//!
//! Map glyphs are indexed by model ID; ordinary SVG elements resolve the same
//! bytes by asset path. Our assets take precedence over the pinned upstream
//! source, which supplies window controls and panel icons.
//!
//! Font registration checks the expected family names after loading. A font
//! that parses but silently falls back would change the measured layout.
//! Brand bitmaps have separate light and dark appearances and small and large
//! forms so the shell can choose artwork without tinting or resampling it.

pub mod icons;

#[cfg(feature = "application")]
mod application;
#[cfg(feature = "application")]
pub use application::*;

/// Attribution travels with the same asset package as the glyphs it covers.
pub const ICON_ATTRIBUTION: &str = include_str!("../assets/icons/ATTRIBUTION.md");
