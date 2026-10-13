use gpui::{Bounds, Corners, Pixels, Point, point, px, size};
use k10s_atlas::{Camera, LodPolicy, Scene, WorkloadPresentation};
use k10s_core::{Rect, SceneSnapshot};
use k10s_theme::{MapType, quantize};

use crate::PickPath;

const ISLAND_RADIUS: f32 = 0.14;
const ISLAND_RADIUS_MAX_PX: f32 = 16.0;
pub(crate) const ISLAND_DETAIL_MIN_PX: f32 = 96.0;
const CARD_RADIUS: f32 = 0.14;
const CARD_RADIUS_MAX_PX: f32 = 14.0;
const POD_RADIUS: f32 = 0.22;
const GLYPH_RADIUS: f32 = 0.22;
pub(crate) const WL_MEDALLION_MIN_PX: f32 = 16.0;
const WL_MEDALLION_OF_CARD: f32 = 0.55;
pub(crate) const WL_ICON_MAX_PX: f32 = 32.0;
const SAT_ICON_OF_CELL: f32 = 1.05;
pub(crate) const SAT_ICON_MAX_PX: f32 = 48.0;

pub(crate) const NAME_MIN_WIDTH: f32 = 152.0;
pub(crate) const POD_MARK_MIN: f32 = 24.0;
pub(crate) const POD_DETAIL_MIN_WIDTH: f32 = 144.0;
pub(crate) const POD_DETAIL_MAX_WIDTH: f32 = 224.0;
const POD_MARK_MAX: f32 = POD_DETAIL_MIN_HEIGHT;
pub(crate) const POD_DETAIL_MIN_HEIGHT: f32 = 64.0;
const POD_DETAIL_MAX_HEIGHT: f32 = 104.0;
pub(crate) const TEXT_PAD: f32 = 8.0;
pub(crate) const STATUS_GAP: f32 = 6.0;
pub(crate) const HEADING_CLEARANCE: f32 = 4.0;

#[inline]
pub(crate) fn name_band_height(type_: MapType) -> f32 {
    (workload_row_pitch(type_) * 2.0 + TEXT_PAD * 2.0).ceil()
}

#[inline]
pub(crate) fn workload_row_pitch(type_: MapType) -> f32 {
    row_pitch(type_.workload, type_.line_height).max(WL_ICON_MAX_PX)
}

