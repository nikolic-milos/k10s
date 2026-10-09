//! One pass of the inspector over a scripted cluster: what it reads, what it
//! logs, and the denial it labels rather than retries.

use crate::*;

#[test]
fn the_inspector_reads_events_and_logs_and_labels_a_denial() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        200,
        r#"{"kind":"EventList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"name":"e1","namespace":"prod"},"type":"Warning","reason":"BackOff",
             "message":"Back-off restarting failed container","count":7,
             "lastTimestamp":"2026-08-02T04:00:00Z",
             "involvedObject":{"kind":"Pod","name":"api-1","namespace":"prod"}},
            {"metadata":{"name":"e2","namespace":"prod"},"type":"Normal","reason":"Pulled",
             "message":"Container image pulled","count":1,
             "lastTimestamp":"2026-08-02T05:00:00Z",
             "involvedObject":{"kind":"Pod","name":"api-1","namespace":"prod"}}
        ]}"#,
    );
    current_pod(&script);
    script.route(
        "GET",
        PLAIN_LOG_PATH,
        200,
        "2026-08-02T05:00:00Z listening on :8080\n2026-08-02T05:00:01Z ready\n",
    );
    tail_identity(&script, Some("uid-api-1"));
    script.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"events is forbidden"}"#,
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let (tx, _rx) = crossbeam_channel::unbounded();
    let sync = runtime
        .block_on(async {
            sync_from(
                script.clients(),
                "prod",
                &options(),
                tx,
                Arc::new(IngestMetrics::default()),
            )
            .await
        })
        .expect("the scripted cluster syncs");

    let recv = |rx: futures::channel::oneshot::Receiver<InspectDetail>| {
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), rx).await })
            .expect("a reply within the budget")
            .expect("the fetch task replies")
    };

    let (reply, rx) = futures::channel::oneshot::channel();
    let _stop = sync
        .inspector
        .fetch_events("prod", "uid-api-1", move |detail| {
            let _ = reply.send(detail);
        });
    let InspectDetail::Events(events) = recv(rx) else {
        panic!("events must resolve");
    };
    assert_eq!(events.len(), 2);
    assert_eq!(
        (events[0].reason.as_str(), events[0].kind.as_str()),
        ("Pulled", "Normal"),
        "newest first: {events:?}"
    );
    assert_eq!(events[1].count, 7);
    let sent = script.requests_for("/events");
    assert_inspector_get(
        &sent[0],
        "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3Duid-api-1&limit=64",
    );

    let (reply, rx) = futures::channel::oneshot::channel();
    let _stop =
        sync.inspector
            .fetch_log_tail("prod", &Arc::from("api-1"), "uid-api-1", move |detail| {
                let _ = reply.send(detail);
            });
    let InspectDetail::Log(tail) = recv(rx) else {
        panic!("logs must resolve");
    };
    assert_eq!(tail.lines.len(), 2);
    assert!(tail.note.is_none());
    assert!(!tail.previous_unavailable);
    assert!(tail.lines[1].ends_with("ready"));
    let log_request = &script.requests_for("/pods/api-1/log")[0];
    assert_inspector_get(
        log_request,
        "/api/v1/namespaces/prod/pods/api-1/log?limitBytes=65536&tailLines=200&timestamps=true",
    );
    let rows = script.requests_for("/pods/api-1");
    assert_eq!(rows.len(), 3);
    assert_inspector_get(&rows[0], POD_PATH);
    assert_identity_get(&rows[2]);

    let (reply, rx) = futures::channel::oneshot::channel();
    let _stop = sync
        .inspector
        .fetch_events("prod", "uid-api-1", move |detail| {
            let _ = reply.send(detail);
        });
    assert_eq!(
        recv(rx),
        InspectDetail::Denied { what: "events" },
        "a 403 is a labelled state, not an error string"
    );
    let sent = script.requests_for("/events");
    assert_eq!(sent.len(), 2);
    assert_inspector_get(
        &sent[1],
        "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3Duid-api-1&limit=64",
    );

    drop(runtime);
}

/// A crash-looping pod's running container is a fresh process with nothing to
/// say; the explanation is in the instance that exited. The tail follows the
/// evidence and labels itself, so nobody reads an empty pane as a quiet app.
#[test]
fn a_crash_looping_pod_tails_the_instance_that_died_and_says_so() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        200,
        r#"{"kind":"Pod","apiVersion":"v1",
            "metadata":{"name":"api-1","namespace":"prod","uid":"uid-api-1"},
            "status":{"phase":"Running","containerStatuses":[
              {"name":"api","restartCount":7,"ready":false,
               "state":{"waiting":{"reason":"CrashLoopBackOff"}},
               "lastState":{"terminated":{"exitCode":137,"reason":"OOMKilled"}}}
            ]}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1/log?",
        200,
        "2026-08-02T04:59:59Z out of memory\n",
    );
    tail_identity(&script, Some("uid-api-1"));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let (tx, _rx) = crossbeam_channel::unbounded();
    let sync = runtime
        .block_on(async {
            sync_from(
                script.clients(),
                "prod",
                &options(),
                tx,
                Arc::new(IngestMetrics::default()),
            )
            .await
        })
        .expect("the scripted cluster syncs");

    let recv = |rx: futures::channel::oneshot::Receiver<InspectDetail>| {
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), rx).await })
            .expect("a reply within the budget")
            .expect("the fetch task replies")
    };

    let (reply, rx) = futures::channel::oneshot::channel();
    let _stop =
        sync.inspector
            .fetch_log_tail("prod", &Arc::from("api-1"), "uid-api-1", move |detail| {
                let _ = reply.send(detail);
            });
    let InspectDetail::Log(tail) = recv(rx) else {
        panic!("logs must resolve");
    };

    let note = tail.note.expect("a crash tail names itself");
    assert!(!tail.previous_unavailable);
    assert!(note.contains("previous instance of api"), "{note}");
    assert!(note.contains("OOMKilled"), "{note}");
    assert!(note.contains("exit 137"), "{note}");
    assert!(note.contains("CrashLoopBackOff"), "{note}");
    assert!(note.contains("7 restarts"), "{note}");

    let request = &script.requests_for("/pods/api-1/log")[0];
    assert_inspector_get(request, PREVIOUS_LOG_PATH);
    let rows = script.requests_for("/pods/api-1");
    assert_eq!(rows.len(), 3);
    assert_inspector_get(&rows[0], POD_PATH);
    assert_identity_get(&rows[2]);

    drop(runtime);
}

const POD_PATH: &str = "/api/v1/namespaces/prod/pods/api-1";
const PLAIN_LOG_PATH: &str =
    "/api/v1/namespaces/prod/pods/api-1/log?limitBytes=65536&tailLines=200&timestamps=true";
