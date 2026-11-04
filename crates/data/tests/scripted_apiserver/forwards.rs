//! Resolving a forward target through the pod or the service that fronts it,
//! and the three ways it fails: a portless pod is labelled, a nameless
//! selected pod is labelled before anything binds a socket, and a denied one
//! is a denial.

use crate::*;
use k10s_data::read::Fetched;

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
            service_port: None,
            expected_service: None,
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
            service_port: None,
            expected_service: None,
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
fn a_selected_service_port_resolves_its_own_named_target() {
    use k10s_data::forward::ForwardRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    let service = r#"{"metadata":{"name":"grafana","namespace":"prod"},
        "spec":{"selector":{"app":"grafana"},"ports":[
            {"name":"metrics","port":9090,"targetPort":19090},
            {"name":"same-number-udp","port":3000,"protocol":"UDP","targetPort":13001},
            {"name":"http","port":3000,"targetPort":"web"},
            {"name":"statsd","port":8125,"protocol":"UDP"}]}}"#;
    for _ in 0..3 {
        script.route(
            "GET",
            "/api/v1/namespaces/prod/services/grafana",
            200,
            service,
        );
    }
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"name":"grafana-1","namespace":"prod"},
             "spec":{"containers":[{"name":"grafana","ports":[
                {"name":"web","containerPort":13000}]}]},
             "status":{"phase":"Running"}}]}"#,
    );
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    for (port, expected) in [
        (3000, None),
        (9999, Some("no longer declares selected port 9999")),
        (8125, Some("selected port 8125 is not TCP")),
    ] {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.resolve_forward(
            ForwardRequest {
                namespace: "prod".into(),
                name: "grafana".into(),
                service: true,
                service_port: Some(port),
                expected_service: None,
            },
            move |outcome| {
                let _ = tx.send(outcome);
            },
        );
        match (wait(&rx), expected) {
            (Fetched::Ok(spec), None) => {
                assert_eq!(spec.pod, "grafana-1");
                assert_eq!((spec.local_port, spec.remote_port), (3000, 13000));
            }
            (Fetched::Failed { what, why }, Some(expected)) => {
                assert_eq!(what, "port-forward");
                assert!(why.contains(expected), "{why}");
            }
            (outcome, expected) => panic!("unexpected {outcome:?} for {expected:?}"),
        }
    }

    let services = script.requests_for("/api/v1/namespaces/prod/services/grafana");
    assert_eq!(services.len(), 3);
    for request in &services {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/api/v1/namespaces/prod/services/grafana");
        assert_eq!(request.accept, "");
        assert!(request.body_collected);
        assert!(request.body.is_empty());
    }
    let pods = script.requests_for("/api/v1/namespaces/prod/pods?");
    assert_eq!(pods.len(), 1, "invalid selected ports do not list pods");
    assert_eq!(pods[0].method, "GET");
    assert_eq!(
        pods[0].path.replacen("?&", "?", 1),
        "/api/v1/namespaces/prod/pods?labelSelector=app%3Dgrafana&limit=10"
    );
    assert_eq!(pods[0].accept, "");
    assert!(pods[0].body_collected);
    assert!(pods[0].body.is_empty());
    drop(runtime);
}

