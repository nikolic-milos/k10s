//! That an initial sync conforms, that a 410 mid-watch relists and reaps what
//! vanished, and that a Secret is asked for through the metadata projection
//! where a pod is not.

use crate::*;

const EXPIRED_WATCH_EVENT: &str = r#"{"type":"ERROR","object":{"kind":"Status","apiVersion":"v1","status":"Failure","code":410,"reason":"Expired","message":"too old resource version: 900 (1200)"}}
"#;
fn live_resources(live: &[IngestEvent]) -> Vec<&ResourceEvent> {
    live.iter()
        .filter_map(|e| match e {
            IngestEvent::Resource(r) => Some(r),
            _ => None,
        })
        .collect()
}
#[test]
fn a_scripted_cluster_produces_a_conforming_initial_sync() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let (sync, live) = run_live(&script, options(), |_| {
        script.requests_for("watch=true").len() >= 10
    });
    let report = &sync.report;

    assert!(
        !report.aggregated_discovery,
        "this server has no aggregated document, so the fallback path ran"
    );
    assert_eq!(report.server_version.as_deref(), Some("v1.32.3"));
    assert_eq!(report.kinds_discovered, 10);
    assert_eq!(
        report.kinds_watched, 10,
        "every kind of the watch set this server serves"
    );
    assert_eq!(report.streams, 10, "cluster-wide, so one stream each");
    assert_eq!(report.namespaced_streams, 0);
    assert!(!report.probe_degraded);

    let watches = script.requests_for("watch=true");
    assert_eq!(watches.len(), 10, "one open watch per stream: {watches:?}");
    assert!(report.desyncs.is_empty(), "{:?}", report.desyncs);
    assert!(
        live.is_empty(),
        "a cluster that stops changing emits nothing: {live:?}"
    );

    let (input, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default(), "{stats:?}");
    assert_eq!(input.namespaces.len(), 1);
    let prod = &input.namespaces[0];
    assert_eq!(&*prod.name, "prod");
    assert_eq!(
        prod.workloads.len(),
        1,
        "the Deployment, not the ReplicaSet"
    );
    let api = &prod.workloads[0];
    assert_eq!(&*api.name, "api");
    assert_eq!(api.kind, KindId::DEPLOYMENT);
    assert_eq!(api.tool, k10s_core::ToolId::NGINX, "read from the labels");
    assert_eq!(api.pods.len(), 2);
    assert_eq!(
        api.sats.len(),
        4,
        "service by selector, config map and secret and claim by reference"
    );

    let crashing = resources(&sync)
        .into_iter()
        .find(|r| r.uid.as_ref() == "uid-pod-1")
        .expect("the crashing pod");
    let Payload::Instance { state } = crashing.payload else {
        panic!("expected an instance")
    };
    assert_eq!(state.severity, Severity::Err);
    assert_eq!(
        sync.catalog.reason_display(state.reason),
        "CrashLoopBackOff"
    );
    assert_eq!(state.reason, k10s_core::ReasonId::CRASH_LOOP_BACK_OFF);

    assert_eq!(synced(&sync).len(), 10);
    assert_eq!(capability(&sync, KindId::POD), Some(Capability::Watchable));
    assert_eq!(
        capability(&sync, KindId::SECRET),
        Some(Capability::Watchable)
    );
}
#[test]
fn a_410_mid_watch_relists_and_reaps_what_vanished() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/pods?watch=true", 200, EXPIRED_WATCH_EVENT);
    script.route(
        "GET",
        "/api/v1/pods?",
        200,
        list(&[pod_json("api-1", "uid-pod-1", true)], "Pod"),
    );

    let (sync, live) = run_live(&script, options(), |live| {
        live_resources(live).iter().any(|r| r.op == Op::Deleted)
    });

    assert_eq!(sync.report.assemble.instances, 2);

    let deleted: Vec<&ResourceEvent> = live_resources(&live)
        .into_iter()
        .filter(|r| r.op == Op::Deleted)
        .collect();
    assert_eq!(
        deleted.len(),
        1,
        "one delete, for the one pod the relist did not list: {live:?}"
    );
    assert_eq!(&*deleted[0].uid, "uid-pod-2");
    assert_eq!(
        deleted[0].kind,
        KindId::POD,
        "the kind comes from the copy that went away, not from a guess"
    );
    assert_eq!(
        deleted[0].parent.as_deref(),
        Some("uid-dep"),
        "a delete still says where the object was"
    );

    assert!(
        live_resources(&live)
            .iter()
            .any(|r| &*r.uid == "uid-pod-1" && r.op == Op::Modified),
        "{live:?}"
    );

    let lists = script
        .requests_for("/api/v1/pods?")
        .into_iter()
        .filter(|r| !r.path.contains("watch=true"))
        .count();
    assert_eq!(lists, 2, "the 410 has to relist, not resume");
    let expired = k10s_core::DesyncReason::Expired;
    assert!(
        sync.report.desyncs.contains(&(KindId::POD, expired))
            || live.iter().any(|e| matches!(
                e,
                IngestEvent::Desync { kind, reason } if *kind == KindId::POD && *reason == expired
            )),
        "a 410 is an Expired desync, not a reconnect: {:?} {live:?}",
        sync.report.desyncs
    );
}
#[test]
fn a_secret_is_requested_through_the_metadata_projection_and_a_pod_is_not() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let sync = run(&script, options());

    let secret_requests = script.requests_for("/secrets");
    assert!(
        !secret_requests.is_empty(),
        "the secret list has to have been requested at all"
    );
    for request in &secret_requests {
        assert!(
            request.accept.contains("PartialObjectMetadata"),
            "a Secret was requested as a whole object: {request:?}"
        );
    }

    let pod_requests = script.requests_for("/pods");
    assert!(!pod_requests.is_empty());
    for request in &pod_requests {
        assert!(
            !request.accept.contains("PartialObjectMetadata"),
            "a Pod must be fetched whole or its state is unknowable: {request:?}"
        );
    }

    for request in script.requests_for("/configmaps") {
        assert!(
            request.accept.contains("PartialObjectMetadata"),
            "{request:?}"
        );
    }

    let secret = resources(&sync)
        .into_iter()
        .find(|r| r.kind == KindId::SECRET)
        .expect("the secret is on the map");
    let Payload::Attached { detail, .. } = &secret.payload else {
        panic!("expected an attachment")
    };
    assert_eq!(&*secret.name, "api-token");
    assert!(detail.is_empty());
}

