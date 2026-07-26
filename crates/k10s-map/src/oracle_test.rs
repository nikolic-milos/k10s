//! The cull oracle, promoted from a `debug_assert` behind a window to a headless test.
//!
//! The invariant (ROADMAP §6.7): *painter and cull oracle agreeing exactly, swept across zoom
//! stages, blend states and stress flags, in release CI rather than only in a debug build with a
//! window.*
//!
//! How it holds up:
//!
//! * **Same logic, not a copy.** [`crate::frame::walk`] *is* the painter's traversal. `paint_map`
//!   calls it with a [`PaintSink`] and then submits the buffers; this test calls it with a
//!   [`Tally`] that counts and drops. `walk` never reads anything back from its sink, so a sink
//!   cannot influence a counter, and there is no second implementation to drift.
//! * **Release-safe.** Plain `assert_eq!`, so `cargo test --release -p k10s-map` checks it.
//! * **No globals.** The LOD policy used to be a process-wide `OnceLock` fed by eight `K10S_*`
//!   variables, which cannot vary per test and would race across cargo's test threads. It now
//!   arrives as [`FrameOpts`] plus a borrowed `LodPolicy` built by `lod::policy(Knobs)`, a pure
//!   function of the knobs. The process still reads the environment once; nothing under test does.
//!
//! Two of the eight knobs are deliberately absent from the sweep because neither can move a
//! counter: `K10S_REPAINT_ALWAYS` only asks the pacer for another frame, and `K10S_NO_GLOW` only
//! decides whether the sink builds the glow pass. Glow is still exercised, both ways, by
//! [`painter_sink_agrees_with_tally`].

use gpui::{Bounds, PaintQuad, Pixels, point, px, size};
use k10s_atlas::testing::{SceneSpec, scene as base_scene};
use k10s_atlas::{Camera, CullStats, LodPolicy, MAX_ZOOM, MIN_ZOOM, StageBlend};
use k10s_core::{
    KindId, NsExt, NsNode, PodExt, PodNode, ReasonId, SatExt, SatNode, SceneSnapshot, Severity,
    State, ToolId, WlExt, WorkloadNode,
};
use std::sync::Arc;

use crate::frame::{FrameOpts, FrameSink, IconJob, LabelJob, PaintSink, walk};
use crate::lod::{self, Knobs};

/// Pinned logical viewports, so a case is reproducible on any machine (ROADMAP §6.6): the app's
/// requested 1600x1000 and the portrait 1251x1350 the compositor actually handed the baseline run.
/// Two aspect ratios also mean a transposed `vw`/`vh` on either side of the invariant cannot hide.
/// The origin is deliberately not (0, 0): the painter offsets every emitted coordinate by it.
const VIEWPORTS: [(&str, f32, f32); 2] =
    [("1600x1000", 1600.0, 1000.0), ("1251x1350", 1251.0, 1350.0)];
const OX: f32 = 17.0;
const OY: f32 = 23.0;

fn viewport(vw: f32, vh: f32) -> Bounds<Pixels> {
    Bounds {
        origin: point(px(OX), px(OY)),
        size: size(px(vw), px(vh)),
    }
}

/// Counts what the walk emits and drops it. Also refuses non-finite geometry, which would reach
/// lyon and the GPU as silent garbage.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    bg_quads: usize,
    fg_quads: usize,
    labels: usize,
    icons: usize,
    hexes: usize,
    curves: usize,
    edges: usize,
}

fn finite(p: (f32, f32)) -> bool {
    p.0.is_finite() && p.1.is_finite()
}

fn finite_quad(q: &PaintQuad) -> bool {
    let b = q.bounds;
    f32::from(b.origin.x).is_finite()
        && f32::from(b.origin.y).is_finite()
        && f32::from(b.size.width).is_finite()
        && f32::from(b.size.height).is_finite()
}

impl FrameSink for Tally {
    fn bg_quad(&mut self, quad: PaintQuad) {
        assert!(finite_quad(&quad), "non-finite background quad");
        self.bg_quads += 1;
    }

