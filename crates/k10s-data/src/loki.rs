//! LogQL client over a bound Loki, via [`crate::reach`].
//!
//! We render queries, not Grafana's log panel. A bound Loki already answers
//! `/loki/api/v1/query_range` and `/loki/api/v1/query`; this module is the
//! typed half of that HTTP, not a replica of Explore. Tails ask
//! `direction=BACKWARD` so the newest lines arrive first, and `limit` is sent
//! as a query parameter so Loki itself bounds the answer rather than us
//! downloading a window and throwing most of it away.
//!
//! The bytes are attacker-shaped. Sixteen streams, two thousand lines, eight
//! kibibytes per line, and [`crate::reach::MAX_BODY_BYTES`] on the body.
//! Crossing a cap is [`Logs::truncated`] plus a count of what did not fit; a
//! silent drop would look like the cluster had fewer logs. An oversize body is
//! refused whole, because half a JSON document is not a query result.

use kube::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::reach::{self, Bound, MAX_BODY_BYTES};
use crate::read::Fetched;

pub const MAX_STREAMS: usize = 16;
pub const MAX_LINES: usize = 2_000;
pub const MAX_LINE_BYTES: usize = 8 << 10;
pub const MAX_QUERY_BYTES: usize = 8 << 10;

const QUERY_RANGE: &str = "loki/api/v1/query_range";
const QUERY: &str = "loki/api/v1/query";

/// A window of LogQL. `limit` 0 means the module's own cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeQuery {
    pub query: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub limit: usize,
}

/// Instant LogQL. `time_ns` none means Loki's now. `limit` 0 means the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstantQuery {
    pub query: String,
    pub time_ns: Option<u64>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub ts_ns: u64,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogStream {
    pub labels: Vec<(String, String)>,
    pub lines: Vec<LogLine>,
}

/// Streams Loki named, reduced to what a panel can hold.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Logs {
    pub streams: Vec<LogStream>,
    /// A cap was applied. The counts say which, so a short result is not
    /// mistaken for a quiet cluster.
    pub truncated: bool,
    pub dropped_streams: usize,
    pub dropped_lines: usize,
    pub clipped_lines: usize,
}

#[derive(Deserialize, Default)]
struct WireEnvelope {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: String,
    #[serde(default, rename = "errorType")]
    error_type: String,
    #[serde(default)]
    data: WireData,
}

#[derive(Deserialize, Default)]
struct WireData {
    #[serde(default, rename = "resultType")]
    result_type: String,
    #[serde(default)]
    result: Value,
}

/// POST the range on a proxy bind (LogQL is often too long for a URL), GET
/// otherwise: [`crate::reach::tool_post`] is proxy-only, and a settings URL
/// still has to work.
pub async fn query_range(client: &Client, bound: &Bound, query: &RangeQuery) -> Fetched<Logs> {
    match range_form(query) {
        Ok(form) => fetch_parsed(client, bound, QUERY_RANGE, &form).await,
        Err(why) => Fetched::Failed { what: "loki", why },
    }
}

pub async fn query(client: &Client, bound: &Bound, query: &InstantQuery) -> Fetched<Logs> {
    match instant_form(query) {
        Ok(form) => fetch_parsed(client, bound, QUERY, &form).await,
        Err(why) => Fetched::Failed { what: "loki", why },
    }
}

pub(crate) fn range_form(query: &RangeQuery) -> Result<String, String> {
    reject_query(&query.query)?;
    if query.start_ns > query.end_ns {
        return Err("the range starts after it ends".to_string());
    }
    let start = query.start_ns.to_string();
    let end = query.end_ns.to_string();
    let limit = effective_limit(query.limit).to_string();
    Ok(form(&[
        ("query", query.query.trim()),
        ("start", &start),
        ("end", &end),
        ("limit", &limit),
        ("direction", "BACKWARD"),
    ]))
}

pub(crate) fn instant_form(query: &InstantQuery) -> Result<String, String> {
    reject_query(&query.query)?;
    let limit = effective_limit(query.limit).to_string();
    let time = query.time_ns.map(|t| t.to_string());
    let mut pairs = vec![
        ("query", query.query.trim()),
        ("limit", limit.as_str()),
        ("direction", "BACKWARD"),
    ];
    if let Some(time) = time.as_deref() {
        pairs.push(("time", time));
    }
    Ok(form(&pairs))
}

fn reject_query(query: &str) -> Result<(), String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("the LogQL query is empty".to_string());
    }
    if query.len() > MAX_QUERY_BYTES {
        return Err(format!(
            "the LogQL query is {} bytes; the cap is {MAX_QUERY_BYTES}",
            query.len()
        ));
    }
    Ok(())
}

fn effective_limit(limit: usize) -> usize {
    if limit == 0 {
        MAX_LINES
    } else {
        limit.min(MAX_LINES)
    }
}

