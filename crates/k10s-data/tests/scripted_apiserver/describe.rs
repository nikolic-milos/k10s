//! Describe over one object: fields rendered, owners walked, events joined by
//! uid -- and a Secret describe that is metadata-only by construction rather
//! than by filtering.

use crate::*;

#[test]
fn describe_renders_fields_walks_owners_and_joins_events_by_uid() {
    use k10s_core::KindId;
    use k10s_data::describe::DescribeRequest;
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
        pod_json("api-1", "uid-pod-1", true),
    );
    script.route_accepting(
        "GET",
        "/apis/apps/v1/namespaces/prod/replicasets/api-7f9",
        "as=PartialObjectMetadata",
        200,
        r#"{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
            "metadata":{"name":"api-7f9","namespace":"prod","uid":"uid-rs",
              "ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"api","uid":"uid-dep","controller":true}]}}"#,
    );
    script.route_accepting(
        "GET",
        "/apis/apps/v1/namespaces/prod/deployments/api",
        "as=PartialObjectMetadata",
        200,
        r#"{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
            "metadata":{"name":"api","namespace":"prod","uid":"uid-dep"}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        200,
        r#"{"kind":"EventList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"name":"e1","namespace":"prod"},"type":"Warning","reason":"BackOff",
             "message":"Back-off restarting failed container","count":7,
             "lastTimestamp":"2026-08-02T04:00:00Z",
             "involvedObject":{"kind":"Pod","name":"api-1","namespace":"prod","uid":"uid-pod-1"}}
        ]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_describe(
        DescribeRequest {
            kind: KindId::POD,
            namespace: Some("prod".to_string()),
            name: "api-1".to_string(),
            uid: "uid-pod-1".to_string(),
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Ok(described) = wait(&rx) else {
        panic!("describe must resolve");
    };
    assert_eq!(described.title, "Pod api-1");
    let text = described.lines.join("\n");
    assert!(text.contains("kind: Pod"), "{text}");
    assert!(
        text.contains("reason: CrashLoopBackOff"),
        "field-level rendering reaches into status: {text}"
    );
    let rs_at = described
        .lines
        .iter()
        .position(|l| l.trim() == "ReplicaSet api-7f9")
        .unwrap_or_else(|| panic!("the direct owner is walked: {text}"));
    let dep_at = described
        .lines
        .iter()
        .position(|l| l.trim() == "Deployment api")
        .unwrap_or_else(|| panic!("the chain reaches the root: {text}"));
    assert!(rs_at < dep_at, "the chain reads upward");
    assert!(text.contains("BackOff x7"), "{text}");

    for request in script.requests_for("/replicasets/api-7f9") {
        assert!(
            request.accept.contains("as=PartialObjectMetadata"),
            "an owner hop needs metadata only: {request:?}"
        );
    }
    let events = script.requests_for("/namespaces/prod/events");
    assert!(
        events[0].path.contains("involvedObject.uid%3Duid-pod-1"),
        "events join by uid, not by name collision: {}",
        events[0].path
    );

    drop(runtime);
}
#[test]
fn a_secret_describe_is_metadata_only_by_construction() {
    use k10s_core::KindId;
    use k10s_data::describe::DescribeRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route_accepting(
        "GET",
        "/api/v1/namespaces/prod/secrets/api-token",
        "as=PartialObjectMetadata",
        200,
        r#"{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
            "metadata":{"name":"api-token","namespace":"prod","uid":"uid-sec",
                        "annotations":{"kubernetes.io/service-account.name":"api"}}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/events?",
        200,
        r#"{"kind":"EventList","apiVersion":"v1","metadata":{},"items":[]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_describe(
        DescribeRequest {
            kind: KindId::SECRET,
            namespace: Some("prod".to_string()),
            name: "api-token".to_string(),
            uid: "uid-sec".to_string(),
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Ok(described) = wait(&rx) else {
        panic!("describe must resolve");
    };
    let text = described.lines.join("\n");
    assert!(
        described.lines[0].contains("values withheld"),
        "the document says what is missing: {text}"
    );
    assert!(text.contains("kind: Secret"), "{text}");
    assert!(
        !text.contains("PartialObjectMetadata"),
        "the wire shape is not the story: {text}"
    );
    assert!(text.contains("(none recorded)"), "{text}");

    let secret_requests = script.requests_for("/secrets/api-token");
    assert!(!secret_requests.is_empty());
    for request in &secret_requests {
        assert!(
            request.accept.contains("as=PartialObjectMetadata"),
            "a Secret describe must be structurally metadata-only: {request:?}"
        );
    }

    drop(runtime);
}