const PREVIOUS_LOG_PATH: &str = "/api/v1/namespaces/prod/pods/api-1/log?container=api&limitBytes=65536&previous=true&tailLines=200&timestamps=true";
const CURRENT_LOG_PATH: &str = "/api/v1/namespaces/prod/pods/api-1/log?container=api&limitBytes=65536&tailLines=200&timestamps=true";
const MISSING_PREVIOUS: &str = "previous terminated container \"api\" in pod \"api-1\" not found";
const IDENTITY_ACCEPT: &str = "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1";
const PREVIOUS_CONTAINER_ID: &str =
    "containerd://49337fd2ddc60a59b79a0e7105dba4a9d48a539c858c581e2694f2fcdf95fefb";

fn assert_inspector_get(row: &Seen, path: &str) {
    assert_eq!(row.method, "GET");
    assert_eq!(row.path.replacen("?&", "?", 1), path);
    assert_eq!(row.accept, "");
    assert_eq!(row.content_type, "");
    assert!(row.body.is_empty());
}

fn assert_identity_get(row: &Seen) {
    assert_eq!(row.method, "GET");
    assert_eq!(row.path, POD_PATH);
    assert_eq!(row.accept, IDENTITY_ACCEPT);
    assert_eq!(row.content_type, "application/json");
    assert!(row.body.is_empty());
}

fn tail_identity(script: &Script, uid: Option<&str>) {
    script.route_accepting("GET", POD_PATH, IDENTITY_ACCEPT, 200, identity_json(uid));
}

fn identity_json(uid: Option<&str>) -> String {
    let mut object = serde_json::json!({
        "apiVersion":"meta.k8s.io/v1", "kind":"PartialObjectMetadata",
        "metadata":{"name":"api-1", "namespace":"prod", "uid":uid}
    });
    if uid.is_none() {
        object["metadata"].as_object_mut().unwrap().remove("uid");
    }
    object.to_string()
}

fn inspector_cluster(script: &Script) -> (tokio::runtime::Runtime, Sync) {
    script_discovery(script);
    script_rules_review(script);
    script_access_reviews(script, true, 32);
    script_lists(script);
    let runtime = runtime();
    let (sync, _events) = sync_on(&runtime, script);
    (runtime, sync)
}

fn inspector_tail(sync: &Sync) -> k10s_data::inspect::InspectDetail {
    inspector_tail_uid(sync, "uid-api-1")
}

fn inspector_tail_uid(sync: &Sync, uid: &str) -> k10s_data::inspect::InspectDetail {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync
        .inspector
        .fetch_log_tail("prod", &Arc::from("api-1"), uid, move |detail| {
            let _ = tx.send(detail);
        });
    wait(&rx)
}

fn inspector_events(sync: &Sync, uid: &str) -> k10s_data::inspect::InspectDetail {
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.inspector.fetch_events("prod", uid, move |detail| {
        let _ = tx.send(detail);
    });
    wait(&rx)
}

fn crashing_pod(script: &Script) {
    crashing_pod_with_id(script, None);
}

fn current_pod(script: &Script) {
    script.route(
        "GET",
        POD_PATH,
        200,
        r#"{"apiVersion":"v1","kind":"Pod",
            "metadata":{"name":"api-1","namespace":"prod","uid":"uid-api-1"},
            "status":{"phase":"Running","containerStatuses":[{
                "name":"api","ready":true,"restartCount":0,
                "state":{"running":{"startedAt":"2026-08-02T05:00:00Z"}}
            }]}}"#,
    );
}

fn crashing_pod_with_id(script: &Script, container_id: Option<&str>) {
    let mut object = serde_json::json!({
        "kind":"Pod", "apiVersion":"v1",
        "metadata":{"name":"api-1", "namespace":"prod", "uid":"uid-api-1"},
        "status":{"containerStatuses":[{
            "name":"api", "restartCount":1, "ready":false,
            "state":{"waiting":{"reason":"CrashLoopBackOff"}},
            "lastState":{"terminated":{"exitCode":1, "reason":"Error"}}
        }]}
    });
    if let Some(container_id) = container_id {
        object["status"]["containerStatuses"][0]["lastState"]["terminated"]["containerID"] =
            container_id.into();
    }
    script.route("GET", POD_PATH, 200, object.to_string());
}

fn inspector_error(code: u16, reason: &str, message: &str) -> String {
    serde_json::json!({
        "apiVersion":"v1", "kind":"Status", "status":"Failure",
        "code":code, "reason":reason, "message":message
    })
    .to_string()
}

#[test]
fn unavailable_previous_logs_label_the_current_tail_and_the_original_refusal() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    let (_runtime, sync) = inspector_cluster(&script);
    crashing_pod(&script);
    script.route(
        "GET",
        PREVIOUS_LOG_PATH,
        400,
        inspector_error(400, "BadRequest", MISSING_PREVIOUS),
    );
    script.route(
        "GET",
        CURRENT_LOG_PATH,
        200,
        "2026-08-02T05:00:00Z current instance\n",
    );
    tail_identity(&script, Some("uid-api-1"));

    let InspectDetail::Log(tail) = inspector_tail(&sync) else {
        panic!("the explicit missing-previous refusal permits a current tail");
    };
    assert_eq!(tail.lines, ["2026-08-02T05:00:00Z current instance"]);
    assert!(tail.previous_unavailable);
    let note = tail.note.expect("the fallback must identify its source");
    assert!(note.starts_with("current container api;"), "{note}");
    assert!(note.contains(MISSING_PREVIOUS), "{note}");
    assert!(!note.contains("gone from the node"), "{note}");
    let rows = script.requests_for("/pods/api-1");
    assert_eq!(rows.len(), 4);
    assert_inspector_get(&rows[0], POD_PATH);
    assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
    assert_inspector_get(&rows[2], CURRENT_LOG_PATH);
    assert_identity_get(&rows[3]);
}

#[test]
fn a_recorded_previous_runtime_refusal_labels_current_logs_and_retained_history() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    let (_runtime, sync) = inspector_cluster(&script);
    crashing_pod_with_id(&script, Some(PREVIOUS_CONTAINER_ID));
    let refusal = format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}");
    script.route("GET", PREVIOUS_LOG_PATH, 200, &refusal);
    script.route(
        "GET",
        CURRENT_LOG_PATH,
        200,
        "2026-08-02T05:00:00.123456789Z current instance\n",
    );
    tail_identity(&script, Some("uid-api-1"));

    let InspectDetail::Log(tail) = inspector_tail(&sync) else {
        panic!("the exact refusal of the recorded previous ID permits current logs");
    };
    assert_eq!(
        tail.lines,
        ["2026-08-02T05:00:00.123456789Z current instance"]
    );
    assert!(tail.previous_unavailable);
    let note = tail
        .note
        .expect("current logs identify the previous refusal");
    assert!(note.starts_with("current container api;"), "{note}");
    assert!(
        note.ends_with(&format!("previous log unavailable: {refusal}")),
        "{note}"
    );
    let rows = script.requests_for("/pods/api-1");
    assert_eq!(rows.len(), 4);
    assert_inspector_get(&rows[0], POD_PATH);
    assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
    assert_inspector_get(&rows[2], CURRENT_LOG_PATH);
    assert_identity_get(&rows[3]);
}

