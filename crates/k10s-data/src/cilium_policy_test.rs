use super::*;
use serde_json::json;
use std::net::IpAddr;

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

fn endpoint(namespace: &str, name: &str, pairs: &[(&str, &str)]) -> EndpointRef {
    endpoint_with(namespace, name, pairs, None, &[])
}

fn endpoint_with(
    namespace: &str,
    name: &str,
    pairs: &[(&str, &str)],
    identity_id: Option<i64>,
    ips: &[&str],
) -> EndpointRef {
    EndpointRef {
        name: name.into(),
        namespace: namespace.into(),
        uid: String::new(),
        labels: labels(pairs),
        identity_id,
        ips: ips
            .iter()
            .map(|ip| ip.parse::<IpAddr>().expect("test IP"))
            .collect(),
        ports: Vec::new(),
    }
}

fn world() -> EndpointRef {
    endpoint_with(
        "default",
        "external",
        &[("reserved:world", "")],
        Some(2),
        &[],
    )
}

fn traffic(port: u16) -> Traffic {
    Traffic {
        protocol: Protocol::Tcp,
        port,
        l7: None,
    }
}

fn cnp(value: serde_json::Value) -> CiliumPolicy {
    let mut doc = value;
    if doc.get("kind").is_none() {
        doc["kind"] = json!("CiliumNetworkPolicy");
    }
    let parsed = parse_policy_document(&doc);
    assert_eq!(parsed.len(), 1, "fixture is one policy: {doc}");
    parsed.into_iter().next().unwrap()
}

fn ccnp(value: serde_json::Value) -> CiliumPolicy {
    let mut doc = value;
    doc["kind"] = json!("CiliumClusterwideNetworkPolicy");
    let parsed = parse_policy_document(&doc);
    assert_eq!(parsed.len(), 1, "fixture is one clusterwide policy: {doc}");
    parsed.into_iter().next().unwrap()
}

#[test]
fn default_allow_until_a_cnp_selects_the_pod() {
    let src = endpoint("prod", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let open = declare(&[]).verdict(&src, &dst, traffic(80));
    assert_eq!(open.decision, Decision::Allow);
    assert!(open.reasons.contains(&VerdictReason::DefaultAllow {
        direction: Direction::Ingress,
    }));

    let deny = cnp(json!({
        "metadata": { "name": "lock-api", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": []
        }
    }));
    let isolated = declare(&[deny]).verdict(&src, &dst, traffic(80));
    assert_eq!(isolated.decision, Decision::Deny);
    assert!(isolated.reasons.contains(&VerdictReason::Isolated {
        direction: Direction::Ingress,
        selecting_policies: 1,
    }));
}

#[test]
fn omitted_ingress_is_not_isolation_empty_list_is() {
    let src = endpoint("prod", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let omitted = cnp(json!({
        "metadata": { "name": "egress-only", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "egress": []
        }
    }));
    let declared = declare(&[omitted]);
    assert_eq!(
        declared.verdict(&src, &dst, traffic(80)).decision,
        Decision::Allow,
        "a missing ingress field does not isolate ingress"
    );
    assert_eq!(
        declared.verdict(&dst, &src, traffic(80)).decision,
        Decision::Deny,
        "an empty egress list isolates egress"
    );

    let empty_ingress = cnp(json!({
        "metadata": { "name": "deny-in", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": []
        }
    }));
    assert_eq!(
        declare(&[empty_ingress])
            .verdict(&src, &dst, traffic(80))
            .decision,
        Decision::Deny
    );
}

#[test]
fn a_nil_from_endpoints_rule_allows_all_l3_an_empty_list_allows_none() {
    let src = endpoint("prod", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let nil_peers = cnp(json!({
        "metadata": { "name": "allow-all-l3", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{}]
        }
    }));
    assert_eq!(
        declare(&[nil_peers])
            .verdict(&src, &dst, traffic(80))
            .decision,
        Decision::Allow
    );

    let empty_peers = cnp(json!({
        "metadata": { "name": "no-peers", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{ "fromEndpoints": [] }]
        }
    }));
    assert_eq!(
        declare(&[empty_peers])
            .verdict(&src, &dst, traffic(80))
            .decision,
        Decision::Deny,
        "an empty fromEndpoints list matches no endpoints"
    );
}

#[test]
fn selector_boundaries_are_exact_and_k8s_prefix_is_normalised() {
    let dst = endpoint("prod", "api", &[("app", "api"), ("tier", "front")]);
    let web = endpoint("prod", "web", &[("app", "web")]);
    let job = endpoint("prod", "job", &[("app", "job")]);
    let extra = endpoint("prod", "web2", &[("app", "web"), ("track", "canary")]);
    let other_ns = endpoint("other", "web", &[("app", "web")]);
    let prefixed = endpoint(
        "prod",
        "web3",
        &[
            ("k8s:app", "web"),
            ("k8s:io.kubernetes.pod.namespace", "prod"),
        ],
    );

    let policy = cnp(json!({
        "metadata": { "name": "allow-web", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }]
            }]
        }
    }));
    let declared = declare(&[policy]);
    assert_eq!(
        declared.verdict(&web, &dst, traffic(80)).decision,
        Decision::Allow
    );
    assert_eq!(
        declared.verdict(&extra, &dst, traffic(80)).decision,
        Decision::Allow,
        "extra labels on the peer still match"
    );
    assert_eq!(
        declared.verdict(&prefixed, &dst, traffic(80)).decision,
        Decision::Allow,
        "k8s:app on the identity matches app on the selector"
    );
    assert_eq!(
        declared.verdict(&job, &dst, traffic(80)).decision,
        Decision::Deny
    );
    assert_eq!(
        declared.verdict(&other_ns, &dst, traffic(80)).decision,
        Decision::Deny,
        "a namespaced fromEndpoints does not cross namespaces unless the selector names one"
    );

    let empty_selector = cnp(json!({
        "metadata": { "name": "all-in-ns", "namespace": "prod" },
        "spec": {
            "endpointSelector": {},
            "ingress": [{ "fromEndpoints": [{ "matchLabels": {} }] }]
        }
    }));
    let declared = declare(&[empty_selector]);
    assert_eq!(
        declared.verdict(&job, &dst, traffic(80)).decision,
        Decision::Allow,
        "an empty selector matches every endpoint in the policy namespace"
    );
    assert_eq!(
        declared.verdict(&other_ns, &dst, traffic(80)).decision,
        Decision::Deny
    );
}