fn synced_kinds(events: &[IngestEvent]) -> Vec<KindId> {
    events
        .iter()
        .filter_map(|event| match event {
            IngestEvent::Synced { kind } => Some(*kind),
            _ => None,
        })
        .collect()
}

// The first frame is geometry. A ConfigMap listing that never answers must not
// hold the namespaces, workloads and pods a person opened the map to see.
#[test]
fn an_attachment_listing_that_never_answers_does_not_hold_the_first_frame() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script.route_hanging("GET", "/api/v1/configmaps?");
    script_lists_except(&script, "/api/v1/configmaps?");

    let started = std::time::Instant::now();
    let sync = run(&script, options());
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "the sync waited on the hanging list: {took:?} against a 5 s timeout"
    );
    let report = &sync.report;
    assert_eq!(
        report.deferred,
        vec![KindId::CONFIG_MAP],
        "{:?}",
        report.deferred
    );
    assert!(
        report.unsettled.is_empty(),
        "a deferred kind is not a timed-out one: {:?}",
        report.unsettled
    );
    assert!(
        !resources(&sync)
            .iter()
            .any(|r| r.kind == KindId::CONFIG_MAP),
        "nothing of the hanging kind was published"
    );
    let synced = synced_kinds(&sync.events);
    assert!(
        !synced.contains(&KindId::CONFIG_MAP),
        "Synced would claim a listing that never finished"
    );
    assert_eq!(
        synced.len(),
        report.kinds_watched - 1,
        "every other kind is synced: {synced:?}"
    );
    assert_eq!(
        report.assemble.instances, 2,
        "the pods are in the first frame"
    );
}

// A listing that finishes after the first publish lands as one batch and then
// says so, rather than as one reconcile per object.
#[test]
fn a_late_attachment_listing_lands_as_one_batch_and_then_says_synced() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script.route_delayed(
        "GET",
        "/api/v1/configmaps?",
        200,
        list(
            &[
                meta("api-config", "uid-cm", Some("prod"), ""),
                meta("web-config", "uid-cm-2", Some("prod"), ""),
            ],
            "ConfigMap",
        ),
        Duration::from_millis(400),
    );
    script_lists_except(&script, "/api/v1/configmaps?");

    let (sync, live) = run_live(&script, options(), |live| {
        synced_kinds(live).contains(&KindId::CONFIG_MAP)
    });
    assert_eq!(sync.report.deferred, vec![KindId::CONFIG_MAP]);
    assert!(
        !resources(&sync)
            .iter()
            .any(|r| r.kind == KindId::CONFIG_MAP),
        "the first frame went out before the list answered"
    );

    // Two were listed; one is mounted by a pod. An attachment nothing
    // references is not drawn, by the same rule the first frame applies, so
    // the batch that lands is the referenced one and only that.
    let arrived: Vec<&ResourceEvent> = live_resources(&live)
        .into_iter()
        .filter(|r| r.kind == KindId::CONFIG_MAP)
        .collect();
    let names: Vec<&str> = arrived.iter().map(|r| r.name.as_ref()).collect();
    assert_eq!(
        names,
        vec!["api-config"],
        "the mounted ConfigMap arrived live: {live:?}"
    );
    assert!(
        arrived.iter().all(|r| r.op == Op::Added),
        "a first listing is additions: {arrived:?}"
    );
    let last = live.last().expect("something arrived");
    assert!(
        matches!(last, IngestEvent::Synced { kind } if *kind == KindId::CONFIG_MAP),
        "Synced closes the batch: {last:?}"
    );
    assert!(
        live.iter().all(|event| match event {
            IngestEvent::Resource(r) => r.kind == KindId::CONFIG_MAP,
            IngestEvent::Synced { kind } => *kind == KindId::CONFIG_MAP,
            _ => false,
        }),
        "nothing else moved: {live:?}"
    );
}
