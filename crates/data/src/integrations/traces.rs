//! Trace lookup against Tempo or Jaeger, via [`crate::reach`].
//!
//! A bound Tempo or Jaeger is queried by hex id. The answer is reduced here
//! to identity, parent, service, timing, status and Kubernetes resource
//! attributes. Other attributes, events and logs stay on the wire. Those fields are attacker-shaped and often hold
//! tokens, and this type has nowhere to put them.
//!
//! JSON only. Both Tempo routes request `Accept: application/json` explicitly.
//! Tempo is asked at `/api/v2/traces/{id}` first; a completed 404 permits the
//! v1 compatibility path. A partial v2 envelope is refused with its message.
//! A protobuf body is refused, not decoded. Jaeger is the query
//! service's `/api/traces/{id}` envelope.
//!
//! The body and span caps refuse rather than truncate: the body at
//! [`crate::reach::MAX_BODY_BYTES`], and the span list at [`MAX_SPANS`]. A
//! missing trace is visible; a silent drop is not.
//! Kubernetes resource identities retain their exact bytes up to 253, the
//! maximum DNS subdomain name length. Larger attributes are refused so a
//! clipped prefix cannot become an identity match.
//! Malformed timing fields and Jaeger span containers are refused. ProtoJSON
//! omitted or null fixed64 fields retain their default zero value.
//! Tempo timestamps accept unsigned JSON integers and exact quoted decimal or
//! exponent forms with an integer value. Unquoted decimal and exponent forms
//! refuse because their original precision is lost in the JSON value decoder.
//! Every Tempo span requires a nonzero sixteen-byte trace ID and eight-byte
//! span ID. Supported hex and ProtoJSON base64 forms normalize without clipping.
//! Jaeger trace IDs are nonzero 64-bit or 128-bit hex, and span IDs are
//! nonzero 64-bit hex. Only a direct parent or same-trace `CHILD_OF` reference
//! becomes a hierarchical parent; `FOLLOWS_FROM` remains outside this view.

use base64::Engine;
use kube::Client;
use serde_json::Value;

use crate::reach::{Bound, MAX_BODY_BYTES, ToolKind, tool_get_with_status};
use crate::read::Fetched;

pub const MAX_SPANS: usize = 4096;
const MAX_FIELD_CHARS: usize = 200;
const MAX_RESOURCE_IDENTITY_BYTES: usize = 253;

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
    pub namespace: String,
    pub pod: String,
    pub pod_uid: String,
    pub node: String,
}

/// One trace: the id the backend named, and every span it returned, in
/// document order, up to [`MAX_SPANS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub trace_id: String,
    pub spans: Vec<Span>,
    /// The read path attaches the bound source after a successful lookup.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceError {
    TooLarge {
        bytes: usize,
    },
    Protobuf,
    NotJson(String),
    TooManySpans,
    NotATrace,
    Rejected(String),
    InvalidField {
        field: &'static str,
        expected: &'static str,
    },
    ResourceIdentityTooLarge {
        field: &'static str,
        bytes: usize,
    },
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
            TraceError::InvalidField { field, expected } => {
                write!(f, "trace field {field} must be {expected}")
            }
            TraceError::ResourceIdentityTooLarge { field, bytes } => write!(
                f,
                "trace resource attribute {field} is {bytes} bytes; the identity cap is {MAX_RESOURCE_IDENTITY_BYTES}"
            ),
        }
    }
}

/// GET a trace by id from a bound Tempo or Jaeger.
///
/// Tempo uses `/api/v2/traces/{id}` and falls back to `/api/traces/{id}` only
/// after a completed 404. Jaeger uses `/api/traces/{id}`. The id must be hex.
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
        let (status, outcome) = tool_get_with_status(client, bound, rest).await;
        match outcome {
            Fetched::Ok(bytes) => match parse(&bytes) {
                Ok(trace)
                    if trace.trace_id.eq_ignore_ascii_case(trace_id.trim())
                        || trace.trace_id.is_empty() && trace.spans.is_empty() =>
                {
                    return Fetched::Ok(trace);
                }
                Ok(_) => {
                    return Fetched::Failed {
                        what: bound.kind.slug(),
                        why: "the trace response does not match the requested trace ID".to_string(),
                    };
                }
                Err(error) => return fail(bound, error),
            },
            Fetched::Denied { what } => return Fetched::Denied { what },
            Fetched::Failed { what, why }
                if rest.starts_with("api/v2/traces/")
                    && status == Some(http::StatusCode::NOT_FOUND) =>
            {
                last = Fetched::Failed { what, why };
            }
            Fetched::Failed { what, why } => return Fetched::Failed { what, why },
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
    if let Some(status) = root.get("status") {
        if status.as_str() == Some("PARTIAL") || status.as_u64() == Some(1) {
            return Err(TraceError::Rejected(
                root.get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("Tempo returned a partial trace")
                    .to_string(),
            ));
        }
        if status.as_str() != Some("COMPLETE") && status.as_u64() != Some(0) {
            return Err(TraceError::Rejected(
                "Tempo returned an unsupported trace completion status".to_string(),
            ));
        }
    }
    let mut spans = Vec::new();
    let mut trace_id = String::new();
    let batches = tempo_batches(root)?;
    for batch in batches {
        if !batch.is_object() {
            return Err(TraceError::NotATrace);
        }
        collect_otel_batch(batch, &mut trace_id, &mut spans)?;
    }
    Ok(Trace {
        trace_id,
        spans,
        source: String::new(),
    })
}

