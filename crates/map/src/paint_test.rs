//! Label accounting and displayed counters.

use super::{Grouped, LabelCounts};

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
fn the_hud_groups_thousands_the_way_an_eye_reads_them() {
    assert_eq!(format!("{}", Grouped(0)), "0");
    assert_eq!(format!("{}", Grouped(999)), "999");
    assert_eq!(format!("{}", Grouped(1_000)), "1,000");
    assert_eq!(format!("{}", Grouped(1_234_567)), "1,234,567");
    assert_eq!(format!("{}", Grouped(u32::MAX)), "4,294,967,295");
}
