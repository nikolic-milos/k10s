use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub profile: Profile,
    pub suites: Vec<Suite>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub label: String,
    pub recorded_at: String,
    pub os: String,
    pub kernel: String,
    pub architecture: String,
    pub cpu: String,
    pub governor: String,
    pub turbo: bool,
    pub cpu_affinity: usize,
    pub rustc: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub name: String,
    pub file: String,
    pub source_schema: u32,
    pub case_keys: Vec<String>,
    #[serde(default)]
    pub top_exact: Vec<String>,
    #[serde(default)]
    pub exact: Vec<String>,
    #[serde(default)]
    pub ceilings: Vec<Ceiling>,
    pub metrics: Vec<Metric>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ceiling {
    pub field: String,
    pub maximum: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub field: String,
    pub maximum_ratio: f64,
    #[serde(default)]
    pub absolute_slack: f64,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub minimum_samples: Option<usize>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub checks: usize,
    pub skipped: usize,
    pub regressions: Vec<String>,
    pub suites: Vec<SuiteReport>,
}

#[derive(Debug)]
pub struct SuiteReport {
    pub name: String,
    pub cases: usize,
    pub checks: usize,
    pub skipped: usize,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.regressions.is_empty()
    }
}

pub fn compare(manifest_path: &Path, result_directory: &Path) -> Result<Report, Box<dyn Error>> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
    if manifest.schema_version != 1 {
        return Err(format!(
            "unsupported baseline manifest schema {}",
            manifest.schema_version
        )
        .into());
    }
    if manifest.suites.is_empty() {
        return Err("baseline manifest contains no suites".into());
    }

    let baseline_directory = manifest_path
        .parent()
        .ok_or("baseline manifest has no parent directory")?;
    let mut report = Report::default();
    for suite in &manifest.suites {
        let baseline: Value =
            serde_json::from_slice(&fs::read(baseline_directory.join(&suite.file))?)?;
        let current: Value =
            serde_json::from_slice(&fs::read(result_directory.join(&suite.file))?)?;
        let suite_report = compare_suite(suite, &baseline, &current, &mut report)?;
        report.suites.push(suite_report);
    }
    Ok(report)
}

fn compare_suite(
    suite: &Suite,
    baseline: &Value,
    current: &Value,
    report: &mut Report,
) -> Result<SuiteReport, Box<dyn Error>> {
    if suite.case_keys.is_empty() {
        return Err(format!("suite '{}' has no case keys", suite.name).into());
    }
    require_schema(suite, baseline, "baseline")?;
    require_schema(suite, current, "current result")?;

    for field in &suite.top_exact {
        report.checks += 1;
        if baseline.get(field).is_none() && current.get(field).is_none() {
            return Err(format!(
                "{}: top-level field '{field}' exists in neither document; \
                 a misspelled manifest field gates nothing",
                suite.name
            )
            .into());
        }
        if baseline.get(field) != current.get(field) {
            report.regressions.push(format!(
                "{}: top-level field '{}' changed from {} to {}",
                suite.name,
                field,
                printable(baseline.get(field)),
                printable(current.get(field))
            ));
        }
    }

    let baseline_cases = index_cases(suite, baseline, "baseline")?;
    let current_cases = index_cases(suite, current, "current result")?;
    let baseline_keys: BTreeSet<_> = baseline_cases.keys().cloned().collect();
    let current_keys: BTreeSet<_> = current_cases.keys().cloned().collect();
    for missing in baseline_keys.difference(&current_keys) {
        report
            .regressions
            .push(format!("{}: missing case {missing}", suite.name));
    }
    for added in current_keys.difference(&baseline_keys) {
        report
            .regressions
            .push(format!("{}: unexpected case {added}", suite.name));
    }

    let checks_before = report.checks;
    let skipped_before = report.skipped;
    for key in baseline_keys.intersection(&current_keys) {
        let baseline_case = baseline_cases[key];
        let current_case = current_cases[key];
        for field in &suite.exact {
            report.checks += 1;
            if baseline_case.get(field).is_none() && current_case.get(field).is_none() {
                return Err(format!(
                    "{} {key}: structural field '{field}' exists in neither document; \
                     a misspelled manifest field gates nothing",
                    suite.name
                )
                .into());
            }
            if baseline_case.get(field) != current_case.get(field) {
                report.regressions.push(format!(
                    "{} {key}: structural field '{}' changed from {} to {}",
                    suite.name,
                    field,
                    printable(baseline_case.get(field)),
                    printable(current_case.get(field))
                ));
            }
        }
        for ceiling in &suite.ceilings {
            let value = number(current_case, &ceiling.field, &suite.name, key)?;
            report.checks += 1;
            if value > ceiling.maximum {
                report.regressions.push(format!(
                    "{} {key}: '{}' is {:.6}, above ceiling {:.6}",
                    suite.name, ceiling.field, value, ceiling.maximum
                ));
            }
        }
        for metric in &suite.metrics {
            if let Some(minimum) = metric.minimum_samples {
                let baseline_samples = integer(baseline_case, "samples", &suite.name, key)?;
                let current_samples = integer(current_case, "samples", &suite.name, key)?;
                if baseline_samples < minimum {
                    report.skipped += 1;
                    continue;
                }
                if current_samples < minimum {
                    report.checks += 1;
                    report.regressions.push(format!(
                        "{} {key}: sample count collapsed from {baseline_samples} to \
                         {current_samples}, below the {minimum} needed to gate '{}'; \
                         a tail regression must not disable its own gate",
                        suite.name, metric.field
                    ));
                    continue;
                }
            }

            let Some((old, new)) =
                metric_numbers(baseline_case, current_case, metric, &suite.name, key)?
            else {
                report.skipped += 1;
                continue;
            };
            let maximum = old * metric.maximum_ratio + metric.absolute_slack;
            report.checks += 1;
            if new > maximum {
                report.regressions.push(format!(
                    "{} {key}: '{}' regressed from {:.3} to {:.3} ({:.3}x; maximum {:.3}x + {:.3})",
                    suite.name,
                    metric.field,
                    old,
                    new,
                    ratio(new, old),
                    metric.maximum_ratio,
                    metric.absolute_slack
                ));
            }
        }
    }

    Ok(SuiteReport {
        name: suite.name.clone(),
        cases: current_cases.len(),
        checks: report.checks - checks_before,
        skipped: report.skipped - skipped_before,
    })
}

