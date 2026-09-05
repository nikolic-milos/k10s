//! Pod and workload usage over the wire: metrics-server first, the kubelet's
//! resource endpoint as the zero-install fallback, and the degradations --
//! a denial is final and never routed around, a cluster serving neither
//! source says Absent exactly once, and a kubelet answer that is not
//! exposition or exceeds its cap is a bounded failure. The requests are
//! asserted verbatim: what matters is what goes on the wire, not just what
//! comes back.

use crate::*;
use std::time::Duration;

use k10s_data::metrics::{
    Bytes, Millicores, UsageOutcome, UsageRequest, UsageSample, UsageSource, UsageTarget,
};

const POD_JSON: &str = r#"{"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod","resourceVersion":"900"},
    "spec":{"nodeName":"n1",
            "containers":[{"name":"app",
                           "resources":{"requests":{"cpu":"500m","memory":"64Mi"},
                                        "limits":{"cpu":"1","memory":"128Mi"}}}]},
    "status":{"phase":"Running"}}"#;

const POD_METRICS_JSON: &str = r#"{"kind":"PodMetrics","apiVersion":"metrics.k8s.io/v1beta1",
    "metadata":{"name":"api-1","namespace":"prod"},
    "timestamp":"2026-08-12T00:00:00Z","window":"15s",
    "containers":[{"name":"app","usage":{"cpu":"250m","memory":"32Mi"}}]}"#;

fn kubelet_text(cpu_seconds: f64, stamp_ms: i64, memory_bytes: u64) -> String {
    format!(
        "# HELP pod_cpu_usage_seconds_total Cumulative cpu time consumed by the pod\n\
         # TYPE pod_cpu_usage_seconds_total counter\n\
         pod_cpu_usage_seconds_total{{namespace=\"prod\",pod=\"api-1\"}} {cpu_seconds} {stamp_ms}\n\
         pod_memory_working_set_bytes{{namespace=\"prod\",pod=\"api-1\"}} {memory_bytes} {stamp_ms}\n"
    )
}

fn pod_request(interval: Duration) -> UsageRequest {
    UsageRequest {
        namespace: "prod".to_string(),
        target: UsageTarget::Pod {
            name: "api-1".to_string(),
        },
        interval,
    }
}

// A poll that must not tick twice inside a test gets an interval far past the
// wait budget; a poll that needs a second sample ticks fast.
const ONE_TICK: Duration = Duration::from_secs(60);
const FAST: Duration = Duration::from_millis(50);

fn poll_outcomes(
    sync: &Sync,
    request: UsageRequest,
) -> (
    k10s_data::metrics::UsageStop,
    std::sync::mpsc::Receiver<UsageOutcome>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = sync.reader.poll_usage(
        request,
        Box::new(move |outcome| {
            let _ = tx.send(outcome);
        }),
    );
    (stop, rx)
}

#[test]
fn a_pod_reads_its_usage_from_metrics_server_first() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    script.route(
        "GET",
        "/apis/metrics.k8s.io/v1beta1/namespaces/prod/pods/api-1",
        200,
        POD_METRICS_JSON,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(ONE_TICK));

    assert_eq!(
        wait(&rx),
        UsageOutcome::Usage(UsageSample {
            cpu: Some(Millicores(250)),
            memory: Some(Bytes(32 * 1024 * 1024)),
            cpu_request: Some(Millicores(500)),
            cpu_limit: Some(Millicores(1000)),
            memory_request: Some(Bytes(64 * 1024 * 1024)),
            memory_limit: Some(Bytes(128 * 1024 * 1024)),
            source: UsageSource::MetricsServer,
            pods_measured: 1,
            pods_total: 1,
            truncated: false,
        }),
        "usage, requests and limits all describe the same pod"
    );

    let metrics = script.requests_for("/apis/metrics.k8s.io");
    assert_eq!(metrics.len(), 1);
    assert_eq!(
        metrics[0].path, "/apis/metrics.k8s.io/v1beta1/namespaces/prod/pods/api-1",
        "the ask is namespaced to the pod, never a cluster-wide list"
    );
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "metrics-server answered, so no kubelet was consulted"
    );

    drop(runtime);
}

