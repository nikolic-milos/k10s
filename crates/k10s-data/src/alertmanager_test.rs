//! Alertmanager v2 JSON, caps, Grafana refusal, and confirm=false never
//! leaving this process.

use super::*;
use crate::reach::{Bound, ToolAuth, ToolKind, Transport};
use crate::read::Fetched;
use kube::client::Body;
use std::task::{Context, Poll};
use tower::Service;

fn v2_alerts() -> &'static str {
    r#"[
      {
        "annotations": {
          "description": "This is an alert meant to ensure that the entire alerting pipeline is functional.",
          "summary": "An alert that should always be firing to certify that Alertmanager is installed correctly."
        },
        "endsAt": "2024-01-01T00:05:00.000Z",
        "fingerprint": "0a3c0b6c0e8f1d2e",
        "receivers": [{"name": "Default"}],
        "startsAt": "2024-01-01T00:00:00.000Z",
        "status": {
          "inhibitedBy": [],
          "mutedBy": [],
          "silencedBy": [],
          "state": "active"
        },
        "updatedAt": "2024-01-01T00:00:00.000Z",
        "generatorURL": "http://prometheus.monitoring.svc:9090/graph?g0.expr=vector%281%29",
        "labels": {
          "alertname": "Watchdog",
          "prometheus": "monitoring/k8s",
          "severity": "none"
        }
      },
      {
        "annotations": {"summary": "Pod is crash looping."},
        "endsAt": "2024-01-01T00:05:00.000Z",
        "fingerprint": "b7e2aa11cc009901",
        "receivers": [{"name": "Default"}],
        "startsAt": "2024-01-01T00:01:00.000Z",
        "status": {
          "inhibitedBy": ["0a3c0b6c0e8f1d2e"],
          "mutedBy": [],
          "silencedBy": ["silence-watchdog"],
          "state": "suppressed"
        },
        "updatedAt": "2024-01-01T00:01:00.000Z",
        "generatorURL": "http://prometheus.monitoring.svc:9090/graph",
        "labels": {
          "alertname": "KubePodCrashLooping",
          "namespace": "prod",
          "name": "api-0",
          "severity": "warning",
          "pod": "api-0"
        }
      }
    ]"#
}

fn v2_silences() -> &'static str {
    r#"[
      {
        "id": "silence-watchdog",
        "status": {"state": "active"},
        "updatedAt": "2024-01-01T00:00:00.000Z",
        "comment": "quiet the watchdog while we drain",
        "createdBy": "k10s",
        "endsAt": "2024-01-02T00:00:00.000Z",
        "startsAt": "2024-01-01T00:00:00.000Z",
        "matchers": [
          {"name": "alertname", "value": "Watchdog", "isRegex": false, "isEqual": true}
        ]
      }
    ]"#
}

fn grafana_dashboard() -> &'static str {
    r#"{"uid":"k8s","title":"Cluster","panels":[{"id":1,"type":"timeseries","targets":[]}]}"#
}

fn grafana_alerting() -> &'static str {
    r#"{
      "apiVersion": 1,
      "groups": [{
        "orgId": 1,
        "name": "eval",
        "folder": "Kubernetes",
        "interval": "1m",
        "rules": [{
          "uid": "watchdog",
          "title": "Watchdog",
          "condition": "A",
          "grafana_alert": {"uid": "watchdog", "title": "Watchdog"}
        }]
      }]
    }"#
}

fn grafana_am_config() -> &'static str {
    r#"{"alertmanager_config":"global:\n  resolve_timeout: 5m\n","template_files":{}}"#
}