#[test]
fn a_namespace_label_on_from_endpoints_crosses_namespaces() {
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let web = endpoint(
        "clients",
        "web",
        &[
            ("app", "web"),
            ("k8s:io.kubernetes.pod.namespace", "clients"),
        ],
    );
    let policy = cnp(json!({
        "metadata": { "name": "allow-clients", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{
                    "matchLabels": {
                        "app": "web",
                        "k8s:io.kubernetes.pod.namespace": "clients"
                    }
                }]
            }]
        }
    }));
    assert_eq!(
        declare(&[policy]).verdict(&web, &dst, traffic(80)).decision,
        Decision::Allow
    );
}

#[test]
fn match_expressions_in_not_in_exists_and_unknown_fail_closed() {
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let web = endpoint("prod", "web", &[("app", "web"), ("env", "prod")]);
    let job = endpoint("prod", "job", &[("app", "job")]);
    let policy = cnp(json!({
        "metadata": { "name": "expr", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{
                    "matchExpressions": [
                        { "key": "app", "operator": "In", "values": ["web"] },
                        { "key": "env", "operator": "Exists" }
                    ]
                }]
            }]
        }
    }));
    let declared = declare(&[policy]);
    assert_eq!(
        declared.verdict(&web, &dst, traffic(80)).decision,
        Decision::Allow
    );
    assert_eq!(
        declared.verdict(&job, &dst, traffic(80)).decision,
        Decision::Deny
    );

    let unknown = cnp(json!({
        "metadata": { "name": "bad-op", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{
                    "matchExpressions": [
                        { "key": "app", "operator": "Gt", "values": ["web"] }
                    ]
                }]
            }]
        }
    }));
    assert_eq!(
        declare(&[unknown])
            .verdict(&web, &dst, traffic(80))
            .decision,
        Decision::Deny,
        "an unknown operator fails closed"
    );
}