fn metric_numbers(
    baseline: &Value,
    current: &Value,
    metric: &Metric,
    suite: &str,
    key: &str,
) -> Result<Option<(f64, f64)>, Box<dyn Error>> {
    let old = baseline
        .get(&metric.field)
        .ok_or_else(|| format!("{suite} {key}: baseline has no field '{}'", metric.field))?;
    let new = current.get(&metric.field).ok_or_else(|| {
        format!(
            "{suite} {key}: current result has no field '{}'",
            metric.field
        )
    })?;
    if metric.optional && old.is_null() && new.is_null() {
        return Ok(None);
    }
    let old = old
        .as_f64()
        .ok_or_else(|| format!("{suite} {key}: baseline '{}' is not numeric", metric.field))?;
    let new = new.as_f64().ok_or_else(|| {
        format!(
            "{suite} {key}: current result '{}' is not numeric",
            metric.field
        )
    })?;
    if !old.is_finite() || old < 0.0 || !new.is_finite() || new < 0.0 {
        return Err(format!(
            "{suite} {key}: '{}' values are not finite and nonnegative",
            metric.field
        )
        .into());
    }
    Ok(Some((old, new)))
}

fn require_schema(suite: &Suite, document: &Value, label: &str) -> Result<(), Box<dyn Error>> {
    let actual = document
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{} {} has no integer schema_version", suite.name, label))?;
    if actual != u64::from(suite.source_schema) {
        return Err(format!(
            "{} {} uses schema {}, expected {}",
            suite.name, label, actual, suite.source_schema
        )
        .into());
    }
    Ok(())
}

fn index_cases<'a>(
    suite: &Suite,
    document: &'a Value,
    label: &str,
) -> Result<BTreeMap<String, &'a Value>, Box<dyn Error>> {
    let cases = document
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{} {} has no cases array", suite.name, label))?;
    let mut indexed = BTreeMap::new();
    for case in cases {
        let mut parts = Vec::with_capacity(suite.case_keys.len());
        for field in &suite.case_keys {
            let value = case.get(field).ok_or_else(|| {
                format!("{} {} case has no key field '{field}'", suite.name, label)
            })?;
            parts.push(format!("{field}={}", printable(Some(value))));
        }
        let key = format!("[{}]", parts.join(", "));
        if indexed.insert(key.clone(), case).is_some() {
            return Err(format!("{} {} has duplicate case {key}", suite.name, label).into());
        }
    }
    Ok(indexed)
}

fn number(case: &Value, field: &str, suite: &str, key: &str) -> Result<f64, Box<dyn Error>> {
    let value = case
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{suite} {key}: '{field}' is not numeric"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("{suite} {key}: '{field}' is not finite and nonnegative").into());
    }
    Ok(value)
}

fn integer(case: &Value, field: &str, suite: &str, key: &str) -> Result<usize, Box<dyn Error>> {
    case.get(field)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("{suite} {key}: '{field}' is not an unsigned integer").into())
}

