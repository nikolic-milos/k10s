//! Quantity grammars across cpu and memory, the effective request an init
//! floor and sidecars produce, and how a node's status reads its ready,
//! pressure and cordon conditions.

use super::*;
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

#[test]
fn quantities_parse_across_cpu_and_memory_grammars() {
    assert_eq!(parse_cpu_millis("2"), Some(2000));
    assert_eq!(parse_cpu_millis("1500m"), Some(1500));
    assert_eq!(parse_cpu_millis("0.5"), Some(500));
    assert_eq!(parse_cpu_millis("156340764n"), Some(156));
    assert_eq!(parse_cpu_millis("250u"), Some(0));
    assert_eq!(parse_bytes("128974848"), Some(128974848));
    assert_eq!(parse_bytes("64Mi"), Some(64 * 1024 * 1024));
    assert_eq!(parse_bytes("16Gi"), Some(16 * 1024 * 1024 * 1024));
    assert_eq!(parse_bytes("1234Ki"), Some(1234 * 1024));
    assert_eq!(parse_bytes("5G"), Some(5_000_000_000));
    assert_eq!(parse_bytes("12e3"), Some(12_000), "exponent, not exa");
    assert_eq!(parse_bytes("129e6"), Some(129_000_000));
    assert_eq!(parse_bytes(""), None);
    assert_eq!(parse_bytes("Gi"), None);
    assert_eq!(parse_bytes("banana"), None);
}

#[test]
fn formatting_round_trips_the_common_shapes() {
    assert_eq!(fmt_cpu(2000), "2");
    assert_eq!(fmt_cpu(1500), "1500m");
    assert_eq!(fmt_bytes(16 * 1024 * 1024 * 1024), "16.0Gi");
    assert_eq!(fmt_bytes(64 * 1024 * 1024), "64Mi");
    assert_eq!(fmt_bytes(512), "512");
}

