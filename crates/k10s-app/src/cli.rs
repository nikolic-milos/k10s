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
  --objects N                             objects to generate (default 25000)
  --seed S                                generator seed (default 55)
  --churn EVENTS_PER_SEC                  churn events per second (default 120, max 100000)
  --scenario NAME                         platform|observability|data|ns-fanout|wl-fanout
                                          (default platform)

shared options:
  --layout spread|dense                   layout mode (default spread)
  --machine LABEL                         machine label recorded in bench reports
  --bench                                 run the scripted flight bench
  --json                                  bench report as JSON on stdout (needs --bench and --machine)
  -h, --help                              print this message

unrecognized arguments are reported on stderr and ignored";

const DEFAULT_MACHINE: &str = "unlabeled";
const DEFAULT_SYNC_TIMEOUT_SECS: f32 = 30.0;
const MAX_SYNC_TIMEOUT_SECS: f32 = 3600.0;
const MAX_CHURN_PER_SEC: f32 = 100_000.0;

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
            .as_deref()
            .unwrap_or(DEFAULT_MACHINE)
            .to_string()
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
    BenchWithCluster,
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
            ArgError::JsonWithoutBench => write!(f, "--json requires --bench"),
            ArgError::MissingMachineLabel => {
                write!(f, "--bench --json requires --machine LABEL")
            }
            ArgError::BenchWithCluster => write!(
                f,
                "--bench needs the generator: its baselines are for a fixed scene, \
                 so a cluster's numbers cannot be compared against them"
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
                args.objects = number("--objects", "a u32", inline, &mut rest)?;
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
            "--context" => args.context = Some(value("--context", inline, &mut rest)?),
            "--namespace" => args
                .namespaces
                .push(value("--namespace", inline, &mut rest)?),
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
            "--json" => args.json = true,
            "--help" | "-h" => args.help = true,
            other => args.ignored.push(other.to_string()),
        }
    }

    if args.help {
        return Ok(args);
    }
    if args.bench && args.cluster {
        return Err(ArgError::BenchWithCluster);
    }
    if args.json && !args.bench {
        return Err(ArgError::JsonWithoutBench);
    }
    if args.bench && args.json && args.machine.is_none() {
        return Err(ArgError::MissingMachineLabel);
    }
    Ok(args)
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
mod tests {
    use super::*;

    fn parse_argv(argv: &[&str]) -> Result<Args, ArgError> {
        parse(argv.iter().map(|arg| (*arg).to_string()))
    }

    fn ok(argv: &[&str]) -> Args {
        parse_argv(argv).expect("argv should parse")
    }

