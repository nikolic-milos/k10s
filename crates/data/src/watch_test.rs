//! Which failures a watch can recover from and which stop it: a 410 is
//! expired and recoverable where a 403 stops rather than retrying forever, a
//! relist deletes what it did not list, and a malformed object is reported once
//! per listing epoch without ending the stream. Every path still settles
//! exactly once.

use super::*;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::core::Status;

fn status(code: u16, reason: &str) -> Box<Status> {
    Box::new(Status {
        code,
        reason: reason.to_string(),
        message: "denied".to_string(),
        ..Default::default()
    })
}

fn pod(uid: Option<&str>) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some("api-1".into()),
            namespace: Some("prod".into()),
            uid: uid.map(str::to_string),
            resource_version: Some("42".into()),
            ..Default::default()
        },
        spec: None,
        status: None,
    }
}

fn stage_pod(pod: &Pod) -> Option<Staged> {
    mapping::stage_pod(KindId::POD, &AttachKinds::default(), pod)
}

#[test]
fn a_410_is_expired_and_recoverable() {
    let reason = desync_reason(&watcher::Error::WatchError(status(410, "Expired")));
    assert_eq!(reason, DesyncReason::Expired);
    assert!(reason.is_recoverable());
    assert!(!should_stop(reason));

    let via_list = desync_reason(&watcher::Error::InitialListFailed(kube::Error::Api(
        status(410, "Expired"),
    )));
    assert_eq!(via_list, DesyncReason::Expired);
}

#[test]
fn a_403_stops_the_stream_instead_of_retrying_forever() {
    for code in [401, 403] {
        let reason = desync_reason(&watcher::Error::WatchStartFailed(kube::Error::Api(status(
            code,
            "Forbidden",
        ))));
        assert_eq!(reason, DesyncReason::Forbidden, "{code}");
        assert!(!reason.is_recoverable(), "{code}");
        assert!(should_stop(reason), "{code}");
    }
}

#[test]
fn a_decode_failure_is_malformed_and_a_transport_failure_is_closed() {
    let bad_json = serde_json::from_str::<Pod>("{").expect_err("invalid json");
    assert_eq!(
        desync_reason(&watcher::Error::WatchFailed(kube::Error::SerdeError(
            bad_json
        ))),
        DesyncReason::Malformed
    );
    assert_eq!(
        desync_reason(&watcher::Error::NoResourceVersion),
        DesyncReason::Malformed
    );
    assert_eq!(
        desync_reason(&watcher::Error::WatchFailed(kube::Error::ReadEvents(
            std::io::Error::other("connection reset by peer")
        ))),
        DesyncReason::Closed
    );
    for code in [404, 500, 503, 504] {
        assert_eq!(
            desync_reason(&watcher::Error::WatchError(status(code, "x"))),
            DesyncReason::Closed,
            "{code}"
        );
    }
}

#[test]
fn every_watcher_event_maps_to_exactly_one_signal() {
    assert_eq!(
        signal_of::<Pod>(Ok(watcher::Event::Init), &stage_pod),
        Signal::Restarted
    );
    assert_eq!(
        signal_of(Ok(watcher::Event::InitDone), &stage_pod),
        Signal::Settled
    );
    let Signal::Apply(staged) = signal_of(Ok(watcher::Event::Apply(pod(Some("u1")))), &stage_pod)
    else {
        panic!("expected an apply")
    };
    assert_eq!(&*staged.uid, "u1");
    assert_eq!(staged.resource_version, 42);
    let Signal::Apply(staged) =
        signal_of(Ok(watcher::Event::InitApply(pod(Some("u1")))), &stage_pod)
    else {
        panic!("an init apply is an apply")
    };
    assert_eq!(&*staged.uid, "u1");
    assert_eq!(
        signal_of(Ok(watcher::Event::Delete(pod(Some("u1")))), &stage_pod),
        Signal::Delete(Arc::from("u1"))
    );
}

