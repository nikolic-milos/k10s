use std::collections::HashSet;
use std::sync::Arc;

use futures::{StreamExt, stream::BoxStream};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Service};
use k10s_core::{DesyncReason, KindId, Role};
use kube::api::{Api, DynamicObject, PartialObjectMeta};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Client, Resource};

use crate::discover::{Fidelity, WatchTarget};
use crate::mapping::{self, AttachKinds, Staged};
use crate::rbac::WatchScope;

#[derive(Debug, PartialEq)]
pub enum Signal {
    Restarted,
    Apply(Box<Staged>),
    Delete(Arc<str>),
    Undecodable,
    Settled,
    Error(DesyncReason),
}

#[derive(Debug)]
pub enum Message {
    Apply { kind: KindId, staged: Box<Staged> },
    Delete { kind: KindId, uid: Arc<str> },
    Settled { kind: KindId, listed: bool },
    Desync { kind: KindId, reason: DesyncReason },
}

pub fn desync_reason(err: &watcher::Error) -> DesyncReason {
    match err {
        watcher::Error::InitialListFailed(e)
        | watcher::Error::WatchStartFailed(e)
        | watcher::Error::WatchFailed(e) => client_reason(e),
        watcher::Error::WatchError(status) => status_reason(status.code),
        watcher::Error::NoResourceVersion => DesyncReason::Malformed,
    }
}

fn client_reason(err: &kube::Error) -> DesyncReason {
    match err {
        kube::Error::Api(status) => status_reason(status.code),
        kube::Error::SerdeError(_) => DesyncReason::Malformed,
        _ => DesyncReason::Closed,
    }
}

fn status_reason(code: u16) -> DesyncReason {
    match code {
        401 | 403 => DesyncReason::Forbidden,
        410 => DesyncReason::Expired,
        _ => DesyncReason::Closed,
    }
}

pub fn signal_of<K: Resource>(
    event: watcher::Result<watcher::Event<K>>,
    stage: &impl Fn(&K) -> Option<Staged>,
) -> Signal {
    match event {
        Ok(watcher::Event::Init) => Signal::Restarted,
        Ok(watcher::Event::InitApply(obj)) | Ok(watcher::Event::Apply(obj)) => match stage(&obj) {
            Some(staged) => Signal::Apply(Box::new(staged)),
            None => Signal::Undecodable,
        },
        Ok(watcher::Event::Delete(obj)) => match obj.meta().uid.as_deref() {
            Some(uid) => Signal::Delete(uid.into()),
            None => Signal::Undecodable,
        },
        Ok(watcher::Event::InitDone) => Signal::Settled,
        Err(err) => Signal::Error(desync_reason(&err)),
    }
}

fn signal_stream<K>(
    api: Api<K>,
    stage: impl Fn(&K) -> Option<Staged> + Send + 'static,
) -> BoxStream<'static, Signal>
where
    K: Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug + Send + 'static,
{
    watcher(api, watcher::Config::default())
        .default_backoff()
        .map(move |event| signal_of(event, &stage))
        .boxed()
}

pub fn stream_scopes(target: &WatchTarget, scope: &WatchScope) -> Vec<Option<String>> {
    match scope {
        WatchScope::Denied => Vec::new(),
        WatchScope::All => vec![None],
        WatchScope::Namespaces(list) => {
            if target.target.namespaced {
                list.iter().map(|ns| Some(ns.clone())).collect()
            } else {
                vec![None]
            }
        }
    }
}

pub fn streams_for(
    client: &Client,
    target: &WatchTarget,
    scope: &WatchScope,
    attach: AttachKinds,
) -> Vec<BoxStream<'static, Signal>> {
    stream_scopes(target, scope)
        .into_iter()
        .map(|ns| one_stream(client, target, ns.as_deref(), attach))
        .collect()
}

