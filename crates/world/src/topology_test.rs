use std::sync::Arc;

use k10s_core::{IngestEvent, KindId, Op, Payload, ResourceEvent, State, replay};

use crate::test_support::{assert_published_matches_full, workload_named};
use crate::{LayoutMode, PublishBench, SNAPSHOT_POOL_DEPTH, topology};

fn satellite(parent: &str, kind: KindId) -> IngestEvent {
    IngestEvent::Resource(ResourceEvent {
        kind,
        uid: "sat-api".into(),
        namespace: "prod".into(),
        name: "api".into(),
        resource_version: 0,
        parent: Some(parent.into()),
        op: Op::Modified,
        payload: Payload::Attached {
            kind,
            detail: Arc::from(""),
        },
    })
}

fn fixture(linked: bool) -> PublishBench {
    let mut api = replay::owner("wl-api", "prod", "api", KindId::DEPLOYMENT, Op::Added);
    if let IngestEvent::Resource(resource) = &mut api
        && let Payload::Owner { depends_on, .. } = &mut resource.payload
    {
        depends_on.push("wl-side".into());
    }
    let mut events = vec![
        replay::scope("ns-prod", "prod", Op::Added),
        api,
        replay::owner("wl-side", "prod", "side", KindId::DEPLOYMENT, Op::Added),
    ];
    for parent in ["wl-api", "wl-side"] {
        for pod in 0..16 {
            events.push(replay::instance(
                &format!("{parent}-{pod}"),
                "prod",
                parent,
                State::OK,
                Op::Added,
            ));
        }
    }
    if linked {
        events.push(satellite("wl-api", KindId::SERVICE));
    }
    let mut bench = PublishBench::new(&events, LayoutMode::Spread);
    for _ in 0..SNAPSHOT_POOL_DEPTH {
        bench.flip_pods(1);
        bench.run_publish();
    }
    bench
}

#[test]
fn an_empty_namespace_gets_an_empty_edge_range() {
    let mut bench = fixture(true);
    bench.apply_events(&[replay::scope("ns-canary", "canary", Op::Added)]);
    bench.run_publish();
    let after = bench.snapshot();
    assert_eq!(after.region_edges.len(), after.regions.len());
    topology::verify_derived_state(&mut bench.world);
    assert_published_matches_full(&bench.world, &after);
}

#[test]
fn attachment_changes_preserve_dependencies_until_their_targets_change() {
    let mut bench = fixture(true);
    let mut api = replay::owner("wl-api", "prod", "api", KindId::DEPLOYMENT, Op::Modified);
    if let IngestEvent::Resource(resource) = &mut api
        && let Payload::Owner { depends_on, .. } = &mut resource.payload
    {
        depends_on.extend([Arc::from("wl-side"), Arc::from("wl-future")]);
    }
    let batches = [
        vec![api, replay::scope("ns-canary", "canary", Op::Added)],
        vec![replay::owner(
            "wl-future",
            "canary",
            "future",
            KindId::DEPLOYMENT,
            Op::Added,
        )],
        vec![replay::instance(
            "extra",
            "prod",
            "wl-api",
            State::OK,
            Op::Added,
        )],
        vec![replay::instance(
            "extra",
            "prod",
            "wl-api",
            State::OK,
            Op::Deleted,
        )],
        vec![replay::owner(
            "wl-future",
            "canary",
            "future",
            KindId::DEPLOYMENT,
            Op::Deleted,
        )],
        vec![replay::owner(
            "wl-replacement",
            "canary",
            "replacement",
            KindId::DEPLOYMENT,
            Op::Added,
        )],
    ];
    for (batch, cross_edges) in batches.into_iter().zip([0, 1, 1, 1, 0, 0]) {
        bench.apply_events(&batch);
        bench.run_publish();
        topology::verify_derived_state(&mut bench.world);
        let after = bench.snapshot();
        assert_published_matches_full(&bench.world, &after);
        assert_eq!(after.cross_edges.len(), cross_edges);
    }
}

#[test]
fn a_pod_leaving_a_linked_parent_drops_its_old_service_edge() {
    let mut bench = fixture(true);
    let held = bench.snapshot();
    let old_edges = held.edges.clone();
    let old_segments = held.edge_segments.clone();
    let patches = bench.stats().structural_patches;
    bench.apply_events(&[replay::instance(
        "wl-api-0",
        "prod",
        "wl-side",
        State::OK,
        Op::Modified,
    )]);
    bench.run_publish();
    let after = bench.snapshot();
    assert_eq!(after.edges.len(), old_edges.len() - 1);
    assert!(bench.stats().structural_patches > patches);
    topology::verify_derived_state(&mut bench.world);
    assert_published_matches_full(&bench.world, &after);
    assert_eq!(held.edges, old_edges);
    assert_eq!(held.edge_segments, old_segments);
}