    fn fg_quad(&mut self, quad: PaintQuad) {
        assert!(finite_quad(&quad), "non-finite foreground quad");
        self.fg_quads += 1;
    }

    fn label(&mut self, label: LabelJob) {
        assert!(
            label.x.is_finite() && label.y.is_finite() && label.size_px > 0.0,
            "non-finite label placement"
        );
        self.labels += 1;
    }

    fn icon(&mut self, _icon: IconJob) {
        self.icons += 1;
    }

    fn hex_ring(&mut self, ring: &[(f32, f32); 6]) {
        assert!(ring.iter().copied().all(finite), "non-finite hex ring");
        self.hexes += 1;
    }

    fn curve(&mut self, hub: (f32, f32), ctrl: (f32, f32), sat: (f32, f32)) {
        assert!(
            finite(hub) && finite(ctrl) && finite(sat),
            "non-finite satellite curve"
        );
        self.curves += 1;
    }

    fn edge(&mut self, a: (f32, f32), ctrl: (f32, f32), b: (f32, f32)) {
        assert!(finite(a) && finite(ctrl) && finite(b), "non-finite edge");
        self.edges += 1;
    }
}

/// Give the engine's generic test scene the extensions the painter looks up, cycling through
/// every `Severity`, built-in `KindId`, `ToolId` and `ReasonId` so no colour or glyph branch is
/// unreached, plus ids past the built-in tables so the fallback paths are swept too.
fn snapshot(spec: SceneSpec) -> SceneSnapshot {
    let base = base_scene(spec);
    const SEVERITIES: [Severity; 4] = [
        Severity::Ok,
        Severity::Warn,
        Severity::Err,
        Severity::Unknown,
    ];
    const REASONS: [ReasonId; 5] = [
        ReasonId::RUNNING,
        ReasonId::NOT_READY,
        ReasonId::CRASH_LOOP_BACK_OFF,
        ReasonId::UNKNOWN,
        // Past the built-in table: the severity must degrade to Unknown, never Ok.
        ReasonId(9_001),
    ];
    const KINDS: [KindId; 6] = [
        KindId::DEPLOYMENT,
        KindId::STATEFUL_SET,
        KindId::DAEMON_SET,
        KindId::JOB,
        KindId::CRON_JOB,
        // Stands in for a CRD: no compiled-in colour or glyph, so this sweeps the
        // fallback the whole open model depends on.
        KindId(9_000),
    ];
    const TOOLS: [ToolId; 5] = [
        ToolId::NONE,
        ToolId::POSTGRES,
        ToolId::NONE,
        ToolId::ISTIO,
        ToolId::PROMETHEUS,
    ];
    const SATS: [KindId; 4] = [
        KindId::VOLUME,
        KindId::SERVICE,
        KindId::CONFIG_MAP,
        KindId::SECRET,
    ];

    SceneSnapshot {
        rev: base.rev,
        bounds: base.bounds,
        regions: base
            .regions
            .iter()
            .enumerate()
            .map(|(i, r)| NsNode {
                rect: r.rect,
                label: r.label.clone(),
                weight: r.weight,
                children: r.children.clone(),
                ext: NsExt {
                    // 0.0, 0.15, 0.3, 0.45, 0.6: below, inside and above every heat breakpoint.
                    unhealthy_frac: (i % 5) as f32 * 0.15,
                    rollup: SEVERITIES[i % SEVERITIES.len()],
                },
            })
            .collect(),
        blocks: base
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| WorkloadNode {
                rect: b.rect,
                inner: b.inner,
                label: b.label.clone(),
                children: b.children.clone(),
                sats: b.sats.clone(),
                ext: WlExt {
                    kind: KINDS[i % KINDS.len()],
                    tool: TOOLS[i % TOOLS.len()],
                    rollup: SEVERITIES[i % SEVERITIES.len()],
                    ns: 0,
                },
            })
            .collect(),
        cells: base
            .cells
            .iter()
            .enumerate()
            .map(|(i, c)| PodNode {
                rect: c.rect,
                label: c.label.clone(),
                ext: PodExt {
                    state: State::of(REASONS[i % REASONS.len()]),
                },
            })
            .collect(),
        sats: base
            .sats
            .iter()
            .enumerate()
            .map(|(i, s)| SatNode {
                rect: s.rect,
                label: s.label.clone(),
                ext: SatExt {
                    kind: SATS[i % SATS.len()],
                    detail: Arc::from(format!("{i}Gi").as_str()),
                },
            })
            .collect(),
        edges: base.edges.clone(),
        region_edges: base.region_edges.clone(),
        cross_edges: base.cross_edges.clone(),
        totals: base.totals,
    }
}

