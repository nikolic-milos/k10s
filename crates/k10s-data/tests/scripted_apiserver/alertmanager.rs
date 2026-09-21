//! Alertmanager v2 through the API-server service proxy: GET alerts and
//! silences, POST/DELETE silences by method, path, and bytes, and
//! confirm=false sending nothing.

use crate::*;

use k10s_data::alertmanager::{
    SilenceOutcome, SilenceSpec, create_silence, expire_silence, fetch_alerts, fetch_silences,
};
use k10s_data::reach::{Bound, FoundService, ToolAuth, ToolKind, Transport};
use k10s_data::read::Fetched;

const ALERTS_JSON: &str = r#"[
  {
    "fingerprint": "0a3c0b6c0e8f1d2e",
    "startsAt": "2024-01-01T00:00:00.000Z",
    "status": {"state": "active", "inhibitedBy": [], "silencedBy": []},
    "labels": {"alertname": "Watchdog", "severity": "none"}
  }
]"#;

const SILENCES_JSON: &str = r#"[
  {
    "id": "silence-watchdog",
    "createdBy": "k10s",
    "comment": "quiet",
    "startsAt": "2024-01-01T00:00:00Z",
    "endsAt": "2024-01-02T00:00:00Z",
    "matchers": [{"name": "alertname", "value": "Watchdog", "isRegex": false}]
  }
]"#;

const CREATED_JSON: &str = r#"{"silenceID":"silence-watchdog"}"#;

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn am_bound(auth: ToolAuth, transport: Transport) -> Bound {
    Bound {
        kind: ToolKind::Alertmanager,
        found: Some(FoundService {
            kind: ToolKind::Alertmanager,
            namespace: "monitoring".into(),
            name: "alertmanager".into(),
            port: 9093,
            port_name: None,
        }),
        transport,
        auth,
    }
}

fn proxy() -> Transport {
    Transport::Proxy {
        namespace: "monitoring".into(),
        service: "alertmanager".into(),
        port: 9093,
    }
}

fn spec() -> SilenceSpec {
    SilenceSpec {
        matchers: vec![k10s_data::alertmanager::Matcher {
            name: "alertname".into(),
            value: "Watchdog".into(),
            is_regex: false,
            is_equal: true,
        }],
        starts_at: "2024-01-01T00:00:00Z".into(),
        ends_at: "2024-01-02T00:00:00Z".into(),
        created_by: "k10s".into(),
        comment: "quiet".into(),
    }
}

#[test]
fn alerts_are_a_get_through_the_service_proxy() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/alerts",
        200,
        ALERTS_JSON,
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        fetch_alerts(&script.client(), &am_bound(ToolAuth::Anonymous, proxy())).await
    });
    let Fetched::Ok(alerts) = outcome else {
        panic!("alerts must resolve: {outcome:?}");
    };
    assert_eq!(alerts.items.len(), 1);
    assert_eq!(alerts.items[0].alertname, "Watchdog");

    let hits = script.requests_for("/proxy/api/v2/alerts");
    assert_eq!(hits.len(), 1, "one GET, nothing else: {hits:?}");
    assert_eq!(hits[0].method, "GET");
    assert!(
        hits[0].path.ends_with("/proxy/api/v2/alerts")
            || hits[0].path.contains("/proxy/api/v2/alerts?"),
        "the ask is Alertmanager v2 through the service proxy: {}",
        hits[0].path
    );
    drop(runtime);
}

#[test]
fn silences_are_a_get_through_the_same_proxy() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences",
        200,
        SILENCES_JSON,
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        fetch_silences(&script.client(), &am_bound(ToolAuth::Anonymous, proxy())).await
    });
    let Fetched::Ok(silences) = outcome else {
        panic!("silences must resolve: {outcome:?}");
    };
    assert_eq!(silences.items[0].id, "silence-watchdog");

    let hits = script.requests_for("/proxy/api/v2/silences");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].method, "GET");
    drop(runtime);
}

#[test]
fn a_403_on_alerts_is_denied() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/alerts",
        403,
        status(403, "Forbidden"),
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        fetch_alerts(&script.client(), &am_bound(ToolAuth::Anonymous, proxy())).await
    });
    assert_eq!(
        outcome,
        Fetched::Denied {
            what: "alertmanager"
        }
    );
    drop(runtime);
}

#[test]
fn a_404_on_alerts_is_failed_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/alerts",
        404,
        status(404, "NotFound"),
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        fetch_alerts(&script.client(), &am_bound(ToolAuth::Anonymous, proxy())).await
    });
    let Fetched::Failed { what, why } = outcome else {
        panic!("a bound tool that is not v2 is Failed, not Absent: {outcome:?}");
    };
    assert_eq!(what, "alertmanager");
    assert!(
        why.contains("NotFound") || why.contains("not found") || why.contains("404"),
        "{why}"
    );
    drop(runtime);
}