#[test]
fn other_in_band_previous_refusals_keep_their_sentence_without_fallback() {
    use k10s_data::inspect::InspectDetail;

    let exact = format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}");
    for (container_id, refusal) in [
        (None, exact.clone()),
        (Some(""), exact.clone()),
        (
            Some("49337fd2"),
            "unable to retrieve container logs for 49337fd2".to_string(),
        ),
        (
            Some("containerd://"),
            "unable to retrieve container logs for containerd://".to_string(),
        ),
        (
            Some("://49337fd2"),
            "unable to retrieve container logs for ://49337fd2".to_string(),
        ),
        (
            Some(PREVIOUS_CONTAINER_ID),
            "unable to retrieve container logs for containerd://another-container".to_string(),
        ),
        (
            Some(PREVIOUS_CONTAINER_ID),
            "unable to retrieve container logs for containerd://49337fd2".to_string(),
        ),
        (
            Some(PREVIOUS_CONTAINER_ID),
            exact.replace("containerd://", "docker://"),
        ),
        (
            Some(PREVIOUS_CONTAINER_ID),
            format!("{exact}; backend unavailable"),
        ),
        (Some(PREVIOUS_CONTAINER_ID), format!("{exact}\n")),
        (
            Some(PREVIOUS_CONTAINER_ID),
            format!("{exact}\nadditional refusal"),
        ),
        (
            Some(PREVIOUS_CONTAINER_ID),
            "failed to open log file \"/var/log/pods/previous.log\": no such file".to_string(),
        ),
        (
            Some(PREVIOUS_CONTAINER_ID),
            "the runtime did not serve the requested logs".to_string(),
        ),
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        crashing_pod_with_id(&script, container_id);
        script.route("GET", PREVIOUS_LOG_PATH, 200, &refusal);
        assert_eq!(
            inspector_tail(&sync),
            InspectDetail::Failed {
                what: "logs",
                why: refusal.clone()
            },
            "previous ID {container_id:?}"
        );
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 2, "no current or final metadata GET: {refusal}");
        assert_inspector_get(&rows[0], POD_PATH);
        assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
    }
}

#[test]
fn timestamped_application_refusal_words_stay_logs_and_empty_tails_stay_empty() {
    use k10s_data::inspect::InspectDetail;

    let refusal = format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}");
    for previous in [false, true] {
        for text in [
            String::new(),
            format!("2026-08-02T05:00:00.000000000Z {refusal}\n"),
            format!(
                "2026-08-02T05:00:00.123456789+02:00 {refusal}\ncontinued application output\n"
            ),
        ] {
            let script = Script::default();
            let (_runtime, sync) = inspector_cluster(&script);
            let path = if previous {
                crashing_pod_with_id(&script, Some(PREVIOUS_CONTAINER_ID));
                PREVIOUS_LOG_PATH
            } else {
                current_pod(&script);
                PLAIN_LOG_PATH
            };
            script.route("GET", path, 200, &text);
            tail_identity(&script, Some("uid-api-1"));
            let InspectDetail::Log(tail) = inspector_tail(&sync) else {
                panic!("timestamps distinguish application output from raw control responses");
            };
            assert_eq!(tail.lines, text.lines().collect::<Vec<_>>());
            assert!(!tail.previous_unavailable);
            assert_eq!(tail.note.is_some(), previous);
            let rows = script.requests_for("/pods/api-1");
            assert_eq!(rows.len(), 3);
            assert_inspector_get(&rows[0], POD_PATH);
            assert_inspector_get(&rows[1], path);
            assert_identity_get(&rows[2]);
        }
    }
}

#[test]
fn raw_current_log_refusals_fail_before_identity_even_after_previous_unavailability() {
    use k10s_data::inspect::InspectDetail;

    let previous_refusal = format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}");
    for mode in ["current", "canonical fallback", "runtime fallback"] {
        for refusal in [
            previous_refusal.as_str(),
            "unable to retrieve container logs for containerd://current-container",
            "unknown current log refusal",
            "2026-99-99T05:00:00Z invalid timestamp is not application output",
            "\n",
        ] {
            let script = Script::default();
            let (_runtime, sync) = inspector_cluster(&script);
            let path = if mode == "current" {
                current_pod(&script);
                PLAIN_LOG_PATH
            } else {
                crashing_pod_with_id(&script, Some(PREVIOUS_CONTAINER_ID));
                if mode == "canonical fallback" {
                    script.route(
                        "GET",
                        PREVIOUS_LOG_PATH,
                        400,
                        inspector_error(400, "BadRequest", MISSING_PREVIOUS),
                    );
                } else {
                    script.route("GET", PREVIOUS_LOG_PATH, 200, &previous_refusal);
                }
                CURRENT_LOG_PATH
            };
            script.route("GET", path, 200, refusal);
            assert_eq!(
                inspector_tail(&sync),
                InspectDetail::Failed {
                    what: "logs",
                    why: refusal.to_string()
                },
                "{mode}"
            );
            let rows = script.requests_for("/pods/api-1");
            assert_eq!(rows.len(), if mode == "current" { 2 } else { 3 });
            assert_inspector_get(&rows[0], POD_PATH);
            if mode != "current" {
                assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
            }
            assert_inspector_get(rows.last().unwrap(), path);
        }
    }
}

#[test]
fn a_recorded_runtime_fallback_still_requires_the_selected_uid_after_current_logs() {
    use k10s_data::inspect::InspectDetail;

    for uid in [Some("replacement-uid"), None, Some("")] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        crashing_pod_with_id(&script, Some(PREVIOUS_CONTAINER_ID));
        script.route(
            "GET",
            PREVIOUS_LOG_PATH,
            200,
            format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}"),
        );
        script.route(
            "GET",
            CURRENT_LOG_PATH,
            200,
            "2026-08-02T05:00:00Z withheld current logs\n",
        );
        tail_identity(&script, uid);
        let why = if uid.is_some_and(|uid| !uid.is_empty()) {
            "Pod api-1 no longer has the selected UID; its log tail is withheld"
        } else {
            "Pod api-1 has no UID; its log tail cannot be scoped"
        };
        assert_eq!(
            inspector_tail(&sync),
            InspectDetail::Failed {
                what: "logs",
                why: why.to_string()
            }
        );
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 4);
        assert_inspector_get(&rows[0], POD_PATH);
        assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        assert_inspector_get(&rows[2], CURRENT_LOG_PATH);
        assert_identity_get(&rows[3]);
    }
}