/// Cameras spanning every zoom stage plus the boundaries between them, at three framings: the
/// whole scene, one block, and a corner where regions straddle the viewport edge (so the
/// `region_inside` / `block_inside` containment shortcuts are taken both ways).
fn cameras(scene: &SceneSnapshot, vw: f32, vh: f32) -> Vec<(&'static str, Camera)> {
    let b = scene.bounds;
    let (cx, cy) = b.center();
    let mut fitted = Camera::default();
    fitted.fit(b, vw, vh);
    let (bx, by) = scene
        .blocks
        .first()
        .map_or((cx, cy), |blk| blk.inner.center());

    vec![
        ("fit", fitted),
        (
            "z0-min-zoom",
            Camera {
                cx,
                cy,
                zoom: MIN_ZOOM,
            },
        ),
        ("z0", Camera { cx, cy, zoom: 0.05 }),
        (
            "z1-entry",
            Camera {
                cx,
                cy,
                zoom: lod::STAGE_WL,
            },
        ),
        (
            "z1-corner",
            Camera {
                cx: b.x,
                cy: b.y,
                zoom: 0.3,
            },
        ),
        (
            "z2-entry",
            Camera {
                cx,
                cy,
                zoom: lod::STAGE_POD,
            },
        ),
        ("z2-wide", Camera { cx, cy, zoom: 0.7 }),
        (
            "z2-block",
            Camera {
                cx: bx,
                cy: by,
                zoom: 1.0,
            },
        ),
        (
            "z3-entry",
            Camera {
                cx: bx,
                cy: by,
                zoom: lod::STAGE_POD_LABEL,
            },
        ),
        (
            "z3",
            Camera {
                cx: bx,
                cy: by,
                zoom: 8.0,
            },
        ),
        (
            "z3-max-zoom",
            Camera {
                cx: bx,
                cy: by,
                zoom: MAX_ZOOM,
            },
        ),
        (
            "offscreen",
            Camera {
                cx: b.x - b.w - 1.0,
                cy: b.y - b.h - 1.0,
                zoom: 1.0,
            },
        ),
    ]
}

/// Settled stages and every fade the `StageMachine` can produce, including reversals and the
/// two-stage jump. `walk_stage()` is `max(from, to)`, so these pair a low zoom with a high walk
/// stage and vice versa: exactly where a painter that consults `stage` and an oracle that consults
/// the blend would part company.
const BLENDS: [StageBlend; 13] = [
    StageBlend {
        from: 0,
        to: 0,
        t: 1.0,
    },
    StageBlend {
        from: 1,
        to: 1,
        t: 1.0,
    },
    StageBlend {
        from: 2,
        to: 2,
        t: 1.0,
    },
    StageBlend {
        from: 3,
        to: 3,
        t: 1.0,
    },
    StageBlend {
        from: 0,
        to: 1,
        t: 0.0,
    },
    StageBlend {
        from: 0,
        to: 1,
        t: 0.5,
    },
    StageBlend {
        from: 1,
        to: 0,
        t: 0.5,
    },
    StageBlend {
        from: 1,
        to: 2,
        t: 0.0,
    },
    StageBlend {
        from: 1,
        to: 2,
        t: 0.5,
    },
    StageBlend {
        from: 1,
        to: 2,
        t: 1.0,
    },
    StageBlend {
        from: 2,
        to: 1,
        t: 0.5,
    },
    StageBlend {
        from: 2,
        to: 3,
        t: 0.5,
    },
    StageBlend {
        from: 3,
        to: 1,
        t: 0.5,
    },
];

