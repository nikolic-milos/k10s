//! Day-2 clicks against the scripted server: scale, rollout, delete, evict,
//! cordon, drain, debug. Each test pins method, path, content-type, and body.

use crate::*;
use k10s_data::day2::{
    self, Blast, Caps, CordonRequest, DEBUG_CONTAINER, Day2Outcome, DebugRequest, DeleteRequest,
    DrainRequest, EvictRequest, RESTART_ANNOTATION, RolloutAction, RolloutRequest, ScaleRequest,
};
use k10s_data::discover::{self, KindTarget};
use kube::discovery::{ApiCapabilities, ApiResource, Scope};

fn target(group: &str, version: &str, kind: &str, plural: &str, namespaced: bool) -> KindTarget {
    let mut catalog = k10s_core::Catalog::new();
    discover::intern(
        &mut catalog,
        ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: if group.is_empty() {
                version.to_string()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.to_string(),
            plural: plural.to_string(),
        },
        &ApiCapabilities {
            scope: if namespaced {
                Scope::Namespaced
            } else {
                Scope::Cluster
            },
            subresources: Vec::new(),
            operations: vec![
                "get".into(),
                "list".into(),
                "watch".into(),
                "patch".into(),
                "delete".into(),
                "create".into(),
            ],
        },
    )
}

fn patchable_target(
    group: &str,
    version: &str,
    kind: &str,
    plural: &str,
    namespaced: bool,
    patchable: bool,
) -> KindTarget {
    let mut t = target(group, version, kind, plural, namespaced);
    t.patchable = patchable;
    t
}

fn deploy() -> KindTarget {
    target("apps", "v1", "Deployment", "deployments", true)
}

fn pod() -> KindTarget {
    target("", "v1", "Pod", "pods", true)
}

fn node() -> KindTarget {
    target("", "v1", "Node", "nodes", false)
}

fn namespace_kind() -> KindTarget {
    target("", "v1", "Namespace", "namespaces", false)
}

fn patch_caps() -> Caps {
    Caps {
        patch: true,
        delete: false,
        create: false,
    }
}

fn delete_caps() -> Caps {
    Caps {
        patch: false,
        delete: true,
        create: false,
    }
}

fn create_caps() -> Caps {
    Caps {
        patch: false,
        delete: false,
        create: true,
    }
}

fn drain_caps() -> Caps {
    Caps {
        patch: true,
        delete: false,
        create: true,
    }
}

fn ok_json() -> &'static str {
    r#"{"kind":"Status","apiVersion":"v1","status":"Success"}"#
}

fn run<F, Fut, T>(script: &Script, f: F) -> T
where
    F: FnOnce(kube::Client) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let runtime = runtime();
    runtime.block_on(async { f(script.client()).await })
}