async fn fetch_parsed(client: &Client, bound: &Bound, path: &str, form: &str) -> Fetched<Logs> {
    finish(speak(client, bound, path, form).await)
}

pub(crate) fn finish(fetched: Fetched<Vec<u8>>) -> Fetched<Logs> {
    match fetched {
        Fetched::Ok(bytes) => match parse(&bytes) {
            Ok(logs) => Fetched::Ok(logs),
            Err(why) => Fetched::Failed { what: "loki", why },
        },
        Fetched::Denied { what } => Fetched::Denied { what },
        Fetched::Failed { what, why } => Fetched::Failed { what, why },
    }
}

async fn speak(client: &Client, bound: &Bound, path: &str, form: &str) -> Fetched<Vec<u8>> {
    match &bound.transport {
        reach::Transport::Proxy { .. } => {
            reach::tool_post(
                client,
                bound,
                path,
                "application/x-www-form-urlencoded",
                form.as_bytes().to_vec(),
            )
            .await
        }
        _ => {
            let rest = format!("{path}?{form}");
            reach::tool_get(client, bound, &rest).await
        }
    }
}

/// Parse Loki's query JSON. Caps are applied here so a caller that already
/// holds the bytes still cannot smuggle an unbounded result into a panel.
pub fn parse(bytes: &[u8]) -> Result<Logs, String> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!(
            "Loki answered with more than {MAX_BODY_BYTES} bytes; the body is hidden"
        ));
    }
    let envelope: WireEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| format!("Loki's answer is not query JSON: {error}"))?;
    if !envelope.status.is_empty() && envelope.status != "success" {
        let why = if !envelope.error.is_empty() {
            envelope.error
        } else if !envelope.error_type.is_empty() {
            envelope.error_type
        } else {
            format!("Loki answered status {}", envelope.status)
        };
        return Err(why);
    }
    let result_type = envelope.data.result_type.to_ascii_lowercase();
    if !result_type.is_empty() && result_type != "streams" {
        return Err(format!(
            "this LogQL produced {result_type}, not log streams; metric LogQL is not shown here"
        ));
    }
    let Some(items) = envelope.data.result.as_array() else {
        if matches!(envelope.data.result, Value::Null) {
            return Ok(Logs::default());
        }
        return Err("Loki's result is not a stream list".to_string());
    };
    Ok(collect(items))
}

fn collect(items: &[Value]) -> Logs {
    let mut logs = Logs::default();
    let mut total_lines = 0usize;
    for item in items {
        let values = item.get("values").and_then(Value::as_array);
        let raw_len = values.map(Vec::len).unwrap_or(0);
        // A stream that would start with no room left is dropped whole. A
        // stream that fills the last of the line budget is kept, and the
        // next iteration hits this branch.
        if logs.streams.len() >= MAX_STREAMS || total_lines >= MAX_LINES {
            logs.dropped_streams += 1;
            logs.dropped_lines += raw_len;
            logs.truncated = true;
            continue;
        }
        let mut lines = Vec::new();
        if let Some(values) = values {
            for (i, value) in values.iter().enumerate() {
                if total_lines >= MAX_LINES {
                    logs.dropped_lines += values.len() - i;
                    logs.truncated = true;
                    break;
                }
                match line_of(value) {
                    None => {
                        logs.dropped_lines += 1;
                        logs.truncated = true;
                    }
                    Some(mut line) => {
                        if clip_line(&mut line) {
                            logs.clipped_lines += 1;
                            logs.truncated = true;
                        }
                        lines.push(line);
                        total_lines += 1;
                    }
                }
            }
        }
        logs.streams.push(LogStream {
            labels: labels_of(item),
            lines,
        });
    }
    logs
}

fn labels_of(item: &Value) -> Vec<(String, String)> {
    let Some(obj) = item.get("stream").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut labels: Vec<(String, String)> = obj
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
        .collect();
    labels.sort_by(|a, b| a.0.cmp(&b.0));
    labels
}

fn line_of(value: &Value) -> Option<LogLine> {
    let arr = value.as_array()?;
    let ts = arr.first()?.as_str()?;
    let line = arr.get(1)?.as_str()?;
    let ts_ns = ts.parse::<u64>().ok()?;
    Some(LogLine {
        ts_ns,
        line: line.to_string(),
    })
}

fn clip_line(line: &mut LogLine) -> bool {
    if line.line.len() <= MAX_LINE_BYTES {
        return false;
    }
    let mut cut = MAX_LINE_BYTES;
    while cut > 0 && !line.line.is_char_boundary(cut) {
        cut -= 1;
    }
    line.line.truncate(cut);
    line.line.push('\u{2026}');
    true
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(key);
        out.push('=');
        encode_form_into(value, &mut out);
    }
    out
}

fn encode_form_into(text: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in text.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
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
#[path = "loki_test.rs"]
mod tests;