fn one_stream(
    client: &Client,
    target: &WatchTarget,
    namespace: Option<&str>,
    attach: AttachKinds,
) -> BoxStream<'static, Signal> {
    let kind = target.target.id;
    let role = target.target.role;
    let resource = target.target.resource.clone();

    match (target.target.group(), target.target.kind()) {
        ("", "Pod") => {
            let api = typed_api::<Pod>(client, namespace);
            signal_stream(api, move |pod: &Pod| mapping::stage_pod(kind, &attach, pod))
        }
        ("", "Service") => {
            let api = typed_api::<Service>(client, namespace);
            signal_stream(api, move |svc: &Service| mapping::stage_service(kind, svc))
        }
        ("", "PersistentVolumeClaim") => {
            let api = typed_api::<PersistentVolumeClaim>(client, namespace);
            signal_stream(api, move |pvc: &PersistentVolumeClaim| {
                mapping::stage_pvc(kind, pvc)
            })
        }
        _ => {
            debug_assert_eq!(
                target.fidelity,
                Fidelity::Metadata,
                "a kind declared Full with no typed handler would silently be watched as metadata"
            );
            let api: Api<PartialObjectMeta<DynamicObject>> = match namespace {
                Some(ns) => Api::namespaced_with(client.clone(), ns, &resource),
                None => Api::all_with(client.clone(), &resource),
            };
            signal_stream(api, move |obj: &PartialObjectMeta<DynamicObject>| {
                stage_partial(kind, role, obj)
            })
        }
    }
}

fn stage_partial(
    kind: KindId,
    role: Role,
    obj: &PartialObjectMeta<DynamicObject>,
) -> Option<Staged> {
    mapping::stage_meta(kind, role, &obj.metadata)
}

fn typed_api<K>(client: &Client, namespace: Option<&str>) -> Api<K>
where
    K: Resource<Scope = kube::core::NamespaceResourceScope>,
    K::DynamicType: Default,
{
    match namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    }
}

fn vanished(held: &HashSet<Arc<str>>, listed: &HashSet<Arc<str>>) -> Vec<Arc<str>> {
    let mut gone: Vec<Arc<str>> = held.difference(listed).cloned().collect();
    gone.sort_unstable();
    gone
}

pub async fn drive(
    kind: KindId,
    mut stream: BoxStream<'static, Signal>,
    tx: tokio::sync::mpsc::Sender<Message>,
) {
    let mut listed = false;
    let mut settled_sent = false;
    let mut undecodable = 0u32;
    let mut held: HashSet<Arc<str>> = HashSet::new();
    let mut listing: Option<HashSet<Arc<str>>> = None;
    while let Some(signal) = stream.next().await {
        let send = match signal {
            Signal::Restarted => {
                listing = Some(HashSet::new());
                continue;
            }
            Signal::Apply(staged) => {
                if let Some(seen) = &mut listing {
                    seen.insert(staged.uid.clone());
                }
                held.insert(staged.uid.clone());
                Message::Apply { kind, staged }
            }
            Signal::Delete(uid) => {
                held.remove(&uid);
                if let Some(seen) = &mut listing {
                    seen.remove(&uid);
                }
                Message::Delete { kind, uid }
            }
            Signal::Undecodable => {
                undecodable += 1;
                if undecodable > 1 {
                    continue;
                }
                Message::Desync {
                    kind,
                    reason: DesyncReason::Malformed,
                }
            }
            Signal::Settled => {
                listed = true;
                if let Some(relisted) = listing.take() {
                    for uid in vanished(&held, &relisted) {
                        if tx.send(Message::Delete { kind, uid }).await.is_err() {
                            return;
                        }
                    }
                    held = relisted;
                }
                if settled_sent {
                    continue;
                }
                settled_sent = true;
                Message::Settled { kind, listed: true }
            }
            Signal::Error(reason) => {
                let fatal = !reason.is_recoverable();
                if tx.send(Message::Desync { kind, reason }).await.is_err() {
                    return;
                }
                if fatal {
                    break;
                }
                continue;
            }
        };
        if tx.send(send).await.is_err() {
            return;
        }
    }
    if !settled_sent {
        let _ = tx.send(Message::Settled { kind, listed }).await;
    }
}

pub fn should_stop(reason: DesyncReason) -> bool {
    !reason.is_recoverable()
}

#[cfg(test)]
pub(crate) fn scripted(signals: Vec<Signal>) -> BoxStream<'static, Signal> {
    futures::stream::iter(signals).boxed()
}

#[cfg(test)]
mod tests {
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
            let reason = desync_reason(&watcher::Error::WatchStartFailed(kube::Error::Api(
                status(code, "Forbidden"),
            )));
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
        let Signal::Apply(staged) =
            signal_of(Ok(watcher::Event::Apply(pod(Some("u1")))), &stage_pod)
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
}
