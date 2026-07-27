use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use k10s_bench::dependency::{self, Policy};
use serde_json::Value;

fn main() {
    if let Err(error) = run() {
        eprintln!("dependency budget failed: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let policy_path = PathBuf::from(
        args.next()
            .ok_or("usage: dependency-budget <policy.json> [cargo-metadata.json]")?,
    );
    let metadata_path = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err("dependency-budget accepts at most two arguments".into());
    }

    let policy: Policy = serde_json::from_slice(&fs::read(policy_path)?)?;
    let metadata: Value = match metadata_path {
        Some(path) => serde_json::from_slice(&fs::read(path)?)?,
        None => serde_json::from_slice(&cargo_metadata()?)?,
    };
    let audit = dependency::audit(&policy, &metadata)?;
    println!(
        "dependency graph: {} packages, {} external, {} Git-sourced",
        audit.counts.packages, audit.counts.external_packages, audit.counts.git_packages
    );
    if audit.passed() {
        return Ok(());
    }
    for violation in &audit.violations {
        eprintln!("dependency violation: {violation}");
    }
    std::process::exit(1);
}

fn cargo_metadata() -> Result<Vec<u8>, Box<dyn Error>> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .args(["metadata", "--locked", "--format-version", "1"])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(output.stdout)
}
