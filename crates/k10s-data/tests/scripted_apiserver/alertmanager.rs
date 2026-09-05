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