#[test]
fn a_satellite_move_refits_both_parents_and_retargets_its_edges() {
    let mut bench = fixture(true);
    let held = bench.snapshot();
    let old_api = workload_named(&held, "api").1.clone();
    assert_ne!(old_api.rect, old_api.inner);
    bench.apply_events(&[satellite("wl-side", KindId::SERVICE)]);
    bench.run_publish();
    let after = bench.snapshot();
    let api = workload_named(&after, "api").1;
    assert_eq!(api.rect, api.inner, "the departed satellite leaves no halo");
    topology::verify_derived_state(&mut bench.world);
    assert_published_matches_full(&bench.world, &after);
    assert_eq!(workload_named(&held, "api").1, &old_api);
}

#[test]
fn changing_a_satellite_kind_rebuilds_its_relations_without_a_move() {
    let mut bench = fixture(true);
    for kind in [KindId::CONFIG_MAP, KindId::VOLUME, KindId::SERVICE] {
        bench.apply_events(&[satellite("wl-api", kind)]);
        bench.run_publish();
        topology::verify_derived_state(&mut bench.world);
        assert_published_matches_full(&bench.world, &bench.snapshot());
    }
}

#[test]
fn a_resized_card_refreshes_geometry_when_the_edge_endpoints_stay_the_same() {
    let mut bench = fixture(false);
    let held = bench.snapshot();
    let edges = held.edges.clone();
    let geometry = held.edge_segments.clone();
    let patches = bench.stats().structural_patches;
    for pod in 16..32 {
        bench.apply_events(&[replay::instance(
            &format!("wl-api-{pod}"),
            "prod",
            "wl-api",
            State::OK,
            Op::Added,
        )]);
        bench.run_publish();
        let after = bench.snapshot();
        assert_eq!(after.edges, edges);
        assert_published_matches_full(&bench.world, &after);
    }
    assert!(bench.stats().structural_patches > patches);
    assert_ne!(bench.snapshot().edge_segments, geometry);
    assert_eq!(held.edge_segments, geometry);
}

#[test]
fn a_batch_places_arrivals_moves_and_reused_slots_without_overlapping() {
    let mut bench = fixture(true);
    let mut events = vec![replay::owner(
        "wl-new",
        "prod",
        "new",
        KindId::DEPLOYMENT,
        Op::Added,
    )];
    for pod in 0..12 {
        events.push(replay::instance(
            &format!("wl-new-{pod}"),
            "prod",
            "wl-new",
            State::OK,
            Op::Added,
        ));
    }
    for (uid, parent, op) in [
        ("wl-api-0", "wl-new", Op::Modified),
        ("wl-api-1", "wl-api", Op::Deleted),
        ("reused", "wl-side", Op::Added),
        ("wl-api-1", "wl-api", Op::Added),
        ("wl-new-0", "wl-api", Op::Modified),
    ] {
        events.push(replay::instance(uid, "prod", parent, State::OK, op));
    }
    bench.apply_events(&events);
    bench.run_publish();
    topology::verify_derived_state(&mut bench.world);
    let after = bench.snapshot();
    assert_published_matches_full(&bench.world, &after);
    assert_eq!(after.totals.cells, 45);
    for (block, owner) in after.blocks.iter().enumerate() {
        let children: Vec<usize> = after.block_cell_indices(block).collect();
        for (position, &pod) in children.iter().enumerate() {
            let rect = after.cells[pod].rect;
            assert!(owner.inner.contains(&rect));
            for &other in &children[position + 1..] {
                assert!(
                    !rect.intersects(&after.cells[other].rect),
                    "pods {pod} and {other} overlap in {}",
                    owner.label
                );
            }
        }
    }
    let new = workload_named(&after, "new").0;
    let children: Vec<&str> = after
        .block_cell_indices(new)
        .map(|pod| after.ids.cells[pod].as_ref())
        .collect();
    assert_eq!(children.len(), 12);
    assert!(children.contains(&"wl-api-0"));
    assert!(!children.contains(&"wl-new-0"));
}
