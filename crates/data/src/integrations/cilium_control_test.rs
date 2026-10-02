//! Field extraction, caps, the document, 404/403 classification, and the
//! planted Envoy secret. A cluster is not required.

use super::*;
use serde_json::json;

const PLANTED: &str = "PLANTED_SECRET_do_not_leak";

fn cec_json() -> Value {
    json!({
        "metadata": { "name": "edge", "namespace": "prod", "uid": "uid-cec" },
        "spec": {
            "services": [
                { "name": "frontend", "namespace": "prod" },
                { "name": "bare" }
            ],
            "resources": [{
                "@type": "type.googleapis.com/envoy.config.listener.v3.Listener",
                "name": "listener",
                "typed_config": {
                    "private_key": PLANTED,
                    "inline_string": PLANTED
                }
            }]
        }
    })
}

fn resource(kind: Kind, value: Value) -> Resource {
    parse_object(kind, kind.version(), &value).expect("the fixture is a cilium.io object")
}

fn leak_haystack(inventory: &Inventory) -> String {
    let mut text = format!("{inventory:?}");
    text.push_str(&render(inventory).join("\n"));
    if let Some(page) = table_page(inventory) {
        for row in &page.rows {
            for cell in &row.cells {
                text.push_str(cell);
            }
        }
    }
    text
}

#[test]
fn from_api_kind_skips_policy_identity_endpoint_and_node() {
    for name in [
        "CiliumNetworkPolicy",
        "CiliumClusterwideNetworkPolicy",
        "CiliumEndpoint",
        "CiliumIdentity",
        "CiliumNode",
        "Gateway",
        "HTTPRoute",
        "GatewayClass",
    ] {
        assert_eq!(
            Kind::from_api_kind(name),
            None,
            "{name} is not a control-plane inventory kind"
        );
    }
    assert_eq!(
        Kind::from_api_kind("CiliumEnvoyConfig"),
        Some(Kind::CiliumEnvoyConfig)
    );
    assert_eq!(Kind::ALL.len(), 18);
}

#[test]
fn each_kind_scope_matches_the_upstream_crd_markers() {
    // scope per the kubebuilder markers in cilium/cilium
    // pkg/k8s/apis/cilium.io; CiliumEndpointSlice is scope="Cluster".
    let namespaced = [
        Kind::CiliumEnvoyConfig,
        Kind::CiliumLocalRedirectPolicy,
        Kind::CiliumNodeConfig,
    ];
    for kind in Kind::ALL {
        assert_eq!(
            kind.namespaced(),
            namespaced.contains(&kind),
            "{} scope disagrees with the upstream marker",
            kind.as_str()
        );
    }
}

#[test]
fn each_kind_fallback_version_matches_the_cilium_1_18_registers() {
    // per cilium/cilium v1.18 pkg/k8s/apis/cilium.io/{v2,v2alpha1}/register.go:
    // only these five kinds are absent from the v2 register.
    let v2alpha1 = [
        Kind::CiliumL2AnnouncementPolicy,
        Kind::CiliumPodIPPool,
        Kind::CiliumEndpointSlice,
        Kind::CiliumBGPPeeringPolicy,
        Kind::CiliumGatewayClassConfig,
    ];
    for kind in Kind::ALL {
        let want = if v2alpha1.contains(&kind) {
            "v2alpha1"
        } else {
            "v2"
        };
        assert_eq!(
            kind.version(),
            want,
            "{} fallback version disagrees with the upstream register",
            kind.as_str()
        );
    }
}

#[test]
fn a_denied_version_document_leaves_unanswered_kinds_denied_not_absent() {
    // v2 answered 403 while v2alpha1 served: the kinds only the denied
    // document could have named must not read as "not installed".
    let mut inventory = Inventory {
        group: GroupState::Served,
        cidr_groups: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 0,
        },
        load_balancer_ip_pools: KindSet::Denied,
        ..Inventory::default()
    };
    deny_unanswered(&mut inventory);
    assert!(
        matches!(inventory.envoy_configs, KindSet::Denied),
        "an unanswered kind is Denied, not NotServed"
    );
    assert!(
        matches!(inventory.cidr_groups, KindSet::Served { .. }),
        "a kind the served document answered stays Served"
    );
    assert!(matches!(inventory.load_balancer_ip_pools, KindSet::Denied));
}

