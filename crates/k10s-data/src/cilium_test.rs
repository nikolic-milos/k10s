use super::*;
use crate::mesh::{ObservedReach, TelemetryExporter, TelemetryReason};
use crate::prom::{QueryResult, ResultType, Series};
use serde_json::json;

fn cnp_json() -> Value {
    json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumNetworkPolicy",
        "metadata": {
            "name": "allow-web",
            "namespace": "prod",
            "uid": "uid-cnp"
        },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{
                "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                "toPorts": [{
                    "ports": [{ "port": "80", "protocol": "TCP" }],
                    "rules": { "http": [{ "method": "GET", "path": "/public" }] }
                }]
            }],
            "egress": [{
                "toEntities": ["world"],
                "toCIDRSet": [{ "cidr": "10.0.0.0/8", "except": ["10.1.0.0/16"] }]
            }]
        }
    })
}

fn identity_json() -> Value {
    json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumIdentity",
        "metadata": {
            "name": "12345",
            "uid": "uid-cid",
            "labels": {
                "k8s:io.kubernetes.pod.namespace": "prod",
                "k8s:k8s-app": "api",
                "k8s:app": "api"
            }
        }
    })
}

fn endpoint_json() -> Value {
    json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumEndpoint",
        "metadata": {
            "name": "api-0",
            "namespace": "prod",
            "uid": "uid-cep"
        },
        "status": {
            "identity": {
                "id": 12345,
                "labels": [
                    "k8s:io.kubernetes.pod.namespace=prod",
                    "k8s:k8s-app=api"
                ]
            },
            "networking": {
                "addressing": [{ "ipv4": "10.0.0.20", "ipv6": "2001:db8::20" }]
            },
            "state": "ready"
        }
    })
}

fn node_json() -> Value {
    json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumNode",
        "metadata": { "name": "worker-1", "uid": "uid-node" },
        "spec": {
            "addresses": [
                { "type": "InternalIP", "ip": "192.168.1.10" },
                { "type": "CiliumInternalIP", "ip": "10.0.0.1" }
            ],
            "nodeidentity": 6
        }
    })
}

fn resource(kind: Kind, value: &Value) -> Resource {
    parse_item(kind, "v2", value)
        .expect("the fixture is a Cilium object")
        .0
}

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(
        kube::core::Status::failure("scripted", "Scripted").with_code(code),
    ))
}

#[test]
fn a_real_shaped_cnp_keeps_inventory_fields_and_declared_l7() {
    let item = resource(Kind::CiliumNetworkPolicy, &cnp_json());
    assert_eq!(item.name, "allow-web");
    assert_eq!(item.namespace, "prod");
    assert_eq!(item.uid, "uid-cnp");
    assert!(item.detail.contains("1 ingress, 1 egress"));
    assert!(
        item.detail.contains("declared L7 HTTP"),
        "L7 is labelled declared: {}",
        item.detail
    );
    assert!(item.detail.contains("app=api"), "{}", item.detail);
    let policies = parse_policy_document(&cnp_json());
    assert_eq!(policies.len(), 1);
    assert_eq!(
        policies[0].ingress.as_ref().unwrap()[0].http[0].path,
        "/public"
    );
}

#[test]
fn a_specs_only_cnp_counts_its_rules_instead_of_claiming_star() {
    let value = json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumNetworkPolicy",
        "metadata": { "name": "multi-rule", "namespace": "prod", "uid": "uid-specs" },
        "specs": [
            {
                "endpointSelector": { "matchLabels": { "app": "api" } },
                "ingress": [{
                    "fromEndpoints": [{ "matchLabels": { "app": "web" } }],
                    "toPorts": [{
                        "ports": [{ "port": "80", "protocol": "TCP" }],
                        "rules": { "http": [{ "method": "GET", "path": "/public" }] }
                    }]
                }]
            },
            {
                "endpointSelector": { "matchLabels": { "app": "worker" } },
                "egress": [{ "toEntities": ["world"] }]
            }
        ]
    });
    let item = resource(Kind::CiliumNetworkPolicy, &value);
    assert!(
        item.detail.contains("1 ingress, 1 egress"),
        "{}",
        item.detail
    );
    assert!(item.detail.contains("declared L7 HTTP"), "{}", item.detail);
    assert!(
        item.detail.contains("app=api"),
        "the selector comes from the rules, not a claimed *: {}",
        item.detail
    );
    assert!(!item.detail.contains('*'), "{}", item.detail);
    assert_eq!(
        parse_policy_document(&value).len(),
        2,
        "the row and the compiled set describe the same object"
    );
}