/// All 16 settings of the four LOD knobs. `policy()` collapses the two stress modes when both are
/// set, which is part of what this sweeps.
fn knob_set() -> Vec<Knobs> {
    (0..16u8)
        .map(|m| Knobs {
            stress_quads: m & 1 != 0,
            stress_curves: m & 2 != 0,
            no_curves: m & 4 != 0,
            no_icons: m & 8 != 0,
        })
        .collect()
}

/// `max_labels`, `max_icons`, `max_edges`, `max_curves`. `None` keeps the shipping budgets.
type Caps = Option<(usize, usize, usize, usize)>;

/// Production budgets, and budgets tight enough that every drop path runs on every scene.
const BUDGETS: [(&str, Caps); 3] = [
    ("shipping", None),
    ("tight", Some((7, 3, 2, 5))),
    ("zero", Some((0, 0, 0, 0))),
];

fn with_budgets(mut pol: LodPolicy, caps: Caps) -> LodPolicy {
    if let Some((labels, icons, edges, curves)) = caps {
        pol.max_labels = labels;
        pol.max_icons = icons;
        pol.max_edges = edges;
        pol.max_curves = curves;
    }
    pol
}

/// One combination: what the case was, for a failure message that names it exactly.
struct Case<'a> {
    scene: &'a str,
    view: &'a str,
    vw: f32,
    vh: f32,
    camera: &'a str,
    blend: StageBlend,
    knobs: Knobs,
    budgets: &'a str,
    opts: FrameOpts<'a>,
}

impl std::fmt::Display for Case<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let o = &self.opts;
        write!(
            f,
            "scene {} viewport {} camera {} blend Z{}>Z{}@{:.2} knobs {:?} budgets {} edges_on {} skip_wl {} hex {}",
            self.scene,
            self.view,
            self.camera,
            self.blend.from,
            self.blend.to,
            self.blend.t,
            self.knobs,
            self.budgets,
            o.edges_on,
            o.skip_blocks,
            o.hex,
        )
    }
}

/// Walk one combination and check every invariant the oracle can express.
///
/// Returns the counters so callers can assert that the sweep actually reached the interesting
/// states rather than quietly walking an empty scene a hundred thousand times.
fn check(case: &Case<'_>, scene: &SceneSnapshot, camera: Camera) -> CullStats {
    let opts = case.opts;
    let pol = opts.policy;
    let mut tally = Tally::default();
    let view = viewport(case.vw, case.vh);
    let painted = walk(view, scene, camera, case.blend, opts, &mut tally);
    let oracle = lod::cull(scene, &camera, case.blend, case.vw, case.vh, opts);

    // The invariant. `CullStats` is compared whole, so a field added in Phase B is covered the day
    // it appears instead of the day someone remembers to extend this test.
    assert_eq!(oracle, painted, "cull oracle diverged from painter: {case}");

    // The painter's counters against what the painter actually handed the sink. The oracle
    // re-derives; these two catch a hand-maintained counter drifting from the emit next to it.
    assert_eq!(
        painted.quads,
        tally.bg_quads + tally.fg_quads,
        "quad counter drifted from emitted quads: {case}"
    );
    assert_eq!(
        painted.labels, tally.labels,
        "label counter drifted: {case}"
    );
    assert_eq!(painted.icons, tally.icons, "icon counter drifted: {case}");
    assert_eq!(painted.bg_cells, tally.hexes, "hex counter drifted: {case}");
    assert_eq!(
        painted.curves, tally.curves,
        "curve counter drifted: {case}"
    );
    assert_eq!(painted.edges, tally.edges, "edge counter drifted: {case}");

    // Bounded visible work: every budget is a ceiling, and nothing may be reported as dropped
    // unless the corresponding budget is actually full.
    assert_eq!(
        painted.stage,
        case.blend.walk_stage(),
        "walk stage disagrees with the blend: {case}"
    );
    assert!(
        painted.labels <= pol.max_labels,
        "label budget exceeded: {case}"
    );
    assert!(
        painted.icons <= pol.max_icons,
        "icon budget exceeded: {case}"
    );
    assert!(
        painted.edges <= pol.max_edges,
        "edge budget exceeded: {case}"
    );
    assert!(
        painted.curves <= pol.curve_budget(),
        "curve budget exceeded: {case}"
    );
    if painted.labels_dropped > 0 {
        assert_eq!(
            painted.labels, pol.max_labels,
            "labels dropped below budget: {case}"
        );
    }
    if painted.icons_dropped > 0 {
        assert_eq!(
            painted.icons, pol.max_icons,
            "icons dropped below budget: {case}"
        );
    }
    if painted.curves_dropped > 0 {
        assert_eq!(
            painted.curves,
            pol.curve_budget(),
            "curves dropped below budget: {case}"
        );
    }
    if opts.skip_blocks {
        assert_eq!(
            painted.drawn_blocks, 0,
            "K10S_SKIP_WL still painted blocks: {case}"
        );
    }
    if !opts.hex_shown() {
        assert_eq!(painted.bg_cells, 0, "hex grid not suppressed: {case}");
    }
    painted
}

