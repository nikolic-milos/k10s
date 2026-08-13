//! Map overlay marks: colour, sparklines, extra edges.
//!
//! Overlay state is not the scene. First paint never waits on it. A missing
//! adapter is no overlay, not a hole in the cluster. The walk in `frame.rs`
//! must not grow a per-object overlay lookup; stamp from a bounded side table
//! keyed by uid, and only for what is already being drawn.

use gpui::{Corners, Pixels, Window, point, px, quad, size, transparent_black};
use k10s_atlas::{Camera, Level, LodPolicy, Rect};
use k10s_core::{SceneSnapshot, Severity};
use k10s_theme::{MapTheme, Point as UnitPoint, Series, scale_alpha, sparkline};

use crate::PickPath;

/// One object's overlay, looked up by uid after the walk has decided to draw it.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayMark {
    pub uid: String,
    pub tint: Option<Severity>,
    pub sparkline: Option<Series>,
    /// Sync/health/policy word a HUD can show. Never a secret.
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Sync,
    Metrics,
    Policy,
    MeshDeclared,
    MeshObserved,
}

impl OverlayKind {
    pub const ALL: [OverlayKind; 5] = [
        OverlayKind::Sync,
        OverlayKind::Metrics,
        OverlayKind::Policy,
        OverlayKind::MeshDeclared,
        OverlayKind::MeshObserved,
    ];