#[test]
fn named_kinds_keep_only_the_kinds_this_module_lists() {
    let doc = json!({
        "kind": "APIResourceList",
        "groupVersion": "cilium.io/v2",
        "resources": [
            {"name": "ciliumenvoyconfigs", "kind": "CiliumEnvoyConfig", "namespaced": true, "verbs": ["list"]},
            {"name": "ciliumnetworkpolicies", "kind": "CiliumNetworkPolicy", "namespaced": true, "verbs": ["list"]},
            {"name": "ciliumendpoints", "kind": "CiliumEndpoint", "namespaced": true, "verbs": ["list"]},
            {"name": "ciliumidentities", "kind": "CiliumIdentity", "namespaced": false, "verbs": ["list"]},
            {"name": "ciliumnodes", "kind": "CiliumNode", "namespaced": false, "verbs": ["list"]},
            {"name": "ciliumclusterwidenetworkpolicies", "kind": "CiliumClusterwideNetworkPolicy", "namespaced": false, "verbs": ["list"]},
            {"name": "ciliumenvoyconfigs/status", "kind": "CiliumEnvoyConfig", "namespaced": true, "verbs": ["get"]}
        ]
    });
    assert_eq!(named_kinds(&doc), vec![Kind::CiliumEnvoyConfig]);
}

#[test]
fn a_cec_keeps_the_service_selector_and_drops_envoy_resources() {
    let item = resource(Kind::CiliumEnvoyConfig, cec_json());
    assert_eq!(item.name, "edge");
    assert_eq!(item.namespace, "prod");
    assert_eq!(item.note, "prod/frontend, bare");
    let debug = format!("{item:?}");
    assert!(!debug.contains(PLANTED), "{debug}");
    assert!(!debug.contains("typed_config"), "{debug}");
    assert!(!debug.contains("Listener"), "{debug}");
}

#[test]
fn a_clusterwide_cec_is_the_same_shape_without_a_namespace() {
    let mut value = cec_json();
    value["metadata"]["namespace"] = json!("");
    value["metadata"]["name"] = json!("cluster-edge");
    let item = resource(Kind::CiliumClusterwideEnvoyConfig, value);
    assert_eq!(item.name, "cluster-edge");
    assert!(item.namespace.is_empty());
    assert_eq!(item.note, "prod/frontend, bare");
}

#[test]
fn a_local_redirect_keeps_the_frontend_service() {
    let item = resource(
        Kind::CiliumLocalRedirectPolicy,
        json!({
            "metadata": { "name": "lrp", "namespace": "prod" },
            "spec": {
                "redirectFrontend": {
                    "serviceMatcher": { "serviceName": "metadata", "namespace": "kube-system" }
                }
            }
        }),
    );
    assert_eq!(item.note, "kube-system/metadata");
}

#[test]
fn an_egress_gateway_keeps_node_selector_cidrs_and_the_spec_egress_ip() {
    // Upstream declares the IP as spec.egressGateway.egressIP; there is no
    // status field carrying it.
    let item = resource(
        Kind::CiliumEgressGatewayPolicy,
        json!({
            "metadata": { "name": "egress" },
            "spec": {
                "destinationCIDRs": ["1.1.1.1/32", "8.8.8.0/24"],
                "egressGateway": {
                    "nodeSelector": { "matchLabels": { "role": "egress" } },
                    "egressIP": "192.168.1.10"
                }
            }
        }),
    );
    assert!(item.note.contains("role=egress"), "{}", item.note);
    assert!(item.note.contains("1.1.1.1/32"), "{}", item.note);
    assert!(item.note.contains("192.168.1.10"), "{}", item.note);
}

#[test]
fn an_egress_gateway_ip_is_read_from_the_multi_gateway_shape_or_status() {
    let multi = resource(
        Kind::CiliumEgressGatewayPolicy,
        json!({
            "metadata": { "name": "multi" },
            "spec": {
                "egressGateways": [{
                    "nodeSelector": { "matchLabels": { "role": "egress" } },
                    "egressIP": "10.168.60.100"
                }]
            }
        }),
    );
    assert!(multi.note.contains("10.168.60.100"), "{}", multi.note);

    let status_only = resource(
        Kind::CiliumEgressGatewayPolicy,
        json!({
            "metadata": { "name": "fallback" },
            "spec": {},
            "status": { "egressIP": "192.168.1.11" }
        }),
    );
    assert!(
        status_only.note.contains("192.168.1.11"),
        "{}",
        status_only.note
    );
}

