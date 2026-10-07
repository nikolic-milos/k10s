use gpui::{Bounds, Corners, Pixels, Point, point, px, size};
use k10s_atlas::{Camera, LodPolicy, WorkloadPresentation};
use k10s_core::{Rect, SceneSnapshot};
use k10s_theme::{MapType, quantize};

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
pub(crate) const WL_MEDALLION_MIN_PX: f32 = 16.0;
const WL_MEDALLION_OF_HALO: f32 = 0.55;
pub(crate) const WL_ICON_MAX_PX: f32 = 96.0;
const SAT_ICON_OF_CELL: f32 = 1.05;
pub(crate) const SAT_ICON_MAX_PX: f32 = 48.0;

pub(crate) const NAME_MIN_WIDTH: f32 = 152.0;
pub(crate) const POD_MARK_MIN: f32 = 24.0;
pub(crate) const POD_DETAIL_MIN_WIDTH: f32 = 144.0;
pub(crate) const POD_DETAIL_MAX_WIDTH: f32 = 224.0;
const POD_MARK_MAX: f32 = 96.0;
const POD_DETAIL_MIN_HEIGHT: f32 = 64.0;
const POD_DETAIL_MAX_HEIGHT: f32 = 104.0;
pub(crate) const TEXT_PAD: f32 = 8.0;
pub(crate) const STATUS_GAP: f32 = 6.0;
pub(crate) const HEADING_CLEARANCE: f32 = 4.0;

#[inline]
pub(crate) fn name_band_height(type_: MapType) -> f32 {
    (type_.workload * type_.line_height * 2.0 + TEXT_PAD * 2.0).ceil()
}

#[inline]
pub(crate) fn name_min_width(type_: MapType) -> f32 {
    (NAME_MIN_WIDTH * type_.workload / MapType::default().workload).max(NAME_MIN_WIDTH)
}

#[inline]
pub(crate) fn top_clearance(type_: MapType, viewport: (f32, f32)) -> f32 {
    let line = type_.workload * type_.line_height;
    match crate::chrome::density(viewport.0, viewport.1) {
        crate::chrome::Density::Minimal => 64.0,
        crate::chrome::Density::Compact => (line + 40.0).max(64.0),
        crate::chrome::Density::Full => line * 2.0 + 60.0,
    }
}

#[inline]
pub(crate) fn bottom_clearance(type_: MapType) -> f32 {
    ((type_.workload - 4.0).max(6.0) * type_.line_height + 40.0).max(64.0)
}

#[inline]
pub(crate) fn detail_min_width(type_: MapType) -> f32 {
    (POD_DETAIL_MIN_WIDTH * type_.pod / MapType::default().pod)
        .clamp(POD_DETAIL_MIN_WIDTH, POD_DETAIL_MAX_WIDTH)
}

#[inline]
pub(crate) fn workload_presentation(
    policy: &LodPolicy,
    inner: Rect,
    zoom: f32,
    stage: u8,
    type_: MapType,
) -> WorkloadPresentation {
    let stage = if stage == 1
        && inner.w * zoom >= name_min_width(type_)
        && inner.h * zoom >= name_band_height(type_)
    {
        2
    } else {
        stage
    };
    policy.workload_presentation(inner.w, zoom, stage)
}

