//! That a frame's label sites share the scene's `Arc` instead of copying the
//! strings out of it, and that they stop sharing exactly at the inline cap.

use std::sync::Arc;

use k10s_core::{
    NsExt, NsNode, PodExt, PodNode, ReasonId, SatExt, SatNode, State, WlExt, WorkloadNode,
};

use super::*;
use crate::lod::{Knobs, policy};
use k10s_core::SceneData;

const INLINE_CAP: usize = 23;

// The Spread header, which is what `k10s-world` publishes for the shipping
// layout mode. A scene that leaves it at the `Default` zero draws no header
// at all, which is correct for an empty scene and wrong for a fixture.
pub(crate) const CARD_HEADER_FIXTURE: f32 = 26.0;

pub(crate) const VW: f32 = 1600.0;
pub(crate) const VH: f32 = 1000.0;

const CAMERA: Camera = Camera {
    cx: 50.0,
    cy: 50.0,
    zoom: 4.0,
};

pub(crate) fn viewport() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(VW), px(VH)),
    }
}

fn scene(cell_label: &str) -> SceneSnapshot {
    SceneSnapshot {
        ids: Default::default(),
        scene: SceneData {
            rev: 1,
            card_header: CARD_HEADER_FIXTURE,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            regions: vec![NsNode {
                rect: Rect::new(0.0, 0.0, 100.0, 100.0),
                label: Arc::from("payments-production-eu-west"),
                weight: 1,
                children: 0..1,
                ext: NsExt {
                    unhealthy_frac: 0.25,
                    rollup: Severity::Warn,
                },
            }],
            blocks: vec![WorkloadNode {
                rect: Rect::new(10.0, 10.0, 60.0, 60.0),
                inner: Rect::new(10.0, 10.0, 60.0, 60.0),
                label: Arc::from("checkout-api-canary-rollout"),
                children: 0..1,
                sats: 0..1,
                ext: WlExt {
                    kind: KindId::DEPLOYMENT,
                    tool: ToolId::NONE,
                    rollup: Severity::Ok,
                    ns: 0,
                },
            }],
            cells: vec![PodNode {
                rect: Rect::new(12.0, 12.0, 20.0, 20.0),
                label: Arc::from(cell_label),
                ext: PodExt {
                    state: State::of(ReasonId::RUNNING),
                },
            }],
            sats: vec![SatNode {
                rect: Rect::new(75.0, 20.0, 10.0, 10.0),
                label: Arc::from("checkout-api-primary-service"),
                ext: SatExt {
                    kind: KindId::SERVICE,
                    detail: Arc::from("ClusterIP 10.96.0.1:8443/tcp"),
                },
            }],
            ..SceneData::default()
        },
    }
}

#[derive(Default)]
pub(crate) struct Collect {
    pub(crate) labels: Vec<LabelJob>,
    pub(crate) icons: Vec<IconJob>,
    pub(crate) bg: Vec<PaintQuad>,
    pub(crate) fg: Vec<PaintQuad>,
}

impl FrameSink for Collect {
    fn bg_quad(&mut self, quad: PaintQuad) {
        self.bg.push(quad);
    }
    fn fg_quad(&mut self, quad: PaintQuad) {
        self.fg.push(quad);
    }
    fn label(&mut self, label: LabelJob) {
        self.labels.push(label);
    }
    fn icon(&mut self, icon: IconJob) {
        self.icons.push(icon);
    }
    fn hex_ring(&mut self, _: &[(f32, f32); 6]) {}
    fn curve(&mut self, _: (f32, f32), _: (f32, f32), _: (f32, f32)) {}
    fn edge(&mut self, _: (f32, f32), _: (f32, f32), _: (f32, f32)) {}
}

pub(crate) fn icon_bounds(job: &IconJob) -> Bounds<Pixels> {
    match job {
        IconJob::Wl(_, bounds) | IconJob::ToolId(_, bounds) | IconJob::Sat(_, bounds) => *bounds,
    }
}

pub(crate) fn walk_at(scene: &SceneSnapshot, camera: Camera) -> Collect {
    let pol = policy(Knobs::default());
    let opts = FrameOpts {
        theme: &k10s_theme::K10S_DARK.map,
        policy: &pol,
        type_: k10s_theme::Typography::default().map(),
        edges_on: false,
        skip_blocks: false,
        hex: false,
    };
    let blend = StageBlend::settled(pol.stage_for_zoom(camera.zoom));
    let mut sink = Collect::default();
    walk(viewport(), scene, camera, blend, opts, &mut sink);
    sink
}

fn walk_labels(scene: &SceneSnapshot) -> Collect {
    let pol = policy(Knobs::default());
    let opts = FrameOpts {
        theme: &k10s_theme::K10S_DARK.map,
        policy: &pol,
        type_: k10s_theme::Typography::default().map(),
        edges_on: false,
        skip_blocks: false,
        hex: false,
    };
    let blend = StageBlend::settled(pol.stage_for_zoom(CAMERA.zoom));
    let mut sink = Collect::default();
    let st = walk(viewport(), scene, CAMERA, blend, opts, &mut sink);
    assert_eq!(
        st.labels,
        sink.labels.len(),
        "the sink and the counter disagree"
    );
    sink
}

#[test]
fn every_label_site_shares_the_scenes_arc() {
    let scene = scene("checkout-api-7f9c8d6b5-tzq4x");
    let sink = walk_labels(&scene);
    assert_eq!(sink.labels.len(), 5, "the fixture must fire all five sites");

    for (site, label) in [
        ("region", &scene.regions[0].label),
        ("block", &scene.blocks[0].label),
        ("cell", &scene.cells[0].label),
        ("satellite", &scene.sats[0].label),
        ("satellite detail", &scene.sats[0].ext.detail),
    ] {
        assert!(
            label.len() > INLINE_CAP,
            "{site}: a {}-byte fixture label inlines, so it proves nothing",
            label.len()
        );
        assert_eq!(
            Arc::strong_count(label),
            2,
            "{site}: the label was copied instead of shared"
        );
    }
    drop(sink);
}

#[test]
fn a_label_shares_the_arc_only_past_the_inline_cap() {
    for (len, strong) in [(INLINE_CAP, 1), (INLINE_CAP + 1, 2)] {
        let scene = scene(&"p".repeat(len));
        let sink = walk_labels(&scene);
        assert_eq!(
            Arc::strong_count(&scene.cells[0].label),
            strong,
            "{len}-byte cell label"
        );
        drop(sink);
    }
}