#[test]
fn confirm_false_create_sends_nothing() {
    let script = Script::default();
    script.route(
        "POST",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences",
        200,
        CREATED_JSON,
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        create_silence(
            &script.client(),
            &am_bound(ToolAuth::Anonymous, proxy()),
            &spec(),
            false,
        )
        .await
    });
    match outcome {
        SilenceOutcome::NeedsConfirm { .. } => {}
        other => panic!("confirm=false is NeedsConfirm, not {other:?}"),
    }
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "confirm=false never touches the wire: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn confirm_false_expire_sends_nothing() {
    let script = Script::default();
    script.route(
        "DELETE",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silence/silence-watchdog",
        200,
        "{}",
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        expire_silence(
            &script.client(),
            &am_bound(ToolAuth::Anonymous, proxy()),
            "silence-watchdog",
            false,
        )
        .await
    });
    match outcome {
        SilenceOutcome::NeedsConfirm { .. } => {}
        other => panic!("confirm=false is NeedsConfirm, not {other:?}"),
    }
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "confirm=false never touches the wire: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn create_silence_posts_the_v2_body() {
    let script = Script::default();
    script.route(
        "POST",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences",
        200,
        CREATED_JSON,
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        create_silence(
            &script.client(),
            &am_bound(ToolAuth::Anonymous, proxy()),
            &spec(),
            true,
        )
        .await
    });
    match outcome {
        SilenceOutcome::Applied { id, .. } => assert_eq!(id, "silence-watchdog"),
        other => panic!("create must apply: {other:?}"),
    }

    let hits = script.requests_for("/proxy/api/v2/silences");
    assert_eq!(hits.len(), 1, "one POST: {hits:?}");
    assert_eq!(hits[0].method, "POST");
    assert_eq!(hits[0].content_type, "application/json");
    assert!(
        hits[0].path.ends_with("/proxy/api/v2/silences"),
        "{}",
        hits[0].path
    );
    assert_eq!(
        hits[0].body,
        r#"{"matchers":[{"name":"alertname","value":"Watchdog","isRegex":false,"isEqual":true}],"startsAt":"2024-01-01T00:00:00Z","endsAt":"2024-01-02T00:00:00Z","createdBy":"k10s","comment":"quiet"}"#
    );
    drop(runtime);
}

#[test]
fn expire_silence_deletes_the_v2_path() {
    let script = Script::default();
    script.route(
        "DELETE",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silence/silence-watchdog",
        200,
        "{}",
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        expire_silence(
            &script.client(),
            &am_bound(ToolAuth::Anonymous, proxy()),
            "silence-watchdog",
            true,
        )
        .await
    });
    match outcome {
        SilenceOutcome::Applied { id, .. } => assert_eq!(id, "silence-watchdog"),
        other => panic!("expire must apply: {other:?}"),
    }

    let hits = script.requests_for("/proxy/api/v2/silence/silence-watchdog");
    assert_eq!(hits.len(), 1, "one DELETE: {hits:?}");
    assert_eq!(hits[0].method, "DELETE");
    assert!(
        hits[0]
            .path
            .ends_with("/proxy/api/v2/silence/silence-watchdog"),
        "{}",
        hits[0].path
    );
    assert!(
        hits[0].body.is_empty(),
        "expire is a DELETE with no body: {}",
        hits[0].body
    );
    drop(runtime);
}

#[test]
fn a_named_token_never_rides_the_service_proxy() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/alerts",
        200,
        ALERTS_JSON,
    );

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        fetch_alerts(
            &script.client(),
            &am_bound(ToolAuth::NamedToken("am-token".into()), proxy()),
        )
        .await
    });
    let Fetched::Failed { why, .. } = outcome else {
        panic!("a token on the proxy must not be sent: {outcome:?}");
    };
    assert!(why.contains("proxy"), "{why}");
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "refusing means the request is not issued: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn the_reader_posts_the_reviewed_pod_matchers_to_the_reviewed_endpoint() {
    use k10s_data::alertmanager::Matcher;
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences";
    script.route("POST", path, 200, CREATED_JSON);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let reviewed = SilenceSpec {
        matchers: vec![
            Matcher {
                name: "namespace".into(),
                value: "prod-eu".into(),
                is_regex: false,
                is_equal: true,
            },
            Matcher {
                name: "pod".into(),
                value: "api.v2-7d4f".into(),
                is_regex: false,
                is_equal: true,
            },
        ],
        starts_at: "2026-09-12T03:00:00Z".into(),
        ends_at: "2026-09-12T04:00:00Z".into(),
        created_by: "k10s".into(),
        comment: "Investigating this pod.".into(),
    };
    let requests_before = script.seen().len();
    for confirm in [false, true] {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.create_silence(
            am_bound(ToolAuth::Anonymous, proxy()),
            reviewed.clone(),
            confirm,
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        let answer = wait(&rx);
        if confirm {
            assert!(
                matches!(answer, SilenceOutcome::Applied { ref id, .. } if id == "silence-watchdog")
            );
        } else {
            assert!(matches!(answer, SilenceOutcome::NeedsConfirm { .. }));
            assert_eq!(
                script.seen().len(),
                requests_before,
                "review neither writes nor rebinds"
            );
        }
    }
    let seen = script.seen();
    let added = &seen[requests_before..];
    assert_eq!(
        added.len(),
        1,
        "confirmation does not discover a new destination"
    );
    assert_eq!(added[0].method, "POST");
    assert_eq!(added[0].path, path);
    assert_eq!(added[0].accept, "");
    assert_eq!(added[0].content_type, "application/json");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&added[0].body).expect("JSON"),
        serde_json::json!({
            "matchers": [
                {"name":"namespace", "value":"prod-eu", "isRegex":false, "isEqual":true},
                {"name":"pod", "value":"api.v2-7d4f", "isRegex":false, "isEqual":true}
            ],
            "startsAt":"2026-09-12T03:00:00Z", "endsAt":"2026-09-12T04:00:00Z",
            "createdBy":"k10s", "comment":"Investigating this pod."
        })
    );
}