#[test]
fn a_cnp_with_spec_and_specs_sums_both() {
    let value = json!({
        "kind": "CiliumNetworkPolicy",
        "metadata": { "name": "both", "namespace": "prod", "uid": "uid-both" },
        "spec": {
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "ingress": [{ "fromEndpoints": [{ "matchLabels": { "app": "web" } }] }]
        },
        "specs": [{
            "endpointSelector": { "matchLabels": { "app": "api" } },
            "egress": [{ "toEntities": ["world"] }, { "toEntities": ["host"] }]
        }]
    });
    let item = resource(Kind::CiliumNetworkPolicy, &value);
    assert!(
        item.detail.contains("1 ingress, 2 egress"),
        "{}",
        item.detail
    );
    assert_eq!(parse_policy_document(&value).len(), 2);
}

#[test]
fn identity_endpoint_and_node_keep_id_and_address() {
    let identity = resource(Kind::CiliumIdentity, &identity_json());
    assert_eq!(identity.identity_id, Some(12345));
    assert_eq!(identity.namespace, "prod");
    assert_eq!(
        identity
            .labels
            .get("k8s:io.kubernetes.pod.namespace")
            .map(String::as_str),
        Some("prod")
    );
    assert_eq!(
        identity.labels.get("k8s:k8s-app").map(String::as_str),
        Some("api")
    );

    let endpoint = resource(Kind::CiliumEndpoint, &endpoint_json());
    assert_eq!(endpoint.identity_id, Some(12345));
    assert_eq!(endpoint.address, "10.0.0.20,2001:db8::20");
    assert_eq!(endpoint.detail, "ready");
    assert_eq!(endpoint.uid, "uid-cep");

    let node = resource(Kind::CiliumNode, &node_json());
    assert_eq!(node.identity_id, Some(6));
    assert_eq!(node.address, "192.168.1.10,10.0.0.1");
    assert_eq!(node.namespace, "");
    assert!(node.detail.contains("node identity 6"));
}

