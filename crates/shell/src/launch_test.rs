//! The launch screen as a state machine driven headlessly: every state it can
//! reach has a choice highlighted, navigation steps over headers and holds at
//! both ends, a click reaches exactly what the keyboard reaches, and an
//! unreadable kubeconfig is a labelled state that leaves the screen usable.

use super::*;

fn context(name: &str, current: bool) -> ContextRow {
    ContextRow {
        name: name.to_string(),
        current,
        server: None,
        namespace: None,
    }
}

fn source(label: &str, contexts: Vec<ContextRow>) -> ConfigSource {
    ConfigSource {
        label: label.to_string(),
        contexts,
        implicit: false,
        note: None,
    }
}

fn detected(sources: Vec<ConfigSource>) -> LaunchState {
    let mut state = LaunchState::new();
    state.scanned(&ScanRequest::Detected, ScanOutcome::Sources(sources));
    state
}

fn labels(state: &LaunchState) -> Vec<String> {
    state
        .rows
        .iter()
        .map(|row| match row {
            Row::Source(label) => format!("[{label}]"),
            Row::Note(note) => format!("({note})"),
            Row::Choice(Choice::Context { label, .. }) => label.clone(),
            Row::Choice(Choice::OpenKubeconfig) => "open".to_string(),
            Row::Choice(Choice::Demo) => "demo".to_string(),
        })
        .collect()
}

#[test]
fn the_two_ways_out_are_offered_before_any_scan_has_answered() {
    let state = LaunchState::new();
    assert_eq!(state.status, Status::Scanning);
    assert_eq!(
        labels(&state),
        vec!["(Looking for a kubeconfig…)", "open", "demo"]
    );
    assert_eq!(
        state.selected_choice(),
        Some(&Choice::OpenKubeconfig),
        "the highlight lands on a choice, never on the note"
    );
}

#[test]
fn a_scan_lists_every_source_under_its_own_header_and_marks_current_context() {
    let state = detected(vec![
        source(
            "~/.kube/config",
            vec![context("prod-eu-west", true), context("staging", false)],
        ),
        source("/etc/k8s/edge.yaml", vec![context("edge", false)]),
    ]);
    assert_eq!(state.status, Status::Ready);
    assert_eq!(
        labels(&state),
        vec![
            "[~/.kube/config]",
            "prod-eu-west",
            "staging",
            "[/etc/k8s/edge.yaml]",
            "edge",
            "open",
            "demo",
        ]
    );
    let Some(Choice::Context {
        request, current, ..
    }) = state.choice_at(1)
    else {
        panic!("{:?}", state.rows)
    };
    assert!(current, "current-context is a property of the row it marks");
    assert_eq!(request.context.as_deref(), Some("prod-eu-west"));
    assert_eq!(
        request.source,
        ScanRequest::Detected,
        "the row carries where it was found, so connecting cannot guess"
    );
}

#[test]
fn a_source_that_declares_no_contexts_says_so_instead_of_vanishing() {
    let state = detected(vec![source("~/.kube/empty", Vec::new())]);
    assert_eq!(
        labels(&state),
        vec!["[~/.kube/empty]", "(declares no contexts)", "open", "demo",]
    );
    assert!(
        state.selected_choice().is_some(),
        "a screen with no contexts still has something highlighted"
    );

    // An in-cluster account declares none and is connectable anyway, which
    // is the difference `implicit` exists to carry.
    let mut in_cluster = LaunchState::new();
    in_cluster.scanned(
        &ScanRequest::Detected,
        ScanOutcome::Sources(vec![ConfigSource {
            label: "in-cluster service account".to_string(),
            contexts: Vec::new(),
            implicit: true,
            note: None,
        }]),
    );
    let Some(Choice::Context { request, .. }) = in_cluster.choice_at(1) else {
        panic!("{:?}", in_cluster.rows)
    };
    assert_eq!(
        request.context, None,
        "an implicit account is connected with no context named"
    );

    // And a source that would not read keeps its place with the reason on
    // it: among several kubeconfigs, one bad file must not shorten the list
    // to something the user cannot tell is short.
    let mut broken = LaunchState::new();
    broken.scanned(
        &ScanRequest::Detected,
        ScanOutcome::Sources(vec![
            ConfigSource {
                label: "/etc/k8s/bad.yaml".to_string(),
                contexts: Vec::new(),
                implicit: false,
                note: Some("/etc/k8s/bad.yaml: invalid type: integer".to_string()),
            },
            source("~/.kube/config", vec![context("prod", true)]),
        ]),
    );
    assert_eq!(
        labels(&broken),
        vec![
            "[/etc/k8s/bad.yaml]",
            "(/etc/k8s/bad.yaml: invalid type: integer)",
            "[~/.kube/config]",
            "prod",
            "open",
            "demo",
        ]
    );
    assert_eq!(
        broken.selected, 3,
        "the highlight still lands on the current context, past the failure"
    );
}