fn printable(value: Option<&Value>) -> String {
    value.map_or_else(|| "<missing>".to_owned(), Value::to_string)
}

fn ratio(new: f64, old: f64) -> f64 {
    if old == 0.0 { f64::INFINITY } else { new / old }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn suite() -> Suite {
        Suite {
            name: "test".to_owned(),
            file: "test.json".to_owned(),
            source_schema: 1,
            case_keys: vec!["name".to_owned()],
            top_exact: vec!["mode".to_owned()],
            exact: vec!["drawn".to_owned()],
            ceilings: vec![Ceiling {
                field: "p50_rmad".to_owned(),
                maximum: 0.1,
            }],
            metrics: vec![Metric {
                field: "p50_ns".to_owned(),
                maximum_ratio: 1.2,
                absolute_slack: 2.0,
                optional: false,
                minimum_samples: None,
            }],
        }
    }

    #[test]
    fn accepts_improvement_and_rejects_structural_change() {
        let baseline = json!({
            "schema_version": 1,
            "mode": "timing",
            "cases": [{"name": "a", "drawn": 3, "p50_rmad": 0.01, "p50_ns": 100.0}]
        });
        let current = json!({
            "schema_version": 1,
            "mode": "timing",
            "cases": [{"name": "a", "drawn": 4, "p50_rmad": 0.02, "p50_ns": 90.0}]
        });
        let mut report = Report::default();
        compare_suite(&suite(), &baseline, &current, &mut report).unwrap();
        assert_eq!(report.regressions.len(), 1);
        assert!(report.regressions[0].contains("structural field"));
    }

    #[test]
    fn skips_tails_the_baseline_never_had_enough_samples_for() {
        let mut suite = suite();
        suite.metrics[0].minimum_samples = Some(100);
        let baseline = json!({
            "schema_version": 1,
            "mode": "timing",
            "cases": [{"name": "a", "drawn": 3, "samples": 50, "p50_rmad": 0.01, "p50_ns": 100.0}]
        });
        let current = json!({
            "schema_version": 1,
            "mode": "timing",
            "cases": [{"name": "a", "drawn": 3, "samples": 50, "p50_rmad": 0.01, "p50_ns": 1000.0}]
        });
        let mut report = Report::default();
        compare_suite(&suite, &baseline, &current, &mut report).unwrap();
        assert!(report.passed());
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn a_sample_count_collapse_is_a_regression_not_a_skip() {
        let mut suite = suite();
        suite.metrics[0].minimum_samples = Some(100);
        let baseline = json!({
            "schema_version": 1,
            "mode": "timing",
            "cases": [{"name": "a", "drawn": 3, "samples": 150, "p50_rmad": 0.01, "p50_ns": 100.0}]
        });
        let current = json!({
            "schema_version": 1,
            "mode": "timing",
            "cases": [{"name": "a", "drawn": 3, "samples": 90, "p50_rmad": 0.01, "p50_ns": 100.0}]
        });
        let mut report = Report::default();
        compare_suite(&suite, &baseline, &current, &mut report).unwrap();
        assert_eq!(report.regressions.len(), 1, "{:?}", report.regressions);
        assert!(report.regressions[0].contains("sample count collapsed"));
        assert_eq!(report.skipped, 0);
    }

    #[test]
    fn an_exact_field_absent_from_both_documents_is_an_error() {
        let mut suite = suite();
        suite.exact = vec!["drwan".to_owned()];
        let doc = json!({
            "schema_version": 1,
            "mode": "timing",
            "cases": [{"name": "a", "drawn": 3, "p50_rmad": 0.01, "p50_ns": 100.0}]
        });
        let mut report = Report::default();
        let error = compare_suite(&suite, &doc, &doc, &mut report).unwrap_err();
        assert!(error.to_string().contains("gates nothing"), "{error}");

        suite.exact = vec![];
        suite.top_exact = vec!["mdoe".to_owned()];
        let error = compare_suite(&suite, &doc, &doc, &mut report).unwrap_err();
        assert!(error.to_string().contains("gates nothing"), "{error}");
    }

    #[test]
    fn a_null_exact_field_is_present_and_comparable() {
        let mut suite = suite();
        suite.exact = vec!["drawn".to_owned()];
        let baseline = json!({
            "schema_version": 1,
            "mode": "timing",
            "cases": [{"name": "a", "drawn": null, "p50_rmad": 0.01, "p50_ns": 100.0}]
        });
        let mut report = Report::default();
        compare_suite(&suite, &baseline, &baseline, &mut report).unwrap();
        assert!(report.passed());
    }
}