#[test]
fn forward_resolution_uses_tcp_and_kubernetes_target_port_rules() {
    use k10s_data::forward::ForwardRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    let services = [
        (
            "headless",
            r#"{"clusterIP":"None","ports":[{"port":18001,"targetPort":19001}]}"#,
        ),
        (
            "zero-target",
            r#"{"ports":[{"port":18002,"targetPort":0}]}"#,
        ),
        (
            "mixed",
            r#"{"ports":[{"name":"udp","protocol":"UDP","port":18003},{"name":"http","port":18004,"targetPort":"web"}]}"#,
        ),
        (
            "named-udp",
            r#"{"ports":[{"port":18005,"targetPort":"udp-only"}]}"#,
        ),
        ("udp-only", r#"{"ports":[{"protocol":"UDP","port":18006}]}"#),
        ("zero-service", r#"{"ports":[{"port":0}]}"#),
        (
            "bad-target",
            r#"{"ports":[{"port":18007,"targetPort":70000}]}"#,
        ),
    ];
    for (name, spec) in services {
        let mut spec: serde_json::Value = serde_json::from_str(spec).expect("service spec");
        spec["selector"] = serde_json::json!({"app":"demo"});
        let body = serde_json::json!({"metadata":{"name":name,"namespace":"prod"},"spec":spec});
        script.route(
            "GET",
            &format!("/api/v1/namespaces/prod/services/{name}"),
            200,
            body.to_string(),
        );
    }
    let pod = r#"{"metadata":{"name":"demo-1","namespace":"prod"},
        "spec":{"containers":[{"name":"app","ports":[
            {"name":"udp-only","containerPort":8125,"protocol":"UDP"},
            {"name":"web","containerPort":13001,"protocol":"UDP"},
            {"name":"web","containerPort":13000,"protocol":"TCP"}]}]},
        "status":{"phase":"Running"}}"#;
    for _ in 0..4 {
        script.route(
            "GET",
            "/api/v1/namespaces/prod/pods?",
            200,
            format!(r#"{{"apiVersion":"v1","kind":"PodList","metadata":{{}},"items":[{pod}]}}"#),
        );
    }
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/pod-mixed",
        200,
        r#"{"metadata":{"name":"pod-mixed"},"spec":{"containers":[{"name":"app","ports":[
            {"containerPort":8125,"protocol":"UDP"},{"containerPort":0},{"containerPort":8080}]}]},
            "status":{"phase":"Running"}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods/pod-udp",
        200,
        r#"{"metadata":{"name":"pod-udp"},"spec":{"containers":[{"name":"app","ports":[
            {"containerPort":8125,"protocol":"UDP"}]}]},"status":{"phase":"Running"}}"#,
    );
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    for (name, service, port, expected, failure) in [
        ("headless", true, Some(18001), Some((18001, 18001)), None),
        ("zero-target", true, None, Some((18002, 18002)), None),
        ("mixed", true, None, Some((18004, 13000)), None),
        (
            "named-udp",
            true,
            None,
            None,
            Some("no valid TCP container port named"),
        ),
        ("udp-only", true, None, None, Some("declares no TCP ports")),
        (
            "zero-service",
            true,
            None,
            None,
            Some("service port 0 is not a valid TCP port"),
        ),
        (
            "bad-target",
            true,
            None,
            None,
            Some("target port 70000 is not a valid TCP port"),
        ),
        ("pod-mixed", false, None, Some((8080, 8080)), None),
        (
            "pod-udp",
            false,
            None,
            None,
            Some("declares no valid TCP containerPort"),
        ),
    ] {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.resolve_forward(
            ForwardRequest {
                namespace: "prod".into(),
                name: name.into(),
                service,
                service_port: port,
                expected_service: None,
            },
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        match (wait(&rx), expected, failure) {
            (Fetched::Ok(spec), Some(expected), None) => {
                assert_eq!((spec.local_port, spec.remote_port), expected, "{name}");
            }
            (Fetched::Failed { what, why }, None, Some(expected)) => {
                assert_eq!(what, "port-forward");
                assert!(why.contains(expected), "{name}: {why}");
            }
            (answer, expected, failure) => {
                panic!("{name}: {answer:?}; expected {expected:?} or {failure:?}")
            }
        }
    }
    let mut requests = script.requests_for("/api/v1/namespaces/prod/services/");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/v1/namespaces/prod/services/headless",
            "/api/v1/namespaces/prod/services/zero-target",
            "/api/v1/namespaces/prod/services/mixed",
            "/api/v1/namespaces/prod/services/named-udp",
            "/api/v1/namespaces/prod/services/udp-only",
            "/api/v1/namespaces/prod/services/zero-service",
            "/api/v1/namespaces/prod/services/bad-target",
        ]
    );
    requests.extend(script.requests_for("/api/v1/namespaces/prod/pods?"));
    let direct_pods = script.requests_for("/api/v1/namespaces/prod/pods/pod-");
    assert_eq!(
        direct_pods
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/v1/namespaces/prod/pods/pod-mixed",
            "/api/v1/namespaces/prod/pods/pod-udp",
        ]
    );
    requests.extend(direct_pods);
    assert_eq!(requests.len(), 13, "invalid service ports do not list pods");
    for request in requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.accept, "");
        assert!(request.body_collected);
        assert!(request.body.is_empty());
        if request.path.contains("/pods?") {
            assert_eq!(
                request.path.replacen("?&", "?", 1),
                "/api/v1/namespaces/prod/pods?labelSelector=app%3Ddemo&limit=10"
            );
        }
    }
    drop(runtime);
}

