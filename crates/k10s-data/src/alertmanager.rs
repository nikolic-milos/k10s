//! Alertmanager HTTP API v2 over a bound tool, via [`crate::reach`].
//!
//! k10s never installs Alertmanager. This module assumes a [`crate::reach::Bound`]:
//! a missing bind is the caller's [`crate::reach::ToolReach::Absent`], and
//! absence stays invisible. Once bound, GET `api/v2/alerts` and
//! `api/v2/silences` (Alertmanager 0.16+) and reduce the JSON to the fields
//! a list can show. Create and expire are the POST and DELETE Alertmanager
//! already speaks; nothing is patched that the API does not name.
//!
//! A Grafana alerting document is not Alertmanager. The v2 lists are JSON
//! arrays. A dashboard, a unified-alerting export, or Grafana's
//! `alertmanager_config` envelope is refused, not coerced into rows.
//!
//! Auth is [`crate::reach::Bound::auth`] the way PromQL uses it: a named
//! token is for Alertmanager, not kube, and never rides the API-server
//! proxy. A Secret is not read.
//!
//! Bind [`crate::reach::ToolKind::Alertmanager`]. A Prometheus bind is
//! refused: the v2 lists are not PromQL.

use kube::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::reach::{
    Bound, FoundService, MAX_BODY_BYTES, PROBE_DEADLINE, ToolAuth, Transport, proxy_path, tool_get,
};
use crate::read::{Fetched, classify};

pub const MAX_ALERTS: usize = 512;
pub const MAX_SILENCES: usize = 256;
pub const MAX_LABEL_CHARS: usize = 200;
pub const MAX_MATCHERS: usize = 32;

const WHAT: &str = "alertmanager";
const ALERTS: &str = "api/v2/alerts";
const SILENCES: &str = "api/v2/silences";
const JSON: &str = "application/json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub fingerprint: String,
    pub state: String,
    pub severity: String,
    pub alertname: String,
    pub namespace: String,
    pub name: String,
    /// `labels.cluster`, as the rule's own `by()` produced it. Empty when the
    /// rule dropped it, which is a fact about the alert -- never backfilled
    /// from the kubeconfig, because this alert may not be about this cluster.
    pub cluster: String,
    /// `labels.pod`. Kept beside `name` because a pod-level alert and an
    /// object-level one join to different things.
    pub pod: String,
    /// `annotations.summary`: the sentence whoever wrote the rule wrote.
    pub summary: String,
    /// `annotations.runbook_url`. A link to open, never a page to fetch.
    pub runbook_url: String,
    pub starts_at: String,
    pub inhibited: bool,
    pub silenced_by: Vec<String>,
    pub muted_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Alerts {
    pub items: Vec<Alert>,
    pub truncated: bool,
    pub dropped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matcher {
    pub name: String,
    pub value: String,
    pub is_regex: bool,
    pub is_equal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silence {
    pub id: String,
    pub created_by: String,
    pub comment: String,
    pub starts_at: String,
    pub ends_at: String,
    pub matchers: Vec<Matcher>,
    /// Matchers past [`MAX_MATCHERS`] the wire named but this list does not
    /// carry. Matchers are ANDed, so a trimmed list looks broader than the
    /// silence is; a non-zero count says so.
    pub matchers_dropped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Silences {
    pub items: Vec<Silence>,
    pub truncated: bool,
    pub dropped: usize,
}

/// The body POST `/api/v2/silences` already speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilenceSpec {
    pub matchers: Vec<Matcher>,
    pub starts_at: String,
    pub ends_at: String,
    pub created_by: String,
    pub comment: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SilenceOutcome {
    Applied { id: String, summary: String },
    NeedsConfirm { summary: String },
    Denied { what: &'static str, why: String },
    Failed { what: &'static str, why: String },
}

#[derive(Deserialize)]
struct WireAlert {
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    status: WireAlertStatus,
    #[serde(default, rename = "startsAt")]
    starts_at: String,
}

#[derive(Deserialize, Default)]
struct WireAlertStatus {
    #[serde(default)]
    state: String,
    #[serde(default, rename = "inhibitedBy")]
    inhibited_by: Vec<String>,
    #[serde(default, rename = "silencedBy")]
    silenced_by: Vec<String>,
    #[serde(default, rename = "mutedBy")]
    muted_by: Vec<String>,
}

#[derive(Deserialize, Default)]
struct WireSilence {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "createdBy")]
    created_by: String,
    #[serde(default)]
    comment: String,
    #[serde(default, rename = "startsAt")]
    starts_at: String,
    #[serde(default, rename = "endsAt")]
    ends_at: String,
    #[serde(default)]
    matchers: Vec<WireMatcher>,
}

#[derive(Deserialize)]
struct WireMatcher {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default, rename = "isRegex")]
    is_regex: bool,
    // An omitted isEqual is true upstream; a plain default would invert it.
    #[serde(default = "default_true", rename = "isEqual")]
    is_equal: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Default)]
