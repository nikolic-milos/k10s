use k10s_clustergen::Scenario;
use k10s_world::LayoutMode;

pub const USAGE: &str = "\
usage: k10s [options]

cluster options:
  --cluster                               read a real cluster instead of the generator
  --context NAME                          kubeconfig context (default: current-context)
  --namespace NS                          namespace to probe RBAC in; repeatable.
                                          only needed where cluster-wide list is denied
  --sync-timeout SECS                     how long to wait for the initial list (default 30, max 3600)
  --list-contexts                         print the kubeconfig's contexts and exit

generator options:
  --objects N                             objects to generate (default 25000, max 1000000)
  --seed S                                generator seed (default 55)
  --churn EVENTS_PER_SEC                  churn events per second (default 120, max 100000)
  --scenario NAME                         platform|observability|data|ns-fanout|wl-fanout
                                          (default platform)

shared options:
  --layout spread|dense                   layout mode (default spread)
  --machine LABEL                         hardware label required by either benchmark
                                          (e.g. linux-x86_64-i5-12600k)
  --bench                                 run the scripted flight bench (needs --machine)
  --startup-bench                         exit after the first useful presented frame
                                          (needs --machine)
  --json                                  benchmark report as JSON on stdout
  -h, --help                              print this message

unrecognized arguments are reported on stderr and ignored";

const DEFAULT_SYNC_TIMEOUT_SECS: f32 = 30.0;
const MAX_SYNC_TIMEOUT_SECS: f32 = 3600.0;
const MAX_CHURN_PER_SEC: f32 = 100_000.0;
const MAX_OBJECTS: u32 = 1_000_000;

const PLACEHOLDER_MACHINES: &[&str] = &["my-box", "unlabeled", "todo", "test", "placeholder"];

const VALUE_FLAGS: [&str; 9] = [
    "--objects",
    "--seed",
    "--churn",
    "--scenario",
    "--layout",
    "--machine",
    "--context",
    "--namespace",
    "--sync-timeout",
];

const SCENARIOS: [Scenario; 5] = [
    Scenario::Platform,
    Scenario::Observability,
    Scenario::Data,
    Scenario::NsFanOut,
    Scenario::WlFanOut,
];
const LAYOUTS: [LayoutMode; 2] = [LayoutMode::Spread, LayoutMode::Dense];

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub objects: u32,
    pub seed: u64,
    pub churn: f32,
    pub scenario: Scenario,
    pub layout: LayoutMode,
    pub machine: Option<String>,
    pub bench: bool,
    pub startup_bench: bool,
    pub json: bool,
    pub help: bool,
    pub cluster: bool,
    pub churn_explicit: bool,
    pub objects_explicit: bool,
    pub seed_explicit: bool,
    pub scenario_explicit: bool,
    pub context: Option<String>,
    pub namespaces: Vec<String>,
    pub sync_timeout_secs: f32,
    pub sync_timeout_explicit: bool,
    pub list_contexts: bool,
    pub ignored: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            objects: 25_000,
            seed: 55,
            churn: 120.0,
            scenario: Scenario::Platform,
            layout: LayoutMode::Spread,
            machine: None,
            bench: false,
            startup_bench: false,
            json: false,
            help: false,
            cluster: false,
            churn_explicit: false,
            objects_explicit: false,
            seed_explicit: false,
            scenario_explicit: false,
            context: None,
            namespaces: Vec::new(),
            sync_timeout_secs: DEFAULT_SYNC_TIMEOUT_SECS,
            sync_timeout_explicit: false,
            list_contexts: false,
            ignored: Vec::new(),
        }
    }
}

impl Args {
    pub fn machine_label(&self) -> String {
        self.machine
            .clone()
            .expect("parse rejects a benchmark without a usable --machine")
    }

    pub fn measuring(&self) -> bool {
        self.bench || self.startup_bench
    }

    pub fn effective_churn(&self) -> f32 {
        if self.cluster { 0.0 } else { self.churn }
    }

    pub fn churn_was_overridden(&self) -> bool {
        self.cluster && self.churn_explicit && self.churn > 0.0
    }