    /// Short HUD badge. These five strings are the overlay's name; they must
    /// not be reused as health, LOD, or each other's copy.
    pub fn badge(self) -> &'static str {
        match self {
            OverlayKind::Sync => "SYNC",
            OverlayKind::Metrics => "METRICS",
            OverlayKind::Policy => "POLICY",
            OverlayKind::MeshDeclared => "MESH DECLARED",
            OverlayKind::MeshObserved => "MESH OBSERVED",
        }
    }

    /// What this overlay is, in one line. Declared mesh and observed mesh are
    /// different sentences on purpose.
    pub fn blurb(self) -> &'static str {
        match self {
            OverlayKind::Sync => "GitOps desired versus live",
            OverlayKind::Metrics => "series from in-cluster queries",
            OverlayKind::Policy => "admission and policy reports",
            OverlayKind::MeshDeclared => "can reach, per policy",
            OverlayKind::MeshObserved => "did reach, per telemetry",
        }
    }

    pub fn legend_title(self) -> &'static str {
        self.badge()
    }

    pub fn legend_aria(self) -> &'static str {
        match self {
            OverlayKind::Sync => "GitOps sync legend",
            OverlayKind::Metrics => "Metrics legend",
            OverlayKind::Policy => "Policy legend",
            OverlayKind::MeshDeclared => "Declared mesh legend",
            OverlayKind::MeshObserved => "Observed mesh legend",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            OverlayKind::Sync => "sync",
            OverlayKind::Metrics => "metrics",
            OverlayKind::Policy => "policy",
            OverlayKind::MeshDeclared => "mesh-declared",
            OverlayKind::MeshObserved => "mesh-observed",
        }
    }

    pub fn parse(text: &str) -> Option<OverlayKind> {
        match text.trim().to_ascii_lowercase().as_str() {
            "sync" => Some(OverlayKind::Sync),
            "metrics" => Some(OverlayKind::Metrics),
            "policy" => Some(OverlayKind::Policy),
            "mesh-declared" | "mesh_declared" | "declared" => Some(OverlayKind::MeshDeclared),
            "mesh-observed" | "mesh_observed" | "observed" => Some(OverlayKind::MeshObserved),
            _ => None,
        }
    }

    pub fn next(self) -> Option<OverlayKind> {
        match self {
            OverlayKind::Sync => Some(OverlayKind::Metrics),
            OverlayKind::Metrics => Some(OverlayKind::Policy),
            OverlayKind::Policy => Some(OverlayKind::MeshDeclared),
            OverlayKind::MeshDeclared => Some(OverlayKind::MeshObserved),
            OverlayKind::MeshObserved => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayFrame {
    pub kind: Option<OverlayKind>,
    pub marks: Vec<OverlayMark>,
}

impl OverlayFrame {
    pub fn get(&self, uid: &str) -> Option<&OverlayMark> {
        self.marks.iter().find(|m| m.uid == uid)
    }

    pub fn is_empty(&self) -> bool {
        self.kind.is_none() && self.marks.is_empty()
    }

    /// Stamps for marks whose objects are already on screen at this camera.
    ///
    /// Walks the overlay table, not the scene. A uid the snapshot does not
    /// carry, or a rect the camera has culled, is skipped: missing overlay,
    /// not a default colour. Sparkline geometry is unit-space points scaled
    /// onto the card; tint is a severity the painter reads from the theme.
    pub fn visible_stamps(
        &self,
        scene: &SceneSnapshot,
        camera: Camera,
        policy: &LodPolicy,
        vw: f32,
        vh: f32,
    ) -> Vec<OverlayStamp> {
        if self.marks.is_empty() {
            return Vec::new();
        }
        let visible = camera.visible_world(vw, vh);
        let mut stamps = Vec::new();
        for mark in &self.marks {
            let Some(located) = scene.locate(&mark.uid) else {
                continue;
            };
            if !object_is_drawn(scene, policy, camera.zoom, &visible, located) {
                continue;
            }
            let (x, y) = camera.w2s(located.rect.x, located.rect.y, vw, vh);
            let screen = Rect::new(
                x,
                y,
                located.rect.w * camera.zoom,
                located.rect.h * camera.zoom,
            );
            let island = located.level == Level::Region;
            let spark = match (located.level, mark.sparkline.as_ref()) {
                (Level::Block, Some(series))
                    if policy.block_chrome_shown(located.rect.w, camera.zoom) =>
                {
                    place_sparkline(&sparkline(&series.samples), spark_band(screen))
                }
                _ => Vec::new(),
            };
            stamps.push(OverlayStamp {
                screen,
                tint: mark.tint,
                island,
                spark,
            });
        }
        stamps
    }
}

/// One visible overlay stamp, in viewport pixels, ready to paint outside the walk.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayStamp {
    pub screen: Rect,
    pub tint: Option<Severity>,
    pub island: bool,
    pub spark: Vec<UnitPoint>,
}

const TINT_ALPHA: f32 = 0.32;
const SPARK_STROKE: f32 = 1.5;
const SPARK_INSET: f32 = 0.08;
const SPARK_BAND: f32 = 0.22;
const CARD_RADIUS: f32 = 0.14;
const CARD_RADIUS_MAX_PX: f32 = 14.0;
const ISLAND_RADIUS: f32 = 0.34;

fn object_is_drawn(
    scene: &SceneSnapshot,
    policy: &LodPolicy,
    zoom: f32,
    visible: &Rect,
    located: k10s_core::Located,
) -> bool {
    if !located.rect.intersects(visible) {
        return false;
    }
    match located.level {
        Level::Region => true,
        Level::Block => scene
            .blocks
            .get(located.slot)
            .is_some_and(|block| policy.block_painted(block.inner.w, zoom)),
        Level::Cell => policy.stage_for_zoom(zoom) >= 2,
        Level::Sat => scene
            .sats
            .get(located.slot)
            .is_some_and(|sat| policy.sat_painted(sat.rect.w, zoom)),
    }
}

pub(crate) fn uid_at(scene: &SceneSnapshot, path: PickPath) -> Option<&str> {
    let id = match path.level() {
        Level::Sat => scene.ids.sats.get(path.sat? as usize),
        Level::Cell => scene.ids.cells.get(path.cell? as usize),
        Level::Block => scene.ids.blocks.get(path.block? as usize),
        Level::Region => scene.ids.regions.get(path.region as usize),
    }?;
    let uid = id.as_ref();
    (!uid.is_empty()).then_some(uid)
}

/// Map unit-space sparkline points onto `dest`. Unit y is up; dest y is down.
pub(crate) fn place_sparkline(unit: &[UnitPoint], dest: Rect) -> Vec<UnitPoint> {
    if dest.w <= 0.0 || dest.h <= 0.0 || unit.len() < 2 {
        return Vec::new();
    }
    unit.iter()
        .map(|p| UnitPoint {
            x: dest.x + p.x * dest.w,
            y: dest.y + (1.0 - p.y) * dest.h,
        })
        .collect()
}

/// Axis-aligned quads covering each sparkline segment, at least `stroke` thick.
pub(crate) fn sparkline_quads(placed: &[UnitPoint], stroke: f32) -> Vec<Rect> {
    let mut out = Vec::with_capacity(placed.len().saturating_sub(1));
    for pair in placed.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let w = (a.x - b.x).abs().max(stroke);
        let h = (a.y - b.y).abs().max(stroke);
        out.push(Rect::new(
            (a.x + b.x) * 0.5 - w * 0.5,
            (a.y + b.y) * 0.5 - h * 0.5,
            w,
            h,
        ));
    }
    out
}

