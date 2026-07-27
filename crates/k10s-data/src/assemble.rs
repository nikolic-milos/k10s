//! Turning a set of watched objects into a conforming initial sync.
//!
//! Watches arrive per kind, concurrently, in whatever order the API server lists
//! them. The contract wants the opposite: a hierarchy, parents before children,
//! every event an [`Op::Added`], and no child whose parent never arrived.
//! `k10s_world::input::fold` asserts exactly that in debug builds, so the streams
//! stage into a [`Store`] and this module emits once, in order.
//!
//! The joins that make it a hierarchy are the substance:
//!
//! - **A pod's parent is the workload, not the ReplicaSet.** Kubernetes puts a
//!   ReplicaSet between a Deployment and its pods, and showing it would double
//!   every Deployment on the map. The walk steps over it.
//! - **A controller we do not watch still becomes one card.** A pod owned by an
//!   Argo `Rollout` has an owner reference naming a kind, a name and an
//!   `apiVersion`, which is a GVK, which is enough to intern the kind and emit an
//!   owner. Falling back to "standalone pod" would scatter one workload across
//!   fifty cards.
//! - **An attachment's parent is the workload that uses it.** A ConfigMap sits
//!   under whatever mounts it, a Service under whatever its selector matches.
//!   Both are joins over the whole set, which is why they cannot happen while
//!   staging one object.
//!
//! Everything unplaceable is counted, never dropped quietly: a producer bug has
//! to show up as a number rather than as a quietly smaller cluster.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use k10s_core::{Catalog, IngestEvent, KindId, Op, Payload, ResourceEvent, Role, State, ToolId};

use crate::mapping::{AttachRef, Controller, Detail, Labels, Staged};

/// How far up an owner chain the walk goes before giving up.
///
/// Kubernetes chains are short (CronJob to Job to pod is the deepest built-in),
/// and a cycle in owner references is possible in a cluster someone has been
/// editing by hand. A bound turns that from a hang into a counted miss.
const MAX_OWNER_HOPS: usize = 8;

/// Prefix for the owner synthesised for a pod that has no controller at all.
///
/// A slash cannot appear in a Kubernetes uid, so this can never collide with one.
pub const STANDALONE_PREFIX: &str = "k10s:standalone/";

/// The reflector cache: every object we have seen, by uid.
///
/// This is what a reflector is, and holding it is what makes the joins possible.
/// It is also the largest thing the data plane owns, which is why staging keeps
/// labels only where a join needs them.
///
/// Deliberately unordered: emission order comes from a sort on
/// `(namespace, name, uid)`, which is total because uids are unique, so keeping an
/// insertion order here would only add a tombstone problem on every delete.
#[derive(Debug, Default)]
pub struct Store {
    objects: HashMap<Arc<str>, Staged>,
    /// Kinds watched only to resolve ownership, never emitted.
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
        self.objects.insert(staged.uid.clone(), staged);
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

    /// Every object of one role, in hash order.
    ///
    /// Never emitted in this order: every caller sorts. `(namespace, name, uid)` is
    /// a total order because a uid is unique, so the sort alone gives determinism
    /// and the store needs no insertion order of its own.
    fn by_role(&self, role: Role) -> impl Iterator<Item = &Staged> {
        self.objects.values().filter(move |s| s.role == role)
    }

    fn sorted_by_role(&self, role: Role) -> Vec<&Staged> {
        let mut out: Vec<&Staged> = self.by_role(role).collect();
        out.sort_by(|a, b| (&a.namespace, &a.name, &a.uid).cmp(&(&b.namespace, &b.name, &b.uid)));
        out
    }
}

/// What the assembly could not place.
///
/// Every field is a shape a real cluster has and this pass does not draw. A
/// nonzero count is information, not necessarily a bug.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssembleStats {
    pub scopes: u32,
    pub owners: u32,
    pub instances: u32,
    pub attachments: u32,
    /// Owners invented for a controller we do not watch, plus one per pod with no
    /// controller at all.
    pub synthetic_owners: u32,
    /// Objects in a namespace we never saw. Happens when the Namespace watch is
    /// forbidden and a namespaced kind is not.
    pub unknown_namespace: u32,
    /// Attachments nothing references. A ConfigMap no pod mounts has no owner to
    /// sit under, so it is invisible until the scene can hold a namespace-level
    /// attachment.
    pub unattached: u32,
    /// Owner chains longer than the walk bound, which means a cycle.
    pub owner_cycles: u32,
}

