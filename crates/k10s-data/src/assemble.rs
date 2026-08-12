use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use k10s_core::{Catalog, IngestEvent, KindId, Op, Payload, ResourceEvent, Role, State, ToolId};

use crate::mapping::{AttachRef, Controller, Detail, Labels, Staged};

const MAX_OWNER_HOPS: usize = 8;

pub const STANDALONE_PREFIX: &str = "k10s:standalone/";

#[derive(Debug, Default)]
pub struct Store {
    objects: HashMap<Arc<str>, Staged>,
    pass_through: Vec<KindId>,
}

impl Store {
    pub fn new(pass_through: Vec<KindId>) -> Store {
        Store {
            pass_through,
            ..Store::default()
        }
    }

    pub fn is_pass_through(&self, kind: KindId) -> bool {
        self.pass_through.contains(&kind)
    }

    pub fn apply(&mut self, staged: Staged) {
        let _ = self.replace(staged);
    }

    pub(crate) fn replace(&mut self, staged: Staged) -> Option<Staged> {
        self.objects.insert(staged.uid.clone(), staged)
    }

    pub fn remove(&mut self, uid: &str) -> Option<Staged> {
        self.objects.remove(uid)
    }

    pub fn get(&self, uid: &str) -> Option<&Staged> {
        self.objects.get(uid)
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    fn by_role(&self, role: Role) -> impl Iterator<Item = &Staged> {
        self.objects.values().filter(move |s| s.role == role)
    }

    fn sorted_by_role(&self, role: Role) -> Vec<&Staged> {
        let mut out: Vec<&Staged> = self.by_role(role).collect();
        out.sort_by(|a, b| (&a.namespace, &a.name, &a.uid).cmp(&(&b.namespace, &b.name, &b.uid)));
        out
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssembleStats {
    pub scopes: u32,
    pub owners: u32,
    pub instances: u32,
    pub attachments: u32,
    pub synthetic_owners: u32,
    pub unknown_namespace: u32,
    pub unattached: u32,
    pub owner_cycles: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Index {
    scope_of: HashMap<Arc<str>, Arc<str>>,
    attach_owner: HashMap<AttachKey, Arc<str>>,
    parent_of: HashMap<Arc<str>, Arc<str>>,
    owners: HashSet<Arc<str>>,
}

impl Index {
    pub fn scope_uid(&self, namespace: &str) -> Option<&Arc<str>> {
        self.scope_of.get(namespace)
    }

    pub fn emitted_owner(&self, uid: &str) -> bool {
        self.owners.contains(uid)
    }

    pub fn parent_of(&self, uid: &str) -> Option<&Arc<str>> {
        self.parent_of.get(uid)
    }

    pub fn attachment_owner(&self, kind: KindId, namespace: &str, name: &str) -> Option<&Arc<str>> {
        self.attach_owner
            .get(&(kind, Arc::from(namespace), Arc::from(name)))
    }
}

#[derive(Debug, Default)]
pub struct Assembled {
    pub events: Vec<IngestEvent>,
    pub stats: AssembleStats,
    pub index: Index,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerOf {
    Watched(Arc<str>),
    Promote(Arc<str>),
    Reference(Controller),
    Standalone,
    Cyclic,
}

fn owner_for(store: &Store, controller: Option<&Controller>) -> OwnerOf {
    let Some(mut cur) = controller.cloned() else {
        return OwnerOf::Standalone;
    };
    for _ in 0..MAX_OWNER_HOPS {
        let Some(found) = store.get(&cur.uid) else {
            return OwnerOf::Reference(cur);
        };
        if !store.is_pass_through(found.kind) {
            return if found.role == Role::Owner {
                OwnerOf::Watched(found.uid.clone())
            } else {
                OwnerOf::Standalone
            };
        }
        match &found.controller {
            Some(next) => cur = next.clone(),
            None => return OwnerOf::Promote(found.uid.clone()),
        }
    }
    OwnerOf::Cyclic
}

pub fn selector_matches(selector: &Labels, labels: &Labels) -> bool {
    if selector.is_empty() {
        return false;
    }
    selector
        .iter()
        .all(|(k, v)| labels.iter().any(|(lk, lv)| lk == k && lv == v))
}

fn prefer(existing: Option<&Arc<str>>, candidate: &Arc<str>) -> bool {
    match existing {
        None => true,
        Some(cur) => candidate < cur,
    }
}

type AttachKey = (KindId, Arc<str>, Arc<str>);

type LabelKey = (Arc<str>, Arc<str>, Arc<str>);

struct Emit<'a> {
    uid: Arc<str>,
    kind: KindId,
    name: Arc<str>,
    namespace: Arc<str>,
    resource_version: u64,
    watched: Option<&'a Staged>,
}

pub fn assemble(store: &Store, catalog: &mut Catalog) -> Assembled {
    let mut out = Assembled::default();

    let mut scopes: Vec<&Staged> = store.by_role(Role::Scope).collect();
    scopes.sort_by(|a, b| (&a.name, &a.uid).cmp(&(&b.name, &b.uid)));
    for scope in &scopes {
        out.index
            .scope_of
            .insert(scope.name.clone(), scope.uid.clone());
        out.events.push(IngestEvent::Resource(ResourceEvent {
            kind: scope.kind,
            uid: scope.uid.clone(),
            namespace: Arc::from(""),
            name: scope.name.clone(),
            resource_version: scope.resource_version,
            parent: None,
            op: Op::Added,
            payload: Payload::Scope,
        }));
        out.stats.scopes += 1;
    }

    let mut owners: Vec<Emit<'_>> = Vec::new();
    let mut owner_slot: HashMap<Arc<str>, usize> = HashMap::new();

    for owner in store.sorted_by_role(Role::Owner) {
        if store.is_pass_through(owner.kind) {
            continue;
        }
        if owner_slot.contains_key(&owner.uid) {
            continue;
        }
        owner_slot.insert(owner.uid.clone(), owners.len());
        owners.push(Emit {
            uid: owner.uid.clone(),
            kind: owner.kind,
            name: owner.name.clone(),
            namespace: owner.namespace.clone(),
            resource_version: owner.resource_version,
            watched: Some(owner),
        });
    }

    let instances = store.sorted_by_role(Role::Instance);
    let mut parent_of: HashMap<Arc<str>, Arc<str>> = HashMap::new();
    for inst in &instances {
        let resolved = owner_for(store, inst.controller.as_ref());
        let emit = match resolved {
            OwnerOf::Watched(uid) => {
                if owner_slot.contains_key(&uid) {
                    parent_of.insert(inst.uid.clone(), uid);
                }
                continue;
            }
            OwnerOf::Cyclic => {
                out.stats.owner_cycles += 1;
                continue;
            }
            OwnerOf::Promote(uid) => {
                let Some(staged) = store.get(&uid) else {
                    continue;
                };
                Emit {
                    uid: staged.uid.clone(),
                    kind: staged.kind,
                    name: staged.name.clone(),
                    namespace: staged.namespace.clone(),
                    resource_version: staged.resource_version,
                    watched: Some(staged),
                }
            }
            OwnerOf::Reference(controller) => Emit {
                uid: controller.uid.clone(),
                kind: intern_reference(catalog, &controller),
                name: controller.name.clone(),
                namespace: inst.namespace.clone(),
                resource_version: 0,
                watched: None,
            },
            OwnerOf::Standalone => Emit {
                uid: format!("{STANDALONE_PREFIX}{}", inst.uid).into(),
                kind: inst.kind,
                name: inst.name.clone(),
                namespace: inst.namespace.clone(),
                resource_version: 0,
                watched: None,
            },
        };
        let uid = emit.uid.clone();
        if !owner_slot.contains_key(&uid) {
            out.stats.synthetic_owners += 1;
            owner_slot.insert(uid.clone(), owners.len());
            owners.push(emit);
        }
        parent_of.insert(inst.uid.clone(), uid);
    }

    let mut attach_owner: HashMap<AttachKey, Arc<str>> = HashMap::new();
    let mut by_label: HashMap<LabelKey, Vec<usize>> = HashMap::new();
    for (i, inst) in instances.iter().enumerate() {
        let Some(owner) = parent_of.get(&inst.uid) else {
            continue;
        };
        let Detail::Instance { labels, refs, .. } = &inst.detail else {
            continue;
        };
        for AttachRef { kind, name } in refs {
            let key = (*kind, inst.namespace.clone(), name.clone());
            if prefer(attach_owner.get(&key), owner) {
                attach_owner.insert(key, owner.clone());
            }
        }
        for (k, v) in labels {
            by_label
                .entry((inst.namespace.clone(), k.clone(), v.clone()))
                .or_default()
                .push(i);
        }
    }

    let attachments = store.sorted_by_role(Role::Attached);
    for att in &attachments {
        let Detail::Attached { selector, .. } = &att.detail else {
            continue;
        };
        let Some(first) = selector.first() else {
            continue;
        };
        let candidates = by_label
            .get(&(att.namespace.clone(), first.0.clone(), first.1.clone()))
            .map(|v| v.as_slice())
            .unwrap_or_default();
        let mut best: Option<Arc<str>> = None;
        for &i in candidates {
            let Detail::Instance { labels, .. } = &instances[i].detail else {
                continue;
            };
            if !selector_matches(selector, labels) {
                continue;
            }
            if let Some(owner) = parent_of.get(&instances[i].uid)
                && prefer(best.as_ref(), owner)
            {
                best = Some(owner.clone());
            }
        }
        if let Some(owner) = best {
            attach_owner.insert((att.kind, att.namespace.clone(), att.name.clone()), owner);
        }
    }

    let mut emitted: HashSet<Arc<str>> = HashSet::new();
    for owner in &owners {
        let Some(scope) = out.index.scope_of.get(&owner.namespace) else {
            out.stats.unknown_namespace += 1;
            continue;
        };
        let depends_on = owner
            .watched
            .and_then(|s| s.controller.as_ref())
            .filter(|c| owner_slot.contains_key(&c.uid))
            .map(|c| vec![c.uid.clone()])
            .unwrap_or_default();
        let tool = match owner.watched.map(|s| &s.detail) {
            Some(Detail::Owner { tool }) => *tool,
            _ => ToolId::NONE,
        };
        out.events.push(IngestEvent::Resource(ResourceEvent {
            kind: owner.kind,
            uid: owner.uid.clone(),
            namespace: owner.namespace.clone(),
            name: owner.name.clone(),
            resource_version: owner.resource_version,
            parent: Some(scope.clone()),
            op: Op::Added,
            payload: Payload::Owner {
                kind: owner.kind,
                tool,
                depends_on,
            },
        }));
        emitted.insert(owner.uid.clone());
        out.stats.owners += 1;
    }

    for inst in &instances {
        let Some(parent) = parent_of.get(&inst.uid).filter(|p| emitted.contains(*p)) else {
            if parent_of.contains_key(&inst.uid) {
                out.stats.unknown_namespace += 1;
            }
            continue;
        };
        let Detail::Instance { reason, .. } = &inst.detail else {
            continue;
        };
        let state = State {
            severity: reason.severity,
            reason: catalog.intern_reason(&reason.display),
        };
        out.events.push(IngestEvent::Resource(ResourceEvent {
            kind: inst.kind,
            uid: inst.uid.clone(),
            namespace: inst.namespace.clone(),
            name: inst.name.clone(),
            resource_version: inst.resource_version,
            parent: Some(parent.clone()),
            op: Op::Added,
            payload: Payload::Instance { state },
        }));
        out.stats.instances += 1;
    }

    for att in &attachments {
        let Detail::Attached { detail, .. } = &att.detail else {
            continue;
        };
        let key = (att.kind, att.namespace.clone(), att.name.clone());
        let Some(parent) = attach_owner.get(&key).filter(|p| emitted.contains(*p)) else {
            out.stats.unattached += 1;
            continue;
        };
        out.events.push(IngestEvent::Resource(ResourceEvent {
            kind: att.kind,
            uid: att.uid.clone(),
            namespace: att.namespace.clone(),
            name: att.name.clone(),
            resource_version: att.resource_version,
            parent: Some(parent.clone()),
            op: Op::Added,
            payload: Payload::Attached {
                kind: att.kind,
                detail: detail.clone(),
            },
        }));
        out.stats.attachments += 1;
    }

    parent_of.retain(|_, owner| emitted.contains(owner));
    attach_owner.retain(|_, owner| emitted.contains(owner));
    out.index.attach_owner = attach_owner;
    out.index.parent_of = parent_of;
    out.index.owners = emitted;
    out
}

fn intern_reference(catalog: &mut Catalog, controller: &Controller) -> KindId {
    let (group, version) = split_api_version(&controller.api_version);
    catalog.intern_gvk_as(group, version, &controller.kind, Role::Owner)
}

pub fn split_api_version(api_version: &str) -> (&str, &str) {
    match api_version.split_once('/') {
        Some((group, version)) => (group, version),
        None => ("", api_version),
    }
}

#[cfg(test)]
#[path = "assemble_test.rs"]
mod tests;
