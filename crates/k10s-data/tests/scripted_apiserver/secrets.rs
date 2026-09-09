//! Secret boundaries beyond list and watch: workload reads must refuse before
//! fetching an object, writes negotiate metadata without a full-object fallback,
//! and describe cannot print the declared values carried inside an annotation.

use crate::*;
use k10s_data::apply::{ApplyOutcome, ApplyRequest};
use k10s_data::day2::{Caps, Day2Call, Day2Outcome, DeleteRequest};
use k10s_data::describe::DescribeRequest;
use k10s_data::logs::{LogChunk, WorkloadLogRequest};
use k10s_data::metrics::{UsageOutcome, UsageRequest, UsageTarget};
use k10s_data::read::Fetched;

const OBJECT_PATH: &str = "/api/v1/namespaces/prod/secrets/api-token";
const METADATA_ACCEPT: &str = "application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1";
const YAML: &str = "apiVersion: v1\nkind: Secret\nmetadata:\n  name: api-token\n";
const METADATA: &str = r#"{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
    "metadata":{"name":"api-token","namespace":"prod","uid":"uid-sec"}}"#;

fn prepare(script: &Script) {
    script_discovery_with_secret(script, api_resource("Secret", "secrets", true));
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],
                "resourceRules":[{"apiGroups":["*"],"resources":["*"],"verbs":["*"]}]}}"#,
    );
    script_access_reviews(script, true, 32);
    script_lists(script);
}

#[test]
fn workload_logs_refuse_a_secret_before_fetching_it() {
    let script = Script::default();
    prepare(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.follow_workload_logs(
        WorkloadLogRequest {
            namespace: "prod".to_string(),
            kind: KindId::SECRET,
            name: "api-token".to_string(),
        },
        Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }),
    );
    assert_eq!(
        wait(&rx),
        LogChunk::Failed {
            what: "workload logs",
            why: "a Secret has no workload logs; its values are withheld".to_string(),
        }
    );
    assert!(script.requests_for(OBJECT_PATH).is_empty());
    assert!(script.requests_for("/log?").is_empty());
}

#[test]
fn workload_usage_refuses_a_secret_before_fetching_it() {
    let script = Script::default();
    prepare(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (tx, rx) = std::sync::mpsc::channel();
    let _stop = sync.reader.poll_usage(
        UsageRequest {
            namespace: "prod".to_string(),
            target: UsageTarget::Workload {
                kind: KindId::SECRET,
                name: "api-token".to_string(),
            },
            interval: Duration::from_secs(60),
        },
        Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }),
    );
    assert_eq!(
        wait(&rx),
        UsageOutcome::Absent {
            why: "a Secret has no pod usage; its values are withheld".to_string(),
        }
    );
    assert!(script.requests_for(OBJECT_PATH).is_empty());
    assert!(script.requests_for("metrics.k8s.io").is_empty());
    assert!(script.requests_for("/proxy/").is_empty());
}

#[test]
fn describe_omits_a_secrets_declared_values_and_keeps_other_metadata() {
    let script = Script::default();
    prepare(&script);
    let declared = serde_json::json!({
        "data": {"token": "ZW5jb2RlZC1jYW5hcnk="},
        "stringData": {"token": "plaintext-canary"},
    });
    let mut object: serde_json::Value = serde_json::from_str(METADATA).unwrap();
    object["metadata"]["annotations"] = serde_json::json!({
        "kubectl.kubernetes.io/last-applied-configuration": declared.to_string(),
        "kubernetes.io/service-account.name": "api",
    });
    script.route_accepting("GET", OBJECT_PATH, METADATA_ACCEPT, 200, object.to_string());
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
    let Fetched::Ok(doc) = wait(&rx) else {
        panic!("the metadata describe resolves");
    };
    let text = doc.lines.join("\n");
    for withheld in [
        "plaintext-canary",
        "ZW5jb2RlZC1jYW5hcnk=",
        "last-applied-configuration",
    ] {
        assert!(!text.contains(withheld), "{text}");
    }
    assert!(text.contains("values withheld"), "{text}");
    assert!(text.contains("kind: Secret"), "{text}");
    assert!(
        text.contains("kubernetes.io/service-account.name: api"),
        "{text}"
    );
    let requests = script.requests_for(OBJECT_PATH);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, OBJECT_PATH);
    assert_eq!(requests[0].accept, METADATA_ACCEPT);
    assert_eq!(requests[0].body, "");
}