fn container(name: &str, cpu: &str, restart_policy: Option<&str>) -> Container {
    Container {
        name: name.to_string(),
        restart_policy: restart_policy.map(str::to_string),
        resources: Some(ResourceRequirements {
            requests: Some(
                [("cpu".to_string(), Quantity(cpu.to_string()))]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn the_effective_request_takes_the_init_floor_and_accumulates_sidecars() {
    let spec = PodSpec {
        containers: vec![
            container("app", "500m", None),
            container("proxy", "250m", None),
        ],
        init_containers: Some(vec![
            container("migrate", "2", None),
            container("sidecar-log", "100m", Some("Always")),
        ]),
        ..Default::default()
    };
    assert_eq!(
        effective_request(&spec, "cpu", parse_cpu_millis),
        100 + 2000,
        "the big init container is the floor, plus the running sidecar"
    );

    let steady = PodSpec {
        containers: vec![container("app", "500m", None)],
        init_containers: Some(vec![container("sidecar-log", "100m", Some("Always"))]),
        ..Default::default()
    };
    assert_eq!(effective_request(&steady, "cpu", parse_cpu_millis), 600);

    let missing = PodSpec {
        containers: vec![Container {
            name: "bare".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(effective_request(&missing, "cpu", parse_cpu_millis), 0);
}

#[test]
fn node_status_reads_ready_pressure_and_cordon_state() {
    use k8s_openapi::api::core::v1::{NodeCondition, NodeSpec, NodeStatus};
    let node = |ready: &str, pressure: bool, unschedulable: bool| Node {
        status: Some(NodeStatus {
            conditions: Some(vec![
                NodeCondition {
                    type_: "Ready".to_string(),
                    status: ready.to_string(),
                    ..Default::default()
                },
                NodeCondition {
                    type_: "MemoryPressure".to_string(),
                    status: if pressure { "True" } else { "False" }.to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        spec: Some(NodeSpec {
            unschedulable: Some(unschedulable),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(status_text(&node("True", false, false)), "Ready");
    assert_eq!(
        status_text(&node("False", true, true)),
        "NotReady,MemoryPressure,SchedulingDisabled"
    );
    assert_eq!(status_text(&Node::default()), "Unknown");
}

#[test]
fn roles_come_from_the_role_labels_sorted_or_are_absent() {
    let mut node = Node::default();
    node.metadata.labels = Some(
        [
            ("node-role.kubernetes.io/worker".to_string(), String::new()),
            (
                "node-role.kubernetes.io/control-plane".to_string(),
                String::new(),
            ),
            ("kubernetes.io/hostname".to_string(), "n1".to_string()),
        ]
        .into_iter()
        .collect(),
    );
    assert_eq!(roles_text(&node), "control-plane,worker");
    assert_eq!(roles_text(&Node::default()), "<none>");
}

#[test]
fn node_addresses_prefer_internal_over_external_and_refuse_empty_values() {
    use k8s_openapi::api::core::v1::{NodeAddress, NodeStatus};

    let mut node = Node {
        status: Some(NodeStatus {
            addresses: Some(vec![
                NodeAddress {
                    type_: "ExternalIP".to_string(),
                    address: "203.0.113.8".to_string(),
                },
                NodeAddress {
                    type_: "InternalIP".to_string(),
                    address: "10.0.0.8".to_string(),
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(node_address(&node).as_deref(), Some("10.0.0.8"));

    node.status.as_mut().unwrap().addresses = Some(vec![NodeAddress {
        type_: "InternalIP".to_string(),
        address: String::new(),
    }]);
    assert_eq!(node_address(&node), None);
}

#[test]
fn pdb_selectors_follow_policy_v1_semantics_including_the_nil_empty_split() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;

    let labels = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let api = labels(&[("app", "api"), ("tier", "web")]);

    assert!(
        !selector_matches(None, &api),
        "a nil selector selects no pods"
    );
    assert!(
        selector_matches(Some(&LabelSelector::default()), &api),
        "an empty selector selects every pod in the namespace"
    );

    let by_label = LabelSelector {
        match_labels: Some(labels(&[("app", "api")])),
        ..Default::default()
    };
    assert!(selector_matches(Some(&by_label), &api));
    assert!(!selector_matches(
        Some(&by_label),
        &labels(&[("app", "web")])
    ));
    assert!(!selector_matches(Some(&by_label), &BTreeMap::new()));

    let expression = |key: &str, operator: &str, values: &[&str]| LabelSelector {
        match_expressions: Some(vec![LabelSelectorRequirement {
            key: key.to_string(),
            operator: operator.to_string(),
            values: (!values.is_empty()).then(|| values.iter().map(|v| v.to_string()).collect()),
        }]),
        ..Default::default()
    };
    assert!(selector_matches(
        Some(&expression("app", "In", &["api", "job"])),
        &api
    ));
    assert!(!selector_matches(
        Some(&expression("app", "In", &["job"])),
        &api
    ));
    assert!(!selector_matches(
        Some(&expression("app", "NotIn", &["api"])),
        &api
    ));
    assert!(
        selector_matches(
            Some(&expression("app", "NotIn", &["api"])),
            &BTreeMap::new()
        ),
        "NotIn matches a pod without the key at all"
    );
    assert!(selector_matches(
        Some(&expression("tier", "Exists", &[])),
        &api
    ));
    assert!(!selector_matches(
        Some(&expression("gone", "Exists", &[])),
        &api
    ));
    assert!(selector_matches(
        Some(&expression("gone", "DoesNotExist", &[])),
        &api
    ));
    assert!(
        !selector_matches(Some(&expression("app", "Wildcard", &["*"])), &api),
        "an unknown operator never matches, so the count cannot be wrong upward"
    );
}

#[test]
fn a_used_over_allocatable_cell_carries_the_percentage() {
    assert_eq!(counted(1500, Some(4000), fmt_cpu), "1500m/4 (38%)");
    assert_eq!(counted(12, Some(110), |n| n.to_string()), "12/110 (11%)");
    assert_eq!(counted(1500, None, fmt_cpu), "1500m");
    assert_eq!(counted_opt(None, Some(4000), fmt_cpu), "?");
}
