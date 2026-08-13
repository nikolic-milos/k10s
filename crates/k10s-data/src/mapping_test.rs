//! Reading a Kubernetes object into the model: which signal decides a pod's
//! state, that an unrecognised reason is unknown and never ok, and that
//! staging keeps exactly what the scene needs and nothing it does not.

use super::*;
use k8s_openapi::api::core::v1::{
    ContainerState, ContainerStateTerminated, ContainerStateWaiting, PodCondition, PodStatus,
};
use std::collections::BTreeMap;

fn meta(name: &str, uid: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.into()),
        uid: Some(uid.into()),
        namespace: Some("prod".into()),
        resource_version: Some("1234".into()),
        ..Default::default()
    }
}

fn labeled(name: &str, uid: &str, labels: &[(&str, &str)]) -> ObjectMeta {
    let mut m = meta(name, uid);
    m.labels = Some(
        labels
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    );
    m
}

fn waiting(reason: &str) -> ContainerStatus {
    ContainerStatus {
        name: "app".into(),
        ready: false,
        restart_count: 3,
        image: "img".into(),
        image_id: String::new(),
        state: Some(ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some(reason.into()),
                message: Some("some detail".into()),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn terminated(reason: &str, exit_code: i32, restarts: i32) -> ContainerStatus {
    ContainerStatus {
        name: "app".into(),
        ready: false,
        restart_count: restarts,
        image: "img".into(),
        image_id: String::new(),
        state: Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                reason: Some(reason.into()),
                exit_code,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn running(ready: bool) -> ContainerStatus {
    ContainerStatus {
        name: "app".into(),
        ready,
        restart_count: 0,
        image: "img".into(),
        image_id: String::new(),
        state: Some(ContainerState {
            running: Some(Default::default()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn pod(phase: &str, statuses: Vec<ContainerStatus>) -> Pod {
    Pod {
        metadata: meta("api-1", "uid-pod-1"),
        spec: None,
        status: Some(PodStatus {
            phase: Some(phase.into()),
            container_statuses: Some(statuses),
            ..Default::default()
        }),
    }
}

#[test]
fn crashloopbackoff_is_its_own_reason_at_error_severity() {
    let r = pod_reason(&pod("Running", vec![waiting("CrashLoopBackOff")]));
    assert_eq!(&*r.display, "CrashLoopBackOff");
    assert_eq!(r.severity, Severity::Err);
    let failed = pod_reason(&pod("Failed", vec![]));
    assert_ne!(r.display, failed.display);
    assert_eq!(failed.severity, Severity::Err);
}

#[test]
fn a_running_pod_with_a_broken_sidecar_is_not_running() {
    let r = pod_reason(&pod(
        "Running",
        vec![running(true), waiting("ImagePullBackOff")],
    ));
    assert_eq!(&*r.display, "ImagePullBackOff");
    assert_eq!(r.severity, Severity::Err);
}

#[test]
fn an_init_container_failure_beats_the_pod_phase() {
    let mut p = pod("Pending", vec![running(false)]);
    p.status.as_mut().unwrap().init_container_statuses =
        Some(vec![waiting("CreateContainerConfigError")]);
    let r = pod_reason(&p);
    assert_eq!(&*r.display, "CreateContainerConfigError");
    assert_eq!(
        r.severity,
        Severity::Err,
        "a compiled-in Err reason on an init container outranks a Pending phase"
    );
}

#[test]
fn a_completed_init_container_is_not_a_problem() {
    let mut p = pod("Running", vec![running(true)]);
    p.status.as_mut().unwrap().init_container_statuses = Some(vec![terminated("Completed", 0, 0)]);
    let r = pod_reason(&p);
    assert_eq!(&*r.display, "Running");
    assert_eq!(r.severity, Severity::Ok);
}

#[test]
fn a_finished_job_pod_is_ok_and_a_failed_one_is_not() {
    let done = pod_reason(&pod("Succeeded", vec![terminated("Completed", 0, 0)]));
    assert_eq!(&*done.display, "Succeeded");
    assert_eq!(done.severity, Severity::Ok);

    let oom = pod_reason(&pod("Failed", vec![terminated("OOMKilled", 137, 0)]));
    assert_eq!(&*oom.display, "OOMKilled");
    assert_eq!(oom.severity, Severity::Err);
}

#[test]
fn readiness_not_phase_decides_a_running_pod() {
    let ready = pod_reason(&pod("Running", vec![running(true)]));
    assert_eq!(&*ready.display, "Running");
    let not_ready = pod_reason(&pod("Running", vec![running(false)]));
    assert_eq!(&*not_ready.display, "NotReady");
    assert_eq!(not_ready.severity, Severity::Warn);

    let mut p = pod("Running", vec![running(false)]);
    p.status.as_mut().unwrap().conditions = Some(vec![PodCondition {
        type_: "Ready".into(),
        status: "True".into(),
        ..Default::default()
    }]);
    assert_eq!(&*pod_reason(&p).display, "Running");
}

#[test]
fn a_deleting_pod_is_terminating_whatever_its_containers_say() {
    let mut p = pod("Running", vec![running(true)]);
    p.metadata.deletion_timestamp = Some(
        serde_json::from_value(serde_json::json!("2024-01-01T00:00:00Z"))
            .expect("a timestamp the API server would send"),
    );
    let r = pod_reason(&p);
    assert_eq!(&*r.display, "Terminating");
    assert_eq!(r.severity, Severity::Warn);
}

#[test]
fn an_evicted_pod_reports_the_pod_level_reason() {
    let mut p = pod("Failed", vec![running(true)]);
    p.status.as_mut().unwrap().reason = Some("Evicted".into());
    let r = pod_reason(&p);
    assert_eq!(&*r.display, "Evicted");
    assert_eq!(r.severity, Severity::Err);
}

#[test]
fn a_pod_with_no_status_is_pending_and_one_with_no_phase_is_unknown() {
    let bare = Pod {
        metadata: meta("api-1", "uid-1"),
        spec: None,
        status: None,
    };
    assert_eq!(&*pod_reason(&bare).display, "Pending");

    let mut odd = pod("", vec![]);
    odd.status.as_mut().unwrap().phase = None;
    let r = pod_reason(&odd);
    assert_eq!(&*r.display, "Unknown");
    assert_eq!(r.severity, Severity::Unknown);
}

#[test]
fn an_unrecognised_reason_is_unknown_and_never_ok() {
    assert_eq!(severity_of_reason("SomeFutureReason"), Severity::Unknown);
    assert_eq!(severity_of_reason(""), Severity::Unknown);
    let r = pod_reason(&pod("Running", vec![waiting("SomeFutureReason")]));
    assert_eq!(
        &*r.display, "NotReady",
        "unknown is not unhealthy, so the phase answer stands"
    );

    for reason in [
        "CrashLoopBackOff",
        "ImagePullBackOff",
        "ErrImagePull",
        "InvalidImageName",
        "CreateContainerConfigError",
        "RunContainerError",
        "OOMKilled",
        "DeadlineExceeded",
        "Evicted",
        "NodeAffinity",
    ] {
        assert_eq!(severity_of_reason(reason), Severity::Err, "{reason}");
    }
}

#[test]
fn only_the_controlling_owner_reference_becomes_the_parent() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
    let mut m = meta("api-1", "uid-1");
    m.owner_references = Some(vec![
        OwnerReference {
            api_version: "v1".into(),
            kind: "Service".into(),
            name: "api".into(),
            uid: "uid-svc".into(),
            controller: Some(false),
            block_owner_deletion: None,
        },
        OwnerReference {
            api_version: "apps/v1".into(),
            kind: "ReplicaSet".into(),
            name: "api-abc".into(),
            uid: "uid-rs".into(),
            controller: Some(true),
            block_owner_deletion: None,
        },
    ]);
    let c = controller_of(&m).expect("a controller reference");
    assert_eq!(&*c.uid, "uid-rs");
    assert_eq!(&*c.kind, "ReplicaSet");
    assert_eq!(&*c.name, "api-abc");
    assert_eq!(
        &*c.api_version, "apps/v1",
        "the reference has to carry a GVK, so an unwatched controller is still nameable"
    );

    m.owner_references.as_mut().unwrap()[1].controller = None;
    m.owner_references.as_mut().unwrap()[0].controller = None;
    assert_eq!(controller_of(&m), None);
}

#[test]
fn resource_version_parses_or_reports_zero() {
    let mut m = meta("a", "u");
    assert_eq!(resource_version(&m), 1234);
    m.resource_version = Some("not-a-number".into());
    assert_eq!(resource_version(&m), 0);
    m.resource_version = None;
    assert_eq!(resource_version(&m), 0);
}

#[test]
fn vendors_are_recognised_from_the_labels_charts_actually_set() {
    let cases: &[(&[(&str, &str)], ToolId)] = &[
        (
            &[("app.kubernetes.io/name", "postgresql")],
            ToolId::POSTGRES,
        ),
        (&[("app.kubernetes.io/name", "redis")], ToolId::REDIS),
        (&[("app", "grafana")], ToolId::GRAFANA),
        (&[("k8s-app", "kube-prometheus-stack")], ToolId::PROMETHEUS),
        (&[("app.kubernetes.io/part-of", "argo-cd")], ToolId::ARGO_CD),
        (
            &[("app.kubernetes.io/name", "my-redis-primary")],
            ToolId::REDIS,
        ),
        (&[("app.kubernetes.io/name", "checkout-api")], ToolId::NONE),
        (&[("unrelated", "redis")], ToolId::NONE),
        (&[], ToolId::NONE),
    ];
    for (labels, want) in cases {
        let m = labeled("x", "u", labels);
        assert_eq!(tool_of(&m), *want, "labels {labels:?}");
    }
}

#[test]
fn a_vendor_never_lands_outside_the_compiled_in_table() {
    for value in ["totally-made-up", "", "none", "x", "13"] {
        let m = labeled("x", "u", &[("app", value)]);
        assert!(tool_of(&m).is_builtin(), "{value}");
    }
}

#[test]
fn a_pod_spec_yields_every_attachment_it_names_once() {
    let spec: PodSpec = serde_json::from_value(serde_json::json!({
            "containers": [{
                "name": "app",
                "envFrom": [
                    {"configMapRef": {"name": "app-config"}},
                    {"secretRef": {"name": "app-secret"}}
                ],
                "env": [
                    {"name": "PW", "valueFrom": {"secretKeyRef": {"name": "db-secret", "key": "password"}}},
                    {"name": "HOST", "valueFrom": {"configMapKeyRef": {"name": "app-config", "key": "host"}}},
                    {"name": "PLAIN", "value": "literal"}
                ]
            }],
            "initContainers": [{
                "name": "migrate",
                "envFrom": [{"secretRef": {"name": "db-secret"}}]
            }],
            "volumes": [
                {"name": "data", "persistentVolumeClaim": {"claimName": "data-pvc"}},
                {"name": "cfg", "configMap": {"name": "app-config"}},
                {"name": "tls", "secret": {"secretName": "tls-cert"}},
                {"name": "proj", "projected": {"sources": [
                    {"configMap": {"name": "trust-bundle"}},
                    {"secret": {"name": "token-secret"}},
                    {"serviceAccountToken": {"path": "token"}}
                ]}},
                {"name": "scratch", "emptyDir": {}}
            ],
            "imagePullSecrets": [{"name": "registry-creds"}]
        }))
        .expect("a pod spec the API server could send");

    let refs = attachment_refs(&spec, &AttachKinds::default());
    let names: Vec<(KindId, &str)> = refs.iter().map(|r| (r.kind, &*r.name)).collect();
    for want in [
        (KindId::CONFIG_MAP, "app-config"),
        (KindId::CONFIG_MAP, "trust-bundle"),
        (KindId::SECRET, "app-secret"),
        (KindId::SECRET, "db-secret"),
        (KindId::SECRET, "tls-cert"),
        (KindId::SECRET, "token-secret"),
        (KindId::SECRET, "registry-creds"),
        (KindId::VOLUME, "data-pvc"),
    ] {
        assert!(names.contains(&want), "missing {want:?} in {names:?}");
    }
    assert_eq!(
        names
            .iter()
            .filter(|(k, n)| *k == KindId::CONFIG_MAP && *n == "app-config")
            .count(),
        1
    );
    assert_eq!(names.len(), 8, "no phantom references: {names:?}");
}

#[test]
fn a_debug_container_names_the_secrets_it_reads_like_any_other() {
    let spec: PodSpec = serde_json::from_value(serde_json::json!({
        "containers": [{"name": "app"}],
        "ephemeralContainers": [{
            "name": "debugger",
            "envFrom": [{"secretRef": {"name": "debug-secret"}}],
            "env": [
                {"name": "CFG", "valueFrom": {"configMapKeyRef": {"name": "debug-config", "key": "k"}}}
            ]
        }]
    }))
    .expect("a pod spec with an ephemeral container");

    let refs = attachment_refs(&spec, &AttachKinds::default());
    let names: Vec<(KindId, &str)> = refs.iter().map(|r| (r.kind, &*r.name)).collect();
    assert_eq!(
        names,
        [
            (KindId::SECRET, "debug-secret"),
            (KindId::CONFIG_MAP, "debug-config")
        ],
        "an ephemeral container reaches the same objects the graph must draw"
    );
}

#[test]
fn a_service_carries_its_selector_and_a_readable_detail() {
    let svc: Service = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "api", "uid": "uid-svc", "namespace": "prod", "resourceVersion": "7"},
        "spec": {
            "type": "ClusterIP",
            "selector": {"app": "api", "tier": "web"},
            "ports": [{"port": 80}, {"port": 443}]
        }
    }))
    .expect("a service");
    let staged = stage_service(KindId::SERVICE, &svc).expect("staged");
    assert_eq!(staged.role, Role::Attached);
    let Detail::Attached { detail, selector } = &staged.detail else {
        panic!("expected an attachment")
    };
    assert_eq!(&**detail, "ClusterIP 80,443");
    assert_eq!(
        selector,
        &vec![
            (Arc::from("app"), Arc::from("api")),
            (Arc::from("tier"), Arc::from("web"))
        ],
        "selectors must be sorted so matching is order-free"
    );
}

#[test]
fn a_selectorless_service_is_staged_without_one() {
    let svc: Service = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "db", "uid": "uid-db", "namespace": "prod"},
        "spec": {"type": "ExternalName", "externalName": "db.example.com"}
    }))
    .expect("a service");
    let staged = stage_service(KindId::SERVICE, &svc).expect("staged");
    let Detail::Attached { detail, selector } = &staged.detail else {
        panic!("expected an attachment")
    };
    assert!(selector.is_empty());
    assert_eq!(&**detail, "ExternalName");
}

#[test]
fn a_claim_reports_its_bound_size_then_its_request() {
    let bound: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "data", "uid": "uid-pvc", "namespace": "prod"},
        "spec": {"resources": {"requests": {"storage": "8Gi"}}},
        "status": {"phase": "Bound", "capacity": {"storage": "10Gi"}}
    }))
    .expect("a claim");
    assert_eq!(&*pvc_detail(&bound), "10Gi");

    let pending: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "data", "uid": "uid-pvc", "namespace": "prod"},
        "spec": {"resources": {"requests": {"storage": "8Gi"}}},
        "status": {"phase": "Pending"}
    }))
    .expect("a claim");
    assert_eq!(&*pvc_detail(&pending), "8Gi");

    let staged = stage_pvc(KindId::VOLUME, &bound).expect("staged");
    assert!(matches!(staged.detail, Detail::Attached { .. }));
}

