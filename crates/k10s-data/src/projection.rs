use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use k10s_core::{Catalog, IngestEvent, Op, Payload, ResourceEvent};

use crate::assemble::{self, Assembled, Index, Store};
use crate::mapping::{Detail, Staged};

pub(crate) struct Change {
    pub(crate) op: Op,
    pub(crate) uid: Arc<str>,
    pub(crate) before: Option<Box<Staged>>,
}

pub(crate) struct Projection {
    index: Index,
    resources: HashMap<Arc<str>, ResourceEvent>,
}

impl Projection {
    pub(crate) fn from_assembled(assembled: &Assembled) -> Self {
        Projection {
            index: assembled.index.clone(),
            resources: resources(&assembled.events),
        }
    }

    pub(crate) fn project(
        &mut self,
        store: &Store,
        catalog: &mut Catalog,
        change: &Change,
    ) -> Vec<IngestEvent> {
        if structural_change(store, change) {
            return self.reconcile(store, catalog);
        }

        let Some(event) = live_event(store, &self.index, catalog, change) else {
            return Vec::new();
        };
        let IngestEvent::Resource(resource) = &event else {
            unreachable!("a projected object change is always a resource event")
        };
        self.resources
            .insert(resource.uid.clone(), canonical(resource.clone()));
        vec![event]
    }

    fn reconcile(&mut self, store: &Store, catalog: &mut Catalog) -> Vec<IngestEvent> {
        let assembled = assemble::assemble(store, catalog);
        let next = resources(&assembled.events);
        let mut replaced = HashSet::new();
        let mut deleted = Vec::new();

        for (uid, old) in &self.resources {
            match next.get(uid) {
                Some(new) if same_role(old, new) => {}
                Some(_) => {
                    replaced.insert(uid.clone());
                    deleted.push(deleted_event(old));
                }
                None => deleted.push(deleted_event(old)),
            }
        }
        deleted.sort_by(|a, b| delete_key(a).cmp(&delete_key(b)));

        let mut out = Vec::with_capacity(deleted.len() + assembled.events.len());
        out.extend(deleted.into_iter().map(IngestEvent::Resource));
        for event in &assembled.events {
            let IngestEvent::Resource(new) = event else {
                continue;
            };
            match self.resources.get(&new.uid) {
                None => out.push(event.clone()),
                Some(_) if replaced.contains(&new.uid) => out.push(event.clone()),
                Some(old) if old != new => {
                    let mut modified = new.clone();
                    modified.op = Op::Modified;
                    out.push(IngestEvent::Resource(modified));
                }
                Some(_) => {}
            }
        }

        self.index = assembled.index;
        self.resources = next;
        out
    }
}

fn resources(events: &[IngestEvent]) -> HashMap<Arc<str>, ResourceEvent> {
    events
        .iter()
        .filter_map(|event| match event {
            IngestEvent::Resource(resource) => {
                Some((resource.uid.clone(), canonical(resource.clone())))
            }
            _ => None,
        })
        .collect()
}

fn canonical(mut resource: ResourceEvent) -> ResourceEvent {
    resource.op = Op::Added;
    resource
}

fn same_role(a: &ResourceEvent, b: &ResourceEvent) -> bool {
    std::mem::discriminant(&a.payload) == std::mem::discriminant(&b.payload)
}

fn deleted_event(resource: &ResourceEvent) -> ResourceEvent {
    let mut deleted = resource.clone();
    deleted.op = Op::Deleted;
    deleted
}

fn delete_key(resource: &ResourceEvent) -> (std::cmp::Reverse<u8>, &str, &str, &str) {
    (
        std::cmp::Reverse(role_rank(&resource.payload)),
        &resource.namespace,
        &resource.name,
        &resource.uid,
    )
}

fn role_rank(payload: &Payload) -> u8 {
    match payload {
        Payload::Scope => 0,
        Payload::Owner { .. } => 1,
        Payload::Instance { .. } => 2,
        Payload::Attached { .. } => 3,
    }
}

fn structural_change(store: &Store, change: &Change) -> bool {
    if change.op != Op::Modified {
        return true;
    }
    let (Some(before), Some(after)) = (change.before.as_deref(), store.get(&change.uid)) else {
        return true;
    };
    if before.kind != after.kind
        || before.role != after.role
        || before.namespace != after.namespace
        || before.name != after.name
        || before.controller != after.controller
    {
        return true;
    }
    match (&before.detail, &after.detail) {
        (Detail::Scope, Detail::Scope) | (Detail::Owner { .. }, Detail::Owner { .. }) => false,
        (
            Detail::Instance {
                labels: before_labels,
                refs: before_refs,
                ..
            },
            Detail::Instance {
                labels: after_labels,
                refs: after_refs,
                ..
            },
        ) => before_labels != after_labels || before_refs != after_refs,
        (
            Detail::Attached {
                selector: before, ..
            },
            Detail::Attached {
                selector: after, ..
            },
        ) => before != after,
        _ => true,
    }
}

fn live_event(
    store: &Store,
    index: &Index,
    catalog: &mut Catalog,
    change: &Change,
) -> Option<IngestEvent> {
    let staged = store.get(&change.uid)?;
    let uid = &*change.uid;
    let (parent, payload) = match &staged.detail {
        Detail::Scope => (None, Payload::Scope),
        Detail::Owner { tool } => {
            if store.is_pass_through(staged.kind) && !index.emitted_owner(uid) {
                return None;
            }
            (
                Some(index.scope_uid(&staged.namespace)?.clone()),
                Payload::Owner {
                    kind: staged.kind,
                    tool: *tool,
                    depends_on: live_depends_on(index, staged),
                },
            )
        }
        Detail::Instance { reason, .. } => (
            Some(index.parent_of(uid)?.clone()),
            Payload::Instance {
                state: k10s_core::State {
                    severity: reason.severity,
                    reason: catalog.intern_reason(&reason.display),
                },
            },
        ),
        Detail::Attached { detail, .. } => (
            Some(
                index
                    .attachment_owner(staged.kind, &staged.namespace, &staged.name)?
                    .clone(),
            ),
            Payload::Attached {
                kind: staged.kind,
                detail: detail.clone(),
            },
        ),
    };
    Some(IngestEvent::Resource(ResourceEvent {
        kind: staged.kind,
        uid: staged.uid.clone(),
        namespace: staged.namespace.clone(),
        name: staged.name.clone(),
        resource_version: staged.resource_version,
        parent,
        op: change.op,
        payload,
    }))
}

fn live_depends_on(index: &Index, staged: &Staged) -> Vec<Arc<str>> {
    staged
        .controller
        .as_ref()
        .filter(|controller| index.emitted_owner(&controller.uid))
        .map(|controller| vec![controller.uid.clone()])
        .unwrap_or_default()
}
