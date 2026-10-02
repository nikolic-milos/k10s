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

// A silence write waits as long as any confirmed write, the interactive
// client's deadline, and has no shorter timer of its own: an answer after the
// four seconds a probe gets is still the answer. The clock is paused, so the
// test does not wait them out.
#[test]
fn a_silence_answered_after_a_probes_deadline_is_created() {
    let script = Script::default();
    script.route_delayed(
        "POST",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences",
        200,
        CREATED_JSON,
        Duration::from_secs(6),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime");
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
        other => panic!("a late answer is still the answer: {other:?}"),
    }
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
            assert!(
                after_sync(&script, requests_before).is_empty(),
                "review neither writes nor rebinds: {:?}",
                after_sync(&script, requests_before)
            );
        }
    }
    let added = after_sync(&script, requests_before);
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

// An answer too large to show is still a 2xx: the silence exists.
#[test]
fn a_create_answered_too_large_to_show_says_the_silence_was_created() {
    let script = Script::default();
    let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences";
    let huge = format!(
        r#"{{"silenceID":"s","pad":"{}"}}"#,
        "x".repeat(k10s_data::reach::MAX_BODY_BYTES)
    );
    script.route("POST", path, 200, huge);
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
        matches!(answer, SilenceOutcome::Failed { ref why, .. }
            if why.starts_with("the silence was created, but Alertmanager answered with more than ")),
        "{answer:?}"
    );
    assert_eq!(script.seen().len(), 1);
}

#[test]
fn a_create_answered_without_a_silence_id_says_it_was_created_without_one() {
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
        // A 2xx means the silence exists; only its id could not be read.
        assert!(
            matches!(answer, SilenceOutcome::Failed { ref why, .. }
                if why.starts_with("the silence was created, but its answer could not be read: ")
                    && why.contains("silenceID")),
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

// A denial keeps its label. The API server's own 500, recorded on the lab for
// an admission webhook it could not reach (C1/lab/internal-error), is no
// verdict like any 5xx, so the silence may exist.
#[test]
fn a_denied_silence_write_keeps_its_label_and_a_5xx_says_it_may_exist() {
    let internal = "Internal error occurred: failed calling webhook \"unreachable.closeout.k10s.test\": \
                    failed to call webhook: Post \"https://nobody.closeout-ie.svc:443/?timeout=10s\": \
                    service \"nobody\" not found";
    for (code, reason, sentence, expected) in [
        (
            403,
            "Forbidden",
            "creating silences is forbidden",
            SilenceOutcome::Denied {
                what: "alertmanager",
                why: "creating silences is forbidden".into(),
            },
        ),
        (
            500,
            "InternalError",
            internal,
            SilenceOutcome::Failed {
                what: "alertmanager",
                why: format!(
                    "the silence may exist, so look at Alertmanager's silences before creating it \
                     again: {internal}"
                ),
            },
        ),
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
        assert_eq!(answer, expected);
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

// A redirect does not confirm or refuse the write. A status without a
// message names its code.
#[test]
fn a_redirect_is_not_a_created_silence_and_an_empty_answer_names_its_code() {
    let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences";
    let runtime = runtime();
    let create = |script: &Script| {
        runtime.block_on(async {
            create_silence(
                &script.client(),
                &am_bound(ToolAuth::Anonymous, proxy()),
                &spec(),
                true,
            )
            .await
        })
    };
    let script = Script::default();
    script.route("POST", path, 302, "Found");
    assert_eq!(
        create(&script),
        SilenceOutcome::Failed {
            what: "alertmanager",
            why: "the silence may exist, so look at Alertmanager's silences before creating it again: answered with status 302 and no Location".to_string()
        },
        "a redirect leaves the write unconfirmed"
    );

    for (code, why) in [
        (400, "answered with status 400 and no message".to_string()),
        (
            500,
            "the silence may exist, so look at Alertmanager's silences before creating it again: \
             answered with status 500 and no message"
                .to_string(),
        ),
    ] {
        let script = Script::default();
        script.route(
            "POST",
            path,
            code,
            format!(
                r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","message":"","code":{code}}}"#
            ),
        );
        assert_eq!(
            create(&script),
            SilenceOutcome::Failed {
                what: "alertmanager",
                why
            }
        );
    }
}

#[test]
fn a_redirected_create_may_have_stored_the_silence() {
    for code in [301, 302, 303, 307, 308] {
        let script = Script::default();
        let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences";
        script.route("POST", path, code, "");
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
        assert_eq!(
            outcome,
            SilenceOutcome::Failed {
                what: "alertmanager",
                why: format!(
                    "the silence may exist, so look at Alertmanager's silences before creating it again: answered with status {code} and no Location"
                )
            }
        );
        let sent = script.requests_for(path);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, "POST");
        assert_eq!(sent[0].path, path);
        assert_eq!(sent[0].accept, "");
        assert_eq!(sent[0].content_type, "application/json");
        let body: serde_json::Value = serde_json::from_str(&sent[0].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "matchers": [{"name": "alertname", "value": "Watchdog", "isRegex": false, "isEqual": true}],
                "startsAt": "2024-01-01T00:00:00Z", "endsAt": "2024-01-02T00:00:00Z", "createdBy": "k10s", "comment": "quiet"
            })
        );
    }
}

// A silence is a confirmed write and takes the interactive client.
#[test]
fn a_silence_takes_the_interactive_client() {
    let sync = Script::default();
    script_discovery(&sync);
    script_rules_review(&sync);
    script_access_reviews(&sync, true, 32);
    script_lists(&sync);
    let interactive = Script::default();
    let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences";
    interactive.route("POST", path, 200, CREATED_JSON);

    let runtime = runtime();
    let (data, _live) = sync_on_split(&runtime, &sync, &interactive);
    let before = sync.seen().len();
    let (tx, rx) = std::sync::mpsc::channel();
    data.reader.create_silence(
        am_bound(ToolAuth::Anonymous, proxy()),
        spec(),
        true,
        move |answer| {
            let _ = tx.send(answer);
        },
    );
    let answer = wait(&rx);
    assert!(
        matches!(answer, SilenceOutcome::Applied { .. }),
        "{answer:?}"
    );
    let sent = interactive.seen();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("POST", path)
    );
    let stray = after_sync(&sync, before);
    assert!(stray.is_empty(), "nothing on the sync client: {stray:?}");
    drop(runtime);
}