#[test]
fn a_scale_sends_a_merge_patch_to_the_apps_v1_scale_subresource() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/apps/v1/namespaces/prod/deployments/web/scale?",
        200,
        ok_json(),
    );

    let outcome = run(&script, |client| async move {
        day2::scale(
            &client,
            &deploy(),
            &ScaleRequest {
                namespace: Some("prod".to_string()),
                name: "web".to_string(),
                current: 3,
                replicas: 5,
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Applied(applied) = outcome else {
        panic!("a confirmed scale stores the replica count: {outcome:?}");
    };
    assert!(
        applied.summary.contains("from 3 to 5"),
        "{}",
        applied.summary
    );
    assert!(!applied.truncated);

    let writes = script.requests_for("/deployments/web/scale");
    assert_eq!(writes.len(), 1, "one scale is one request: {writes:?}");
    let write = &writes[0];
    assert_eq!(write.method, "PATCH");
    assert_eq!(
        write.path,
        "/apis/apps/v1/namespaces/prod/deployments/web/scale?"
    );
    assert_eq!(write.content_type, "application/merge-patch+json");
    assert_eq!(write.body, r#"{"spec":{"replicas":5}}"#);
}

#[test]
fn a_scale_without_confirm_never_reaches_the_wire_and_names_the_replica_delta() {
    let script = Script::default();
    let outcome = run(&script, |client| async move {
        day2::scale(
            &client,
            &deploy(),
            &ScaleRequest {
                namespace: Some("prod".to_string()),
                name: "web".to_string(),
                current: 3,
                replicas: 1,
                confirm: false,
                caps: patch_caps(),
            },
        )
        .await
    });
    let Day2Outcome::NeedsConfirm { blast, summary } = outcome else {
        panic!("without confirm the replica delta is the blast: {outcome:?}");
    };
    assert_eq!(blast, Blast::Replicas { from: 3, to: 1 });
    assert!(summary.contains("from 3 to 1"), "{summary}");
    assert!(
        script.seen().is_empty(),
        "nothing was sent: {:?}",
        script.seen()
    );
}

#[test]
fn a_scale_this_account_cannot_patch_never_fires() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/apps/v1/namespaces/prod/deployments/web/scale?",
        200,
        ok_json(),
    );
    let outcome = run(&script, |client| async move {
        day2::scale(
            &client,
            &deploy(),
            &ScaleRequest {
                namespace: Some("prod".to_string()),
                name: "web".to_string(),
                current: 1,
                replicas: 2,
                confirm: true,
                caps: Caps::default(),
            },
        )
        .await
    });
    let Day2Outcome::Denied { what, why } = outcome else {
        panic!("missing patch capability is a denial: {outcome:?}");
    };
    assert_eq!(what, "scale");
    assert!(why.contains("cannot patch"), "{why}");
    assert!(
        script.requests_for("/scale").is_empty(),
        "the denial is before the wire: {:?}",
        script.seen()
    );
}

#[test]
fn a_kind_with_no_patch_verb_is_not_a_permission_problem() {
    let script = Script::default();
    let secret = patchable_target("", "v1", "Secret", "secrets", true, false);
    let outcome = run(&script, |client| async move {
        day2::scale(
            &client,
            &secret,
            &ScaleRequest {
                namespace: Some("prod".to_string()),
                name: "api-token".to_string(),
                current: 1,
                replicas: 2,
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Failed { why } = outcome else {
        panic!("an unpatchable kind is a labelled failure: {outcome:?}");
    };
    assert!(why.contains("without a patch verb"), "{why}");
    assert!(script.seen().is_empty(), "{:?}", script.seen());
}

#[test]
fn a_rollout_restart_merge_patches_the_restarted_at_annotation() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/apps/v1/namespaces/prod/deployments/web?",
        200,
        ok_json(),
    );
    let at = "2026-08-13T16:31:00Z";
    let outcome = run(&script, |client| async move {
        day2::rollout(
            &client,
            &deploy(),
            &RolloutRequest {
                namespace: Some("prod".to_string()),
                name: "web".to_string(),
                action: RolloutAction::Restart {
                    restarted_at: at.to_string(),
                },
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    assert!(matches!(outcome, Day2Outcome::Applied(_)), "{outcome:?}");

    let writes = script.requests_for("/deployments/web?");
    assert_eq!(writes.len(), 1);
    let write = &writes[0];
    assert_eq!(write.method, "PATCH");
    assert_eq!(write.path, "/apis/apps/v1/namespaces/prod/deployments/web?");
    assert_eq!(write.content_type, "application/merge-patch+json");
    assert_eq!(
        write.body,
        format!(
            r#"{{"spec":{{"template":{{"metadata":{{"annotations":{{"{RESTART_ANNOTATION}":"{at}"}}}}}}}}}}"#
        )
    );
}

#[test]
fn pause_and_resume_are_spec_paused_merge_patches() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/apps/v1/namespaces/prod/deployments/web?",
        200,
        ok_json(),
    );
    script.route(
        "PATCH",
        "/apis/apps/v1/namespaces/prod/deployments/web?",
        200,
        ok_json(),
    );

    let pause = run(&script, |client| async move {
        day2::rollout(
            &client,
            &deploy(),
            &RolloutRequest {
                namespace: Some("prod".to_string()),
                name: "web".to_string(),
                action: RolloutAction::Pause,
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    assert!(matches!(pause, Day2Outcome::Applied(_)), "{pause:?}");

    let resume = run(&script, |client| async move {
        day2::rollout(
            &client,
            &deploy(),
            &RolloutRequest {
                namespace: Some("prod".to_string()),
                name: "web".to_string(),
                action: RolloutAction::Resume,
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    assert!(matches!(resume, Day2Outcome::Applied(_)), "{resume:?}");

    let writes = script.requests_for("/deployments/web?");
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].content_type, "application/merge-patch+json");
    assert_eq!(writes[0].body, r#"{"spec":{"paused":true}}"#);
    assert_eq!(writes[1].body, r#"{"spec":{"paused":false}}"#);
}

#[test]
fn rollout_undo_is_labelled_and_never_fakes_kubectl_or_helm() {
    let script = Script::default();
    let outcome = run(&script, |client| async move {
        day2::rollout(
            &client,
            &deploy(),
            &RolloutRequest {
                namespace: Some("prod".to_string()),
                name: "web".to_string(),
                action: RolloutAction::Undo,
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Failed { why } = outcome else {
        panic!("undo is a labelled refusal, not a guessed ReplicaSet: {outcome:?}");
    };
    assert!(why.contains("ReplicaSet controller history"), "{why}");
    assert!(why.contains("does not fake"), "{why}");
    assert!(script.seen().is_empty(), "{:?}", script.seen());
}

#[test]
fn a_delete_sends_grace_period_seconds_in_the_kube_delete_body() {
    let script = Script::default();
    script.route(
        "DELETE",
        "/api/v1/namespaces/prod/pods/api-1?",
        200,
        ok_json(),
    );
    let outcome = run(&script, |client| async move {
        day2::delete(
            &client,
            &pod(),
            &DeleteRequest {
                namespace: Some("prod".to_string()),
                name: "api-1".to_string(),
                grace_period_seconds: Some(30),
                confirm: true,
                caps: delete_caps(),
            },
        )
        .await
    });
    assert!(matches!(outcome, Day2Outcome::Applied(_)), "{outcome:?}");

    let writes = script.requests_for("/pods/api-1?");
    assert_eq!(writes.len(), 1);
    let write = &writes[0];
    assert_eq!(write.method, "DELETE");
    assert_eq!(write.path, "/api/v1/namespaces/prod/pods/api-1?");
    assert_eq!(write.content_type, "application/json");
    assert_eq!(write.body, r#"{"gracePeriodSeconds":30}"#);
}

#[test]
fn deleting_a_namespace_or_a_node_is_a_different_blast_from_one_namespaced_object() {
    let script = Script::default();

    let ns = run(&script, |client| async move {
        day2::delete(
            &client,
            &namespace_kind(),
            &DeleteRequest {
                namespace: None,
                name: "prod".to_string(),
                grace_period_seconds: None,
                confirm: false,
                caps: delete_caps(),
            },
        )
        .await
    });
    let Day2Outcome::NeedsConfirm { blast, summary } = ns else {
        panic!("{ns:?}");
    };
    assert_eq!(
        blast,
        Blast::Namespace {
            name: "prod".to_string()
        }
    );
    assert!(summary.contains("every namespaced object"), "{summary}");

    let node_blast = run(&script, |client| async move {
        day2::delete(
            &client,
            &node(),
            &DeleteRequest {
                namespace: None,
                name: "worker-1".to_string(),
                grace_period_seconds: None,
                confirm: false,
                caps: delete_caps(),
            },
        )
        .await
    });
    let Day2Outcome::NeedsConfirm { blast, summary } = node_blast else {
        panic!("{node_blast:?}");
    };
    assert_eq!(
        blast,
        Blast::Node {
            name: "worker-1".to_string()
        }
    );
    assert!(summary.contains("does not drain"), "{summary}");

    let pod_blast = run(&script, |client| async move {
        day2::delete(
            &client,
            &pod(),
            &DeleteRequest {
                namespace: Some("prod".to_string()),
                name: "api-1".to_string(),
                grace_period_seconds: None,
                confirm: false,
                caps: delete_caps(),
            },
        )
        .await
    });
    let Day2Outcome::NeedsConfirm { blast, .. } = pod_blast else {
        panic!("{pod_blast:?}");
    };
    assert_eq!(
        blast,
        Blast::Object {
            kind: "Pod".to_string(),
            namespace: Some("prod".to_string()),
            name: "api-1".to_string(),
        }
    );
    assert!(script.seen().is_empty(), "{:?}", script.seen());
}

#[test]
fn a_delete_this_account_cannot_make_never_fires() {
    let script = Script::default();
    script.route(
        "DELETE",
        "/api/v1/namespaces/prod/pods/api-1?",
        200,
        ok_json(),
    );
    let outcome = run(&script, |client| async move {
        day2::delete(
            &client,
            &pod(),
            &DeleteRequest {
                namespace: Some("prod".to_string()),
                name: "api-1".to_string(),
                grace_period_seconds: Some(0),
                confirm: true,
                caps: Caps::default(),
            },
        )
        .await
    });
    let Day2Outcome::Denied { what, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(what, "delete");
    assert!(script.seen().is_empty(), "{:?}", script.seen());
}

#[test]
fn an_evict_posts_policy_v1_eviction_json() {
    let script = Script::default();
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/api-1/eviction?",
        201,
        ok_json(),
    );
    let outcome = run(&script, |client| async move {
        day2::evict(
            &client,
            &pod(),
            &EvictRequest {
                namespace: "prod".to_string(),
                name: "api-1".to_string(),
                grace_period_seconds: None,
                confirm: true,
                caps: create_caps(),
            },
        )
        .await
    });
    assert!(matches!(outcome, Day2Outcome::Applied(_)), "{outcome:?}");

    let writes = script.requests_for("/pods/api-1/eviction");
    assert_eq!(writes.len(), 1);
    let write = &writes[0];
    assert_eq!(write.method, "POST");
    assert_eq!(write.path, "/api/v1/namespaces/prod/pods/api-1/eviction?");
    assert_eq!(write.content_type, "application/json");
    assert_eq!(
        write.body,
        r#"{"apiVersion":"policy/v1","kind":"Eviction","metadata":{"name":"api-1","namespace":"prod"}}"#
    );
}

#[test]
fn an_eviction_refused_by_a_pdb_is_labelled_not_retried() {
    let script = Script::default();
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/api-1/eviction?",
        429,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":429,"reason":"TooManyRequests","message":"Cannot evict pod as it would violate the pod's PDB"}"#,
    );
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/api-1/eviction?",
        201,
        ok_json(),
    );
    let outcome = run(&script, |client| async move {
        day2::evict(
            &client,
            &pod(),
            &EvictRequest {
                namespace: "prod".to_string(),
                name: "api-1".to_string(),
                grace_period_seconds: None,
                confirm: true,
                caps: create_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Failed { why } = outcome else {
        panic!("a 429 is a labelled PDB refusal: {outcome:?}");
    };
    assert!(why.contains("PodDisruptionBudget"), "{why}");
    let writes = script.requests_for("/eviction");
    assert_eq!(writes.len(), 1, "a 429 is not retried: {writes:?}");
}

#[test]
fn cordon_and_uncordon_merge_patch_node_unschedulable() {
    let script = Script::default();
    script.route("PATCH", "/api/v1/nodes/worker-1?", 200, ok_json());
    script.route("PATCH", "/api/v1/nodes/worker-1?", 200, ok_json());

    let cordon = run(&script, |client| async move {
        day2::cordon(
            &client,
            &node(),
            &CordonRequest {
                name: "worker-1".to_string(),
                unschedulable: true,
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    assert!(matches!(cordon, Day2Outcome::Applied(_)), "{cordon:?}");

    let uncordon = run(&script, |client| async move {
        day2::cordon(
            &client,
            &node(),
            &CordonRequest {
                name: "worker-1".to_string(),
                unschedulable: false,
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    assert!(matches!(uncordon, Day2Outcome::Applied(_)), "{uncordon:?}");

    let writes = script.requests_for("/api/v1/nodes/worker-1?");
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].content_type, "application/merge-patch+json");
    assert_eq!(writes[0].body, r#"{"spec":{"unschedulable":true}}"#);
    assert_eq!(writes[1].body, r#"{"spec":{"unschedulable":false}}"#);
}

fn pod_on(name: &str, namespace: &str, owner_kind: &str, phase: &str) -> String {
    format!(
        r#"{{"metadata":{{"name":"{name}","namespace":"{namespace}","ownerReferences":[{{"kind":"{owner_kind}","controller":true}}]}},"status":{{"phase":"{phase}"}}}}"#
    )
}

fn pod_list(items: &[String]) -> String {
    format!(
        r#"{{"kind":"PodList","apiVersion":"v1","metadata":{{}},"items":[{}]}}"#,
        items.join(",")
    )
}

#[test]
fn drain_cordons_then_evicts_skipping_daemonsets_unless_forced() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/pods?",
        200,
        pod_list(&[
            pod_on("api-1", "prod", "ReplicaSet", "Running"),
            pod_on("ds-1", "kube-system", "DaemonSet", "Running"),
            pod_on("done", "prod", "Job", "Succeeded"),
        ]),
    );
    script.route("PATCH", "/api/v1/nodes/worker-1?", 200, ok_json());
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/api-1/eviction?",
        201,
        ok_json(),
    );
    script.route(
        "POST",
        "/api/v1/namespaces/kube-system/pods/ds-1/eviction?",
        201,
        ok_json(),
    );

    let outcome = run(&script, |client| async move {
        day2::drain(
            &client,
            &node(),
            &DrainRequest {
                name: "worker-1".to_string(),
                force: false,
                confirm: true,
                caps: drain_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Applied(applied) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(applied.summary.contains("cordoned"), "{}", applied.summary);
    assert!(applied.summary.contains("DaemonSet"), "{}", applied.summary);
    assert!(!applied.truncated);

    let list = script.requests_for("/api/v1/pods?");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].method, "GET");
    assert!(
        list[0]
            .path
            .contains("fieldSelector=spec.nodeName%3Dworker-1"),
        "{}",
        list[0].path
    );

    let cordon = script.requests_for("/api/v1/nodes/worker-1?");
    assert_eq!(cordon.len(), 1);
    assert_eq!(cordon[0].method, "PATCH");
    assert_eq!(cordon[0].content_type, "application/merge-patch+json");
    assert_eq!(cordon[0].body, r#"{"spec":{"unschedulable":true}}"#);

    let evictions = script.requests_for("/eviction");
    assert_eq!(
        evictions.len(),
        1,
        "the DaemonSet pod is skipped: {evictions:?}"
    );
    assert_eq!(evictions[0].method, "POST");
    assert_eq!(
        evictions[0].path,
        "/api/v1/namespaces/prod/pods/api-1/eviction?"
    );
    assert_eq!(evictions[0].content_type, "application/json");
    assert_eq!(
        evictions[0].body,
        r#"{"apiVersion":"policy/v1","kind":"Eviction","metadata":{"name":"api-1","namespace":"prod"}}"#
    );
}

#[test]
fn drain_stops_at_sixteen_pods_and_says_so() {
    let items: Vec<String> = (0..17)
        .map(|i| pod_on(&format!("p{i}"), "prod", "ReplicaSet", "Running"))
        .collect();
    let script = Script::default();
    script.route("GET", "/api/v1/pods?", 200, pod_list(&items));
    script.route("PATCH", "/api/v1/nodes/worker-1?", 200, ok_json());
    for i in 0..16 {
        script.route(
            "POST",
            &format!("/api/v1/namespaces/prod/pods/p{i}/eviction?"),
            201,
            ok_json(),
        );
    }

    let outcome = run(&script, |client| async move {
        day2::drain(
            &client,
            &node(),
            &DrainRequest {
                name: "worker-1".to_string(),
                force: false,
                confirm: true,
                caps: drain_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Applied(applied) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(applied.truncated, "{}", applied.summary);
    assert_eq!(script.requests_for("/eviction").len(), 16);
    assert!(
        script.requests_for("/pods/p16/eviction").is_empty(),
        "the seventeenth pod is not touched: {:?}",
        script.seen()
    );
}

#[test]
fn a_pdb_429_during_drain_is_labelled_and_the_rest_of_the_press_continues() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/pods?",
        200,
        pod_list(&[
            pod_on("blocked", "prod", "ReplicaSet", "Running"),
            pod_on("ok", "prod", "ReplicaSet", "Running"),
        ]),
    );
    script.route("PATCH", "/api/v1/nodes/worker-1?", 200, ok_json());
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/blocked/eviction?",
        429,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":429,"reason":"TooManyRequests","message":"Cannot evict pod as it would violate the pod's PDB"}"#,
    );
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/ok/eviction?",
        201,
        ok_json(),
    );

    let outcome = run(&script, |client| async move {
        day2::drain(
            &client,
            &node(),
            &DrainRequest {
                name: "worker-1".to_string(),
                force: false,
                confirm: true,
                caps: drain_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Applied(applied) = outcome else {
        panic!("a PDB on one pod does not fail the drain: {outcome:?}");
    };
    assert!(
        applied.summary.contains("PodDisruptionBudget"),
        "{}",
        applied.summary
    );
    assert_eq!(
        script.requests_for("/pods/blocked/eviction").len(),
        1,
        "the 429 is not retried"
    );
    assert_eq!(script.requests_for("/pods/ok/eviction").len(), 1);
}

#[test]
fn drain_without_confirm_lists_for_blast_and_does_not_cordon_or_evict() {
    let script = Script::default();
    script.route(
        "GET",
        "/api/v1/pods?",
        200,
        pod_list(&[pod_on("api-1", "prod", "ReplicaSet", "Running")]),
    );
    script.route("PATCH", "/api/v1/nodes/worker-1?", 200, ok_json());

    let outcome = run(&script, |client| async move {
        day2::drain(
            &client,
            &node(),
            &DrainRequest {
                name: "worker-1".to_string(),
                force: false,
                confirm: false,
                caps: drain_caps(),
            },
        )
        .await
    });
    let Day2Outcome::NeedsConfirm { blast, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        blast,
        Blast::Drain {
            node: "worker-1".to_string(),
            pods: 1,
        }
    );
    assert_eq!(script.requests_for("/api/v1/pods?").len(), 1);
    assert!(
        script.requests_for("/nodes/worker-1").is_empty(),
        "cordon waits on confirm: {:?}",
        script.seen()
    );
    assert!(script.requests_for("/eviction").is_empty());
}

#[test]
fn drain_denied_for_missing_create_never_lists() {
    let script = Script::default();
    script.route("GET", "/api/v1/pods?", 200, pod_list(&[]));
    let outcome = run(&script, |client| async move {
        day2::drain(
            &client,
            &node(),
            &DrainRequest {
                name: "worker-1".to_string(),
                force: false,
                confirm: true,
                caps: patch_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Denied { what, why } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(what, "drain");
    assert!(why.contains("evict"), "{why}");
    assert!(script.seen().is_empty(), "{:?}", script.seen());
}

#[test]
fn debug_posts_an_ephemeral_container_named_k10s_debug() {
    let script = Script::default();
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/api-1/ephemeralcontainers?",
        200,
        ok_json(),
    );
    let outcome = run(&script, |client| async move {
        day2::debug(
            &client,
            &pod(),
            &DebugRequest {
                namespace: "prod".to_string(),
                name: "api-1".to_string(),
                image: "busybox".to_string(),
                confirm: true,
                caps: create_caps(),
            },
        )
        .await
    });
    assert!(matches!(outcome, Day2Outcome::Applied(_)), "{outcome:?}");

    let writes = script.requests_for("/ephemeralcontainers");
    assert_eq!(writes.len(), 1);
    let write = &writes[0];
    assert_eq!(write.method, "POST");
    assert_eq!(
        write.path,
        "/api/v1/namespaces/prod/pods/api-1/ephemeralcontainers?"
    );
    assert_eq!(write.content_type, "application/json");
    assert_eq!(
        write.body,
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "EphemeralContainers",
            "metadata": { "name": "api-1", "namespace": "prod" },
            "ephemeralContainers": [{
                "name": DEBUG_CONTAINER,
                "image": "busybox",
                "stdin": true,
                "tty": true,
            }],
        })
        .to_string()
    );
}

#[test]
fn a_missing_ephemeralcontainers_subresource_is_an_absent_cluster_not_a_missing_pod() {
    let script = Script::default();
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/api-1/ephemeralcontainers?",
        404,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"the server could not find the requested resource"}"#,
    );
    let outcome = run(&script, |client| async move {
        day2::debug(
            &client,
            &pod(),
            &DebugRequest {
                namespace: "prod".to_string(),
                name: "api-1".to_string(),
                image: "busybox".to_string(),
                confirm: true,
                caps: create_caps(),
            },
        )
        .await
    });
    let Day2Outcome::Failed { why } = outcome else {
        panic!("a 404 on the subresource is Absent-like: {outcome:?}");
    };
    assert!(why.contains("too old"), "{why}");
    assert!(why.contains("ephemeralcontainers"), "{why}");
}

#[test]
fn debug_without_create_never_fires() {
    let script = Script::default();
    script.route(
        "POST",
        "/api/v1/namespaces/prod/pods/api-1/ephemeralcontainers?",
        200,
        ok_json(),
    );
    let outcome = run(&script, |client| async move {
        day2::debug(
            &client,
            &pod(),
            &DebugRequest {
                namespace: "prod".to_string(),
                name: "api-1".to_string(),
                image: "busybox".to_string(),
                confirm: true,
                caps: Caps::default(),
            },
        )
        .await
    });
    let Day2Outcome::Denied { what, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(what, "debug");
    assert!(script.seen().is_empty(), "{:?}", script.seen());
}