#[test]
fn a_recorded_runtime_fallback_preserves_final_metadata_denial_or_deletion() {
    use k10s_data::inspect::InspectDetail;

    for (code, reason, message) in [
        (
            403,
            "Forbidden",
            "Pod identity is forbidden after runtime refusal",
        ),
        (
            404,
            "NotFound",
            "pods \"api-1\" not found after runtime refusal",
        ),
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        crashing_pod_with_id(&script, Some(PREVIOUS_CONTAINER_ID));
        script.route(
            "GET",
            PREVIOUS_LOG_PATH,
            200,
            format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}"),
        );
        script.route(
            "GET",
            CURRENT_LOG_PATH,
            200,
            "2026-08-02T05:00:00Z withheld current logs\n",
        );
        script.route_accepting(
            "GET",
            POD_PATH,
            IDENTITY_ACCEPT,
            code,
            inspector_error(code, reason, message),
        );
        let expected = if code == 403 {
            InspectDetail::Denied { what: "pods" }
        } else {
            InspectDetail::Failed {
                what: "pods",
                why: message.to_string(),
            }
        };
        assert_eq!(inspector_tail(&sync), expected);
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 4);
        assert_inspector_get(&rows[0], POD_PATH);
        assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        assert_inspector_get(&rows[2], CURRENT_LOG_PATH);
        assert_identity_get(&rows[3]);
    }
}

const LOCAL_LOG_BODY_LIMIT: usize = 64 * 1024;
const OVERSIZED_LOG_BODY: &str = "logs answered with more than 65536 bytes; the body is refused";

fn timestamped_tail_body(bytes: usize) -> Vec<u8> {
    let mut body = b"2026-08-02T05:00:00Z ".to_vec();
    body.resize(bytes, b'x');
    body
}

#[test]
fn an_exact_local_limit_tail_is_published_only_after_uid_verification() {
    use k10s_data::inspect::InspectDetail;

    for mode in [
        "current",
        "previous",
        "canonical fallback",
        "runtime fallback",
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        let path = if mode == "current" {
            current_pod(&script);
            PLAIN_LOG_PATH
        } else {
            crashing_pod_with_id(&script, Some(PREVIOUS_CONTAINER_ID));
            if mode == "canonical fallback" {
                script.route(
                    "GET",
                    PREVIOUS_LOG_PATH,
                    400,
                    inspector_error(400, "BadRequest", MISSING_PREVIOUS),
                );
            } else if mode == "runtime fallback" {
                script.route(
                    "GET",
                    PREVIOUS_LOG_PATH,
                    200,
                    format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}"),
                );
            }
            if mode == "previous" {
                PREVIOUS_LOG_PATH
            } else {
                CURRENT_LOG_PATH
            }
        };
        script.route_bytes(
            "GET",
            path,
            200,
            timestamped_tail_body(LOCAL_LOG_BODY_LIMIT),
        );
        tail_identity(&script, Some("uid-api-1"));
        let InspectDetail::Log(tail) = inspector_tail(&sync) else {
            panic!("a complete exact-limit body is valid: {mode}");
        };
        assert_eq!(tail.lines.len(), 1);
        assert!(tail.lines[0].starts_with("2026-08-02T05:00:00Z "));
        assert!(tail.lines[0].ends_with('\u{2026}'));
        assert_eq!(tail.previous_unavailable, mode.ends_with("fallback"));
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), if mode.ends_with("fallback") { 4 } else { 3 });
        assert_inspector_get(&rows[0], POD_PATH);
        if mode.ends_with("fallback") {
            assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        }
        assert_inspector_get(&rows[rows.len() - 2], path);
        assert_identity_get(rows.last().unwrap());
    }
}

#[test]
fn an_extra_success_byte_refuses_every_tail_before_fallback_or_final_identity() {
    use k10s_data::inspect::InspectDetail;

    for mode in [
        "current",
        "previous",
        "previous refusal",
        "canonical fallback",
        "runtime fallback",
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        let path = if mode == "current" {
            current_pod(&script);
            PLAIN_LOG_PATH
        } else {
            crashing_pod_with_id(&script, Some(PREVIOUS_CONTAINER_ID));
            if mode == "canonical fallback" {
                script.route(
                    "GET",
                    PREVIOUS_LOG_PATH,
                    400,
                    inspector_error(400, "BadRequest", MISSING_PREVIOUS),
                );
            } else if mode == "runtime fallback" {
                script.route(
                    "GET",
                    PREVIOUS_LOG_PATH,
                    200,
                    format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}"),
                );
            }
            if mode.starts_with("previous") {
                PREVIOUS_LOG_PATH
            } else {
                CURRENT_LOG_PATH
            }
        };
        let mut body = if mode == "previous refusal" {
            format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}").into_bytes()
        } else {
            timestamped_tail_body(LOCAL_LOG_BODY_LIMIT + 1)
        };
        body.resize(LOCAL_LOG_BODY_LIMIT + 1, b'x');
        script.route_bytes("GET", path, 200, body);
        assert_eq!(
            inspector_tail(&sync),
            InspectDetail::Failed {
                what: "logs",
                why: OVERSIZED_LOG_BODY.to_string()
            },
            "{mode}"
        );
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), if mode.ends_with("fallback") { 3 } else { 2 });
        assert_inspector_get(&rows[0], POD_PATH);
        if mode.ends_with("fallback") {
            assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        }
        assert_inspector_get(rows.last().unwrap(), path);
        assert!(!rows.iter().any(|row| row.accept == IDENTITY_ACCEPT));
    }
}

