//! Folds the one-line reports `k10s --startup-bench --json` prints, one per
//! process, into the case shape the baseline comparator reads.
//!
//! A startup measurement is one process. Ten of them are a sample. This binary
//! reads the reports as JSON lines on stdin, groups them by what was launched
//! (the chooser, a generated scene of N objects, or a cluster), and writes one
//! document with a case per group carrying the median, maximum and relative
//! deviation of the milestones a person can see: the first frame, the useful
//! frame, and the window. It refuses to write at all, exiting non-zero, when a
//! group has fewer reports than `--min-samples`, or when a launch a person
//! expects to be instant (the chooser, or a generated scene of at most 25,000
//! objects) presents its useful frame later than `--budget-ms`. That is the
//! same fail-closed rule the editor and shell suites apply to a keystroke: a
//! ratio against the last recording accepts whatever the last recording was.
//! One hundred milliseconds is the point past which a launch stops reading as
//! immediate, and six 60 Hz frames past the first one.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_MIN_SAMPLES: usize = 10;
const DEFAULT_BUDGET_MS: f64 = 100.0;
const INSTANT_OBJECTS: u64 = 25_000;

#[derive(Serialize)]
struct Document {
    schema_version: u32,
    mode: &'static str,
    machine: String,
    platform: String,
    viewport: Value,
    cases: Vec<Case>,
}

#[derive(Serialize)]
struct Case {
    source: String,
    objects: u64,
    samples: usize,
    first_presented_p50_ms: f64,
    first_presented_max_ms: f64,
    first_presented_rmad: f64,
    useful_presented_p50_ms: f64,
    useful_presented_max_ms: f64,
    useful_presented_rmad: f64,
    window_built_p50_ms: f64,
}

struct Args {
    min_samples: usize,
    budget_ms: f64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        min_samples: DEFAULT_MIN_SAMPLES,
        budget_ms: DEFAULT_BUDGET_MS,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--min-samples" => {
                args.min_samples = value
                    .parse()
                    .map_err(|_| format!("--min-samples {value}: not a count"))?
            }
            "--budget-ms" => {
                args.budget_ms = value
                    .parse()
                    .map_err(|_| format!("--budget-ms {value}: not a number"))?
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(args)
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Median absolute deviation over the median, the same noise figure the
/// batching harness reports as `p50_rmad`.
fn rmad(sorted: &[f64]) -> f64 {
    let med = median(sorted);
    if med == 0.0 {
        return 0.0;
    }
    let mut deviations: Vec<f64> = sorted.iter().map(|v| (v - med).abs()).collect();
    deviations.sort_by(f64::total_cmp);
    median(&deviations) / med
}

fn number(report: &Value, section: &str, field: &str) -> Result<f64, String> {
    report
        .get(section)
        .and_then(|s| s.get(field))
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("report has no numeric {section}.{field}"))
}

fn run(args: &Args) -> Result<Document, String> {
    let stdin = io::stdin();
    let mut groups: BTreeMap<(String, u64), Vec<Value>> = BTreeMap::new();
    let mut machine = None;
    let mut platform = None;
    let mut viewport = None;
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("reading stdin: {e}"))?;
        if line.trim().is_empty() || !line.starts_with('{') {
            continue;
        }
        let report: Value =
            serde_json::from_str(&line).map_err(|e| format!("a report line is not JSON: {e}"))?;
        if report.get("mode").and_then(Value::as_str) != Some("startup") {
            return Err("a report line is not a startup report".to_string());
        }
        if report.get("schema_version").and_then(Value::as_u64) != Some(2) {
            return Err(
                "a report line has a startup schema this aggregator does not read".to_string(),
            );
        }
        let source = report
            .get("source")
            .and_then(Value::as_str)
            .ok_or("report has no source")?
            .to_string();
        let objects = report
            .get("generator")
            .and_then(|g| g.get("objects"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        for (slot, key) in [(&mut machine, "machine"), (&mut platform, "platform")] {
            let value = report
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("report has no {key}"))?
                .to_string();
            match slot {
                Some(have) if *have != value => {
                    return Err(format!("reports disagree on {key}: {have} and {value}"));
                }
                Some(_) => {}
                None => *slot = Some(value),
            }
        }
        let vp = report
            .get("viewport")
            .cloned()
            .ok_or("report has no viewport")?;
        match &viewport {
            Some(have) if *have != vp => return Err("reports disagree on viewport".to_string()),
            Some(_) => {}
            None => viewport = Some(vp),
        }
        groups.entry((source, objects)).or_default().push(report);
    }
    if groups.is_empty() {
        return Err("no startup reports on stdin".to_string());
    }
    let mut cases = Vec::new();
    for ((source, objects), reports) in groups {
        if reports.len() < args.min_samples {
            return Err(format!(
                "{source} objects={objects}: {} reports, below the {} needed for a case",
                reports.len(),
                args.min_samples
            ));
        }
        let column = |field: &str| -> Result<Vec<f64>, String> {
            let mut values = reports
                .iter()
                .map(|r| number(r, "milestones_ms", field))
                .collect::<Result<Vec<_>, _>>()?;
            values.sort_by(f64::total_cmp);
            Ok(values)
        };
        let first = column("first_presented")?;
        let useful = column("useful_presented")?;
        let window = column("window_built")?;
        let case = Case {
            samples: reports.len(),
            first_presented_p50_ms: median(&first),
            first_presented_max_ms: *first.last().expect("non-empty"),
            first_presented_rmad: rmad(&first),
            useful_presented_p50_ms: median(&useful),
            useful_presented_max_ms: *useful.last().expect("non-empty"),
            useful_presented_rmad: rmad(&useful),
            window_built_p50_ms: median(&window),
            source: source.clone(),
            objects,
        };
        let instant = source == "launch" || (source == "generator" && objects <= INSTANT_OBJECTS);
        if instant && case.useful_presented_p50_ms > args.budget_ms {
            return Err(format!(
                "{source} objects={objects}: useful frame median {:.1} ms is past the {:.0} ms \
                 budget a launch a person expects to be immediate carries; refusing to report",
                case.useful_presented_p50_ms, args.budget_ms
            ));
        }
        cases.push(case);
    }
    Ok(Document {
        schema_version: SCHEMA_VERSION,
        mode: "startup",
        machine: machine.expect("at least one report"),
        platform: platform.expect("at least one report"),
        viewport: viewport.expect("at least one report"),
        cases,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(why) => {
            eprintln!("startup-aggregate: {why}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(document) => {
            let text = serde_json::to_string_pretty(&document).expect("a serializable document");
            let mut out = io::stdout().lock();
            let _ = out.write_all(text.as_bytes());
            let _ = out.write_all(b"\n");
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("startup-aggregate: {why}");
            ExitCode::from(1)
        }
    }
}