#[test]
fn an_unreadable_kubeconfig_is_a_labelled_state_that_leaves_the_screen_usable() {
    let mut state = LaunchState::new();
    state.scanned(
        &ScanRequest::Detected,
        ScanOutcome::Failed("failed to parse kubeconfig YAML at line 8, column 11".to_string()),
    );
    assert!(matches!(state.status, Status::Unreadable(_)));
    assert_eq!(
        labels(&state),
        vec![
            "(failed to parse kubeconfig YAML at line 8, column 11)",
            "open",
            "demo",
        ]
    );
    assert_eq!(
        state.footer(),
        None,
        "the reason is standing in for the rows; saying it twice reads as two \
             different problems"
    );
    state.select_next();
    assert_eq!(
        state.confirm(),
        Some(Choice::Demo),
        "the generated starmap is reachable from every failure state"
    );

    // A file the user then opens lands beside the failure rather than
    // replacing the reason it happened.
    let mut state = detected(vec![source("~/.kube/config", vec![context("prod", true)])]);
    state.scanned(
        &ScanRequest::File("/tmp/broken.yaml".into()),
        ScanOutcome::Failed("no such file".to_string()),
    );
    assert_eq!(
        labels(&state),
        vec!["[~/.kube/config]", "prod", "open", "demo"]
    );
    assert_eq!(state.status, Status::Unreadable("no such file".to_string()));
    assert_eq!(
        state.footer().as_deref(),
        Some("no such file"),
        "and with rows to stand in front of, the reason has to be said under \
             them: a file that would not open must never look like one that did"
    );
}

#[test]
fn navigation_steps_over_headers_and_notes_and_holds_at_both_ends() {
    let mut state = detected(vec![
        source("a", vec![context("one", false)]),
        source("b", vec![context("two", false)]),
    ]);
    assert_eq!(state.selected, 1);
    for expected in [3, 4, 5] {
        state.select_next();
        assert_eq!(state.selected, expected, "{:?}", labels(&state));
    }
    state.select_next();
    assert_eq!(state.selected, 5, "the last row holds rather than wrapping");
    for expected in [4, 3, 1] {
        state.select_previous();
        assert_eq!(state.selected, expected);
    }
    state.select_previous();
    assert_eq!(
        state.selected, 1,
        "the first choice holds, the header is not selectable"
    );
}

#[test]
fn a_click_reaches_exactly_what_the_keyboard_reaches() {
    let mut state = detected(vec![source("~/.kube/config", vec![context("prod", true)])]);
    assert!(
        !state.select(0),
        "a header is not selectable by mouse either"
    );
    assert!(state.select(1));
    assert_eq!(state.selected, 1);
    assert!(!state.select(99));
}

#[test]
fn filtering_hides_contexts_and_their_empty_headers_but_never_the_way_out() {
    let mut state = detected(vec![
        source(
            "~/.kube/config",
            vec![context("prod-eu-west", true), context("staging", false)],
        ),
        source("/etc/k8s/edge.yaml", vec![context("edge", false)]),
    ]);
    state.set_query("prod".to_string());
    assert_eq!(
        labels(&state),
        vec!["[~/.kube/config]", "prod-eu-west", "open", "demo"],
        "a header whose contexts all filtered out goes with them"
    );

    state.set_query("zzz".to_string());
    assert_eq!(
        labels(&state),
        vec!["open", "demo"],
        "a query matching nothing still leaves both ways out"
    );
    assert!(state.selected_choice().is_some());

    state.delete_char();
    state.delete_char();
    state.delete_char();
    assert_eq!(state.query, "");
    assert_eq!(labels(&state).len(), 7, "clearing the filter restores them");
}

#[test]
fn every_line_under_the_list_is_the_state_saying_what_it_is_doing() {
    let mut state = LaunchState::new();
    assert_eq!(state.footer().as_deref(), Some("Looking for a kubeconfig…"));

    state.scanned(
        &ScanRequest::Detected,
        ScanOutcome::Sources(vec![source("~/.kube/config", vec![context("prod", true)])]),
    );
    assert_eq!(state.footer(), None, "a list that is ready says nothing");

    state.confirm();
    assert_eq!(state.footer().as_deref(), Some("Connecting to prod…"));
    state.refused("connection refused".to_string());
    assert_eq!(state.footer().as_deref(), Some("connection refused"));

    state.select_next();
    state.select_next();
    assert_eq!(state.confirm(), Some(Choice::Demo));
    assert_eq!(state.footer().as_deref(), Some("Generating a starmap…"));
}