#[test]
fn forwards_require_a_running_pod_within_the_bounded_selection() {
    use k10s_data::forward::ForwardRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    for (name, selector) in [
        ("pending", Some("pending")),
        ("mixed", Some("mixed")),
        ("empty", Some("empty")),
        ("without-selector", None),
    ] {
        let mut spec = serde_json::json!({"clusterIP":"None","ports":[{"port":18080}]});
        if let Some(selector) = selector {
            spec["selector"] = serde_json::json!({"app":selector});
        }
        script.route(
            "GET",
            &format!("/api/v1/namespaces/prod/services/{name}"),
            200,
            serde_json::json!({"metadata":{"name":name,"namespace":"prod"},"spec":spec})
                .to_string(),
        );
    }
    for (selector, items, metadata) in [
        (
            "pending",
            r#"[{"metadata":{"name":"waiting-a"},"status":{"phase":"Pending"}},
                {"metadata":{"name":"waiting-b"},"status":{"phase":"Pending"}}]"#,
            r#"{"continue":"more-pods"}"#,
        ),
        (
            "mixed",
            r#"[{"metadata":{"name":"waiting"},"status":{"phase":"Pending"}},
                {"metadata":{"name":"running"},"status":{"phase":"Running"}}]"#,
            "{}",
        ),
        ("empty", "[]", "{}"),
    ] {
        script.route(
            "GET",
            &format!("/api/v1/namespaces/prod/pods?labelSelector=app%3D{selector}&limit=10"),
            200,
            format!(
                r#"{{"apiVersion":"v1","kind":"PodList","metadata":{metadata},"items":{items}}}"#
            ),
        );
    }
    for (name, status) in [
        ("pod-pending", r#", "status":{"phase":"Pending"}"#),
        ("pod-unknown", ""),
        ("pod-running", r#", "status":{"phase":"Running"}"#),
    ] {
        script.route(
            "GET",
            &format!("/api/v1/namespaces/prod/pods/{name}"),
            200,
            format!(
                r#"{{"metadata":{{"name":"{name}"}},"spec":{{"containers":[{{"name":"app","ports":[{{"containerPort":18080}}]}}]}}{status}}}"#
            ),
        );
    }
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let resolve = |name: &str, service: bool| {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.resolve_forward(
            ForwardRequest {
                namespace: "prod".into(),
                name: name.into(),
                service,
                service_port: None,
                expected_service: None,
            },
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        wait(&rx)
    };

    for name in ["pending", "empty"] {
        let Fetched::Failed { what, why } = resolve(name, true) else {
            panic!("a service without a Running pod cannot resolve");
        };
        assert_eq!(what, "port-forward");
        assert_eq!(
            why,
            format!("service {name} has no Running pod among up to 10 selected pods")
        );
    }
    let Fetched::Ok(spec) = resolve("mixed", true) else {
        panic!("the later Running pod must be selected");
    };
    assert_eq!(spec.pod, "running");
    assert_eq!((spec.local_port, spec.remote_port), (18080, 18080));

    let Fetched::Failed { what, why } = resolve("without-selector", true) else {
        panic!("a selectorless service cannot select a pod");
    };
    assert_eq!(what, "port-forward");
    assert_eq!(
        why,
        "service without-selector has no selector, so this forward cannot choose a pod"
    );
    for (name, phase) in [("pod-pending", "Pending"), ("pod-unknown", "unknown")] {
        let Fetched::Failed { what, why } = resolve(name, false) else {
            panic!("a pod without a Running phase cannot resolve");
        };
        assert_eq!(what, "port-forward");
        assert_eq!(
            why,
            format!("pod {name} has no confirmed Running phase (phase: {phase})")
        );
    }
    let Fetched::Ok(spec) = resolve("pod-running", false) else {
        panic!("a Running pod with a TCP port must resolve");
    };
    assert_eq!(spec.pod, "pod-running");
    assert_eq!((spec.local_port, spec.remote_port), (18080, 18080));
    assert!(sync.reader.forwards().list().is_empty());

    let services = script.requests_for("/api/v1/namespaces/prod/services/");
    assert_eq!(services.len(), 4);
    for (request, name) in services
        .iter()
        .zip(["pending", "empty", "mixed", "without-selector"])
    {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path,
            format!("/api/v1/namespaces/prod/services/{name}")
        );
        assert_eq!(request.accept, "");
        assert!(request.body_collected && request.body.is_empty());
    }
    let pods = script.requests_for("/api/v1/namespaces/prod/pods?");
    assert_eq!(pods.len(), 3, "a selectorless service does not list pods");
    for (request, selector) in pods.iter().zip(["pending", "empty", "mixed"]) {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path.replacen("?&", "?", 1),
            format!("/api/v1/namespaces/prod/pods?labelSelector=app%3D{selector}&limit=10")
        );
        assert_eq!(request.accept, "");
        assert!(request.body_collected && request.body.is_empty());
    }
    let direct = script.requests_for("/api/v1/namespaces/prod/pods/pod-");
    assert_eq!(direct.len(), 3);
    for (request, name) in direct
        .iter()
        .zip(["pod-pending", "pod-unknown", "pod-running"])
    {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, format!("/api/v1/namespaces/prod/pods/{name}"));
        assert_eq!(request.accept, "");
        assert!(request.body_collected && request.body.is_empty());
    }
    drop(runtime);
}

