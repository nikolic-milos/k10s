//! The log ring and its search: eviction from the front keeps the view stable,
//! follow sticks to the bottom until the user scrolls, indices survive
//! eviction, and an invalid pattern is a labelled state rather than a panic.

use super::*;

fn numbered(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("line {i}")).collect()
}

#[test]
fn the_ring_evicts_from_the_front_and_keeps_the_view_stable() {
    let mut state = TextState::new(10);
    state.set_viewport(4);
    state.append(numbered(10));
    state.scroll_by(2);
    assert_eq!(state.top(), 2);
    assert!(!state.following());

    state.append(vec!["line 10".to_string(), "line 11".to_string()]);
    assert_eq!(state.len(), 10);
    assert_eq!(state.dropped(), 2);
    assert_eq!(
        state.top(),
        0,
        "the anchor shifts with eviction so the same lines stay on screen"
    );
    assert_eq!(state.visible().next().unwrap().1, "line 2");
}

#[test]
fn follow_sticks_to_the_bottom_until_the_user_scrolls() {
    let mut state = TextState::new(100);
    state.set_viewport(5);
    state.toggle_follow();
    state.append(numbered(20));
    assert_eq!(state.top(), 15, "follow rides the tail");
    assert!(state.at_bottom());

    state.scroll_by(-3);
    assert!(!state.following(), "scrolling up breaks follow");
    state.append(numbered(5));
    assert_eq!(state.top(), 12, "and the view stays put");

    state.toggle_follow();
    assert!(state.at_bottom());
}

#[test]
fn search_is_case_insensitive_cycles_and_survives_appends() {
    let mut state = TextState::new(100);
    state.set_viewport(6);
    state.set_lines(vec![
        "listening".to_string(),
        "ERROR one".to_string(),
        "ready".to_string(),
        "error two".to_string(),
    ]);
    state.set_search(Some("error".to_string()));
    let (query, current, total) = state.search().expect("a search");
    assert_eq!((query, current, total), ("error", 1, 2));
    assert_eq!(state.current_match_line(), Some(1));

    state.next_match();
    assert_eq!(state.current_match_line(), Some(3));
    state.next_match();
    assert_eq!(state.current_match_line(), Some(1), "wraps");
    state.prev_match();
    assert_eq!(state.current_match_line(), Some(3));

    state.append(vec!["Error three".to_string()]);
    assert_eq!(state.search().unwrap().2, 3, "appends join the matches");

    state.set_search(None);
    assert!(state.search().is_none());
}

#[test]
fn a_regex_pattern_matches_structure_not_just_substrings() {
    let mut state = TextState::new(100);
    state.set_viewport(6);
    state.set_lines(vec![
        "error one".to_string(),
        "an error mid-line".to_string(),
        "GET /healthz 200".to_string(),
        "GET /metrics 500".to_string(),
    ]);

    state.set_search(Some("^error".to_string()));
    assert_eq!(state.search().unwrap().2, 1, "anchors anchor");
    assert_eq!(state.current_match_line(), Some(0));
    assert!(state.search_error().is_none());

    state.set_search(Some(r"GET .* (200|500)".to_string()));
    assert_eq!(state.search().unwrap().2, 2, "alternation works");

    state.set_search(Some("get /HEALTHZ".to_string()));
    assert_eq!(state.search().unwrap().2, 1, "still case-insensitive");
}

#[test]
fn an_invalid_pattern_is_a_labelled_state_never_a_panic() {
    let mut state = TextState::new(10);
    state.set_viewport(4);
    state.set_lines(vec!["error [one]".to_string()]);

    state.set_search(Some("[".to_string()));
    let (query, _, total) = state.search().expect("the search stays visible");
    assert_eq!(query, "[");
    assert_eq!(total, 0, "an invalid pattern matches nothing");
    let reason = state.search_error().expect("the state is labelled");
    assert!(!reason.contains('\n'), "one status line: {reason:?}");

    state.append(vec!["error [two]".to_string()]);
    state.next_match();
    state.prev_match();
    assert_eq!(state.search().unwrap().2, 0, "appends stay unmatched");
    assert!(state.search_error().is_some());

    state.set_search(Some(r"\[".to_string()));
    assert!(state.search_error().is_none(), "a fix clears the label");
    assert_eq!(state.search().unwrap().2, 2);
}

#[test]
fn search_indices_survive_ring_eviction() {
    let mut state = TextState::new(4);
    state.append(vec![
        "error a".to_string(),
        "calm".to_string(),
        "error b".to_string(),
        "calm".to_string(),
    ]);
    state.set_search(Some("error".to_string()));
    assert_eq!(state.search().unwrap().2, 2);

    state.append(vec!["calm".to_string(), "error c".to_string()]);
    let (_, _, total) = state.search().expect("still searching");
    assert_eq!(total, 2, "the evicted match is gone, the new one counted");
    state.next_match();
    let line = state.current_match_line().expect("a match line");
    assert!(state.visible().count() > 0);
    assert!(line < state.len());
}

#[test]
fn scrolling_clamps_and_pages_by_the_viewport() {
    let mut state = TextState::new(100);
    state.set_viewport(10);
    state.set_lines(numbered(25));
    state.scroll_by(-5);
    assert_eq!(state.top(), 0);
    state.page_down();
    assert_eq!(state.top(), 9);
    state.page_down();
    assert_eq!(state.top(), 15, "clamped to the last full screen");
    state.end();
    assert_eq!(state.top(), 15);
    state.home();
    assert_eq!(state.top(), 0);
    state.scroll_by(1000);
    assert_eq!(state.top(), 15);
}

#[test]
fn the_kubelet_timestamp_is_stripped_only_when_it_is_one() {
    assert_eq!(strip_timestamp("2026-08-02T05:00:01Z ready"), "ready");
    assert_eq!(
        strip_timestamp("2026-08-02T05:00:01.123456789Z GET /healthz"),
        "GET /healthz"
    );
    assert_eq!(strip_timestamp("plain line"), "plain line");
    assert_eq!(
        strip_timestamp("12:00:01 not a date"),
        "12:00:01 not a date"
    );
    assert_eq!(strip_timestamp(""), "");
}

#[test]
fn a_line_that_only_starts_like_a_date_keeps_its_head() {
    assert_eq!(
        strip_timestamp("2026-08-02Tnope here is the rest"),
        "2026-08-02Tnope here is the rest"
    );
    assert_eq!(
        strip_timestamp("1234-56-78T90 pod started"),
        "1234-56-78T90 pod started"
    );
    assert_eq!(
        strip_timestamp("2026-8-02T05:00:01Z ready"),
        "2026-8-02T05:00:01Z ready"
    );
}
