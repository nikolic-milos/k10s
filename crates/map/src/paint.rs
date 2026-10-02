//! Draw prepared frame data using reusable buffers and cached text.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use gpui::{
    App, Bounds, PaintQuad, Pixels, SharedString, TextAlign, TextRun, TransformationMatrix, Window,
    fill, point, px, quad, rgb, size,
};
use k10s_assets::icons::{kind_icon, tool_icon};
use k10s_atlas::{Camera, DrawnCounts, FLIGHT_VIEWPORT, FrameSpans, FrameStats, StageBlend};
use k10s_core::SceneSnapshot;
use k10s_theme::scale_alpha;

use crate::frame::{self, FrameOpts, IconJob, LabelFace, LabelJob, PaintSink};
use crate::lod::lod;
use crate::text::{self, Grouped, TextCache};
use crate::{OverlayFrame, PickPath, hex, mark_primitive, overlay};

fn skip_workloads() -> bool {
    static SKIP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SKIP.get_or_init(|| std::env::var_os("K10S_SKIP_WL").is_some_and(|v| v != "0"))
}

/// The device-pixel grid a logical rect occupies at `scale`.
///
/// Used to decide whether a bounds change is worth another frame: comparing
/// the f32s themselves treats a 0.01 px Hyprland rescale as damage.
pub(super) fn device_px(rect: k10s_core::Rect, scale: f32) -> [i32; 4] {
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    [
        (rect.x * s).round() as i32,
        (rect.y * s).round() as i32,
        (rect.w * s).round() as i32,
        (rect.h * s).round() as i32,
    ]
}

/// Round a paint rect onto the device pixel grid before `paint_svg`.
///
/// gpui keys its SVG atlas on the size of `snap_bounds`, which rounds each
/// edge independently. Two icons of the same ladder size at different
/// subpixel origins then become two tiles, and a fractional scale (1.25 /
/// 1.5) makes that the common case. Rounding origin and size here keeps the
/// atlas key equal to the ladder size the walk already chose. The walk is
/// untouched: only the paint quad moves.
fn snap_to_device(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    if !(scale.is_finite() && scale > 0.0) {
        return bounds;
    }
    let snap = |v: f32| px((v * scale).round() / scale);
    Bounds {
        origin: point(
            snap(f32::from(bounds.origin.x)),
            snap(f32::from(bounds.origin.y)),
        ),
        size: size(
            snap(f32::from(bounds.size.width)),
            snap(f32::from(bounds.size.height)),
        ),
    }
}

fn say_once(message: &'static str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        eprintln!("k10s: {message}");
    }
}

fn letterbox_bounds(canvas: Bounds<Pixels>) -> Bounds<Pixels> {
    let [lw, lh] = FLIGHT_VIEWPORT;
    let cw = f32::from(canvas.size.width);
    let ch = f32::from(canvas.size.height);
    let ox = f32::from(canvas.origin.x) + (cw - lw) * 0.5;
    let oy = f32::from(canvas.origin.y) + (ch - lh) * 0.5;
    Bounds {
        origin: point(px(ox), px(oy)),
        size: size(px(lw), px(lh)),
    }
}

#[cfg(test)]
#[path = "letterbox_test.rs"]
mod letterbox_tests;

fn glow_on() -> bool {
    static GLOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GLOW.get_or_init(|| std::env::var_os("K10S_NO_GLOW").is_none_or(|v| v == "0"))
}

/// The pointer and selection marks, carried into the paint closure as one
/// `Copy` value so the closure captures a fact rather than the view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Marks {
    pub(super) hovered: Option<PickPath>,
    pub(super) selected: Option<PickPath>,
}

impl Marks {
    fn is_empty(&self) -> bool {
        self.hovered.is_none() && self.selected.is_none()
    }
}

// How far the ring stands off the thing it marks and how thick it is, in screen
// pixels. Both are constants rather than fractions of the target: a ring is
// chrome, and chrome that scales with the camera stops reading as chrome. Its
// CORNERS are not constant -- they follow the target, or a ring round an island
// reads as a box drawn over a blob.
const MARK_INSET: f32 = 3.0;
const MARK_WIDTH: f32 = 2.0;
const MARK_HALO_ALPHA: f32 = 0.16;

