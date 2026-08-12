//! Bounded, accessible furniture over the Starmap canvas.
//!
//! Nothing in this module walks the scene. The map traversal remains the sole
//! owner of per-node work; chrome receives totals, one resolved hover path and
//! the current camera, then builds a fixed number of GPUI elements. That keeps
//! the map calm at idle and keeps cluster size out of interaction cost.

use gpui::{Context, SharedString, rgb};
use k10s_atlas::{Camera, LodPolicy};
use k10s_core::{BUILTIN_KINDS, SceneSnapshot, Severity, Totals};
use k10s_theme::Theme;

use crate::{Grouped, PickPath, path_rect};
const HOVER_WIDTH: f32 = 252.0;
const HOVER_HEIGHT: f32 = 74.0;
const EDGE_MARGIN: f32 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailBand {
    Orbit,
    Region,
    System,
    Instance,
}

impl DetailBand {
    const ALL: [DetailBand; 4] = [
        DetailBand::Orbit,
        DetailBand::Region,
        DetailBand::System,
        DetailBand::Instance,
    ];

    fn label(self) -> &'static str {
        match self {
            DetailBand::Orbit => "ORBIT",
            DetailBand::Region => "REGION",
            DetailBand::System => "SYSTEM",
            DetailBand::Instance => "INSTANCE",
        }
    }

    fn description(self) -> &'static str {
        match self {
            DetailBand::Orbit => "namespace health",
            DetailBand::Region => "workload landmarks",
            DetailBand::System => "pods and resources",
            DetailBand::Instance => "instance detail",
        }
    }
}

