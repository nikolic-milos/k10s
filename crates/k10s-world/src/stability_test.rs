use k10s_clustergen::{GenConfig, Scenario, generate};
use k10s_core::{IngestEvent, KindId, Op, State, replay};

use crate::PublishBench;
use crate::layout::LayoutMode;
use crate::test_support::*;
use crate::topology;

// The stability invariant at a scale where the answer is not obvious by
// inspection, and across every structural shape rather than only the
// rolling-update one. A cluster of twelve thousand objects is where a reflow
// would be catastrophic and also where it would be hardest to notice: three
// named rects at three objects cannot tell you that the nine thousandth pod
// stayed put.
//
// Each shape is checked on its own, because they fail differently. A pod
// arriving grows one card; a workload arriving grows one namespace; a
// namespace arriving grows the world and must move no namespace already in
// it; and a delete must not close the gap it leaves by pulling its siblings
// across.
// Scaling a workload up and back down leaves the card the size its contents
// need, not the size they once needed.
//
// Found on a real cluster rather than here: a Deployment taken to eighty
// replicas and back to eight kept the card it had at eighty. Growing was the
// only half that existed, which is correct for arrivals -- a pod must be
// visible the frame it appears -- and left departures with nothing to undo
// it.
//
// The three things this pins are the three that make shrinking safe rather
// than merely tidy: the card comes back down, it never comes down past what
// it still holds, and the workload beside it does not move while that
// happens. The last is the whole layout's reason for existing, and a fit that
// repacked to close the gap would break it.
#[test]
fn a_card_that_grew_for_pods_comes_back_down_when_they_leave() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);

    // A second workload beside the one that will grow, to have something that
    // must not move.
    bench.apply_events(&[replay::owner(
        "wl-side",
        "prod",
        "side",
        KindId::DEPLOYMENT,
        Op::Added,
    )]);
    bench.run_publish();
    let neighbour = workload_named(&bench.snapshot(), "side").1.rect;

    let grown: Vec<IngestEvent> = (0..40)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Added,
            )
        })
        .collect();
    bench.apply_events(&grown);
    bench.run_publish();
    let big = workload_named(&bench.snapshot(), "api").1.inner;

    let gone: Vec<IngestEvent> = (4..40)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Deleted,
            )
        })
        .collect();
    bench.apply_events(&gone);
    bench.run_publish();
    let after = bench.snapshot();
    let (api_slot, api) = workload_named(&after, "api");
    let small = api.inner;

    assert!(
        small.h < big.h,
        "the card kept the height it needed at forty pods: {big:?} -> {small:?}"
    );

    // Never smaller than what is still in it, or a pod that kept its place is
    // clipped by its own card.
    let mut held = 0usize;
    after.for_each_block_cell(api_slot, |_, pod| {
        held += 1;
        assert!(
            small.contains(&pod.rect),
            "a pod that stayed put is outside the shrunk card: {:?} not in {small:?}",
            pod.rect
        );
    });
    assert_eq!(held, 6, "two from the initial sync and four of the burst");

    // Which pods left decides nothing. Fitting alone could only bring the card
    // down as far as the furthest survivor, so deleting from the *front* left a
    // pod at the bottom holding the card open above itself -- the limitation
    // this file used to pin. `repack_pod_grid` is what removes it: eight
    // survivors scattered through a forty-cell grid are fewer than half of it,
    // so the grid is rebuilt tight and the card lands on the same size either
    // way.
    let mut sparse = PublishBench::new(&initial.events, LayoutMode::Spread);
    sparse.apply_events(&grown);
    sparse.run_publish();
    let front: Vec<IngestEvent> = (0..36)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Deleted,
            )
        })
        .collect();
    sparse.apply_events(&front);
    sparse.run_publish();
    let from_the_front = workload_named(&sparse.snapshot(), "api").1.inner;
    assert_eq!(
        (from_the_front.w, from_the_front.h),
        (small.w, small.h),
        "the card that lost its front pods is a different size from the one \
         that lost its back pods"
    );

    assert_eq!(
        workload_named(&after, "side").1.rect,
        neighbour,
        "the workload beside it did not move to close the gap"
    );
    topology::verify_derived_state(&mut bench.world);
}