#[tokio::test(start_paused = true)]
async fn an_oversized_stream_is_refused_without_polling_its_held_remainder() {
    use futures::StreamExt;
    use http_body_util::BodyExt;
    use k10s_data::inspect::InspectDetail;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let script = Script::default();
    current_pod(&script);
    script.route_bytes(
        "GET",
        PLAIN_LOG_PATH,
        200,
        timestamped_tail_body(LOCAL_LOG_BODY_LIMIT + 1),
    );
    let backend = script.client();
    let remainder_polls = Arc::new(AtomicUsize::new(0));
    let seen_remainder = remainder_polls.clone();
    let service = tower::service_fn(move |request: http::Request<Body>| {
        let backend = backend.clone();
        let seen_remainder = seen_remainder.clone();
        let is_log = request.uri().path().ends_with("/pods/api-1/log");
        async move {
            let response = backend.send(request).await?;
            if !is_log {
                return Ok::<_, kube::Error>(response.map(|body| body.boxed_unsync()));
            }
            let frames = futures::stream::once(async {
                Body::from(timestamped_tail_body(LOCAL_LOG_BODY_LIMIT))
                    .frame()
                    .await
                    .expect("the full-limit frame")
            })
            .chain(futures::stream::once(async {
                Body::from(vec![b'x'])
                    .frame()
                    .await
                    .expect("one extra byte")
            }))
            .chain(futures::stream::once(async move {
                seen_remainder.fetch_add(1, Ordering::Relaxed);
                futures::future::pending().await
            }));
            Ok(response.map(|_| http_body_util::StreamBody::new(frames).boxed_unsync()))
        }
    });
    let (sync, _events) = async_inspector_cluster(
        &script,
        Clients {
            sync: script.client(),
            interactive: kube::Client::new(service, "prod"),
        },
    )
    .await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _stop =
        sync.inspector
            .fetch_log_tail("prod", &Arc::from("api-1"), "uid-api-1", move |detail| {
                tx.send(detail).expect("the local body refusal replies");
            });
    assert_eq!(
        rx.await.expect("tail reply"),
        InspectDetail::Failed {
            what: "logs",
            why: OVERSIZED_LOG_BODY.to_string()
        }
    );
    inspector_capacity_is(&sync, 0).await;
    assert_eq!(remainder_polls.load(Ordering::Relaxed), 0);
    let rows = script.requests_for("/pods/api-1");
    assert_eq!(rows.len(), 2);
    assert_inspector_get(&rows[0], POD_PATH);
    assert_inspector_get(&rows[1], PLAIN_LOG_PATH);
}

#[test]
fn other_previous_log_errors_keep_their_state_and_never_request_current_logs() {
    use k10s_data::inspect::InspectDetail;

    for (code, reason, message) in [
        (400, "BadRequest", "container api is waiting to start"),
        (
            400,
            "BadRequest",
            "previous terminated container \"sidecar\" in pod \"api-1\" not found",
        ),
        (
            400,
            "BadRequest",
            "previous terminated container \"api\" in pod \"api-2\" not found",
        ),
        (
            400,
            "BadRequest",
            "previous terminated container \"api\" in pod \"api-1\" not found; backend unavailable",
        ),
        (400, "Invalid", MISSING_PREVIOUS),
        (401, "Unauthorized", "authentication expired"),
        (403, "Forbidden", "pods/log is forbidden"),
        (404, "NotFound", "pod api-1 no longer exists"),
        (500, "InternalError", "container runtime unavailable"),
        (500, "InternalError", MISSING_PREVIOUS),
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        crashing_pod(&script);
        script.route(
            "GET",
            PREVIOUS_LOG_PATH,
            code,
            inspector_error(code, reason, message),
        );
        script.route("GET", CURRENT_LOG_PATH, 200, "must not be fetched\n");

        let expected = if matches!(code, 401 | 403) {
            InspectDetail::Denied { what: "logs" }
        } else {
            InspectDetail::Failed {
                what: "logs",
                why: message.to_string(),
            }
        };
        assert_eq!(inspector_tail(&sync), expected, "status {code}");
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 2, "status {code}");
        assert_inspector_get(&rows[0], POD_PATH);
        assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
    }
}

#[test]
fn a_refused_current_fallback_keeps_its_own_denial_or_server_sentence() {
    use k10s_data::inspect::InspectDetail;

    for (code, reason, message) in [
        (403, "Forbidden", "current container logs are forbidden"),
        (
            500,
            "InternalError",
            "current container runtime unavailable",
        ),
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        crashing_pod(&script);
        script.route(
            "GET",
            PREVIOUS_LOG_PATH,
            400,
            inspector_error(400, "BadRequest", MISSING_PREVIOUS),
        );
        script.route(
            "GET",
            CURRENT_LOG_PATH,
            code,
            inspector_error(code, reason, message),
        );
        let expected = if code == 403 {
            InspectDetail::Denied { what: "logs" }
        } else {
            InspectDetail::Failed {
                what: "logs",
                why: message.to_string(),
            }
        };
        assert_eq!(inspector_tail(&sync), expected);
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 3);
        assert_inspector_get(&rows[0], POD_PATH);
        assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        assert_inspector_get(&rows[2], CURRENT_LOG_PATH);
    }
}

#[test]
fn a_lost_previous_log_connection_never_implies_unavailable_history() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    let (_runtime, sync) = inspector_cluster(&script);
    crashing_pod(&script);
    script.route_lost("GET", PREVIOUS_LOG_PATH);
    script.route("GET", CURRENT_LOG_PATH, 200, "must not be fetched\n");

    let InspectDetail::Failed { what, why } = inspector_tail(&sync) else {
        panic!("a lost connection is a failure");
    };
    assert_eq!(what, "logs");
    assert!(why.contains("the scripted connection was lost"), "{why}");
    assert!(!why.contains("previous log"), "{why}");
    let rows = script.requests_for("/pods/api-1");
    assert_eq!(rows.len(), 2);
    assert_inspector_get(&rows[0], POD_PATH);
    assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
}

#[test]
fn metadata_failure_does_not_hide_a_denial_or_guess_which_container_to_read() {
    use k10s_data::inspect::InspectDetail;

    for (code, reason, message) in [
        (403, "Forbidden", "pods is forbidden"),
        (500, "InternalError", "Pod status cannot be read"),
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        script.route(
            "GET",
            POD_PATH,
            code,
            inspector_error(code, reason, message),
        );
        let expected = if code == 403 {
            InspectDetail::Denied { what: "pods" }
        } else {
            InspectDetail::Failed {
                what: "pods",
                why: message.to_string(),
            }
        };
        assert_eq!(inspector_tail(&sync), expected);
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 1);
        assert_inspector_get(&rows[0], POD_PATH);
    }
}

#[test]
fn same_name_event_histories_are_requested_by_the_selected_uid() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    let (_runtime, sync) = inspector_cluster(&script);
    for (uid, message) in [
        ("retired-uid", "old crash"),
        ("current-uid", "new container started"),
    ] {
        let path = format!(
            "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3D{uid}&limit=64"
        );
        script.route(
            "GET",
            &path,
            200,
            serde_json::json!({
                "apiVersion":"v1", "kind":"EventList", "metadata":{},
                "items":[{
                    "metadata":{"name":format!("event-{uid}"), "namespace":"prod"},
                    "involvedObject":{"apiVersion":"v1", "kind":"Pod", "namespace":"prod", "name":"api-1", "uid":uid},
                    "message":message, "reason":"Probe", "type":"Normal"
                }]
            }).to_string(),
        );
        let InspectDetail::Events(events) = inspector_events(&sync, uid) else {
            panic!("events for {uid} must answer");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, message);
    }
    let rows = script.requests_for("/events");
    assert_eq!(rows.len(), 2);
    assert_inspector_get(
        &rows[0],
        "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3Dretired-uid&limit=64",
    );
    assert_inspector_get(
        &rows[1],
        "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3Dcurrent-uid&limit=64",
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.path.contains("involvedObject.name"))
    );
}