#[test]
fn a_cluster_without_metrics_server_is_carried_by_the_kubelet_two_samples_at_a_time() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    for _ in 0..3 {
        script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    }
    // metrics.k8s.io stays unscripted: the harness answers 404, which is what
    // a cluster that never installed metrics-server says.
    script.route(
        "GET",
        "/api/v1/nodes/n1/proxy/metrics/resource",
        200,
        kubelet_text(10.0, 1_700_000_001_000, 5 * 1024 * 1024),
    );
    script.route(
        "GET",
        "/api/v1/nodes/n1/proxy/metrics/resource",
        200,
        kubelet_text(12.5, 1_700_000_011_000, 6 * 1024 * 1024),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(FAST));

    let first = wait(&rx);
    let UsageOutcome::Usage(first) = first else {
        panic!("the kubelet carries the first tick: {first:?}");
    };
    assert_eq!(first.source, UsageSource::Kubelet);
    assert_eq!(
        first.cpu, None,
        "one cumulative counter is a baseline, not a rate; None is not zero"
    );
    assert_eq!(first.memory, Some(Bytes(5 * 1024 * 1024)));
    assert_eq!(first.cpu_request, Some(Millicores(500)));
    assert_eq!(first.pods_measured, 1);

    let second = wait(&rx);
    let UsageOutcome::Usage(second) = second else {
        panic!("the second sample yields the rate: {second:?}");
    };
    assert_eq!(
        second.cpu,
        Some(Millicores(250)),
        "2.5 core-seconds over the kubelet's own 10 seconds is 250m"
    );
    assert_eq!(second.memory, Some(Bytes(6 * 1024 * 1024)));

    let proxied = script.requests_for("/proxy/");
    assert!(!proxied.is_empty());
    assert_eq!(
        proxied[0].path, "/api/v1/nodes/n1/proxy/metrics/resource",
        "the fallback goes through the API server's node proxy, verbatim"
    );
    assert!(
        !script.requests_for("/apis/metrics.k8s.io").is_empty(),
        "and only after metrics.k8s.io was consulted and said not-found"
    );

    drop(runtime);
}

#[test]
fn an_unscraped_pod_on_a_served_group_stays_with_metrics_server_rather_than_flashing_kubelet_numbers()
 {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    // The pod's own metrics answer 404 -- metrics-server has not scraped it
    // yet -- but the group document says the API is served, so the poll must
    // report "not measured yet" instead of consulting the kubelet: two
    // sources alternating tick over tick would be numbers with no provenance.
    // The pod route is scripted explicitly (not left to the unscripted
    // default) because the group path is a prefix of the pod path and routes
    // match by prefix in registration order: a bare group route registered
    // alone would be eaten by the pod fetch.
    script.route(
        "GET",
        "/apis/metrics.k8s.io/v1beta1/namespaces/prod/pods/api-1",
        404,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"pods \"api-1\" not found"}"#,
    );
    script.route(
        "GET",
        "/apis/metrics.k8s.io/v1beta1",
        200,
        r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"metrics.k8s.io/v1beta1","resources":[]}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(ONE_TICK));

    let outcome = wait(&rx);
    let UsageOutcome::Usage(sample) = &outcome else {
        panic!("an unscraped pod is a sample that says so: {outcome:?}");
    };
    assert_eq!(sample.source, UsageSource::MetricsServer);
    assert_eq!(sample.cpu, None, "unmeasured is None, never zero");
    assert_eq!(sample.memory, None);
    assert_eq!((sample.pods_measured, sample.pods_total), (0, 1));
    assert_eq!(
        sample.cpu_request,
        Some(Millicores(500)),
        "the declared bounds still arrive; only the usage is pending"
    );
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "the kubelet is not consulted while metrics-server is the source"
    );

    drop(runtime);
}

#[test]
fn a_cluster_serving_neither_source_says_absent_once_and_is_not_retried() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    // Neither metrics.k8s.io nor the node proxy is scripted: both answer the
    // harness's 404.

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(FAST));

    let outcome = wait(&rx);
    let UsageOutcome::Absent { why } = &outcome else {
        panic!("neither source is served, which is absence: {outcome:?}");
    };
    assert!(
        why.contains("metrics-server is not installed") && why.contains("kubelet"),
        "the reason names both consulted sources: {why}"
    );

    // The poll is over: ticks at 50ms would have asked again many times over
    // by the end of this sleep if Absent were retried.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        script.requests_for("/proxy/").len(),
        1,
        "an absent kind is not retried"
    );
    assert_eq!(script.requests_for("/apis/metrics.k8s.io").len(), 2);
    assert!(
        rx.try_recv().is_err(),
        "and nothing further lands after the label"
    );

    drop(runtime);
}