#[test]
fn an_incremental_change_at_scale_moves_only_what_is_above_it() {
    let spec = generate(&GenConfig {
        seed: 55,
        target_objects: 12_000,
        scenario: Scenario::Platform,
    });
    let events = k10s_clustergen::stream::snapshot(&spec, LayoutMode::Spread.emits_attachments());
    // The busiest namespace, not the first one, and keyed by the namespace
    // *uid* the generator really used rather than by its name. Both halves
    // were wrong in the first draft and both made this pass by leaving it
    // nothing to be wrong about: a namespace holding one workload has no
    // sibling for an arrival to disturb, and `replay::owner` spells a parent
    // as `ns-{name}`, a convention this generator does not follow -- so the
    // arriving workload landed in a fresh namespace of its own, where moving
    // the neighbours it did not have was not observable.
    let mut owners: OwnersByScope = std::collections::HashMap::new();
    for event in &events {
        if let IngestEvent::Resource(resource) = event
            && matches!(resource.payload, k10s_core::Payload::Owner { .. })
            && let Some(scope) = resource.parent.clone()
        {
            owners
                .entry((scope, resource.namespace.clone()))
                .or_default()
                .push(resource.uid.clone());
        }
    }
    let ((scope, namespace), in_namespace) = owners
        .into_iter()
        .max_by_key(|((scope, _), uids)| (uids.len(), scope.clone()))
        .expect("the generated stream has an owner");
    assert!(
        in_namespace.len() > 1,
        "the busiest namespace holds one workload, so an arrival there disturbs nobody"
    );
    let owner = in_namespace[0].clone();
    // Built here rather than through `replay::owner`, because the parent has
    // to be the namespace this stream actually used.
    let arriving_workload = IngestEvent::Resource(k10s_core::ResourceEvent {
        kind: KindId::DEPLOYMENT,
        uid: "wl-scale".into(),
        namespace: namespace.clone(),
        name: "scale".into(),
        resource_version: 0,
        parent: Some(scope),
        op: Op::Added,
        payload: k10s_core::Payload::Owner {
            kind: KindId::DEPLOYMENT,
            tool: k10s_core::ToolId::NONE,
            depends_on: Vec::new(),
        },
    });
    let mut bench = PublishBench::new(&events, LayoutMode::Spread);
    let namespaces_before = bench.snapshot().regions.len();

    let batches: Vec<(&str, Vec<IngestEvent>)> = vec![
        (
            "a pod arrives",
            vec![replay::instance(
                "pod-scale-1",
                &namespace,
                &owner,
                State::OK,
                Op::Added,
            )],
        ),
        ("a workload arrives", vec![arriving_workload]),
        (
            "a namespace arrives with content",
            vec![
                replay::scope("ns-scale", "scale", Op::Added),
                replay::owner(
                    "wl-scale-2",
                    "scale",
                    "scale-2",
                    KindId::DEPLOYMENT,
                    Op::Added,
                ),
                replay::instance("pod-scale-2", "scale", "wl-scale-2", State::OK, Op::Added),
            ],
        ),
        (
            "a pod leaves",
            vec![replay::instance(
                "pod-scale-1",
                &namespace,
                &owner,
                State::OK,
                Op::Deleted,
            )],
        ),
    ];

    for (what, batch) in &batches {
        let before = placement(&bench.snapshot());
        bench.apply_events(batch);
        bench.run_publish();
        // Paired with the equivalence oracle on purpose, and the pairing is
        // load-bearing rather than belt-and-braces. A stability oracle can
        // only see what was published, and at this scale a structural patch
        // rewrites the slots the batch touched and nothing else -- so a
        // layout that moved an untouched sibling *in the topology* would
        // leave the snapshot innocent and this assertion green. That
        // divergence is precisely what comparing the patch to a fresh
        // materialize catches, and neither question covers the other.
        assert_published_matches_full(&bench.world, &bench.snapshot());
        assert_only_ancestors_moved(
            &before,
            &placement(&bench.snapshot()),
            &touched_uids(batch),
            &[],
            what,
        );
    }
    // Exactly one of those four batches introduces a namespace. If the
    // arriving workload had made one of its own -- which is how this test
    // passed for the wrong reason once already -- it would have arrived
    // somewhere with no neighbours to leave alone.
    assert_eq!(
        bench.snapshot().regions.len(),
        namespaces_before + 1,
        "only the namespace batch may add a namespace"
    );
    topology::verify_derived_state(&mut bench.world);
}