// An expire answered 2xx with a body too large to show still expired the
// silence: a 2xx is done, whatever its body.
#[test]
fn an_expire_answered_with_a_body_too_large_to_show_is_done() {
    let script = Script::default();
    script.route(
        "DELETE",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silence/abc",
        200,
        "0".repeat(k10s_data::reach::MAX_BODY_BYTES + 1),
    );
    let runtime = runtime();
    let expired = runtime.block_on(async {
        expire_silence(
            &script.client(),
            &am_bound(ToolAuth::Anonymous, proxy()),
            "abc",
            true,
        )
        .await
    });
    assert!(
        matches!(&expired, SilenceOutcome::Applied { id, .. } if id == "abc"),
        "{expired:?}"
    );
}

// An unconfirmed create may have made the silence, and the answer
// says so; a create the server refused says only what the server said. The
// same unconfirmed state on an expire says it may have expired.
#[test]
fn an_unconfirmed_silence_create_says_the_silence_may_exist() {
    let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silences";
    let timeout = serde_json::json!({
        "apiVersion":"v1", "kind":"Status", "status":"Failure", "code":504,
        "reason":"Timeout", "message":"the proxy timed out waiting for Alertmanager"
    })
    .to_string();
    // The service proxy answers 503 whether or not Alertmanager read the POST
    // (measured: C1/lab/proxy-502 and proxy-503), so a 503 is no verdict.
    let unavailable = serde_json::json!({
        "apiVersion":"v1", "kind":"Status", "status":"Failure", "code":503,
        "reason":"ServiceUnavailable", "message":"error trying to reach service: EOF"
    })
    .to_string();
    let may_exist =
        "the silence may exist, so look at Alertmanager's silences before creating it again: ";
    let runtime = runtime();
    let create = |script: &Script| {
        runtime.block_on(async {
            create_silence(
                &script.client(),
                &am_bound(ToolAuth::Anonymous, proxy()),
                &spec(),
                true,
            )
            .await
        })
    };

    let script = Script::default();
    script.route("POST", path, 504, timeout.clone());
    assert_eq!(
        create(&script),
        SilenceOutcome::Failed {
            what: "alertmanager",
            why: format!("{may_exist}the proxy timed out waiting for Alertmanager")
        }
    );

    let script = Script::default();
    script.route_lost("POST", path);
    let SilenceOutcome::Failed { why, .. } = create(&script) else {
        panic!("a lost create is a failure");
    };
    assert!(why.starts_with(may_exist), "{why}");

    // A create whose credential failed never left the process: no silence
    // can exist, and the sentence does not send the person looking for one.
    let script = Script::default();
    script.route_unsent("POST", path);
    let SilenceOutcome::Failed { why, .. } = create(&script) else {
        panic!("an unsent create is a failure");
    };
    assert_eq!(
        why,
        "nothing was sent: exec-plugin response did not contain a status"
    );

    let script = Script::default();
    script.route("POST", path, 503, unavailable);
    assert_eq!(
        create(&script),
        SilenceOutcome::Failed {
            what: "alertmanager",
            why: format!("{may_exist}error trying to reach service: EOF")
        }
    );

    // Alertmanager's own refusal is its JSON, not a Status, with a code that is
    // not the HTTP status (recorded on the lab: C1/lab/alertmanager-refusal),
    // and says only what it said.
    let script = Script::default();
    script.route(
        "POST",
        path,
        422,
        r#"{"code":612,"message":"matchers in body should have at least 1 items"}"#,
    );
    assert_eq!(
        create(&script),
        SilenceOutcome::Failed {
            what: "alertmanager",
            why: "matchers in body should have at least 1 items".to_string()
        }
    );

    let script = Script::default();
    script.route(
        "DELETE",
        "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silence/abc",
        504,
        timeout,
    );
    let expired = runtime.block_on(async {
        expire_silence(
            &script.client(),
            &am_bound(ToolAuth::Anonymous, proxy()),
            "abc",
            true,
        )
        .await
    });
    assert_eq!(
        expired,
        SilenceOutcome::Failed {
            what: "alertmanager",
            why: "the silence may have expired, so look at Alertmanager's silences before \
                  expiring it again: the proxy timed out waiting for Alertmanager"
                .to_string()
        }
    );

    let path = "/api/v1/namespaces/monitoring/services/alertmanager:9093/proxy/api/v2/silence/abc";
    let script = Script::default();
    script.route_unsent("DELETE", path);
    let expired = runtime.block_on(async {
        expire_silence(
            &script.client(),
            &am_bound(ToolAuth::Anonymous, proxy()),
            "abc",
            true,
        )
        .await
    });
    assert_eq!(
        expired,
        SilenceOutcome::Failed {
            what: "alertmanager",
            why: "nothing was sent: exec-plugin response did not contain a status".to_string(),
        }
    );
    let sent = script.seen();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("DELETE", path)
    );
    assert!(sent[0].accept.is_empty());
    assert!(sent[0].body.is_empty());
}