    pub fn sync_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f32(self.sync_timeout_secs)
    }

    /// Whether the command line already said what this window is going to show.
    ///
    /// The launch screen exists for the person who typed `k10s` and nothing
    /// else. Somebody who named a cluster, or shaped a generated one, or is
    /// recording a bench flight has answered the question the screen would ask,
    /// and asking it again would be a modal in the way of an answer they already
    /// gave. A recording is the strictest of the three: its environment must not
    /// depend on the recording machine's home directory at all.
    pub fn scene_was_named(&self) -> bool {
        self.cluster
            || self.bench
            || self.objects_explicit
            || self.seed_explicit
            || self.scenario_explicit
            || self.churn_explicit
    }

    pub fn cluster_flags_without_cluster(&self) -> Vec<&'static str> {
        if self.cluster || self.list_contexts {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.context.is_some() {
            out.push("--context");
        }
        if !self.namespaces.is_empty() {
            out.push("--namespace");
        }
        if self.sync_timeout_explicit {
            out.push("--sync-timeout");
        }
        out
    }

    pub fn generator_flags_with_cluster(&self) -> Vec<&'static str> {
        if !self.cluster {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.objects_explicit {
            out.push("--objects");
        }
        if self.seed_explicit {
            out.push("--seed");
        }
        if self.scenario_explicit {
            out.push("--scenario");
        }
        out
    }
}

pub fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    MissingValue {
        flag: &'static str,
    },
    BadValue {
        flag: &'static str,
        expected: String,
        got: String,
    },
    JsonWithoutBench,
    MissingMachineLabel,
    BadMachineLabel {
        got: String,
    },
    BenchWithCluster,
    ConflictingBenchmarks,
}

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArgError::MissingValue { flag } => write!(f, "{flag} needs a value"),
            ArgError::BadValue {
                flag,
                expected,
                got,
            } => write!(f, "{flag} expects {expected}, got {got}"),
            ArgError::JsonWithoutBench => {
                write!(f, "--json requires --bench or --startup-bench")
            }
            ArgError::MissingMachineLabel => {
                write!(f, "benchmarks require --machine LABEL")
            }
            ArgError::BadMachineLabel { got } => write!(
                f,
                "--machine rejects placeholder {got:?}; pass a hardware-labelled name \
                 (e.g. linux-x86_64-i5-12600k)"
            ),
            ArgError::BenchWithCluster => write!(
                f,
                "--bench needs the generator: its baselines are for a fixed scene, \
                 so a cluster's numbers cannot be compared against them"
            ),
            ArgError::ConflictingBenchmarks => write!(
                f,
                "--bench and --startup-bench are separate measurements; run one at a time"
            ),
        }
    }
}

impl std::error::Error for ArgError {}