fn tempo_batches(root: &Value) -> Result<&[Value], TraceError> {
    let inner = root
        .get("trace")
        .filter(|value| !value.is_null())
        .unwrap_or(root);
    if !inner.is_object() {
        return Err(TraceError::NotATrace);
    }
    span_array(
        inner
            .get("resourceSpans")
            .or_else(|| inner.get("batches"))
            .or_else(|| root.get("resourceSpans"))
            .or_else(|| root.get("batches")),
    )
}

fn span_array(value: Option<&Value>) -> Result<&[Value], TraceError> {
    // ProtoJSON treats omitted or null repeated fields as an empty list.
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(array)) => Ok(array),
        _ => Err(TraceError::NotATrace),
    }
}

fn collect_otel_batch(
    batch: &Value,
    trace_id: &mut String,
    spans: &mut Vec<Span>,
) -> Result<(), TraceError> {
    let service = service_of(batch.get("resource"));
    for key in ["scopeSpans", "instrumentationLibrarySpans"] {
        let scopes = span_array(batch.get(key))?;
        for scope in scopes {
            if !scope.is_object() {
                return Err(TraceError::NotATrace);
            }
            let raw = span_array(scope.get("spans"))?;
            for span in raw {
                if !span.is_object() {
                    return Err(TraceError::NotATrace);
                }
                push_otel_span(span, &service, batch.get("resource"), trace_id, spans)?;
            }
        }
    }
    Ok(())
}

fn push_otel_span(
    span: &Value,
    service: &str,
    resource: Option<&Value>,
    trace_id: &mut String,
    spans: &mut Vec<Span>,
) -> Result<(), TraceError> {
    if spans.len() >= MAX_SPANS {
        return Err(TraceError::TooManySpans);
    }
    let span_trace = otel_id(
        span.get("traceId").or_else(|| span.get("trace_id")),
        "traceId",
        16,
        true,
    )?;
    let span_id = otel_id(
        span.get("spanId").or_else(|| span.get("span_id")),
        "spanId",
        8,
        true,
    )?;
    let parent_id = otel_id(
        span.get("parentSpanId")
            .or_else(|| span.get("parent_span_id")),
        "parentSpanId",
        8,
        false,
    )?;
    if trace_id.is_empty() {
        *trace_id = span_trace;
    } else if *trace_id != span_trace {
        return Err(TraceError::Rejected(
            "the response mixes multiple trace IDs".to_string(),
        ));
    }

    let start_nano = protojson_u64(
        span.get("startTimeUnixNano")
            .or_else(|| span.get("start_time_unix_nano")),
        "startTimeUnixNano",
    )?;
    let end_nano = protojson_u64(
        span.get("endTimeUnixNano")
            .or_else(|| span.get("end_time_unix_nano")),
        "endTimeUnixNano",
    )?;
    if end_nano < start_nano {
        return Err(TraceError::Rejected(
            "a span ends before it starts".to_string(),
        ));
    }
    spans.push(Span {
        id: span_id,
        parent: parent_id,
        name: clip(str_in(span, &["name"])),
        service: service.to_string(),
        start_us: start_nano / 1_000,
        duration_us: (end_nano - start_nano) / 1_000,
        status: otel_status(span.get("status")),
        namespace: resource_attribute(resource, "k8s.namespace.name")?,
        pod: resource_attribute(resource, "k8s.pod.name")?,
        pod_uid: resource_attribute(resource, "k8s.pod.uid")?,
        node: resource_attribute(resource, "k8s.node.name")?,
    });
    Ok(())
}

