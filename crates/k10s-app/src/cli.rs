use k10s_clustergen::Scenario;
use k10s_world::LayoutMode;

pub const USAGE: &str = "\
usage: k10s [options]

options:
  --objects N                             objects to generate (default 25000)
  --seed S                                generator seed (default 55)
  --churn EVENTS_PER_SEC                  churn events per second (default 120)
  --scenario platform|observability|data  cluster shape (default platform)
  --layout spread|dense                   layout mode (default spread)
  --machine LABEL                         machine label recorded in bench reports
  --bench                                 run the scripted flight bench
  --json                                  bench report as JSON on stdout (needs --bench and --machine)
  -h, --help                              print this message

unrecognized arguments are reported on stderr and ignored";

const DEFAULT_MACHINE: &str = "unlabeled";

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
            "--churn" => args.churn = number("--churn", "an f32", &mut rest)?,
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
            "--bench" => args.bench = true,
            "--json" => args.json = true,
            "--help" | "-h" => args.help = true,
            other => args.ignored.push(other.to_string()),
        }
    }

    if args.help {
        return Ok(args);
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
    fn missing_values_are_errors() {
        let cases: &[(&[&str], &'static str)] = &[
            (&["--objects"], "--objects"),
            (&["--seed"], "--seed"),
            (&["--churn"], "--churn"),
            (&["--scenario"], "--scenario"),
            (&["--layout"], "--layout"),
            (&["--machine"], "--machine"),
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
