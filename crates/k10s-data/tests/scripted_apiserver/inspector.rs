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
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1/log?",
        200,
        "2026-08-02T05:00:00Z listening on :8080\n2026-08-02T05:00:01Z ready\n",
    );
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
                script.client(),
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
    sync.inspector.fetch_events("prod", "api-1", move |detail| {
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
    assert!(
        sent[0]
            .path
            .contains("fieldSelector=involvedObject.name%3Dapi-1")
            || sent[0]
                .path
                .contains("fieldSelector=involvedObject.name=api-1"),
        "the query must scope to the object: {}",
        sent[0].path
    );

    let (reply, rx) = futures::channel::oneshot::channel();
    sync.inspector
        .fetch_log_tail("prod", &Arc::from("api-1"), move |detail| {
            let _ = reply.send(detail);
        });
    let InspectDetail::Log(tail) = recv(rx) else {
        panic!("logs must resolve");
    };
    assert_eq!(tail.lines.len(), 2);
    assert!(tail.lines[1].ends_with("ready"));
    let log_request = &script.requests_for("/pods/api-1/log")[0];
    assert!(
        log_request.path.contains("tailLines=200"),
        "the tail must be bounded: {}",
        log_request.path
    );

    let (reply, rx) = futures::channel::oneshot::channel();
    sync.inspector.fetch_events("prod", "api-1", move |detail| {
        let _ = reply.send(detail);
    });
    assert_eq!(
        recv(rx),
        InspectDetail::Denied { what: "events" },
        "a 403 is a labelled state, not an error string"
    );

    drop(runtime);
}