#[test]
fn a_service_selecting_a_nameless_pod_is_labelled_at_resolve() {
    use k10s_data::forward::ForwardRequest;
    use k10s_data::read::Fetched;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/services/api",
        200,
        r#"{"metadata":{"name":"api","namespace":"prod","uid":"uid-svc"},
            "spec":{"selector":{"app":"api"},"ports":[{"port":80}]}}"#,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[
            {"metadata":{"uid":"uid-ghost","namespace":"prod"},
             "spec":{"containers":[{"name":"app","ports":[{"containerPort":8080}]}]},
             "status":{"phase":"Running"}}]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.resolve_forward(
        ForwardRequest {
            namespace: "prod".to_string(),
            name: "api".to_string(),
            service: true,
            service_port: None,
            expected_service: None,
        },
        move |outcome| {
            let _ = tx.send(outcome);
        },
    );
    let Fetched::Failed { what, why } = wait(&rx) else {
        panic!(
            "a resolution answering with an empty pod name would be forwarded to \
             /pods//portforward, so the failure is named here rather than at the upgrade"
        );
    };
    assert_eq!(what, "port-forward");
    assert!(why.contains("carries no name"), "{why}");

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
        service_port: None,
        expected_service: None,
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
    assert!(why.contains("declares no valid TCP containerPort"), "{why}");

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader
        .resolve_forward(request("api-1"), move |outcome| {
            let _ = tx.send(outcome);
        });
    assert_eq!(wait(&rx), Fetched::Denied { what: "pod" });

    drop(runtime);
}

