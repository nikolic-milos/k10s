use std::time::Duration;

use k10s_clustergen::Scenario;
use k10s_core::{KindId, Op, ReasonId, Severity, State, WorldCtrl, replay};

use crate::PublishBench;
use crate::layout::LayoutMode;
use crate::spawn_world;
use crate::test_support::*;

#[test]
fn an_initial_stream_replays_changes_before_its_first_snapshot() {
    let mut events = replay::initial_sync().events;
    events.push(replay::instance(
        "pod-1",
        "prod",
        "wl-api",
        State::of(ReasonId::CRASH_LOOP_BACK_OFF),
        Op::Modified,
    ));
    events.push(replay::instance(
        "pod-2",
        "prod",
        "wl-api",
        State::OK,
        Op::Deleted,
    ));

    let bench = PublishBench::new(&events, LayoutMode::Spread);
    let snapshot = bench.snapshot();
    assert_eq!(snapshot.totals.cells, 1);
    assert_eq!(
        pod_named(&snapshot, "pod-1").1.ext.state.severity,
        Severity::Err
    );
    assert!(
        snapshot
            .cells
            .iter()
            .all(|pod| pod.label.as_ref() != "pod-2")
    );
}

#[test]
fn publish_hook_fires_per_snapshot_not_per_tick() {
    let scene = k10s_core::new_shared_scene();
    let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let world = spawn_world(
        stream_of(2, 500, Scenario::Platform),
        crossbeam_channel::never(),
        scene.clone(),
        ctrl_rx,
        2,
        0.0,
        LayoutMode::Spread,
        move || {
            let _ = wake_tx.send(());
        },
    );

    wake_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("initial publish must fire the hook");
    assert_eq!(scene.load().rev, 1);
    assert!(
        wake_rx.recv_timeout(Duration::from_millis(400)).is_err(),
        "no rev bump -> no wake"
    );

    ctrl_tx.send(WorldCtrl::Shutdown).unwrap();
    world.join().unwrap();
}

// The seam the launch screen stands on: a window opens on an empty world and a
// scene chosen afterwards replaces it wholesale, laid out by the same batch
// layout the command line's scenes use. The events that were queued for the
// scene being replaced are dropped rather than applied on top of the new one,
// which is what makes the replacement one act instead of a race.
#[test]
fn a_scene_chosen_after_spawn_replaces_an_empty_world_and_then_replaces_itself() {
    let scene = k10s_core::new_shared_scene();
    let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let (live_tx, live_rx) = crossbeam_channel::unbounded();
    let world = spawn_world(
        Vec::new(),
        live_rx,
        scene.clone(),
        ctrl_rx,
        2,
        0.0,
        LayoutMode::Spread,
        move || {
            let _ = wake_tx.send(());
        },
    );
    let settle = |rx: &crossbeam_channel::Receiver<()>| {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("a publish arrives");
        while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
    };
    settle(&wake_rx);
    let empty_rev = scene.load().rev;
    assert_eq!(
        scene.load().totals,
        Default::default(),
        "an empty world publishes an empty scene rather than nothing at all"
    );

    // Something from a scene that is being replaced, queued and never wanted.
    live_tx
        .send(replay::scope("ns-stale", "stale", Op::Added))
        .expect("queued");
    ctrl_tx
        .send(WorldCtrl::Rebuild(replay::initial_sync().events))
        .expect("sent");
    settle(&wake_rx);
    let filled = scene.load_full();
    assert!(
        filled.rev > empty_rev,
        "a replacement stays in the process-wide revision domain"
    );
    let filled_rev = filled.rev;
    assert_eq!(filled.totals.cells, 2);
    assert_eq!(region_named(&filled, "prod").1.label.as_ref(), "prod");
    assert!(
        filled.regions.iter().all(|ns| ns.label.as_ref() != "stale"),
        "what was queued for the old scene must not survive into the new one"
    );
    let built = PublishBench::new(&replay::initial_sync().events, LayoutMode::Spread).snapshot();
    assert_eq!(
        filled.regions[0].rect, built.regions[0].rect,
        "and it is laid out exactly as the same stream is at startup"
    );
    drop(filled);

    ctrl_tx
        .send(WorldCtrl::Rebuild(vec![
            replay::scope("ns-edge", "edge", Op::Added),
            replay::owner("wl-cdn", "edge", "cdn", KindId::DEPLOYMENT, Op::Added),
            replay::instance("pod-cdn", "edge", "wl-cdn", State::OK, Op::Added),
        ]))
        .expect("sent");
    settle(&wake_rx);
    let second = scene.load_full();
    assert!(second.rev > filled_rev);
    assert_eq!(second.totals.regions, 1);
    assert_eq!(second.totals.cells, 1);
    assert_eq!(region_named(&second, "edge").1.label.as_ref(), "edge");
    assert!(
        second.regions.iter().all(|ns| ns.label.as_ref() != "prod"),
        "nothing of the first scene survives into the second"
    );
    drop(second);

    // And the live channel still belongs to whatever is attached now.
    live_tx
        .send(replay::instance(
            "pod-cdn",
            "edge",
            "wl-cdn",
            State::of(ReasonId::CRASH_LOOP_BACK_OFF),
            Op::Modified,
        ))
        .expect("queued");
    settle(&wake_rx);
    assert_eq!(
        pod_named(&scene.load_full(), "pod-cdn")
            .1
            .ext
            .state
            .severity,
        Severity::Err,
        "a rebuilt world still reads its live deltas"
    );

    ctrl_tx.send(WorldCtrl::Shutdown).expect("sent");
    world.join().expect("the world thread ends cleanly");
}

#[test]
fn a_replacement_queued_before_the_first_tick_still_has_a_distinct_revision() {
    let scene = k10s_core::new_shared_scene();
    let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    ctrl_tx
        .send(WorldCtrl::Rebuild(Vec::new()))
        .expect("the replacement is queued before spawn");
    let world = spawn_world(
        Vec::new(),
        crossbeam_channel::never(),
        scene.clone(),
        ctrl_rx,
        2,
        0.0,
        LayoutMode::Spread,
        move || {
            let _ = wake_tx.try_send(());
        },
    );

    wake_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the replacement publishes on the first tick");
    assert_eq!(
        scene.load().rev,
        2,
        "the unseen initial world still owns revision one, so a replacement can never alias it"
    );

    ctrl_tx.send(WorldCtrl::Shutdown).expect("sent");
    world.join().expect("the world thread ends cleanly");
}

// A world spawned before anything has been chosen starts with no churn, so
// whichever choice arrives has to be able to set the rate it needs. Asserted
// in the direction that cannot be flaky: a rate that arrives makes something
// happen, waited for with a generous bound, rather than a rate of zero making
// nothing happen inside an arbitrary window.
#[test]
fn a_churn_rate_set_after_spawn_is_what_the_world_spends() {
    let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let world = spawn_world(
        stream_of(2, 500, Scenario::Platform),
        crossbeam_channel::never(),
        k10s_core::new_shared_scene(),
        ctrl_rx,
        2,
        0.0,
        LayoutMode::Spread,
        move || {
            let _ = wake_tx.send(());
        },
    );
    wake_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the initial publish");
    assert!(
        wake_rx.recv_timeout(Duration::from_millis(400)).is_err(),
        "a world spawned at rate zero invents nothing"
    );

    ctrl_tx.send(WorldCtrl::SetChurnRate(600.0)).unwrap();
    wake_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a rate set after spawn moves pods");

    ctrl_tx.send(WorldCtrl::Shutdown).unwrap();
    world.join().unwrap();
}