#[test]
fn entities_are_spelled_as_cilium_spells_them() {
    let api = endpoint_with("prod", "api", &[("app", "api")], Some(12345), &[]);
    let web = endpoint_with("prod", "web", &[("app", "web")], Some(67890), &[]);
    let host = endpoint_with("kube-system", "host", &[], Some(1), &[]);
    let remote = endpoint_with("kube-system", "node", &[], Some(6), &[]);
    let init = endpoint_with("prod", "starting", &[], Some(5), &[]);
    let ingress = endpoint_with("cilium", "envoy", &[], Some(8), &[]);
    let unmanaged = endpoint_with("prod", "raw", &[], Some(3), &[]);
    let apiserver = endpoint_with("kube-system", "kube-apiserver", &[], Some(7), &[]);

    let from_world = cnp(json!({
        "metadata": { "name": "from-world", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{ "fromEntities": ["world"] }]
        }
    }));
    let declared = declare(&[from_world]);
    assert_eq!(
        declared.verdict(&world(), &api, traffic(80)).decision,
        Decision::Allow
    );
    assert_eq!(
        declared.verdict(&web, &api, traffic(80)).decision,
        Decision::Deny,
        "world does not match a cluster workload"
    );

    let from_cluster = cnp(json!({
        "metadata": { "name": "from-cluster", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{ "fromEntities": ["cluster"] }]
        }
    }));
    let declared = declare(&[from_cluster]);
    assert_eq!(
        declared.verdict(&web, &api, traffic(80)).decision,
        Decision::Allow
    );
    assert_eq!(
        declared.verdict(&world(), &api, traffic(80)).decision,
        Decision::Deny
    );

    let from_named = cnp(json!({
        "metadata": { "name": "from-reserved", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEntities": [
                    "host", "remote-node", "init", "ingress", "unmanaged", "kube-apiserver"
                ]
            }]
        }
    }));
    let declared = declare(&[from_named]);
    for peer in [&host, &remote, &init, &ingress, &unmanaged, &apiserver] {
        assert_eq!(
            declared.verdict(peer, &api, traffic(80)).decision,
            Decision::Allow,
            "entity peer {}",
            peer.name
        );
    }
    assert_eq!(
        declared.verdict(&web, &api, traffic(80)).decision,
        Decision::Deny
    );
}

#[test]
fn clusterwide_and_namespaced_policies_apply_together() {
    let src = endpoint("clients", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let cluster = ccnp(json!({
        "metadata": { "name": "lock-all" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": []
        }
    }));
    let allow = cnp(json!({
        "metadata": { "name": "allow-web", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{
                    "matchLabels": {
                        "app": "web",
                        "io.kubernetes.pod.namespace": "clients"
                    }
                }]
            }]
        }
    }));
    let locked = declare(std::slice::from_ref(&cluster));
    assert_eq!(
        locked.verdict(&src, &dst, traffic(80)).decision,
        Decision::Deny,
        "a clusterwide empty ingress isolates every selected pod"
    );
    let together = declare(&[cluster, allow]).verdict(&src, &dst, traffic(80));
    assert_eq!(together.decision, Decision::Allow);
    assert!(together.reasons.iter().any(|reason| matches!(
        reason,
        VerdictReason::AllowedByPolicy {
            name,
            clusterwide: false,
            ..
        } if name == "allow-web"
    )));
}

#[test]
fn to_cidr_set_matches_peer_ips_and_honours_except() {
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let admitted = endpoint_with("other", "ok", &[], None, &["10.2.3.4"]);
    let excluded = endpoint_with("other", "no", &[], None, &["10.1.2.3"]);
    let policy = cnp(json!({
        "metadata": { "name": "cidrs", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromCIDRSet": [{ "cidr": "10.0.0.0/8", "except": ["10.1.0.0/16"] }]
            }]
        }
    }));
    let declared = declare(&[policy]);
    assert_eq!(
        declared.verdict(&admitted, &dst, traffic(80)).decision,
        Decision::Allow
    );
    assert_eq!(
        declared.verdict(&excluded, &dst, traffic(80)).decision,
        Decision::Deny
    );
}