#[test]
fn confirming_a_context_reports_the_request_and_marks_the_attempt_in_flight() {
    let mut state = detected(vec![source("~/.kube/config", vec![context("prod", true)])]);
    let choice = state.confirm().expect("a context is highlighted");
    let Choice::Context { request, .. } = &choice else {
        panic!("{choice:?}")
    };
    assert_eq!(request.context.as_deref(), Some("prod"));
    assert_eq!(state.status, Status::Connecting("prod".to_string()));
    assert_eq!(
        state.confirm(),
        None,
        "a second enter must not open a second connection to the same question"
    );

    state.refused("connection refused".to_string());
    assert_eq!(
        state.status,
        Status::Refused("connection refused".to_string())
    );
    assert_eq!(
        state.selected, 1,
        "a refusal leaves the highlight where it was so the next row is one keystroke away"
    );
    assert!(
        state.confirm().is_some(),
        "and the screen is usable again immediately"
    );
}

#[test]
fn asking_for_a_kubeconfig_starts_nothing_and_leaves_enter_working() {
    let mut state = detected(vec![source("~/.kube/empty", Vec::new())]);
    assert_eq!(state.confirm(), Some(Choice::OpenKubeconfig));
    assert_eq!(
        state.status,
        Status::Ready,
        "a picker is not an attempt at a cluster"
    );
    assert_eq!(state.confirm(), Some(Choice::OpenKubeconfig));
}

#[test]
fn a_rescan_replaces_its_own_source_and_keeps_the_highlight_on_what_it_named() {
    let mut state = detected(vec![source("~/.kube/config", vec![context("prod", true)])]);
    state.scanned(
        &ScanRequest::File("/tmp/edge.yaml".into()),
        ScanOutcome::Sources(vec![source("/tmp/edge.yaml", vec![context("edge", false)])]),
    );
    assert_eq!(
        labels(&state),
        vec![
            "[~/.kube/config]",
            "prod",
            "[/tmp/edge.yaml]",
            "edge",
            "open",
            "demo",
        ]
    );
    state.select(3);

    state.scanned(
        &ScanRequest::File("/tmp/edge.yaml".into()),
        ScanOutcome::Sources(vec![source(
            "/tmp/edge.yaml",
            vec![context("edge", false), context("edge-canary", false)],
        )]),
    );
    assert_eq!(
        labels(&state),
        vec![
            "[~/.kube/config]",
            "prod",
            "[/tmp/edge.yaml]",
            "edge",
            "edge-canary",
            "open",
            "demo",
        ],
        "scanning the same file twice replaces it rather than listing it twice"
    );
    assert_eq!(
        state.selected_choice().and_then(|choice| match choice {
            Choice::Context { label, .. } => Some(label.as_str()),
            _ => None,
        }),
        Some("edge"),
        "the row the user was on survives its source being re-read"
    );
}

#[test]
fn the_only_kubeconfig_fields_a_row_shows_are_the_server_and_the_namespace() {
    let full = ContextRow {
        name: "prod".to_string(),
        current: true,
        server: Some("https://prod.example:6443".to_string()),
        namespace: Some("payments".to_string()),
    };
    assert_eq!(
        detail(&full).as_deref(),
        Some("https://prod.example:6443  ·  namespace payments")
    );
    assert_eq!(
        detail(&context("prod", true)),
        None,
        "a context that declares neither shows no second line at all"
    );
}

#[test]
fn every_state_this_machine_can_reach_has_a_choice_highlighted() {
    let mut state = LaunchState::new();
    let mut reached: Vec<Status> = Vec::new();
    for step in 0..6 {
        match step {
            0 => {}
            1 => state.scanned(&ScanRequest::Detected, ScanOutcome::Sources(Vec::new())),
            2 => state.scanned(
                &ScanRequest::Detected,
                ScanOutcome::Failed("unreadable".to_string()),
            ),
            3 => {
                state.scanned(
                    &ScanRequest::Detected,
                    ScanOutcome::Sources(vec![source("f", vec![context("c", false)])]),
                );
                state.confirm();
            }
            4 => state.refused("refused".to_string()),
            _ => {
                state.set_query("nothing matches".to_string());
                state.rescanning();
            }
        }
        reached.push(state.status.clone());
        assert!(
            state.selected_choice().is_some(),
            "no reachable state may leave the screen with nothing to press: {:?}",
            state.status
        );
    }
    assert_eq!(reached.len(), 6);
}
