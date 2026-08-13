//! The on-demand NetworkPolicy inventory keeps typed policy and pod fields
//! without widening the always-on watch projection.

use crate::*;
use k10s_data::{
    netpol::{
        Completeness, Decision, MAX_NAMESPACES, MAX_PODS, MAX_POLICIES, Protocol, Traffic,
        VerdictReason, fetch,
    },
    read::Fetched,
};

const POLICY_LIST: &str = r#"{
  "kind":"NetworkPolicyList","apiVersion":"networking.k8s.io/v1","metadata":{},"items":[
    {"metadata":{"name":"allow-web","namespace":"prod"},"spec":{
      "podSelector":{"matchLabels":{"app":"api"}},"policyTypes":["Ingress"],
      "ingress":[{"from":[{
        "namespaceSelector":{"matchLabels":{"access":"clients"}},
        "podSelector":{"matchLabels":{"app":"web"}}
      }],"ports":[{"port":"https"}]}]
    }},
    {"metadata":{"name":"allow-api","namespace":"clients"},"spec":{
      "podSelector":{"matchLabels":{"app":"web"}},"policyTypes":["Egress"],
      "egress":[{"to":[{
        "namespaceSelector":{"matchLabels":{"access":"backend"}},
        "podSelector":{"matchLabels":{"app":"api"}}
      }],"ports":[{"port":"https"}]}]
    }}
  ]
}"#;

const INVENTORY_PODS: &str = r#"{
  "kind":"PodList","apiVersion":"v1","metadata":{},"items":[
    {"metadata":{"name":"web","namespace":"clients","labels":{"app":"web"}},
     "spec":{"containers":[{"name":"web"}]},
     "status":{"podIP":"10.0.0.10","podIPs":[{"ip":"10.0.0.10"}] }},
    {"metadata":{"name":"api","namespace":"prod","labels":{"app":"api"}},
     "spec":{"containers":[{"name":"api","ports":[
       {"name":"https","containerPort":8443},
       {"name":"dns","containerPort":5353,"protocol":"UDP"}
     ]}]},
     "status":{"podIP":"10.0.0.20","podIPs":[
       {"ip":"10.0.0.20"},{"ip":"2001:db8::20"}
     ]}}
  ]
}"#;

const INVENTORY_NAMESPACES: &str = r#"{
  "kind":"NamespaceList","apiVersion":"v1","metadata":{},"items":[
    {"metadata":{"name":"clients","labels":{"access":"clients"}}},
    {"metadata":{"name":"prod","labels":{"access":"backend"}}}
  ]
}"#;

fn inventory_routes(script: &Script, policies: impl Into<String>) {
    script.route(
        "GET",
        &format!("/apis/networking.k8s.io/v1/networkpolicies?limit={MAX_POLICIES}"),
        200,
        policies,
    );
    script.route(
        "GET",
        &format!("/api/v1/pods?limit={MAX_PODS}"),
        200,
        INVENTORY_PODS,
    );
    script.route(
        "GET",
        &format!("/api/v1/namespaces?limit={MAX_NAMESPACES}"),
        200,
        INVENTORY_NAMESPACES,
    );
}

#[test]
fn reader_fetches_one_typed_cache_input_and_retains_no_secret_objects() {
    let script = Script::default();
    script_discovery(&script);
    script_rules_review(&script);
    script_access_reviews(&script, true, 32);
    script_lists(&script);
    let runtime = runtime();
    let (sync, _events) = sync_on(&runtime, &script);
    let secret_requests_before = script.requests_for("/secrets").len();
    let requests_before = script.seen().len();
    inventory_routes(&script, POLICY_LIST);

    let (tx, rx) = std::sync::mpsc::channel();
    sync.reader.fetch_network_policy_inventory(move |outcome| {
        let _ = tx.send(outcome);
    });
    let Fetched::Ok(inventory) = wait(&rx) else {
        panic!("the typed inventory must resolve");
    };

    assert!(inventory.status.complete());
    assert_eq!(inventory.status.policies.kept, 2);
    assert_eq!(inventory.status.pods.kept, 2);
    assert_eq!(inventory.status.namespaces.kept, 2);
    assert_eq!(inventory.declared.completeness(), Completeness::Complete);

    let source = inventory.pod("clients", "web").expect("source pod");
    let destination = inventory.pod("prod", "api").expect("destination pod");
    assert_eq!(
        destination
            .ips
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["10.0.0.20", "2001:db8::20"]
    );
    assert_eq!(destination.ports.len(), 2);
    assert_eq!(
        inventory
            .namespace_labels("clients")
            .and_then(|labels| labels.get("access"))
            .map(String::as_str),
        Some("clients")
    );
    assert_eq!(
        inventory
            .declared
            .verdict(
                source,
                destination,
                Traffic {
                    protocol: Protocol::Tcp,
                    port: 8443,
                },
            )
            .decision,
        Decision::Allow
    );
    assert_eq!(
        inventory
            .declared
            .verdict(
                source,
                destination,
                Traffic {
                    protocol: Protocol::Tcp,
                    port: 443,
                },
            )
            .decision,
        Decision::Deny
    );

    assert_eq!(
        script.requests_for("/secrets").len(),
        secret_requests_before,
        "the inventory reads only policies, pods, and namespaces"
    );
    assert_eq!(
        script.seen().len() - requests_before,
        3,
        "one bounded request per kind"
    );
    drop(runtime);
}