/// What the sweep managed to reach, so the suite can prove it is not testing an empty scene.
#[derive(Debug, Default)]
struct Reached {
    cases: usize,
    stages: [bool; 4],
    labels_dropped: bool,
    icons_dropped: bool,
    curves_dropped: bool,
    edges_capped: bool,
    hexes: bool,
    curves: bool,
    icons: bool,
    cells: bool,
    empty_view: bool,
    max_quads: usize,
    max_labels: usize,
    max_sats: usize,
    max_cells: usize,
    max_hexes: usize,
    max_edges: usize,
    max_curves: usize,
}

impl Reached {
    fn note(&mut self, st: &CullStats, pol: &LodPolicy) {
        self.cases += 1;
        self.stages[st.stage.min(3) as usize] = true;
        self.labels_dropped |= st.labels_dropped > 0;
        self.icons_dropped |= st.icons_dropped > 0;
        self.curves_dropped |= st.curves_dropped > 0;
        // A zero budget would make "capped" vacuous, so only a non-empty cap counts.
        self.edges_capped |= pol.max_edges > 0 && st.edges == pol.max_edges;
        self.hexes |= st.bg_cells > 0;
        self.curves |= st.curves > 0;
        self.icons |= st.icons > 0;
        self.cells |= st.drawn_cells > 0;
        self.empty_view |= st.drawn_regions == 0;
        self.max_quads = self.max_quads.max(st.quads);
        self.max_labels = self.max_labels.max(st.labels);
        self.max_sats = self.max_sats.max(st.drawn_sats);
        self.max_cells = self.max_cells.max(st.drawn_cells);
        self.max_hexes = self.max_hexes.max(st.bg_cells);
        self.max_edges = self.max_edges.max(st.edges);
        self.max_curves = self.max_curves.max(st.curves);
    }

    fn assert_covered(&self, scene: &str) {
        assert!(self.cases > 0, "{scene}: swept nothing");
        for (stage, hit) in self.stages.iter().enumerate() {
            assert!(hit, "{scene}: never reached stage Z{stage} ({self:?})");
        }
        assert!(self.hexes, "{scene}: never drew a hex ({self:?})");
        assert!(self.curves, "{scene}: never drew a curve ({self:?})");
        assert!(self.icons, "{scene}: never drew an icon ({self:?})");
        assert!(self.cells, "{scene}: never drew a cell ({self:?})");
        assert!(
            self.labels_dropped,
            "{scene}: never hit the label budget ({self:?})"
        );
        assert!(
            self.icons_dropped,
            "{scene}: never hit the icon budget ({self:?})"
        );
        assert!(
            self.curves_dropped,
            "{scene}: never hit the curve budget ({self:?})"
        );
        assert!(
            self.edges_capped,
            "{scene}: never hit the edge budget ({self:?})"
        );
        assert!(
            self.empty_view,
            "{scene}: never looked at empty space ({self:?})"
        );

        // The sweep must actually see a busy frame somewhere, or every assertion above is
        // satisfied by a scene nobody is looking at.
        assert!(self.max_quads >= 200, "{scene}: too few quads ({self:?})");
        assert!(self.max_cells >= 100, "{scene}: too few cells ({self:?})");
        assert!(self.max_sats >= 100, "{scene}: too few sats ({self:?})");
        assert!(self.max_hexes >= 100, "{scene}: too few hexes ({self:?})");
        assert!(self.max_edges >= 10, "{scene}: too few edges ({self:?})");
        assert!(self.max_curves >= 100, "{scene}: too few curves ({self:?})");
        assert!(self.max_labels >= 10, "{scene}: too few labels ({self:?})");
    }
}