#[inline]
pub(crate) fn workload_label_shown(policy: &LodPolicy, width: f32, zoom: f32) -> bool {
    width * zoom >= policy.block_label_min_px && zoom > policy.block_label_min_zoom
}

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
    pub(crate) fn cell(
        self,
        rect: Rect,
        stage: u8,
        type_: MapType,
        heading: Option<MarkPrimitive>,
    ) -> Option<MarkPrimitive> {
        let natural = self.bounds(rect);
        let width = f32::from(natural.size.width);
        let height = f32::from(natural.size.height);
        let detailed =
            stage >= 3 && width >= detail_min_width(type_) && height >= POD_DETAIL_MIN_HEIGHT;
        let (width, height) = if detailed {
            (
                width.min(POD_DETAIL_MAX_WIDTH),
                (type_.pod * type_.line_height * 2.0 + TEXT_PAD * 2.0)
                    .ceil()
                    .clamp(POD_DETAIL_MIN_HEIGHT, POD_DETAIL_MAX_HEIGHT)
                    .min(height),
            )
        } else {
            (width.min(POD_MARK_MAX), height.min(POD_MARK_MAX))
        };
        let center = natural.center();
        let primitive = pod(Bounds {
            origin: point(center.x - px(width * 0.5), center.y - px(height * 0.5)),
            size: size(px(width), px(height)),
        });
        if heading.is_some_and(|heading| {
            primitive
                .bounds
                .intersects(&heading.outset(HEADING_CLEARANCE).bounds)
        }) {
            return None;
        }
        Some(primitive)
    }

    #[inline]
    pub(crate) fn name_band(self, inner: Rect, type_: MapType) -> Option<MarkPrimitive> {
        let body = self.bounds(inner);
        let left = f32::from(body.origin.x).max(self.origin.0 + TEXT_PAD);
        let right = f32::from(body.right()).min(self.origin.0 + self.viewport.0 - TEXT_PAD);
        let top = f32::from(body.origin.y).max(self.origin.1 + top_clearance(type_, self.viewport));
        let bottom =
            f32::from(body.bottom()).min(self.origin.1 + self.viewport.1 - bottom_clearance(type_));
        let height = name_band_height(type_);
        if right - left < name_min_width(type_) || bottom - top < height {
            return None;
        }
        Some(MarkPrimitive {
            bounds: Bounds {
                origin: point(px(left), px(top)),
                size: size(px(right - left), px(height)),
            },
            corners: Corners::all(px(6.0)),
        })
    }

    #[inline]
    pub(crate) fn label_visible(self, bounds: Bounds<Pixels>, type_: MapType) -> bool {
        f32::from(bounds.origin.y) >= self.origin.1 + top_clearance(type_, self.viewport)
            && f32::from(bounds.bottom())
                <= self.origin.1 + self.viewport.1 - bottom_clearance(type_)
            && f32::from(bounds.right()) > self.origin.0
            && f32::from(bounds.origin.x) < self.origin.0 + self.viewport.0
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
    pub(crate) fn band_icon(
        self,
        band: MarkPrimitive,
        row: usize,
        type_: MapType,
    ) -> MarkPrimitive {
        let line = type_.workload * type_.line_height;
        let side = quantize(type_.workload.max(16.0));
        glyph(Bounds {
            origin: point(
                band.bounds.origin.x + px(TEXT_PAD),
                band.bounds.origin.y + px(TEXT_PAD + row as f32 * line + (line - side) * 0.5),
            ),
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
        corners: Corners::all(px((short * POD_RADIUS).min(CARD_RADIUS_MAX_PX))),
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
    mark_primitive_typed(
        scene,
        path,
        camera,
        policy,
        stage,
        viewport,
        origin,
        MapType::default(),
    )
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn mark_primitive_typed(
    scene: &SceneSnapshot,
    path: PickPath,
    camera: Camera,
    policy: &LodPolicy,
    stage: u8,
    viewport: (f32, f32),
    origin: (f32, f32),
    type_: MapType,
) -> Option<MarkPrimitive> {
    scene.regions.get(path.region as usize)?;
    let projection = Projection::new(camera, viewport, origin);
    if let Some(cell_index) = path.cell {
        let block = scene.blocks.get(path.block? as usize)?;
        let presentation = workload_presentation(policy, block.inner, camera.zoom, stage, type_);
        if stage < 2
            || !presentation.cells_shown()
            || policy.cells_aggregated(
                block.children.len(),
                block
                    .inner
                    .intersection_fraction(&camera.visible_world(viewport.0, viewport.1)),
            )
        {
            return None;
        }
        let heading = workload_label_shown(policy, block.inner.w, camera.zoom)
            .then(|| projection.name_band(block.inner, type_))
            .flatten();
        return scene
            .cells
            .get(cell_index as usize)
            .and_then(|cell| projection.cell(cell.rect, stage, type_, heading));
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
        return match workload_presentation(policy, block.inner, camera.zoom, stage, type_) {
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
    fn a_name_band_is_admitted_at_its_inclusive_width_minimum() {
        let policy = crate::lod::policy(Default::default());
        let camera = Camera {
            cx: 0.0,
            cy: 0.0,
            zoom: 1.0,
        };
        let projection = Projection::new(camera, (600.0, 420.0), (0.0, 0.0));
        let inner = Rect::new(-76.0, -60.0, NAME_MIN_WIDTH, 120.0);
        assert!(workload_label_shown(&policy, inner.w, camera.zoom));
        assert!(projection.name_band(inner, MapType::default()).is_some());
        assert!(
            projection
                .name_band(
                    Rect {
                        w: NAME_MIN_WIDTH - 1.0,
                        ..inner
                    },
                    MapType::default()
                )
                .is_none()
        );
    }

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