#[test]
fn a_continue_token_marks_the_compiled_answer_incomplete_not_denied() {
    let script = Script::default();
    let policies = r#"{
      "kind":"NetworkPolicyList","apiVersion":"networking.k8s.io/v1",
      "metadata":{"continue":"opaque-token","remainingItemCount":1},"items":[
        {"metadata":{"name":"deny-prod","namespace":"prod"},
         "spec":{"podSelector":{},"policyTypes":["Ingress"]}}
      ]
    }"#;
    inventory_routes(&script, policies);
    let runtime = runtime();
    let fetched = runtime.block_on(async { fetch(&script.client()).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("the partial inventory is still a labelled successful fetch");
    };
    assert_eq!(inventory.status.policies.kept, 1);
    assert_eq!(inventory.status.policies.remaining, Some(1));
    assert!(inventory.status.policies.incomplete);
    assert_eq!(
        inventory.declared.completeness(),
        Completeness::IncompleteInventory {
            policies: true,
            pods: false,
            namespaces: false,
        }
    );

    let source = inventory.pod("clients", "web").expect("source pod");
    let destination = inventory.pod("prod", "api").expect("destination pod");
    let verdict = inventory.declared.verdict(
        source,
        destination,
        Traffic {
            protocol: Protocol::Tcp,
            port: 8443,
        },
    );
    assert_eq!(verdict.decision, Decision::Indeterminate);
    assert_eq!(verdict.allowed(), None);
    assert!(
        verdict
            .reasons
            .contains(&VerdictReason::InventoryIncomplete {
                policies: true,
                pods: false,
                namespaces: false,
            })
    );
    assert!(script.requests_for("/secrets").is_empty());
}

#[test]
fn inventory_preserves_denial_and_failure_as_fetched_states() {
    let runtime = runtime();
    let denied = Script::default();
    denied.route(
        "GET",
        &format!(
            "/apis/networking.k8s.io/v1/networkpolicies?limit={MAX_POLICIES}"
        ),
        403,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"no"}"#,
    );
    denied.route(
        "GET",
        &format!("/api/v1/pods?limit={MAX_PODS}"),
        200,
        INVENTORY_PODS,
    );
    denied.route(
        "GET",
        &format!("/api/v1/namespaces?limit={MAX_NAMESPACES}"),
        200,
        INVENTORY_NAMESPACES,
    );
    assert!(matches!(
        runtime.block_on(async { fetch(&denied.client()).await }),
        Fetched::Denied {
            what: "network policy inventory"
        }
    ));

    let failed = Script::default();
    failed.route(
        "GET",
        &format!("/apis/networking.k8s.io/v1/networkpolicies?limit={MAX_POLICIES}"),
        200,
        POLICY_LIST,
    );
    failed.route(
        "GET",
        &format!("/api/v1/pods?limit={MAX_PODS}"),
        500,
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":500,"reason":"InternalError","message":"etcd unavailable"}"#,
    );
    failed.route(
        "GET",
        &format!("/api/v1/namespaces?limit={MAX_NAMESPACES}"),
        200,
        INVENTORY_NAMESPACES,
    );
    let Fetched::Failed { what, why } = runtime.block_on(async { fetch(&failed.client()).await })
    else {
        panic!("a transport failure must remain a failed fetch");
    };
    assert_eq!(what, "network policy inventory");
    assert!(why.contains("etcd unavailable"), "{why}");
    assert!(failed.requests_for("/secrets").is_empty());
}