#[test]
fn an_unknown_selected_uid_refuses_before_listing_events() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    let (_runtime, sync) = inspector_cluster(&script);
    assert_eq!(
        inspector_events(&sync, ""),
        InspectDetail::Failed {
            what: "events",
            why: "the selected object has no UID; its events cannot be scoped".to_string(),
        }
    );
    assert!(script.requests_for("/events").is_empty());
}

#[test]
fn an_unknown_selected_pod_uid_refuses_before_any_tail_request() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    let (_runtime, sync) = inspector_cluster(&script);
    assert_eq!(
        inspector_tail_uid(&sync, ""),
        InspectDetail::Failed {
            what: "logs",
            why: "the selected Pod has no UID; its log tail cannot be scoped".to_string(),
        }
    );
    assert!(script.requests_for("/pods/api-1").is_empty());
}

#[test]
fn a_reused_pod_name_or_missing_status_uid_refuses_before_requesting_logs() {
    use k10s_data::inspect::InspectDetail;

    for uid in [Some("replacement-uid"), None, Some("")] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        let mut object: serde_json::Value =
            serde_json::from_str(&pod_json("api-1", "uid-api-1", false)).unwrap();
        if let Some(uid) = uid {
            object["metadata"]["uid"] = serde_json::json!(uid);
        } else {
            object["metadata"].as_object_mut().unwrap().remove("uid");
        }
        script.route("GET", POD_PATH, 200, object.to_string());
        let why = if uid.is_some_and(|uid| !uid.is_empty()) {
            "Pod api-1 no longer has the selected UID; its log tail is withheld"
        } else {
            "Pod api-1 has no UID; its log tail cannot be scoped"
        };
        assert_eq!(
            inspector_tail(&sync),
            InspectDetail::Failed {
                what: "logs",
                why: why.to_string(),
            }
        );
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 1);
        assert_inspector_get(&rows[0], POD_PATH);
    }
}

#[test]
fn identity_loss_during_a_tail_withholds_lines_and_retained_history_permission() {
    use k10s_data::inspect::InspectDetail;

    for mode in ["current", "previous", "fallback"] {
        for uid in [Some("replacement-uid"), None, Some("")] {
            let script = Script::default();
            let (_runtime, sync) = inspector_cluster(&script);
            if mode == "current" {
                current_pod(&script);
                script.route(
                    "GET",
                    PLAIN_LOG_PATH,
                    200,
                    "2026-08-02T05:00:00Z must be withheld\n",
                );
            } else {
                crashing_pod(&script);
                if mode == "previous" {
                    script.route(
                        "GET",
                        PREVIOUS_LOG_PATH,
                        200,
                        "2026-08-02T05:00:00Z must be withheld\n",
                    );
                } else {
                    script.route(
                        "GET",
                        PREVIOUS_LOG_PATH,
                        400,
                        inspector_error(400, "BadRequest", MISSING_PREVIOUS),
                    );
                    script.route(
                        "GET",
                        CURRENT_LOG_PATH,
                        200,
                        "2026-08-02T05:00:00Z must be withheld\n",
                    );
                }
            }
            tail_identity(&script, uid);
            let why = if uid.is_some_and(|uid| !uid.is_empty()) {
                "Pod api-1 no longer has the selected UID; its log tail is withheld"
            } else {
                "Pod api-1 has no UID; its log tail cannot be scoped"
            };
            assert_eq!(
                inspector_tail(&sync),
                InspectDetail::Failed {
                    what: "logs",
                    why: why.to_string(),
                },
                "{mode}: {uid:?}"
            );
            let rows = script.requests_for("/pods/api-1");
            assert_eq!(rows.len(), if mode == "fallback" { 4 } else { 3 });
            assert_inspector_get(&rows[0], POD_PATH);
            assert_inspector_get(
                &rows[1],
                if mode == "current" {
                    PLAIN_LOG_PATH
                } else {
                    PREVIOUS_LOG_PATH
                },
            );
            if mode == "fallback" {
                assert_inspector_get(&rows[2], CURRENT_LOG_PATH);
            }
            assert_identity_get(rows.last().unwrap());
        }
    }
}

#[test]
fn identity_read_denial_deletion_or_failure_keeps_its_own_state() {
    use k10s_data::inspect::InspectDetail;

    for (code, reason, message) in [
        (401, "Unauthorized", "authentication expired after the tail"),
        (403, "Forbidden", "Pod identity is forbidden after the tail"),
        (404, "NotFound", "pods \"api-1\" not found"),
        (500, "InternalError", "Pod identity storage is unavailable"),
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        crashing_pod(&script);
        script.route(
            "GET",
            PREVIOUS_LOG_PATH,
            200,
            "2026-08-02T05:00:00Z must be withheld\n",
        );
        script.route_accepting(
            "GET",
            POD_PATH,
            IDENTITY_ACCEPT,
            code,
            inspector_error(code, reason, message),
        );
        let expected = if matches!(code, 401 | 403) {
            InspectDetail::Denied { what: "pods" }
        } else {
            InspectDetail::Failed {
                what: "pods",
                why: message.to_string(),
            }
        };
        assert_eq!(inspector_tail(&sync), expected);
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 3);
        assert_inspector_get(&rows[0], POD_PATH);
        assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        assert_identity_get(&rows[2]);
    }
}

#[test]
fn identity_read_never_falls_back_to_a_whole_pod_response() {
    use k10s_data::inspect::InspectDetail;

    for body in [
        pod_json("api-1", "uid-api-1", false),
        identity_json(Some("uid-api-1")).replace("meta.k8s.io/v1", "meta.k8s.io/v1beta1"),
        "not a metadata object".to_string(),
    ] {
        let script = Script::default();
        let (_runtime, sync) = inspector_cluster(&script);
        crashing_pod(&script);
        script.route(
            "GET",
            PREVIOUS_LOG_PATH,
            200,
            "2026-08-02T05:00:00Z must be withheld\n",
        );
        script.route_accepting("GET", POD_PATH, IDENTITY_ACCEPT, 200, body);
        let InspectDetail::Failed { what, why } = inspector_tail(&sync) else {
            panic!("unsupported metadata cannot release a tail");
        };
        assert_eq!(what, "pods");
        assert!(why.starts_with("Pod identity"), "{why}");
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 3, "no full-object retry");
        assert_inspector_get(&rows[0], POD_PATH);
        assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        assert_identity_get(&rows[2]);
    }
}