#[test]
fn a_reviewed_service_origin_is_checked_before_pod_resolution_or_a_listener() {
    use k10s_data::forward::ForwardRequest;
    use std::sync::Arc;

    for (uid, version, matches_source) in [
        (Some("grafana-a"), Some("1"), true),
        (Some("grafana-b"), Some("1"), false),
        (Some("grafana-a"), Some("2"), false),
        (None, Some("1"), false),
        (Some("grafana-a"), None, false),
    ] {
        let script = Script::default();
        script_discovery(&script);
        script_rules_review(&script);
        script_access_reviews(&script, true, 32);
        script_lists(&script);
        let service = serde_json::json!({
            "metadata": {"name":"grafana", "namespace":"prod", "uid":uid, "resourceVersion":version},
            "spec": {"selector":{"app":"grafana"}, "ports":[{"name":"http", "port":3000, "targetPort":"web"}]}
        }).to_string();
        script.route(
            "GET",
            "/api/v1/namespaces/prod/services/grafana",
            200,
            &service,
        );
        if matches_source {
            script.route("GET", "/api/v1/namespaces/prod/pods?", 200,
                r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[
                  {"metadata":{"name":"grafana-pod","namespace":"prod","uid":"grafana-pod-uid"},
                   "spec":{"containers":[{"name":"grafana","ports":[{"name":"web","containerPort":3300}]}]},
                   "status":{"phase":"Running"}}]}"#);
        }
        let runtime = runtime();
        let (sync, _live) = sync_on(&runtime, &script);
        let source = Arc::new(k10s_core::ServiceIdentity {
            namespace: "prod".into(),
            name: "grafana".into(),
            port: 3000,
            uid: "grafana-a".into(),
            resource_version: "1".into(),
        });
        let request = ForwardRequest {
            namespace: "prod".into(),
            name: "grafana".into(),
            service: true,
            service_port: Some(3000),
            expected_service: Some(source.clone()),
        };
        if matches_source {
            let (tx, rx) = std::sync::mpsc::channel();
            sync.reader.resolve_forward(request, move |answer| {
                let _ = tx.send(answer);
            });
            let Fetched::Ok(spec) = wait(&rx) else {
                panic!("the reviewed origin must resolve");
            };
            assert_eq!(spec.pod, "grafana-pod");
            assert_eq!((spec.local_port, spec.remote_port), (3000, 3300));
            assert!(Arc::ptr_eq(
                spec.expected_service.as_ref().unwrap(),
                &source
            ));
        } else {
            let (tx, rx) = std::sync::mpsc::channel();
            let _stop = sync
                .reader
                .open_forward_cancellable(request, move |answer| {
                    let _ = tx.send(answer);
                });
            let Fetched::Failed { what, why } = wait(&rx) else {
                panic!("a stale origin must not open or reuse a forward");
            };
            assert_eq!(what, "service");
            assert_eq!(
                why,
                "the Service no longer matches the reviewed source; refresh the catalogue before forwarding"
            );
        }
        assert!(sync.reader.forwards().list().is_empty());
        let services = script.requests_for("/api/v1/namespaces/prod/services/grafana");
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].method, "GET");
        assert_eq!(services[0].path, "/api/v1/namespaces/prod/services/grafana");
        assert_eq!(services[0].accept, "");
        assert!(services[0].body_collected && services[0].body.is_empty());
        let pods = script.requests_for("/api/v1/namespaces/prod/pods?");
        assert_eq!(pods.len(), usize::from(matches_source));
        if let Some(request) = pods.first() {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.path.replacen("?&", "?", 1),
                "/api/v1/namespaces/prod/pods?labelSelector=app%3Dgrafana&limit=10"
            );
            assert_eq!(request.accept, "");
            assert!(request.body_collected && request.body.is_empty());
        }
        drop(runtime);
    }
}

#[test]
fn a_pinned_forward_cannot_change_its_requested_service_or_omit_identity() {
    use k10s_data::forward::ForwardRequest;
    use std::sync::Arc;

    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let original = k10s_core::ServiceIdentity {
        namespace: "prod".into(),
        name: "grafana".into(),
        port: 3000,
        uid: "grafana-a".into(),
        resource_version: "1".into(),
    };
    for identity in [
        k10s_core::ServiceIdentity {
            namespace: "other".into(),
            ..original.clone()
        },
        k10s_core::ServiceIdentity {
            name: "other".into(),
            ..original.clone()
        },
        k10s_core::ServiceIdentity {
            port: 3001,
            ..original.clone()
        },
        k10s_core::ServiceIdentity {
            uid: String::new(),
            ..original.clone()
        },
        k10s_core::ServiceIdentity {
            resource_version: String::new(),
            ..original
        },
    ] {
        let (tx, rx) = std::sync::mpsc::channel();
        sync.reader.resolve_forward(
            ForwardRequest {
                namespace: "prod".into(),
                name: "grafana".into(),
                service: true,
                service_port: Some(3000),
                expected_service: Some(Arc::new(identity)),
            },
            move |answer| {
                let _ = tx.send(answer);
            },
        );
        assert_eq!(
            wait(&rx),
            Fetched::Failed {
                what: "service",
                why: "the forward request does not match its reviewed Service identity".into(),
            }
        );
    }
    assert!(
        script
            .requests_for("/api/v1/namespaces/prod/services/grafana")
            .is_empty()
    );
    assert!(script.requests_for("labelSelector").is_empty());
    drop(runtime);
}
