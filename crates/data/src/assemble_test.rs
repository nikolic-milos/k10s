//! What one assembly of the store promises: who parents whom, what is
//! counted rather than emitted, and that the same objects twice give the same
//! scene.
//!
//! The suite lives beside the assembler rather than inside it: a `#[cfg(test)]`
//! module compiles into no binary a benchmark or the app links, so moving it
//! out is free in a way moving the implementation demonstrably is not.

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
fn an_owner_chain_longer_than_the_hop_bound_stops_walking() {
    let hops = 10;
    let mut objects = vec![scope("ns-1", "prod")];
    for hop in 0..hops {
        let next = if hop + 1 == hops {
            None
        } else {
            Some(ctrl(
                &format!("rs-{}", hop + 1),
                "ReplicaSet",
                "r",
                "apps/v1",
            ))
        };
        objects.push(replicaset(&format!("rs-{hop}"), "prod", "r", next));
    }
    objects.push(instance(
        "pod-1",
        "prod",
        "p",
        Some(ctrl("rs-0", "ReplicaSet", "r", "apps/v1")),
    ));

    let a = assemble(&store(objects), &mut Catalog::new());
    assert_eq!(
        a.stats.owner_cycles, 1,
        "a chain that outruns the hop bound is refused the same way a loop is"
    );
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