#[test]
fn an_object_with_no_uid_is_undecodable_rather_than_staged() {
    assert_eq!(
        signal_of(Ok(watcher::Event::Apply(pod(None))), &stage_pod),
        Signal::Undecodable
    );
    assert_eq!(
        signal_of(Ok(watcher::Event::Delete(pod(None))), &stage_pod),
        Signal::Undecodable
    );
}

async fn collect(signals: Vec<Signal>) -> Vec<Message> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    drive(KindId::POD, scripted(signals), tx).await;
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        out.push(m);
    }
    out
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

#[test]
fn a_stream_settles_once_even_across_a_relist() {
    let messages = runtime().block_on(collect(vec![
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Settled,
        Signal::Error(DesyncReason::Expired),
        Signal::Restarted,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Settled,
    ]));
    let settled = messages
        .iter()
        .filter(|m| matches!(m, Message::Settled { .. }))
        .count();
    assert_eq!(settled, 1, "one settle per stream, not one per list");
    assert!(matches!(
        messages
            .iter()
            .find(|m| matches!(m, Message::Desync { .. })),
        Some(Message::Desync {
            reason: DesyncReason::Expired,
            ..
        })
    ));
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::Apply { .. }))
            .count(),
        2,
        "a relist re-applies, which the store coalesces by uid"
    );
}

#[test]
fn a_relist_deletes_what_it_did_not_list() {
    let messages = runtime().block_on(collect(vec![
        Signal::Restarted,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Apply(Box::new(stage_pod(&pod(Some("u2"))).expect("staged"))),
        Signal::Settled,
        Signal::Error(DesyncReason::Expired),
        Signal::Restarted,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Settled,
    ]));
    let deleted: Vec<&str> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Delete { uid, .. } => Some(&**uid),
            _ => None,
        })
        .collect();
    assert_eq!(deleted, ["u2"], "{messages:?}");
}

#[test]
fn an_initial_list_deletes_nothing_and_a_relist_repeats_no_delete() {
    let first = runtime().block_on(collect(vec![
        Signal::Restarted,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Apply(Box::new(stage_pod(&pod(Some("u2"))).expect("staged"))),
        Signal::Settled,
    ]));
    assert!(
        first.iter().all(|m| !matches!(m, Message::Delete { .. })),
        "{first:?}"
    );

    let after_delete = runtime().block_on(collect(vec![
        Signal::Restarted,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Apply(Box::new(stage_pod(&pod(Some("u2"))).expect("staged"))),
        Signal::Settled,
        Signal::Delete(Arc::from("u2")),
        Signal::Error(DesyncReason::Expired),
        Signal::Restarted,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Settled,
    ]));
    assert_eq!(
        after_delete
            .iter()
            .filter(|m| matches!(m, Message::Delete { .. }))
            .count(),
        1,
        "{after_delete:?}"
    );
}

#[test]
fn a_forbidden_stream_stops_and_still_settles() {
    let messages = runtime().block_on(collect(vec![
        Signal::Error(DesyncReason::Forbidden),
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Settled,
    ]));
    assert!(matches!(
        messages.first(),
        Some(Message::Desync {
            reason: DesyncReason::Forbidden,
            ..
        })
    ));
    assert!(
        messages.iter().all(|m| !matches!(m, Message::Apply { .. })),
        "the stream must stop at the denial"
    );
    assert!(matches!(
        messages.last(),
        Some(Message::Settled { listed: false, .. })
    ));
}

#[test]
fn malformed_objects_are_reported_once_and_do_not_end_the_stream() {
    let messages = runtime().block_on(collect(vec![
        Signal::Undecodable,
        Signal::Undecodable,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Settled,
    ]));
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(
                m,
                Message::Desync {
                    reason: DesyncReason::Malformed,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::Apply { .. }))
            .count(),
        1,
        "objects after a bad one still arrive"
    );
    assert!(matches!(
        messages.last(),
        Some(Message::Settled { listed: true, .. })
    ));
}

