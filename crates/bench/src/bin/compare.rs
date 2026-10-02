use std::env;
use std::error::Error;
use std::path::PathBuf;

use k10s_bench::baseline;

fn main() {
    if let Err(error) = run() {
        eprintln!("benchmark comparison failed: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let manifest = PathBuf::from(
        args.next()
            .ok_or("usage: compare <baseline-manifest.json> <result-directory>")?,
    );
    let results = PathBuf::from(
        args.next()
            .ok_or("usage: compare <baseline-manifest.json> <result-directory>")?,
    );
    if args.next().is_some() {
        return Err("compare accepts exactly two arguments".into());
    }

    let report = baseline::compare(&manifest, &results)?;
    for suite in &report.suites {
        println!(
            "{}: {} cases, {} checks, {} inapplicable or low-confidence metrics skipped",
            suite.name, suite.cases, suite.checks, suite.skipped
        );
    }
    if report.passed() {
        println!(
            "baseline comparison passed: {} checks, {} inapplicable or low-confidence metrics skipped",
            report.checks, report.skipped
        );
        return Ok(());
    }

    for regression in &report.regressions {
        eprintln!("regression: {regression}");
    }
    eprintln!(
        "baseline comparison rejected {} regression(s)",
        report.regressions.len()
    );
    std::process::exit(1);
}