struct WireCreated {
    #[serde(default, rename = "silenceID")]
    silence_id: String,
}

/// GET `/api/v2/alerts` on a bound Alertmanager.
pub async fn fetch_alerts(client: &Client, bound: &Bound) -> Fetched<Alerts> {
    if let Some(failed) = refuse_bind(bound) {
        return failed;
    }
    finish(tool_get(client, bound, ALERTS).await, parse_alerts)
}

/// GET `/api/v2/silences` on a bound Alertmanager.
pub async fn fetch_silences(client: &Client, bound: &Bound) -> Fetched<Silences> {
    if let Some(failed) = refuse_bind(bound) {
        return failed;
    }
    finish(tool_get(client, bound, SILENCES).await, parse_silences)
}

/// An alert whose rule dropped `cluster` has no cluster context, and saying so
/// is the honest cell. Filling it from the connected kubeconfig would assert
/// that this alert is about this cluster, which is exactly what the missing
/// label leaves unknown.
fn cluster_cell(alert: &Alert) -> String {
    if alert.cluster.is_empty() {
        return "alert has no cluster context".to_string();
    }
    alert.cluster.clone()
}

/// Native list rows. `None` when the caller has no Bound: this module
/// cannot invent presence. An empty `Some` is Alertmanager, quiet.
pub fn table_page(alerts: Option<&Alerts>) -> Option<TablePage> {
    let alerts = alerts?;
    let columns = [
        "Fingerprint",
        "State",
        "Severity",
        "Alertname",
        "Namespace",
        "Name",
        "Pod",
        "Cluster",
        "Runbook",
        "Starts",
        "Inhibited",
        "Silenced",
        "Muted",
    ]
    .iter()
    .map(|name| TableColumn {
        name: name.to_string(),
        wide: false,
    })
    .collect();
    let rows = alerts
        .items
        .iter()
        .map(|alert| {
            let silenced = alert.silenced_by.join(",");
            let muted = alert.muted_by.join(",");
            TableRow {
                cells: vec![
                    alert.fingerprint.clone(),
                    alert.state.clone(),
                    alert.severity.clone(),
                    alert.alertname.clone(),
                    alert.namespace.clone(),
                    alert.name.clone(),
                    alert.pod.clone(),
                    cluster_cell(alert),
                    alert.runbook_url.clone(),
                    alert.starts_at.clone(),
                    if alert.inhibited {
                        "true".to_string()
                    } else {
                        String::new()
                    },
                    silenced,
                    muted,
                ],
                name: if alert.alertname.is_empty() {
                    alert.fingerprint.clone()
                } else {
                    alert.alertname.clone()
                },
                namespace: if alert.namespace.is_empty() {
                    None
                } else {
                    Some(alert.namespace.clone())
                },
                uid: alert.fingerprint.clone(),
            }
        })
        .collect();
    Some(TablePage {
        columns,
        rows,
        truncated: alerts.truncated,
        continue_token: None,
    })
}

/// POST `/api/v2/silences`. `confirm=false` returns [`SilenceOutcome::NeedsConfirm`]
/// and does not touch the wire.
pub async fn create_silence(
    client: &Client,
    bound: &Bound,
    spec: &SilenceSpec,
    confirm: bool,
) -> SilenceOutcome {
    if let Some(failed) = refuse_auth(bound) {
        return failed;
    }
    let body = match silence_post_body(spec) {
        Ok(body) => body,
        Err(why) => return failed_outcome(why),
    };
    let summary = format!(
        "create silence for {} {}",
        spec.matchers.len(),
        if spec.matchers.len() == 1 {
            "matcher"
        } else {
            "matchers"
        }
    );
    if !confirm {
        return SilenceOutcome::NeedsConfirm { summary };
    }
    match tool_write(client, bound, http::Method::POST, SILENCES, Some(body)).await {
        Fetched::Ok(bytes) => match parse_created(&bytes) {
            Ok(id) => SilenceOutcome::Applied { id, summary },
            Err(why) => failed_outcome(why),
        },
        Fetched::Denied { .. } => SilenceOutcome::Denied {
            what: WHAT,
            why: "access denied for this account".to_string(),
        },
        Fetched::Failed { why, .. } => failed_outcome(why),
    }
}