#[test]
fn a_denied_metrics_api_is_denied_and_the_kubelet_is_not_asked_to_route_around_it() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    script.route(
        "GET",
        "/apis/metrics.k8s.io/v1beta1/namespaces/prod/pods/api-1",
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"podmetrics is forbidden"}"#,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(FAST));

    assert_eq!(
        wait(&rx),
        UsageOutcome::Denied {
            what: "pod metrics"
        },
        "a 403 is a labelled state, not an error string"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        script.requests_for("/proxy/").is_empty(),
        "a denial is an answer; the kubelet must not be asked to route around it"
    );
    assert_eq!(
        script.requests_for("/apis/metrics.k8s.io").len(),
        1,
        "and a denial is not retried"
    );

    drop(runtime);
}

#[test]
fn a_kubelet_answer_that_is_not_exposition_is_a_bounded_failure() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    script.route(
        "GET",
        "/api/v1/nodes/n1/proxy/metrics/resource",
        200,
        "<html>the proxy answered with someone else's page</html>",
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(ONE_TICK));

    let outcome = wait(&rx);
    let UsageOutcome::Failed { what, why } = &outcome else {
        panic!("a body that is not exposition is refused whole: {outcome:?}");
    };
    assert_eq!(*what, "node metrics");
    assert!(
        why.contains("not Prometheus text"),
        "the reason says what did not parse: {why}"
    );

    drop(runtime);
}

#[test]
fn a_kubelet_answer_past_the_byte_cap_is_refused_before_parsing() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    // Well-formed exposition, just too much of it: the cap must fire on size,
    // not on shape.
    let line = "pod_memory_working_set_bytes{namespace=\"prod\",pod=\"api-1\"} 4096\n";
    script.route(
        "GET",
        "/api/v1/nodes/n1/proxy/metrics/resource",
        200,
        line.repeat((2 << 20) / line.len() + 2),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(ONE_TICK));

    let outcome = wait(&rx);
    let UsageOutcome::Failed { what, why } = &outcome else {
        panic!("an oversized answer is refused before parsing: {outcome:?}");
    };
    assert_eq!(*what, "node metrics");
    assert!(why.contains("2 MiB"), "the reason names the cap: {why}");

    drop(runtime);
}

#[test]
fn a_metrics_api_that_never_answers_is_carried_by_the_kubelet_within_the_deadline() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    // A live aggregated API with a dead backend was observed holding the
    // request open forever; the connection staying open is exactly what
    // route_hanging is for.
    script.route_hanging(
        "GET",
        "/apis/metrics.k8s.io/v1beta1/namespaces/prod/pods/api-1",
    );
    script.route(
        "GET",
        "/api/v1/nodes/n1/proxy/metrics/resource",
        200,
        kubelet_text(10.0, 1_700_000_001_000, 5 * 1024 * 1024),
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(ONE_TICK));

    let outcome = wait(&rx);
    let UsageOutcome::Usage(sample) = &outcome else {
        panic!("a source that will not answer must not hold the panel: {outcome:?}");
    };
    assert_eq!(
        sample.source,
        UsageSource::Kubelet,
        "not answering within the deadline is the 503 class, and the kubelet is the answer"
    );
    assert_eq!(sample.memory, Some(Bytes(5 * 1024 * 1024)));

    drop(runtime);
}

#[test]
fn a_kubelet_that_never_answers_is_a_failure_naming_the_deadline() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route("GET", "/api/v1/namespaces/prod/pods/api-1", 200, POD_JSON);
    // metrics.k8s.io stays unscripted (404, not installed); the kubelet holds
    // its connection open instead of answering.
    script.route_hanging("GET", "/api/v1/nodes/n1/proxy/metrics/resource");

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let (_stop, rx) = poll_outcomes(&sync, pod_request(ONE_TICK));

    let outcome = wait(&rx);
    let UsageOutcome::Failed { what, why } = &outcome else {
        panic!("a kubelet that will not answer is a bounded failure: {outcome:?}");
    };
    assert_eq!(*what, "node metrics");
    assert!(
        why.contains("did not answer within 4 seconds"),
        "the reason names the deadline: {why}"
    );

    drop(runtime);
}

