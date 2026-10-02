use gpui::{Bounds, Corners, Pixels, Point, point, px, size};
use k10s_atlas::{Camera, LodPolicy, WorkloadPresentation};
use k10s_core::{Rect, SceneSnapshot};
use k10s_theme::quantize;

use crate::PickPath;

const ISLAND_RADIUS: f32 = 0.34;
pub(crate) const ISLAND_DETAIL_MIN_PX: f32 = 96.0;
const ISLAND_JITTER: [f32; 16] = [
    0.30, 0.38, 0.47, 0.55, 0.64, 0.72, 0.81, 0.89, 0.34, 0.43, 0.51, 0.60, 0.68, 0.77, 0.85, 1.00,
];
const CARD_RADIUS: f32 = 0.14;
const CARD_RADIUS_MAX_PX: f32 = 14.0;
const POD_RADIUS: f32 = 0.22;
const GLYPH_RADIUS: f32 = 0.22;
const WL_ICON_OF_HEADER: f32 = 0.84;
pub(crate) const WL_MEDALLION_MIN_PX: f32 = 16.0;
const WL_MEDALLION_OF_HALO: f32 = 0.55;
pub(crate) const WL_ICON_MAX_PX: f32 = 96.0;
const SAT_ICON_OF_CELL: f32 = 1.05;
pub(crate) const SAT_ICON_MAX_PX: f32 = 48.0;

/// Keeping bounds and corners together prevents post-walk chrome and hit tests
/// from quietly rebuilding a different silhouette than the painter used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkPrimitive {
    pub bounds: Bounds<Pixels>,
    pub corners: Corners<Pixels>,
}

impl MarkPrimitive {
    #[inline]
    pub fn outset(self, inset: f32) -> Self {
        let grow = |radius: Pixels| px(f32::from(radius) + inset);
        Self {
            bounds: Bounds {
                origin: point(
                    self.bounds.origin.x - px(inset),
                    self.bounds.origin.y - px(inset),
                ),
                size: size(
                    self.bounds.size.width + px(inset * 2.0),
                    self.bounds.size.height + px(inset * 2.0),
                ),
            },
            corners: Corners {
                top_left: grow(self.corners.top_left),
                top_right: grow(self.corners.top_right),
                bottom_right: grow(self.corners.bottom_right),
                bottom_left: grow(self.corners.bottom_left),
            },
        }
    }

    #[inline]
    pub fn contains(self, point: Point<Pixels>) -> bool {
        let x = f32::from(point.x);
        let y = f32::from(point.y);
        let left = f32::from(self.bounds.origin.x);
        let top = f32::from(self.bounds.origin.y);
        let right = left + f32::from(self.bounds.size.width);
        let bottom = top + f32::from(self.bounds.size.height);
        if x < left || x >= right || y < top || y >= bottom {
            return false;
        }

        !outside_corner(x, y, left, top, self.corners.top_left, true, true)
            && !outside_corner(x, y, right, top, self.corners.top_right, false, true)
            && !outside_corner(x, y, right, bottom, self.corners.bottom_right, false, false)
            && !outside_corner(x, y, left, bottom, self.corners.bottom_left, true, false)
    }
}