/// The resolved parents and the owners that got a card, kept so live events after
/// the initial sync can be placed without redoing the joins.
///
/// Only owners the sync emitted appear here. An object whose owner was dropped —
/// its namespace unreadable, so its card never arrived — has nothing to be named
/// under, and naming it anyway would put an orphan in the stream the first time it
/// changed.
#[derive(Debug, Default)]
pub struct Index {
    scope_of: HashMap<Arc<str>, Arc<str>>,
    attach_owner: HashMap<AttachKey, Arc<str>>,
    parent_of: HashMap<Arc<str>, Arc<str>>,
    owners: HashSet<Arc<str>>,
}

impl Index {
    /// The uid of a namespace by name.
    pub fn scope_uid(&self, namespace: &str) -> Option<&Arc<str>> {
        self.scope_of.get(namespace)
    }

    /// Whether the sync drew an owner card for this uid.
    ///
    /// The question the live phase cannot answer from a kind: a pass-through
    /// ReplicaSet is [`Role::Owner`] in the store whether or not it was promoted,
    /// so only the set of cards actually emitted says which one has something to
    /// update.
    pub fn emitted_owner(&self, uid: &str) -> bool {
        self.owners.contains(uid)
    }

    /// The owner an already-placed object sits under.
    pub fn parent_of(&self, uid: &str) -> Option<&Arc<str>> {
        self.parent_of.get(uid)
    }

    /// The owner an attachment sits under, by identity rather than uid, because a
    /// recreated ConfigMap keeps its name and loses its uid.
    pub fn attachment_owner(&self, kind: KindId, namespace: &str, name: &str) -> Option<&Arc<str>> {
        self.attach_owner
            .get(&(kind, Arc::from(namespace), Arc::from(name)))
    }
}

/// A conforming initial sync plus what it could not place.
#[derive(Debug, Default)]
pub struct Assembled {
    pub events: Vec<IngestEvent>,
    pub stats: AssembleStats,
    pub index: Index,
}

/// Where an instance's owner came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerOf {
    /// An owner we watch and emit anyway.
    Watched(Arc<str>),
    /// A pass-through object with nothing above it: a bare ReplicaSet. Emitted as
    /// an owner using the kind id we already hold.
    Promote(Arc<str>),
    /// A controller we do not watch, to be emitted from its owner reference.
    Reference(Controller),
    /// No controller at all: a standalone pod.
    Standalone,
    /// The chain cycled.
    Cyclic,
}

