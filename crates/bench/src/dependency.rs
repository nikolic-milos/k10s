use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    pub maximum_packages: usize,
    pub maximum_external_packages: usize,
    pub maximum_git_packages: usize,
    pub allowed_git_sources: BTreeSet<String>,
    pub forbidden_packages: BTreeSet<String>,
    /// Normal and build dependencies allowed across a protected crate boundary.
    /// Development dependencies may exercise higher layers in tests.
    #[serde(default)]
    pub restricted_dependencies: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Counts {
    pub packages: usize,
    pub external_packages: usize,
    pub git_packages: usize,
}

#[derive(Debug)]
pub struct Audit {
    pub counts: Counts,
    pub violations: Vec<String>,
}

impl Audit {
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }
}

pub fn audit(policy: &Policy, metadata: &Value) -> Result<Audit, Box<dyn Error>> {
    if policy.schema_version != 1 {
        return Err(format!(
            "unsupported dependency policy schema {}",
            policy.schema_version
        )
        .into());
    }
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("cargo metadata has no packages array")?;

    let mut external_packages = 0;
    let mut git_packages = 0;
    let mut git_sources = BTreeSet::new();
    let mut present_packages = BTreeSet::new();
    for package in packages {
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .ok_or("cargo metadata package has no name")?;
        present_packages.insert(name.to_owned());
        if let Some(source) = package.get("source").and_then(Value::as_str) {
            external_packages += 1;
            if source.starts_with("git+") {
                git_packages += 1;
                git_sources.insert(source.to_owned());
            }
        }
    }

    let counts = Counts {
        packages: packages.len(),
        external_packages,
        git_packages,
    };
    let mut violations = Vec::new();
    check_maximum(
        "packages",
        counts.packages,
        policy.maximum_packages,
        &mut violations,
    );
    check_maximum(
        "external packages",
        counts.external_packages,
        policy.maximum_external_packages,
        &mut violations,
    );
    check_maximum(
        "Git packages",
        counts.git_packages,
        policy.maximum_git_packages,
        &mut violations,
    );
    for source in git_sources.difference(&policy.allowed_git_sources) {
        violations.push(format!("unapproved Git source: {source}"));
    }
    for package in policy.forbidden_packages.intersection(&present_packages) {
        violations.push(format!("forbidden package is present: {package}"));
    }
    for (name, allowed) in &policy.restricted_dependencies {
        let package = packages
            .iter()
            .find(|package| package["name"].as_str() == Some(name) && package["source"].is_null())
            .ok_or_else(|| format!("protected local package is missing: {name}"))?;
        let dependencies = package["dependencies"]
            .as_array()
            .ok_or_else(|| format!("cargo metadata package {name} has no dependencies array"))?;
        for dependency in dependencies {
            if dependency["kind"].as_str() == Some("dev") {
                continue;
            }
            let target = dependency["name"]
                .as_str()
                .ok_or("cargo metadata dependency has no name")?;
            if !allowed.contains(target) {
                violations.push(format!("{name} cannot depend on {target} outside tests"));
            }
        }
    }

    Ok(Audit { counts, violations })
}

fn check_maximum(name: &str, actual: usize, maximum: usize, violations: &mut Vec<String>) {
    if actual > maximum {
        violations.push(format!("{name}: {actual} exceeds ceiling {maximum}"));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn policy() -> Policy {
        Policy {
            schema_version: 1,
            maximum_packages: 2,
            maximum_external_packages: 1,
            maximum_git_packages: 1,
            allowed_git_sources: BTreeSet::from(["git+https://example/repo?rev=a#a".to_owned()]),
            forbidden_packages: BTreeSet::from(["obsolete".to_owned()]),
            restricted_dependencies: BTreeMap::new(),
        }
    }

    #[test]
    fn exact_graph_passes() {
        let metadata = json!({"packages": [
            {"name": "local", "source": null},
            {"name": "remote", "source": "git+https://example/repo?rev=a#a"}
        ]});
        let audit = audit(&policy(), &metadata).unwrap();
        assert!(audit.passed());
        assert_eq!(
            audit.counts,
            Counts {
                packages: 2,
                external_packages: 1,
                git_packages: 1
            }
        );
    }

    #[test]
    fn reports_growth_sources_and_forbidden_packages() {
        let metadata = json!({"packages": [
            {"name": "local", "source": null},
            {"name": "obsolete", "source": "git+https://evil/repo?rev=b#b"},
            {"name": "extra", "source": "registry+https://example/index"}
        ]});
        let audit = audit(&policy(), &metadata).unwrap();
        assert_eq!(audit.violations.len(), 4);
    }

    #[test]
    fn protected_boundary_allows_test_consumers_but_rejects_runtime_and_build_edges() {
        let mut policy = policy();
        policy
            .restricted_dependencies
            .insert("local".into(), BTreeSet::from(["model".into()]));
        for kind in [Value::Null, json!("build")] {
            let metadata = json!({"packages": [{
                "name": "local", "source": null, "dependencies": [
                    {"name": "model", "kind": null},
                    {"name": "renderer", "kind": "dev"}
                ]
            }]});
            assert!(audit(&policy, &metadata).unwrap().passed());

            let mut leaked = metadata;
            leaked["packages"][0]["dependencies"][1]["kind"] = kind;
            leaked["packages"][0]["dependencies"][1]["rename"] = json!("model");
            assert_eq!(
                audit(&policy, &leaked).unwrap().violations,
                ["local cannot depend on renderer outside tests"]
            );
        }
    }

    #[test]
    fn protected_boundary_requires_its_local_package_and_dependency_metadata() {
        let mut policy = policy();
        policy
            .restricted_dependencies
            .insert("local".into(), BTreeSet::new());
        for packages in [
            json!([]),
            json!([{"name": "local", "source": "registry+https://example/index"}]),
            json!([{"name": "local", "source": null}]),
            json!([{"name": "local", "source": null, "dependencies": [{}]}]),
        ] {
            assert!(audit(&policy, &json!({"packages": packages})).is_err());
        }
    }
}