fn spec() -> SilenceSpec {
    SilenceSpec {
        matchers: vec![Matcher {
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

fn bound(auth: ToolAuth, transport: Transport) -> Bound {
    Bound {
        kind: ToolKind::Alertmanager,
        found: None,
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

#[derive(Clone)]
struct Unused;

impl Service<http::Request<Body>> for Unused {
    type Response = http::Response<Body>;
    type Error = tower::BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<Body>) -> Self::Future {
        Box::pin(async {
            panic!("confirm=false and refuse_bind must not touch the wire");
        })
    }
}

fn unused_client() -> kube::Client {
    kube::Client::new(Unused, "default")
}

#[test]
fn a_v2_alert_list_keeps_fingerprint_state_severity_and_silence() {
    let alerts = parse_alerts(v2_alerts().as_bytes()).expect("v2 alerts");
    assert!(!alerts.truncated && alerts.dropped == 0);
    assert_eq!(alerts.items.len(), 2);
    let watchdog = &alerts.items[0];
    assert_eq!(watchdog.fingerprint, "0a3c0b6c0e8f1d2e");
    assert_eq!(watchdog.state, "active");
    assert_eq!(watchdog.severity, "none");
    assert_eq!(watchdog.alertname, "Watchdog");
    assert!(watchdog.namespace.is_empty());
    assert!(watchdog.name.is_empty());
    assert_eq!(watchdog.starts_at, "2024-01-01T00:00:00.000Z");
    assert!(!watchdog.inhibited);
    assert!(watchdog.silenced_by.is_empty());
    let crash = &alerts.items[1];
    assert_eq!(crash.alertname, "KubePodCrashLooping");
    assert_eq!(crash.namespace, "prod");
    assert_eq!(crash.name, "api-0");
    assert_eq!(crash.severity, "warning");
    assert_eq!(crash.state, "suppressed");
    assert!(crash.inhibited);
    assert_eq!(crash.silenced_by, ["silence-watchdog"]);
}

#[test]
fn a_v2_silence_keeps_id_comment_and_name_value_is_regex() {
    let silences = parse_silences(v2_silences().as_bytes()).expect("v2 silences");
    assert_eq!(silences.items.len(), 1);
    let silence = &silences.items[0];
    assert_eq!(silence.id, "silence-watchdog");
    assert_eq!(silence.created_by, "k10s");
    assert_eq!(silence.comment, "quiet the watchdog while we drain");
    assert_eq!(silence.starts_at, "2024-01-01T00:00:00.000Z");
    assert_eq!(silence.ends_at, "2024-01-02T00:00:00.000Z");
    assert_eq!(silence.matchers.len(), 1);
    assert_eq!(silence.matchers[0].name, "alertname");
    assert_eq!(silence.matchers[0].value, "Watchdog");
    assert!(!silence.matchers[0].is_regex);
    assert!(silence.matchers[0].is_equal);
    assert_eq!(silence.matchers_dropped, 0);
}

#[test]
fn a_grafana_dashboard_is_not_alertmanager() {
    let err = parse_alerts(grafana_dashboard().as_bytes()).expect_err("dashboard");
    assert!(err.contains("Grafana"), "{err}");
    let err = parse_silences(grafana_dashboard().as_bytes()).expect_err("dashboard");
    assert!(err.contains("Grafana"), "{err}");
}

#[test]
fn a_grafana_unified_alerting_export_is_refused() {
    let err = parse_alerts(grafana_alerting().as_bytes()).expect_err("unified alerting");
    assert!(err.contains("Grafana"), "{err}");
    assert!(err.contains("Alertmanager API v2"), "{err}");
}

#[test]
fn grafana_alertmanager_config_is_refused() {
    let err = parse_alerts(grafana_am_config().as_bytes()).expect_err("am config");
    assert!(err.contains("Grafana"), "{err}");
}

#[test]
fn an_am_v1_envelope_is_not_v2() {
    let err = parse_alerts(br#"{"status":"success","data":{"alerts":[]}}"#).expect_err("v1");
    assert!(
        err.contains("JSON array"),
        "v1 is an object, not a v2 array: {err}"
    );
    assert!(!err.contains("Grafana"), "{err}");
}

#[test]
fn an_oversize_body_is_hidden_whole() {
    let mut bytes = Vec::from(b"[".as_slice());
    bytes.resize(MAX_BODY_BYTES + 2, b'0');
    bytes.push(b']');
    let err = parse_alerts(&bytes).expect_err("oversize");
    assert!(err.contains("hidden"), "{err}");
}

#[test]
fn alerts_past_the_cap_are_counted_not_kept() {
    let mut items = Vec::new();
    for i in 0..(MAX_ALERTS + 3) {
        items.push(format!(
            r#"{{"fingerprint":"{i}","labels":{{"alertname":"A{i}"}},"status":{{"state":"active"}},"startsAt":"2024-01-01T00:00:00Z"}}"#
        ));
    }
    let json = format!("[{}]", items.join(","));
    let alerts = parse_alerts(json.as_bytes()).expect("capped");
    assert_eq!(alerts.items.len(), MAX_ALERTS);
    assert!(alerts.truncated);
    assert_eq!(alerts.dropped, 3);
}

#[test]
fn a_malformed_alert_is_counted_dropped_not_shown_as_a_blank_row() {
    let json = r#"[
      {"fingerprint":"f1","labels":{"alertname":"A"},"status":{"state":"active"},"startsAt":"2024-01-01T00:00:00Z"},
      7,
      {"fingerprint":"f2","labels":["not","a","map"],"status":{"state":"active"},"startsAt":"t"},
      {"labels":{"alertname":"NoFingerprint"},"status":{"state":"active"},"startsAt":"t"}
    ]"#;
    let alerts = parse_alerts(json.as_bytes()).expect("the readable alert survives");
    assert_eq!(alerts.items.len(), 1);
    assert_eq!(alerts.items[0].fingerprint, "f1");
    assert!(alerts.truncated);
    assert_eq!(alerts.dropped, 3);
}

#[test]
fn an_alert_muted_by_a_time_interval_names_it() {
    let json = r#"[{
      "fingerprint": "f-muted",
      "labels": {"alertname": "NightlyNoise"},
      "status": {"state": "suppressed", "mutedBy": ["weekends", "nights"]},
      "startsAt": "2024-01-01T00:00:00Z"
    }]"#;
    let alerts = parse_alerts(json.as_bytes()).expect("muted alert");
    assert_eq!(alerts.items[0].muted_by, ["weekends", "nights"]);
    let page = table_page(Some(&alerts)).expect("rows");
    assert_eq!(page.rows[0].cells[9], "weekends,nights");
}

#[test]
fn a_negated_matcher_keeps_is_equal_false_and_an_omitted_one_is_true() {
    let json = r#"[{
      "id": "s-negated",
      "createdBy": "k10s",
      "comment": "everything but prod",
      "startsAt": "2024-01-01T00:00:00Z",
      "endsAt": "2024-01-02T00:00:00Z",
      "matchers": [
        {"name": "env", "value": "prod", "isRegex": false, "isEqual": false},
        {"name": "team", "value": "core", "isRegex": false}
      ]
    }]"#;
    let silences = parse_silences(json.as_bytes()).expect("negated matcher");
    let matchers = &silences.items[0].matchers;
    assert!(!matchers[0].is_equal, "env!=prod is not env=prod");
    assert!(matchers[1].is_equal, "isEqual omitted defaults to true");
}

#[test]
fn matchers_past_the_cap_are_counted_on_the_silence() {
    let matchers: Vec<String> = (0..(MAX_MATCHERS + 2))
        .map(|i| format!(r#"{{"name":"l{i}","value":"v{i}","isRegex":false}}"#))
        .collect();
    let json = format!(
        r#"[{{"id":"s-wide","createdBy":"k10s","comment":"c","startsAt":"t","endsAt":"t","matchers":[{}]}}]"#,
        matchers.join(",")
    );
    let silences = parse_silences(json.as_bytes()).expect("capped matchers");
    let silence = &silences.items[0];
    assert_eq!(silence.matchers.len(), MAX_MATCHERS);
    assert_eq!(silence.matchers_dropped, 2);
}

#[test]
fn silences_past_the_cap_are_counted_not_kept() {
    let mut items = Vec::new();
    for i in 0..(MAX_SILENCES + 2) {
        items.push(format!(
            r#"{{"id":"s{i}","createdBy":"k10s","comment":"c","startsAt":"2024-01-01T00:00:00Z","endsAt":"2024-01-02T00:00:00Z","matchers":[]}}"#
        ));
    }
    let json = format!("[{}]", items.join(","));
    let silences = parse_silences(json.as_bytes()).expect("capped");
    assert_eq!(silences.items.len(), MAX_SILENCES);
    assert!(silences.truncated);
    assert_eq!(silences.dropped, 2);
}

#[test]
fn a_long_label_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(MAX_LABEL_CHARS + 40);
    let json = format!(
        r#"[{{"fingerprint":"{huge}","labels":{{"alertname":"{huge}","severity":"{huge}","namespace":"{huge}","name":"{huge}"}},"status":{{"state":"{huge}","silencedBy":["{huge}"]}},"startsAt":"{huge}"}}]"#
    );
    let alerts = parse_alerts(json.as_bytes()).expect("clipped");
    let alert = &alerts.items[0];
    for field in [
        &alert.fingerprint,
        &alert.state,
        &alert.severity,
        &alert.alertname,
        &alert.namespace,
        &alert.name,
        &alert.starts_at,
        &alert.silenced_by[0],
    ] {
        assert!(
            field.chars().count() <= MAX_LABEL_CHARS + 1,
            "clipped where carried: {} chars",
            field.chars().count()
        );
        assert!(field.ends_with('\u{2026}'), "{field}");
    }
}

#[test]
fn a_long_silence_comment_is_clipped() {
    let huge = "c".repeat(MAX_LABEL_CHARS + 8);
    let json = format!(
        r#"[{{"id":"s1","createdBy":"{huge}","comment":"{huge}","startsAt":"t","endsAt":"t","matchers":[{{"name":"{huge}","value":"{huge}","isRegex":true}}]}}]"#
    );
    let silences = parse_silences(json.as_bytes()).expect("clipped");
    let silence = &silences.items[0];
    assert!(silence.comment.ends_with('\u{2026}'));
    assert!(silence.created_by.ends_with('\u{2026}'));
    assert!(silence.matchers[0].name.ends_with('\u{2026}'));
    assert!(silence.matchers[0].is_regex);
}

#[test]
fn table_page_is_none_when_the_caller_has_no_bound() {
    assert!(table_page(None).is_none());
}

#[test]
fn table_page_is_some_for_a_quiet_alertmanager() {
    let page = table_page(Some(&Alerts::default())).expect("bound and quiet");
    assert!(page.rows.is_empty());
    assert_eq!(page.columns.len(), 10);
}

#[test]
fn table_page_one_row_per_alert() {
    let alerts = parse_alerts(v2_alerts().as_bytes()).expect("v2");
    let page = table_page(Some(&alerts)).expect("rows");
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0].name, "Watchdog");
    assert_eq!(page.rows[1].namespace.as_deref(), Some("prod"));
    assert_eq!(page.rows[1].cells[7], "true");
    assert_eq!(page.rows[1].cells[8], "silence-watchdog");
}

