//! Following logs: a single stream ends labelled and cancels mid-open, a
//! workload follow merges its pods prefixed under one guard, and a denied pod
//! list makes the whole merged follow a labelled denial.

use crate::*;

#[test]
fn a_log_follow_streams_lines_ends_labelled_and_cancels_mid_open() {
    use k10s_data::logs::{LogChunk, LogRequest};

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1/log?",
        200,
        "2026-08-02T05:00:00Z listening on :8080\n2026-08-02T05:00:01Z ready\n2026-08-02T05:00:02Z serving\n",
    );
    script.route_hanging("GET", "/api/v1/namespaces/prod/pods/api-2/log?");
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-3/log?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"pods/log is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let request = |pod: &str| LogRequest {
        namespace: "prod".to_string(),
        pod: pod.to_string(),
        container: None,
        previous: false,
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.follow_log(
        request("api-1"),
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );
    let mut lines: Vec<String> = Vec::new();
    loop {
        match wait(&rx) {
            LogChunk::Lines(batch) => lines.extend(batch),
            LogChunk::Ended { why } => {
                assert_eq!(why, "the stream ended");
                break;
            }
            other => panic!("unexpected chunk {other:?}"),
        }
    }
    assert_eq!(lines.len(), 3);
    assert!(lines[1].ends_with("ready"), "{lines:?}");
    drop(stop);
    let follow = &script.requests_for("/pods/api-1/log")[0];
    assert!(follow.path.contains("follow=true"), "{}", follow.path);
    assert!(follow.path.contains("tailLines=500"), "{}", follow.path);
    assert!(follow.path.contains("timestamps=true"), "{}", follow.path);

    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.follow_log(
        request("api-2"),
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "the held connection produces nothing"
    );
    drop(stop);
    assert_eq!(
        wait(&rx),
        LogChunk::Ended { why: "stopped" },
        "dropping the guard cancels a follow that is still opening"
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.follow_log(
        request("api-3"),
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );
    assert_eq!(wait(&rx), LogChunk::Denied { what: "logs" });

    drop(runtime);
}
#[test]
fn a_workload_follow_merges_its_pods_prefixed_and_one_guard_cancels_them_all() {
    use k10s_data::logs::{LogChunk, WorkloadLogRequest};

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/apis/apps/v1/namespaces/prod/deployments/api",
        200,
        r#"{"metadata":{"name":"api","namespace":"prod","uid":"uid-dep"},
            "spec":{"selector":{"matchLabels":{"app":"api"}},"replicas":2}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod"}},
            {"metadata":{"name":"api-2","uid":"uid-pod-2","namespace":"prod"}}]}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1/log?",
        200,
        "2026-08-02T05:00:00Z listening on :8080\n2026-08-02T05:00:01Z ready\n",
    );
    script.route_hanging("GET", "/api/v1/namespaces/prod/pods/api-2/log?");

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.follow_workload_logs(
        WorkloadLogRequest {
            namespace: "prod".to_string(),
            kind: KindId::DEPLOYMENT,
            name: "api".to_string(),
        },
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );

    // api-1's whole stream plus its ending marker; api-2 is still held open.
    let mut lines: Vec<String> = Vec::new();
    while !lines.iter().any(|l| l.contains("log follow ended")) {
        match wait(&rx) {
            LogChunk::Lines(batch) => lines.extend(batch),
            other => panic!("the merge must still be live: {other:?}"),
        }
    }
    assert!(
        lines.contains(&"2026-08-02T05:00:00Z api-1 listening on :8080".to_string()),
        "each line names its pod after the timestamp: {lines:?}"
    );
    assert!(
        lines.contains(&"api-1 <log follow ended: the stream ended>".to_string()),
        "one pod ending is a line, not the end: {lines:?}"
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "the held pod keeps the merge open and quiet"
    );

    let pod_list = script
        .requests_for("labelSelector")
        .into_iter()
        .next()
        .expect("the pods were found by the workload's selector");
    assert!(
        pod_list.path.contains("labelSelector=app%3Dapi")
            || pod_list.path.contains("labelSelector=app=api"),
        "{}",
        pod_list.path
    );
    assert!(
        pod_list.path.contains("limit=17"),
        "the pod list is bounded at the merge cap plus one: {}",
        pod_list.path
    );
    let follows = script.requests_for("/log?");
    assert_eq!(follows.len(), 2, "one follow per pod: {follows:?}");

    drop(stop);
    let mut saw_final_end = false;
    for _ in 0..4 {
        match wait(&rx) {
            LogChunk::Ended { why } => {
                assert_eq!(why, "every pod follow ended");
                saw_final_end = true;
                break;
            }
            LogChunk::Lines(batch) => {
                assert!(
                    batch.iter().all(|l| l.contains("api-2")),
                    "only the cancelled pod has anything left to say: {batch:?}"
                );
            }
            other => panic!("unexpected chunk {other:?}"),
        }
    }
    assert!(
        saw_final_end,
        "dropping the one guard must stop the held follow too"
    );

    drop(runtime);
}
#[test]
fn a_denied_pod_list_makes_the_merged_follow_a_labelled_denial() {
    use k10s_data::logs::{LogChunk, WorkloadLogRequest};

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/apis/apps/v1/namespaces/prod/deployments/api",
        200,
        r#"{"metadata":{"name":"api","namespace":"prod","uid":"uid-dep"},
            "spec":{"selector":{"matchLabels":{"app":"api"}}}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"pods is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.follow_workload_logs(
        WorkloadLogRequest {
            namespace: "prod".to_string(),
            kind: KindId::DEPLOYMENT,
            name: "api".to_string(),
        },
        Box::new(move |chunk| {
            let _ = tx.send(chunk);
        }),
    );
    assert_eq!(
        wait(&rx),
        LogChunk::Denied { what: "pods" },
        "a denied pod list is a labelled state, not an empty feed"
    );

    drop(runtime);
}
#[test]
fn containers_come_from_the_pod_spec_in_run_order() {
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        200,
        r#"{"metadata":{"name":"api-1","namespace":"prod","uid":"uid-pod-1","resourceVersion":"900"},
            "spec":{"containers":[{"name":"app","image":"nginx"},{"name":"proxy","image":"envoy"}],
                    "initContainers":[{"name":"init-db","image":"flyway"}],
                    "ephemeralContainers":[{"name":"debug","image":"busybox"}]},
            "status":{"phase":"Running"}}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .fetch_containers("prod", "api-1", move |outcome| {
            let _ = tx.send(outcome);
        });
    assert_eq!(
        wait(&rx),
        Fetched::Ok(vec![
            "app".to_string(),
            "proxy".to_string(),
            "init-db".to_string(),
            "debug".to_string(),
        ])
    );

    drop(runtime);
}