#[test]
fn a_secret_is_staged_from_metadata_and_carries_no_detail() {
    let staged = stage_meta(
        KindId::SECRET,
        Role::Attached,
        &labeled("db-password", "uid-secret", &[("app", "postgres")]),
    )
    .expect("staged");
    assert_eq!(staged.kind, KindId::SECRET);
    let Detail::Attached { detail, selector } = &staged.detail else {
        panic!("expected an attachment")
    };
    assert!(detail.is_empty(), "a secret detail must be empty");
    assert!(selector.is_empty());
    assert_eq!(&*staged.name, "db-password");
    assert_eq!(&*staged.namespace, "prod");
}

#[test]
fn an_object_without_a_uid_is_not_staged() {
    let mut m = meta("a", "u");
    m.uid = None;
    assert!(stage_meta(KindId::POD, Role::Instance, &m).is_none());
    m.uid = Some("u".into());
    m.name = None;
    assert!(stage_meta(KindId::POD, Role::Instance, &m).is_none());
}

#[test]
fn a_cluster_scoped_object_stages_with_an_empty_namespace() {
    let mut m = meta("prod", "uid-ns");
    m.namespace = None;
    let staged = stage_meta(KindId::NAMESPACE, Role::Scope, &m).expect("staged");
    assert!(staged.namespace.is_empty());
    assert_eq!(staged.detail, Detail::Scope);
}

#[test]
fn staging_a_pod_keeps_labels_sorted_for_selector_matching() {
    let mut p = pod("Running", vec![running(true)]);
    p.metadata.labels = Some(BTreeMap::from([
        ("tier".to_string(), "web".to_string()),
        ("app".to_string(), "api".to_string()),
    ]));
    let staged = stage_pod(KindId::POD, &AttachKinds::default(), &p).expect("staged");
    let Detail::Instance { labels, .. } = &staged.detail else {
        panic!("expected an instance")
    };
    assert_eq!(labels[0].0.as_ref(), "app");
    assert_eq!(labels[1].0.as_ref(), "tier");
}
