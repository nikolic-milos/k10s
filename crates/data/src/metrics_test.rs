//! The typed units and their human rendering, the request and limit sums and
//! the refusals inside them, the bounded kubelet text parser, the two-sample
//! rate decision, and the fallback-order decision on the wire's own errors.

use std::collections::HashMap;

use super::*;
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

#[test]
fn millicores_render_sub_core_as_millis_and_cores_trimmed() {
    assert_eq!(Millicores(0).to_string(), "0m");
    assert_eq!(Millicores(250).to_string(), "250m");
    assert_eq!(Millicores(999).to_string(), "999m");
    assert_eq!(Millicores(1000).to_string(), "1 core");
    assert_eq!(Millicores(1250).to_string(), "1.25 cores");
    assert_eq!(Millicores(1500).to_string(), "1.5 cores");
    assert_eq!(Millicores(2000).to_string(), "2 cores");
    assert_eq!(Millicores(12_340).to_string(), "12.34 cores");
}

#[test]
fn bytes_render_through_the_binary_ladder() {
    assert_eq!(Bytes(0).to_string(), "0");
    assert_eq!(Bytes(512).to_string(), "512");
    assert_eq!(Bytes(800 * 1024).to_string(), "800Ki");
    assert_eq!(Bytes(512 * 1024 * 1024).to_string(), "512Mi");
    assert_eq!(Bytes(16 * 1024 * 1024 * 1024).to_string(), "16.0Gi");
    assert_eq!(
        Bytes(1024 * 1024 * 1024 + 512 * 1024 * 1024).to_string(),
        "1.5Gi"
    );
    // The rungs themselves: each boundary value must climb, not sit under
    // the rung below it (a survived >= mutant is why these exist).
    assert_eq!(Bytes(1024).to_string(), "1Ki");
    assert_eq!(Bytes(1024 * 1024).to_string(), "1Mi");
    assert_eq!(Bytes(1024 * 1024 * 1024).to_string(), "1.0Gi");
}

#[test]
fn typed_parsing_reuses_the_quantity_grammar_and_refuses_negatives() {
    assert_eq!(Millicores::parse("1500m"), Some(Millicores(1500)));
    assert_eq!(Millicores::parse("2"), Some(Millicores(2000)));
    assert_eq!(Millicores::parse("156340764n"), Some(Millicores(156)));
    assert_eq!(Bytes::parse("64Mi"), Some(Bytes(64 * 1024 * 1024)));
    assert_eq!(Bytes::parse("5.24288e+06"), Some(Bytes(5_242_880)));
    assert_eq!(Millicores::parse("-1"), None, "usage cannot be negative");
    assert_eq!(Bytes::parse("-64Mi"), None);
    assert_eq!(Bytes::parse("banana"), None);
}

