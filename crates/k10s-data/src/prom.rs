//! PromQL instant and query_range over a bound Prometheus, via [`crate::reach`].
//!
//! This is the run half of "we render queries, not dashboards". Grafana JSON
//! already named the expression; this module sends it to Prometheus, Mimir, or
//! Thanos over the HTTP API they share (`/api/v1/query`, `/api/v1/query_range`)
//! and reduces the JSON to labelled series. Native histograms and protobuf
//! remote-read are not parsed: JSON vector, matrix, and scalar are the contract.
//!
//! A PromQL answer is attacker-shaped. An expression of a few bytes can name
//! every series in the TSDB, so the expression is capped at 8KiB, the body at
//! [`crate::reach::MAX_BODY_BYTES`], the series count at [`MAX_SERIES`], and
//! each series at [`MAX_POINTS_PER_SERIES`]. Overflow is refused or counted
//! ([`QueryResult::truncated`] plus [`QueryResult::dropped_series`]), never
//! applied silently: a prefix of a range is not the range.
//!
//! Instant queries GET; range queries POST
//! `application/x-www-form-urlencoded`. The HTTP round trip is bounded by
//! [`crate::reach::PROBE_DEADLINE`]. A named token never rides
//! [`crate::reach::Transport::Proxy`]: the kube client's Authorization header
//! is already spoken for, so a token on that path would either clobber kube
//! auth or leak into Prometheus. Reach already binds those to a forward;
//! this module refuses the combination if it is handed one.
//!
//! k10s-data does not depend on k10s-theme: [`Sample`] is the same `(t_ms,
//! value)` pair a chart consumes, duplicated here so theme can map without a
//! crate edge.

use kube::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::reach::{Bound, MAX_BODY_BYTES, ToolAuth, ToolKind, Transport, tool_get, tool_post};
use crate::read::Fetched;

/// PromQL text, not the encoded URL. Grafana's extractor uses the same figure.
pub const MAX_EXPR_BYTES: usize = 8 << 10;

/// A vector or matrix wider than this is not a chart, it is a dump.
pub const MAX_SERIES: usize = 256;

/// One series denser than this chose a step that this view will not draw.
pub const MAX_POINTS_PER_SERIES: usize = 5_000;

const FORM_URLENCODED: &str = "application/x-www-form-urlencoded";

/// One sample. Time is unix milliseconds; value is the already-parsed number.
/// NaN and Inf are dropped before a Sample exists.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub t_ms: i64,
    pub value: f64,
}

/// Labels as Prometheus returned them, points as `(millis, finite f64)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub labels: Vec<(String, String)>,
    pub points: Vec<(i64, f64)>,
}