#[inline]
fn outside_corner(
    x: f32,
    y: f32,
    edge_x: f32,
    edge_y: f32,
    radius: Pixels,
    left: bool,
    top: bool,
) -> bool {
    let radius = f32::from(radius);
    if radius <= 0.0 {
        return false;
    }
    let center_x = edge_x + if left { radius } else { -radius };
    let center_y = edge_y + if top { radius } else { -radius };
    let in_x = if left { x < center_x } else { x >= center_x };
    let in_y = if top { y < center_y } else { y >= center_y };
    in_x && in_y
        && (x - center_x).mul_add(x - center_x, (y - center_y) * (y - center_y)) > radius * radius
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Projection {
    camera: Camera,
    viewport: (f32, f32),
    origin: (f32, f32),
}

impl Projection {
    #[inline]
    pub(crate) const fn new(camera: Camera, viewport: (f32, f32), origin: (f32, f32)) -> Self {
        Self {
            camera,
            viewport,
            origin,
        }
    }

    #[inline]
    pub(crate) fn bounds(self, rect: Rect) -> Bounds<Pixels> {
        let (x, y) = self
            .camera
            .w2s(rect.x, rect.y, self.viewport.0, self.viewport.1);
        Bounds {
            origin: point(px(self.origin.0 + x), px(self.origin.1 + y)),
            size: size(px(rect.w * self.camera.zoom), px(rect.h * self.camera.zoom)),
        }
    }

    #[inline]
    pub(crate) fn region(self, rect: Rect) -> MarkPrimitive {
        let short_px = rect.w.min(rect.h) * self.camera.zoom;
        region(
            rect,
            self.bounds(rect),
            short_px,
            short_px >= ISLAND_DETAIL_MIN_PX,
        )
    }

    #[inline]
    pub(crate) fn card(self, rect: Rect) -> MarkPrimitive {
        card(self.bounds(rect))
    }

    #[inline]
    pub(crate) fn pod(self, rect: Rect) -> MarkPrimitive {
        pod(self.bounds(rect))
    }

    #[inline]
    pub(crate) fn medallion(self, halo: Rect, inner: Rect) -> MarkPrimitive {
        let halo_short = halo.w.min(halo.h) * self.camera.zoom;
        let side = icon_side(
            (halo_short * WL_MEDALLION_OF_HALO)
                .max(WL_MEDALLION_MIN_PX)
                .min(halo_short),
            WL_ICON_MAX_PX,
        );
        let (center_x, center_y) = inner.center();
        self.centered_glyph(center_x, center_y, side)
    }

    #[inline]
    pub(crate) fn header_icon(self, inner: Rect, header_height: f32) -> MarkPrimitive {
        let header_px = header_height * self.camera.zoom;
        let side = icon_side(header_px * WL_ICON_OF_HEADER, WL_ICON_MAX_PX);
        let (x, y) = self
            .camera
            .w2s(inner.x, inner.y, self.viewport.0, self.viewport.1);
        let inset = (header_px - side) * 0.5;
        glyph(Bounds {
            origin: point(px(self.origin.0 + x + inset), px(self.origin.1 + y + inset)),
            size: size(px(side), px(side)),
        })
    }

    #[inline]
    pub(crate) fn satellite(self, rect: Rect) -> MarkPrimitive {
        let side = icon_side(
            rect.w * self.camera.zoom * SAT_ICON_OF_CELL,
            SAT_ICON_MAX_PX,
        );
        let (center_x, center_y) = rect.center();
        self.centered_glyph(center_x, center_y, side)
    }

    #[inline]
    fn centered_glyph(self, world_x: f32, world_y: f32, side: f32) -> MarkPrimitive {
        let (x, y) = self
            .camera
            .w2s(world_x, world_y, self.viewport.0, self.viewport.1);
        glyph(Bounds {
            origin: point(
                px(self.origin.0 + x - side * 0.5),
                px(self.origin.1 + y - side * 0.5),
            ),
            size: size(px(side), px(side)),
        })
    }
}

#[inline]
pub(crate) fn region(
    rect: Rect,
    bounds: Bounds<Pixels>,
    short_px: f32,
    detailed: bool,
) -> MarkPrimitive {
    let corners = if detailed {
        island_radii(&rect, short_px)
    } else {
        Corners::all(px(short_px * ISLAND_RADIUS))
    };
    MarkPrimitive { bounds, corners }
}

#[inline]
pub(crate) fn card(bounds: Bounds<Pixels>) -> MarkPrimitive {
    let short = f32::from(bounds.size.width).min(f32::from(bounds.size.height));
    MarkPrimitive {
        bounds,
        corners: Corners::all(px((short * CARD_RADIUS).min(CARD_RADIUS_MAX_PX))),
    }
}

#[inline]
pub(crate) fn pod(bounds: Bounds<Pixels>) -> MarkPrimitive {
    let short = f32::from(bounds.size.width).min(f32::from(bounds.size.height));
    MarkPrimitive {
        bounds,
        corners: Corners::all(px(short * POD_RADIUS)),
    }
}

#[inline]
fn glyph(bounds: Bounds<Pixels>) -> MarkPrimitive {
    let short = f32::from(bounds.size.width).min(f32::from(bounds.size.height));
    MarkPrimitive {
        bounds,
        corners: Corners::all(px(short * GLYPH_RADIUS)),
    }
}

#[inline]
fn icon_side(want: f32, max_px: f32) -> f32 {
    quantize(want.min(max_px))
}

#[inline]
pub(crate) fn island_radii(rect: &Rect, short_px: f32) -> Corners<Pixels> {
    let base = short_px * ISLAND_RADIUS;
    // Both layout engines normalize the first island to the origin, so a salt
    // prevents its otherwise all-zero identity from producing square corners.
    let bits = ((rect.x.to_bits() as u64) ^ ((rect.y.to_bits() as u64) << 32))
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    let hash = (bits ^ (bits >> 29)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let corner = |shift: u32| px(base * ISLAND_JITTER[((hash >> shift) & 15) as usize]);
    Corners {
        top_left: corner(4),
        top_right: corner(20),
        bottom_right: corner(36),
        bottom_left: corner(52),
    }
}

/// Resolution rejects hidden hierarchy levels so a stale selection cannot draw
/// chrome around geometry that the current LOD did not paint.
pub fn mark_primitive(
    scene: &SceneSnapshot,
    path: PickPath,
    camera: Camera,
    policy: &LodPolicy,
    stage: u8,
    viewport: (f32, f32),
    origin: (f32, f32),
) -> Option<MarkPrimitive> {
    scene.regions.get(path.region as usize)?;
    let projection = Projection::new(camera, viewport, origin);
    if let Some(cell_index) = path.cell {
        let block = scene.blocks.get(path.block? as usize)?;
        let presentation = policy.workload_presentation(block.inner.w, camera.zoom, stage);
        if !presentation.cells_shown()
            || policy.cells_aggregated(
                block.children.len(),
                block
                    .inner
                    .intersection_fraction(&camera.visible_world(viewport.0, viewport.1)),
            )
        {
            return None;
        }
        return scene
            .cells
            .get(cell_index as usize)
            .map(|cell| projection.pod(cell.rect));
    }
    if let Some(sat_index) = path.sat {
        let block = scene.blocks.get(path.block? as usize)?;
        let sat = scene.sats.get(sat_index as usize)?;
        if stage < 2
            || (!policy.block_painted(block.inner.w, camera.zoom) && !policy.stress_curves)
            || !policy.sat_painted(sat.rect.w, camera.zoom)
        {
            return None;
        }
        return Some(projection.satellite(sat.rect));
    }
    if let Some(block_index) = path.block {
        let block = scene.blocks.get(block_index as usize)?;
        return match policy.workload_presentation(block.inner.w, camera.zoom, stage) {
            WorkloadPresentation::Hidden => None,
            WorkloadPresentation::Medallion => Some(projection.medallion(block.rect, block.inner)),
            WorkloadPresentation::Card | WorkloadPresentation::Detailed => {
                Some(projection.card(block.inner))
            }
        };
    }
    scene
        .regions
        .get(path.region as usize)
        .map(|node| projection.region(node.rect))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outset_preserves_each_corner_at_fractional_scale() {
        let primitive = MarkPrimitive {
            bounds: Bounds {
                origin: point(px(10.25), px(20.75)),
                size: size(px(31.5), px(18.25)),
            },
            corners: Corners {
                top_left: px(2.25),
                top_right: px(4.5),
                bottom_right: px(6.75),
                bottom_left: px(3.0),
            },
        };
        let ring = primitive.outset(3.0);

        for scale in [1.0, 1.25, 1.5, 2.0] {
            assert_eq!(
                f32::from(ring.corners.top_right) * scale,
                (f32::from(primitive.corners.top_right) + 3.0) * scale
            );
            assert_eq!(
                f32::from(ring.bounds.size.width) * scale,
                (f32::from(primitive.bounds.size.width) + 6.0) * scale
            );
        }
    }

    #[test]
    fn rounded_containment_rejects_only_the_painted_corner_cutouts() {
        let primitive = MarkPrimitive {
            bounds: Bounds {
                origin: point(px(10.0), px(20.0)),
                size: size(px(40.0), px(30.0)),
            },
            corners: Corners {
                top_left: px(12.0),
                top_right: px(2.0),
                bottom_right: px(8.0),
                bottom_left: px(0.0),
            },
        };

        assert!(!primitive.contains(point(px(10.5), px(20.5))));
        assert!(primitive.contains(point(px(21.0), px(22.0))));
        assert!(primitive.contains(point(px(49.0), px(21.0))));
        assert!(!primitive.contains(point(px(49.5), px(49.5))));
        assert!(primitive.contains(point(px(10.0), px(49.0))));
    }
}