#[test]
fn a_malformed_object_is_reported_again_after_a_relist() {
    let messages = runtime().block_on(collect(vec![
        Signal::Undecodable,
        Signal::Undecodable,
        Signal::Error(DesyncReason::Expired),
        Signal::Restarted,
        Signal::Undecodable,
        Signal::Undecodable,
        Signal::Settled,
    ]));
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(
                m,
                Message::Desync {
                    reason: DesyncReason::Malformed,
                    ..
                }
            ))
            .count(),
        2,
        "each listing epoch reports its own garbage once: {messages:?}"
    );
}

#[test]
fn an_interrupted_listing_deletes_nothing_when_it_settles() {
    let messages = runtime().block_on(collect(vec![
        Signal::Restarted,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Apply(Box::new(stage_pod(&pod(Some("u2"))).expect("staged"))),
        Signal::Settled,
        Signal::Error(DesyncReason::Expired),
        Signal::Restarted,
        Signal::Apply(Box::new(stage_pod(&pod(Some("u1"))).expect("staged"))),
        Signal::Error(DesyncReason::Closed),
        Signal::Settled,
    ]));
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m, Message::Delete { .. })),
        "a listing window that broke never saw u2, which is not the same as u2 being gone: \
         {messages:?}"
    );
}

#[test]
fn only_the_kinds_with_a_typed_stream_are_watched_at_full_fidelity() {
    use crate::discover::fidelity_of;

    for (group, kind) in [("", "Pod"), ("", "Service"), ("", "PersistentVolumeClaim")] {
        assert_eq!(
            fidelity_of(group, kind),
            Fidelity::Full,
            "{group}/{kind} has a typed arm in one_stream"
        );
    }
    for (group, kind) in [
        ("", "Namespace"),
        ("", "ConfigMap"),
        ("", "Secret"),
        ("apps", "Deployment"),
        ("apps", "StatefulSet"),
        ("apps", "DaemonSet"),
        ("apps", "ReplicaSet"),
        ("batch", "Job"),
        ("batch", "CronJob"),
    ] {
        assert_eq!(
            fidelity_of(group, kind),
            Fidelity::Metadata,
            "one_stream has no typed arm for {group}/{kind}: declaring it Full would watch it \
             as metadata anyway and lose spec and status without saying so"
        );
    }
}

#[test]
fn a_stream_that_ends_without_listing_still_settles() {
    let messages = runtime().block_on(collect(Vec::new()));
    assert!(matches!(
        messages.as_slice(),
        [Message::Settled { listed: false, .. }]
    ));
}

fn target(kind: &str, namespaced: bool, role: Role) -> WatchTarget {
    WatchTarget {
        target: crate::discover::KindTarget {
            id: KindId::POD,
            resource: kube::discovery::ApiResource {
                group: String::new(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: kind.into(),
                plural: format!("{}s", kind.to_ascii_lowercase()),
            },
            role,
            namespaced,
            listable: true,
            watchable: true,
            patchable: true,
            status_subresource: false,
        },
        fidelity: Fidelity::Full,
        pass_through: false,
    }
}

#[test]
fn a_denied_kind_is_planned_as_no_requests_at_all() {
    let pods = target("Pod", true, Role::Instance);
    assert!(stream_scopes(&pods, &WatchScope::Denied).is_empty());
    assert_eq!(stream_scopes(&pods, &WatchScope::All), vec![None]);
    assert_eq!(
        stream_scopes(
            &pods,
            &WatchScope::Namespaces(vec!["team-a".into(), "team-b".into()])
        ),
        vec![Some("team-a".to_string()), Some("team-b".to_string())],
        "one request per permitted namespace"
    );
}

#[test]
fn a_cluster_scoped_kind_is_never_split_per_namespace() {
    let namespaces = target("Namespace", false, Role::Scope);
    assert_eq!(
        stream_scopes(
            &namespaces,
            &WatchScope::Namespaces(vec!["team-a".into(), "team-b".into()])
        ),
        vec![None]
    );
}
