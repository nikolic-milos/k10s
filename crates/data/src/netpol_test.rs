use super::*;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use serde_json::json;

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

fn pod(namespace: &str, name: &str, pairs: &[(&str, &str)]) -> PodRef {
    pod_with(namespace, name, pairs, &[], &[])
}

fn pod_with(
    namespace: &str,
    name: &str,
    pairs: &[(&str, &str)],
    ips: &[&str],
    ports: &[PodPort],
) -> PodRef {
    PodRef {
        name: name.into(),
        namespace: namespace.into(),
        uid: String::new(),
        labels: labels(pairs),
        ips: ips
            .iter()
            .map(|ip| ip.parse().expect("test pod IP is valid"))
            .collect(),
        ports: ports.to_vec(),
    }
}

fn ns(name: &str, pairs: &[(&str, &str)]) -> NamespaceRef {
    NamespaceRef {
        name: name.into(),
        labels: labels(pairs),
    }
}

fn named_port(name: &str, port: u16, protocol: Protocol) -> PodPort {
    PodPort {
        name: name.into(),
        port,
        protocol,
    }
}

fn traffic(protocol: Protocol, port: u16) -> Traffic {
    Traffic { protocol, port }
}

fn np(value: serde_json::Value) -> NetworkPolicy {
    serde_json::from_value(value).expect("NetworkPolicy JSON")
}

#[test]
fn default_allow_and_ingress_isolation_have_distinct_reasons() {
    let dst = pod("prod", "api", &[("app", "api")]);
    let src = pod("other", "job", &[("app", "job")]);
    let namespaces = [ns("prod", &[]), ns("other", &[])];

    let open = declare(&[], &[dst.clone(), src.clone()], &namespaces);
    let open_verdict = open.verdict(&src, &dst, traffic(Protocol::Tcp, 443));
    assert_eq!(open_verdict.decision, Decision::Allow);
    assert_eq!(
        open_verdict.reasons,
        vec![
            VerdictReason::DefaultAllow {
                direction: Direction::Ingress,
            },
            VerdictReason::DefaultAllow {
                direction: Direction::Egress,
            },
        ]
    );

    let deny = np(json!({
        "metadata": { "name": "deny-ingress", "namespace": "prod" },
        "spec": { "podSelector": {}, "policyTypes": ["Ingress"] }
    }));
    let isolated = declare(&[deny], &[dst.clone(), src.clone()], &namespaces);
    let denied = isolated.verdict(&src, &dst, traffic(Protocol::Tcp, 443));
    assert_eq!(denied.decision, Decision::Deny);
    assert!(denied.reasons.contains(&VerdictReason::Isolated {
        direction: Direction::Ingress,
        selecting_policies: 1,
    }));
    assert!(isolated.can_receive(&dst, &dst));
}

#[test]
fn source_egress_and_destination_ingress_must_both_allow() {
    let src = pod("clients", "web", &[("app", "web")]);
    let dst = pod("backend", "api", &[("app", "api")]);
    let namespaces = [
        ns("clients", &[("access", "clients")]),
        ns("backend", &[("access", "backend")]),
    ];
    let ingress = np(json!({
        "metadata": { "name": "allow-web", "namespace": "backend" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "api" } },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": { "matchLabels": { "access": "clients" } },
                    "podSelector": { "matchLabels": { "app": "web" } }
                }]
            }]
        }
    }));
    let deny_egress = np(json!({
        "metadata": { "name": "deny-egress", "namespace": "clients" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "web" } },
            "policyTypes": ["Egress"]
        }
    }));
    let allow_egress = np(json!({
        "metadata": { "name": "allow-api", "namespace": "clients" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "web" } },
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{
                    "namespaceSelector": { "matchLabels": { "access": "backend" } },
                    "podSelector": { "matchLabels": { "app": "api" } }
                }]
            }]
        }
    }));

    let denied = declare(
        &[ingress.clone(), deny_egress.clone()],
        &[src.clone(), dst.clone()],
        &namespaces,
    )
    .verdict(&src, &dst, traffic(Protocol::Tcp, 443));
    assert_eq!(denied.decision, Decision::Deny);
    assert!(denied.reasons.contains(&VerdictReason::Isolated {
        direction: Direction::Egress,
        selecting_policies: 1,
    }));

    let allowed = declare(
        &[ingress, deny_egress, allow_egress],
        &[src.clone(), dst.clone()],
        &namespaces,
    )
    .verdict(&src, &dst, traffic(Protocol::Tcp, 443));
    assert_eq!(allowed.decision, Decision::Allow);
    assert!(allowed.reasons.contains(&VerdictReason::AllowedByPolicy {
        direction: Direction::Ingress,
        namespace: "backend".into(),
        name: "allow-web".into(),
    }));
    assert!(allowed.reasons.contains(&VerdictReason::AllowedByPolicy {
        direction: Direction::Egress,
        namespace: "clients".into(),
        name: "allow-api".into(),
    }));
}