#[test]
fn a_cidr_group_is_a_count_not_every_ip() {
    let item = resource(
        Kind::CiliumCIDRGroup,
        json!({
            "metadata": { "name": "office" },
            "spec": { "externalCIDRs": ["10.0.0.0/8", "192.168.1.0/24", "172.16.0.0/12"] }
        }),
    );
    assert_eq!(item.note, "3 CIDRs");
    assert!(!item.note.contains("10.0.0.0"));
    assert!(!item.note.contains("192.168.1.0"));
}

#[test]
fn a_load_balancer_pool_clips_blocks_and_names_disabled() {
    let item = resource(
        Kind::CiliumLoadBalancerIPPool,
        json!({
            "metadata": { "name": "first" },
            "spec": {
                "disabled": true,
                "blocks": [
                    { "cidr": "10.10.10.0/24" },
                    { "start": "20.0.20.100", "stop": "20.0.20.200" }
                ]
            }
        }),
    );
    assert!(item.note.contains("10.10.10.0/24"), "{}", item.note);
    assert!(
        item.note.contains("20.0.20.100-20.0.20.200"),
        "{}",
        item.note
    );
    assert!(item.note.contains("disabled"), "{}", item.note);
}

#[test]
fn a_pod_ip_pool_counts_family_cidrs() {
    let item = resource(
        Kind::CiliumPodIPPool,
        json!({
            "metadata": { "name": "pool" },
            "spec": {
                "ipv4": { "cidrs": ["10.10.0.0/16", "10.11.0.0/16"], "maskSize": 24 },
                "ipv6": { "cidrs": ["fd00::/80"], "maskSize": 96 }
            }
        }),
    );
    assert_eq!(item.note, "ipv4 2 CIDRs, ipv6 1 CIDR");
    assert!(!item.note.contains("10.10.0.0"));
}

#[test]
fn a_node_config_keeps_the_selector_and_not_defaults() {
    let item = resource(
        Kind::CiliumNodeConfig,
        json!({
            "metadata": { "name": "bgp", "namespace": "kube-system" },
            "spec": {
                "nodeSelector": { "matchLabels": { "rack": "a" } },
                "defaults": { "token": PLANTED, "enable-bgp-control-plane": "true" }
            }
        }),
    );
    assert_eq!(item.note, "rack=a");
    assert!(!format!("{item:?}").contains(PLANTED));
}

#[test]
fn an_endpoint_slice_is_a_count_and_identity_refs() {
    let item = resource(
        Kind::CiliumEndpointSlice,
        json!({
            "metadata": { "name": "ces" },
            "endpoints": [
                { "name": "pod-a", "identityID": 111, "networking": { "addressing": [{ "ipv4": "10.0.0.7" }] } },
                { "name": "pod-b", "id": 222, "networking": { "addressing": [{ "ipv4": "10.0.0.8" }] } },
                { "name": "pod-c", "identity": { "id": 111 } }
            ]
        }),
    );
    assert!(item.note.starts_with("3 endpoints"), "{}", item.note);
    assert!(item.note.contains("111"), "{}", item.note);
    assert!(item.note.contains("222"), "{}", item.note);
    assert!(!item.note.contains("10.0.0.7"));
    assert!(!item.note.contains("pod-a"));
}

#[test]
fn endpoint_identity_refs_stop_at_the_ref_ceiling() {
    let endpoints: Vec<Value> = (0..20)
        .map(|i| json!({ "identityID": 1000 + i, "networking": { "addressing": [{ "ipv4": format!("10.0.0.{i}") }] } }))
        .collect();
    let item = resource(
        Kind::CiliumEndpointSlice,
        json!({ "metadata": { "name": "ces" }, "endpoints": endpoints }),
    );
    assert!(item.note.starts_with("20 endpoints"), "{}", item.note);
    assert!(item.note.contains('\u{2026}'), "{}", item.note);
    assert!(!item.note.contains("10.0.0."));
    assert!(
        !item.note.contains("1016"),
        "only the first refs: {}",
        item.note
    );
}