fn otel_id(
    value: Option<&Value>,
    field: &'static str,
    bytes: usize,
    required: bool,
) -> Result<String, TraceError> {
    let invalid = || TraceError::InvalidField {
        field,
        expected: if !required {
            "an eight-byte parent identifier encoded as hexadecimal or base64, or an empty root"
        } else if bytes == 16 {
            "a nonzero sixteen-byte identifier encoded as hexadecimal or base64"
        } else {
            "a nonzero eight-byte identifier encoded as hexadecimal or base64"
        },
    };
    if !required && matches!(value, None | Some(Value::Null)) {
        return Ok(String::new());
    }
    let raw = value.and_then(Value::as_str).ok_or_else(invalid)?;
    if !required && raw.is_empty() {
        return Ok(String::new());
    }
    if raw.len() > bytes * 2 {
        return Err(invalid());
    }
    if raw.len() == bytes * 2 && raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return if raw.bytes().any(|byte| byte != b'0') {
            Ok(raw.to_ascii_lowercase())
        } else if !required {
            Ok(String::new())
        } else {
            Err(invalid())
        };
    }
    match decode_b64(raw) {
        Some(decoded) if decoded.len() == bytes => {
            if decoded.iter().any(|byte| *byte != 0) {
                Ok(to_hex(&decoded))
            } else if !required {
                Ok(String::new())
            } else {
                Err(invalid())
            }
        }
        _ => Err(invalid()),
    }
}

fn resource_attribute(resource: Option<&Value>, key: &'static str) -> Result<String, TraceError> {
    let value = resource
        .and_then(|resource| resource.get("attributes"))
        .and_then(Value::as_array)
        .and_then(|attrs| attrs.iter().find(|attr| str_in(attr, &["key"]) == key))
        .map(|attr| attr_string(attr.get("value")))
        .unwrap_or_default();
    if value.len() > MAX_RESOURCE_IDENTITY_BYTES {
        return Err(TraceError::ResourceIdentityTooLarge {
            field: key,
            bytes: value.len(),
        });
    }
    Ok(value.to_string())
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
            return clip(attr_string(attr.get("value")));
        }
    }
    String::new()
}

fn attr_string(value: Option<&Value>) -> &str {
    let Some(value) = value else {
        return "";
    };
    if let Some(text) = value.as_str() {
        return text;
    }
    str_in(value, &["stringValue", "string_value"])
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
        Some(code) if code.as_u64() == Some(2) => "error".to_string(),
        Some(code) if code.as_u64() == Some(1) => "ok".to_string(),
        _ => String::new(),
    }
}

fn trace_from_jaeger(root: &Value) -> Result<Trace, TraceError> {
    let Some(data) = root.get("data").and_then(Value::as_array) else {
        return Err(TraceError::InvalidField {
            field: "Jaeger data",
            expected: "a trace array",
        });
    };
    if data.is_empty() {
        if let Some(why) = jaeger_error(root) {
            return Err(TraceError::Rejected(why));
        }
        return Ok(Trace {
            trace_id: String::new(),
            spans: Vec::new(),
            source: String::new(),
        });
    }
    if data.len() != 1 || !data[0].is_object() {
        return Err(TraceError::InvalidField {
            field: "Jaeger data",
            expected: "an array containing one trace object",
        });
    }
    let trace = &data[0];
    let processes = trace.get("processes");
    let mut spans = Vec::new();
    let mut trace_id = jaeger_id(
        trace,
        &["traceID", "traceId"],
        "Jaeger traceID",
        &[16, 32],
        true,
    )?;
    let Some(raw) = trace.get("spans").and_then(Value::as_array) else {
        return Err(TraceError::InvalidField {
            field: "Jaeger spans",
            expected: "a span array",
        });
    };
    for span in raw {
        if spans.len() >= MAX_SPANS {
            return Err(TraceError::TooManySpans);
        }
        if !span.is_object() {
            return Err(TraceError::InvalidField {
                field: "Jaeger span",
                expected: "a span object",
            });
        }
        let span_trace = jaeger_id(
            span,
            &["traceID", "traceId"],
            "Jaeger span traceID",
            &[16, 32],
            false,
        )?;
        if trace_id.is_empty() {
            trace_id = span_trace;
        } else if trace_id != span_trace {
            return Err(TraceError::Rejected(
                "Jaeger span traceID does not match the returned trace ID".to_string(),
            ));
        }
        let start_us = jaeger_u64(span.get("startTime"), "Jaeger startTime")?;
        spans.push(Span {
            id: jaeger_id(span, &["spanID", "spanId"], "Jaeger spanID", &[16], false)?,
            parent: jaeger_parent(span, &trace_id)?,
            name: clip(str_in(span, &["operationName", "operation_name"])),
            service: jaeger_service(span, processes),
            start_us,
            duration_us: jaeger_u64(span.get("duration"), "Jaeger duration")?,
            status: jaeger_status(span),
            namespace: String::new(),
            pod: String::new(),
            pod_uid: String::new(),
            node: String::new(),
        });
    }
    Ok(Trace {
        trace_id,
        spans,
        source: String::new(),
    })
}