#[test]
fn live_topology_uses_stable_slots_without_moving_existing_nodes() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    let before = bench.snapshot();
    let (prod_slot, prod) = region_named(&before, "prod");
    let (api_slot, api) = workload_named(&before, "api");
    let (pod_slot, pod) = pod_named(&before, "pod-1");
    let (prod_rect, api_rect, pod_rect) = (prod.rect, api.inner, pod.rect);
    drop(before);

    let added = [
        replay::scope("ns-canary", "canary", Op::Added),
        replay::owner("wl-canary", "canary", "edge", KindId::DEPLOYMENT, Op::Added),
        replay::instance("pod-canary", "canary", "wl-canary", State::OK, Op::Added),
    ];
    bench.apply_events(&added);
    bench.run_publish();
    let grown = bench.snapshot();
    assert_eq!(grown.totals.regions, 2);
    assert_eq!(grown.totals.blocks, 2);
    assert_eq!(grown.totals.cells, 3);
    assert_eq!(region_named(&grown, "prod").0, prod_slot);
    assert_eq!(workload_named(&grown, "api").0, api_slot);
    assert_eq!(pod_named(&grown, "pod-1").0, pod_slot);
    assert_eq!(grown.regions[prod_slot].rect, prod_rect);
    assert_eq!(grown.blocks[api_slot].inner, api_rect);
    assert_eq!(grown.cells[pod_slot].rect, pod_rect);

    let (canary_slot, _) = region_named(&grown, "canary");
    let (edge_slot, _) = workload_named(&grown, "edge");
    let (canary_pod_slot, _) = pod_named(&grown, "pod-canary");
    assert_eq!(
        grown.region_block_indices(canary_slot).collect::<Vec<_>>(),
        [edge_slot]
    );
    assert_eq!(
        grown.block_cell_indices(edge_slot).collect::<Vec<_>>(),
        [canary_pod_slot]
    );
    drop(grown);

    let deleted = [
        replay::instance("pod-canary", "canary", "wl-canary", State::OK, Op::Deleted),
        replay::owner(
            "wl-canary",
            "canary",
            "edge",
            KindId::DEPLOYMENT,
            Op::Deleted,
        ),
        replay::scope("ns-canary", "canary", Op::Deleted),
    ];
    bench.apply_events(&deleted);
    bench.run_publish();
    let shrunk = bench.snapshot();
    assert_eq!(shrunk.totals.regions, 1);
    assert_eq!(shrunk.totals.blocks, 1);
    assert_eq!(shrunk.totals.cells, 2);
    assert_eq!(shrunk.regions[prod_slot].rect, prod_rect);
    assert_eq!(shrunk.blocks[api_slot].inner, api_rect);
    assert_eq!(shrunk.cells[pod_slot].rect, pod_rect);
    assert!(
        shrunk
            .regions
            .iter()
            .all(|node| node.label.as_ref() != "canary")
    );
    drop(shrunk);

    bench.apply_events(&added);
    bench.run_publish();
    let readded = bench.snapshot();
    assert_eq!(region_named(&readded, "canary").0, canary_slot);
    assert_eq!(workload_named(&readded, "edge").0, edge_slot);
    assert_eq!(pod_named(&readded, "pod-canary").0, canary_pod_slot);
}

