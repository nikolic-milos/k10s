use super::*;

#[test]
fn each_milestone_arms_once_and_scene_waits_for_a_published_revision() {
    let mut probe = PresentProbe::first(|_, _| {}).on_scene(|_, _| {});

    let first = probe.take(false);
    assert!(first.first.is_some());
    assert!(first.scene.is_none());
    assert!(probe.take(false).is_empty());

    let scene = probe.take(true);
    assert!(scene.first.is_none());
    assert!(scene.scene.is_some());
    assert!(probe.take(true).is_empty());
}

#[test]
fn a_scene_ready_on_the_first_draw_shares_one_presentation_timestamp() {
    let mut probe = PresentProbe::first(|_, _| {}).on_scene(|_, _| {});
    let callbacks = probe.take(true);

    assert!(callbacks.first.is_some());
    assert!(callbacks.scene.is_some());
    assert!(probe.take(true).is_empty());
}

#[test]
fn the_default_scene_gate_rejects_the_unpublished_snapshot() {
    let probe = PresentProbe::first(|_, _| {}).on_scene(|_, _| {});
    let ready = probe.scene_ready.as_ref().expect("scene gate is installed");
    let mut scene = SceneSnapshot::default();
    assert!(!ready(&scene));
    scene.scene.rev = 1;
    assert!(ready(&scene));
}
