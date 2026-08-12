//! What the access probe asks and what it concludes. The distinctions this
//! suite holds are the ones that decide whether a kind is shown as denied,
//! attempted, or absent -- and a review the server could not answer is none of
//! the three by default.

use crate::*;

const POD_IN_PROD_JSON: &str = r#"{"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod","resourceVersion":"900",
      "ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"api","uid":"uid-dep","controller":true}]},
    "status":{"phase":"Running","containerStatuses":[
      {"name":"app","ready":true,"restartCount":0,"image":"nginx","imageID":"","state":{"running":{}}}]}}"#;
fn script_unavailable_reviews_for_one_kind(script: &Script) {
    for _ in 0..2 {
        script.route(
            "POST",
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            503,
            r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":503,"reason":"ServiceUnavailable","message":"the server is currently unable to handle the request"}"#,
        );
    }
}
#[test]
fn the_probe_sends_one_rules_review_per_namespace_and_names_plural_resources() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],"resourceRules":[]}}"#,
    );
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let mut opts = options();
    opts.probe_namespaces = vec!["team-a".into(), "team-b".into()];
    let sync = run(&script, opts);

    let reviews = script.requests_for("selfsubjectrulesreviews");
    assert_eq!(reviews.len(), 2, "one per namespace, not one per kind");

    let access = script.requests_for("selfsubjectaccessreviews");
    assert_eq!(
        access.len(),
        20,
        "list and watch for each of ten watched kinds"
    );
    for request in reviews.iter().chain(access.iter()) {
        assert_eq!(request.method, "POST", "{request:?}");
    }
    assert!(sync.report.probe_requests >= 22);
}
#[test]
fn a_kind_the_probe_denies_is_forbidden_and_never_requested() {
    let script = Script::default();
    script_discovery(&script);
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],
                      "resourceRules":[{"apiGroups":[""],"resources":["pods"],"verbs":["list","watch"]}]}}"#,
    );
    script_access_reviews(&script, false, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        list(&[POD_IN_PROD_JSON.to_string()], "Pod"),
    );

    let sync = run(&script, options());

    assert_eq!(capability(&sync, KindId::POD), Some(Capability::Watchable));
    assert!(
        sync.report.namespaced_streams > 0,
        "a namespace-scoped grant must produce a namespace-scoped stream"
    );
    assert_eq!(
        capability(&sync, KindId::SECRET),
        Some(Capability::Forbidden)
    );
    assert_eq!(
        capability(&sync, KindId::DEPLOYMENT),
        Some(Capability::Forbidden)
    );
    assert_eq!(
        capability(&sync, KindId::NAMESPACE),
        Some(Capability::Forbidden)
    );

    assert!(
        script.requests_for("/secrets").is_empty(),
        "a denied kind must not be asked for"
    );
    assert!(!synced(&sync).contains(&KindId::SECRET));

    let pod_requests = script.requests_for("/pods");
    assert!(!pod_requests.is_empty());
    assert!(
        pod_requests
            .iter()
            .all(|r| r.path.contains("/namespaces/prod/pods")),
        "{pod_requests:?}"
    );

    assert_eq!(sync.report.assemble.scopes, 0);
    assert!(
        sync.report.assemble.unknown_namespace > 0,
        "an object in an unreadable namespace has to be counted: {:?}",
        sync.report.assemble
    );
    assert_eq!(sync.report.assemble.instances, 0);
    let (_, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
}
#[test]
fn a_failed_rules_review_keeps_its_namespace_attempted_rather_than_invisible() {
    let script = Script::default();
    script_discovery(&script);
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],
                      "resourceRules":[{"apiGroups":[""],"resources":["pods"],"verbs":["list","watch"]}]}}"#,
    );
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        503,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":503,"reason":"ServiceUnavailable","message":"etcdserver: request timed out"}"#,
    );
    script_access_reviews(&script, false, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/team-a/pods?",
        200,
        list(&[POD_IN_PROD_JSON.to_string()], "Pod"),
    );
    script.route(
        "GET",
        "/api/v1/namespaces/team-b/pods?",
        200,
        list(&[], "Pod"),
    );
    for (path, kind) in [
        ("/apis/apps/v1/namespaces/team-b/deployments?", "Deployment"),
        ("/apis/apps/v1/namespaces/team-b/replicasets?", "ReplicaSet"),
        ("/apis/batch/v1/namespaces/team-b/jobs?", "Job"),
        ("/apis/batch/v1/namespaces/team-b/cronjobs?", "CronJob"),
        ("/api/v1/namespaces/team-b/services?", "Service"),
        ("/api/v1/namespaces/team-b/configmaps?", "ConfigMap"),
        ("/api/v1/namespaces/team-b/secrets?", "Secret"),
        (
            "/api/v1/namespaces/team-b/persistentvolumeclaims?",
            "PersistentVolumeClaim",
        ),
    ] {
        script.route("GET", path, 200, list(&[], kind));
    }

    let mut opts = options();
    opts.probe_namespaces = vec!["team-a".into(), "team-b".into()];
    let sync = run(&script, opts);
    let report = &sync.report;

    assert_eq!(report.probed_namespaces, vec!["team-a", "team-b"]);
    assert_eq!(
        report.namespaces_unanswered,
        vec!["team-b"],
        "the report must name the namespace it is guessing about"
    );
    assert!(
        !report.probe_degraded,
        "one failed rules review out of two is a gap, not a dead probe"
    );

    assert_eq!(capability(&sync, KindId::POD), Some(Capability::Watchable));
    assert!(
        !script.requests_for("/namespaces/team-a/pods").is_empty(),
        "the granted namespace lists pods"
    );
    assert!(
        !script.requests_for("/namespaces/team-b/pods").is_empty(),
        "the unanswered namespace must be attempted, not silently dropped"
    );

    assert!(
        !script
            .requests_for("/namespaces/team-b/deployments")
            .is_empty(),
        "a kind the answered namespace denies is still attempted where no answer exists"
    );
    assert!(
        script
            .requests_for("/namespaces/team-a/deployments")
            .is_empty(),
        "the answered namespace's denial still gates"
    );
}
#[test]
fn a_kind_the_probe_could_not_answer_for_is_attempted_rather_than_denied() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_unavailable_reviews_for_one_kind(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);

    let sync = run(&script, options());
    let report = &sync.report;

    assert_eq!(report.kinds_unanswered, 1);
    assert!(!report.probe_degraded, "nine kinds were answered");
    assert_eq!(
        report.streams, 10,
        "every kind opens a stream, the unanswered one included"
    );
    assert_eq!(
        report.namespaced_streams, 0,
        "no answer is not a cluster-wide denial, so nothing falls back"
    );

    assert!(!script.requests_for("/api/v1/namespaces?").is_empty());
    assert_eq!(
        capability(&sync, KindId::NAMESPACE),
        Some(Capability::Watchable)
    );
    assert_eq!(report.assemble.scopes, 1);
    assert_eq!(report.assemble.instances, 2);
    let (_, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
}
#[test]
fn one_unanswered_kind_does_not_forbid_a_restricted_account_its_own_namespace() {
    let script = Script::default();
    script_discovery(&script);
    script.route(
        "POST",
        "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
        201,
        r#"{"kind":"SelfSubjectRulesReview","apiVersion":"authorization.k8s.io/v1","spec":{},
            "status":{"incomplete":false,"nonResourceRules":[],
                      "resourceRules":[{"apiGroups":[""],"resources":["pods"],"verbs":["list","watch"]}]}}"#,
    );
    script_unavailable_reviews_for_one_kind(&script);
    script_access_reviews(&script, false, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        list(&[POD_IN_PROD_JSON.to_string()], "Pod"),
    );

    let sync = run(&script, options());
    let report = &sync.report;

    assert!(
        !report.probe_degraded,
        "nine kinds were answered, and denied"
    );
    assert_eq!(
        capability(&sync, KindId::SECRET),
        Some(Capability::Forbidden)
    );
    assert!(script.requests_for("/secrets").is_empty());
    assert_eq!(report.kinds_unanswered, 1);
    assert_eq!(
        capability(&sync, KindId::NAMESPACE),
        Some(Capability::Watchable)
    );
    assert_eq!(capability(&sync, KindId::POD), Some(Capability::Watchable));
    let pod_requests = script.requests_for("/pods");
    assert!(!pod_requests.is_empty());
    assert!(
        pod_requests
            .iter()
            .all(|r| r.path.contains("/namespaces/prod/pods")),
        "a cluster-wide pod list is the 403 the probe already knew about: {pod_requests:?}"
    );
    assert_eq!(
        report.streams, 2,
        "the namespace attempt and the pod fallback, and nothing else"
    );
    assert_eq!(report.namespaced_streams, 1);

    assert_eq!(report.assemble.scopes, 1);
    assert_eq!(report.assemble.instances, 1);
    let (_, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
}
#[test]
fn a_403_on_the_list_becomes_a_labelled_desync_rather_than_a_retry_loop() {
    let denied = Script::default();
    script_discovery(&denied);
    script_rules_review(&denied);
    script_access_reviews(&denied, true, 32);
    denied.route(
        "GET",
        "/api/v1/secrets?",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"secrets is forbidden"}"#,
    );
    script_lists(&denied);

    let sync = run(&denied, options());

    assert!(
        sync.report
            .desyncs
            .iter()
            .any(|(kind, reason)| *kind == KindId::SECRET
                && *reason == k10s_core::DesyncReason::Forbidden),
        "a 403 has to surface as a Forbidden desync: {:?}",
        sync.report.desyncs
    );
    assert!(
        !k10s_core::DesyncReason::Forbidden.is_recoverable(),
        "and it must not look retryable"
    );
    assert_eq!(
        denied.requests_for("/secrets").len(),
        1,
        "a denied list must be attempted once"
    );
    assert!(!synced(&sync).contains(&KindId::SECRET));
    assert!(sync.report.assemble.instances >= 2);
    let (_, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
}
#[test]
fn a_server_that_does_not_serve_a_kind_makes_it_absent_rather_than_broken() {
    let script = Script::default();
    script.route_accepting("GET", "/apis", "APIGroupDiscoveryList", 406, "{}");
    script.route_accepting("GET", "/api", "APIGroupDiscoveryList", 406, "{}");
    script.route(
        "GET",
        "/apis",
        200,
        r#"{"kind":"APIGroupList","apiVersion":"v1","groups":[]}"#,
    );
    script.route(
        "GET",
        "/api",
        200,
        r#"{"kind":"APIVersions","versions":["v1"]}"#,
    );
    script.route(
        "GET",
        "/api/v1",
        200,
        format!(
            r#"{{"kind":"APIResourceList","groupVersion":"v1","resources":[{},{}]}}"#,
            api_resource("Namespace", "namespaces", false),
            api_resource("Pod", "pods", true),
        ),
    );
    script.route("GET", "/version", 404, "{}");
    script_rules_review(&script);
    script_access_reviews(&script, true, 8);
    script.route(
        "GET",
        "/api/v1/namespaces?",
        200,
        list(&[meta("prod", "uid-ns", None, "")], "Namespace"),
    );
    script.route("GET", "/api/v1/pods?", 200, list(&[], "Pod"));

    let sync = run(&script, options());
    assert_eq!(sync.report.kinds_discovered, 2);
    assert_eq!(sync.report.kinds_watched, 2);
    assert_eq!(sync.report.server_version, None, "no /version, no version");
    assert!(script.requests_for("/cronjobs").is_empty());
    assert!(script.requests_for("/deployments").is_empty());
    assert_eq!(synced(&sync).len(), 2);

    assert_eq!(sync.report.assemble.scopes, 1);
    assert_eq!(sync.report.assemble.instances, 0);
    let (input, stats) = k10s_world::input::fold(&sync.events);
    assert_eq!(stats, k10s_world::input::FoldStats::default());
    assert_eq!(input.namespaces.len(), 1);
    assert_eq!(input.total_pods, 0);
}