#[test]
fn l2_bgp_and_gateway_class_notes_stay_selectors_and_counts() {
    let l2 = resource(
        Kind::CiliumL2AnnouncementPolicy,
        json!({
            "metadata": { "name": "l2" },
            "spec": {
                "nodeSelector": { "matchLabels": { "pool": "edge" } },
                "serviceSelector": { "matchLabels": { "color": "blue" } },
                "loadBalancerIPs": true
            }
        }),
    );
    assert!(l2.note.contains("pool=edge"), "{}", l2.note);
    assert!(l2.note.contains("color=blue"), "{}", l2.note);
    assert!(l2.note.contains("loadBalancerIPs"), "{}", l2.note);

    let bgp = resource(
        Kind::CiliumBGPClusterConfig,
        json!({
            "metadata": { "name": "rack" },
            "spec": {
                "nodeSelector": { "matchLabels": { "rack": "0" } },
                "bgpInstances": [{ "name": "i", "localASN": 65001, "peers": [{ "name": "p" }, { "name": "q" }] }]
            }
        }),
    );
    assert!(bgp.note.contains("ASN 65001"), "{}", bgp.note);
    assert!(bgp.note.contains("2 peers"), "{}", bgp.note);

    let peer = resource(
        Kind::CiliumBGPPeerConfig,
        json!({
            "metadata": { "name": "peer" },
            "spec": {
                "authSecretRef": "bgp-auth",
                "families": [{ "afi": "ipv4", "safi": "unicast" }]
            }
        }),
    );
    assert_eq!(peer.note, "authSecretRef bgp-auth  ipv4/unicast");

    let adv = resource(
        Kind::CiliumBGPAdvertisement,
        json!({
            "metadata": { "name": "adv" },
            "spec": { "advertisements": [{ "advertisementType": "PodCIDR" }, { "advertisementType": "Service" }] }
        }),
    );
    assert_eq!(adv.note, "PodCIDR, Service");

    let node = resource(
        Kind::CiliumBGPNodeConfig,
        json!({
            "metadata": { "name": "n1" },
            "spec": { "bgpInstances": [{ "name": "i" }] },
            "status": { "bgpInstances": [{ "peers": [{ "name": "core", "peeringState": "established" }] }] }
        }),
    );
    assert_eq!(node.note, "core=established");

    let node_override = resource(
        Kind::CiliumBGPNodeConfigOverride,
        json!({
            "metadata": { "name": "worker-1" },
            "spec": {
                "bgpInstances": [{ "name": "instance-65000", "routerID": "192.168.10.1", "localPort": 1790 }]
            }
        }),
    );
    assert_eq!(node_override.note, "routerID 192.168.10.1, localPort 1790");

    let peering = resource(
        Kind::CiliumBGPPeeringPolicy,
        json!({
            "metadata": { "name": "legacy" },
            "spec": {
                "virtualRouters": [{ "localASN": 64512, "neighbors": [{ "peerAddress": "10.0.0.1/32" }] }]
            }
        }),
    );
    assert!(peering.note.contains("ASN 64512"), "{}", peering.note);
    assert!(peering.note.contains("1 neighbor"), "{}", peering.note);

    let gcc = resource(
        Kind::CiliumGatewayClassConfig,
        json!({
            "metadata": { "name": "cilium" },
            "spec": { "description": "Cilium Gateway API", "service": { "type": "LoadBalancer" } }
        }),
    );
    assert_eq!(gcc.note, "LoadBalancer  Cilium Gateway API");
}

