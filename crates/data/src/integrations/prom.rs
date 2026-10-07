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
//! Queries use GET so API-server proxy authorization requires only read access.
//! Parameters are URL-encoded. The HTTP round trip is bounded by
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
use std::time::Instant;

use crate::reach::{Bound, ToolAuth, ToolKind, Transport, tool_get};
use crate::read::Fetched;

mod decode;

/// PromQL text, not the encoded URL. Grafana's extractor uses the same figure.
pub const MAX_EXPR_BYTES: usize = 8 << 10;

/// A vector or matrix wider than this is not a chart, it is a dump.
pub const MAX_SERIES: usize = 256;

/// One series denser than this chose a step that this view will not draw.
pub const MAX_POINTS_PER_SERIES: usize = 5_000;

/// One sample. Time is unix milliseconds; value is the already-parsed number.
/// Non-finite samples are retained separately as gaps.
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
    /// Non-finite values break a line at their original timestamp.
    pub gaps: Vec<i64>,
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
    pub warnings: Vec<String>,
    pub infos: Vec<String>,
    pub source: String,
    pub timing: QueryTiming,
    pub step_ms: u64,
    pub expr: String,
}

/// The transport and decoder are measured separately from queueing and paint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryTiming {
    pub bytes: usize,
    pub network_ns: u64,
    pub decode_ns: u64,
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

/// Range query: GET `/api/v1/query_range` with `query`, `start`, `end`, `step`
/// and a server-side series limit. `start` and `end` are unix seconds.
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
    if end < start || step.trim().is_empty() {
        return failed(
            bound,
            "the query_range window or step is invalid; it is not sent".to_string(),
        );
    }
    query_range_path(client, bound, "api/v1/query_range", expr, start, end, step).await
}

pub(crate) async fn query_range_path(
    client: &Client,
    bound: &Bound,
    path: &str,
    expr: &str,
    start: f64,
    end: f64,
    step: &str,
) -> Fetched<QueryResult> {
    if let Some(why) = refuse_expr(expr) {
        return failed(bound, why);
    }
    if !start.is_finite() || !end.is_finite() || end < start || step.trim().is_empty() {
        return failed(
            bound,
            "the query range or resolution is invalid".to_string(),
        );
    }
    let step_ms = match super::grafana::duration_ms(step).or_else(|_| {
        let seconds: f64 = step
            .parse()
            .map_err(|_| "invalid numeric resolution".to_string())?;
        let milliseconds = seconds * 1000.0;
        if !milliseconds.is_finite()
            || milliseconds < 1.0
            || milliseconds >= u64::MAX as f64
            || (milliseconds - milliseconds.round()).abs() > 1e-6
        {
            return Err("resolution requires positive whole milliseconds".to_string());
        }
        Ok(milliseconds.round() as u64)
    }) {
        Ok(step_ms) => step_ms,
        Err(why) => return failed(bound, why),
    };
    let start_text = unix_text(start);
    let end_text = unix_text(end);
    let rest = query_path(
        path,
        &[
            ("query", expr),
            ("start", &start_text),
            ("end", &end_text),
            ("step", step),
            ("limit", "256"),
        ],
    );
    let started = Instant::now();
    match tool_get(client, bound, &rest).await {
        Fetched::Ok(bytes) => {
            let network_ns = started.elapsed().as_nanos() as u64;
            let decode_started = Instant::now();
            let result = parse_response(&bytes).map(|mut result| {
                result.timing = QueryTiming {
                    bytes: bytes.len(),
                    network_ns,
                    decode_ns: decode_started.elapsed().as_nanos() as u64,
                };
                result.source = bound.source();
                result.step_ms = step_ms;
                result.expr = expr.to_string();
                result
            });
            into_fetched(bound.kind.slug(), result)
        }
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

/// Parse a Prometheus HTTP API JSON body. The byte cap is checked here too so
/// a caller who did not go through [`tool_get`] still cannot expand a bomb.
pub fn parse_response(bytes: &[u8]) -> Result<QueryResult, String> {
    decode::parse(bytes)
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

fn query_path(path: &str, pairs: &[(&str, &str)]) -> String {
    let encoded_len = |text: &str| {
        text.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b' ' => 1,
                _ => 3,
            })
            .sum::<usize>()
    };
    let mut out = String::with_capacity(
        path.len()
            + pairs
                .iter()
                .map(|(key, value)| encoded_len(key) + encoded_len(value) + 2)
                .sum::<usize>(),
    );
    out.push_str(path);
    out.push('?');
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