#[test]
fn declared_l7_http_is_labelled_declared_and_is_not_observed() {
    let src = endpoint("prod", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let policy = cnp(json!({
        "metadata": { "name": "http-get", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "toPorts": [{
                    "ports": [{ "port": "80", "protocol": "TCP" }],
                    "rules": { "http": [{ "method": "GET", "path": "/public" }] }
                }]
            }]
        }
    }));
    let declared = declare(&[policy]);
    let l4 = declared.verdict(&src, &dst, traffic(80));
    assert_eq!(l4.decision, Decision::Allow);
    assert!(
        l4.reasons.iter().any(|reason| matches!(
            reason,
            VerdictReason::AllowedByPolicy {
                declared_l7: true,
                ..
            }
        )),
        "L7 on the rule is declared, not a Hubble HTTP flow: {:?}",
        l4.reasons
    );

    let get = declared.verdict(
        &src,
        &dst,
        Traffic {
            protocol: Protocol::Tcp,
            port: 80,
            l7: Some(DeclaredL7 {
                method: "GET".into(),
                path: "/public/v1".into(),
            }),
        },
    );
    assert_eq!(get.decision, Decision::Allow);

    let post = declared.verdict(
        &src,
        &dst,
        Traffic {
            protocol: Protocol::Tcp,
            port: 80,
            l7: Some(DeclaredL7 {
                method: "POST".into(),
                path: "/public".into(),
            }),
        },
    );
    assert_eq!(post.decision, Decision::Deny);

    let wrong_port = declared.verdict(&src, &dst, traffic(443));
    assert_eq!(wrong_port.decision, Decision::Deny);
}

#[test]
fn omitted_to_ports_protocol_is_any_not_tcp() {
    let src = endpoint("prod", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let policy = cnp(json!({
        "metadata": { "name": "any-proto", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "toPorts": [{ "ports": [{ "port": "53" }] }]
            }]
        }
    }));
    let declared = declare(&[policy]);
    assert_eq!(
        declared
            .verdict(
                &src,
                &dst,
                Traffic {
                    protocol: Protocol::Udp,
                    port: 53,
                    l7: None,
                }
            )
            .decision,
        Decision::Allow,
        "Cilium omits protocol as ANY, not Kubernetes TCP"
    );
}

#[test]
fn same_endpoint_is_allowed_even_when_isolated() {
    let pod = endpoint("prod", "api", &[("app", "api")]);
    let policy = cnp(json!({
        "metadata": { "name": "lock", "namespace": "prod" },
        "spec": {
            "endpointSelector": {},
            "ingress": [],
            "egress": []
        }
    }));
    let verdict = declare(&[policy]).verdict(&pod, &pod, traffic(80));
    assert_eq!(verdict.decision, Decision::Allow);
    assert_eq!(verdict.reasons, vec![VerdictReason::SameEndpoint]);
}

#[test]
fn truncation_cannot_produce_a_definitive_deny() {
    let src = endpoint("prod", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let mut policies = vec![cnp(json!({
        "metadata": { "name": "lock", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": []
        }
    }))];
    policies.extend((1..MAX_POLICIES).map(|index| {
        cnp(json!({
            "metadata": { "name": format!("filler-{index}"), "namespace": "filler" },
            "spec": {
                "endpointSelector": {},
                "ingress": []
            }
        }))
    }));
    policies.push(cnp(json!({
        "metadata": { "name": "allow-late", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{}]
        }
    })));
    let declared = declare(&policies);
    let verdict = declared.verdict(&src, &dst, traffic(80));
    assert!(matches!(
        verdict.completeness,
        Completeness::Truncated { .. }
    ));
    assert_eq!(verdict.decision, Decision::Indeterminate);
    assert_eq!(verdict.allowed(), None);
}

#[test]
fn a_real_shaped_cnp_parses_spec_and_specs() {
    let doc = json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumNetworkPolicy",
        "metadata": { "name": "allow-web", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "fromEntities": ["cluster"],
                "toPorts": [{
                    "ports": [{ "port": "80", "protocol": "TCP" }],
                    "rules": { "http": [{ "method": "GET", "path": "/public" }] }
                }]
            }],
            "egress": [{
                "toEntities": ["world"],
                "toCIDRSet": [{ "cidr": "10.0.0.0/8", "except": ["10.1.0.0/16"] }]
            }]
        },
        "specs": [{
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{ "fromEntities": ["host"] }]
        }]
    });
    let policies = parse_policy_document(&doc);
    assert_eq!(policies.len(), 2);
    assert!(!policies[0].clusterwide);
    assert_eq!(policies[0].namespace, "prod");
    let ingress = policies[0].ingress.as_ref().expect("ingress present");
    assert_eq!(ingress[0].entities.as_ref().unwrap()[0], Entity::Cluster);
    assert_eq!(ingress[0].http[0].method, "GET");
    assert_eq!(
        policies[0].egress.as_ref().unwrap()[0]
            .entities
            .as_ref()
            .unwrap()[0],
        Entity::World
    );
    assert_eq!(
        policies[1].ingress.as_ref().unwrap()[0]
            .entities
            .as_ref()
            .unwrap()[0],
        Entity::Host
    );
}