    type FieldCase = (&'static [&'static str], fn(&Args) -> bool);

    #[test]
    fn no_arguments_yields_defaults() {
        assert_eq!(ok(&[]), Args::default());
    }

    #[test]
    fn every_flag_sets_its_field() {
        let cases: &[FieldCase] = &[
            (&["--objects", "1000"], |a| a.objects == 1000),
            (&["--seed", "7"], |a| a.seed == 7),
            (&["--churn", "12.5"], |a| {
                (a.churn - 12.5).abs() < f32::EPSILON
            }),
            (&["--scenario", "platform"], |a| {
                a.scenario == Scenario::Platform
            }),
            (&["--scenario", "observability"], |a| {
                a.scenario == Scenario::Observability
            }),
            (&["--scenario", "data"], |a| a.scenario == Scenario::Data),
            (&["--scenario", "ns-fanout"], |a| {
                a.scenario == Scenario::NsFanOut
            }),
            (&["--scenario", "wl-fanout"], |a| {
                a.scenario == Scenario::WlFanOut
            }),
            (&["--layout", "spread"], |a| a.layout == LayoutMode::Spread),
            (&["--layout", "dense"], |a| a.layout == LayoutMode::Dense),
            (&["--machine", "m2-air"], |a| {
                a.machine.as_deref() == Some("m2-air")
            }),
            (&["--bench"], |a| a.bench),
            (&["--bench", "--json", "--machine", "m"], |a| a.json),
            (&["--help"], |a| a.help),
            (&["-h"], |a| a.help),
            (&["--cluster"], |a| a.cluster),
            (&["--context", "prod"], |a| {
                a.context.as_deref() == Some("prod")
            }),
            (&["--namespace", "payments"], |a| {
                a.namespaces == vec!["payments".to_string()]
            }),
            (&["--sync-timeout", "5"], |a| {
                (a.sync_timeout_secs - 5.0).abs() < f32::EPSILON
            }),
            (&["--list-contexts"], |a| a.list_contexts),
        ];
        for &(argv, holds) in cases {
            let args = ok(argv);
            assert!(holds(&args), "{argv:?} did not set its field: {args:?}");
        }
    }

    #[test]
    fn flags_combine_and_leave_other_defaults_alone() {
        let args = ok(&[
            "--objects",
            "9",
            "--seed",
            "3",
            "--churn",
            "0",
            "--scenario",
            "data",
            "--layout",
            "dense",
            "--machine",
            "ci-runner",
            "--bench",
            "--json",
        ]);
        assert_eq!(args.objects, 9);
        assert_eq!(args.seed, 3);
        assert_eq!(args.churn, 0.0);
        assert_eq!(args.scenario, Scenario::Data);
        assert_eq!(args.layout, LayoutMode::Dense);
        assert_eq!(args.machine.as_deref(), Some("ci-runner"));
        assert!(args.bench);
        assert!(args.json);
        assert!(args.ignored.is_empty());
    }

    #[test]
    fn bad_values_are_errors_not_panics() {
        let cases: &[(&[&str], ArgError)] = &[
            (
                &["--objects", "many"],
                ArgError::BadValue {
                    flag: "--objects",
                    expected: "a u32".to_string(),
                    got: "many".to_string(),
                },
            ),
            (
                &["--objects", "-1"],
                ArgError::BadValue {
                    flag: "--objects",
                    expected: "a u32".to_string(),
                    got: "-1".to_string(),
                },
            ),
            (
                &["--seed", "1.5"],
                ArgError::BadValue {
                    flag: "--seed",
                    expected: "a u64".to_string(),
                    got: "1.5".to_string(),
                },
            ),
            (
                &["--churn", "fast"],
                ArgError::BadValue {
                    flag: "--churn",
                    expected: "a rate from 0 to 100000".to_string(),
                    got: "fast".to_string(),
                },
            ),
            (
                &["--scenario", "Platform"],
                ArgError::BadValue {
                    flag: "--scenario",
                    expected: "platform|observability|data|ns-fanout|wl-fanout".to_string(),
                    got: "Platform".to_string(),
                },
            ),
            (
                &["--layout", "sparse"],
                ArgError::BadValue {
                    flag: "--layout",
                    expected: "spread|dense".to_string(),
                    got: "sparse".to_string(),
                },
            ),
            (
                &["--sync-timeout", "1e30"],
                ArgError::BadValue {
                    flag: "--sync-timeout",
                    expected: "seconds from 0 to 3600".to_string(),
                    got: "1e30".to_string(),
                },
            ),
        ];
        for (argv, expected) in cases {
            assert_eq!(parse_argv(argv).as_ref(), Err(expected), "argv {argv:?}");
        }
    }

    #[test]
    fn numbers_that_parse_but_cannot_be_used_are_errors() {
        let cases: &[&[&str]] = &[
            &["--sync-timeout", "inf"],
            &["--sync-timeout", "1e30"],
            &["--sync-timeout", "-1"],
            &["--sync-timeout", "nan"],
            &["--sync-timeout", "3601"],
            &["--churn", "inf"],
            &["--churn", "-1"],
            &["--churn", "-inf"],
            &["--churn", "1e30"],
        ];
        for &argv in cases {
            let parsed = parse_argv(argv);
            assert!(
                matches!(parsed, Err(ArgError::BadValue { .. })),
                "argv {argv:?} was accepted: {parsed:?}"
            );
        }
        assert_eq!(
            ok(&["--sync-timeout", "3600"]).sync_timeout().as_secs(),
            3600
        );
        assert_eq!(ok(&["--churn", "0"]).churn, 0.0);
        assert_eq!(ok(&["--churn", "100000"]).churn, 100_000.0);
    }

    #[test]
    fn the_eq_form_works_for_every_flag_that_takes_a_value() {
        let cases: &[FieldCase] = &[
            (&["--objects=50000"], |a| a.objects == 50_000),
            (&["--seed=7"], |a| a.seed == 7),
            (&["--churn=12.5"], |a| (a.churn - 12.5).abs() < f32::EPSILON),
            (&["--scenario=ns-fanout"], |a| {
                a.scenario == Scenario::NsFanOut
            }),
            (&["--layout=dense"], |a| a.layout == LayoutMode::Dense),
            (&["--machine=m2-air"], |a| {
                a.machine.as_deref() == Some("m2-air")
            }),
            (&["--context=prod"], |a| {
                a.context.as_deref() == Some("prod")
            }),
            (&["--namespace=payments"], |a| {
                a.namespaces == vec!["payments".to_string()]
            }),
            (&["--sync-timeout=5"], |a| {
                (a.sync_timeout_secs - 5.0).abs() < f32::EPSILON
            }),
        ];
        for &(argv, holds) in cases {
            let args = ok(argv);
            assert!(holds(&args), "{argv:?} did not set its field: {args:?}");
            assert!(args.ignored.is_empty(), "{argv:?} was reported as ignored");
        }
        assert!(parse_argv(&["--sync-timeout=-1"]).is_err());
    }

    #[test]
    fn repeated_namespaces_accumulate() {
        let args = ok(&[
            "--cluster",
            "--namespace",
            "team-a",
            "--namespace",
            "team-b",
        ]);
        assert_eq!(args.namespaces, vec!["team-a", "team-b"]);
    }

    #[test]
    fn cluster_mode_has_no_synthetic_churn() {
        let args = ok(&["--cluster", "--churn", "120"]);
        assert_eq!(args.effective_churn(), 0.0);
        assert!(args.churn_was_overridden());

        let generated = ok(&["--churn", "120"]);
        assert_eq!(generated.effective_churn(), 120.0);
        assert!(!generated.churn_was_overridden());

        assert!(!ok(&["--cluster"]).churn_was_overridden());
        assert_eq!(ok(&["--cluster"]).effective_churn(), 0.0);
        assert!(!ok(&["--cluster", "--churn", "0"]).churn_was_overridden());
    }

    #[test]
    fn cluster_only_flags_are_reported_when_there_is_no_cluster() {
        assert_eq!(
            ok(&["--context", "prod"]).cluster_flags_without_cluster(),
            vec!["--context"]
        );
        assert_eq!(
            ok(&["--namespace", "a", "--context", "p"]).cluster_flags_without_cluster(),
            vec!["--context", "--namespace"]
        );
        assert!(
            ok(&["--cluster", "--context", "prod"])
                .cluster_flags_without_cluster()
                .is_empty()
        );
        assert!(
            ok(&["--list-contexts", "--context", "prod"])
                .cluster_flags_without_cluster()
                .is_empty()
        );
        assert_eq!(
            ok(&["--sync-timeout", "5"]).cluster_flags_without_cluster(),
            vec!["--sync-timeout"]
        );
        assert!(
            ok(&["--cluster", "--sync-timeout", "5"])
                .cluster_flags_without_cluster()
                .is_empty()
        );
        assert!(ok(&[]).cluster_flags_without_cluster().is_empty());
    }

    #[test]
    fn generator_only_flags_are_reported_when_there_is_a_cluster() {
        assert_eq!(
            ok(&["--cluster", "--objects", "50000"]).generator_flags_with_cluster(),
            vec!["--objects"]
        );
        assert_eq!(
            ok(&[
                "--cluster",
                "--scenario",
                "data",
                "--seed",
                "9",
                "--objects",
                "1"
            ])
            .generator_flags_with_cluster(),
            vec!["--objects", "--seed", "--scenario"]
        );
        assert!(ok(&["--cluster"]).generator_flags_with_cluster().is_empty());
        assert!(
            ok(&["--objects", "50000"])
                .generator_flags_with_cluster()
                .is_empty()
        );
        assert!(
            ok(&["--cluster", "--churn", "5"])
                .generator_flags_with_cluster()
                .is_empty()
        );
    }

    #[test]
    fn the_bench_refuses_a_cluster() {
        assert_eq!(
            parse_argv(&["--bench", "--cluster"]),
            Err(ArgError::BenchWithCluster)
        );
        assert!(ArgError::BenchWithCluster.to_string().contains("generator"));
    }

    #[test]
    fn a_sync_timeout_is_a_duration_it_was_asked_for() {
        assert_eq!(
            ok(&["--sync-timeout", "2.5"]).sync_timeout().as_millis(),
            2500
        );
        assert_eq!(Args::default().sync_timeout().as_secs(), 30);
    }

    #[test]
    fn missing_values_are_errors() {
        for flag in VALUE_FLAGS {
            assert_eq!(
                parse_argv(&[flag]),
                Err(ArgError::MissingValue { flag }),
                "argv {flag:?}"
            );
        }
    }

    #[test]
    fn unknown_arguments_are_ignored_not_fatal() {
        let cases: &[&[&str]] = &[
            &["--bogus-flag"],
            &["-psn_0_774050"],
            &["/home/user/pod.yaml"],
            &["--gapplication-service"],
            &["--gapplication-app-id=org.k10s"],
            &["--cluster=no"],
            &["--bogus-flag=1"],
        ];
        for &argv in cases {
            let args = ok(argv);
            assert_eq!(args.ignored, vec![argv[0].to_string()], "argv {argv:?}");
        }
        assert!(!ok(&["--cluster=no"]).cluster);
    }

    #[test]
    fn unknown_arguments_do_not_stop_later_flags() {
        let args = ok(&["--bogus-flag", "--objects", "1000", "extra"]);
        assert_eq!(args.objects, 1000);
        assert_eq!(
            args.ignored,
            vec!["--bogus-flag".to_string(), "extra".to_string()]
        );
    }

    #[test]
    fn json_without_bench_is_an_error() {
        assert_eq!(parse_argv(&["--json"]), Err(ArgError::JsonWithoutBench));
    }

    #[test]
    fn json_bench_without_machine_is_an_error() {
        assert_eq!(
            parse_argv(&["--bench", "--json"]),
            Err(ArgError::MissingMachineLabel)
        );
    }

    #[test]
    fn bench_without_json_needs_no_machine() {
        let args = ok(&["--bench"]);
        assert!(args.bench);
        assert_eq!(args.machine_label(), DEFAULT_MACHINE);
    }

    #[test]
    fn machine_label_prefers_the_explicit_label() {
        assert_eq!(ok(&["--machine", "m4-max"]).machine_label(), "m4-max");
    }

    #[test]
    fn help_wins_over_validation() {
        for argv in [&["--help", "--json"][..], &["--json", "-h"][..]] {
            let args = ok(argv);
            assert!(args.help, "argv {argv:?}");
        }
    }

    #[test]
    fn usage_lists_every_flag() {
        for flag in [
            "--objects",
            "--seed",
            "--churn",
            "--scenario",
            "--layout",
            "--machine",
            "--bench",
            "--json",
            "--help",
            "--cluster",
            "--context",
            "--namespace",
            "--sync-timeout",
            "--list-contexts",
        ] {
            assert!(USAGE.contains(flag), "usage is missing {flag}");
        }
    }

    #[test]
    fn usage_names_the_numeric_bounds_it_enforces() {
        for max in [MAX_SYNC_TIMEOUT_SECS, MAX_CHURN_PER_SEC] {
            assert!(USAGE.contains(&max.to_string()), "usage is missing {max}");
        }
    }

    #[test]
    fn usage_lists_every_scenario_and_layout() {
        for scenario in SCENARIOS.map(|s| s.as_str()) {
            assert!(USAGE.contains(scenario), "usage is missing {scenario}");
        }
        for layout in LAYOUTS.map(|l| l.as_str()) {
            assert!(USAGE.contains(layout), "usage is missing {layout}");
        }
    }

    #[test]
    fn platform_records_arch_and_os() {
        let platform = platform();
        assert!(platform.contains(std::env::consts::ARCH));
        assert!(platform.contains(std::env::consts::OS));
    }
}