/// Walks up from a controller reference to the owner that should hold the
/// instance.
fn owner_for(store: &Store, controller: Option<&Controller>) -> OwnerOf {
    let Some(mut cur) = controller.cloned() else {
        return OwnerOf::Standalone;
    };
    for _ in 0..MAX_OWNER_HOPS {
        // A reference to something we do not hold: either a kind outside the
        // watch set, or one whose watch is forbidden. Either way the reference
        // carries enough to draw a card.
        let Some(found) = store.get(&cur.uid) else {
            return OwnerOf::Reference(cur);
        };
        if !store.is_pass_through(found.kind) {
            return if found.role == Role::Owner {
                OwnerOf::Watched(found.uid.clone())
            } else {
                // Controlled by something that is not an owner in our model.
                // Nothing sensible to parent it to, so it stands alone rather
                // than inventing a hierarchy.
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

/// Whether a set of labels satisfies a selector.
///
/// An empty selector matches nothing: a Service with no selector selects no pods,
/// and treating it as matching everything would attach every headless Service to
/// an arbitrary workload.
pub fn selector_matches(selector: &Labels, labels: &Labels) -> bool {
    if selector.is_empty() {
        return false;
    }
    selector
        .iter()
        .all(|(k, v)| labels.iter().any(|(lk, lv)| lk == k && lv == v))
}

/// Whether `candidate` beats what is already recorded.
///
/// A ConfigMap mounted by three workloads has one parent in a four-level scene.
/// Smallest uid is arbitrary but stable, which is the property that matters: an
/// unstable choice makes the map reshuffle for no reason.
fn prefer(existing: Option<&Arc<str>>, candidate: &Arc<str>) -> bool {
    match existing {
        None => true,
        Some(cur) => candidate < cur,
    }
}

/// An attachment's identity: kind, namespace, name. Not its uid, because a
/// recreated ConfigMap keeps its name and loses its uid, and a pod spec names it by
/// name.
type AttachKey = (KindId, Arc<str>, Arc<str>);

/// A label pair inside one namespace, which is what a Service selector is matched
/// against.
type LabelKey = (Arc<str>, Arc<str>, Arc<str>);

/// An owner to emit.
struct Emit<'a> {
    uid: Arc<str>,
    kind: KindId,
    name: Arc<str>,
    namespace: Arc<str>,
    resource_version: u64,
    /// The staged object behind it, absent for a synthesised owner.
    watched: Option<&'a Staged>,
}

/// Assembles the store into a conforming initial sync.
pub fn assemble(store: &Store, catalog: &mut Catalog) -> Assembled {
    let mut out = Assembled::default();

    // Scopes first, by name, so islands land in a stable order.
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

    // Resolve every instance's owner. This is also where owners we do not watch
    // are discovered, which is why it runs before owners are emitted.
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

    // What each attachment sits under, from what pods reference and from Service
    // selectors.
    let mut attach_owner: HashMap<AttachKey, Arc<str>> = HashMap::new();
    // Label pair to the pods carrying it. One namespace holding thousands of pods
    // is a shape real clusters have, and a selector join that scanned every pod
    // per Service would stall startup on it.
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

    // Owners, each under its scope.
    let mut emitted: HashSet<Arc<str>> = HashSet::new();
    for owner in &owners {
        let Some(scope) = out.index.scope_of.get(&owner.namespace) else {
            out.stats.unknown_namespace += 1;
            continue;
        };
        // An owner-to-owner dependency: a Job under a CronJob is the built-in
        // case. The endpoint must be an owner we are actually emitting.
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
            // Either no owner resolved, or the owner sat in a namespace we never
            // saw. Both counted, neither emitted: an orphan makes the world's
            // fold assert.
            if parent_of.contains_key(&inst.uid) {
                out.stats.unknown_namespace += 1;
            }
            continue;
        };
        let Detail::Instance { reason, .. } = &inst.detail else {
            continue;
        };
        // The one place a reason string becomes an id, on a single thread.
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

    // The index's invariant, applied here rather than where the parents are
    // resolved, because the resolution that lands on an owner nobody can see is
    // exactly what `unknown_namespace` and `unattached` count.
    parent_of.retain(|_, owner| emitted.contains(owner));
    attach_owner.retain(|_, owner| emitted.contains(owner));
    out.index.attach_owner = attach_owner;
    out.index.parent_of = parent_of;
    out.index.owners = emitted;
    out
}

/// Interns the kind an owner reference names.
///
/// An `ownerReferences` entry carries `apiVersion` and `kind`, which is a GVK, so
/// a controller we do not watch still gets a real [`KindId`] rather than a
/// placeholder. This is where an Argo `Rollout` becomes nameable with nobody
/// having compiled it in.
fn intern_reference(catalog: &mut Catalog, controller: &Controller) -> KindId {
    let (group, version) = split_api_version(&controller.api_version);
    catalog.intern_gvk_as(group, version, &controller.kind, Role::Owner)
}

/// Splits `group/version`. A bare `v1` is the core group.
pub fn split_api_version(api_version: &str) -> (&str, &str) {
    match api_version.split_once('/') {
        Some((group, version)) => (group, version),
        None => ("", api_version),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::Reason;
    use k10s_core::{ReasonId, Severity};

    /// The fake ReplicaSet id: a pass-through kind that is not a built-in, which
    /// also proves pass-through is not hard-coded to a compiled-in id.
    const RS: KindId = KindId(9_500);

    fn scope(uid: &str, name: &str) -> Staged {
        Staged {
            kind: KindId::NAMESPACE,
            role: Role::Scope,
            uid: uid.into(),
            namespace: Arc::from(""),
            name: name.into(),
            resource_version: 1,
            controller: None,
            detail: Detail::Scope,
        }
    }

    fn owner(uid: &str, ns: &str, name: &str, kind: KindId) -> Staged {
        Staged {
            kind,
            role: Role::Owner,
            uid: uid.into(),
            namespace: ns.into(),
            name: name.into(),
            resource_version: 2,
            controller: None,
            detail: Detail::Owner { tool: ToolId::NONE },
        }
    }

    fn ctrl(uid: &str, kind: &str, name: &str, api_version: &str) -> Controller {
        Controller {
            uid: uid.into(),
            kind: kind.into(),
            name: name.into(),
            api_version: api_version.into(),
        }
    }

    fn instance(uid: &str, ns: &str, name: &str, controller: Option<Controller>) -> Staged {
        Staged {
            kind: KindId::POD,
            role: Role::Instance,
            uid: uid.into(),
            namespace: ns.into(),
            name: name.into(),
            resource_version: 3,
            controller,
            detail: Detail::Instance {
                reason: Reason {
                    severity: Severity::Ok,
                    display: "Running".into(),
                },
                labels: Vec::new(),
                refs: Vec::new(),
            },
        }
    }

    fn with_detail(mut s: Staged, labels: Labels, refs: Vec<AttachRef>) -> Staged {
        s.detail = Detail::Instance {
            reason: Reason {
                severity: Severity::Ok,
                display: "Running".into(),
            },
            labels,
            refs,
        };
        s
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        let mut out: Labels = pairs
            .iter()
            .map(|(k, v)| (Arc::from(*k), Arc::from(*v)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn attached(uid: &str, ns: &str, name: &str, kind: KindId, selector: Labels) -> Staged {
        Staged {
            kind,
            role: Role::Attached,
            uid: uid.into(),
            namespace: ns.into(),
            name: name.into(),
            resource_version: 4,
            controller: None,
            detail: Detail::Attached {
                detail: "d".into(),
                selector,
            },
        }
    }

    fn replicaset(uid: &str, ns: &str, name: &str, controller: Option<Controller>) -> Staged {
        let mut s = owner(uid, ns, name, RS);
        s.controller = controller;
        s
    }

    fn store(objects: Vec<Staged>) -> Store {
        let mut s = Store::new(vec![RS]);
        for o in objects {
            s.apply(o);
        }
        s
    }

    fn resources(a: &Assembled) -> Vec<&ResourceEvent> {
        a.events
            .iter()
            .filter_map(|e| match e {
                IngestEvent::Resource(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    fn find<'a>(a: &'a Assembled, uid: &str) -> &'a ResourceEvent {
        resources(a)
            .into_iter()
            .find(|r| r.uid.as_ref() == uid)
            .unwrap_or_else(|| panic!("{uid} was not emitted"))
    }

    /// The property the world's fold asserts on.
    fn assert_conforming(a: &Assembled) {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for r in resources(a) {
            assert_eq!(r.op, Op::Added, "{} was not Added", r.uid);
            if let Some(p) = &r.parent {
                assert!(
                    seen.contains(&**p),
                    "{} arrived before its parent {p}",
                    r.uid
                );
            }
            assert!(seen.insert(&r.uid), "{} emitted twice", r.uid);
        }
    }

    #[test]
    fn a_deployment_pod_parents_to_the_deployment_not_the_replicaset() {
        // The join that makes a real cluster look like the map's model. Emitting
        // the ReplicaSet as an owner would double every Deployment.
        let s = store(vec![
            scope("ns-1", "prod"),
            owner("dep-1", "prod", "api", KindId::DEPLOYMENT),
            replicaset(
                "rs-1",
                "prod",
                "api-abc",
                Some(ctrl("dep-1", "Deployment", "api", "apps/v1")),
            ),
            instance(
                "pod-1",
                "prod",
                "api-abc-1",
                Some(ctrl("rs-1", "ReplicaSet", "api-abc", "apps/v1")),
            ),
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_conforming(&a);

        assert_eq!(resources(&a).len(), 3, "namespace, deployment, pod");
        assert_eq!(find(&a, "pod-1").parent.as_deref(), Some("dep-1"));
        assert!(resources(&a).iter().all(|r| r.uid.as_ref() != "rs-1"));
        assert_eq!(
            a.stats,
            AssembleStats {
                scopes: 1,
                owners: 1,
                instances: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_pod_owned_by_a_kind_we_do_not_watch_still_groups_under_one_card() {
        // An Argo Rollout, a KubeVirt VMI, any operator's CRD. One card per pod
        // would make a fifty-replica rollout look like fifty workloads.
        let s = store(vec![
            scope("ns-1", "prod"),
            instance(
                "pod-1",
                "prod",
                "web-1",
                Some(ctrl("ro-1", "Rollout", "web", "argoproj.io/v1alpha1")),
            ),
            instance(
                "pod-2",
                "prod",
                "web-2",
                Some(ctrl("ro-1", "Rollout", "web", "argoproj.io/v1alpha1")),
            ),
        ]);
        let mut catalog = Catalog::new();
        let a = assemble(&s, &mut catalog);
        assert_conforming(&a);

        let rollout = find(&a, "ro-1");
        assert_eq!(&*rollout.name, "web");
        assert!(
            !rollout.kind.is_builtin(),
            "a CRD kind, interned at runtime"
        );
        let entry = catalog.kind(rollout.kind).expect("interned");
        assert_eq!(&*entry.kind, "Rollout");
        assert_eq!(&*entry.group, "argoproj.io");
        assert_eq!(&*entry.version, "v1alpha1");
        assert_eq!(a.stats.synthetic_owners, 1, "one card for two pods");
        assert_eq!(a.stats.instances, 2);
        assert_eq!(find(&a, "pod-1").parent.as_deref(), Some("ro-1"));
        assert_eq!(find(&a, "pod-2").parent.as_deref(), Some("ro-1"));
    }

    #[test]
    fn a_standalone_pod_gets_its_own_card_rather_than_vanishing() {
        // A bare pod is a real thing people run, and dropping it makes the map
        // lie about what is in the namespace.
        let s = store(vec![
            scope("ns-1", "prod"),
            instance("pod-1", "prod", "debug", None),
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_conforming(&a);
        assert_eq!(resources(&a).len(), 3);
        let card = resources(&a)
            .into_iter()
            .find(|r| r.uid.starts_with(STANDALONE_PREFIX))
            .expect("a card for the standalone pod");
        assert_eq!(&*card.name, "debug");
        assert_eq!(card.kind, KindId::POD);
        // The real uid stays on the real object.
        assert_eq!(find(&a, "pod-1").parent.as_deref(), Some(&*card.uid));
        assert_eq!(a.stats.synthetic_owners, 1);
    }

    #[test]
    fn a_bare_replicaset_is_promoted_to_an_owner() {
        // Pass-through only makes sense when something sits above it; a
        // ReplicaSet created by hand has to hold its own pods.
        let s = store(vec![
            scope("ns-1", "prod"),
            replicaset("rs-1", "prod", "hand-rolled", None),
            instance(
                "pod-1",
                "prod",
                "hand-rolled-1",
                Some(ctrl("rs-1", "ReplicaSet", "hand-rolled", "apps/v1")),
            ),
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_conforming(&a);
        assert_eq!(find(&a, "pod-1").parent.as_deref(), Some("rs-1"));
        assert_eq!(find(&a, "rs-1").kind, RS);
        assert_eq!(a.stats.owners, 1);
    }

    #[test]
    fn a_job_depends_on_its_cronjob() {
        // The one owner-to-owner edge the built-in kinds produce, and the reason
        // `depends_on` is not always empty.
        let mut job = owner("job-1", "prod", "nightly-123", KindId::JOB);
        job.controller = Some(ctrl("cj-1", "CronJob", "nightly", "batch/v1"));
        let s = store(vec![
            scope("ns-1", "prod"),
            owner("cj-1", "prod", "nightly", KindId::CRON_JOB),
            job,
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_conforming(&a);
        let Payload::Owner { depends_on, .. } = &find(&a, "job-1").payload else {
            panic!("expected an owner payload")
        };
        assert_eq!(depends_on, &vec![Arc::<str>::from("cj-1")]);
    }

    #[test]
    fn an_attachment_sits_under_the_workload_that_uses_it() {
        // Mounted ConfigMaps, referenced Secrets, claimed volumes: the join that
        // gives attachments a home in a four-level scene.
        let pod = with_detail(
            instance(
                "pod-1",
                "prod",
                "api-1",
                Some(ctrl("dep-1", "Deployment", "api", "apps/v1")),
            ),
            labels(&[("app", "api")]),
            vec![
                AttachRef {
                    kind: KindId::CONFIG_MAP,
                    name: "api-config".into(),
                },
                AttachRef {
                    kind: KindId::SECRET,
                    name: "api-secret".into(),
                },
                AttachRef {
                    kind: KindId::VOLUME,
                    name: "api-data".into(),
                },
            ],
        );
        let s = store(vec![
            scope("ns-1", "prod"),
            owner("dep-1", "prod", "api", KindId::DEPLOYMENT),
            pod,
            attached("cm-1", "prod", "api-config", KindId::CONFIG_MAP, Vec::new()),
            attached("sec-1", "prod", "api-secret", KindId::SECRET, Vec::new()),
            attached("pvc-1", "prod", "api-data", KindId::VOLUME, Vec::new()),
            attached(
                "cm-2",
                "prod",
                "nobody-mounts-me",
                KindId::CONFIG_MAP,
                Vec::new(),
            ),
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_conforming(&a);

        assert_eq!(a.stats.attachments, 3);
        assert_eq!(
            a.stats.unattached, 1,
            "an unreferenced ConfigMap has no home"
        );
        for uid in ["cm-1", "sec-1", "pvc-1"] {
            assert_eq!(find(&a, uid).parent.as_deref(), Some("dep-1"), "{uid}");
        }
        assert_eq!(
            a.index
                .attachment_owner(KindId::SECRET, "prod", "api-secret")
                .map(|u| u.to_string()),
            Some("dep-1".to_string())
        );
    }

    #[test]
    fn a_service_attaches_to_the_workload_its_selector_matches() {
        let pod = with_detail(
            instance(
                "pod-1",
                "prod",
                "api-1",
                Some(ctrl("dep-1", "Deployment", "api", "apps/v1")),
            ),
            labels(&[("app", "api"), ("tier", "web")]),
            Vec::new(),
        );
        let s = store(vec![
            scope("ns-1", "prod"),
            owner("dep-1", "prod", "api", KindId::DEPLOYMENT),
            owner("dep-2", "prod", "worker", KindId::DEPLOYMENT),
            pod,
            attached(
                "svc-1",
                "prod",
                "api",
                KindId::SERVICE,
                labels(&[("app", "api")]),
            ),
            attached(
                "svc-2",
                "prod",
                "other",
                KindId::SERVICE,
                labels(&[("app", "absent")]),
            ),
            attached("svc-3", "prod", "headless", KindId::SERVICE, Vec::new()),
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_conforming(&a);

        assert_eq!(find(&a, "svc-1").parent.as_deref(), Some("dep-1"));
        assert_eq!(a.stats.attachments, 1);
        assert_eq!(a.stats.unattached, 2, "no selector match is no parent");
    }

    #[test]
    fn a_selector_must_match_every_pair_and_an_empty_one_matches_nothing() {
        let pod_labels = labels(&[("app", "api"), ("tier", "web")]);
        assert!(selector_matches(&labels(&[("app", "api")]), &pod_labels));
        assert!(selector_matches(
            &labels(&[("app", "api"), ("tier", "web")]),
            &pod_labels
        ));
        assert!(!selector_matches(
            &labels(&[("app", "api"), ("tier", "batch")]),
            &pod_labels
        ));
        assert!(!selector_matches(&labels(&[("app", "other")]), &pod_labels));
        assert!(
            !selector_matches(&Vec::new(), &pod_labels),
            "an empty selector selects nothing, which is what Kubernetes means"
        );
    }

    #[test]
    fn an_object_in_a_namespace_we_cannot_see_is_counted_not_emitted() {
        // The RBAC shape: pods readable, namespaces not. The world's fold asserts
        // on an orphan in debug, so this has to be dropped and counted.
        let s = store(vec![
            owner("dep-1", "prod", "api", KindId::DEPLOYMENT),
            instance(
                "pod-1",
                "prod",
                "api-1",
                Some(ctrl("dep-1", "Deployment", "api", "apps/v1")),
            ),
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_conforming(&a);
        assert!(a.events.is_empty());
        assert_eq!(a.stats.unknown_namespace, 2);
        assert_eq!(a.stats.owners, 0);
    }

    #[test]
    fn the_index_names_only_owners_the_sync_emitted() {
        // The index is what parents live events, so an entry naming an owner that
        // was never emitted is a promise to emit an orphan the first time the
        // object changes. Same RBAC shape as above: pods readable, namespaces not.
        let objects = |scoped: bool| {
            let mut out = vec![
                owner("dep-1", "prod", "api", KindId::DEPLOYMENT),
                with_detail(
                    instance(
                        "pod-1",
                        "prod",
                        "api-1",
                        Some(ctrl("dep-1", "Deployment", "api", "apps/v1")),
                    ),
                    Vec::new(),
                    vec![AttachRef {
                        kind: KindId::CONFIG_MAP,
                        name: "api-config".into(),
                    }],
                ),
                attached("cm-1", "prod", "api-config", KindId::CONFIG_MAP, Vec::new()),
            ];
            if scoped {
                out.push(scope("ns-1", "prod"));
            }
            out
        };

        // Both joins hold when the namespace is readable, which is what makes the
        // absences below mean something rather than passing on an empty index.
        let placed = assemble(&store(objects(true)), &mut Catalog::new());
        assert_eq!(placed.index.parent_of("pod-1").map(|u| &**u), Some("dep-1"));
        assert_eq!(
            placed
                .index
                .attachment_owner(KindId::CONFIG_MAP, "prod", "api-config")
                .map(|u| &**u),
            Some("dep-1")
        );

        let dropped = assemble(&store(objects(false)), &mut Catalog::new());
        assert!(dropped.events.is_empty(), "{:?}", dropped.events);
        assert!(dropped.index.parent_of("pod-1").is_none());
        assert!(
            dropped
                .index
                .attachment_owner(KindId::CONFIG_MAP, "prod", "api-config")
                .is_none()
        );
        // And the counts are the half that must not change: the joins did resolve,
        // onto an owner nobody can see.
        assert_eq!(dropped.stats.unknown_namespace, 2);
        assert_eq!(dropped.stats.unattached, 1);
    }

    #[test]
    fn the_index_names_the_promoted_replicaset_and_not_the_passed_through_one() {
        // The two ReplicaSets are indistinguishable by kind and by role, and only
        // one of them was drawn. The live phase has no other way to tell them apart,
        // and suppressing both would freeze the hand-rolled one's card.
        let s = store(vec![
            scope("ns-1", "prod"),
            owner("dep-1", "prod", "api", KindId::DEPLOYMENT),
            replicaset(
                "rs-1",
                "prod",
                "api-abc",
                Some(ctrl("dep-1", "Deployment", "api", "apps/v1")),
            ),
            replicaset("rs-2", "prod", "hand-rolled", None),
            instance(
                "pod-1",
                "prod",
                "api-abc-1",
                Some(ctrl("rs-1", "ReplicaSet", "api-abc", "apps/v1")),
            ),
            instance(
                "pod-2",
                "prod",
                "hand-rolled-1",
                Some(ctrl("rs-2", "ReplicaSet", "hand-rolled", "apps/v1")),
            ),
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_conforming(&a);
        assert!(a.index.emitted_owner("dep-1"));
        assert!(a.index.emitted_owner("rs-2"), "promoted, so it has a card");
        assert!(
            !a.index.emitted_owner("rs-1"),
            "passed through, so it has none"
        );
        // A uid that is on the map but is not an owner is not an owner card either.
        assert!(!a.index.emitted_owner("pod-1"));
        assert!(!a.index.emitted_owner("ns-1"));
    }

    #[test]
    fn an_owner_reference_cycle_is_bounded_rather_than_hanging() {
        // Possible in a cluster someone has been editing by hand, and a walk with
        // no bound is a hang rather than an error.
        let s = store(vec![
            scope("ns-1", "prod"),
            replicaset(
                "rs-a",
                "prod",
                "a",
                Some(ctrl("rs-b", "ReplicaSet", "b", "apps/v1")),
            ),
            replicaset(
                "rs-b",
                "prod",
                "b",
                Some(ctrl("rs-a", "ReplicaSet", "a", "apps/v1")),
            ),
            instance(
                "pod-1",
                "prod",
                "p",
                Some(ctrl("rs-a", "ReplicaSet", "a", "apps/v1")),
            ),
        ]);
        let a = assemble(&s, &mut Catalog::new());
        assert_eq!(a.stats.owner_cycles, 1);
        assert_eq!(a.stats.instances, 0);
        assert_conforming(&a);
    }

    #[test]
    fn the_reason_string_becomes_the_state_the_scene_carries() {
        let mut crash = instance("pod-1", "prod", "api-1", None);
        crash.detail = Detail::Instance {
            reason: Reason {
                severity: Severity::Err,
                display: "CrashLoopBackOff".into(),
            },
            labels: Vec::new(),
            refs: Vec::new(),
        };
        let mut catalog = Catalog::new();
        let a = assemble(&store(vec![scope("ns-1", "prod"), crash]), &mut catalog);
        let Payload::Instance { state } = find(&a, "pod-1").payload else {
            panic!("expected an instance")
        };
        assert_eq!(state.reason, ReasonId::CRASH_LOOP_BACK_OFF);
        assert_eq!(state.severity, Severity::Err);

        // A reason nobody compiled in keeps the severity the mapping decided,
        // which `State::of` could not do because `reason_severity` only knows
        // built-ins.
        let mut pull = instance("pod-2", "prod", "api-2", None);
        pull.detail = Detail::Instance {
            reason: Reason {
                severity: Severity::Err,
                display: "ErrImagePull".into(),
            },
            labels: Vec::new(),
            refs: Vec::new(),
        };
        let a = assemble(&store(vec![scope("ns-1", "prod"), pull]), &mut catalog);
        let Payload::Instance { state } = find(&a, "pod-2").payload else {
            panic!("expected an instance")
        };
        assert_eq!(state.severity, Severity::Err);
        assert_eq!(catalog.reason_display(state.reason), "ErrImagePull");
        assert_eq!(
            k10s_core::reason_severity(state.reason),
            Severity::Unknown,
            "and the static table still knows nothing about it, which is the point"
        );
    }

    #[test]
    fn assembling_the_same_objects_twice_gives_identical_output() {
        // Determinism is what makes a golden fixture possible, and what stops the
        // map reshuffling between two runs against the same cluster.
        let mut objects = vec![
            scope("ns-2", "staging"),
            scope("ns-1", "prod"),
            owner("dep-2", "prod", "worker", KindId::DEPLOYMENT),
            owner("dep-1", "prod", "api", KindId::DEPLOYMENT),
        ];
        for i in 0..8 {
            objects.push(instance(
                &format!("pod-{i}"),
                "prod",
                &format!("api-{i}"),
                Some(ctrl("dep-1", "Deployment", "api", "apps/v1")),
            ));
        }
        let a = assemble(&store(objects.clone()), &mut Catalog::new());
        objects.reverse();
        let b = assemble(&store(objects), &mut Catalog::new());
        assert_eq!(
            a.events, b.events,
            "order must not depend on insertion order"
        );
        assert_eq!(a.stats, b.stats);
    }

    #[test]
    fn a_shared_attachment_picks_one_owner_and_always_the_same_one() {
        let mut objects = vec![
            scope("ns-1", "prod"),
            owner("dep-a", "prod", "a", KindId::DEPLOYMENT),
            owner("dep-b", "prod", "b", KindId::DEPLOYMENT),
            owner("dep-c", "prod", "c", KindId::DEPLOYMENT),
            attached("cm-1", "prod", "shared", KindId::CONFIG_MAP, Vec::new()),
        ];
        for (i, dep) in ["dep-c", "dep-a", "dep-b"].iter().enumerate() {
            objects.push(with_detail(
                instance(
                    &format!("pod-{i}"),
                    "prod",
                    &format!("p-{i}"),
                    Some(ctrl(dep, "Deployment", dep, "apps/v1")),
                ),
                Vec::new(),
                vec![AttachRef {
                    kind: KindId::CONFIG_MAP,
                    name: "shared".into(),
                }],
            ));
        }
        let a = assemble(&store(objects.clone()), &mut Catalog::new());
        assert_eq!(find(&a, "cm-1").parent.as_deref(), Some("dep-a"));
        objects.reverse();
        let b = assemble(&store(objects), &mut Catalog::new());
        assert_eq!(find(&b, "cm-1").parent, find(&a, "cm-1").parent);
    }

    #[test]
    fn an_api_version_splits_into_group_and_version() {
        assert_eq!(split_api_version("apps/v1"), ("apps", "v1"));
        assert_eq!(split_api_version("v1"), ("", "v1"));
        assert_eq!(
            split_api_version("argoproj.io/v1alpha1"),
            ("argoproj.io", "v1alpha1")
        );
        assert_eq!(split_api_version(""), ("", ""));
    }

    #[test]
    fn a_removed_object_is_gone_from_the_next_assembly() {
        let mut s = store(vec![
            scope("ns-1", "prod"),
            owner("dep-1", "prod", "api", KindId::DEPLOYMENT),
        ]);
        assert_eq!(s.len(), 2);
        let removed = s.remove("dep-1").expect("the object was there");
        assert_eq!(&*removed.name, "api", "the caller gets what went away");
        assert_eq!(s.len(), 1);
        assert!(s.remove("dep-1").is_none(), "removing twice is a no-op");
        assert_eq!(assemble(&s, &mut Catalog::new()).stats.owners, 0);
    }

    #[test]
    fn a_uid_re_added_after_a_delete_appears_once() {
        // Kubernetes does not reuse uids, but the store must not depend on that:
        // yielding one object twice would double it on the map.
        let mut s = store(vec![scope("ns-1", "prod")]);
        s.apply(owner("dep-1", "prod", "api", KindId::DEPLOYMENT));
        s.remove("dep-1");
        s.apply(owner("dep-1", "prod", "api", KindId::DEPLOYMENT));
        let a = assemble(&s, &mut Catalog::new());
        assert_eq!(a.stats.owners, 1);
        assert_conforming(&a);
    }

    #[test]
    fn an_empty_store_assembles_to_an_empty_sync() {
        let s = store(Vec::new());
        assert!(s.is_empty());
        let a = assemble(&s, &mut Catalog::new());
        assert!(a.events.is_empty());
        assert_eq!(a.stats, AssembleStats::default());
    }
}