#[test]
fn a_lost_identity_connection_withholds_successful_log_data() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    let (_runtime, sync) = inspector_cluster(&script);
    crashing_pod(&script);
    script.route(
        "GET",
        PREVIOUS_LOG_PATH,
        200,
        "2026-08-02T05:00:00Z must be withheld\n",
    );
    script.route_lost("GET", POD_PATH);
    let InspectDetail::Failed { what, why } = inspector_tail(&sync) else {
        panic!("identity transport failure withholds the tail");
    };
    assert_eq!(what, "pods");
    assert!(why.contains("the scripted connection was lost"), "{why}");
    let rows = script.requests_for("/pods/api-1");
    assert_eq!(rows.len(), 3);
    assert_inspector_get(&rows[0], POD_PATH);
    assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
    assert_identity_get(&rows[2]);
}

async fn async_inspector_cluster(script: &Script, clients: Clients) -> (Sync, EventReceiver) {
    script_discovery(script);
    script_rules_review(script);
    script_access_reviews(script, true, 32);
    script_lists(script);
    let (tx, rx) = crossbeam_channel::unbounded();
    let sync = sync_from(
        clients,
        "prod",
        &options(),
        tx,
        Arc::new(IngestMetrics::default()),
    )
    .await
    .expect("the scripted Inspector cluster");
    (sync, rx)
}

async fn inspector_requests_reach(script: &Script, path: &str, count: usize) {
    for _ in 0..1_000 {
        if script.requests_for(path).len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "expected {count} Inspector requests for {path}: {:?}",
        script.requests_for(path)
    );
}