#[test]
fn adding_a_pod_grows_its_card_without_moving_occupied_slots() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    let before = bench.snapshot();
    let (api_slot, api) = workload_named(&before, "api");
    let first = pod_named(&before, "pod-1").1.rect;
    let second = pod_named(&before, "pod-2").1.rect;
    let card = api.inner;
    drop(before);

    bench.apply_events(&[replay::instance(
        "pod-3",
        "prod",
        "wl-api",
        State::OK,
        Op::Added,
    )]);
    bench.run_publish();
    let after = bench.snapshot();
    assert_eq!(pod_named(&after, "pod-1").1.rect, first);
    assert_eq!(pod_named(&after, "pod-2").1.rect, second);
    assert!(after.blocks[api_slot].inner.h >= card.h);
    assert_eq!(after.totals.cells, 3);
}

// What a scale-down is supposed to look like, stated as the only thing that
// makes it checkable: the card a Deployment lands on after 80 -> 8 is the card
// the batch engine would have laid out for eight pods from scratch.
//
// That equality is worth more than any absolute number. The two engines had
// drifted -- `layout::layout` lays a square `pod_grid` and the live path
// appends into whatever column count the card already had -- and the drift is
// what made a live cluster look different from the same cluster after a
// reconnect. `repack_pod_grid` closes it on exactly the batches where the shape
// has gone wrong.
#[test]
fn a_scale_down_rebuilds_the_grid_the_batch_engine_would_have_laid_out() {
    use k10s_core::layout::{CARD_HEADER, CARD_PAD, POD_GAP, POD_PITCH};

    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    let burst: Vec<IngestEvent> = (0..78)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Added,
            )
        })
        .collect();
    bench.apply_events(&burst);
    bench.run_publish();
    let grown = workload_named(&bench.snapshot(), "api").1.inner;

    // Every third pod survives, so the survivors are scattered through the grid
    // rather than forming a suffix of it. Fitting alone cannot shrink this at
    // all: the last pod deleted is not the last pod placed.
    let gone: Vec<IngestEvent> = (0..78)
        .filter(|i| i % 13 != 0)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Deleted,
            )
        })
        .collect();
    bench.apply_events(&gone);
    bench.run_publish();
    let after = bench.snapshot();
    let (api_slot, api) = workload_named(&after, "api");
    let small = api.inner;

    let mut held = 0usize;
    after.for_each_block_cell(api_slot, |_, _| held += 1);
    assert_eq!(
        held, 8,
        "six survivors of the burst plus the two initial pods"
    );

    let (columns, rows) = crate::layout::pod_grid(held);
    assert_eq!(
        (small.w, small.h),
        (
            columns as f32 * POD_PITCH - POD_GAP + CARD_PAD * 2.0,
            CARD_HEADER + rows as f32 * POD_PITCH - POD_GAP + CARD_PAD * 2.0,
        ),
        "the card is not the size the batch engine gives eight pods"
    );
    assert!(
        small.w < grown.w && small.h < grown.h,
        "the card did not come down on either axis: {grown:?} -> {small:?}"
    );

    // Tight means tight: eight pods in a 3x3 grid occupy the first eight cells
    // and no cell twice.
    let mut cells: Vec<(u32, u32)> = Vec::new();
    after.for_each_block_cell(api_slot, |_, pod| {
        assert!(small.contains(&pod.rect), "a pod is outside its own card");
        cells.push((
            ((pod.rect.x - small.x - CARD_PAD) / POD_PITCH) as u32,
            ((pod.rect.y - small.y - CARD_HEADER - CARD_PAD) / POD_PITCH) as u32,
        ));
    });
    cells.sort_unstable();
    let mut unique = cells.clone();
    unique.dedup();
    assert_eq!(cells, unique, "two pods share a grid cell");
    let mut first_cells: Vec<(u32, u32)> = (0..held)
        .map(|at| ((at % columns) as u32, (at / columns) as u32))
        .collect();
    first_cells.sort_unstable();
    assert_eq!(
        cells, first_cells,
        "the survivors are not packed into the first cells of the grid"
    );
    topology::verify_derived_state(&mut bench.world);
}

