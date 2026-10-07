use super::*;

fn take<T: Send + 'static, R>(call: impl FnOnce(Reply<T>) -> R) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let _guard = call(Box::new(move |value| {
        let _ = tx.send(value);
    }));
    rx.recv().expect("NullProvider answers on this thread")
}

#[test]
fn null_provider_labels_the_new_surfaces_and_hides_observe_tools() {
    let provider = NullProvider;
    assert!(matches!(
        take(|reply| provider.fetch_grafana(reply)),
        GrafanaOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.fetch_policy(reply)),
        TableOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.fetch_harbor(reply)),
        TableOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.fetch_mesh(reply)),
        TableOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.query_promql(
            PromRequest {
                expr: "up".into(),
                window: QueryWindow {
                    start_ms: 0,
                    end_ms: 1000,
                    step_ms: 1000
                }
            },
            reply
        )),
        PromOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.query_loki(
            LokiRequest {
                query: "{app=\"a\"}".into(),
                window: QueryWindow {
                    start_ms: 0,
                    end_ms: 1000,
                    step_ms: 1000
                },
                limit: 10
            },
            reply
        )),
        LokiOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.lookup_trace("abc".into(), reply)),
        TraceOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.reveal_helm(None, "ingress".into(), 1, reply)),
        HelmRevealOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.diff_helm(None, "ingress".into(), 1, 2, reply)),
        DocOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.rollback_helm(None, "ingress".into(), 1, reply)),
        HelmRollbackOutcome::Failed(_)
    ));
    let reach = take(|reply| provider.probe_observe(reply));
    assert_eq!(reach.prometheus, ToolPresence::Missing);
    assert_eq!(reach.loki, ToolPresence::Missing);
    assert_eq!(reach.traces, ToolPresence::Missing);
}
#[test]
fn grafana_routes_keep_reserved_characters_inside_the_uid_segment() {
    assert_eq!(
        grafana_panel_path("pilot-native", 3).as_deref(),
        Some("/d/pilot-native?viewPanel=3")
    );
    assert_eq!(
        grafana_panel_path("a/b?c#d", 7).as_deref(),
        Some("/d/a%2Fb%3Fc%23d?viewPanel=7")
    );
    for uid in ["", ".", ".."] {
        assert!(grafana_panel_path(uid, 1).is_none());
    }
}
