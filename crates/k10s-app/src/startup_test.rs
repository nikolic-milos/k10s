use super::*;
use std::time::Duration;

fn at(start: Instant, milliseconds: u64) -> Instant {
    start + Duration::from_millis(milliseconds)
}

#[test]
fn phases_partition_the_path_to_the_useful_frame() {
    let started = Instant::now();
    let state = State {
        started,
        arguments_parsed: at(started, 1),
        source_ready: Some(at(started, 4)),
        content_ready: Some(at(started, 12)),
        matching_scene_published: Some(at(started, 24)),
        world_spawned: Some(at(started, 6)),
        platform_started: Some(at(started, 10)),
        application_ready: Some(at(started, 14)),
        fonts_ready: Some(at(started, 18)),
        configuration_ready: Some(at(started, 19)),
        window_built: Some(at(started, 20)),
        first_presented: Some(at(started, 28)),
        viewport: Some([1600.0, 1000.0]),
        machine: "test-machine".to_string(),
        platform: "test-platform".to_string(),
        source: "generator",
        generator: Some(GeneratorMeta {
            objects: 25_000,
            seed: 55,
            scenario: "platform".to_string(),
            layout: "spread".to_string(),
            churn_per_second: 0.0,
        }),
        json: true,
    };

    let (report, json) = state
        .report(at(started, 31))
        .expect("all synthetic milestones are present");
    assert!(json);
    assert_eq!(report.phases_ms.argument_parse, 1.0);
    assert_eq!(report.phases_ms.source_prepare, 3.0);
    assert_eq!(report.phases_ms.content_prepare, Some(11.0));
    assert_eq!(report.phases_ms.scene_ready_after_content, Some(12.0));
    assert_eq!(report.phases_ms.world_start, 2.0);
    assert_eq!(report.phases_ms.application_setup, 4.0);
    assert_eq!(report.phases_ms.platform_launch, 4.0);
    assert_eq!(report.phases_ms.font_registration, 4.0);
    assert_eq!(report.phases_ms.configuration, 1.0);
    assert_eq!(report.phases_ms.window_open, 1.0);
    assert_eq!(report.phases_ms.first_present, 8.0);
    assert_eq!(report.phases_ms.useful_after_first, 3.0);
    assert_eq!(report.phases_ms.useful_after_content, Some(19.0));
    assert_eq!(report.phases_ms.useful_after_scene, Some(7.0));
    assert_eq!(report.phases_ms.total, 31.0);
    assert_eq!(report.milestones_ms.useful_presented, 31.0);
}

#[test]
fn an_incomplete_run_is_a_failed_measurement_instead_of_a_partial_number() {
    let started = Instant::now();
    let state = State {
        started,
        arguments_parsed: started,
        source_ready: None,
        content_ready: None,
        matching_scene_published: None,
        world_spawned: None,
        platform_started: None,
        application_ready: None,
        fonts_ready: None,
        configuration_ready: None,
        window_built: None,
        first_presented: None,
        viewport: None,
        machine: "test-machine".to_string(),
        platform: "test-platform".to_string(),
        source: "launch",
        generator: None,
        json: false,
    };

    let error = state
        .report(started)
        .err()
        .expect("missing milestones must reject the report");
    assert!(error.to_string().contains("source_ready"));
}

#[test]
fn generated_content_is_useful_only_after_its_nonempty_snapshot_arrives() {
    let started = Instant::now();
    let args = cli::Args {
        startup_bench: true,
        machine: Some("test-machine".to_string()),
        objects_explicit: true,
        ..Default::default()
    };
    let bench = StartupBench::new(started, started, &args);
    let mut scene = SceneSnapshot::default();
    scene.scene.rev = 1;

    assert!(!bench.scene_is_useful(&scene));
    bench.content_ready();
    assert!(
        !bench.scene_is_useful(&scene),
        "the world's initial empty revision is not generated content"
    );
    scene.scene.rev = 2;
    scene.scene.totals.regions = 1;
    assert!(bench.scene_is_useful(&scene));
}

#[test]
fn an_empty_cluster_is_useful_only_after_it_replaces_the_empty_shell() {
    let started = Instant::now();
    let args = cli::Args {
        cluster: true,
        startup_bench: true,
        machine: Some("test-machine".to_string()),
        ..Default::default()
    };
    let bench = StartupBench::new(started, started, &args);
    let mut scene = SceneSnapshot::default();
    scene.scene.rev = 1;

    bench.content_ready();
    assert!(!bench.scene_is_useful(&scene));
    scene.scene.rev = 2;
    assert!(
        bench.scene_is_useful(&scene),
        "a synced cluster with no visible objects is still a completed scene"
    );
}

#[test]
fn scene_readiness_uses_the_later_side_of_the_worker_publish_race() {
    let started = Instant::now();
    let state = State {
        started,
        arguments_parsed: started,
        source_ready: Some(started),
        content_ready: Some(at(started, 20)),
        matching_scene_published: Some(at(started, 15)),
        world_spawned: Some(started),
        platform_started: Some(started),
        application_ready: Some(started),
        fonts_ready: Some(started),
        configuration_ready: Some(started),
        window_built: Some(started),
        first_presented: Some(at(started, 10)),
        viewport: Some([1600.0, 1000.0]),
        machine: "test-machine".to_string(),
        platform: "test-platform".to_string(),
        source: "generator",
        generator: Some(GeneratorMeta {
            objects: 0,
            seed: 55,
            scenario: "platform".to_string(),
            layout: "spread".to_string(),
            churn_per_second: 0.0,
        }),
        json: true,
    };

    let (report, _) = state
        .report(at(started, 25))
        .expect("both halves of readiness are present");
    assert_eq!(report.milestones_ms.scene_ready, Some(20.0));
    assert_eq!(report.phases_ms.scene_ready_after_content, Some(0.0));
    assert_eq!(report.phases_ms.useful_after_scene, Some(5.0));
}