fn container(name: &str, requests: &[(&str, &str)], limits: &[(&str, &str)]) -> Container {
    let map = |pairs: &[(&str, &str)]| {
        (!pairs.is_empty()).then(|| {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), Quantity(value.to_string())))
                .collect()
        })
    };
    Container {
        name: name.to_string(),
        resources: Some(ResourceRequirements {
            requests: map(requests),
            limits: map(limits),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn sidecar(mut container: Container) -> Container {
    container.restart_policy = Some("Always".to_string());
    container
}

#[test]
fn a_request_total_is_none_only_when_nothing_declares_one() {
    let bare = PodSpec {
        containers: vec![container("app", &[], &[])],
        ..Default::default()
    };
    assert_eq!(
        total_request(std::slice::from_ref(&bare), "cpu", parse_cpu_millis),
        None,
        "no pod declares a request, so there is no request -- zero would be invented"
    );

    let declared = PodSpec {
        containers: vec![
            container("app", &[("cpu", "500m")], &[]),
            container("quiet", &[], &[]),
        ],
        init_containers: Some(vec![sidecar(container("log", &[("cpu", "100m")], &[]))]),
        ..Default::default()
    };
    assert_eq!(
        total_request(&[declared.clone(), bare], "cpu", parse_cpu_millis),
        Some(600),
        "the declaring pod sums by the scheduler's rule and the silent pod adds zero"
    );
    assert_eq!(
        total_request(&[declared], "memory", parse_bytes),
        None,
        "declaring cpu says nothing about memory"
    );
}

#[test]
fn a_limit_total_exists_only_when_every_running_container_is_capped() {
    let capped = PodSpec {
        containers: vec![container("app", &[], &[("cpu", "1"), ("memory", "256Mi")])],
        init_containers: Some(vec![
            sidecar(container(
                "log",
                &[],
                &[("cpu", "500m"), ("memory", "64Mi")],
            )),
            // A migration that ran to completion: its limit no longer binds
            // and must not count.
            container("migrate", &[], &[("cpu", "4"), ("memory", "2Gi")]),
        ]),
        ..Default::default()
    };
    assert_eq!(
        total_limit(std::slice::from_ref(&capped), "cpu", parse_cpu_millis),
        Some(1500)
    );
    assert_eq!(
        total_limit(std::slice::from_ref(&capped), "memory", parse_bytes),
        Some(320 * 1024 * 1024)
    );

    let uncapped = PodSpec {
        containers: vec![container("app", &[], &[("memory", "128Mi")])],
        ..Default::default()
    };
    assert_eq!(
        total_limit(&[capped.clone(), uncapped.clone()], "cpu", parse_cpu_millis),
        None,
        "one uncapped pod uncaps the set"
    );
    assert_eq!(
        total_limit(&[capped, uncapped], "memory", parse_bytes),
        Some((320 + 128) * 1024 * 1024),
        "while the resource every container caps still sums"
    );
    assert_eq!(total_limit(&[], "cpu", parse_cpu_millis), None);
}

const KUBELET_TEXT: &str = r#"# HELP pod_cpu_usage_seconds_total Cumulative cpu time consumed by the pod in core-seconds
# TYPE pod_cpu_usage_seconds_total counter
pod_cpu_usage_seconds_total{namespace="prod",pod="api-1"} 34.5 1700000010000
pod_cpu_usage_seconds_total{namespace="other",pod="noise"} 99 1700000010000
pod_memory_working_set_bytes{namespace="prod",pod="api-1"} 5.24288e+06 1700000010000
container_cpu_usage_seconds_total{container="app",namespace="prod",pod="api-1"} 30.1 1700000010000
scrape_error 0
"#;

#[test]
fn the_parser_reads_exactly_the_two_pod_families_and_keys_by_namespace_and_pod() {
    let parsed = parse_resource_metrics(KUBELET_TEXT).expect("kubelet text parses");
    let api = parsed
        .get(&("prod".to_string(), "api-1".to_string()))
        .expect("the pod is present");
    assert_eq!(api.cpu_seconds, 34.5);
    assert_eq!(api.cpu_stamp_ms, Some(1_700_000_010_000));
    assert_eq!(api.memory_bytes, Some(5_242_880));
    let noise = parsed
        .get(&("other".to_string(), "noise".to_string()))
        .expect("the parser keys by namespace and pod; the caller filters");
    assert_eq!(noise.memory_bytes, None, "no memory series was exposed");
    assert_eq!(
        parsed.len(),
        2,
        "container-level series and scrape_error are recognised but not collected"
    );
}

#[test]
fn a_body_that_is_not_exposition_is_refused_whole_not_read_as_empty() {
    assert!(parse_resource_metrics("<html>not metrics</html>").is_err());
    assert!(parse_resource_metrics("").is_err());
    assert!(
        parse_resource_metrics("error while fetching metrics").is_err(),
        "prose has no metric name and no parseable value"
    );
    let comments_only = "# HELP pod_cpu_usage_seconds_total ...\n# TYPE ... counter\n";
    assert_eq!(
        parse_resource_metrics(comments_only)
            .expect("a node with no pods still answers exposition")
            .len(),
        0,
        "an empty exposition is empty, not malformed"
    );
}

#[test]
fn hostile_lines_are_skipped_without_poisoning_the_parse() {
    let text = format!(
        "pod_cpu_usage_seconds_total{{namespace=\"prod\",pod=\"api-1\"}} -3 1700000010000\n\
         pod_memory_working_set_bytes{{namespace=\"prod\",pod=\"api-1\"}} NaN\n\
         pod_memory_working_set_bytes{{namespace=\"prod\"}} 12 1700000010000\n\
         pod_memory_working_set_bytes{{namespace=\"prod\",pod=\"{}\"}} 12 1700000010000\n\
         pod_memory_working_set_bytes{{namespace=\"prod\",pod=\"api-2\"}} 4096 1700000010000\n",
        "x".repeat(MAX_METRIC_LINE_BYTES)
    );
    let parsed = parse_resource_metrics(&text).expect("the honest line carries the parse");
    assert_eq!(
        parsed.len(),
        1,
        "negative, NaN, unlabelled and oversized lines are skipped"
    );
    assert_eq!(
        parsed
            .get(&("prod".to_string(), "api-2".to_string()))
            .and_then(|sample| sample.memory_bytes),
        Some(4096)
    );
}

fn sample(seconds: f64, stamp_ms: Option<i64>) -> KubeletSample {
    KubeletSample {
        cpu_seconds: seconds,
        cpu_stamp_ms: stamp_ms,
        memory_bytes: None,
    }
}

fn one(pod: &str, seconds: f64, stamp_ms: Option<i64>) -> HashMap<String, KubeletSample> {
    [(pod.to_string(), sample(seconds, stamp_ms))]
        .into_iter()
        .collect()
}

#[test]
fn a_rate_needs_two_samples_and_advances_honestly() {
    let mut counters = CpuCounters::new();

    assert_eq!(
        advance_rates(&mut counters, &one("api-1", 10.0, Some(1_000))),
        None,
        "the first sample is a baseline, not a rate"
    );
    assert_eq!(
        advance_rates(&mut counters, &one("api-1", 12.5, Some(11_000))),
        Some(Millicores(250)),
        "2.5 core-seconds over 10 seconds is 250m"
    );
    assert_eq!(
        advance_rates(&mut counters, &one("api-1", 12.5, Some(11_000))),
        Some(Millicores(250)),
        "an unmoved kubelet clock repeats the last truth rather than inventing 0m"
    );
    assert_eq!(
        advance_rates(&mut counters, &one("api-1", 0.2, Some(21_000))),
        None,
        "a counter that went backwards is a restart: rebaseline, do not go negative"
    );
    assert_eq!(
        advance_rates(&mut counters, &one("api-1", 1.2, Some(31_000))),
        Some(Millicores(100)),
        "and the next sample measures the new incarnation"
    );
}

#[test]
fn a_workload_rate_is_a_number_only_when_every_pod_yields_one() {
    let mut counters = CpuCounters::new();
    let both: HashMap<String, KubeletSample> = [
        ("api-1".to_string(), sample(10.0, Some(1_000))),
        ("api-2".to_string(), sample(20.0, Some(1_000))),
    ]
    .into_iter()
    .collect();
    assert_eq!(advance_rates(&mut counters, &both), None);

    let second: HashMap<String, KubeletSample> = [
        ("api-1".to_string(), sample(11.0, Some(11_000))),
        ("api-2".to_string(), sample(22.0, Some(11_000))),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        advance_rates(&mut counters, &second),
        Some(Millicores(300)),
        "100m and 200m sum once both pods have a rate"
    );

    let with_newcomer: HashMap<String, KubeletSample> = [
        ("api-1".to_string(), sample(12.0, Some(21_000))),
        ("api-3".to_string(), sample(0.5, Some(21_000))),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        advance_rates(&mut counters, &with_newcomer),
        None,
        "a pod without a baseline makes the sum a lie, so there is no sum"
    );
    assert!(
        !counters.contains_key("api-2"),
        "a pod that left the set does not haunt the counters"
    );

    let stamped_none = one("api-1", 13.0, None);
    let mut counters = CpuCounters::new();
    assert_eq!(
        advance_rates(&mut counters, &stamped_none),
        None,
        "a counter without its timestamp never becomes a rate"
    );
    assert_eq!(
        advance_rates(&mut CpuCounters::new(), &HashMap::new()),
        None,
        "no pods, no rate -- not a zero"
    );
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn the_fallback_order_treats_absence_and_unavailability_alike_and_denial_as_final() {
    assert!(matches!(
        after_metrics_api(&api_error(404)),
        MetricsAnswer::NotServed
    ));
    assert!(matches!(
        after_metrics_api(&api_error(503)),
        MetricsAnswer::NotServed
    ));
    assert!(
        matches!(after_metrics_api(&api_error(403)), MetricsAnswer::Denied),
        "a denial is an answer; the kubelet must not be asked to route around it"
    );
    assert!(matches!(
        after_metrics_api(&api_error(500)),
        MetricsAnswer::Failed(_)
    ));
    assert!(matches!(
        after_metrics_api(&api_error(429)),
        MetricsAnswer::Failed(_)
    ));
}