/// The full cross product per scene: 2 viewports x 12 cameras x 13 blends x 16 knob settings x 3
/// budget profiles x edges_on x skip_wl x hex = 119,808 combinations, each compared against the
/// oracle whole.
fn sweep(scene_name: &str, scene: &SceneSnapshot) -> Reached {
    let mut reached = Reached::default();
    for (view, vw, vh) in VIEWPORTS {
        for (cam_name, camera) in cameras(scene, vw, vh) {
            for blend in BLENDS {
                for knobs in knob_set() {
                    for (budget_name, budgets) in BUDGETS {
                        let pol = with_budgets(lod::policy(knobs), budgets);
                        for edges_on in [true, false] {
                            for skip_blocks in [false, true] {
                                for hex in [true, false] {
                                    let case = Case {
                                        scene: scene_name,
                                        view,
                                        vw,
                                        vh,
                                        camera: cam_name,
                                        blend,
                                        knobs,
                                        budgets: budget_name,
                                        opts: FrameOpts {
                                            policy: &pol,
                                            edges_on,
                                            skip_blocks,
                                            hex,
                                        },
                                    };
                                    reached.note(&check(&case, scene, camera), &pol);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    reached
}

/// A cluster shaped like the benchmark scenarios: many namespaces, a handful of workloads each.
fn uniform_spec() -> SceneSpec {
    SceneSpec {
        regions: 16,
        blocks_per_region: 9,
        cells_per_block: 8,
        sats_per_block: 3,
        edges_per_region: 6,
    }
}

/// ROADMAP §6.4's fan-out shape: one namespace holding everything.
fn fanout_spec() -> SceneSpec {
    SceneSpec {
        regions: 1,
        blocks_per_region: 400,
        cells_per_block: 6,
        sats_per_block: 3,
        edges_per_region: 40,
    }
}

/// Enough satellites to blow the shipping curve budget, not just the tight one.
fn dense_spec() -> SceneSpec {
    SceneSpec {
        regions: 4,
        blocks_per_region: 32,
        cells_per_block: 16,
        sats_per_block: 14,
        edges_per_region: 30,
    }
}

#[test]
fn oracle_matches_painter_uniform() {
    let scene = snapshot(uniform_spec());
    sweep("uniform", &scene).assert_covered("uniform");
}

#[test]
fn oracle_matches_painter_fanout() {
    let scene = snapshot(fanout_spec());
    sweep("fanout", &scene).assert_covered("fanout");
}

#[test]
fn oracle_matches_painter_dense_satellites() {
    let scene = snapshot(dense_spec());
    sweep("dense", &scene).assert_covered("dense");
}

/// Degenerate scenes: nothing to draw, one of everything, and zero-area rects. No coverage claim
/// here, only that painter and oracle stay in step where the interesting counters are all zero.
#[test]
fn oracle_matches_painter_on_degenerate_scenes() {
    let empty = SceneSnapshot::default();
    let single = snapshot(SceneSpec {
        regions: 1,
        blocks_per_region: 1,
        cells_per_block: 1,
        sats_per_block: 1,
        edges_per_region: 0,
    });
    let childless = snapshot(SceneSpec {
        regions: 3,
        blocks_per_region: 2,
        cells_per_block: 0,
        sats_per_block: 0,
        edges_per_region: 0,
    });
    let mut flat = snapshot(uniform_spec());
    for r in &mut flat.regions {
        r.rect.w = 0.0;
    }
    for b in &mut flat.blocks {
        b.inner.w = 0.0;
        b.inner.h = 0.0;
    }
    for c in &mut flat.cells {
        c.rect.w = 0.0;
    }

    for (name, scene) in [
        ("empty", &empty),
        ("single", &single),
        ("childless", &childless),
        ("flat", &flat),
    ] {
        for (view, vw, vh) in VIEWPORTS {
            let cams = if scene.regions.is_empty() {
                vec![("default", Camera::default())]
            } else {
                cameras(scene, vw, vh)
            };
            for (cam_name, camera) in cams {
                for blend in BLENDS {
                    for knobs in knob_set() {
                        for (budget_name, budgets) in BUDGETS {
                            let pol = with_budgets(lod::policy(knobs), budgets);
                            let case = Case {
                                scene: name,
                                view,
                                vw,
                                vh,
                                camera: cam_name,
                                blend,
                                knobs,
                                budgets: budget_name,
                                opts: FrameOpts {
                                    policy: &pol,
                                    edges_on: true,
                                    skip_blocks: false,
                                    hex: true,
                                },
                            };
                            check(&case, scene, camera);
                        }
                    }
                }
            }
        }
    }
}

/// The real painting sink, headless: same walk, but every primitive is turned into a `PaintQuad`,
/// a `SharedString` label job and lyon path segments, with the glow pass both on and off. This is
/// the `K10S_NO_GLOW` axis, and it proves the sink the app actually uses cannot change a counter.
#[test]
fn painter_sink_agrees_with_tally() {
    let scene = snapshot(uniform_spec());
    let mut bg = Vec::new();
    let mut fg = Vec::new();
    let mut labels = Vec::new();
    let mut icons = Vec::new();

    let mut checked = 0usize;
    let (view, vw, vh) = VIEWPORTS[0];
    for (cam_name, camera) in cameras(&scene, vw, vh) {
        for blend in BLENDS {
            for knobs in knob_set() {
                for (budget_name, budgets) in BUDGETS {
                    let pol = with_budgets(lod::policy(knobs), budgets);
                    for glow in [true, false] {
                        let opts = FrameOpts {
                            policy: &pol,
                            edges_on: true,
                            skip_blocks: false,
                            hex: true,
                        };
                        let case = Case {
                            scene: "uniform",
                            view,
                            vw,
                            vh,
                            camera: cam_name,
                            blend,
                            knobs,
                            budgets: budget_name,
                            opts,
                        };
                        let expected = check(&case, &scene, camera);

                        let mut sink =
                            PaintSink::new(&mut bg, &mut fg, &mut labels, &mut icons, glow);
                        let painted =
                            walk(viewport(vw, vh), &scene, camera, blend, opts, &mut sink);
                        let paths = sink.into_paths();
                        assert_eq!(
                            painted, expected,
                            "the painting sink changed the counters (glow {glow}): {case}"
                        );
                        assert_eq!(
                            painted.quads,
                            bg.len() + fg.len(),
                            "painted quads (glow {glow}): {case}"
                        );
                        assert_eq!(painted.labels, labels.len(), "label jobs: {case}");
                        assert_eq!(painted.icons, icons.len(), "icon jobs: {case}");

                        // Tessellating is the last thing the painter does before submitting, and
                        // the only step that can reject the geometry the walk produced.
                        assert!(paths.hex.build().is_ok(), "hex path: {case}");
                        assert!(paths.edges.build().is_ok(), "edge path: {case}");
                        assert!(paths.curve_core.build().is_ok(), "curve path: {case}");
                        assert_eq!(paths.glow, glow);
                        assert!(paths.curve_glow.build().is_ok(), "glow path: {case}");
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 0);
}

/// The knob interlock the painter depends on: the two stress modes must never both be live, or
/// `block_chrome_shown` and `sat_painted` disagree about which one is in charge.
#[test]
fn stress_modes_are_mutually_exclusive() {
    for knobs in knob_set() {
        let pol = lod::policy(knobs);
        assert!(!(pol.stress && pol.stress_curves), "{knobs:?}");
        assert_eq!(pol.stress, knobs.stress_quads, "{knobs:?}");
        assert_eq!(
            pol.stress_curves,
            knobs.stress_curves && !knobs.stress_quads,
            "{knobs:?}"
        );
    }
}