fn jaeger_id(
    value: &Value,
    keys: &[&str],
    field: &'static str,
    hex_digits: &[usize],
    optional: bool,
) -> Result<String, TraceError> {
    let value = keys.iter().find_map(|key| value.get(*key));
    match value {
        None if optional => Ok(String::new()),
        Some(Value::String(id)) if optional && id.is_empty() => Ok(String::new()),
        Some(Value::String(id))
            if hex_digits.contains(&id.len())
                && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && id.bytes().any(|byte| byte != b'0') =>
        {
            Ok(id.to_ascii_lowercase())
        }
        _ => Err(TraceError::InvalidField {
            field,
            expected: if hex_digits.contains(&32) {
                "a nonzero 64-bit or 128-bit hexadecimal identifier"
            } else {
                "a nonzero 64-bit hexadecimal identifier"
            },
        }),
    }
}

fn jaeger_u64(value: Option<&Value>, field: &'static str) -> Result<u64, TraceError> {
    value
        .and_then(Value::as_u64)
        .ok_or(TraceError::InvalidField {
            field,
            expected: "an unsigned 64-bit JSON integer",
        })
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

fn jaeger_parent(span: &Value, trace_id: &str) -> Result<String, TraceError> {
    let mut parent = jaeger_id(
        span,
        &["parentSpanID", "parentSpanId"],
        "Jaeger parentSpanID",
        &[16],
        true,
    )?;
    let refs = match span.get("references") {
        None | Some(Value::Null) => return Ok(parent),
        Some(Value::Array(refs)) => refs,
        _ => {
            return Err(TraceError::InvalidField {
                field: "Jaeger references",
                expected: "a reference array",
            });
        }
    };
    for reference in refs {
        match str_in(reference, &["refType", "ref_type"]) {
            "FOLLOWS_FROM" => continue,
            "CHILD_OF" => {}
            _ => {
                return Err(TraceError::InvalidField {
                    field: "Jaeger reference refType",
                    expected: "CHILD_OF or FOLLOWS_FROM",
                });
            }
        }
        let referenced_trace = jaeger_id(
            reference,
            &["traceID", "traceId"],
            "Jaeger CHILD_OF traceID",
            &[16, 32],
            false,
        )?;
        if referenced_trace != trace_id {
            return Err(TraceError::Rejected(
                "Jaeger CHILD_OF refers to another trace; cross-trace parents are not supported"
                    .to_string(),
            ));
        }
        let id = jaeger_id(
            reference,
            &["spanID", "spanId"],
            "Jaeger CHILD_OF spanID",
            &[16],
            false,
        )?;
        if parent.is_empty() {
            parent = id;
        }
    }
    Ok(parent)
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

fn protojson_u64(value: Option<&Value>, field: &'static str) -> Result<u64, TraceError> {
    let parsed = match value {
        None | Some(Value::Null) => Some(0),
        Some(Value::String(text)) => decimal_u64(text),
        Some(Value::Number(number)) => number.as_u64(),
        _ => None,
    };
    parsed.ok_or(TraceError::InvalidField {
        field,
        expected: "an unsigned 64-bit JSON integer or quoted decimal/exponent text with an exact integer value",
    })
}

fn decimal_u64(text: &str) -> Option<u64> {
    // Check decimal text exactly so rounding cannot conceal a fraction or overflow.
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (text, 0),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
        Some(_) => return None,
        None => (mantissa, ""),
    };
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let scale = exponent.checked_sub(i32::try_from(fraction.len()).ok()?)?;
    let digits = whole.bytes().chain(fraction.bytes());
    let keep = if scale < 0 {
        let removed = usize::try_from(scale.unsigned_abs()).ok()?;
        if digits.clone().rev().take(removed).any(|b| b != b'0') {
            return None;
        }
        (whole.len() + fraction.len()).saturating_sub(removed)
    } else {
        whole.len() + fraction.len()
    };
    let value = digits.take(keep).try_fold(0u64, |value, digit| {
        value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })?;
    if value == 0 || scale <= 0 {
        Some(value)
    } else {
        value.checked_mul(10u64.checked_pow(u32::try_from(scale).ok()?)?)
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