#[test]
fn default_policy_types_include_egress_only_when_the_field_is_present() {
    let selected = pod("prod", "api", &[("app", "api")]);
    let outside = pod("other", "job", &[]);
    let namespaces = [ns("prod", &[]), ns("other", &[])];
    let defaulted = np(json!({
        "metadata": { "name": "defaulted", "namespace": "prod" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "api" } },
            "egress": []
        }
    }));
    let declared = declare(
        &[defaulted],
        &[selected.clone(), outside.clone()],
        &namespaces,
    );
    assert_eq!(
        declared
            .verdict(&selected, &outside, traffic(Protocol::Tcp, 443))
            .decision,
        Decision::Deny,
        "the present egress field defaults policyTypes to include Egress"
    );
    assert_eq!(
        declared
            .verdict(&outside, &selected, traffic(Protocol::Tcp, 443))
            .decision,
        Decision::Deny,
        "every defaulted policy also isolates ingress"
    );

    let egress_only = np(json!({
        "metadata": { "name": "egress-only", "namespace": "prod" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "api" } },
            "policyTypes": ["Egress"],
            "egress": []
        }
    }));
    let declared = declare(
        &[egress_only],
        &[selected.clone(), outside.clone()],
        &namespaces,
    );
    assert_eq!(
        declared
            .verdict(&outside, &selected, traffic(Protocol::Tcp, 443))
            .decision,
        Decision::Allow,
        "an explicit egress-only policy does not isolate ingress"
    );
}

#[test]
fn selecting_policies_combine_rules_additively() {
    let dst = pod("prod", "api", &[("app", "api")]);
    let web = pod("prod", "web", &[("app", "web")]);
    let job = pod("prod", "job", &[("app", "job")]);
    let deny = np(json!({
        "metadata": { "name": "deny-all", "namespace": "prod" },
        "spec": { "podSelector": { "matchLabels": { "app": "api" } } }
    }));
    let allow_web = np(json!({
        "metadata": { "name": "allow-web", "namespace": "prod" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "from": [{ "podSelector": { "matchLabels": { "app": "web" } } }]
            }]
        }
    }));
    let declared = declare(
        &[deny, allow_web],
        &[dst.clone(), web.clone(), job.clone()],
        &[ns("prod", &[])],
    );
    assert_eq!(
        declared
            .verdict(&web, &dst, traffic(Protocol::Tcp, 8080))
            .decision,
        Decision::Allow
    );
    assert_eq!(
        declared
            .verdict(&job, &dst, traffic(Protocol::Tcp, 8080))
            .decision,
        Decision::Deny
    );
}

#[test]
fn numeric_port_ranges_are_inclusive_and_protocol_defaults_to_tcp() {
    let src = pod("other", "client", &[]);
    let dst = pod("prod", "api", &[]);
    let policy = np(json!({
        "metadata": { "name": "admin-range", "namespace": "prod" },
        "spec": {
            "podSelector": {},
            "ingress": [{ "ports": [{ "port": 8000, "endPort": 8010 }] }]
        }
    }));
    let declared = declare(
        &[policy],
        &[src.clone(), dst.clone()],
        &[ns("prod", &[]), ns("other", &[])],
    );
    for port in [8000, 8005, 8010] {
        assert_eq!(
            declared
                .verdict(&src, &dst, traffic(Protocol::Tcp, port))
                .decision,
            Decision::Allow,
            "TCP port {port} is inside the inclusive range"
        );
    }
    for port in [7999, 8011] {
        assert_eq!(
            declared
                .verdict(&src, &dst, traffic(Protocol::Tcp, port))
                .decision,
            Decision::Deny,
            "TCP port {port} is outside the range"
        );
    }
    assert_eq!(
        declared
            .verdict(&src, &dst, traffic(Protocol::Udp, 8000))
            .decision,
        Decision::Deny,
        "an omitted protocol means TCP, not every protocol"
    );
}