impl Series {
    /// The `(t_ms, value)` pair a chart consumes; no k10s-theme crate edge.
    pub fn samples(&self) -> impl Iterator<Item = Sample> + '_ {
        self.points
            .iter()
            .copied()
            .map(|(t_ms, value)| Sample { t_ms, value })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultType {
    Vector,
    Matrix,
    Scalar,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub result_type: ResultType,
    pub series: Vec<Series>,
    /// Some series the server returned were not kept.
    pub truncated: bool,
    /// How many series were refused (over the series cap, over the point cap,
    /// or not an object). Zero when [`Self::truncated`] is false.
    pub dropped_series: usize,
}

#[derive(Deserialize)]
struct WireEnvelope {
    #[serde(default)]
    status: String,
    #[serde(default)]
    data: Option<WireData>,
    #[serde(default)]
    error: String,
    #[serde(default, rename = "errorType")]
    error_type: String,
}

#[derive(Deserialize, Default)]
struct WireData {
    #[serde(default, rename = "resultType")]
    result_type: String,
    #[serde(default)]
    result: Value,
}

/// Instant query: GET `/api/v1/query`. `time` is unix seconds, Prometheus's unit.
pub async fn query(
    client: &Client,
    bound: &Bound,
    expr: &str,
    time: Option<f64>,
) -> Fetched<QueryResult> {
    if let Some(failed) = refuse_bind(bound) {
        return failed;
    }
    if let Some(why) = refuse_expr(expr) {
        return failed(bound, why);
    }
    if time.is_some_and(|ts| !ts.is_finite()) {
        return failed(
            bound,
            "the evaluation time is not a finite unix timestamp".to_string(),
        );
    }
    let mut rest = String::from("api/v1/query?query=");
    push_encoded(expr, &mut rest);
    if let Some(ts) = time {
        rest.push_str("&time=");
        push_encoded(&unix_text(ts), &mut rest);
    }
    match tool_get(client, bound, &rest).await {
        Fetched::Ok(bytes) => into_fetched(bound.kind.slug(), parse_response(&bytes)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

/// Range query: POST `/api/v1/query_range` as form fields `query`, `start`,
/// `end`, `step`. `start` and `end` are unix seconds.
pub async fn query_range(
    client: &Client,
    bound: &Bound,
    expr: &str,
    start: f64,
    end: f64,
    step: &str,
) -> Fetched<QueryResult> {
    if let Some(failed) = refuse_bind(bound) {
        return failed;
    }
    if let Some(why) = refuse_expr(expr) {
        return failed(bound, why);
    }
    if !start.is_finite() || !end.is_finite() {
        return failed(
            bound,
            "the query_range window is not a pair of finite unix timestamps".to_string(),
        );
    }
    if step.trim().is_empty() {
        return failed(
            bound,
            "the query_range step is empty; it is not sent".to_string(),
        );
    }
    let start_text = unix_text(start);
    let end_text = unix_text(end);
    let body = form_body(&[
        ("query", expr),
        ("start", &start_text),
        ("end", &end_text),
        ("step", step),
    ]);
    match tool_post(
        client,
        bound,
        "api/v1/query_range",
        FORM_URLENCODED,
        body.into_bytes(),
    )
    .await
    {
        Fetched::Ok(bytes) => into_fetched(bound.kind.slug(), parse_response(&bytes)),
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

/// Parse a Prometheus HTTP API JSON body. The byte cap is checked here too so
/// a caller who did not go through [`tool_get`] still cannot expand a bomb.
pub fn parse_response(bytes: &[u8]) -> Result<QueryResult, String> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!(
            "the Prometheus answer is more than {MAX_BODY_BYTES} bytes; it is hidden"
        ));
    }
    let envelope: WireEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| format!("the Prometheus answer is not JSON: {error}"))?;
    if envelope.status != "success" {
        let why = if envelope.error.is_empty() {
            format!(
                "Prometheus status is {}",
                if envelope.status.is_empty() {
                    "missing"
                } else {
                    envelope.status.as_str()
                }
            )
        } else if envelope.error_type.is_empty() {
            envelope.error
        } else {
            format!("{}: {}", envelope.error_type, envelope.error)
        };
        return Err(why);
    }
    let data = match envelope.data {
        Some(data) => data,
        None => return Err("Prometheus success carried no data".to_string()),
    };
    match data.result_type.as_str() {
        "vector" => parse_array(ResultType::Vector, &data.result, Mode::Instant),
        "matrix" => parse_array(ResultType::Matrix, &data.result, Mode::Range),
        "scalar" => parse_scalar(&data.result),
        "string" => Err("Prometheus returned a string result, not a numeric series".to_string()),
        "histogram" => Err(
            "native histograms are not parsed; JSON vector and matrix are the contract".to_string(),
        ),
        other => {
            let label = if other.is_empty() { "missing" } else { other };
            Err(format!(
                "Prometheus resultType {label} is not JSON vector, matrix, or scalar"
            ))
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Instant,
    Range,
}

fn parse_array(result_type: ResultType, result: &Value, mode: Mode) -> Result<QueryResult, String> {
    let items = match result.as_array() {
        Some(items) => items,
        None => {
            return Err("Prometheus result is not an array of series".to_string());
        }
    };
    let mut dropped_series = items.len().saturating_sub(MAX_SERIES);
    let mut series = Vec::new();
    for item in items.iter().take(MAX_SERIES) {
        match read_series(item, mode) {
            None => dropped_series += 1,
            Some(found) if found.points.len() > MAX_POINTS_PER_SERIES => {
                dropped_series += 1;
            }
            Some(found) => series.push(found),
        }
    }
    Ok(QueryResult {
        result_type,
        series,
        truncated: dropped_series > 0,
        dropped_series,
    })
}

fn parse_scalar(result: &Value) -> Result<QueryResult, String> {
    let points: Vec<(i64, f64)> = read_sample(result).into_iter().collect();
    Ok(QueryResult {
        result_type: ResultType::Scalar,
        series: vec![Series {
            labels: Vec::new(),
            points,
        }],
        truncated: false,
        dropped_series: 0,
    })
}

fn read_series(item: &Value, mode: Mode) -> Option<Series> {
    let obj = item.as_object()?;
    let labels = labels_of(obj.get("metric"));
    let points = match mode {
        Mode::Instant => obj.get("value").and_then(read_sample).into_iter().collect(),
        Mode::Range => obj
            .get("values")
            .and_then(Value::as_array)
            .map(|values| read_samples(values))
            .unwrap_or_default(),
    };
    Some(Series { labels, points })
}

fn read_samples(values: &[Value]) -> Vec<(i64, f64)> {
    values.iter().filter_map(read_sample).collect()
}

fn read_sample(value: &Value) -> Option<(i64, f64)> {
    let pair = value.as_array()?;
    if pair.len() < 2 {
        return None;
    }
    let ts = json_f64(&pair[0])?;
    let sample = json_f64(&pair[1])?;
    if !ts.is_finite() || !sample.is_finite() {
        return None;
    }
    let t_ms = (ts * 1000.0).round() as i64;
    Some((t_ms, sample))
}

fn json_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn labels_of(metric: Option<&Value>) -> Vec<(String, String)> {
    let Some(Value::Object(map)) = metric else {
        return Vec::new();
    };
    let mut labels: Vec<(String, String)> = map
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
        .collect();
    labels.sort_by(|a, b| a.0.cmp(&b.0));
    labels
}

pub(crate) fn refuse_bind(bound: &Bound) -> Option<Fetched<QueryResult>> {
    if !matches!(
        bound.kind,
        ToolKind::Prometheus | ToolKind::Mimir | ToolKind::Thanos
    ) {
        return Some(failed(
            bound,
            format!(
                "{} does not speak the Prometheus HTTP API; bind Prometheus, Mimir, or Thanos",
                bound.kind.as_str()
            ),
        ));
    }
    if matches!(bound.auth, ToolAuth::NamedToken(_))
        && matches!(bound.transport, Transport::Proxy { .. })
    {
        return Some(failed(
            bound,
            format!(
                "a named {} token cannot ride the API-server proxy; it would share the kube \
                 client's Authorization header. Bind through a port-forward or a settings URL",
                bound.kind.as_str()
            ),
        ));
    }
    None
}

pub(crate) fn refuse_expr(expr: &str) -> Option<String> {
    if expr.len() > MAX_EXPR_BYTES {
        return Some(format!(
            "the PromQL expression is {} bytes; the cap is {MAX_EXPR_BYTES}",
            expr.len()
        ));
    }
    if expr.trim().is_empty() {
        return Some("the PromQL expression is empty; it is not sent".to_string());
    }
    // Grafana variables ($var, $__rate_interval, ${var}) are not PromQL and
    // nothing here can expand them. `$1` in a label_replace replacement and
    // a `$` regex anchor are plain PromQL and must pass.
    let mut rest = expr;
    while let Some(position) = rest.find('$') {
        rest = &rest[position + 1..];
        let next = rest.bytes().next();
        if matches!(next, Some(b'{') | Some(b'_')) || next.is_some_and(|b| b.is_ascii_alphabetic())
        {
            return Some(
                "$ is Grafana's engine (variables, $__rate_interval), not PromQL we can send"
                    .to_string(),
            );
        }
    }
    None
}

fn failed(bound: &Bound, why: String) -> Fetched<QueryResult> {
    Fetched::Failed {
        what: bound.kind.slug(),
        why,
    }
}

fn into_fetched(what: &'static str, parsed: Result<QueryResult, String>) -> Fetched<QueryResult> {
    match parsed {
        Ok(result) => Fetched::Ok(result),
        Err(why) => Fetched::Failed { what, why },
    }
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        push_encoded(key, &mut out);
        out.push('=');
        push_encoded(value, &mut out);
    }
    out
}

fn unix_text(ts: f64) -> String {
    format!("{ts}")
}

#[cfg(test)]
pub(crate) fn encoded_value(text: &str) -> String {
    let mut out = String::new();
    push_encoded(text, &mut out);
    out
}

fn push_encoded(text: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in text.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            byte => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
}

#[cfg(test)]
#[path = "prom_test.rs"]
mod tests;
