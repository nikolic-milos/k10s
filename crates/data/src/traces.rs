//! Trace lookup against Tempo or Jaeger, via [`crate::reach`].
//!
//! A bound Tempo or Jaeger is queried by hex id. The answer is reduced here
//! to identity, parent, service, timing and status: attributes, events and
//! logs stay on the wire. Those fields are attacker-shaped and often hold
//! tokens, and this type has nowhere to put them.
//!
//! JSON only. Tempo's original `/api/traces/{id}` answers protobuf unless the
//! caller sends `Accept: application/json`, and [`crate::reach::tool_get`]
//! does not set that header on the proxy path, so Tempo is asked at
//! `/api/v2/traces/{id}` first (JSON by default) and the v1 path is the
//! fallback. A protobuf body is refused, not decoded. Jaeger is the query
//! service's `/api/traces/{id}` envelope.
//!
//! Two caps, both refuse rather than truncate: the body at
//! [`crate::reach::MAX_BODY_BYTES`], and the span list at [`MAX_SPANS`]. A
//! missing trace is visible; a silent drop is not.

use base64::Engine;
use kube::Client;
use serde_json::Value;

use crate::reach::{Bound, MAX_BODY_BYTES, ToolKind, tool_get};
use crate::read::Fetched;

pub const MAX_SPANS: usize = 4096;
const MAX_FIELD_CHARS: usize = 200;

/// One span, reduced to what a waterfall shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub id: String,
    /// Empty when this span is a root.
    pub parent: String,
    pub name: String,
    pub service: String,
    pub start_us: u64,
    pub duration_us: u64,
    /// `ok`, `error`, or empty when the backend did not say.
    pub status: String,
}

/// One trace: the id the backend named, and every span it returned, in
/// document order, up to [`MAX_SPANS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub trace_id: String,
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceError {
    TooLarge { bytes: usize },
    Protobuf,
    NotJson(String),
    TooManySpans,
    NotATrace,
    Rejected(String),
}

impl std::fmt::Display for TraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceError::TooLarge { bytes } => write!(
                f,
                "trace JSON is {bytes} bytes; the cap is {MAX_BODY_BYTES}"
            ),
            TraceError::Protobuf => write!(
                f,
                "the answer is protobuf OTLP, not JSON; this view reads JSON only"
            ),
            TraceError::NotJson(why) => write!(f, "trace JSON did not parse: {why}"),
            TraceError::TooManySpans => write!(
                f,
                "this trace has more than {MAX_SPANS} spans; it is not shown"
            ),
            TraceError::NotATrace => {
                write!(f, "JSON is not a Tempo or Jaeger trace")
            }
            TraceError::Rejected(why) => write!(f, "{why}"),
        }
    }
}

impl TraceError {
    fn fatal(&self) -> bool {
        matches!(
            self,
            TraceError::TooLarge { .. } | TraceError::TooManySpans | TraceError::Protobuf
        )
    }
}

/// GET a trace by id from a bound Tempo or Jaeger.
///
/// Tempo tries `/api/v2/traces/{id}` then `/api/traces/{id}`. Jaeger uses
/// `/api/traces/{id}`. The id must be hex; anything else is not sent.
pub async fn lookup(client: &Client, bound: &Bound, trace_id: &str) -> Fetched<Trace> {
    let paths = match lookup_paths(bound.kind, trace_id) {
        Fetched::Ok(paths) => paths,
        Fetched::Denied { what } => return Fetched::Denied { what },
        Fetched::Failed { what, why } => return Fetched::Failed { what, why },
    };
    let mut last = Fetched::Failed {
        what: bound.kind.slug(),
        why: "no trace endpoint answered".to_string(),
    };
    for rest in &paths {
        match tool_get(client, bound, rest).await {
            Fetched::Ok(bytes) => match parse(&bytes) {
                Ok(trace) => return Fetched::Ok(trace),
                Err(error) if error.fatal() => return fail(bound, error),
                Err(error) => last = fail(bound, error),
            },
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { what, why } => last = Fetched::Failed { what, why },
        }
    }
    last
}

fn fail(bound: &Bound, error: TraceError) -> Fetched<Trace> {
    Fetched::Failed {
        what: bound.kind.slug(),
        why: error.to_string(),
    }
}

fn lookup_paths(kind: ToolKind, trace_id: &str) -> Fetched<Vec<String>> {
    let id = trace_id.trim();
    if !trace_id_ok(id) {
        return Fetched::Failed {
            what: kind.slug(),
            why: "a trace id is 1 to 32 hex bytes; this one is not".to_string(),
        };
    }
    match kind {
        ToolKind::Jaeger => Fetched::Ok(vec![format!("api/traces/{id}")]),
        ToolKind::Tempo => Fetched::Ok(vec![
            format!("api/v2/traces/{id}"),
            format!("api/traces/{id}"),
        ]),
        other => Fetched::Failed {
            what: other.slug(),
            why: format!(
                "{} is not a trace store; bind Tempo or Jaeger",
                other.as_str()
            ),
        },
    }
}