// Draw whatever the pointer is on and whatever is selected, from the path and
// the camera alone.
//
// Deliberately outside `frame::walk`: the walk's counters are an exact-gated
// benchmark surface and are re-derived by an independently written cull oracle
// on every debug frame, so a ring emitted inside it would have to be reproduced
// in five cull functions and would move committed baselines. Out here it is two
// quads, O(1), and it can be as expensive to look at as it likes.
fn paint_marks(
    bounds: Bounds<Pixels>,
    scene: &SceneSnapshot,
    camera: Camera,
    stage: u8,
    marks: Marks,
    theme: &k10s_theme::MapTheme,
    window: &mut Window,
) {
    let vw = f32::from(bounds.size.width);
    let vh = f32::from(bounds.size.height);
    let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    // Selection under hover: pointing at the selected thing must still show the
    // pointer's own ring, or the map stops responding to the mouse the moment
    // the two coincide.
    for (path, color) in [
        (marks.selected, theme.selection_ring),
        (marks.hovered, theme.hover_ring),
    ] {
        let Some(path) = path else { continue };
        let Some(target) = mark_primitive(scene, path, camera, lod(), stage, (vw, vh), origin)
        else {
            continue;
        };
        let ring = target.outset(MARK_INSET);
        let stroke = rgb(color);
        window.paint_quad(quad(
            ring.bounds,
            ring.corners,
            scale_alpha(stroke, MARK_HALO_ALPHA),
            px(MARK_WIDTH),
            stroke,
            Default::default(),
        ));
    }
}

#[derive(Default, Clone, Copy)]
struct LabelCounts {
    lines: usize,
    glyphs: usize,
}

impl LabelCounts {
    fn count(&mut self, text: &str) {
        self.lines += 1;
        self.glyphs += text.chars().count();
    }
}

