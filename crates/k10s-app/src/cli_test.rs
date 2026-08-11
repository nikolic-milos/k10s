//! Every flag, its bounds, and the combinations that are refused: a value that
//! parses but cannot be used is an error, an unknown argument is ignored
//! without stopping the flags after it, and `--help` wins over validation.

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
        (&["--bench", "--machine", "m2-air"], |a| a.bench),
        (&["--startup-bench", "--machine", "m2-air"], |a| {
            a.startup_bench
        }),
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
fn a_bare_command_line_is_the_only_one_the_launch_screen_answers_for() {
    assert!(!ok(&[]).scene_was_named());
    assert!(!ok(&["--layout", "spread"]).scene_was_named());
    for argv in [
        vec!["--cluster"],
        vec!["--cluster", "--context", "prod"],
        vec!["--bench", "--machine", "ci"],
        vec!["--objects", "1000"],
        vec!["--seed", "7"],
        vec!["--scenario", "data"],
        vec!["--churn", "0"],
    ] {
        assert!(
            ok(&argv).scene_was_named(),
            "{argv:?} already says what to show"
        );
    }
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
    assert!(ok(&["--startup-bench", "--json", "--machine", "m"]).json);
}

#[test]
fn bench_without_machine_is_an_error() {
    assert_eq!(parse_argv(&["--bench"]), Err(ArgError::MissingMachineLabel));
    assert_eq!(
        parse_argv(&["--bench", "--json"]),
        Err(ArgError::MissingMachineLabel)
    );
    assert!(
        ArgError::MissingMachineLabel
            .to_string()
            .contains("--machine")
    );
    assert_eq!(
        parse_argv(&["--startup-bench"]),
        Err(ArgError::MissingMachineLabel)
    );
}

#[test]
fn benchmark_modes_are_one_process_each() {
    assert_eq!(
        parse_argv(&[
            "--bench",
            "--startup-bench",
            "--machine",
            "linux-x86_64-i5-12600k"
        ]),
        Err(ArgError::ConflictingBenchmarks)
    );
}

#[test]
fn placeholder_machine_labels_are_rejected_with_bench() {
    let placeholders: &[&str] = &[
        "my-box",
        "MY-BOX",
        " unlabeled ",
        "TODO",
        "todo",
        "test",
        "placeholder",
        "",
        "   ",
        "---",
        "...",
        "!!!",
    ];
    for &label in placeholders {
        let argv = ["--bench", "--machine", label];
        assert!(
            matches!(parse_argv(&argv), Err(ArgError::BadMachineLabel { .. })),
            "placeholder {label:?} was accepted"
        );
    }
    let err = parse_argv(&["--bench", "--machine", "my-box"]).unwrap_err();
    assert!(
        err.to_string().contains("hardware-labelled"),
        "bad label message should guide the user: {err}"
    );
}

#[test]
fn usable_machine_labels_are_accepted_with_bench() {
    for label in [
        "m2-air",
        "ci-runner",
        "macos-aarch64-macbook-pro-m1",
        "linux-x86_64-i5-12600k",
        "m",
    ] {
        let args = ok(&["--bench", "--machine", label]);
        assert_eq!(args.machine.as_deref(), Some(label));
    }
}

#[test]
fn objects_above_the_cap_are_rejected() {
    assert_eq!(ok(&["--objects", "1000000"]).objects, 1_000_000);
    assert!(matches!(
        parse_argv(&["--objects", "1000001"]),
        Err(ArgError::BadValue {
            flag: "--objects",
            ..
        })
    ));
}

#[test]
fn machine_label_carries_the_validated_label() {
    assert_eq!(
        ok(&["--bench", "--machine", "m4-max"]).machine_label(),
        "m4-max"
    );
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
        "--startup-bench",
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