const DEPLOYMENT_JSON: &str = r#"{"metadata":{"name":"api","uid":"uid-dep","namespace":"prod","resourceVersion":"900"},
    "spec":{"replicas":2,"selector":{"matchLabels":{"app":"api"}}}}"#;

const WORKLOAD_PODS_JSON: &str = r#"{"kind":"PodList","apiVersion":"v1","metadata":{},"items":[
    {"metadata":{"name":"api-1","uid":"uid-pod-1","namespace":"prod","labels":{"app":"api"}},
     "spec":{"nodeName":"n1",
             "containers":[{"name":"app","resources":{"requests":{"cpu":"500m"},"limits":{"cpu":"1"}}}]},
     "status":{"phase":"Running"}},
    {"metadata":{"name":"api-2","uid":"uid-pod-2","namespace":"prod","labels":{"app":"api"}},
     "spec":{"nodeName":"n1",
             "containers":[{"name":"app","resources":{"requests":{"cpu":"500m"}}}]},
     "status":{"phase":"Running"}}
]}"#;

const WORKLOAD_METRICS_JSON: &str = r#"{"kind":"PodMetricsList","apiVersion":"metrics.k8s.io/v1beta1","items":[
    {"metadata":{"name":"api-1","namespace":"prod"},
     "containers":[{"name":"app","usage":{"cpu":"100m","memory":"32Mi"}}]},
    {"metadata":{"name":"api-2","namespace":"prod"},
     "containers":[{"name":"app","usage":{"cpu":"150m","memory":"32Mi"}}]}
]}"#;

#[test]
fn a_workload_sums_the_pods_its_own_selector_matches() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    script.route(
        "GET",
        "/apis/apps/v1/namespaces/prod/deployments/api",
        200,
        DEPLOYMENT_JSON,
    );
    script.route(
        "GET",
        "/api/v1/namespaces/prod/pods?",
        200,
        WORKLOAD_PODS_JSON,
    );
    script.route(
        "GET",
        "/apis/metrics.k8s.io/v1beta1/namespaces/prod/pods?",
        200,
        WORKLOAD_METRICS_JSON,
    );

    let runtime = runtime();
    let (sync, _live) = sync_on(&runtime, &script);
    let deployments = sync
        .reader
        .kinds()
        .into_iter()
        .find(|row| row.display == "deployments.apps")
        .expect("discovery served deployments");

    let (_stop, rx) = poll_outcomes(
        &sync,
        UsageRequest {
            namespace: "prod".to_string(),
            target: UsageTarget::Workload {
                kind: deployments.id,
                name: "api".to_string(),
            },
            interval: ONE_TICK,
        },
    );

    assert_eq!(
        wait(&rx),
        UsageOutcome::Usage(UsageSample {
            cpu: Some(Millicores(250)),
            memory: Some(Bytes(64 * 1024 * 1024)),
            cpu_request: Some(Millicores(1000)),
            cpu_limit: None,
            memory_request: None,
            memory_limit: None,
            source: UsageSource::MetricsServer,
            pods_measured: 2,
            pods_total: 2,
            truncated: false,
        }),
        "usage sums, requests sum, and one uncapped pod uncaps the workload"
    );

    let pod_list = &script.requests_for("/api/v1/namespaces/prod/pods?")[0];
    assert!(
        pod_list.path.contains("labelSelector=app%3Dapi"),
        "the workload's own selector goes on the wire: {}",
        pod_list.path
    );
    assert!(
        pod_list.path.contains("status.phase%21%3DSucceeded"),
        "terminated pods hold no requests and are excluded: {}",
        pod_list.path
    );
    assert!(
        pod_list.path.contains("limit=17"),
        "the pod list is bounded: {}",
        pod_list.path
    );
    let metrics_list = &script.requests_for("/apis/metrics.k8s.io/v1beta1/namespaces/prod/pods")[0];
    assert!(
        metrics_list.path.contains("labelSelector=app%3Dapi")
            && metrics_list.path.contains("limit=17"),
        "the metrics list asks the same bounded question: {}",
        metrics_list.path
    );

    drop(runtime);
}