#[test]
fn silence_post_bytes_are_the_v2_document() {
    let body = silence_post_body(&spec()).expect("body");
    assert_eq!(
        std::str::from_utf8(&body).expect("utf-8"),
        r#"{"matchers":[{"name":"alertname","value":"Watchdog","isRegex":false,"isEqual":true}],"startsAt":"2024-01-01T00:00:00Z","endsAt":"2024-01-02T00:00:00Z","createdBy":"k10s","comment":"quiet"}"#
    );
}

#[test]
fn a_negated_matcher_is_posted_with_is_equal_false() {
    let mut spec = spec();
    spec.matchers[0].is_equal = false;
    let body = silence_post_body(&spec).expect("body");
    let text = std::str::from_utf8(&body).expect("utf-8");
    assert!(text.contains(r#""isEqual":false"#), "{text}");
}

#[test]
fn a_silence_without_matchers_is_not_serialized() {
    let mut spec = spec();
    spec.matchers.clear();
    let err = silence_post_body(&spec).expect_err("empty");
    assert!(err.contains("matcher"), "{err}");
}

#[test]
fn a_named_token_never_uses_the_service_proxy() {
    let token = bound(ToolAuth::NamedToken("am-token".into()), proxy());
    let Fetched::Failed { why, what } = refuse_bind::<()>(&token).expect("refused") else {
        panic!("a named token on the proxy must fail closed");
    };
    assert_eq!(what, "alertmanager");
    assert!(why.contains("proxy"), "{why}");
    assert!(why.contains("Authorization"), "{why}");

    let anonymous = bound(ToolAuth::Anonymous, proxy());
    assert!(refuse_bind::<()>(&anonymous).is_none());

    let forwarded = bound(
        ToolAuth::NamedToken("am-token".into()),
        Transport::NeedsForward {
            namespace: "monitoring".into(),
            name: "alertmanager".into(),
            port: 9093,
        },
    );
    assert!(
        refuse_bind::<()>(&forwarded).is_none(),
        "a token on a forward is reach's bind"
    );
}

#[tokio::test]
async fn confirm_false_create_never_touches_the_wire() {
    let outcome = create_silence(
        &unused_client(),
        &bound(ToolAuth::Anonymous, proxy()),
        &spec(),
        false,
    )
    .await;
    match outcome {
        SilenceOutcome::NeedsConfirm { summary } => {
            assert!(summary.contains("silence"), "{summary}");
        }
        other => panic!("confirm=false is NeedsConfirm, not {other:?}"),
    }
}

#[tokio::test]
async fn confirm_false_expire_never_touches_the_wire() {
    let outcome = expire_silence(
        &unused_client(),
        &bound(ToolAuth::Anonymous, proxy()),
        "silence-watchdog",
        false,
    )
    .await;
    match outcome {
        SilenceOutcome::NeedsConfirm { summary } => {
            assert!(summary.contains("silence-watchdog"), "{summary}");
        }
        other => panic!("confirm=false is NeedsConfirm, not {other:?}"),
    }
}

#[tokio::test]
async fn a_named_token_on_the_proxy_is_refused_before_the_wire() {
    let outcome = create_silence(
        &unused_client(),
        &bound(ToolAuth::NamedToken("am-token".into()), proxy()),
        &spec(),
        true,
    )
    .await;
    match outcome {
        SilenceOutcome::Failed { why, .. } => {
            assert!(why.contains("proxy"), "{why}");
        }
        other => panic!("token+proxy is Failed, not {other:?}"),
    }
}

#[test]
fn an_empty_or_hostile_silence_id_is_not_sent() {
    assert!(silence_id_ok("").is_err());
    assert!(silence_id_ok("../etc/passwd").is_err());
    assert!(silence_id_ok("id/extra").is_err());
    assert!(silence_id_ok("silence-watchdog").is_ok());
}
