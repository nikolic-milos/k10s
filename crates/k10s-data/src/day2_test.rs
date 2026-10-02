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
fn a_400_is_a_rejection_and_any_other_refusal_a_labelled_failure() {
    let Day2Outcome::Rejected { message } =
        classify(&api_error(400, "strict decoding error"), "rollout")
    else {
        panic!("a 400 is a rejection");
    };
    assert_eq!(message, "strict decoding error");

    let Day2Outcome::Failed { why } = classify(
        &api_error(404, "deployments.apps \"web\" not found"),
        "delete",
    ) else {
        panic!("a 404 is a labelled failure");
    };
    assert_eq!(why, "deployments.apps \"web\" not found");
}

// The lab's API server's answer to an eviction a budget refuses, verbatim.
fn recorded(json: &str) -> kube::Error {
    kube::Error::Api(Box::new(
        serde_json::from_str(json).expect("a recorded Status"),
    ))
}

// A budget is the server's DisruptionBudget cause and nothing else: not the
// 429, not the TooManyRequests reason, not any other cause.
#[test]
fn a_disruption_budget_is_found_only_in_the_servers_own_cause() {
    let refused = recorded(
        r#"{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure","message":"Cannot evict pod as it would violate the pod's disruption budget.","reason":"TooManyRequests","details":{"causes":[{"reason":"DisruptionBudget","message":"The disruption budget c1-pdb-probe needs 1 healthy pods and has 1 currently"}]},"code":429}"#,
    );
    let kube::Error::Api(status) = &refused else {
        unreachable!("recorded is an API status");
    };
    assert_eq!(
        disruption_budget(status),
        Some("The disruption budget c1-pdb-probe needs 1 healthy pods and has 1 currently")
    );
    let other_cause = recorded(
        r#"{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure","message":"too many requests","reason":"TooManyRequests","details":{"causes":[{"reason":"FieldValueInvalid","message":"not a budget"}]},"code":429}"#,
    );
    let kube::Error::Api(status) = &other_cause else {
        unreachable!("recorded is an API status");
    };
    assert_eq!(
        disruption_budget(status),
        None,
        "another cause is not a budget"
    );
    let throttled = api_error(429, "Too many requests, please try again later.\n");
    let kube::Error::Api(status) = &throttled else {
        unreachable!("api_error is an API status");
    };
    assert_eq!(disruption_budget(status), None, "a 429 alone is throttling");
    assert_eq!(
        classify(&throttled, "evict"),
        Day2Outcome::Failed {
            why: "Too many requests, please try again later.".to_string()
        },
        "throttling in the server's own words, trimmed"
    );
}

#[test]
fn an_answer_with_no_verdict_on_a_write_says_it_may_have_landed() {
    let gateway = kube::Error::Service(Box::new(crate::connect::NoVerdict("Timeout".to_string())));
    let Day2Outcome::Failed { why } = classify(&gateway, "scale") else {
        panic!("an answer with no verdict is a labelled failure");
    };
    assert_eq!(
        why,
        "the write may or may not have reached the cluster, so read the object again before \
         retrying: Timeout"
    );
}

#[test]
fn the_drains_pod_list_failing_says_nothing_was_sent() {
    for (error, words) in [
        (
            api_error(429, "Too many requests, please try again later."),
            "Too many requests, please try again later.",
        ),
        (api_error(504, "Timeout"), "Timeout"),
        // A read has no document for a 400 to reject.
        (
            api_error(400, "unable to parse requirement: invalid label value"),
            "unable to parse requirement: invalid label value",
        ),
    ] {
        let Day2Outcome::Failed { why } = classify(&error, POD_LIST) else {
            panic!("a failed pod list is a labelled failure");
        };
        // Our clause first, the server's words last and whole.
        assert_eq!(
            why,
            format!("the node's pods could not be read, so nothing was sent: {words}")
        );
    }
    let lost = kube::Error::ReadEvents(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "connection reset by peer",
    ));
    let Day2Outcome::Failed { why } = classify(&lost, POD_LIST) else {
        panic!("a lost pod list is a labelled failure");
    };
    assert!(why.contains("connection reset by peer"), "{why}");
    assert!(
        why.starts_with("the node's pods could not be read, so nothing was sent: "),
        "{why}"
    );
}

// A denied pod list is a denial naming what was denied, not a failure: the
// account cannot list pods on the node, which is its own fact to act on.
#[test]
fn a_denied_pod_list_is_a_denial_of_the_pod_list() {
    let denied = api_error(
        403,
        "pods is forbidden: User \"reader\" cannot list resource \"pods\" in API group \"\" at \
         the cluster scope",
    );
    assert_eq!(
        classify(&denied, POD_LIST),
        Day2Outcome::Denied {
            what: POD_LIST,
            why: "pods is forbidden: User \"reader\" cannot list resource \"pods\" in API group \
                  \"\" at the cluster scope"
                .to_string(),
        }
    );
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
    let Day2Outcome::Failed { why } = classify(&api_error(405, ""), "scale") else {
        panic!("a 405 is a labelled failure");
    };
    assert_eq!(why, "answered with status 405 and no message");
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
        why.starts_with("the write may or may not have reached the cluster"),
        "{why}"
    );
}

// A write whose credential could not be had never left the process, and
// says so rather than that it may have landed.
#[test]
fn a_write_that_never_left_says_nothing_was_sent() {
    let unsent = kube::Error::Auth(kube::client::AuthError::ExecPluginFailed);
    let Day2Outcome::Failed { why } = classify(&unsent, "scale") else {
        panic!("an unsent write is a labelled failure");
    };
    assert!(why.starts_with("nothing was sent: "), "{why}");
    assert!(!why.contains("may or may not"), "{why}");
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

// One eviction on the client every drain eviction takes, against a listener
// that answers these bytes and closes.
async fn evicted_with(raw: &'static [u8]) -> Result<(), Eviction> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("one connection");
        let mut head = [0u8; 4096];
        let _ = socket.read(&mut head).await;
        let _ = socket.write_all(raw).await;
    });
    let clients = crate::connect::clients_from(kube::Config::new(url.parse().expect("a URL")))
        .expect("clients");
    post_eviction(&clients.interactive, &pod_target(), "prod", "p", None).await
}

// A body cut after the headers, on the client every drain eviction takes, is
// a lost connection even after a 201: the interactive client reads the body
// inside its deadline, so the status goes with it, and the drain stops there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_cut_after_the_headers_is_a_lost_connection() {
    match evicted_with(b"HTTP/1.1 201 Created\r\ncontent-length: 64\r\n\r\n{").await {
        Err(Eviction::Unknown {
            lost: true, why, ..
        }) => {
            assert!(
                why.starts_with("error reading a body from connection"),
                "{why}"
            );
            assert!(!why.contains("HyperError"), "{why}");
            assert!(
                why.contains("end of file before message length reached"),
                "{why}"
            );
        }
        Err(other) => panic!(
            "a cut body is a lost connection: {:?}",
            other.into_outcome()
        ),
        Ok(()) => panic!("the status went with the body"),
    }
}