fn trace_id_ok(id: &str) -> bool {
    let n = id.len();
    (1..=64).contains(&n) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse a Tempo or Jaeger JSON body. Tempo shape first, then Jaeger.
///
/// The body is checked against the byte cap before serde runs. A `{`
/// object is JSON; a binary payload is [`TraceError::Protobuf`].
pub fn parse(bytes: &[u8]) -> Result<Trace, TraceError> {
    let value = object(bytes)?;
    if is_tempo_shape(&value) {
        return trace_from_tempo(&value);
    }
    if is_jaeger_shape(&value) {
        return trace_from_jaeger(&value);
    }
    Err(TraceError::NotATrace)
}

fn object(bytes: &[u8]) -> Result<Value, TraceError> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(TraceError::TooLarge { bytes: bytes.len() });
    }
    match bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) {
        None => return Err(TraceError::NotATrace),
        Some(b'{') => {}
        Some(b) if b.is_ascii_graphic() => return Err(TraceError::NotATrace),
        Some(_) => return Err(TraceError::Protobuf),
    }
    serde_json::from_slice(bytes).map_err(|error| TraceError::NotJson(error.to_string()))
}

fn is_tempo_shape(value: &Value) -> bool {
    value.get("batches").is_some()
        || value.get("resourceSpans").is_some()
        || value.get("trace").is_some()
}

fn is_jaeger_shape(value: &Value) -> bool {
    value.get("data").is_some()
}

fn trace_from_tempo(root: &Value) -> Result<Trace, TraceError> {
    let mut spans = Vec::new();
    let mut trace_id = String::new();
    if let Some(batches) = tempo_batches(root) {
        for batch in batches {
            collect_otel_batch(batch, &mut trace_id, &mut spans)?;
        }
    }
    Ok(Trace { trace_id, spans })
}

fn tempo_batches(root: &Value) -> Option<&Vec<Value>> {
    let inner = root.get("trace").unwrap_or(root);
    inner
        .get("resourceSpans")
        .or_else(|| inner.get("batches"))
        .or_else(|| root.get("resourceSpans"))
        .or_else(|| root.get("batches"))
        .and_then(Value::as_array)
}

fn collect_otel_batch(
    batch: &Value,
    trace_id: &mut String,
    spans: &mut Vec<Span>,
) -> Result<(), TraceError> {
    let service = service_of(batch.get("resource"));
    for key in ["scopeSpans", "instrumentationLibrarySpans"] {
        let Some(scopes) = batch.get(key).and_then(Value::as_array) else {
            continue;
        };
        for scope in scopes {
            let Some(raw) = scope.get("spans").and_then(Value::as_array) else {
                continue;
            };
            for span in raw {
                push_otel_span(span, &service, trace_id, spans)?;
            }
        }
    }
    Ok(())
}

fn push_otel_span(
    span: &Value,
    service: &str,
    trace_id: &mut String,
    spans: &mut Vec<Span>,
) -> Result<(), TraceError> {
    if spans.len() >= MAX_SPANS {
        return Err(TraceError::TooManySpans);
    }
    if trace_id.is_empty() {
        *trace_id = normalize_id(str_in(span, &["traceId", "trace_id"]));
    }
    let start_nano = as_u64(
        span.get("startTimeUnixNano")
            .or_else(|| span.get("start_time_unix_nano")),
    );
    let end_nano = as_u64(
        span.get("endTimeUnixNano")
            .or_else(|| span.get("end_time_unix_nano")),
    );
    spans.push(Span {
        id: normalize_id(str_in(span, &["spanId", "span_id"])),
        parent: normalize_id(str_in(span, &["parentSpanId", "parent_span_id"])),
        name: clip(str_in(span, &["name"])),
        service: service.to_string(),
        start_us: start_nano / 1_000,
        duration_us: end_nano.saturating_sub(start_nano) / 1_000,
        status: otel_status(span.get("status")),
    });
    Ok(())
}

fn service_of(resource: Option<&Value>) -> String {
    let Some(resource) = resource else {
        return String::new();
    };
    let named = str_in(resource, &["serviceName", "service_name"]);
    if !named.is_empty() {
        return clip(named);
    }
    let Some(attrs) = resource.get("attributes").and_then(Value::as_array) else {
        return String::new();
    };
    for attr in attrs {
        let key = str_in(attr, &["key"]);
        if key == "service.name" {
            return clip(&attr_string(attr.get("value")));
        }
    }
    String::new()
}

fn attr_string(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    str_in(value, &["stringValue", "string_value"]).to_string()
}

fn otel_status(status: Option<&Value>) -> String {
    let Some(status) = status else {
        return String::new();
    };
    match status.get("code") {
        Some(Value::String(code)) => match code.as_str() {
            "STATUS_CODE_ERROR" | "ERROR" | "error" => "error".to_string(),
            "STATUS_CODE_OK" | "OK" | "ok" => "ok".to_string(),
            _ => String::new(),
        },
        Some(code) if as_u64(Some(code)) == 2 => "error".to_string(),
        Some(code) if as_u64(Some(code)) == 1 => "ok".to_string(),
        _ => String::new(),
    }
}