#[inline]
pub(crate) fn row_pitch(font: f32, line_height: f32) -> f32 {
    (font * line_height).max(quantize(font.max(16.0)))
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
    let stage = if stage > 0 && !policy.stress && !policy.stress_curves {
        let readable = inner.w * zoom >= name_min_width(type_) + TEXT_PAD * 2.0
            && inner.h * zoom >= (name_band_height(type_) + TEXT_PAD + HEADING_CLEARANCE) * 2.0;
        if readable { stage.max(2) } else { 1 }
    } else {
        stage
    };
    let presentation = policy.workload_presentation(inner.w, zoom, stage);
    if presentation == WorkloadPresentation::Medallion
        && inner.w.min(inner.h) * zoom < quantize(0.0)
    {
        WorkloadPresentation::Card
    } else {
        presentation
    }
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WorkloadPrimitive {
    pub body: MarkPrimitive,
    pub heading: Option<MarkPrimitive>,
}

impl MarkPrimitive {
    /// The footer stays between the lower corner centers so a thin quad cannot
    /// paint outside the card's curved edges.
    #[inline]
    pub(crate) fn severity_strip(self) -> Self {
        let body_height = f32::from(self.bounds.size.height);
        let height = (body_height * 0.05).clamp(1.5, 4.0).min(body_height);
        let left = self.corners.bottom_left;
        let right = self.corners.bottom_right;
        Self {
            bounds: Bounds {
                origin: point(
                    self.bounds.origin.x + left,
                    self.bounds.bottom() - px(height),
                ),
                size: size(self.bounds.size.width - left - right, px(height)),
            },
            corners: Corners::all(px(0.0)),
        }
    }

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
        region(self.bounds(rect), short_px)
    }

    #[inline]
    pub(crate) fn card(self, rect: Rect) -> MarkPrimitive {
        card(self.bounds(rect))
    }

    #[inline]
    pub(crate) fn workload<R, B, C, S>(
        self,
        scene: &Scene<R, B, C, S>,
        block_index: usize,
        stage: u8,
        type_: MapType,
        presentation: WorkloadPresentation,
    ) -> WorkloadPrimitive {
        if presentation == WorkloadPresentation::Card {
            let block = &scene.blocks[block_index];
            WorkloadPrimitive {
                body: self.medallion(block.inner),
                heading: None,
            }
        } else {
            self.workload_card(scene, block_index, stage, type_)
        }
    }

    /// Zero or one observed Pod uses compact geometry at every card stage so
    /// adding detail cannot suddenly shrink the same target. Both retain the
    /// published hub and child center without moving relationship anchors.
    /// Multiple Pods keep their layout chrome and placement.
    #[inline]
    pub(crate) fn workload_card<R, B, C, S>(
        self,
        scene: &Scene<R, B, C, S>,
        block_index: usize,
        stage: u8,
        type_: MapType,
    ) -> WorkloadPrimitive {
        let block = &scene.blocks[block_index];
        let mut body = self.card(block.inner);
        let compact = block.children.len() <= 1;
        let heading = if compact {
            let (x, y) = block.inner.center();
            let hub = self.bounds(Rect::new(x, y, 0.0, 0.0));
            let content = scene
                .block_cell_indices(block_index)
                .next()
                .map_or(hub, |index| {
                    self.cell_body(scene.cells[index].rect, stage, type_).bounds
                })
                .union(&hub);
            let width =
                content.size.width.max(px(name_min_width(type_).ceil())) + px(TEXT_PAD * 2.0);
            let top = content.origin.y - px(TEXT_PAD + name_band_height(type_) + HEADING_CLEARANCE);
            let bottom = content.bottom() + px(TEXT_PAD);
            body = card(
                Bounds {
                    origin: point(content.center().x - width * 0.5, top),
                    size: size(width, bottom - top),
                }
                .intersect(&body.bounds),
            );
            let mut band = body.bounds.inset(px(TEXT_PAD));
            let heading = self.name_band_bounds(band, type_);
            if heading.is_some() || !block.children.is_empty() {
                heading
            } else {
                band.origin.x = body.bounds.origin.x;
                band.size.width = body.bounds.size.width;
                self.name_band_bounds(band, type_)
            }
        } else {
            self.name_band(block.inner, type_)
        };
        WorkloadPrimitive { body, heading }
    }

    #[inline]
    pub(crate) fn cell(
        self,
        rect: Rect,
        stage: u8,
        type_: MapType,
        heading: Option<MarkPrimitive>,
    ) -> Option<MarkPrimitive> {
        let primitive = self.cell_body(rect, stage, type_);
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
    fn cell_body(self, rect: Rect, stage: u8, type_: MapType) -> MarkPrimitive {
        let natural = self.bounds(rect);
        let width = f32::from(natural.size.width);
        let height = f32::from(natural.size.height);
        let detailed =
            stage >= 3 && width >= detail_min_width(type_) && height >= POD_DETAIL_MIN_HEIGHT;
        let (width, height) = if detailed {
            (
                width.min(POD_DETAIL_MAX_WIDTH),
                (row_pitch(type_.pod, type_.line_height) * 2.0 + TEXT_PAD * 2.0)
                    .ceil()
                    .clamp(POD_DETAIL_MIN_HEIGHT, POD_DETAIL_MAX_HEIGHT)
                    .min(height),
            )
        } else {
            (width.min(POD_MARK_MAX), height.min(POD_MARK_MAX))
        };
        let center = natural.center();
        pod(Bounds {
            origin: point(center.x - px(width * 0.5), center.y - px(height * 0.5)),
            size: size(px(width), px(height)),
        })
    }

    #[inline]
    pub(crate) fn name_band(self, inner: Rect, type_: MapType) -> Option<MarkPrimitive> {
        self.name_band_bounds(self.bounds(inner), type_)
    }

    #[inline]
    fn name_band_bounds(self, body: Bounds<Pixels>, type_: MapType) -> Option<MarkPrimitive> {
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

    /// An attachment halo cannot enlarge the workload's own identity target.
    #[inline]
    pub(crate) fn medallion(self, inner: Rect) -> MarkPrimitive {
        let short = inner.w.min(inner.h) * self.camera.zoom;
        let side = if short < WL_MEDALLION_MIN_PX {
            quantize(0.0)
        } else {
            icon_side(
                (short * WL_MEDALLION_OF_CARD).max(WL_MEDALLION_MIN_PX),
                WL_ICON_MAX_PX,
            )
        };
        let natural = self.bounds(inner);
        let width = px(side).min(natural.size.width);
        let height = px(side).min(natural.size.height);
        glyph(Bounds {
            origin: point(
                natural.origin.x + (natural.size.width - width) * 0.5,
                natural.origin.y + (natural.size.height - height) * 0.5,
            ),
            size: size(width, height),
        })
    }

    #[inline]
    pub(crate) fn band_icon(
        self,
        band: MarkPrimitive,
        row: usize,
        type_: MapType,
    ) -> MarkPrimitive {
        let pitch = workload_row_pitch(type_);
        let side = WL_ICON_MAX_PX;
        glyph(Bounds {
            origin: point(
                band.bounds.origin.x + px(TEXT_PAD),
                band.bounds.origin.y + px(TEXT_PAD + row as f32 * pitch + (pitch - side) * 0.5),
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
pub(crate) fn region(bounds: Bounds<Pixels>, short_px: f32) -> MarkPrimitive {
    let corners = island_radii(short_px);
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
pub(crate) fn island_radii(short_px: f32) -> Corners<Pixels> {
    Corners::all(px((short_px * ISLAND_RADIUS).min(ISLAND_RADIUS_MAX_PX)))
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
        let block_index = path.block? as usize;
        let block = scene.blocks.get(block_index)?;
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
            .then(|| {
                projection
                    .workload_card(scene, block_index, stage, type_)
                    .heading
            })
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
        let presentation = workload_presentation(policy, block.inner, camera.zoom, stage, type_);
        return match presentation {
            WorkloadPresentation::Hidden => None,
            WorkloadPresentation::Medallion => Some(projection.medallion(block.inner)),
            WorkloadPresentation::Card | WorkloadPresentation::Detailed => Some(
                projection
                    .workload(scene, block_index as usize, stage, type_, presentation)
                    .body,
            ),
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

    fn one_pod_scene() -> Scene {
        let mut scene = Scene::default();
        scene.blocks.push(k10s_atlas::BlockNode {
            rect: Rect::new(40.0, 40.0, 30.0, 56.0),
            inner: Rect::new(40.0, 40.0, 30.0, 56.0),
            label: "api".into(),
            children: 0..1,
            sats: 0..0,
            ext: (),
        });
        scene.cells.push(k10s_atlas::CellNode {
            rect: Rect::new(50.0, 76.0, 10.0, 10.0),
            label: "api-0".into(),
            ext: (),
        });
        scene
    }

    #[test]
    fn small_workloads_bound_chrome_without_moving_pods_or_hubs() {
        let mut scene = one_pod_scene();
        for pods in [1, 0] {
            scene.blocks[0].children = 0..pods;
            for font in [8.0, 14.0, 32.0] {
                let type_ = MapType {
                    workload: font,
                    pod: font,
                    ..Default::default()
                };
                let zoom = crate::lod::STAGE_POD_LABEL.max(detail_min_width(type_) / 10.0);
                for origin in [(0.0, 0.0), (17.25, 23.5)] {
                    let projection = Projection::new(
                        Camera {
                            cx: 55.0,
                            cy: if pods == 1 { 81.0 } else { 68.0 },
                            zoom,
                        },
                        (1600.0, 1000.0),
                        origin,
                    );
                    let geometry = projection.workload_card(&scene, 0, 3, type_);
                    let envelope = projection.bounds(scene.blocks[0].inner);
                    let hub = projection.bounds(Rect::new(55.0, 68.0, 0.0, 0.0)).origin;
                    assert!(geometry.body.bounds.is_contained_within(&envelope));
                    assert!(geometry.body.contains(hub));
                    let heading = geometry.heading.expect("the heading fits");
                    assert!(heading.bounds.is_contained_within(&geometry.body.bounds));
                    if pods == 1 {
                        let pod = projection
                            .cell(scene.cells[0].rect, 3, type_, Some(heading))
                            .expect("the Pod clears its heading");
                        assert_eq!(
                            pod,
                            projection
                                .cell(scene.cells[0].rect, 3, type_, None)
                                .unwrap(),
                        );
                        assert_eq!(
                            pod.bounds.center(),
                            projection.bounds(scene.cells[0].rect).center()
                        );
                        assert!(pod.bounds.is_contained_within(&geometry.body.bounds));
                    }
                    if font == 14.0 {
                        assert_eq!(geometry.body.bounds.size.width, px(168.0));
                        let height = if pods == 1 { 319.2 } else { 100.0 };
                        assert!(
                            (f32::from(geometry.body.bounds.size.height) - height).abs() < 0.001
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn workload_headers_wait_for_both_readable_width_and_height() {
        let policy = crate::lod::policy(Default::default());
        for font in [8.0, 14.0, 32.0] {
            let type_ = MapType {
                workload: font,
                ..Default::default()
            };
            let width = name_min_width(type_) + TEXT_PAD * 2.0;
            let height = (name_band_height(type_) + TEXT_PAD + HEADING_CLEARANCE) * 2.0;
            for (w, h, expected) in [
                (width - 0.5, height, WorkloadPresentation::Medallion),
                (width, height - 0.5, WorkloadPresentation::Medallion),
                (width, height, WorkloadPresentation::Detailed),
            ] {
                let inner = Rect::new(40.0, 40.0, w, h);
                assert_eq!(
                    workload_presentation(&policy, inner, 1.0, 1, type_),
                    expected
                );
            }
        }
    }

    #[test]
    fn empty_cards_keep_name_admission_at_body_and_viewport_thresholds() {
        let mut scene = one_pod_scene();
        scene.blocks[0].children = 0..0;
        let viewport = (600.0, 420.0);
        for (stage, inner) in [
            (1, Rect::new(40.0, 40.0, 400.0, 400.0)),
            (2, Rect::new(40.0, 40.0, 30.0, 42.0)),
            (2, Rect::new(40.0, 40.0, 30.0, 56.0)),
        ] {
            scene.blocks[0].inner = inner;
            let (cx, cy) = inner.center();
            for (font, max_width, height) in [
                (8.0, 168.0, 100.0),
                (14.0, 168.0, 100.0),
                (32.0, 364.0, 123.0),
            ] {
                let type_ = MapType {
                    workload: font,
                    pod: font,
                    ..Default::default()
                };
                let minimum = name_min_width(type_).ceil();
                for (width, edge) in [
                    (minimum, 0),
                    (minimum + 0.5, 0),
                    (minimum + 8.0, 0),
                    (minimum + 16.0, 0),
                    (minimum + 16.0, -1),
                    (minimum + 16.0, 1),
                ] {
                    let zoom = (width / inner.w).max(
                        (name_band_height(type_) + TEXT_PAD + HEADING_CLEARANCE) * 2.0 / inner.h,
                    );
                    let width = inner.w * zoom;
                    let center_x = match edge {
                        -1 => width * 0.5 - 5.0,
                        1 => viewport.0 + 5.0 - width * 0.5,
                        _ => viewport.0 * 0.5,
                    };
                    let camera = Camera {
                        cx: cx - (center_x - viewport.0 * 0.5) / zoom,
                        cy,
                        zoom,
                    };
                    for origin in [(0.0, 0.0), (17.25, 23.5)] {
                        let projection = Projection::new(camera, viewport, origin);
                        assert!(projection.name_band(inner, type_).is_some());
                        let geometry = projection.workload_card(&scene, 0, stage, type_);
                        let heading = geometry.heading.expect("the admitted name stays visible");
                        let envelope = projection.bounds(inner);
                        let hub = projection.bounds(Rect::new(cx, cy, 0.0, 0.0)).origin;
                        assert!(
                            geometry.body.bounds.origin.x >= envelope.origin.x
                                && geometry.body.bounds.origin.y >= envelope.origin.y
                                && geometry.body.bounds.right() <= envelope.right()
                                && geometry.body.bounds.bottom() <= envelope.bottom(),
                            "stage={stage}, font={font}, width={width}, edge={edge}, origin={origin:?}, body={:?}, envelope={envelope:?}",
                            geometry.body.bounds,
                        );
                        assert!(geometry.body.contains(hub));
                        assert!(f32::from(geometry.body.bounds.size.width) <= max_width + 0.001);
                        assert!(
                            (f32::from(geometry.body.bounds.size.height) - height).abs() < 0.001
                        );
                        assert!(
                            heading.bounds.origin.x >= geometry.body.bounds.origin.x
                                && heading.bounds.origin.y >= geometry.body.bounds.origin.y
                                && heading.bounds.right() <= geometry.body.bounds.right()
                                && heading.bounds.bottom() <= geometry.body.bounds.bottom()
                        );
                        assert!(f32::from(heading.bounds.size.width) >= name_min_width(type_));
                        assert!(heading.bounds.origin.x >= px(origin.0 + TEXT_PAD));
                        assert!(heading.bounds.right() <= px(origin.0 + viewport.0 - TEXT_PAD));
                    }
                }
                let zoom = (minimum + 16.0) / inner.w;
                for center_y in [
                    top_clearance(type_, viewport) + name_band_height(type_) - 0.5,
                    viewport.1 - bottom_clearance(type_) + HEADING_CLEARANCE + 0.5,
                ] {
                    let projection = Projection::new(
                        Camera {
                            cx,
                            cy: cy - (center_y - viewport.1 * 0.5) / zoom,
                            zoom,
                        },
                        viewport,
                        (0.0, 0.0),
                    );
                    assert!(
                        projection
                            .workload_card(&scene, 0, stage, type_)
                            .heading
                            .is_none()
                    );
                }
            }
        }
    }

    #[test]
    fn sparse_membership_and_removal_choose_actual_child_geometry() {
        let mut scene = one_pod_scene();
        let projection = Projection::new(
            Camera {
                cx: 55.0,
                cy: 81.0,
                zoom: 14.4,
            },
            (1600.0, 1000.0),
            (0.0, 0.0),
        );
        let type_ = MapType::default();
        let direct = projection.workload_card(&scene, 0, 3, type_);
        let actual = scene.cells[0].clone();
        scene.cells[0].rect = Rect::new(1000.0, 1000.0, 10.0, 10.0);
        scene.cells.push(k10s_atlas::CellNode {
            rect: Rect::default(),
            label: "".into(),
            ext: (),
        });
        scene.cells.push(actual);
        scene.block_cells = vec![2];
        assert_eq!(projection.workload_card(&scene, 0, 3, type_), direct);
        scene.blocks[0].children = 0..0;
        let removed = projection.workload_card(&scene, 0, 3, type_);
        scene.cells.clear();
        scene.block_cells.clear();
        assert_eq!(projection.workload_card(&scene, 0, 3, type_), removed);
        assert_eq!(removed.body.bounds.size.height, px(100.0));
    }

    #[test]
    fn compact_chrome_stays_consistent_across_card_stages_and_preserves_large_groups() {
        let mut scene = one_pod_scene();
        let type_ = MapType::default();
        let projection = Projection::new(
            Camera {
                cx: 55.0,
                cy: 81.0,
                zoom: 14.4,
            },
            (600.0, 420.0),
            (0.0, 0.0),
        );
        let original = WorkloadPrimitive {
            body: projection.card(scene.blocks[0].inner),
            heading: projection.name_band(scene.blocks[0].inner, type_),
        };
        assert_eq!(
            projection.workload_card(&scene, 0, 2, type_),
            projection.workload_card(&scene, 0, 3, type_),
        );
        assert!(
            projection
                .workload_card(&scene, 0, 2, type_)
                .body
                .bounds
                .is_contained_within(&original.body.bounds)
        );
        for pods in [2, 2048] {
            scene.blocks[0].children = 0..pods;
            scene.cells.resize(pods as usize, scene.cells[0].clone());
            assert_eq!(projection.workload_card(&scene, 0, 3, type_), original);
        }
        scene.blocks[0].children = 0..1;
        scene.cells[0].rect = Rect::new(40.0, 40.0, 10.0, 10.0);
        let geometry = projection.workload_card(&scene, 0, 3, type_);
        let envelope = projection.bounds(scene.blocks[0].inner);
        assert_eq!(geometry.body.bounds.origin.y, envelope.origin.y);
        assert!(geometry.body.bounds.is_contained_within(&envelope));
        assert!(geometry.body.contains(envelope.center()));
        if let Some(heading) = geometry.heading {
            assert!(heading.bounds.is_contained_within(&geometry.body.bounds));
        }
    }

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