#[test]
fn a_nameless_object_is_unreadable() {
    assert!(parse_item(Kind::CiliumNetworkPolicy, "v2", &json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = json!({
        "metadata": { "name": huge, "namespace": huge, "uid": huge },
        "spec": { "endpointSelector": { "matchLabels": { "app": huge } } }
    });
    let item = resource(Kind::CiliumNetworkPolicy, &value);
    for field in [&item.name, &item.namespace, &item.uid, &item.detail] {
        assert!(
            field.chars().count() <= MAX_FIELD_CHARS + 1,
            "every field is clipped where it is carried: {} chars",
            field.chars().count()
        );
        assert!(field.ends_with('\u{2026}'), "and looks clipped");
    }
}

#[test]
fn identity_label_cap_is_stated_as_truncated() {
    let mut labels = serde_json::Map::new();
    for index in 0..(MAX_LABELS + 4) {
        labels.insert(format!("k8s:label-{index}"), json!(format!("v{index}")));
    }
    let value = json!({
        "metadata": { "name": "99", "uid": "uid", "labels": labels }
    });
    let (item, truncated) = parse_item(Kind::CiliumIdentity, "v2", &value).expect("named");
    assert_eq!(item.labels.len(), MAX_LABELS);
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
    assert!(matches!(after_list(&api_error(404)), ListErr::NotFound));
    assert!(matches!(after_list(&api_error(403)), ListErr::Denied));
}

#[test]
fn an_unserved_inventory_has_no_table_a_served_empty_one_does() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "every kind 404 is absence, not an empty list"
    );
    let page = table_page(&Inventory {
        network_policies: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            labels_clipped: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("served and empty is still a table");
    assert!(page.rows.is_empty());
}

#[test]
fn a_denied_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        network_policies: KindSet::Denied,
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
    assert!(text.contains("CiliumNetworkPolicy"), "{text}");
}

#[test]
fn a_served_fixture_is_one_row_per_object() {
    let policy = resource(Kind::CiliumNetworkPolicy, &cnp_json());
    let page = table_page(&Inventory {
        network_policies: KindSet::Served {
            items: vec![policy],
            truncated: false,
            labels_clipped: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "allow-web");
    assert_eq!(page.rows[0].cells[0], "CiliumNetworkPolicy");
    assert!(page.rows[0].cells[5].contains("declared L7 HTTP"));
}

#[test]
fn identity_join_stamps_by_uid_and_refuses_an_empty_uid() {
    let endpoint = resource(Kind::CiliumEndpoint, &endpoint_json());
    let identity = resource(Kind::CiliumIdentity, &identity_json());
    let joins = identity_joins(
        std::slice::from_ref(&endpoint),
        std::slice::from_ref(&identity),
    );
    assert_eq!(joins.len(), 1);
    assert_eq!(joins[0].uid, "uid-cep");
    assert_eq!(joins[0].namespace, "prod");
    assert_eq!(joins[0].name, "api-0");
    assert_eq!(joins[0].identity_id, 12345);
    assert_eq!(
        joins[0].labels.get("k8s:k8s-app").map(String::as_str),
        Some("api")
    );

    let mut nameless = endpoint.clone();
    nameless.uid.clear();
    assert!(
        identity_joins(
            std::slice::from_ref(&nameless),
            std::slice::from_ref(&identity)
        )
        .is_empty(),
        "empty uid cannot join"
    );
    assert!(
        identity_joins(std::slice::from_ref(&endpoint), &[]).is_empty(),
        "a CiliumEndpoint without a matching CiliumIdentity cannot stamp"
    );
}

#[test]
fn observed_edges_come_from_hubble_series_not_from_policy() {
    let result = QueryResult {
        result_type: ResultType::Vector,
        series: vec![
            Series {
                labels: vec![
                    ("__name__".into(), "hubble_flows_processed_total".into()),
                    ("source".into(), "prod/web".into()),
                    ("destination".into(), "prod/api".into()),
                ],
                points: vec![(1, 1.0)],
            },
            Series {
                labels: vec![
                    (
                        "__name__".into(),
                        "container_cpu_usage_seconds_total".into(),
                    ),
                    ("source".into(), "prod/web".into()),
                    ("destination".into(), "prod/api".into()),
                ],
                points: vec![(1, 1.0)],
            },
            Series {
                labels: vec![
                    ("source_namespace".into(), "prod".into()),
                    ("source_pod".into(), "web".into()),
                    ("destination_identity".into(), "12345".into()),
                ],
                points: vec![(1, 1.0)],
            },
        ],
        truncated: false,
        dropped_series: 0,
    };
    let observed = observed_from_query(&result);
    assert_eq!(observed.edges.len(), 2);
    assert_eq!(observed.edges[0].from, "prod/web");
    assert_eq!(observed.edges[0].to, "prod/api");
    assert_eq!(
        observed.edges[0].because.exporter,
        TelemetryExporter::Hubble
    );
    assert_eq!(observed.edges[1].from, "prod/web");
    assert_eq!(observed.edges[1].to, "identity:12345");

    let istio = observed_from_reach(&[ObservedReach {
        from: "web".into(),
        to: "api".into(),
        because: TelemetryReason {
            metric: "istio_requests_total".into(),
            exporter: TelemetryExporter::Istio,
        },
    }]);
    assert!(
        istio.edges.is_empty(),
        "Istio observed reach stays on mesh.rs"
    );
}

#[test]
fn observed_cap_is_stated() {
    let series = (0..MAX_OBSERVED_EDGES + 3)
        .map(|index| Series {
            labels: vec![
                ("__name__".into(), "hubble_flows_processed_total".into()),
                ("source".into(), format!("src-{index}")),
                ("destination".into(), format!("dst-{index}")),
            ],
            points: vec![(1, 1.0)],
        })
        .collect();
    let observed = observed_from_query(&QueryResult {
        result_type: ResultType::Vector,
        series,
        truncated: false,
        dropped_series: 0,
    });
    assert_eq!(observed.edges.len(), MAX_OBSERVED_EDGES);
    assert!(observed.truncated);
}

#[test]
fn debug_of_inventory_has_nowhere_to_put_a_secret() {
    let inventory = Inventory {
        network_policies: KindSet::Served {
            items: vec![resource(Kind::CiliumNetworkPolicy, &cnp_json())],
            truncated: false,
            labels_clipped: false,
            unreadable: 0,
        },
        identities: KindSet::Served {
            items: vec![resource(Kind::CiliumIdentity, &identity_json())],
            truncated: false,
            labels_clipped: false,
            unreadable: 0,
        },
        endpoints: KindSet::Served {
            items: vec![resource(Kind::CiliumEndpoint, &endpoint_json())],
            truncated: false,
            labels_clipped: false,
            unreadable: 0,
        },
        nodes: KindSet::Served {
            items: vec![resource(Kind::CiliumNode, &node_json())],
            truncated: false,
            labels_clipped: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    let debug = format!("{inventory:?}").to_ascii_lowercase();
    for needle in [
        "password",
        "token",
        "secret",
        "stringdata",
        "bearer",
        "authorization",
    ] {
        assert!(
            !debug.contains(needle),
            "inventory Debug must not grow a secret-looking field: {needle}"
        );
    }
}

#[test]
fn collection_paths_use_the_discovered_group_version_and_plural() {
    assert_eq!(
        collection_url(Kind::CiliumNetworkPolicy, "v2", None),
        "/apis/cilium.io/v2/ciliumnetworkpolicies"
    );
    assert_eq!(
        collection_url(Kind::CiliumNetworkPolicy, "v2", Some("prod")),
        "/apis/cilium.io/v2/namespaces/prod/ciliumnetworkpolicies"
    );
    assert_eq!(
        collection_url(Kind::CiliumClusterwideNetworkPolicy, "v2", Some("prod")),
        "/apis/cilium.io/v2/ciliumclusterwidenetworkpolicies"
    );
    assert_eq!(
        collection_url(Kind::CiliumIdentity, "v2", None),
        "/apis/cilium.io/v2/ciliumidentities"
    );
    assert_eq!(
        collection_url(Kind::CiliumEndpoint, "v2", None),
        "/apis/cilium.io/v2/ciliumendpoints"
    );
    assert_eq!(
        collection_url(Kind::CiliumNode, "v2", None),
        "/apis/cilium.io/v2/ciliumnodes"
    );
}

#[test]
fn render_names_absence_and_does_not_claim_observed_traffic() {
    let lines = render(&Inventory::default());
    assert_eq!(lines[0], "Cilium is not served by this cluster");
    let text = lines.join("\n");
    assert!(text.contains("nothing is installed to find them"), "{text}");

    let served = render(&Inventory {
        network_policies: KindSet::Served {
            items: vec![resource(Kind::CiliumNetworkPolicy, &cnp_json())],
            truncated: true,
            labels_clipped: false,
            unreadable: 1,
        },
        identities: KindSet::Denied,
        ..Inventory::default()
    });
    let text = served.join("\n");
    assert!(text.contains("1 Cilium object"), "{text}");
    assert!(text.contains("stopped at"), "{text}");
    assert!(text.contains("could not be decoded"), "{text}");
    assert!(text.contains("cilium identities: access denied"), "{text}");
    assert!(text.contains("declared Cilium policy"), "{text}");
    assert!(text.contains("not mixed"), "{text}");
}

#[test]
fn an_oversize_page_is_refused() {
    let huge = "x".repeat(MAX_PAGE_BYTES + 1);
    assert!(matches!(parse_list(&huge), Err(PageError::TooLarge)));
}
