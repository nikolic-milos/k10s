use super::*;

#[test]
fn prom_reply_fits_in_256_bytes() {
    let bytes = std::mem::size_of::<PromOutcome>();
    assert!(bytes <= 256, "PromOutcome grew to {bytes} bytes");
}

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
        grafana_dashboard_path("pilot-native").as_deref(),
        Some("/d/pilot-native")
    );
    assert_eq!(
        grafana_panel_path("pilot-native", 0).as_deref(),
        Some("/d/pilot-native?viewPanel=0")
    );
    assert_eq!(
        grafana_panel_path("pilot-native", 3).as_deref(),
        Some("/d/pilot-native?viewPanel=3")
    );
    assert_eq!(
        grafana_panel_path("a/b?c#d", 7).as_deref(),
        Some("/d/a%2Fb%3Fc%23d?viewPanel=7")
    );
    assert_eq!(
        grafana_dashboard_path("a/b?c#d").as_deref(),
        Some("/d/a%2Fb%3Fc%23d")
    );
    for uid in ["", ".", ".."] {
        assert!(grafana_panel_path(uid, 1).is_none());
        assert!(grafana_dashboard_path(uid).is_none());
    }
}

#[test]
fn a_forward_lease_finishes_once_with_its_handoff_decision() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let closed = Arc::new(AtomicUsize::new(0));
    let kept = Arc::new(AtomicUsize::new(0));
    let row = ForwardRow {
        id: 7,
        namespace: "observability".into(),
        pod: "grafana".into(),
        local_port: 3000,
        remote_port: 3000,
        expected_service: None,
        state: ForwardState::Opening,
    };
    let close = closed.clone();
    let keep = kept.clone();
    let lease = ForwardLease::new(row.clone(), move |retain| {
        if retain {
            keep.fetch_add(1, Ordering::SeqCst);
        } else {
            close.fetch_add(1, Ordering::SeqCst);
        }
    });
    let (tx, rx) = std::sync::mpsc::channel();
    drop(rx);
    assert!(tx.send(ForwardStartOutcome::Started(lease)).is_err());
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert_eq!(kept.load(Ordering::SeqCst), 0);

    let close = closed.clone();
    let keep = kept.clone();
    let lease = ForwardLease::new(row.clone(), move |retain| {
        if retain {
            keep.fetch_add(1, Ordering::SeqCst);
        } else {
            close.fetch_add(1, Ordering::SeqCst);
        }
    });
    assert_eq!(lease.row(), &row);
    lease.keep();
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert_eq!(kept.load(Ordering::SeqCst), 1);
}

#[test]
fn an_empty_slot_inspector_replies_without_owning_cluster_work() {
    let slot = ProviderSlot::empty();
    assert_eq!(
        take(|reply| slot.fetch_events("prod", "selected-uid", reply)),
        Detail::Failed(NO_CLUSTER.to_string())
    );
    assert_eq!(
        take(|reply| slot.fetch_log_tail("prod", "api-1", "selected-uid", reply)),
        Detail::Failed(NO_CLUSTER.to_string())
    );
    assert_eq!(
        take(|reply| slot.fetch_collectors(reply)),
        TableOutcome::Failed(NO_CLUSTER.to_string())
    );
}