#[test]
fn a_to_fqdns_rule_never_matches_a_peer_and_leaves_isolation_unproven() {
    let src = endpoint("prod", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let fqdn_only = cnp(json!({
        "metadata": { "name": "dns-allow", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "web" } },
            "egress": [{
                "toFQDNs": [{ "matchName": "api.example.com" }],
                "toPorts": [{ "ports": [{ "port": "443", "protocol": "TCP" }] }]
            }]
        }
    }));
    let verdict = declare(&[fqdn_only]).verdict(
        &src,
        &dst,
        Traffic {
            protocol: Protocol::Tcp,
            port: 443,
            l7: None,
        },
    );
    assert_eq!(
        verdict.decision,
        Decision::Indeterminate,
        "a toFQDNs rule allows one hostname, not every peer on TCP/443: {verdict:?}"
    );
    assert_eq!(verdict.allowed(), None);
    assert!(verdict.reasons.contains(&VerdictReason::IsolationUnproven {
        direction: Direction::Egress,
        selecting_policies: 1,
    }));

    let with_witness = cnp(json!({
        "metadata": { "name": "dns-and-api", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "web" } },
            "egress": [
                { "toFQDNs": [{ "matchName": "api.example.com" }] },
                { "toEndpoints": [{ "matchLabels": { "app": "api" } }] }
            ]
        }
    }));
    assert_eq!(
        declare(&[with_witness])
            .verdict(&src, &dst, traffic(443))
            .decision,
        Decision::Allow,
        "a sibling rule this module can evaluate still proves the allow"
    );
}

#[test]
fn a_from_nodes_rule_is_not_an_allow_for_a_pod_peer() {
    let src = endpoint("prod", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let policy = cnp(json!({
        "metadata": { "name": "from-infra-nodes", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{ "fromNodes": [{ "matchLabels": { "role": "infra" } }] }]
        }
    }));
    let verdict = declare(&[policy]).verdict(&src, &dst, traffic(80));
    assert_eq!(
        verdict.decision,
        Decision::Indeterminate,
        "node-based L3 is not evaluated, so it neither allows nor proves isolation: {verdict:?}"
    );
    assert!(verdict.reasons.contains(&VerdictReason::IsolationUnproven {
        direction: Direction::Ingress,
        selecting_policies: 1,
    }));
}

#[test]
fn an_uncompilable_to_ports_entry_fails_closed_instead_of_widening_to_every_port() {
    let web = endpoint("prod", "web", &[("app", "web")]);
    let job = endpoint("prod", "job", &[("app", "job")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let inverted = cnp(json!({
        "metadata": { "name": "bad-range", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "toPorts": [{ "ports": [{ "port": "80", "endPort": 79, "protocol": "TCP" }] }]
            }]
        }
    }));
    let declared = declare(&[inverted]);
    let unrelated = declared.verdict(&web, &dst, traffic(22));
    assert_eq!(
        unrelated.decision,
        Decision::Indeterminate,
        "an inverted endPort is not \"every port\": {unrelated:?}"
    );
    assert!(
        unrelated
            .reasons
            .contains(&VerdictReason::IsolationUnproven {
                direction: Direction::Ingress,
                selecting_policies: 1,
            })
    );
    assert_eq!(
        declared.verdict(&job, &dst, traffic(22)).decision,
        Decision::Deny,
        "a peer the rule's L3 does not admit stays a proven deny"
    );

    let unknown_protocol = cnp(json!({
        "metadata": { "name": "future-proto", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "toPorts": [{ "ports": [{ "port": "80", "protocol": "VRRP" }] }]
            }]
        }
    }));
    assert_eq!(
        declare(&[unknown_protocol])
            .verdict(&web, &dst, traffic(80))
            .decision,
        Decision::Indeterminate,
        "an unknown protocol drops the entry without widening the rule"
    );

    let zero_port = cnp(json!({
        "metadata": { "name": "zero-port", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "toPorts": [{ "ports": [{ "port": 0, "protocol": "TCP" }] }]
            }]
        }
    }));
    assert_eq!(
        declare(&[zero_port])
            .verdict(&web, &dst, traffic(22))
            .decision,
        Decision::Indeterminate,
        "a malformed port is not an omitted one"
    );

    let mixed = cnp(json!({
        "metadata": { "name": "one-good-one-bad", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "toPorts": [{ "ports": [
                    { "port": "80", "protocol": "TCP" },
                    { "port": "90", "endPort": 89, "protocol": "TCP" }
                ] }]
            }]
        }
    }));
    assert_eq!(
        declare(&[mixed]).verdict(&web, &dst, traffic(80)).decision,
        Decision::Allow,
        "a compiled sibling entry that matches still proves the allow"
    );
}