#[expect(clippy::too_many_arguments)]
pub(super) fn paint_map(
    canvas_bounds: Bounds<Pixels>,
    scene: &SceneSnapshot,
    camera: Camera,
    blend: StageBlend,
    stats: &Rc<RefCell<FrameStats>>,
    bg_buf: &Rc<RefCell<Vec<PaintQuad>>>,
    fg_buf: &Rc<RefCell<Vec<PaintQuad>>>,
    label_buf: &Rc<RefCell<Vec<LabelJob>>>,
    icon_buf: &Rc<RefCell<Vec<IconJob>>>,
    text_cache: &Rc<RefCell<TextCache>>,
    marks: Marks,
    overlay: &OverlayFrame,
    overlay_cache: &Rc<RefCell<overlay::StampCache>>,
    edges_on: bool,
    churn_on: bool,
    hud_on: bool,
    was_continuous: bool,
    animating: bool,
    bench_letterbox: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let frame_start = std::time::Instant::now();
    stats.borrow_mut().begin_frame(frame_start, was_continuous);
    // One refcount bump per frame, at walk setup and never inside it: the
    // walk itself still reads plain `MapTheme` fields, which is the whole
    // reason that struct is `Copy` scalars.
    let theme = k10s_theme::active(cx).clone();
    let typography = k10s_theme::typography(cx).clone();
    let mut bg = bg_buf.borrow_mut();
    let mut fg = fg_buf.borrow_mut();
    let mut labels = label_buf.borrow_mut();
    let mut icons = icon_buf.borrow_mut();

    let bounds = if bench_letterbox {
        window.paint_quad(fill(canvas_bounds, rgb(theme.map.bg)));
        letterbox_bounds(canvas_bounds)
    } else {
        canvas_bounds
    };

    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let zoom = camera.zoom;
    let block_alpha = blend.stage_alpha(1);
    let cell_alpha = blend.stage_alpha(2);

    let opts = FrameOpts {
        policy: lod(),
        theme: &theme.map,
        type_: typography.map(),
        edges_on,
        skip_blocks: skip_workloads(),
        hex: hex::hex_on(),
    };

    let walk_start = std::time::Instant::now();
    let mut sink = PaintSink::new(&mut bg, &mut fg, &mut labels, &mut icons, glow_on());
    let counts = frame::walk(bounds, scene, camera, blend, opts, &mut sink);
    let paths = sink.into_paths();

    #[cfg(debug_assertions)]
    {
        let vw = f32::from(bounds.size.width);
        let vh = f32::from(bounds.size.height);
        debug_assert_eq!(
            crate::lod::cull(scene, &camera, blend, vw, vh, opts),
            counts,
            "cull oracle diverged from painter"
        );
        debug_assert_eq!(
            counts.quads,
            bg.len() + fg.len(),
            "quad counter drifted from the quads actually emitted"
        );
        debug_assert_eq!(counts.labels, labels.len(), "label counter drifted");
        debug_assert_eq!(counts.icons, icons.len(), "icon counter drifted");
    }

    let bg_quads_start = std::time::Instant::now();
    window.paint_quads(&bg);

    let paths_start = std::time::Instant::now();
    if counts.bg_cells > 0 {
        let hex = paths.hex.build();
        debug_assert!(hex.is_ok(), "hex layer failed to tessellate");
        if let Ok(path) = hex {
            window.paint_path(path, rgb(theme.map.hex_line).alpha(hex::level(zoom).1));
        }
    }

    if counts.edges > 0 {
        let edges = paths.edges.build();
        debug_assert!(edges.is_ok(), "edge layer failed to tessellate");
        if let Ok(path) = edges {
            window.paint_path(path, rgb(theme.map.edge).alpha(0.30 * cell_alpha));
        }
    }

    if counts.curves > 0 {
        if paths.glow {
            let glow = paths.curve_glow.build();
            debug_assert!(glow.is_ok(), "curve glow layer failed to tessellate");
            if let Ok(path) = glow {
                window.paint_path(
                    path,
                    rgb(theme.map.curve_glow).alpha(theme.map.curve_glow_alpha * cell_alpha),
                );
            }
        }
        let core = paths.curve_core.build();
        debug_assert!(core.is_ok(), "curve core layer failed to tessellate");
        if let Ok(path) = core {
            window.paint_path(
                path,
                rgb(theme.map.curve_core).alpha(theme.map.curve_core_alpha * cell_alpha),
            );
        }
    }

    let fg_quads_start = std::time::Instant::now();
    window.paint_quads(&fg);
    // Overlay stamps sit outside `frame::walk`, the same way hover rings do:
    // a post-pass over the bounded mark table, keyed by uid, only for objects
    // the camera already has on screen. CullStats does not grow a field.
    if !overlay.marks.is_empty() {
        let vw = f32::from(bounds.size.width);
        let vh = f32::from(bounds.size.height);
        let stamps = overlay.visible_stamps_cached(
            &mut overlay_cache.borrow_mut(),
            scene,
            camera,
            lod(),
            blend,
            vw,
            vh,
        );
        overlay::paint_stamps(
            &stamps,
            (f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
            &theme.map,
            window,
        );
    }
    if !marks.is_empty() {
        paint_marks(
            bounds,
            scene,
            camera,
            blend.walk_stage(),
            marks,
            &theme.map,
            window,
        );
    }

    let icons_start = std::time::Instant::now();
    if !icons.is_empty() {
        let wl_icon_color: gpui::Hsla =
            scale_alpha(rgb(theme.map.wl_icon), 0.95 * block_alpha).into();
        let scale = window.scale_factor();
        window.paint_layer(bounds, |window| {
            for job in icons.iter() {
                let (key, data, icon_bounds, color) = match job {
                    IconJob::Wl(kind, primitive) => {
                        let (key, data) = kind_icon(*kind);
                        (
                            key,
                            data,
                            snap_to_device(primitive.bounds, scale),
                            wl_icon_color,
                        )
                    }
                    IconJob::ToolId(tool, primitive) => {
                        let (key, data) = tool_icon(*tool);
                        (
                            key,
                            data,
                            snap_to_device(primitive.bounds, scale),
                            scale_alpha(theme.map.tool_color(*tool), 0.95 * block_alpha).into(),
                        )
                    }
                    IconJob::Sat(kind, primitive) => {
                        let (key, data) = kind_icon(*kind);
                        (
                            key,
                            data,
                            snap_to_device(primitive.bounds, scale),
                            scale_alpha(theme.map.kind_color(*kind), cell_alpha).into(),
                        )
                    }
                };
                if window
                    .paint_svg(
                        icon_bounds,
                        key,
                        Some(data),
                        TransformationMatrix::unit(),
                        color,
                        cx,
                    )
                    .is_err()
                {
                    say_once(
                        "a map icon failed to paint; further failures this process are silent",
                    );
                }
            }
        });
    }

    let text_start = std::time::Instant::now();
    let ui_font = gpui::font(typography.ui_family.clone());
    let display_font = gpui::font(typography.display_family.clone());
    let line_factor = typography.map().line_height;
    let mut label_counts = LabelCounts::default();
    let mut cache = text_cache.borrow_mut();
    let cache_before = cache.stats();
    for job in labels.iter() {
        let font = match job.face {
            LabelFace::Ui => &ui_font,
            LabelFace::Display => &display_font,
        };
        let line = cache.shape_label(
            job.text.clone(),
            font,
            job.size_px,
            job.color.into(),
            blend.is_settled(),
            window.text_system(),
        );
        let origin = point(px(job.x), px(job.y));
        let line_height = px(job.size_px * line_factor);
        // A label with a box is centred in it and clipped to it; one without is
        // set left and runs, which is what a pod cell's name wants. Clipping is
        // a content mask rather than an ellipsis because building the elided
        // string would allocate once per label per frame, and the walk's
        // zero-allocation ratchet is worth more than three dots.
        let painted = if job.width > 0.0 {
            let mask = gpui::ContentMask {
                bounds: Bounds {
                    origin: point(origin.x, origin.y - line_height),
                    size: size(px(job.width), line_height * 3.0),
                },
            };
            // Centred while it fits, set left the moment it does not. Centring
            // an overlong name clips it at BOTH ends, and a Kubernetes name is
            // informative from the front: `payments-redis-primary` cut to
            // `ments-redis-pri` names nothing. The line is already shaped, so
            // this costs a comparison and no second pass.
            let align = if line.width() > px(job.width) {
                TextAlign::Left
            } else {
                TextAlign::Center
            };
            window.with_content_mask(Some(mask), |window| {
                line.paint(origin, line_height, align, Some(px(job.width)), window, cx)
            })
        } else {
            line.paint(origin, line_height, TextAlign::Left, None, window, cx)
        };
        if painted.is_ok() {
            label_counts.count(&job.text);
        }
    }
    let cache_delta = cache.stats().since(cache_before);
    drop(cache);
    let text_end = std::time::Instant::now();

    {
        let mut st = stats.borrow_mut();
        st.quads = counts.quads;
        st.lines = label_counts.lines;
        st.glyphs = label_counts.glyphs;
        st.edges = counts.edges;
        st.icons = counts.icons;
        st.sats = counts.drawn_sats;
        st.curves = counts.curves;
        st.curves_dropped = counts.curves_dropped;
        st.bg_cells = counts.bg_cells;
        st.drawn = DrawnCounts {
            regions: counts.drawn_regions,
            blocks: counts.drawn_blocks,
            cells: counts.drawn_cells,
        };
        st.labels_dropped = counts.labels_dropped;
        st.icons_dropped = counts.icons_dropped;
        st.text_cache.hits = cache_delta.hits;
        st.text_cache.misses = cache_delta.misses;
        st.text_cache.evictions = cache_delta.evictions;
        st.end_cpu(frame_start);
        st.commit_counters();
    }

    let hud_start = std::time::Instant::now();
    paint_hud(
        scene,
        &theme,
        &typography,
        stats,
        camera.zoom,
        blend,
        edges_on,
        churn_on,
        hud_on,
        animating,
        ox,
        oy,
        text_cache,
        window,
        cx,
    );
    let hud_end = std::time::Instant::now();

    stats.borrow_mut().push_spans(FrameSpans {
        walk_us: span_us(walk_start, bg_quads_start),
        quads_us: span_us(bg_quads_start, paths_start) + span_us(fg_quads_start, icons_start),
        paths_us: span_us(paths_start, fg_quads_start),
        icons_us: span_us(icons_start, text_start),
        text_us: span_us(text_start, text_end),
        hud_us: span_us(hud_start, hud_end),
    });
}

fn span_us(from: std::time::Instant, to: std::time::Instant) -> f32 {
    (to - from).as_secs_f32() * 1_000_000.0
}

#[expect(clippy::too_many_arguments)]
fn paint_hud(
    scene: &SceneSnapshot,
    theme: &k10s_theme::Theme,
    typography: &k10s_theme::Typography,
    stats: &Rc<RefCell<FrameStats>>,
    zoom: f32,
    blend: StageBlend,
    edges_on: bool,
    churn_on: bool,
    hud_on: bool,
    animating: bool,
    ox: f32,
    oy: f32,
    text_cache: &Rc<RefCell<TextCache>>,
    window: &mut Window,
    cx: &mut App,
) {
    if !hud_on {
        return;
    }

    let st = stats.borrow();
    let (fp50, fp95, fp99) = st.frame_percentiles();
    let (cp50, cp99) = st.cpu_percentiles();
    let t = scene.totals;
    let mut cache = text_cache.borrow_mut();
    let lines = cache.hud_lines_mut();
    for line in lines.iter_mut() {
        line.clear();
    }
    write!(
        lines[0],
        "k10s starmap [rev {}]  {} ns / {} wl / {} pods / {} sats / {} edges",
        scene.rev,
        Grouped(t.regions),
        Grouped(t.blocks),
        Grouped(t.cells),
        Grouped(t.sats),
        Grouped(t.edges),
    )
    .expect("writing to a String is infallible");
    write!(
        lines[1],
        "frame  p50 {fp50:.1}  p95 {fp95:.1}  p99 {fp99:.1} ms   (~{:.0} {})",
        if fp50 > 0.0 { 1000.0 / fp50 } else { 0.0 },
        if animating { "fps" } else { "paints/s" },
    )
    .expect("writing to a String is infallible");
    write!(
        lines[2],
        "paint cpu  p50 {cp50:.2}  p99 {cp99:.2} ms  |  text cache {}H/{}M",
        st.text_cache.hits, st.text_cache.misses,
    )
    .expect("writing to a String is infallible");
    if blend.is_settled() {
        write!(lines[3], "zoom {zoom:.3}  stage Z{}", blend.to)
    } else {
        write!(
            lines[3],
            "zoom {zoom:.3}  stage Z{}>Z{}",
            blend.from, blend.to
        )
    }
    .expect("writing to a String is infallible");
    write!(
        lines[3],
        "  |  quads {}  lines {}  glyphs {}  icons {}  edges {}  dropped {}L/{}I",
        Grouped(st.quads as u32),
        st.lines,
        Grouped(st.glyphs as u32),
        st.icons,
        st.edges,
        st.labels_dropped,
        st.icons_dropped,
    )
    .expect("writing to a String is infallible");
    write!(lines[4], "sats {}  curves {}", st.sats, st.curves)
        .expect("writing to a String is infallible");
    if st.curves_dropped > 0 {
        write!(lines[4], " (-{})", st.curves_dropped).expect("writing to a String is infallible");
    }
    write!(
        lines[4],
        "  hex {}  |  drawn ns {} wl {} pods {}",
        st.bg_cells,
        st.drawn.regions,
        st.drawn.blocks,
        Grouped(st.drawn.cells as u32),
    )
    .expect("writing to a String is infallible");
    write!(
        lines[5],
        "[c]hurn {}  [e]dges {}  [f]it  [h]ide",
        if churn_on { "on" } else { "off" },
        if edges_on { "on" } else { "off" },
    )
    .expect("writing to a String is infallible");
    drop(st);

    let pad = 10.0;
    let line_h = 16.0;
    let hud_bounds = Bounds {
        origin: point(px(ox + 12.0), px(oy + 12.0)),
        size: size(px(600.0), px(2.0 * pad + line_h * lines.len() as f32)),
    };
    window.paint_quad(quad(
        hud_bounds,
        px(6.0),
        rgb(theme.map.hud_bg).alpha(0.88),
        px(1.0),
        rgb(theme.map.ns_border),
        Default::default(),
    ));

    let font = gpui::font(typography.buffer_family.clone());
    for (i, text) in lines.iter().enumerate() {
        let hash = text::content_hash(text);
        let run = TextRun {
            len: text.len(),
            font: font.clone(),
            color: rgb(theme.map.hud_text).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window.text_system().shape_line_by_hash(
            hash,
            text.len(),
            px(11.0),
            &[run],
            None,
            || SharedString::from(text.as_str()),
        );
        let _ = line.paint(
            point(px(ox + 12.0 + pad), px(oy + 12.0 + pad + i as f32 * line_h)),
            px(line_h),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

#[cfg(test)]
#[path = "paint_test.rs"]
mod tests;