#[test]
fn a_create_response_without_a_silence_id_does_not_claim_success() {
    for body in ["", "{}", r#"{"silenceID":""}"#] {
        let script = Script::default();
        let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences";
        script.route("POST", path, 200, body);
        let runtime = runtime();
        let answer = runtime.block_on(async {
            create_silence(
                &script.client(),
                &am_bound(ToolAuth::Anonymous, proxy()),
                &spec(),
                true,
            )
            .await
        });
        assert!(
            matches!(answer, SilenceOutcome::Failed { ref why, .. } if why.contains("silenceID")),
            "{answer:?}"
        );
        let hits = script.seen();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].method, "POST");
        assert_eq!(hits[0].path, path);
        assert_eq!(hits[0].accept, "");
        assert_eq!(hits[0].content_type, "application/json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&hits[0].body).expect("JSON")["matchers"][0]
                ["value"],
            "Watchdog"
        );
    }
}

#[test]
fn the_alerts_read_preserves_the_namespace_and_full_pod_name_for_a_join() {
    let script = Script::default();
    let pod = format!(
        "{}.{}.{}.{}",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61)
    );
    assert_eq!(pod.len(), 253);
    let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/alerts";
    script.route("GET", path, 200, serde_json::json!([{
        "fingerprint":"chosen", "labels":{"namespace":"prod", "pod":pod}, "status":{"state":"active"}
    }]).to_string());
    let runtime = runtime();
    let answer = runtime.block_on(async {
        fetch_alerts(&script.client(), &am_bound(ToolAuth::Anonymous, proxy())).await
    });
    let Fetched::Ok(alerts) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(alerts.items[0].namespace, "prod");
    assert_eq!(alerts.items[0].pod, pod);
    let hits = script.seen();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].method, "GET");
    assert_eq!(hits[0].path, path);
    assert_eq!(hits[0].accept, "");
    assert_eq!(hits[0].body, "");
}

#[test]
fn silence_write_denial_and_failure_keep_their_label_and_server_sentence() {
    for (code, reason, sentence) in [
        (403, "Forbidden", "creating silences is forbidden"),
        (500, "InternalError", "silence store is read-only"),
    ] {
        let script = Script::default();
        let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences";
        script.route("POST", path, code, serde_json::json!({
            "apiVersion":"v1", "kind":"Status", "status":"Failure", "code":code, "reason":reason, "message":sentence
        }).to_string());
        let runtime = runtime();
        let answer = runtime.block_on(async {
            create_silence(
                &script.client(),
                &am_bound(ToolAuth::Anonymous, proxy()),
                &spec(),
                true,
            )
            .await
        });
        if code == 403 {
            assert_eq!(
                answer,
                SilenceOutcome::Denied {
                    what: "alertmanager",
                    why: "access denied for this account".into()
                }
            );
        } else {
            assert!(
                matches!(answer, SilenceOutcome::Failed { ref why, .. } if why.contains(sentence)),
                "{answer:?}"
            );
        }
        let hits = script.seen();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].method, "POST");
        assert_eq!(hits[0].path, path);
        assert_eq!(hits[0].accept, "");
        assert_eq!(hits[0].content_type, "application/json");
        assert_eq!(
            hits[0].body,
            r#"{"matchers":[{"name":"alertname","value":"Watchdog","isRegex":false,"isEqual":true}],"startsAt":"2024-01-01T00:00:00Z","endsAt":"2024-01-02T00:00:00Z","createdBy":"k10s","comment":"quiet"}"#
        );
    }
}
