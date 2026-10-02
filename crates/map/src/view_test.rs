//! What the view counts before it paints: label lines and glyphs stay apart,
//! characters are counted rather than bytes, and an empty scene does not spend
//! the one automatic fit.

use std::sync::Arc;

use k10s_core::{NsExt, NsNode, SceneIds, SceneSnapshot, Severity};

use super::{
    Grouped, KIND_GLYPHS, LabelCounts, MapView, Mark, PickPath, TOOL_GLYPHS, UNKNOWN_GLYPH,
    glyph_of,
};

const POD_LABELS: [&str; 3] = [
    "checkout-api-7f9c8d6b5-tzq4x",
    "postgres-primary-0",
    "otel-collector-agent-vv2mn",
];

#[test]
fn label_counts_keep_lines_and_glyphs_apart() {
    let mut counts = LabelCounts::default();
    for text in POD_LABELS {
        counts.count(text);
    }
    assert_eq!(counts.lines, POD_LABELS.len());
    assert_eq!(
        counts.glyphs,
        POD_LABELS.iter().map(|t| t.chars().count()).sum::<usize>()
    );
    assert!(
        counts.glyphs >= counts.lines,
        "glyphs {} lines {}",
        counts.glyphs,
        counts.lines
    );
    assert_ne!(
        counts.glyphs, counts.lines,
        "a line counter must not be reported as a glyph counter"
    );
}

#[test]
fn label_counts_measure_characters_not_bytes() {
    let text = "naive-wörker-0";
    let mut counts = LabelCounts::default();
    counts.count(text);
    assert_eq!(counts.lines, 1);
    assert_eq!(counts.glyphs, 14);
    assert!(counts.glyphs < text.len());
}

#[test]
fn an_empty_scene_does_not_spend_the_one_automatic_fit() {
    // The incident: a window can now open on an empty world and be filled
    // from the launch screen a moment later. Fitting to nothing marked the
    // view fitted, and the starmap that arrived opened off-camera.
    assert!(!MapView::should_fit_scene(0, false, false, false));
    assert!(!MapView::should_fit_scene(0, false, false, true));
    assert!(MapView::should_fit_scene(197, false, false, false));

    // And the rules that were already true stay true.
    assert!(!MapView::should_fit_scene(197, true, false, false));
    assert!(
        MapView::should_fit_scene(197, true, false, true),
        "a resize re-frames a camera nobody has touched"
    );
    assert!(
        !MapView::should_fit_scene(197, true, true, true),
        "and never one they have"
    );
}

// The two lookups the HUD and the frame lean on, reachable because this module
// is a child of the crate root rather than a module of its own.
#[test]
fn an_id_past_either_glyph_table_falls_back_to_the_unknown_glyph() {
    let (unknown_key, _) = UNKNOWN_GLYPH;
    assert_eq!(glyph_of(KIND_GLYPHS, usize::MAX).0.as_ref(), unknown_key);
    assert_eq!(glyph_of(TOOL_GLYPHS, 9_001).0.as_ref(), unknown_key);
    assert_eq!(
        glyph_of(TOOL_GLYPHS, 0).0.as_ref(),
        unknown_key,
        "tool zero is the catalog's own unknown"
    );
    assert_ne!(glyph_of(KIND_GLYPHS, 0).0.as_ref(), unknown_key);
}

#[test]
fn the_hud_groups_thousands_the_way_an_eye_reads_them() {
    assert_eq!(format!("{}", Grouped(0)), "0");
    assert_eq!(format!("{}", Grouped(999)), "999");
    assert_eq!(format!("{}", Grouped(1_000)), "1,000");
    assert_eq!(format!("{}", Grouped(1_234_567)), "1,234,567");
    assert_eq!(format!("{}", Grouped(u32::MAX)), "4,294,967,295");
}

fn identity_scene(rev: u64, uid: &str) -> SceneSnapshot {
    let mut snapshot = SceneSnapshot::default();
    snapshot.scene.rev = rev;
    snapshot.scene.regions.push(NsNode {
        rect: k10s_core::Rect::new(0.0, 0.0, 100.0, 100.0),
        label: Arc::from("payments"),
        weight: 1,
        children: 0..0,
        ext: NsExt {
            unhealthy_frac: 0.0,
            rollup: Severity::Ok,
        },
    });
    snapshot.ids = Arc::new(SceneIds {
        regions: vec![Arc::from(uid)].into(),
        ..SceneIds::default()
    });
    snapshot
}

#[test]
fn a_mark_follows_the_same_identity_across_publishes() {
    let path = PickPath {
        region: 0,
        block: None,
        cell: None,
        sat: None,
    };
    let mark = Mark::new(&identity_scene(7, "uid-a"), path).expect("region exists");

    assert_eq!(mark.resolve(&identity_scene(8, "uid-a")), Some(path));
}

#[test]
fn a_mark_does_not_jump_when_a_world_slot_is_reused() {
    let path = PickPath {
        region: 0,
        block: None,
        cell: None,
        sat: None,
    };
    let mark = Mark::new(&identity_scene(7, "uid-a"), path).expect("region exists");

    assert_eq!(mark.resolve(&identity_scene(8, "uid-b")), None);
}

#[test]
fn an_identityless_benchmark_mark_is_revision_scoped() {
    let path = PickPath {
        region: 0,
        block: None,
        cell: None,
        sat: None,
    };
    let mark = Mark::new(&identity_scene(7, ""), path).expect("region exists");

    assert_eq!(mark.resolve(&identity_scene(7, "")), Some(path));
    assert_eq!(mark.resolve(&identity_scene(8, "")), None);
}
