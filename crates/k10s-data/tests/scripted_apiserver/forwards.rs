//! Resolving a forward target through the pod or the service that fronts it,
//! and the two ways it fails: a portless pod is labelled, a denied one is a
//! denial.

use crate::*;

#[test]
fn a_forward_target_resolves_ports_from_the_pod_or_through_the_service() {
    use k10s_data::forward::ForwardRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        200,
        r#"{"metadata":{"name":"api-1","namespace":"prod","uid":"uid-pod-1"},
            "spec":{"containers":[{"name":"app","ports":[{"containerPort":8080,"name":"http"}]}]},
            "status":{"phase":"Running"}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/services/api",
        200,
        r#"{"metadata":{"name":"api","namespace":"prod","uid":"uid-svc"},
            "spec":{"selector":{"app":"api"},
                    "ports":[{"port":80,"targetPort":"http"}]}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod"},
             "spec":{"containers":[{"name":"app","ports":[{"containerPort":8080,"name":"http"}]}]},
             "status":{"phase":"Running"}}]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.resolve_forward(
        ForwardRequest {
            namespace: "prod".to_string(),
            name: "api-1".to_string(),
            service: false,
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Ok(spec) = wait(&rx) else {
        panic!("the pod target must resolve");
    };
    assert_eq!(spec.pod, "api-1");
    assert_eq!(
        (spec.local_port, spec.remote_port),
        (8080, 8080),
        "a pod forward uses its first declared containerPort on both ends"
    );

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.resolve_forward(
        ForwardRequest {
            namespace: "prod".to_string(),
            name: "api".to_string(),
            service: true,
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Ok(spec) = wait(&rx) else {
        panic!("the service target must resolve");
    };
    assert_eq!(spec.pod, "api-1", "resolved through the selector");
    assert_eq!(
        (spec.local_port, spec.remote_port),
        (80, 8080),
        "local is the service port, remote is the named targetPort on the pod"
    );
    let pod_scan = script
        .requests_for("labelSelector")
        .into_iter()
        .next()
        .expect("the service's pods were listed by selector");
    assert!(
        pod_scan.path.contains("limit=10"),
        "the pod scan is bounded: {}",
        pod_scan.path
    );

    drop(runtime);
}
#[test]
fn a_forward_to_a_portless_pod_is_labelled_and_a_denied_one_is_a_denial() {
    use k10s_data::forward::ForwardRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/bare",
        200,
        r#"{"metadata":{"name":"bare","namespace":"prod","uid":"uid-bare"},
            "spec":{"containers":[{"name":"app"}]},"status":{"phase":"Running"}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/api-1",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"pods is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let request = |name: &str| ForwardRequest {
        namespace: "prod".to_string(),
        name: name.to_string(),
        service: false,
    };

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .resolve_forward(request("bare"), move |outcome| {
            let _ = tx.send(outcome);
        });
    let Fetched::Failed { what, why } = wait(&rx) else {
        panic!("a portless pod is a labelled failure");
    };
    assert_eq!(what, "port-forward");
    assert!(why.contains("declares no containerPort"), "{why}");

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .resolve_forward(request("api-1"), move |outcome| {
            let _ = tx.send(outcome);
        });
    assert_eq!(wait(&rx), Fetched::Denied { what: "pod" });

    drop(runtime);
}