// The counterweight, and the reason the repack waits for a grid to be half
// empty: a rolling update must move nothing at all.
//
// Kubernetes replaces a quarter of the replicas at a time by default, which
// leaves three quarters of the grid occupied, and the pod that arrives takes the
// cell the pod that left gave up. The retiring pods are interleaved through the
// grid here rather than sitting in a block at the end, because a rollout retires
// by age and age stops matching grid position the first time anything restarts --
// a suffix would let the plain fitting pass pass this test on its own.
#[test]
fn a_rolling_update_never_repacks_a_grid() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    bench.apply_events(&[replay::owner(
        "wl-roll",
        "prod",
        "roll",
        KindId::DEPLOYMENT,
        Op::Added,
    )]);
    let pod = |uid: &str, op: Op| replay::instance(uid, "prod", "wl-roll", State::OK, op);
    let fill: Vec<IngestEvent> = (0..48)
        .map(|i| {
            if i % 4 == 0 {
                pod(&format!("gen0-{}", i / 4), Op::Added)
            } else {
                pod(&format!("stable-{i}"), Op::Added)
            }
        })
        .collect();
    bench.apply_events(&fill);
    bench.run_publish();

    let settled = bench.snapshot();
    let card = workload_named(&settled, "roll").1.inner;
    let stable: Vec<(String, k10s_core::Rect)> = (0..48)
        .filter(|i| i % 4 != 0)
        .map(|i| {
            let name = format!("stable-{i}");
            let rect = pod_named(&settled, &name).1.rect;
            (name, rect)
        })
        .collect();
    assert_eq!(stable.len(), 36);
    drop(settled);

    for round in 0..3u32 {
        let retire: Vec<IngestEvent> = (0..12)
            .map(|i| pod(&format!("gen{round}-{i}"), Op::Deleted))
            .collect();
        let arrive: Vec<IngestEvent> = (0..12)
            .map(|i| pod(&format!("gen{}-{i}", round + 1), Op::Added))
            .collect();
        for (phase, batch) in [("retire", &retire), ("arrive", &arrive)] {
            bench.apply_events(batch);
            bench.run_publish();
            let now = bench.snapshot();
            for (name, rect) in &stable {
                assert_eq!(
                    pod_named(&now, name).1.rect,
                    *rect,
                    "round {round} {phase}: {name} moved during a rolling update"
                );
            }
            assert_eq!(
                workload_named(&now, "roll").1.inner,
                card,
                "round {round} {phase}: the card resized during a rolling update"
            );
        }
    }
    topology::verify_derived_state(&mut bench.world);
}