#[test]
fn named_ports_resolve_on_the_destination_for_both_directions() {
    let src = pod("clients", "web", &[("app", "web")]);
    let dst = pod_with(
        "prod",
        "api",
        &[("app", "api")],
        &[],
        &[named_port("https", 8443, Protocol::Tcp)],
    );
    let ingress = np(json!({
        "metadata": { "name": "named-ingress", "namespace": "prod" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{ "ports": [{ "port": "https" }] }]
        }
    }));
    let egress = np(json!({
        "metadata": { "name": "named-egress", "namespace": "clients" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "web" } },
            "policyTypes": ["Egress"],
            "egress": [{ "ports": [{ "port": "https" }] }]
        }
    }));
    let declared = declare(
        &[ingress, egress],
        &[src.clone(), dst.clone()],
        &[ns("clients", &[]), ns("prod", &[])],
    );
    assert_eq!(
        declared
            .verdict(&src, &dst, traffic(Protocol::Tcp, 8443))
            .decision,
        Decision::Allow
    );
    assert_eq!(
        declared
            .verdict(&src, &dst, traffic(Protocol::Tcp, 443))
            .decision,
        Decision::Deny
    );
    assert_eq!(
        declared
            .verdict(&src, &dst, traffic(Protocol::Udp, 8443))
            .decision,
        Decision::Deny
    );
}

#[test]
fn ip_blocks_match_pod_ips_and_apply_exceptions_for_both_families() {
    let dst = pod("prod", "api", &[]);
    let admitted = pod_with("other", "allowed", &[], &["10.2.3.4"], &[]);
    let excluded = pod_with("other", "excluded", &[], &["10.1.2.3"], &[]);
    let ingress = np(json!({
        "metadata": { "name": "private-sources", "namespace": "prod" },
        "spec": {
            "podSelector": {},
            "ingress": [{
                "from": [{
                    "ipBlock": { "cidr": "10.0.0.0/8", "except": ["10.1.0.0/16"] }
                }]
            }]
        }
    }));
    let declared = declare(
        &[ingress],
        &[dst.clone(), admitted.clone(), excluded.clone()],
        &[ns("prod", &[]), ns("other", &[])],
    );
    assert_eq!(
        declared
            .verdict(&admitted, &dst, traffic(Protocol::Tcp, 443))
            .decision,
        Decision::Allow
    );
    assert_eq!(
        declared
            .verdict(&excluded, &dst, traffic(Protocol::Tcp, 443))
            .decision,
        Decision::Deny
    );

    let source = pod("clients", "web", &[("app", "web")]);
    let ipv6 = pod_with("other", "v6", &[], &["2001:db8::42"], &[]);
    let ipv6_excluded = pod_with("other", "v6-excluded", &[], &["2001:db8::dead"], &[]);
    let egress = np(json!({
        "metadata": { "name": "v6-egress", "namespace": "clients" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "web" } },
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{
                    "ipBlock": { "cidr": "2001:db8::/64", "except": ["2001:db8::dead/128"] }
                }]
            }]
        }
    }));
    let declared = declare(
        &[egress],
        &[source.clone(), ipv6.clone(), ipv6_excluded.clone()],
        &[ns("clients", &[]), ns("other", &[])],
    );
    assert_eq!(
        declared
            .verdict(&source, &ipv6, traffic(Protocol::Tcp, 443))
            .decision,
        Decision::Allow
    );
    assert_eq!(
        declared
            .verdict(&source, &ipv6_excluded, traffic(Protocol::Tcp, 443))
            .decision,
        Decision::Deny
    );
}