fn trace_from_jaeger(root: &Value) -> Result<Trace, TraceError> {
    let Some(data) = root.get("data").and_then(Value::as_array) else {
        return Err(TraceError::NotATrace);
    };
    if data.is_empty() {
        if let Some(why) = jaeger_error(root) {
            return Err(TraceError::Rejected(why));
        }
        return Ok(Trace {
            trace_id: String::new(),
            spans: Vec::new(),
        });
    }
    let trace = &data[0];
    let processes = trace.get("processes");
    let mut spans = Vec::new();
    let mut trace_id = normalize_id(str_in(trace, &["traceID", "traceId"]));
    let Some(raw) = trace.get("spans").and_then(Value::as_array) else {
        return Ok(Trace { trace_id, spans });
    };
    for span in raw {
        if spans.len() >= MAX_SPANS {
            return Err(TraceError::TooManySpans);
        }
        if trace_id.is_empty() {
            trace_id = normalize_id(str_in(span, &["traceID", "traceId"]));
        }
        let start_us = as_u64(span.get("startTime"));
        spans.push(Span {
            id: normalize_id(str_in(span, &["spanID", "spanId"])),
            parent: jaeger_parent(span),
            name: clip(str_in(span, &["operationName", "operation_name"])),
            service: jaeger_service(span, processes),
            start_us,
            duration_us: as_u64(span.get("duration")),
            status: jaeger_status(span),
        });
    }
    Ok(Trace { trace_id, spans })
}

fn jaeger_error(root: &Value) -> Option<String> {
    let errors = root.get("errors").and_then(Value::as_array)?;
    let first = errors.first()?;
    let why = str_in(first, &["msg", "message"]);
    if why.is_empty() {
        Some("Jaeger returned an error".to_string())
    } else {
        Some(why.to_string())
    }
}

fn jaeger_parent(span: &Value) -> String {
    let direct = str_in(span, &["parentSpanID", "parentSpanId"]);
    if !direct.is_empty() {
        return normalize_id(direct);
    }
    let Some(refs) = span.get("references").and_then(Value::as_array) else {
        return String::new();
    };
    let mut fallback = "";
    for r in refs {
        let id = str_in(r, &["spanID", "spanId"]);
        if id.is_empty() {
            continue;
        }
        let ty = str_in(r, &["refType", "ref_type"]);
        if ty.is_empty() || ty == "CHILD_OF" {
            return normalize_id(id);
        }
        if fallback.is_empty() {
            fallback = id;
        }
    }
    normalize_id(fallback)
}

fn jaeger_service(span: &Value, processes: Option<&Value>) -> String {
    let inline = span
        .get("process")
        .map(|p| str_in(p, &["serviceName", "service_name"]))
        .unwrap_or("");
    if !inline.is_empty() {
        return clip(inline);
    }
    let pid = str_in(span, &["processID", "processId"]);
    if pid.is_empty() {
        return String::new();
    }
    let Some(proc) = processes.and_then(|p| p.get(pid)) else {
        return String::new();
    };
    clip(str_in(proc, &["serviceName", "service_name"]))
}

fn jaeger_status(span: &Value) -> String {
    let Some(tags) = span.get("tags").and_then(Value::as_array) else {
        return String::new();
    };
    let mut error = false;
    let mut code = "";
    for tag in tags {
        let key = str_in(tag, &["key"]);
        match key {
            "error" => error = truthy(tag.get("value")),
            "otel.status_code" => code = tag_text(tag.get("value")),
            _ => {}
        }
    }
    if error || eq_ignore(code, "error") || eq_ignore(code, "STATUS_CODE_ERROR") {
        "error".to_string()
    } else if eq_ignore(code, "ok") || eq_ignore(code, "STATUS_CODE_OK") {
        "ok".to_string()
    } else {
        String::new()
    }
}

fn tag_text(value: Option<&Value>) -> &str {
    match value {
        Some(Value::String(s)) => s,
        _ => "",
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true" || s == "1",
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn eq_ignore(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn str_in<'a>(obj: &'a Value, keys: &[&str]) -> &'a str {
    for key in keys {
        if let Some(text) = obj.get(*key).and_then(Value::as_str) {
            return text;
        }
    }
    ""
}

fn as_u64(value: Option<&Value>) -> u64 {
    let Some(value) = value else {
        return 0;
    };
    if let Some(n) = value.as_u64() {
        return n;
    }
    if let Some(n) = value.as_i64() {
        return n.max(0) as u64;
    }
    if let Some(s) = value.as_str() {
        return s.parse().unwrap_or(0);
    }
    0
}

fn normalize_id(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    if raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return clip(&raw.to_ascii_lowercase());
    }
    match decode_b64(raw) {
        Some(bytes) if bytes.len() == 8 || bytes.len() == 16 => clip(&to_hex(&bytes)),
        _ => clip(raw),
    }
}

fn decode_b64(raw: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    STANDARD
        .decode(raw)
        .or_else(|_| STANDARD_NO_PAD.decode(raw))
        .or_else(|_| URL_SAFE.decode(raw))
        .or_else(|_| URL_SAFE_NO_PAD.decode(raw))
        .ok()
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_FIELD_CHARS) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
#[path = "traces_test.rs"]
mod tests;
