use super::*;

fn take<T: Send + 'static>(call: impl FnOnce(Reply<T>)) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    call(Box::new(move |value| {
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
        take(|reply| provider.query_promql("up".into(), reply)),
        PromOutcome::Failed(_)
    ));
    assert!(matches!(
        take(|reply| provider.query_loki("{app=\"a\"}".into(), reply)),
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