pub fn parse(argv: impl Iterator<Item = String>) -> Result<Args, ArgError> {
    let mut args = Args::default();
    let mut rest = argv;
    while let Some(arg) = rest.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, inline)) if VALUE_FLAGS.contains(&flag) => (flag, Some(inline)),
            _ => (arg.as_str(), None),
        };
        match flag {
            "--objects" => {
                let objects: u32 = number("--objects", "a u32", inline, &mut rest)?;
                if objects > MAX_OBJECTS {
                    return Err(ArgError::BadValue {
                        flag: "--objects",
                        expected: format!("a count up to {MAX_OBJECTS}"),
                        got: objects.to_string(),
                    });
                }
                args.objects = objects;
                args.objects_explicit = true;
            }
            "--seed" => {
                args.seed = number("--seed", "a u64", inline, &mut rest)?;
                args.seed_explicit = true;
            }
            "--churn" => {
                args.churn = bounded("--churn", "a rate", MAX_CHURN_PER_SEC, inline, &mut rest)?;
                args.churn_explicit = true;
            }
            "--scenario" => {
                let got = value("--scenario", inline, &mut rest)?;
                args.scenario = Scenario::parse(&got).ok_or_else(|| ArgError::BadValue {
                    flag: "--scenario",
                    expected: SCENARIOS.map(|s| s.as_str()).join("|"),
                    got,
                })?;
                args.scenario_explicit = true;
            }
            "--layout" => {
                let got = value("--layout", inline, &mut rest)?;
                args.layout = LayoutMode::parse(&got).ok_or_else(|| ArgError::BadValue {
                    flag: "--layout",
                    expected: LAYOUTS.map(|l| l.as_str()).join("|"),
                    got,
                })?;
            }
            "--machine" => args.machine = Some(value("--machine", inline, &mut rest)?),
            "--cluster" => args.cluster = true,
            "--context" => args.context = Some(named("--context", inline, &mut rest)?),
            "--namespace" => args
                .namespaces
                .push(named("--namespace", inline, &mut rest)?),
            "--sync-timeout" => {
                args.sync_timeout_secs = bounded(
                    "--sync-timeout",
                    "seconds",
                    MAX_SYNC_TIMEOUT_SECS,
                    inline,
                    &mut rest,
                )?;
                args.sync_timeout_explicit = true;
            }
            "--list-contexts" => args.list_contexts = true,
            "--bench" => args.bench = true,
            "--startup-bench" => args.startup_bench = true,
            "--json" => args.json = true,
            "--help" | "-h" => args.help = true,
            other => args.ignored.push(other.to_string()),
        }
    }

    if args.help {
        return Ok(args);
    }
    if args.bench && args.startup_bench {
        return Err(ArgError::ConflictingBenchmarks);
    }
    if args.bench && args.cluster {
        return Err(ArgError::BenchWithCluster);
    }
    if args.json && !args.measuring() {
        return Err(ArgError::JsonWithoutBench);
    }
    if args.measuring() {
        match args.machine.as_deref() {
            None => return Err(ArgError::MissingMachineLabel),
            Some(label) if !is_usable_machine_label(label) => {
                return Err(ArgError::BadMachineLabel {
                    got: label.to_string(),
                });
            }
            Some(_) => {}
        }
    }
    Ok(args)
}

fn is_usable_machine_label(label: &str) -> bool {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed
        .chars()
        .all(|c| c.is_whitespace() || c.is_ascii_punctuation())
    {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    !PLACEHOLDER_MACHINES.contains(&lower.as_str())
}

fn value(
    flag: &'static str,
    inline: Option<&str>,
    rest: &mut impl Iterator<Item = String>,
) -> Result<String, ArgError> {
    match inline {
        Some(inline) => Ok(inline.to_string()),
        None => rest.next().ok_or(ArgError::MissingValue { flag }),
    }
}

// A name of something in a cluster. An empty one is what an unset shell
// variable expands to, and passing it on would probe a namespace that cannot
// exist or ask for a context nobody named -- an answer that looks like a
// cluster's rather than the command line's.
fn named(
    flag: &'static str,
    inline: Option<&str>,
    rest: &mut impl Iterator<Item = String>,
) -> Result<String, ArgError> {
    let got = value(flag, inline, rest)?;
    if got.trim().is_empty() {
        return Err(ArgError::BadValue {
            flag,
            expected: "a name".to_string(),
            got: format!("{got:?}"),
        });
    }
    Ok(got)
}

fn number<T: std::str::FromStr>(
    flag: &'static str,
    expected: &'static str,
    inline: Option<&str>,
    rest: &mut impl Iterator<Item = String>,
) -> Result<T, ArgError> {
    let got = value(flag, inline, rest)?;
    got.parse::<T>().map_err(|_| ArgError::BadValue {
        flag,
        expected: expected.to_string(),
        got,
    })
}

fn bounded(
    flag: &'static str,
    unit: &'static str,
    max: f32,
    inline: Option<&str>,
    rest: &mut impl Iterator<Item = String>,
) -> Result<f32, ArgError> {
    let got = value(flag, inline, rest)?;
    match got.parse::<f32>() {
        Ok(parsed) if (0.0..=max).contains(&parsed) => Ok(parsed),
        _ => Err(ArgError::BadValue {
            flag,
            expected: format!("{unit} from 0 to {max}"),
            got,
        }),
    }
}

#[cfg(test)]
#[path = "cli_test.rs"]
mod tests;