/// DELETE `/api/v2/silence/{id}`. `confirm=false` does not touch the wire.
pub async fn expire_silence(
    client: &Client,
    bound: &Bound,
    id: &str,
    confirm: bool,
) -> SilenceOutcome {
    if let Some(failed) = refuse_auth(bound) {
        return failed;
    }
    let id = match silence_id_ok(id) {
        Ok(id) => id,
        Err(why) => return failed_outcome(why),
    };
    let summary = format!("expire silence {id}");
    if !confirm {
        return SilenceOutcome::NeedsConfirm { summary };
    }
    match tool_write(
        client,
        bound,
        http::Method::DELETE,
        &format!("api/v2/silence/{id}"),
        None,
    )
    .await
    {
        Fetched::Ok(_) => SilenceOutcome::Applied {
            id: id.to_string(),
            summary,
        },
        Fetched::Denied { .. } => SilenceOutcome::Denied {
            what: WHAT,
            why: "access denied for this account".to_string(),
        },
        Fetched::Failed { why, .. } => failed_outcome(why),
    }
}

/// Parse Alertmanager v2 GET `/api/v2/alerts`. The body cap is checked here
/// so a caller who did not go through [`tool_get`] still cannot expand a bomb.
pub fn parse_alerts(bytes: &[u8]) -> Result<Alerts, String> {
    let items = parse_v2_array(bytes, "alerts")?;
    let dropped = items.len().saturating_sub(MAX_ALERTS);
    let mut out = Vec::new();
    let mut unreadable = 0usize;
    for item in items.iter().take(MAX_ALERTS) {
        match alert_of(item) {
            Some(alert) => out.push(alert),
            None => unreadable += 1,
        }
    }
    Ok(Alerts {
        items: out,
        truncated: dropped > 0 || unreadable > 0,
        dropped: dropped + unreadable,
    })
}

/// Parse Alertmanager v2 GET `/api/v2/silences`.
pub fn parse_silences(bytes: &[u8]) -> Result<Silences, String> {
    let items = parse_v2_array(bytes, "silences")?;
    let dropped = items.len().saturating_sub(MAX_SILENCES);
    let mut out = Vec::new();
    let mut unreadable = 0usize;
    for item in items.iter().take(MAX_SILENCES) {
        match silence_of(item) {
            Some(silence) => out.push(silence),
            None => unreadable += 1,
        }
    }
    Ok(Silences {
        items: out,
        truncated: dropped > 0 || unreadable > 0,
        dropped: dropped + unreadable,
    })
}

fn parse_v2_array(bytes: &[u8], what: &str) -> Result<Vec<Value>, String> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!(
            "the Alertmanager {what} answer is more than {MAX_BODY_BYTES} bytes; it is hidden"
        ));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("the Alertmanager {what} answer is not JSON: {error}"))?;
    if let Some(why) = refuse_grafana(&value) {
        return Err(why);
    }
    match value {
        Value::Array(items) => Ok(items),
        _ => Err(format!(
            "Alertmanager v2 {what} are a JSON array; this document is not"
        )),
    }
}

fn refuse_grafana(value: &Value) -> Option<String> {
    let obj = value.as_object()?;
    let grafana = obj.contains_key("panels")
        || obj.contains_key("dashboard")
        || obj.contains_key("grafana_alert")
        || obj.contains_key("alertmanager_config")
        || obj.contains_key("contactPoints")
        || (obj.contains_key("groups") && obj.contains_key("apiVersion"));
    grafana.then(|| "this is a Grafana alerting document, not Alertmanager API v2".to_string())
}