#[test]
fn an_external_workload_prefers_status_ip() {
    let item = resource(
        Kind::CiliumExternalWorkload,
        json!({
            "metadata": { "name": "vm1" },
            "spec": { "ipv4-alloc-cidr": "10.192.1.0/30" },
            "status": { "ip": "10.192.1.1" }
        }),
    );
    assert_eq!(item.note, "10.192.1.1");
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_object(Kind::CiliumCIDRGroup, "v2alpha1", &json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = json!({
        "metadata": { "name": huge, "namespace": huge },
        "spec": { "services": [{ "name": huge, "namespace": huge }] }
    });
    let item = resource(Kind::CiliumEnvoyConfig, value);
    for field in [&item.name, &item.namespace, &item.note] {
        assert!(
            field.chars().count() <= MAX_FIELD_CHARS + 1,
            "every field is clipped where it is carried: {} chars",
            field.chars().count()
        );
        assert!(field.ends_with('\u{2026}'), "and looks clipped");
    }
}

#[test]
fn a_kind_listing_stops_at_the_object_ceiling() {
    let items: Vec<Value> = (0..MAX_OBJECTS + 5)
        .map(|i| json!({ "metadata": { "name": format!("n{i}") } }))
        .collect();
    let (kept, unreadable, truncated) = ingest_items(Kind::CiliumCIDRGroup, "v2alpha1", items, 0);
    assert_eq!(kept.len(), MAX_OBJECTS);
    assert_eq!(unreadable, 0);
    assert!(truncated);
}

#[test]
fn a_missing_group_is_invisible_and_a_forbidden_one_is_denied() {
    assert!(matches!(
        after_group(&api_error(404)),
        GroupAnswer::NotServed
    ));
    assert!(
        matches!(after_group(&api_error(403)), GroupAnswer::Denied),
        "a 403 is Denied, never an empty inventory that looks like Cilium is absent"
    );
    assert!(matches!(after_group(&api_error(401)), GroupAnswer::Denied));
    assert!(matches!(
        after_group(&api_error(500)),
        GroupAnswer::Failed(_)
    ));
    assert!(matches!(
        after_version(&api_error(404)),
        VersionAnswer::NotFound
    ));
    assert!(matches!(
        after_version(&api_error(403)),
        VersionAnswer::Denied
    ));
    assert!(matches!(after_list(&api_error(404)), ListErr::NotFound));
    assert!(matches!(after_list(&api_error(403)), ListErr::Denied));
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn an_unserved_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "a 404 group is absence, not an empty list"
    );
}

#[test]
fn a_served_group_with_no_named_kinds_is_an_empty_table() {
    let page = table_page(&Inventory {
        group: GroupState::Served,
        ..Inventory::default()
    })
    .expect("a served group is a table even when it named none of our kinds");
    assert!(page.rows.is_empty());
}

#[test]
fn a_denied_group_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        group: GroupState::Denied,
        ..Inventory::default()
    })
    .expect("Denied is served, so the table exists");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("access denied for this account"),
        "a 403 stays labelled: {text}"
    );
    assert!(text.contains("cilium.io"), "{text}");
}

#[test]
fn a_served_fixture_is_one_row_and_does_not_leak_the_planted_secret() {
    let item = resource(Kind::CiliumEnvoyConfig, cec_json());
    let inventory = Inventory {
        group: GroupState::Served,
        envoy_configs: KindSet::Served {
            items: vec![item],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    let page = table_page(&inventory).expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "edge");
    assert_eq!(page.rows[0].cells[0], "CiliumEnvoyConfig");
    assert_eq!(page.rows[0].cells[3], "prod/frontend, bare");
    let haystack = leak_haystack(&inventory);
    assert!(
        !haystack.contains(PLANTED),
        "the planted typed_config must not leak: {haystack}"
    );
    assert!(!haystack.contains("typed_config"));
}

#[test]
fn a_missing_group_renders_as_not_installed_rather_than_empty() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served());
    assert_eq!(
        lines[0],
        "Cilium control-plane CRs are not served by this cluster"
    );
    let text = lines.join("\n");
    assert!(text.contains("nothing is installed to find them"), "{text}");
}

#[test]
fn a_history_renders_notes_caps_and_denials() {
    let cec = resource(Kind::CiliumEnvoyConfig, cec_json());
    let cidr = resource(
        Kind::CiliumCIDRGroup,
        json!({
            "metadata": { "name": "office" },
            "spec": { "externalCIDRs": ["10.0.0.0/8"] }
        }),
    );
    let lines = render(&Inventory {
        group: GroupState::Served,
        envoy_configs: KindSet::Served {
            items: vec![cec],
            truncated: true,
            unreadable: 2,
        },
        cidr_groups: KindSet::Served {
            items: vec![cidr],
            truncated: false,
            unreadable: 0,
        },
        load_balancer_ip_pools: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.starts_with("2 Cilium control-plane objects"), "{text}");
    assert!(text.contains("prod/edge"), "{text}");
    assert!(
        text.contains("CiliumEnvoyConfig  prod/frontend, bare"),
        "{text}"
    );
    assert!(text.contains("CiliumCIDRGroup  1 CIDR"), "{text}");
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
    assert!(
        text.contains("2 Cilium control-plane objects could not be decoded and are not shown"),
        "{text}"
    );
    assert!(
        text.contains("cilium load balancer ip pools: access denied for this account"),
        "{text}"
    );
    assert!(!text.contains(PLANTED), "{text}");
    assert!(
        !text.contains("CiliumNetworkPolicy"),
        "declared policy is not this document: {text}"
    );
}