// A workload that never existed at attach time is laid out one pod at a time,
// and `place_pod` widens the grid as it goes. Two things are asserted together
// because either alone is easy to get and useless without the other: the grid
// ends up the shape the batch engine would have chosen, and getting there moved
// no pod even once.
//
// Before `place_pod` could widen, the card was born exactly one pod wide --
// `POD_PITCH == POD_SIZE + POD_GAP` makes that exact for any constants -- and
// `grow_workload` is provably height-only, so this workload was a one-wide,
// fifty-tall ribbon with no way back.
#[test]
fn a_workload_created_live_and_scaled_up_lands_on_the_batch_grid_without_moving_a_pod() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    bench.apply_events(&[replay::owner(
        "wl-live",
        "prod",
        "live",
        KindId::DEPLOYMENT,
        Op::Added,
    )]);
    let mut placed: Vec<(String, k10s_core::Rect)> = Vec::new();
    for i in 0..50 {
        let name = format!("pod-live-{i}");
        bench.apply_events(&[replay::instance(
            &name,
            "prod",
            "wl-live",
            State::OK,
            Op::Added,
        )]);
        bench.run_publish();
        let now = bench.snapshot();
        for (settled, rect) in &placed {
            assert_eq!(
                pod_named(&now, settled).1.rect,
                *rect,
                "{settled} moved when {name} arrived"
            );
        }
        placed.push((name, pod_named(&now, &format!("pod-live-{i}")).1.rect));
    }

    let after = bench.snapshot();
    let (live_slot, live) = workload_named(&after, "live");
    let card = live.inner;
    let mut held = 0usize;
    after.for_each_block_cell(live_slot, |_, cell| {
        held += 1;
        assert!(
            card.contains(&cell.rect),
            "a pod is outside the card it was placed in"
        );
    });
    assert_eq!(held, 50);

    use k10s_core::layout::{CARD_HEADER, CARD_PAD, POD_GAP, POD_PITCH};
    let (columns, rows) = crate::layout::pod_grid(held);
    assert_eq!((columns, rows), (8, 7), "the batch engine's grid for fifty");
    assert_eq!(
        (card.w, card.h),
        (
            columns as f32 * POD_PITCH - POD_GAP + CARD_PAD * 2.0,
            CARD_HEADER + rows as f32 * POD_PITCH - POD_GAP + CARD_PAD * 2.0,
        ),
        "arriving one pod at a time did not converge on the batch card"
    );
    topology::verify_derived_state(&mut bench.world);
}

// The boundary the stability oracle can no longer prove, proved directly.
//
// `assert_only_ancestors_moved` is told which card a batch may rebuild, so it
// stops being evidence about that card. Here the licence is checked from the
// other side: a repack of one workload leaves the workload beside it -- its
// card, its pods, its satellites -- byte-identical, and leaves the two cards in
// the same relative position they were in.
#[test]
fn a_repack_rebuilds_one_card_and_leaves_its_neighbour_alone() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    bench.apply_events(&[replay::owner(
        "wl-side",
        "prod",
        "side",
        KindId::DEPLOYMENT,
        Op::Added,
    )]);
    let fill: Vec<IngestEvent> = (0..40)
        .flat_map(|i| {
            [
                replay::instance(
                    &format!("pod-api-{i}"),
                    "prod",
                    "wl-api",
                    State::OK,
                    Op::Added,
                ),
                replay::instance(
                    &format!("pod-side-{i}"),
                    "prod",
                    "wl-side",
                    State::OK,
                    Op::Added,
                ),
            ]
        })
        .collect();
    bench.apply_events(&fill);
    bench.run_publish();

    let before = placement(&bench.snapshot());
    let side_card = workload_named(&bench.snapshot(), "side").1.inner;

    let drain: Vec<IngestEvent> = (0..38)
        .map(|i| {
            replay::instance(
                &format!("pod-api-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Deleted,
            )
        })
        .collect();
    let touched = touched_uids(&drain);
    bench.apply_events(&drain);
    bench.run_publish();
    let after_snapshot = bench.snapshot();

    assert!(
        workload_named(&after_snapshot, "api").1.inner.h
            < before.rect[&std::sync::Arc::from("wl-api")].h,
        "the batch did not actually repack anything, so this proves nothing"
    );
    assert_eq!(
        workload_named(&after_snapshot, "side").1.inner,
        side_card,
        "the neighbour's card changed size"
    );
    for i in 0..40 {
        let name = format!("pod-side-{i}");
        assert_eq!(
            pod_named(&after_snapshot, &name).1.rect,
            before.rect[&std::sync::Arc::from(name.as_str())],
            "{name} moved when the workload beside it was repacked"
        );
    }
    assert_only_ancestors_moved(
        &before,
        &placement(&after_snapshot),
        &touched,
        &["wl-api"],
        "a drained card beside a full one",
    );
    assert_published_matches_full(&bench.world, &after_snapshot);
    topology::verify_derived_state(&mut bench.world);
}