#[test]
fn a_namespace_labels_selector_crosses_namespaces_without_inventing_values() {
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let ally = endpoint(
        "rebel-base",
        "web",
        &[("k8s:io.cilium.k8s.namespace.labels.faction", "alliance")],
    );
    let stranger = endpoint("empire", "web", &[]);
    let named_alliance = endpoint("alliance", "web", &[]);
    let policy = cnp(json!({
        "metadata": { "name": "allow-faction", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{
                    "matchLabels": { "io.cilium.k8s.namespace.labels.faction": "alliance" }
                }]
            }]
        }
    }));
    let declared = declare(&[policy]);
    assert_eq!(
        declared.verdict(&ally, &dst, traffic(80)).decision,
        Decision::Allow,
        "namespace labels on the selector lift the same-namespace gate"
    );
    assert_eq!(
        declared.verdict(&stranger, &dst, traffic(80)).decision,
        Decision::Deny
    );
    assert_eq!(
        declared
            .verdict(&named_alliance, &dst, traffic(80))
            .decision,
        Decision::Deny,
        "the namespace name never satisfies a namespace-labels key"
    );
}

#[test]
fn enable_default_deny_false_contributes_allows_without_isolating() {
    let web = endpoint("prod", "web", &[("app", "web")]);
    let job = endpoint("prod", "job", &[("app", "job")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let visibility = cnp(json!({
        "metadata": { "name": "visibility", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "enableDefaultDeny": { "ingress": false },
            "ingress": [{ "fromEndpoints": [{ "matchLabels": { "app": "web" } }] }]
        }
    }));
    let alone = declare(std::slice::from_ref(&visibility));
    let unmatched = alone.verdict(&job, &dst, traffic(80));
    assert_eq!(
        unmatched.decision,
        Decision::Allow,
        "a non-isolating policy leaves an unmatched peer at default allow: {unmatched:?}"
    );
    assert!(unmatched.reasons.contains(&VerdictReason::DefaultAllow {
        direction: Direction::Ingress,
    }));

    let lock = cnp(json!({
        "metadata": { "name": "lock", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": []
        }
    }));
    let together = declare(&[lock, visibility]);
    assert_eq!(
        together.verdict(&web, &dst, traffic(80)).decision,
        Decision::Allow,
        "the non-isolating policy still supplies a witness against another policy's isolation"
    );
    assert_eq!(
        together.verdict(&job, &dst, traffic(80)).decision,
        Decision::Deny
    );
}

#[test]
fn incomplete_inventory_needs_both_directions_proven() {
    let src = endpoint("clients", "web", &[("app", "web")]);
    let dst = endpoint("prod", "api", &[("app", "api")]);
    let ingress_witness = cnp(json!({
        "metadata": { "name": "allow-web", "namespace": "prod" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{
                    "matchLabels": {
                        "app": "web",
                        "io.kubernetes.pod.namespace": "clients"
                    }
                }]
            }]
        }
    }));

    let mut half_proven = declare(std::slice::from_ref(&ingress_witness));
    half_proven.mark_incomplete();
    let verdict = half_proven.verdict(&src, &dst, traffic(80));
    assert_eq!(
        verdict.decision,
        Decision::Indeterminate,
        "a default-allow egress is not proof while the inventory is incomplete: {verdict:?}"
    );
    assert!(
        verdict
            .reasons
            .contains(&VerdictReason::InventoryIncomplete)
    );

    let egress_witness = cnp(json!({
        "metadata": { "name": "allow-out", "namespace": "clients" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "web" } },
            "egress": [{
                "toEndpoints": [{
                    "matchLabels": {
                        "app": "api",
                        "io.kubernetes.pod.namespace": "prod"
                    }
                }]
            }]
        }
    }));
    let mut both_proven = declare(&[ingress_witness, egress_witness]);
    both_proven.mark_incomplete();
    let verdict = both_proven.verdict(&src, &dst, traffic(80));
    assert_eq!(
        verdict.decision,
        Decision::Allow,
        "two explicit witnesses prove the pair even while incomplete: {verdict:?}"
    );
}
