use super::*;
use kube::core::Status;
use kube::discovery::{ApiCapabilities, ApiResource, Scope};

fn target(group: &str, version: &str, kind: &str, plural: &str, namespaced: bool) -> KindTarget {
    let mut catalog = k10s_core::Catalog::new();
    crate::discover::intern(
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

fn api_error(code: u16, message: &str) -> kube::Error {
    kube::Error::Api(Status::failure(message, "Failed").with_code(code).boxed())
}

#[test]
fn only_apps_v1_deployments_statefulsets_and_replicasets_have_a_scale_click() {
    assert!(scalable(&target(
        "apps",
        "v1",
        "Deployment",
        "deployments",
        true
    )));
    assert!(scalable(&target(
        "apps",
        "v1",
        "StatefulSet",
        "statefulsets",
        true
    )));
    assert!(scalable(&target(
        "apps",
        "v1",
        "ReplicaSet",
        "replicasets",
        true
    )));
    assert!(!scalable(&target(
        "apps",
        "v1",
        "DaemonSet",
        "daemonsets",
        true
    )));
    assert!(!scalable(&target("", "v1", "Pod", "pods", true)));
}

#[test]
fn pause_is_a_deployment_field_and_restart_covers_the_pod_template_kinds() {
    let deploy = target("apps", "v1", "Deployment", "deployments", true);
    let sts = target("apps", "v1", "StatefulSet", "statefulsets", true);
    let ds = target("apps", "v1", "DaemonSet", "daemonsets", true);
    let rs = target("apps", "v1", "ReplicaSet", "replicasets", true);
    assert!(pausable(&deploy));
    assert!(!pausable(&sts));
    assert!(restartable(&deploy) && restartable(&sts) && restartable(&ds));
    assert!(!restartable(&rs));
}

#[test]
fn delete_blast_names_a_namespace_a_node_or_one_object() {
    let ns = target("", "v1", "Namespace", "namespaces", false);
    match delete_blast(&ns, None, "prod") {
        Blast::Namespace { name } => assert_eq!(name, "prod"),
        other => panic!("a Namespace is a namespace blast: {other:?}"),
    }

    let node = target("", "v1", "Node", "nodes", false);
    match delete_blast(&node, None, "worker-1") {
        Blast::Node { name } => assert_eq!(name, "worker-1"),
        other => panic!("a Node is a node blast: {other:?}"),
    }

    let pod = target("", "v1", "Pod", "pods", true);
    match delete_blast(&pod, Some("prod"), "api-1") {
        Blast::Object {
            kind,
            namespace,
            name,
        } => {
            assert_eq!(kind, "Pod");
            assert_eq!(namespace.as_deref(), Some("prod"));
            assert_eq!(name, "api-1");
        }
        other => panic!("a Pod is one object: {other:?}"),
    }
}

#[test]
fn a_403_is_a_denial_that_keeps_the_servers_reason() {
    let denied = classify(
        &api_error(
            403,
            "admission webhook \"policy.example.com\" denied the request: no",
        ),
        "scale",
    );
    let Day2Outcome::Denied { what, why } = denied else {
        panic!("a 403 is a denial, not a failure");
    };
    assert_eq!(what, "scale");
    assert!(why.contains("admission webhook"), "{why}");
}

#[test]
fn a_400_is_a_rejection_and_a_500_is_a_labelled_failure() {
    let Day2Outcome::Rejected { message } =
        classify(&api_error(400, "strict decoding error"), "rollout")
    else {
        panic!("a 400 is a rejection");
    };
    assert_eq!(message, "strict decoding error");

    let Day2Outcome::Failed { why } = classify(&api_error(500, "etcd timed out"), "delete") else {
        panic!("a 500 is a labelled failure");
    };
    assert_eq!(why, "etcd timed out");
}

#[test]
fn a_429_on_an_eviction_names_the_pod_disruption_budget() {
    let Day2Outcome::Failed { why } = classify(
        &api_error(429, "Cannot evict pod as it would violate the pod's PDB"),
        "evict",
    ) else {
        panic!("a 429 is a labelled PDB refusal");
    };
    assert!(why.contains("PodDisruptionBudget"), "{why}");
    assert!(why.contains("Cannot evict pod"), "{why}");
}

#[test]
fn a_404_on_debug_is_an_absent_subresource_not_a_missing_pod() {
    let Day2Outcome::Failed { why } = classify(&api_error(404, "not found"), "debug") else {
        panic!("a 404 on debug is a labelled failure");
    };
    assert!(
        why.contains("ephemeralcontainers") && why.contains("too old"),
        "{why}"
    );
}

#[test]
fn a_status_with_no_message_names_its_code_rather_than_printing_nothing() {
    let Day2Outcome::Failed { why } = classify(&api_error(507, ""), "scale") else {
        panic!("a 507 is a labelled failure");
    };
    assert_eq!(why, "the API server refused the scale with status 507");
}

#[test]
fn an_error_that_never_reached_a_server_does_not_claim_the_write_did_not_happen() {
    let broken = kube::Error::ReadEvents(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "connection reset by peer",
    ));
    let Day2Outcome::Failed { why } = classify(&broken, "scale") else {
        panic!("a transport failure is a labelled failure");
    };
    assert!(why.contains("connection reset by peer"), "{why}");
    assert!(
        why.contains("may or may not have reached the cluster"),
        "{why}"
    );
}

#[test]
fn the_eviction_body_is_policy_v1_json_and_grace_is_optional() {
    assert_eq!(
        eviction_body("prod", "api-1", None),
        serde_json::json!({
            "apiVersion": "policy/v1",
            "kind": "Eviction",
            "metadata": { "name": "api-1", "namespace": "prod" },
        })
    );
    assert_eq!(
        eviction_body("prod", "api-1", Some(30)),
        serde_json::json!({
            "apiVersion": "policy/v1",
            "kind": "Eviction",
            "metadata": { "name": "api-1", "namespace": "prod" },
            "deleteOptions": { "gracePeriodSeconds": 30 },
        })
    );
}