// Where a pod sits in its card's grid, as one number. The formula is the one
// `place_pod` and `repack_pod_grid` both write with, read backwards.
fn reading_order(card: k10s_core::Rect, rect: k10s_core::Rect) -> u32 {
    use k10s_core::layout::{CARD_HEADER, CARD_PAD, POD_GAP, POD_PITCH};
    let columns = (((card.w - CARD_PAD * 2.0 + POD_GAP) / POD_PITCH).floor() as u32).max(1);
    let column = ((rect.x - card.x - CARD_PAD) / POD_PITCH).round().max(0.0) as u32;
    let row = ((rect.y - card.y - CARD_HEADER - CARD_PAD) / POD_PITCH)
        .round()
        .max(0.0) as u32;
    row * columns + column
}

// A repack slides pods forward; it does not permute them.
//
// Reading order is the whole difference between "the grid got tighter" and "the
// grid got shuffled", and every assertion about a repack's *size* passes either
// way -- a permutation fills the same first cells. Mutating the sort key to slot
// order survived the size assertions, which is why this exists.
#[test]
fn a_repack_slides_pods_forward_and_never_reorders_them() {
    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    let burst: Vec<IngestEvent> = (0..62)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Added,
            )
        })
        .collect();
    bench.apply_events(&burst);
    bench.run_publish();

    // Where each survivor sits before the repack, in reading order.
    let before = bench.snapshot();
    let card = workload_named(&before, "api").1.inner;
    let mut ranked: Vec<(u32, String)> = (0..62)
        .filter(|i| i % 7 == 0)
        .map(|i| {
            let name = format!("pod-burst-{i}");
            let rect = pod_named(&before, &name).1.rect;
            (reading_order(card, rect), name)
        })
        .collect();
    ranked.sort_unstable();
    assert!(ranked.len() >= 6, "too few survivors to have an order");
    drop(before);

    let gone: Vec<IngestEvent> = (0..62)
        .filter(|i| i % 7 != 0)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Deleted,
            )
        })
        .collect();
    bench.apply_events(&gone);
    bench.run_publish();

    let after = bench.snapshot();
    let card = workload_named(&after, "api").1.inner;
    let mut seen: Vec<u32> = ranked
        .iter()
        .map(|(_, name)| reading_order(card, pod_named(&after, name).1.rect))
        .collect();
    let sorted = {
        let mut copy = seen.clone();
        copy.sort_unstable();
        copy
    };
    assert_eq!(
        seen, sorted,
        "the repack reordered the survivors instead of closing the gaps between them"
    );
    seen.dedup();
    assert_eq!(seen.len(), ranked.len(), "two survivors share a cell");
    topology::verify_derived_state(&mut bench.world);
}