#[test]
fn allowed_peers_keeps_cidr_context_and_resolves_matching_pods() {
    let dst = pod("prod", "api", &[]);
    let admitted = pod_with("other", "allowed", &[], &["10.2.3.4"], &[]);
    let excluded = pod_with("other", "excluded", &[], &["10.1.2.3"], &[]);
    let policy = np(json!({
        "metadata": { "name": "private-sources", "namespace": "prod" },
        "spec": {
            "podSelector": {},
            "ingress": [{
                "from": [{
                    "ipBlock": { "cidr": "10.0.0.0/8", "except": ["10.1.0.0/16"] }
                }],
                "ports": [{ "port": 443 }]
            }]
        }
    }));
    let declared = declare(
        &[policy],
        &[dst.clone(), admitted, excluded],
        &[ns("prod", &[]), ns("other", &[])],
    );
    assert_eq!(
        declared.allowed_peers(&dst),
        vec![
            Peer::Pod {
                namespace: "other".into(),
                name: "allowed".into(),
            },
            Peer::Pod {
                namespace: "prod".into(),
                name: "api".into(),
            },
            Peer::Cidr {
                cidr: "10.0.0.0/8".into(),
                except: vec!["10.1.0.0/16".into()],
            },
        ]
    );
}

#[test]
fn a_peer_with_namespace_and_pod_selectors_requires_both() {
    let dst = pod("prod", "api", &[]);
    let matching = pod("trusted", "web", &[("app", "web")]);
    let wrong_pod = pod("trusted", "job", &[("app", "job")]);
    let wrong_namespace = pod("other", "web", &[("app", "web")]);
    let policy = np(json!({
        "metadata": { "name": "trusted-web", "namespace": "prod" },
        "spec": {
            "podSelector": {},
            "ingress": [{
                "from": [{
                    "namespaceSelector": { "matchLabels": { "trust": "yes" } },
                    "podSelector": { "matchLabels": { "app": "web" } }
                }]
            }]
        }
    }));
    let declared = declare(
        &[policy],
        &[
            dst.clone(),
            matching.clone(),
            wrong_pod.clone(),
            wrong_namespace.clone(),
        ],
        &[
            ns("prod", &[]),
            ns("trusted", &[("trust", "yes")]),
            ns("other", &[("trust", "no")]),
        ],
    );
    assert_eq!(
        declared
            .verdict(&matching, &dst, traffic(Protocol::Tcp, 80))
            .decision,
        Decision::Allow
    );
    for source in [&wrong_pod, &wrong_namespace] {
        assert_eq!(
            declared
                .verdict(source, &dst, traffic(Protocol::Tcp, 80))
                .decision,
            Decision::Deny
        );
    }
}

#[test]
fn truncation_is_explicit_and_cannot_produce_a_definitive_deny() {
    let dst = pod("prod", "api", &[]);
    let src = pod("other", "job", &[]);
    let mut policies = vec![np(json!({
        "metadata": { "name": "deny-prod", "namespace": "prod" },
        "spec": { "podSelector": {}, "policyTypes": ["Ingress"] }
    }))];
    policies.extend((1..MAX_POLICIES).map(|index| {
        np(json!({
            "metadata": { "name": format!("filler-{index}"), "namespace": "filler" },
            "spec": { "podSelector": {}, "policyTypes": ["Ingress"] }
        }))
    }));
    policies.push(np(json!({
        "metadata": { "name": "allow-other", "namespace": "prod" },
        "spec": {
            "podSelector": {},
            "ingress": [{
                "from": [{
                    "namespaceSelector": {
                        "matchLabels": { "kubernetes.io/metadata.name": "other" }
                    }
                }]
            }]
        }
    })));

    let declared = declare(
        &policies,
        &[dst.clone(), src.clone()],
        &[
            ns("prod", &[("kubernetes.io/metadata.name", "prod")]),
            ns("other", &[("kubernetes.io/metadata.name", "other")]),
        ],
    );
    let verdict = declared.verdict(&src, &dst, traffic(Protocol::Tcp, 443));
    assert_eq!(
        verdict.completeness,
        Completeness::Truncated {
            evaluated_policies: MAX_POLICIES,
            total_policies: MAX_POLICIES + 1,
        }
    );
    assert_eq!(verdict.decision, Decision::Indeterminate);
    assert_eq!(verdict.allowed(), None);
    assert!(
        verdict
            .reasons
            .contains(&VerdictReason::PolicySetTruncated {
                evaluated_policies: MAX_POLICIES,
                total_policies: MAX_POLICIES + 1,
            })
    );
    assert!(
        declared.can_receive(&dst, &src),
        "the compatibility bool must not expose a provisional deny"
    );
    assert_eq!(declared.allowed_peers(&dst), vec![Peer::Any]);
}