fn alert_of(value: &Value) -> Option<Alert> {
    let wire: WireAlert = serde_json::from_value(value.clone()).ok()?;
    if wire.fingerprint.is_empty() {
        return None;
    }
    let label = |key: &str| wire.labels.get(key).map(String::as_str).unwrap_or("");
    let annotation = |key: &str| wire.annotations.get(key).map(String::as_str).unwrap_or("");
    Some(Alert {
        fingerprint: clip(&wire.fingerprint),
        state: clip(&wire.status.state),
        severity: clip(label("severity")),
        alertname: clip(label("alertname")),
        namespace: clip(label("namespace")),
        name: clip(label("name")),
        cluster: clip(label("cluster")),
        pod: clip(label("pod")),
        summary: clip(annotation("summary")),
        runbook_url: clip(annotation("runbook_url")),
        starts_at: clip(&wire.starts_at),
        inhibited: !wire.status.inhibited_by.is_empty(),
        silenced_by: wire.status.silenced_by.iter().map(|id| clip(id)).collect(),
        muted_by: wire.status.muted_by.iter().map(|id| clip(id)).collect(),
    })
}

fn silence_of(value: &Value) -> Option<Silence> {
    let wire: WireSilence = serde_json::from_value(value.clone()).ok()?;
    if wire.id.is_empty() {
        return None;
    }
    Some(Silence {
        id: clip(&wire.id),
        created_by: clip(&wire.created_by),
        comment: clip(&wire.comment),
        starts_at: clip(&wire.starts_at),
        ends_at: clip(&wire.ends_at),
        matchers_dropped: wire.matchers.len().saturating_sub(MAX_MATCHERS),
        matchers: wire
            .matchers
            .into_iter()
            .take(MAX_MATCHERS)
            .map(|matcher| Matcher {
                name: clip(&matcher.name),
                value: clip(&matcher.value),
                is_regex: matcher.is_regex,
                is_equal: matcher.is_equal,
            })
            .collect(),
    })
}

fn parse_created(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!(
            "the Alertmanager silence answer is more than {MAX_BODY_BYTES} bytes; it is hidden"
        ));
    }
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(String::new());
    }
    let created: WireCreated = serde_json::from_slice(bytes)
        .map_err(|error| format!("Alertmanager did not name a silenceID: {error}"))?;
    Ok(clip(&created.silence_id))
}

pub(crate) fn silence_post_body(spec: &SilenceSpec) -> Result<Vec<u8>, String> {
    if spec.matchers.is_empty() {
        return Err("a silence needs at least one matcher; it is not sent".to_string());
    }
    if spec.matchers.len() > MAX_MATCHERS {
        return Err(format!(
            "a silence names {} matchers; the cap is {MAX_MATCHERS}",
            spec.matchers.len()
        ));
    }
    for matcher in &spec.matchers {
        if matcher.name.trim().is_empty() {
            return Err("a silence matcher with an empty name is not sent".to_string());
        }
        if matcher.name.len() > MAX_LABEL_CHARS || matcher.value.len() > MAX_LABEL_CHARS {
            return Err(format!(
                "a silence matcher exceeds {MAX_LABEL_CHARS} characters; it is not sent"
            ));
        }
    }
    if spec.starts_at.trim().is_empty() || spec.ends_at.trim().is_empty() {
        return Err("a silence needs startsAt and endsAt; it is not sent".to_string());
    }
    if spec.created_by.trim().is_empty() {
        return Err("a silence needs createdBy; it is not sent".to_string());
    }
    if spec.comment.trim().is_empty() {
        return Err("a silence needs a comment; it is not sent".to_string());
    }
    let mut out = String::from("{\"matchers\":[");
    for (i, matcher) in spec.matchers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        push_json_string(&mut out, &matcher.name);
        out.push_str(",\"value\":");
        push_json_string(&mut out, &matcher.value);
        out.push_str(",\"isRegex\":");
        out.push_str(if matcher.is_regex { "true" } else { "false" });
        out.push_str(",\"isEqual\":");
        out.push_str(if matcher.is_equal { "true" } else { "false" });
        out.push('}');
    }
    out.push_str("],\"startsAt\":");
    push_json_string(&mut out, spec.starts_at.trim());
    out.push_str(",\"endsAt\":");
    push_json_string(&mut out, spec.ends_at.trim());
    out.push_str(",\"createdBy\":");
    push_json_string(&mut out, spec.created_by.trim());
    out.push_str(",\"comment\":");
    push_json_string(&mut out, spec.comment.trim());
    out.push('}');
    Ok(out.into_bytes())
}

fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let code = c as u32;
                out.push_str(&format!("\\u{code:04x}"));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn silence_id_ok(id: &str) -> Result<&str, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("a silence id is empty; it is not sent".to_string());
    }
    if id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(
            "a silence id is letters, digits, hyphen, or underscore; this one is not".to_string(),
        );
    }
    Ok(id)
}