async fn inspector_capacity_is(sync: &Sync, admitted: usize) {
    for _ in 0..1_000 {
        if sync.reader.observe_schedule_counts().admitted == admitted {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "Inspector scheduler capacity: {:?}",
        sync.reader.observe_schedule_counts()
    );
}

#[tokio::test(start_paused = true)]
async fn inspector_events_and_tails_use_the_interactive_client_and_same_wire_contract() {
    use k10s_data::inspect::InspectDetail;

    let sync_script = Script::default();
    let interactive = Script::default();
    let (sync, _events) = async_inspector_cluster(
        &sync_script,
        Clients {
            sync: sync_script.client(),
            interactive: interactive.client(),
        },
    )
    .await;
    interactive.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        200,
        r#"{"apiVersion":"v1","kind":"EventList","metadata":{},"items":[]}"#,
    );
    crashing_pod(&interactive);
    interactive.route(
        "GET",
        PREVIOUS_LOG_PATH,
        200,
        "2026-08-02T05:00:00Z actual interactive tail\n",
    );
    tail_identity(&interactive, Some("uid-api-1"));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _events_stop = sync
        .inspector
        .fetch_events("prod", "uid-api-1", move |reply| {
            tx.send(reply).expect("event reply");
        });
    assert_eq!(
        rx.await.expect("event callback"),
        InspectDetail::Events(Vec::new())
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _log_stop =
        sync.inspector
            .fetch_log_tail("prod", &Arc::from("api-1"), "uid-api-1", move |reply| {
                tx.send(reply).expect("tail reply");
            });
    let InspectDetail::Log(log) = rx.await.expect("tail callback") else {
        panic!("the interactive client has the tail");
    };
    assert_eq!(log.lines, ["2026-08-02T05:00:00Z actual interactive tail"]);
    assert!(!log.previous_unavailable);
    assert!(sync_script.requests_for("/pods/api-1").is_empty());
    assert!(sync_script.requests_for("/events").is_empty());
    let rows = interactive.seen();
    assert_eq!(rows.len(), 4);
    assert_inspector_get(
        &rows[0],
        "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3Duid-api-1&limit=64",
    );
    assert_inspector_get(&rows[1], POD_PATH);
    assert_inspector_get(&rows[2], PREVIOUS_LOG_PATH);
    assert_identity_get(&rows[3]);
}

#[tokio::test(start_paused = true)]
async fn dropping_a_tail_guard_cancels_each_request_stage_without_a_reply_or_next_read() {
    for phase in [
        "status",
        "previous",
        "fallback",
        "runtime fallback",
        "identity",
    ] {
        let script = Script::default();
        let (sync, _events) = async_inspector_cluster(&script, script.clients()).await;
        if phase == "status" {
            script.route_hanging("GET", POD_PATH);
        } else {
            crashing_pod_with_id(&script, Some(PREVIOUS_CONTAINER_ID));
            match phase {
                "previous" => {
                    script.route_hanging("GET", PREVIOUS_LOG_PATH);
                }
                "fallback" => {
                    script.route(
                        "GET",
                        PREVIOUS_LOG_PATH,
                        400,
                        inspector_error(400, "BadRequest", MISSING_PREVIOUS),
                    );
                    script.route_hanging("GET", CURRENT_LOG_PATH);
                }
                "runtime fallback" => {
                    script.route(
                        "GET",
                        PREVIOUS_LOG_PATH,
                        200,
                        format!("unable to retrieve container logs for {PREVIOUS_CONTAINER_ID}"),
                    );
                    script.route_hanging("GET", CURRENT_LOG_PATH);
                }
                "identity" => {
                    script.route(
                        "GET",
                        PREVIOUS_LOG_PATH,
                        200,
                        "2026-08-02T05:00:00Z must never be published\n",
                    );
                    script.route_hanging("GET", POD_PATH);
                }
                _ => unreachable!(),
            }
        }
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let stop =
            sync.inspector
                .fetch_log_tail("prod", &Arc::from("api-1"), "uid-api-1", move |reply| {
                    tx.send(reply).expect("cancelled tail must not reply");
                });
        let count = match phase {
            "status" => 1,
            "previous" => 2,
            _ => 3,
        };
        inspector_requests_reach(&script, "/pods/api-1", count).await;
        drop(stop);
        inspector_capacity_is(&sync, 0).await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), count);
        assert_inspector_get(&rows[0], POD_PATH);
        if phase != "status" {
            assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        }
        if matches!(phase, "fallback" | "runtime fallback") {
            assert_inspector_get(&rows[2], CURRENT_LOG_PATH);
        }
        if phase == "identity" {
            assert_identity_get(&rows[2]);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_an_event_guard_cancels_its_uid_list_and_releases_capacity() {
    let script = Script::default();
    let (sync, _events) = async_inspector_cluster(&script, script.clients()).await;
    script.route_hanging("GET", "/api/v1/namespaces/prod/events?");
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let stop = sync
        .inspector
        .fetch_events("prod", "uid-api-1", move |reply| {
            tx.send(reply).expect("cancelled events must not reply");
        });
    inspector_requests_reach(&script, "/events", 1).await;
    drop(stop);
    inspector_capacity_is(&sync, 0).await;
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    let rows = script.requests_for("/events");
    assert_eq!(rows.len(), 1);
    assert_inspector_get(
        &rows[0],
        "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3Duid-api-1&limit=64",
    );
}

#[tokio::test(start_paused = true)]
async fn inspector_and_observe_share_capacity_and_cancel_queued_reads_before_transport() {
    use k10s_data::inspect::InspectDetail;
    use k10s_data::read::Fetched;

    let script = Script::default();
    let (sync, _events) = async_inspector_cluster(&script, script.clients()).await;
    let service_reads = script.requests_for("/api/v1/services").len();
    let mut guards = Vec::new();
    for uid in ["held-0", "held-1"] {
        let path = format!(
            "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3D{uid}&limit=64"
        );
        script.route_hanging("GET", &path);
        guards.push(
            sync.inspector
                .fetch_events("prod", uid, |_| panic!("held events must not reply")),
        );
    }
    inspector_requests_reach(&script, "/events", 2).await;
    for _ in 0..4 {
        guards.push(sync.inspector.fetch_log_tail(
            "prod",
            &Arc::from("api-1"),
            "uid-api-1",
            |_| {
                panic!("queued tails must not reply");
            },
        ));
    }
    inspector_capacity_is(&sync, 6).await;
    assert_eq!(sync.reader.observe_schedule_counts().active, 2);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _observe = sync.reader.observe_loki_probe(move |reply| {
        tx.send(reply).expect("busy Observe reply");
    });
    let Fetched::Failed { what, why } = rx.await.expect("Observe capacity callback") else {
        panic!("Observe shares Inspector capacity");
    };
    assert_eq!(what, "telemetry");
    assert_eq!(
        why,
        "telemetry is busy: two requests are active and four are waiting"
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _inspector = sync
        .inspector
        .fetch_events("prod", "rejected-uid", move |reply| {
            tx.send(reply).expect("busy Inspector reply");
        });
    assert_eq!(
        rx.await.expect("Inspector capacity callback"),
        InspectDetail::Failed {
            what: "telemetry",
            why
        }
    );
    assert!(script.requests_for("/pods/api-1").is_empty());
    assert_eq!(script.requests_for("/api/v1/services").len(), service_reads);
    drop(guards.pop());
    inspector_capacity_is(&sync, 5).await;
    assert!(script.requests_for("/pods/api-1").is_empty());
    drop(guards);
    inspector_capacity_is(&sync, 0).await;
    let counts = sync.reader.observe_schedule_counts();
    assert_eq!(counts.peak_active, 2);
    assert_eq!(counts.peak_admitted, 6);
    assert_eq!(counts.rejected, 2);
    let rows = script.requests_for("/events");
    assert_eq!(rows.len(), 2);
    for (row, uid) in rows.iter().zip(["held-0", "held-1"]) {
        assert_inspector_get(
            row,
            &format!(
                "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3D{uid}&limit=64"
            ),
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_tail_deadline_covers_status_previous_fallback_and_final_identity_together() {
    use k10s_data::inspect::InspectDetail;

    for identity_stage in [false, true] {
        let script = Script::default();
        let (sync, _events) = async_inspector_cluster(&script, script.clients()).await;
        let object = r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"api-1","namespace":"prod","uid":"uid-api-1"},"status":{"containerStatuses":[{"name":"api","restartCount":1,"ready":false}]}}"#;
        let status_delay = if identity_stage { 31 } else { 20 };
        script.route_delayed(
            "GET",
            POD_PATH,
            200,
            object,
            Duration::from_secs(status_delay),
        );
        if identity_stage {
            script.route_delayed(
                "GET",
                PREVIOUS_LOG_PATH,
                200,
                "2026-08-02T05:00:00Z must never be published\n",
                Duration::from_secs(3),
            );
            script.route_hanging("GET", POD_PATH);
        } else {
            script.route_delayed(
                "GET",
                PREVIOUS_LOG_PATH,
                400,
                inspector_error(400, "BadRequest", MISSING_PREVIOUS),
                Duration::from_secs(10),
            );
            script.route_hanging("GET", CURRENT_LOG_PATH);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _stop =
            sync.inspector
                .fetch_log_tail("prod", &Arc::from("api-1"), "uid-api-1", move |reply| {
                    tx.send(reply).expect("deadline callback");
                });
        inspector_requests_reach(&script, "/pods/api-1", 1).await;
        tokio::time::advance(Duration::from_secs(status_delay)).await;
        inspector_requests_reach(&script, "/pods/api-1", 2).await;
        tokio::time::advance(Duration::from_secs(if identity_stage { 3 } else { 10 })).await;
        inspector_requests_reach(&script, "/pods/api-1", 3).await;
        tokio::time::advance(Duration::from_secs(if identity_stage { 1 } else { 5 })).await;
        assert_eq!(
            rx.await.expect("whole tail deadline"),
            InspectDetail::Failed {
                what: "telemetry",
                why: "the request did not finish before its deadline".to_string(),
            }
        );
        inspector_capacity_is(&sync, 0).await;
        let rows = script.requests_for("/pods/api-1");
        assert_eq!(rows.len(), 3);
        assert_inspector_get(&rows[0], POD_PATH);
        assert_inspector_get(&rows[1], PREVIOUS_LOG_PATH);
        if identity_stage {
            assert_identity_get(&rows[2]);
        } else {
            assert_inspector_get(&rows[2], CURRENT_LOG_PATH);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_tail_queued_behind_expiring_events_never_starts_transport() {
    use k10s_data::inspect::InspectDetail;

    let script = Script::default();
    let (sync, _events) = async_inspector_cluster(&script, script.clients()).await;
    let mut guards = Vec::new();
    for uid in ["held-0", "held-1"] {
        script.route_hanging("GET", "/api/v1/namespaces/prod/events?");
        guards.push(sync.inspector.fetch_events("prod", uid, |_| {}));
    }
    inspector_requests_reach(&script, "/events", 2).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _queued =
        sync.inspector
            .fetch_log_tail("prod", &Arc::from("api-1"), "uid-api-1", move |reply| {
                tx.send(reply).expect("queued deadline callback");
            });
    inspector_capacity_is(&sync, 3).await;
    tokio::time::advance(k10s_data::connect::INTERACTIVE_DEADLINE).await;
    assert_eq!(
        rx.await.expect("queued tail deadline"),
        InspectDetail::Failed {
            what: "telemetry",
            why: "the request did not finish before its deadline".to_string(),
        }
    );
    inspector_capacity_is(&sync, 0).await;
    assert!(script.requests_for("/pods/api-1").is_empty());
    let rows = script.requests_for("/events");
    assert_eq!(rows.len(), 2);
    for (row, uid) in rows.iter().zip(["held-0", "held-1"]) {
        assert_inspector_get(
            row,
            &format!(
                "/api/v1/namespaces/prod/events?fieldSelector=involvedObject.uid%3D{uid}&limit=64"
            ),
        );
    }
}