fn apply_request(dry_run: bool) -> ApplyRequest {
    ApplyRequest {
        kind: KindId::SECRET,
        namespace: Some("prod".to_string()),
        name: "api-token".to_string(),
        yaml: YAML.to_string(),
        dry_run,
        force: false,
    }
}

#[test]
fn both_halves_of_secret_apply_request_only_metadata() {
    for dry_run in [true, false] {
        let script = Script::default();
        prepare(&script);
        script.route_accepting(
            "PATCH",
            &format!("{OBJECT_PATH}?"),
            METADATA_ACCEPT,
            200,
            METADATA,
        );
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.apply(apply_request(dry_run), move |outcome| {
            let _ = tx.send(outcome);
        });
        let ApplyOutcome::Applied(applied) = wait(&rx) else {
            panic!("a metadata response still describes the applied object");
        };
        assert_eq!(applied.dry_run, dry_run);
        assert_eq!(applied.uid.as_deref(), Some("uid-sec"));
        assert!(applied.yaml.contains("kind: Secret"));
        assert!(applied.yaml.contains("values withheld"));
        let requests = script.requests_for(OBJECT_PATH);
        assert_eq!(requests.len(), 1);
        let query = if dry_run { "dryRun=All&" } else { "" };
        assert_eq!(requests[0].method, "PATCH");
        assert_eq!(
            requests[0].path,
            format!("{OBJECT_PATH}?&{query}fieldManager=k10s&fieldValidation=Strict")
        );
        assert_eq!(requests[0].accept, METADATA_ACCEPT);
        assert_eq!(requests[0].content_type, "application/apply-patch+yaml");
        assert_eq!(requests[0].body, YAML);
    }
}

fn delete_request() -> Day2Call {
    Day2Call::Delete(DeleteRequest {
        namespace: Some("prod".to_string()),
        name: "api-token".to_string(),
        grace_period_seconds: Some(0),
        confirm: true,
        caps: Caps::default(),
    })
}

#[test]
fn secret_delete_accepts_metadata_or_status_without_a_full_object_fallback() {
    for response in [
        METADATA,
        r#"{"kind":"Status","apiVersion":"v1","status":"Success"}"#,
    ] {
        let script = Script::default();
        prepare(&script);
        script.route_accepting(
            "DELETE",
            &format!("{OBJECT_PATH}?"),
            METADATA_ACCEPT,
            200,
            response,
        );
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader
            .day2(KindId::SECRET, delete_request(), move |outcome| {
                let _ = tx.send(outcome);
            });
        let outcome = wait(&rx);
        assert!(matches!(outcome, Day2Outcome::Applied(_)), "{outcome:?}");
        let requests = script.requests_for(OBJECT_PATH);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "DELETE");
        assert_eq!(requests[0].path, format!("{OBJECT_PATH}?"));
        assert_eq!(requests[0].accept, METADATA_ACCEPT);
        assert_eq!(requests[0].content_type, "application/json");
        assert_eq!(requests[0].body, r#"{"gracePeriodSeconds":0}"#);
    }
}

#[test]
fn rejected_metadata_negotiation_does_not_retry_with_a_full_secret_response() {
    for delete in [false, true] {
        let script = Script::default();
        prepare(&script);
        script.route_accepting(
            if delete { "DELETE" } else { "PATCH" },
            &format!("{OBJECT_PATH}?"),
            METADATA_ACCEPT,
            406,
            r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":406,
                "reason":"NotAcceptable","message":"metadata representation unavailable"}"#,
        );
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        if delete {
            let (tx, rx) = std::sync::mpsc::channel();
            sync.reader
                .day2(KindId::SECRET, delete_request(), move |outcome| {
                    let _ = tx.send(outcome);
                });
            assert!(
                matches!(wait(&rx), Day2Outcome::Failed { why } if why == "metadata representation unavailable")
            );
        } else {
            let (tx, rx) = std::sync::mpsc::channel();
            sync.reader.apply(apply_request(true), move |outcome| {
                let _ = tx.send(outcome);
            });
            assert!(
                matches!(wait(&rx), ApplyOutcome::Failed { why } if why == "metadata representation unavailable")
            );
        }
        let requests = script.requests_for(OBJECT_PATH);
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].accept, METADATA_ACCEPT);
    }
}