pub(crate) fn refuse_bind<T>(bound: &Bound) -> Option<Fetched<T>> {
    if bound.kind != crate::reach::ToolKind::Alertmanager {
        return Some(Fetched::Failed {
            what: WHAT,
            why: format!(
                "{} is not Alertmanager; bind Alertmanager",
                bound.kind.as_str()
            ),
        });
    }
    if matches!(bound.auth, ToolAuth::NamedToken(_))
        && matches!(bound.transport, Transport::Proxy { .. })
    {
        return Some(Fetched::Failed {
            what: WHAT,
            why: "a named Alertmanager token cannot ride the API-server proxy; it would share the kube \
                 client's Authorization header. Bind through a port-forward or a settings URL"
                .to_string(),
        });
    }
    None
}

fn finish<T>(fetched: Fetched<Vec<u8>>, parse: fn(&[u8]) -> Result<T, String>) -> Fetched<T> {
    match fetched {
        Fetched::Ok(bytes) => match parse(&bytes) {
            Ok(value) => Fetched::Ok(value),
            Err(why) => Fetched::Failed { what: WHAT, why },
        },
        Fetched::Denied { .. } => Fetched::Denied { what: WHAT },
        Fetched::Failed { why, .. } => Fetched::Failed { what: WHAT, why },
    }
}

fn refuse_auth(bound: &Bound) -> Option<SilenceOutcome> {
    match refuse_bind::<()>(bound)? {
        Fetched::Failed { why, .. } => Some(failed_outcome(why)),
        Fetched::Denied { .. } => Some(SilenceOutcome::Denied {
            what: WHAT,
            why: "access denied for this account".to_string(),
        }),
        Fetched::Ok(()) => None,
    }
}

fn failed_outcome(why: String) -> SilenceOutcome {
    SilenceOutcome::Failed { what: WHAT, why }
}

async fn tool_write(
    client: &Client,
    bound: &Bound,
    method: http::Method,
    rest: &str,
    body: Option<Vec<u8>>,
) -> Fetched<Vec<u8>> {
    let Transport::Proxy {
        namespace,
        service,
        port,
    } = &bound.transport
    else {
        return Fetched::Failed {
            what: WHAT,
            why: match &bound.transport {
                Transport::NeedsForward { .. } => {
                    "Alertmanager needs a port-forward before a silence can be written; open one \
                     from the forwards panel"
                        .to_string()
                }
                Transport::Url { .. } => {
                    "Alertmanager silence writes are only implemented on the API-server proxy"
                        .to_string()
                }
                Transport::Proxy { .. } => unreachable!("matched above"),
            },
        };
    };
    if body
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_BODY_BYTES)
    {
        return Fetched::Failed {
            what: WHAT,
            why: "the silence itself exceeds 8 MiB; it is not sent".to_string(),
        };
    }
    let found = FoundService {
        kind: bound.kind,
        namespace: namespace.clone(),
        name: service.clone(),
        port: *port,
        port_name: None,
    };
    let path = proxy_path(&found, rest);
    let bytes = body.unwrap_or_default();
    let builder = http::Request::builder().method(method).uri(&path);
    let builder = if bytes.is_empty() {
        builder
    } else {
        builder.header(http::header::CONTENT_TYPE, JSON)
    };
    let request = match builder.body(bytes) {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what: WHAT,
                why: error.to_string(),
            };
        }
    };
    match tokio::time::timeout(PROBE_DEADLINE, client.request_text(request)).await {
        Err(_) => Fetched::Failed {
            what: WHAT,
            why: "Alertmanager did not answer within 4 seconds".to_string(),
        },
        Ok(Ok(text)) if text.len() > MAX_BODY_BYTES => Fetched::Failed {
            what: WHAT,
            why: format!(
                "Alertmanager answered with more than {MAX_BODY_BYTES} bytes; the body is hidden"
            ),
        },
        Ok(Ok(text)) => Fetched::Ok(text.into_bytes()),
        Ok(Err(error)) => classify(WHAT, &error),
    }
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_LABEL_CHARS) {
        Some((at, _)) => {
            let mut out = text[..at].to_string();
            out.push('\u{2026}');
            out
        }
        None => text.to_string(),
    }
}

#[cfg(test)]
#[path = "alertmanager_test.rs"]
mod tests;
