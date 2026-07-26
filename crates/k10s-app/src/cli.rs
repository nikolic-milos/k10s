use k10s_clustergen::Scenario;
use k10s_world::LayoutMode;

pub const USAGE: &str = "\
usage: k10s [options]

cluster options:
  --cluster                               read a real cluster instead of the generator
  --context NAME                          kubeconfig context (default: current-context)
  --namespace NS                          namespace to probe RBAC in; repeatable.
                                          only needed where cluster-wide list is denied
  --sync-timeout SECS                     how long to wait for the initial list (default 30)
  --list-contexts                         print the kubeconfig's contexts and exit

generator options:
  --objects N                             objects to generate (default 25000)
  --seed S                                generator seed (default 55)
  --churn EVENTS_PER_SEC                  churn events per second (default 120)
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
    /// Read a real cluster rather than the generator.
    pub cluster: bool,
    /// Whether `--churn` was named, as opposed to left at its default. Cluster
    /// mode ignores churn either way, and warning about a default nobody asked for
    /// would print on every run.
    pub churn_explicit: bool,
    pub context: Option<String>,
    pub namespaces: Vec<String>,
    pub sync_timeout_secs: f32,
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
            context: None,
            namespaces: Vec::new(),
            sync_timeout_secs: DEFAULT_SYNC_TIMEOUT_SECS,
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

    /// The churn rate the world should actually run at.
    ///
    /// Synthetic churn flips pod states at random, which against a real cluster
    /// would invent health nobody reported. Cluster mode therefore has no churn
    /// whatever the flag says, and says so rather than silently ignoring it.
    pub fn effective_churn(&self) -> f32 {
        if self.cluster { 0.0 } else { self.churn }
    }

    /// Whether `--churn` was asked for and will be ignored.
    pub fn churn_was_overridden(&self) -> bool {
        self.cluster && self.churn_explicit && self.churn > 0.0
    }

    pub fn sync_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f32(self.sync_timeout_secs.max(0.0))
    }

    /// Flags that only mean anything with `--cluster`.
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
        expected: &'static str,
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
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--objects" => args.objects = number("--objects", "a u32", &mut rest)?,
            "--seed" => args.seed = number("--seed", "a u64", &mut rest)?,
            "--churn" => {
                args.churn = number("--churn", "an f32", &mut rest)?;
                args.churn_explicit = true;
            }
            "--scenario" => {
                let got = value("--scenario", &mut rest)?;
                args.scenario = match Scenario::parse(&got) {
                    Some(scenario) => scenario,
                    None => {
                        return Err(ArgError::BadValue {
                            flag: "--scenario",
                            expected: "platform|observability|data",
                            got,
                        });
                    }
                };
            }
            "--layout" => {
                let got = value("--layout", &mut rest)?;
                args.layout = match LayoutMode::parse(&got) {
                    Some(layout) => layout,
                    None => {
                        return Err(ArgError::BadValue {
                            flag: "--layout",
                            expected: "spread|dense",
                            got,
                        });
                    }
                };
            }
            "--machine" => args.machine = Some(value("--machine", &mut rest)?),
            "--cluster" => args.cluster = true,
            "--context" => args.context = Some(value("--context", &mut rest)?),
            "--namespace" => args.namespaces.push(value("--namespace", &mut rest)?),
            "--sync-timeout" => {
                args.sync_timeout_secs = number("--sync-timeout", "an f32", &mut rest)?;
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
        // The bench is a scripted flight over a fixed scene with committed
        // baselines; running it over whatever a cluster happens to hold would
        // produce a number nobody can compare against anything.
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

fn value(flag: &'static str, rest: &mut impl Iterator<Item = String>) -> Result<String, ArgError> {
    rest.next().ok_or(ArgError::MissingValue { flag })
}

fn number<T: std::str::FromStr>(
    flag: &'static str,
    expected: &'static str,
    rest: &mut impl Iterator<Item = String>,
) -> Result<T, ArgError> {
    let got = value(flag, rest)?;
    match got.parse::<T>() {
        Ok(parsed) => Ok(parsed),
        Err(_) => Err(ArgError::BadValue {
            flag,
            expected,
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
                    expected: "a u32",
                    got: "many".to_string(),
                },
            ),
            (
                &["--objects", "-1"],
                ArgError::BadValue {
                    flag: "--objects",
                    expected: "a u32",
                    got: "-1".to_string(),
                },
            ),
            (
                &["--seed", "1.5"],
                ArgError::BadValue {
                    flag: "--seed",
                    expected: "a u64",
                    got: "1.5".to_string(),
                },
            ),
            (
                &["--churn", "fast"],
                ArgError::BadValue {
                    flag: "--churn",
                    expected: "an f32",
                    got: "fast".to_string(),
                },
            ),
            (
                &["--scenario", "Platform"],
                ArgError::BadValue {
                    flag: "--scenario",
                    expected: "platform|observability|data",
                    got: "Platform".to_string(),
                },
            ),
            (
                &["--layout", "sparse"],
                ArgError::BadValue {
                    flag: "--layout",
                    expected: "spread|dense",
                    got: "sparse".to_string(),
                },
            ),
        ];
        for (argv, expected) in cases {
            assert_eq!(parse_argv(argv).as_ref(), Err(expected), "argv {argv:?}");
        }
    }

    #[test]
    fn repeated_namespaces_accumulate() {
        // The restricted-developer case: two namespaces, two rules reviews.
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
        // Flipping pod states at random against a real cluster would invent health
        // nobody reported, so the flag is overridden rather than honoured.
        let args = ok(&["--cluster", "--churn", "120"]);
        assert_eq!(args.effective_churn(), 0.0);
        assert!(args.churn_was_overridden());

        let generated = ok(&["--churn", "120"]);
        assert_eq!(generated.effective_churn(), 120.0);
        assert!(!generated.churn_was_overridden());

        // A default nobody named is not an override to report, or the warning
        // would print on every cluster run.
        assert!(!ok(&["--cluster"]).churn_was_overridden());
        assert_eq!(ok(&["--cluster"]).effective_churn(), 0.0);
        assert!(!ok(&["--cluster", "--churn", "0"]).churn_was_overridden());
    }

    #[test]
    fn cluster_only_flags_are_reported_when_there_is_no_cluster() {
        // Otherwise `--context prod` against the generator silently does nothing,
        // and the user believes they are looking at prod.
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
    }

    #[test]
    fn the_bench_refuses_a_cluster() {
        // Its baselines are for a fixed synthetic scene; a cluster's numbers are
        // not comparable to them, and a number nobody can compare is worse than no
        // number.
        assert_eq!(
            parse_argv(&["--bench", "--cluster"]),
            Err(ArgError::BenchWithCluster)
        );
        assert!(ArgError::BenchWithCluster.to_string().contains("generator"));
    }

    #[test]
    fn a_sync_timeout_is_a_duration_and_never_negative() {
        assert_eq!(
            ok(&["--sync-timeout", "2.5"]).sync_timeout().as_millis(),
            2500
        );
        assert_eq!(ok(&["--sync-timeout", "-1"]).sync_timeout().as_millis(), 0);
        assert_eq!(Args::default().sync_timeout().as_secs(), 30);
    }

    #[test]
    fn missing_values_are_errors() {
        let cases: &[(&[&str], &'static str)] = &[
            (&["--objects"], "--objects"),
            (&["--seed"], "--seed"),
            (&["--churn"], "--churn"),
            (&["--scenario"], "--scenario"),
            (&["--layout"], "--layout"),
            (&["--machine"], "--machine"),
            (&["--context"], "--context"),
            (&["--namespace"], "--namespace"),
            (&["--sync-timeout"], "--sync-timeout"),
        ];
        for &(argv, flag) in cases {
            assert_eq!(
                parse_argv(argv),
                Err(ArgError::MissingValue { flag }),
                "argv {argv:?}"
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
        ];
        for &argv in cases {
            let args = ok(argv);
            assert_eq!(args.ignored, vec![argv[0].to_string()], "argv {argv:?}");
        }
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
    fn platform_records_arch_and_os() {
        let platform = platform();
        assert!(platform.contains(std::env::consts::ARCH));
        assert!(platform.contains(std::env::consts::OS));
    }
}
