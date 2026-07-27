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

#[derive(Debug, Default)]
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
mod tests {
    use super::*;
    use crate::mapping::Reason;
    use k10s_core::{ReasonId, Severity};

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
        assert_eq!(find(&a, "pod-1").parent.as_deref(), Some(&*card.uid));
        assert_eq!(a.stats.synthetic_owners, 1);
    }

    #[test]
    fn a_bare_replicaset_is_promoted_to_an_owner() {
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
        assert_eq!(dropped.stats.unknown_namespace, 2);
        assert_eq!(dropped.stats.unattached, 1);
    }

    #[test]
    fn the_index_names_the_promoted_replicaset_and_not_the_passed_through_one() {
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
        assert!(!a.index.emitted_owner("pod-1"));
        assert!(!a.index.emitted_owner("ns-1"));
    }

    #[test]
    fn an_owner_reference_cycle_is_bounded_rather_than_hanging() {
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
