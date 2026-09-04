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

impl Message {
    pub fn kind(&self) -> KindId {
        match self {
            Message::Apply { kind, .. }
            | Message::Delete { kind, .. }
            | Message::Settled { kind, .. }
            | Message::Desync { kind, .. } => *kind,
        }
    }
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
                undecodable = 0;
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
                let fatal = should_stop(reason);
                listing = None;
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
#[path = "watch_test.rs"]
mod tests;