// A satellite's ring was sized by the card it orbited when it arrived, and
// nothing ever re-placed it -- so a card that came down left its halo, and
// therefore its namespace, at the size it had at eighty replicas. Rebuilding the
// orbit with the grid is what makes a scale-down visible at the region level
// rather than only inside one card.
#[test]
fn a_repack_brings_the_orbit_and_the_region_down_with_the_card() {
    use k10s_core::{Payload, ResourceEvent};
    let claim = |uid: &str, op: Op| {
        IngestEvent::Resource(ResourceEvent {
            kind: KindId::VOLUME,
            uid: uid.into(),
            namespace: "prod".into(),
            name: uid.into(),
            resource_version: 0,
            parent: Some("wl-api".into()),
            op,
            payload: Payload::Attached {
                kind: KindId::VOLUME,
                detail: std::sync::Arc::from("16Gi"),
            },
        })
    };

    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    let mut grow: Vec<IngestEvent> = (0..62)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Added,
            )
        })
        .collect();
    grow.extend((0..4).map(|i| claim(&format!("pvc-{i}"), Op::Added)));
    bench.apply_events(&grow);
    bench.run_publish();
    let big = bench.snapshot();
    let big_halo = workload_named(&big, "api").1.rect;
    let big_region = region_named(&big, "prod").1.rect;
    drop(big);

    let gone: Vec<IngestEvent> = (4..62)
        .map(|i| {
            replay::instance(
                &format!("pod-burst-{i}"),
                "prod",
                "wl-api",
                State::OK,
                Op::Deleted,
            )
        })
        .collect();
    bench.apply_events(&gone);
    bench.run_publish();
    let after = bench.snapshot();
    let (api_slot, api) = workload_named(&after, "api");
    assert!(
        api.rect.w < big_halo.w && api.rect.h < big_halo.h,
        "the halo kept the orbit it had at sixty-two pods: {big_halo:?} -> {:?}",
        api.rect
    );
    let region = region_named(&after, "prod").1.rect;
    assert!(
        region.w < big_region.w || region.h < big_region.h,
        "the namespace did not come down with the workload inside it: \
         {big_region:?} -> {region:?}"
    );

    // The orbit is still an orbit: every claim is outside the card it belongs to
    // and inside the halo that contains both.
    let mut orbiting = 0usize;
    after.for_each_block_sat(api_slot, |_, sat| {
        orbiting += 1;
        assert!(
            !sat.rect.intersects(&api.inner),
            "a re-orbited satellite landed on top of its own card: {:?}",
            sat.rect
        );
        assert!(
            api.rect.contains(&sat.rect),
            "a re-orbited satellite is outside the halo: {:?} not in {:?}",
            sat.rect,
            api.rect
        );
    });
    assert_eq!(orbiting, 4);
    assert_published_matches_full(&bench.world, &after);
    topology::verify_derived_state(&mut bench.world);
}

// A satellite leaving shrinks the halo it was orbiting. `remove_satellite` set
// the "something departed" flag and then handed the fitting pass nothing to fit,
// so the pass ran, touched no workload, and left the halo -- and the region
// holding it -- at the size the departed satellite had defined.
#[test]
fn a_departing_satellite_shrinks_the_halo_it_was_orbiting() {
    use k10s_core::{Payload, ResourceEvent};
    let claim = |uid: &str, op: Op| {
        IngestEvent::Resource(ResourceEvent {
            kind: KindId::VOLUME,
            uid: uid.into(),
            namespace: "prod".into(),
            name: uid.into(),
            resource_version: 0,
            parent: Some("wl-api".into()),
            op,
            payload: Payload::Attached {
                kind: KindId::VOLUME,
                detail: std::sync::Arc::from("16Gi"),
            },
        })
    };

    let initial = replay::initial_sync();
    let mut bench = PublishBench::new(&initial.events, LayoutMode::Spread);
    bench.apply_events(
        &(0..6)
            .map(|i| claim(&format!("pvc-{i}"), Op::Added))
            .collect::<Vec<_>>(),
    );
    bench.run_publish();
    let full = bench.snapshot();
    let wide = workload_named(&full, "api").1.rect;
    let region = region_named(&full, "prod").1.rect;
    drop(full);

    bench.apply_events(
        &(1..6)
            .map(|i| claim(&format!("pvc-{i}"), Op::Deleted))
            .collect::<Vec<_>>(),
    );
    bench.run_publish();
    let after = bench.snapshot();
    let halo = workload_named(&after, "api").1.rect;
    assert!(
        halo.w < wide.w || halo.h < wide.h,
        "the halo kept the extent of five departed claims: {wide:?} -> {halo:?}"
    );
    assert!(
        region_named(&after, "prod").1.rect != region,
        "the namespace kept the extent of a halo that shrank"
    );
    assert_published_matches_full(&bench.world, &after);
    topology::verify_derived_state(&mut bench.world);
}
