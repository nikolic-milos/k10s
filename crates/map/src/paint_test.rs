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

fn overview_job(text: &str) -> crate::frame::LabelJob {
    crate::frame::LabelJob {
        text: text.to_owned().into(),
        x: 0.0,
        y: 0.0,
        size_px: 14.0,
        color: gpui::rgb(k10s_theme::K10S_DARK.map.region_label),
        face: crate::frame::LabelFace::Display,
        width: 1000.0,
        height: 100.0,
        text_start: 0,
        whole_line: true,
        prepared: None,
    }
}

fn protocol_text_system() -> gpui::WindowTextSystem {
    use std::sync::Arc;
    gpui::WindowTextSystem::new(Arc::new(gpui::TextSystem::new(Arc::new(
        gpui::NoopTextSystem::new(),
    ))))
}

#[test]
fn whole_line_admission_uses_the_retained_line_and_never_counts_a_refusal_as_paint() {
    // These synthetic metrics test admission boundaries, not shipped font fit.
    let system = protocol_text_system();
    let mut cache = super::TextCache::default();
    let mut jobs = [overview_job("12345 workloads")];
    super::prepare_labels(
        &mut jobs,
        &k10s_theme::Typography::default(),
        &mut cache,
        true,
        &system,
    );
    let line = jobs[0].prepared.as_ref().unwrap().clone();
    assert!(line.width() > gpui::px(0.0));
    jobs[0].width = line.width().into();
    jobs[0].height = (line.ascent + line.descent).into();
    let before = cache.stats();
    let mut counts = LabelCounts::default();
    assert!(counts.fits(&jobs[0], &line, crate::frame::tests::viewport()));
    jobs[0].width -= 0.01;
    assert!(!counts.fits(&jobs[0], &line, crate::frame::tests::viewport()));
    jobs[0].width += 0.01;
    jobs[0].height -= 0.01;
    assert!(!counts.fits(&jobs[0], &line, crate::frame::tests::viewport()));
    assert_eq!((counts.lines, counts.glyphs, counts.fit_refused), (0, 0, 2));
    assert_eq!(jobs[0].prepared.as_ref().unwrap().text, "12345 workloads");
    assert_eq!(
        cache.stats(),
        before,
        "admission must not shape or re-enter the cache"
    );
    jobs[0].whole_line = false;
    assert!(
        counts.fits(&jobs[0], &line, crate::frame::tests::viewport()),
        "legacy clipped labels retain their contract"
    );
}

#[test]
fn preparation_retains_an_early_hit_after_later_jobs_evict_it_from_fifo() {
    let system = protocol_text_system();
    let typography = k10s_theme::Typography::default();
    let mut cache = super::TextCache::default();
    for index in 0..1024 {
        let mut job = [overview_job(&format!("namespace-{index}"))];
        super::prepare_labels(&mut job, &typography, &mut cache, true, &system);
    }
    assert_eq!(cache.stats().evictions, 0);
    let mut jobs = [overview_job("namespace-0"), overview_job("namespace-new")];
    let before = cache.stats();
    super::prepare_labels(&mut jobs, &typography, &mut cache, true, &system);
    let delta = cache.stats().since(before);
    assert_eq!((delta.hits, delta.misses, delta.evictions), (1, 1, 1));
    let retained = jobs[0].prepared.as_ref().unwrap();
    assert_eq!(retained.text, "namespace-0");
    let before_draw = cache.stats();
    assert!(LabelCounts::default().fits(&jobs[0], retained, crate::frame::tests::viewport()));
    assert_eq!(cache.stats(), before_draw);
    let mut probe = [overview_job("namespace-0")];
    super::prepare_labels(&mut probe, &typography, &mut cache, true, &system);
    assert_eq!(
        cache.stats().since(before_draw).misses,
        1,
        "the retained line was actually evicted"
    );
    assert_eq!(
        retained.text, "namespace-0",
        "the prepared handle survives eviction"
    );
}

#[test]
fn moving_preparation_keeps_cold_warm_and_unsettled_cache_work_visible() {
    let system = protocol_text_system();
    let typography = k10s_theme::Typography::default();
    let mut cache = super::TextCache::default();
    let mut jobs = [overview_job("prod"), overview_job("2 workloads")];
    let cold = cache.stats();
    super::prepare_labels(&mut jobs, &typography, &mut cache, true, &system);
    assert_eq!(cache.stats().since(cold).misses, 2);
    let cold_owners = jobs
        .each_ref()
        .map(|job| job.prepared.as_ref().unwrap().clone());
    let warm = cache.stats();
    super::prepare_labels(&mut jobs, &typography, &mut cache, true, &system);
    assert_eq!(cache.stats().since(warm).hits, 2);
    for (job, owner) in jobs.iter().zip(&cold_owners) {
        assert!(std::rc::Rc::ptr_eq(job.prepared.as_ref().unwrap(), owner));
    }
    let unsettled = cache.stats();
    super::prepare_labels(&mut jobs, &typography, &mut cache, false, &system);
    assert_eq!(cache.stats().since(unsettled).misses, 2);
    assert!(jobs.iter().all(|job| job.prepared.is_some()));
    for (job, owner) in jobs.iter().zip(&cold_owners) {
        assert!(!std::rc::Rc::ptr_eq(job.prepared.as_ref().unwrap(), owner));
    }
    cache.set_enabled(false);
    let disabled = cache.stats();
    super::prepare_labels(&mut jobs, &typography, &mut cache, true, &system);
    assert_eq!(cache.stats().since(disabled).misses, 2);
    for (job, owner) in jobs.iter().zip(&cold_owners) {
        assert!(!std::rc::Rc::ptr_eq(job.prepared.as_ref().unwrap(), owner));
    }
}

#[test]
fn whole_count_fit_also_requires_its_actual_centered_span_to_clear_the_viewport() {
    let system = protocol_text_system();
    let mut cache = super::TextCache::default();
    let mut jobs = [overview_job("12345 workloads")];
    super::prepare_labels(
        &mut jobs,
        &k10s_theme::Typography::default(),
        &mut cache,
        true,
        &system,
    );
    let line = jobs[0].prepared.as_ref().unwrap().clone();
    let mut counts = LabelCounts::default();
    let viewport = crate::frame::tests::viewport();
    jobs[0].x = -jobs[0].width * 0.5;
    assert!(!counts.fits(&jobs[0], &line, viewport));
    jobs[0].x += f32::from(line.width()) * 0.5;
    assert!(counts.fits(&jobs[0], &line, viewport));
    jobs[0].x = 1600.0 - (jobs[0].width + f32::from(line.width())) * 0.5;
    assert!(counts.fits(&jobs[0], &line, viewport));
    jobs[0].x += 0.01;
    assert!(!counts.fits(&jobs[0], &line, viewport));
    assert_eq!((counts.lines, counts.glyphs, counts.fit_refused), (0, 0, 2));
}