fn spark_band(card: Rect) -> Rect {
    let inset_x = card.w * SPARK_INSET;
    let inset_y = card.h * SPARK_INSET;
    let h = (card.h * SPARK_BAND).max(8.0);
    Rect::new(
        card.x + inset_x,
        card.y + card.h - inset_y - h,
        (card.w - inset_x * 2.0).max(1.0),
        h,
    )
}

pub(crate) fn tint_fill(theme: &MapTheme, severity: Severity) -> gpui::Rgba {
    scale_alpha(theme.pod_color(severity), TINT_ALPHA)
}

/// Paint overlay stamps with `Window` quads and a polyline. Outside the walk,
/// so CullStats and the cull oracle do not grow a field for this.
pub(crate) fn paint_stamps(
    stamps: &[OverlayStamp],
    origin: (f32, f32),
    theme: &MapTheme,
    window: &mut Window,
) {
    if stamps.is_empty() {
        return;
    }
    for stamp in stamps {
        if let Some(severity) = stamp.tint {
            let short = stamp.screen.w.min(stamp.screen.h);
            let radius = if stamp.island {
                px(short * ISLAND_RADIUS)
            } else {
                px((short * CARD_RADIUS).min(CARD_RADIUS_MAX_PX))
            };
            window.paint_quad(quad(
                gpui::Bounds::<Pixels> {
                    origin: point(px(origin.0 + stamp.screen.x), px(origin.1 + stamp.screen.y)),
                    size: size(px(stamp.screen.w), px(stamp.screen.h)),
                },
                Corners::all(radius),
                tint_fill(theme, severity),
                px(0.0),
                transparent_black(),
                Default::default(),
            ));
        }
        if stamp.spark.len() < 2 {
            continue;
        }
        let color = stamp.tint.map_or_else(
            || gpui::rgb(theme.edge),
            |severity| theme.pod_color(severity),
        );
        for rect in sparkline_quads(&stamp.spark, SPARK_STROKE) {
            window.paint_quad(quad(
                gpui::Bounds::<Pixels> {
                    origin: point(px(origin.0 + rect.x), px(origin.1 + rect.y)),
                    size: size(px(rect.w), px(rect.h)),
                },
                Corners::all(px(0.0)),
                color,
                px(0.0),
                transparent_black(),
                Default::default(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use k10s_core::{
        KindId, NsExt, NsNode, PodExt, PodNode, ReasonId, SatExt, SatNode, SceneIds, Severity,
        State, ToolId, WlExt, WorkloadNode,
    };
    use k10s_theme::{K10S_DARK, Sample, Series};

    fn policy() -> LodPolicy {
        crate::lod::policy(Default::default())
    }

    fn ids(regions: &[&str], blocks: &[&str], cells: &[&str], sats: &[&str]) -> SceneIds {
        SceneIds {
            regions: regions.iter().map(|uid| Arc::<str>::from(*uid)).collect(),
            blocks: blocks.iter().map(|uid| Arc::<str>::from(*uid)).collect(),
            cells: cells.iter().map(|uid| Arc::<str>::from(*uid)).collect(),
            sats: sats.iter().map(|uid| Arc::<str>::from(*uid)).collect(),
        }
    }

    fn scene_two_islands() -> SceneSnapshot {
        let mut scene = SceneSnapshot::default();
        scene.regions.push(NsNode {
            rect: Rect::new(0.0, 0.0, 120.0, 80.0),
            label: "near".into(),
            weight: 1,
            children: 0..1,
            ext: NsExt {
                unhealthy_frac: 0.0,
                rollup: Severity::Ok,
            },
        });
        scene.regions.push(NsNode {
            rect: Rect::new(8000.0, 8000.0, 120.0, 80.0),
            label: "far".into(),
            weight: 0,
            children: 1..1,
            ext: NsExt {
                unhealthy_frac: 0.0,
                rollup: Severity::Ok,
            },
        });
        scene.blocks.push(WorkloadNode {
            rect: Rect::new(10.0, 10.0, 80.0, 60.0),
            inner: Rect::new(10.0, 10.0, 80.0, 60.0),
            label: "api".into(),
            children: 0..1,
            sats: 0..1,
            ext: WlExt {
                kind: KindId::DEPLOYMENT,
                tool: ToolId::NONE,
                rollup: Severity::Ok,
                ns: 0,
            },
        });
        scene.cells.push(PodNode {
            rect: Rect::new(12.0, 12.0, 16.0, 16.0),
            label: "api-0".into(),
            ext: PodExt {
                state: State::of(ReasonId::RUNNING),
            },
        });
        scene.sats.push(SatNode {
            rect: Rect::new(100.0, 20.0, 10.0, 10.0),
            label: "api-svc".into(),
            ext: SatExt {
                kind: KindId::SERVICE,
                detail: "ClusterIP".into(),
            },
        });
        scene.ids = Arc::new(ids(
            &["ns-near", "ns-far"],
            &["wl-api"],
            &["pod-api"],
            &["svc-api"],
        ));
        scene
    }

    fn looking_at_near() -> Camera {
        Camera {
            cx: 60.0,
            cy: 40.0,
            zoom: 2.0,
        }
    }

    fn series_two() -> Series {
        Series {
            name: "cpu".into(),
            samples: vec![
                Sample {
                    t_ms: 1_000,
                    value: 10.0,
                },
                Sample {
                    t_ms: 2_000,
                    value: 20.0,
                },
            ],
        }
    }

    #[test]
    fn a_missing_uid_is_no_mark_not_a_default_colour() {
        let frame = OverlayFrame::default();
        assert!(frame.get("pod-1").is_none());
    }

    #[test]
    fn overlay_kinds_keep_five_distinct_hud_sentences() {
        let badges: Vec<_> = OverlayKind::ALL.iter().map(|k| k.badge()).collect();
        let blurbs: Vec<_> = OverlayKind::ALL.iter().map(|k| k.blurb()).collect();
        for (i, kind) in OverlayKind::ALL.iter().enumerate() {
            for other in OverlayKind::ALL.iter().skip(i + 1) {
                assert_ne!(kind.badge(), other.badge(), "{kind:?} vs {other:?}");
                assert_ne!(kind.blurb(), other.blurb(), "{kind:?} vs {other:?}");
            }
        }
        assert!(badges.contains(&"SYNC"));
        assert!(badges.contains(&"METRICS"));
        assert!(badges.contains(&"POLICY"));
        assert!(badges.contains(&"MESH DECLARED"));
        assert!(badges.contains(&"MESH OBSERVED"));
        assert!(blurbs[0].contains("GitOps"));
        assert!(blurbs[1].contains("series"));
        assert!(!blurbs[0].contains("telemetry"));
        assert!(!blurbs[1].contains("GitOps"));
        assert!(!blurbs[2].contains("telemetry"));
        assert!(blurbs[3].contains("can reach"));
        assert!(blurbs[3].contains("policy"));
        assert!(blurbs[4].contains("did reach"));
        assert!(blurbs[4].contains("telemetry"));
        assert_ne!(
            OverlayKind::MeshDeclared.blurb(),
            OverlayKind::MeshObserved.blurb()
        );
        assert_ne!(
            OverlayKind::MeshDeclared.badge(),
            OverlayKind::Policy.badge()
        );
    }

    #[test]
    fn visible_stamps_skip_unknown_uids_and_do_not_invent_a_tint() {
        let scene = scene_two_islands();
        let frame = OverlayFrame {
            kind: Some(OverlayKind::Sync),
            marks: vec![OverlayMark {
                uid: "missing".into(),
                tint: Some(Severity::Err),
                sparkline: None,
                label: Some("OutOfSync".into()),
            }],
        };
        let stamps = frame.visible_stamps(&scene, looking_at_near(), &policy(), 400.0, 300.0);
        assert!(stamps.is_empty());
    }

    #[test]
    fn visible_stamps_are_the_on_screen_table_not_the_cluster() {
        let scene = scene_two_islands();
        let frame = OverlayFrame {
            kind: Some(OverlayKind::Policy),
            marks: vec![
                OverlayMark {
                    uid: "ns-near".into(),
                    tint: Some(Severity::Warn),
                    sparkline: None,
                    label: Some("warn".into()),
                },
                OverlayMark {
                    uid: "ns-far".into(),
                    tint: Some(Severity::Err),
                    sparkline: None,
                    label: Some("err".into()),
                },
            ],
        };
        let stamps = frame.visible_stamps(&scene, looking_at_near(), &policy(), 400.0, 300.0);
        assert_eq!(stamps.len(), 1);
        assert_eq!(stamps[0].tint, Some(Severity::Warn));
        assert!(stamps[0].island);
        assert!(stamps[0].spark.is_empty());
    }

    #[test]
    fn a_mark_without_tint_does_not_pick_a_default_colour() {
        let scene = scene_two_islands();
        let frame = OverlayFrame {
            kind: Some(OverlayKind::Metrics),
            marks: vec![OverlayMark {
                uid: "ns-near".into(),
                tint: None,
                sparkline: None,
                label: None,
            }],
        };
        let stamps = frame.visible_stamps(&scene, looking_at_near(), &policy(), 400.0, 300.0);
        assert_eq!(stamps.len(), 1);
        assert_eq!(stamps[0].tint, None);
    }

    #[test]
    fn a_tiny_card_is_not_stamped_when_the_walk_would_not_draw_it() {
        let mut scene = scene_two_islands();
        scene.blocks[0].inner = Rect::new(10.0, 10.0, 1.0, 1.0);
        scene.blocks[0].rect = Rect::new(10.0, 10.0, 1.0, 1.0);
        let frame = OverlayFrame {
            kind: Some(OverlayKind::Metrics),
            marks: vec![OverlayMark {
                uid: "wl-api".into(),
                tint: Some(Severity::Ok),
                sparkline: Some(series_two()),
                label: None,
            }],
        };
        let camera = Camera {
            cx: 10.0,
            cy: 10.0,
            zoom: 1.0,
        };
        let stamps = frame.visible_stamps(&scene, camera, &policy(), 400.0, 300.0);
        assert!(
            stamps.is_empty(),
            "a card below block_min_px is not already being drawn"
        );
    }

    #[test]
    fn sparklines_scale_from_unit_space_onto_the_card() {
        let unit = sparkline(&series_two().samples);
        assert_eq!(unit.len(), 2);
        let dest = Rect::new(10.0, 20.0, 100.0, 50.0);
        let placed = place_sparkline(&unit, dest);
        assert_eq!(placed[0], UnitPoint { x: 10.0, y: 70.0 });
        assert_eq!(placed[1], UnitPoint { x: 110.0, y: 20.0 });
        let quads = sparkline_quads(&placed, 1.5);
        assert_eq!(quads.len(), 1);
        assert!(quads[0].x >= dest.x - 0.01);
        assert!(quads[0].y >= dest.y - 0.01);
        assert!(quads[0].max_x() <= dest.max_x() + 0.01);
        assert!(quads[0].max_y() <= dest.max_y() + 0.01);
    }

    #[test]
    fn one_sample_is_not_a_sparkline() {
        let dest = Rect::new(0.0, 0.0, 40.0, 20.0);
        assert!(place_sparkline(&[UnitPoint { x: 0.0, y: 0.5 }], dest).is_empty());
        assert!(sparkline_quads(&[], 1.5).is_empty());
    }

    #[test]
    fn a_card_in_view_gets_a_sparkline_when_chrome_fits() {
        let scene = scene_two_islands();
        let frame = OverlayFrame {
            kind: Some(OverlayKind::Metrics),
            marks: vec![OverlayMark {
                uid: "wl-api".into(),
                tint: Some(Severity::Warn),
                sparkline: Some(series_two()),
                label: Some("cpu".into()),
            }],
        };
        let camera = Camera {
            cx: 50.0,
            cy: 40.0,
            zoom: 1.0,
        };
        let stamps = frame.visible_stamps(&scene, camera, &policy(), 800.0, 600.0);
        assert_eq!(stamps.len(), 1);
        assert!(!stamps[0].island);
        assert_eq!(stamps[0].spark.len(), 2);
        let band = spark_band(stamps[0].screen);
        assert!(stamps[0].spark[0].x >= band.x - 0.01);
        assert!(stamps[0].spark[1].x <= band.max_x() + 0.01);
    }

    #[test]
    fn tint_uses_the_theme_severity_ramp_not_a_literal() {
        let theme = &K10S_DARK.map;
        assert_eq!(
            tint_fill(theme, Severity::Err),
            scale_alpha(theme.pod_color(Severity::Err), TINT_ALPHA)
        );
        assert_eq!(
            tint_fill(theme, Severity::Ok),
            scale_alpha(theme.pod_color(Severity::Ok), TINT_ALPHA)
        );
        assert_ne!(
            tint_fill(theme, Severity::Err),
            tint_fill(theme, Severity::Ok)
        );
    }

    #[test]
    fn uid_at_reads_the_hovered_slot_not_its_ancestors() {
        let scene = scene_two_islands();
        let region = PickPath {
            region: 0,
            block: None,
            cell: None,
            sat: None,
        };
        let block = PickPath {
            region: 0,
            block: Some(0),
            cell: None,
            sat: None,
        };
        assert_eq!(uid_at(&scene, region), Some("ns-near"));
        assert_eq!(uid_at(&scene, block), Some("wl-api"));
        assert_eq!(
            uid_at(
                &scene,
                PickPath {
                    region: 0,
                    block: Some(0),
                    cell: Some(0),
                    sat: None,
                }
            ),
            Some("pod-api")
        );
    }
}