pub(crate) fn detail_band(policy: &LodPolicy, zoom: f32) -> DetailBand {
    if zoom < policy.stage_block {
        DetailBand::Orbit
    } else if zoom < policy.stage_cell {
        DetailBand::Region
    } else if zoom < policy.stage_cell_label {
        DetailBand::System
    } else {
        DetailBand::Instance
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Density {
    Full,
    Compact,
    Minimal,
}

fn density(width: f32, height: f32) -> Density {
    if width < 500.0 || height < 300.0 {
        Density::Minimal
    } else if width < 820.0 || height < 520.0 {
        Density::Compact
    } else {
        Density::Full
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SummaryCache {
    totals: Option<Totals>,
    line: SharedString,
}

impl SummaryCache {
    pub(crate) fn line(&mut self, totals: Totals) -> SharedString {
        if self.totals != Some(totals) {
            self.totals = Some(totals);
            self.line = format!(
                "{} namespaces  ·  {} workloads  ·  {} pods",
                Grouped(totals.regions),
                Grouped(totals.blocks),
                Grouped(totals.cells),
            )
            .into();
        }
        self.line.clone()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct HoverInfo {
    kind: &'static str,
    name: SharedString,
    namespace: Option<SharedString>,
    owner: Option<SharedString>,
    status: &'static str,
    mark: &'static str,
    severity: Option<Severity>,
}

impl HoverInfo {
    fn resolve(scene: &SceneSnapshot, path: PickPath) -> Option<Self> {
        let region = scene.regions.get(path.region as usize)?;
        let namespace = SharedString::from(&region.label);
        let owner = path
            .block
            .and_then(|slot| scene.blocks.get(slot as usize))
            .map(|node| SharedString::from(&node.label));

        match path.level() {
            k10s_core::Level::Region => Some(HoverInfo::new(
                "Namespace",
                SharedString::from(&region.label),
                None,
                None,
                Some(region.ext.rollup),
            )),
            k10s_core::Level::Block => {
                let node = scene.blocks.get(path.block? as usize)?;
                Some(HoverInfo::new(
                    kind_name(node.ext.kind),
                    SharedString::from(&node.label),
                    Some(namespace),
                    None,
                    Some(node.ext.rollup),
                ))
            }
            k10s_core::Level::Cell => {
                let node = scene.cells.get(path.cell? as usize)?;
                Some(HoverInfo::new(
                    "Pod",
                    SharedString::from(&node.label),
                    Some(namespace),
                    owner,
                    Some(node.ext.state.severity),
                ))
            }
            k10s_core::Level::Sat => {
                let node = scene.sats.get(path.sat? as usize)?;
                Some(HoverInfo {
                    kind: kind_name(node.ext.kind),
                    name: SharedString::from(&node.label),
                    namespace: Some(namespace),
                    owner,
                    status: "Attached",
                    mark: "◇",
                    severity: None,
                })
            }
        }
    }

    fn new(
        kind: &'static str,
        name: SharedString,
        namespace: Option<SharedString>,
        owner: Option<SharedString>,
        severity: Option<Severity>,
    ) -> HoverInfo {
        let (status, mark) = match severity.unwrap_or(Severity::Unknown) {
            Severity::Ok => ("Healthy", "✓"),
            Severity::Warn => ("Warning", "!"),
            Severity::Err => ("Critical", "×"),
            Severity::Unknown => ("Unknown", "?"),
        };
        HoverInfo {
            kind,
            name,
            namespace,
            owner,
            status,
            mark,
            severity,
        }
    }

    fn color(&self, theme: &Theme) -> gpui::Rgba {
        self.severity.map_or_else(
            || rgb(theme.shell.text_accent),
            |severity| theme.map.pod_color(severity),
        )
    }
}

fn kind_name(kind: k10s_core::KindId) -> &'static str {
    BUILTIN_KINDS
        .get(kind.0 as usize)
        .map_or("Resource", |info| info.kind)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct HoverAnchor {
    left: f32,
    top: f32,
}

fn hover_anchor(
    scene: &SceneSnapshot,
    path: PickPath,
    camera: Camera,
    width: f32,
    height: f32,
) -> Option<HoverAnchor> {
    if width < HOVER_WIDTH + EDGE_MARGIN * 2.0 || height < HOVER_HEIGHT + EDGE_MARGIN * 2.0 {
        return None;
    }
    let rect = path_rect(scene, &path)?;
    let (left, top) = camera.w2s(rect.x, rect.y, width, height);
    let (right, bottom) = camera.w2s(rect.max_x(), rect.max_y(), width, height);
    let wanted_left = (left + right) * 0.5 - HOVER_WIDTH * 0.5;
    let below = bottom + 10.0;
    let wanted_top = if below + HOVER_HEIGHT <= height - EDGE_MARGIN {
        below
    } else {
        top - HOVER_HEIGHT - 10.0
    };
    Some(HoverAnchor {
        left: wanted_left.clamp(
            EDGE_MARGIN,
            (width - HOVER_WIDTH - EDGE_MARGIN).max(EDGE_MARGIN),
        ),
        top: wanted_top.clamp(
            EDGE_MARGIN,
            (height - HOVER_HEIGHT - EDGE_MARGIN).max(EDGE_MARGIN),
        ),
    })
}

pub(crate) struct Overlay<'a> {
    pub(crate) scene: &'a SceneSnapshot,
    pub(crate) camera: Camera,
    pub(crate) policy: &'a LodPolicy,
    pub(crate) hovered: Option<PickPath>,
    pub(crate) summary: SharedString,
    pub(crate) edges_on: bool,
    pub(crate) legend_on: bool,
    pub(crate) viewport: (f32, f32),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct State {
    summary: SharedString,
    band: DetailBand,
    density: Density,
    hover: Option<(HoverInfo, HoverAnchor)>,
    edges_on: bool,
    legend_on: bool,
    empty: bool,
}

impl State {
    pub(crate) fn resolve(overlay: Overlay<'_>) -> State {
        let hover = overlay.hovered.and_then(|path| {
            let info = HoverInfo::resolve(overlay.scene, path)?;
            let anchor = hover_anchor(
                overlay.scene,
                path,
                overlay.camera,
                overlay.viewport.0,
                overlay.viewport.1,
            )?;
            Some((info, anchor))
        });
        State {
            summary: overlay.summary,
            band: detail_band(overlay.policy, overlay.camera.zoom),
            density: density(overlay.viewport.0, overlay.viewport.1),
            hover,
            edges_on: overlay.edges_on,
            legend_on: overlay.legend_on,
            empty: overlay.scene.rev > 0 && overlay.scene.totals.regions == 0,
        }
    }
}

impl Default for State {
    fn default() -> State {
        State {
            summary: SharedString::default(),
            band: DetailBand::Orbit,
            density: Density::Minimal,
            hover: None,
            edges_on: false,
            legend_on: true,
            empty: false,
        }
    }
}

/// A separate reactive boundary for fixed map furniture. Camera frames notify
/// `MapView`, not this entity, so GPUI can replay the cached toolbar, legend and
/// summary subtree while only the canvas is rebuilt. State changes are explicit
/// and bounded: a semantic-band crossing, a toggle, a resize, or a new hover.
#[derive(Default)]
pub(crate) struct Chrome {
    state: State,
}

impl Chrome {
    pub(crate) fn sync(&mut self, next: State, cx: &mut Context<Self>) {
        if self.replace(next) {
            cx.notify();
        }
    }

    fn replace(&mut self, next: State) -> bool {
        if self.state == next {
            return false;
        }
        self.state = next;
        true
    }
}

#[path = "chrome_view.rs"]
mod view;

#[cfg(test)]
#[path = "chrome_test.rs"]
mod tests;
