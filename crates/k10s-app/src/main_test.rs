//! The process contract: which of the four failures a run reports, and which
//! one wins when more than one is true. The codes are what automation reads --
//! a recording that did not happen is not a window that would not open -- so
//! the ordering between them is pinned here rather than left to the shape of
//! an `if` chain.

use super::*;

fn clean() -> Ending {
    Ending {
        bench_failed: false,
        startup: None,
        world_ended_cleanly: true,
        window_failed: false,
        connection_failed: false,
    }
}

fn startup(failed: bool, completed: bool) -> Option<StartupEnding> {
    Some(StartupEnding { failed, completed })
}

#[test]
fn a_run_that_did_everything_it_was_asked_exits_zero() {
    assert_eq!(clean().code(), 0);
    assert_eq!(
        Ending {
            startup: startup(false, true),
            ..clean()
        }
        .code(),
        0,
        "a startup benchmark that wrote its report is a successful run"
    );
    assert!(!clean().startup_ended_before_a_useful_frame());
}

#[test]
fn each_failure_keeps_its_own_code() {
    assert_eq!(
        Ending {
            bench_failed: true,
            ..clean()
        }
        .code(),
        3
    );
    assert_eq!(
        Ending {
            startup: startup(true, false),
            ..clean()
        }
        .code(),
        4
    );
    assert_eq!(
        Ending {
            startup: startup(false, false),
            ..clean()
        }
        .code(),
        4,
        "a measurement that never reached a useful frame is not a completed one"
    );
    assert_eq!(
        Ending {
            world_ended_cleanly: false,
            ..clean()
        }
        .code(),
        1
    );
    assert_eq!(
        Ending {
            window_failed: true,
            ..clean()
        }
        .code(),
        1
    );
    assert_eq!(
        Ending {
            connection_failed: true,
            ..clean()
        }
        .code(),
        1
    );
}

#[test]
fn the_more_specific_failure_wins_when_several_are_true() {
    // A flight that gave up says so even though the world it was flying in
    // also went down with it, and a startup measurement outranks the general
    // failure for the same reason: the specific answer is the useful one.
    assert_eq!(
        Ending {
            bench_failed: true,
            startup: startup(true, false),
            world_ended_cleanly: false,
            window_failed: true,
            connection_failed: true,
        }
        .code(),
        3
    );
    assert_eq!(
        Ending {
            startup: startup(true, false),
            world_ended_cleanly: false,
            window_failed: true,
            ..clean()
        }
        .code(),
        4
    );
}

#[test]
fn only_a_measurement_that_did_not_fail_needs_the_sentence() {
    assert!(
        Ending {
            startup: startup(false, false),
            ..clean()
        }
        .startup_ended_before_a_useful_frame()
    );
    assert!(
        !Ending {
            startup: startup(true, false),
            ..clean()
        }
        .startup_ended_before_a_useful_frame(),
        "a measurement that failed already printed why"
    );
    assert!(
        !Ending {
            startup: startup(false, true),
            ..clean()
        }
        .startup_ended_before_a_useful_frame()
    );
}