#[test]
fn matching_rules_prove_allow_even_when_unrelated_policy_input_is_truncated() {
    let src = pod("clients", "web", &[("app", "web")]);
    let dst = pod("prod", "api", &[("app", "api")]);
    let mut policies = vec![
        np(json!({
            "metadata": { "name": "allow-ingress", "namespace": "prod" },
            "spec": {
                "podSelector": { "matchLabels": { "app": "api" } },
                "ingress": [{}]
            }
        })),
        np(json!({
            "metadata": { "name": "allow-egress", "namespace": "clients" },
            "spec": {
                "podSelector": { "matchLabels": { "app": "web" } },
                "policyTypes": ["Egress"],
                "egress": [{}]
            }
        })),
    ];
    policies.extend((2..=MAX_POLICIES).map(|index| {
        np(json!({
            "metadata": { "name": format!("filler-{index}"), "namespace": "filler" },
            "spec": { "podSelector": {}, "policyTypes": ["Ingress"] }
        }))
    }));
    let declared = declare(
        &policies,
        &[src.clone(), dst.clone()],
        &[ns("clients", &[]), ns("prod", &[])],
    );
    let verdict = declared.verdict(&src, &dst, traffic(Protocol::Tcp, 443));
    assert_eq!(verdict.decision, Decision::Allow);
    assert!(matches!(
        verdict.completeness,
        Completeness::Truncated { .. }
    ));
}

#[test]
fn policy_cannot_block_a_pod_from_itself() {
    let pod = pod("prod", "api", &[]);
    let policy = np(json!({
        "metadata": { "name": "deny-both", "namespace": "prod" },
        "spec": {
            "podSelector": {},
            "policyTypes": ["Ingress", "Egress"]
        }
    }));
    let declared = declare(&[policy], std::slice::from_ref(&pod), &[ns("prod", &[])]);
    let verdict = declared.verdict(&pod, &pod, traffic(Protocol::Tcp, 443));
    assert_eq!(verdict.decision, Decision::Allow);
    assert_eq!(verdict.reasons, vec![VerdictReason::SamePod]);
}

#[test]
fn pod_posture_separates_isolation_from_a_traffic_verdict_and_bounds_names() {
    let api = pod("prod", "api", &[("app", "api")]);
    let policies = [
        np(json!({
            "metadata": { "name": "ingress-a", "namespace": "prod" },
            "spec": {
                "podSelector": { "matchLabels": { "app": "api" } },
                "policyTypes": ["Ingress"]
            }
        })),
        np(json!({
            "metadata": { "name": "ingress-b", "namespace": "prod" },
            "spec": {
                "podSelector": { "matchLabels": { "app": "api" } },
                "policyTypes": ["Ingress"]
            }
        })),
        np(json!({
            "metadata": { "name": "other", "namespace": "prod" },
            "spec": {
                "podSelector": { "matchLabels": { "app": "other" } },
                "policyTypes": ["Egress"]
            }
        })),
    ];
    let posture =
        declare(&policies, std::slice::from_ref(&api), &[ns("prod", &[])]).pod_posture(&api, 1);

    assert!(posture.ingress.isolated);
    assert_eq!(posture.ingress.selecting_policies, 2);
    assert_eq!(posture.ingress.policies, ["prod/ingress-a"]);
    assert!(posture.ingress.policies_truncated);
    assert!(!posture.egress.isolated);
    assert_eq!(posture.egress.selecting_policies, 0);
    assert_eq!(posture.completeness, Completeness::Complete);
}
