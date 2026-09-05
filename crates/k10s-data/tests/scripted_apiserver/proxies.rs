//! Proxy controller CRs listed through kube Request. A 404 group is
//! NotServed, not Failed.

use crate::*;
use k10s_data::proxies::{self, CONTOUR_GROUP, HAPROXY_LEGACY_GROUP, KindSet};
use k10s_data::read::Fetched;

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn group_doc(name: &str, version: &str) -> String {
    format!(
        r#"{{"kind":"APIGroup","name":"{name}","versions":[{{"groupVersion":"{name}/{version}","version":"{version}"}}],"preferredVersion":{{"groupVersion":"{name}/{version}","version":"{version}"}}}}"#
    )
}

fn httpproxy_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "www", "namespace": "prod" },
        "spec": {
            "virtualhost": {
                "fqdn": "example.com",
                "tls": {
                    "secretName": "edge-tls",
                    "data": { "tls.crt": "PLANTED_CERT", "tls.key": "PLANTED_KEY" }
                }
            },
            "routes": [{ "services": [{ "name": "web", "port": 80 }] }]
        }
    })
}

#[test]
fn a_404_proxy_group_is_not_served_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { proxies::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.contour, KindSet::NotServed));
    assert!(matches!(inventory.envoy_gateway, KindSet::NotServed));
    assert!(matches!(inventory.haproxy, KindSet::NotServed));
    assert!(matches!(inventory.kong, KindSet::NotServed));
    assert!(matches!(inventory.nginx, KindSet::NotServed));
    assert!(matches!(inventory.ambassador, KindSet::NotServed));
    assert!(proxies::table_page(&inventory).is_none());
    assert!(
        script.requests_for("httpproxies").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script.requests_for(HAPROXY_LEGACY_GROUP).len() == 1,
        "core.haproxy.org is probed and then dropped: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_served_nginx_group_lists_virtualservers_from_k8s_nginx_org() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/k8s.nginx.org",
        200,
        group_doc("k8s.nginx.org", "v1"),
    );
    script.route(
        "GET",
        "/apis/k8s.nginx.org/v1/virtualservers?",
        200,
        serde_json::json!({
            "kind": "VirtualServerList",
            "items": [{
                "metadata": { "name": "shop", "namespace": "prod" },
                "spec": {
                    "host": "shop.example.com",
                    "upstreams": [{ "name": "shop", "service": "shop-svc", "port": 80 }]
                }
            }]
        })
        .to_string(),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { proxies::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served NGINX group must resolve: {fetched:?}");
    };
    assert_eq!(
        script.requests_for("k8s.nginx.org/v1/virtualservers").len(),
        1,
        "the group string must be proven over the wire, not as a parse-only constant: {:?}",
        script.seen()
    );
    let shop = &inventory.nginx.items()[0];
    assert_eq!(shop.name, "shop");
    assert_eq!(shop.hosts, ["shop.example.com"]);
    assert_eq!(shop.backends, ["shop-svc:80"]);
    drop(runtime);
}

#[test]
fn kong_lists_cluster_plugins_unscoped_and_never_a_bare_ingresses() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/configuration.konghq.com",
        200,
        group_doc("configuration.konghq.com", "v1"),
    );
    script.route(
        "GET",
        "/apis/configuration.konghq.com/v1/namespaces/prod/kongplugins?",
        200,
        serde_json::json!({
            "kind": "KongPluginList",
            "items": [{ "metadata": { "name": "rate", "namespace": "prod" }, "plugin": "rate-limiting" }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/configuration.konghq.com/v1/kongclusterplugins?",
        200,
        serde_json::json!({
            "kind": "KongClusterPluginList",
            "items": [{ "metadata": { "name": "cors-everywhere" }, "plugin": "cors" }]
        })
        .to_string(),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { proxies::fetch(&script.client(), Some("prod")).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served Kong group must resolve: {fetched:?}");
    };
    assert!(
        script
            .requests_for("kongclusterplugins")
            .iter()
            .all(|seen| !seen.path.contains("/namespaces/")),
        "a cluster-scoped kind must never get /namespaces/{{ns}} in its URL: {:?}",
        script.seen()
    );
    assert_eq!(
        script.requests_for("/namespaces/prod/kongplugins").len(),
        1,
        "namespaced kinds keep the namespace scope: {:?}",
        script.seen()
    );
    assert!(
        !script.requests_for("udpingresses").is_empty(),
        "UDPIngress is part of the Kong inventory: {:?}",
        script.seen()
    );
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("/ingresses")),
        "configuration.konghq.com has no kind named Ingress; core Ingress belongs to another module: {:?}",
        script.seen()
    );
    let names: Vec<&str> = inventory
        .kong
        .items()
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert!(names.contains(&"rate"), "{names:?}");
    assert!(names.contains(&"cors-everywhere"), "{names:?}");
    drop(runtime);
}

#[test]
fn a_403_proxy_group_is_denied_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/projectcontour.io",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { proxies::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that controller: {fetched:?}");
    };
    assert!(matches!(inventory.contour, KindSet::Denied));
    assert!(
        inventory.contour.served(),
        "403 is Denied, not served: false"
    );
    drop(runtime);
}

#[test]
fn planted_tls_bytes_stay_out_of_a_served_contour_inventory() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/projectcontour.io",
        200,
        group_doc(CONTOUR_GROUP, "v1"),
    );
    script.route(
        "GET",
        "/apis/projectcontour.io/v1/httpproxies?",
        200,
        serde_json::json!({
            "kind": "HTTPProxyList",
            "items": [httpproxy_item()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/projectcontour.io/v1/tlscertificatedelegations?",
        200,
        serde_json::json!({ "kind": "TLSCertificateDelegationList", "items": [] }).to_string(),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { proxies::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    let proxy = &inventory.contour.items()[0];
    assert_eq!(proxy.name, "www");
    assert_eq!(proxy.tls_secrets, ["edge-tls"]);
    assert_eq!(proxy.backends, ["web:80"]);
    let debug = format!("{inventory:?}");
    assert!(!debug.contains("PLANTED_CERT"), "{debug}");
    assert!(!debug.contains("PLANTED_KEY"), "{debug}");
    let page = proxies::table_page(&inventory).expect("a served controller is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("edge-tls"), "{text}");
    assert!(!text.contains("PLANTED"), "{text}");
    drop(runtime);
}

#[test]
fn one_broken_controller_never_hides_the_other_five() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/projectcontour.io",
        500,
        status(500, "InternalError"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { proxies::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!(
            "one 5xx controller is its own labelled row, not a whole-fetch failure: {fetched:?}"
        );
    };
    assert!(
        matches!(inventory.contour, KindSet::Failed { .. }),
        "the broken controller stays visibly broken: {:?}",
        inventory.contour
    );
    assert!(matches!(inventory.kong, KindSet::NotServed));
    assert!(matches!(inventory.nginx, KindSet::NotServed));
    let page = proxies::table_page(&inventory).expect("a failed controller is a labelled table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("could not be asked"),
        "the failure is a labelled row: {text}"
    );
    drop(runtime);
}

#[test]
fn an_undecodable_listed_object_is_counted_and_labelled_not_dropped() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/projectcontour.io",
        200,
        group_doc(CONTOUR_GROUP, "v1"),
    );
    script.route(
        "GET",
        "/apis/projectcontour.io/v1/httpproxies?",
        200,
        serde_json::json!({
            "kind": "HTTPProxyList",
            // The second item has no metadata.name, so it cannot become a
            // Resource — it must be counted, not silently skipped.
            "items": [httpproxy_item(), { "metadata": {}, "spec": {} }]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/projectcontour.io/v1/tlscertificatedelegations?",
        200,
        serde_json::json!({ "kind": "TLSCertificateDelegationList", "items": [] }).to_string(),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { proxies::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("one bad item never fails the whole listing: {fetched:?}");
    };
    let KindSet::Served {
        items, unreadable, ..
    } = &inventory.contour
    else {
        panic!("the readable item keeps its row: {:?}", inventory.contour);
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        *unreadable, 1,
        "the undecodable sibling item must be counted"
    );
    let page = proxies::table_page(&inventory).expect("a served controller is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("1 object could not be decoded and is not shown"),
        "the lost object is a labelled row, not silence: {text}"
    );
    assert!(
        text.contains("www"),
        "the readable item still shows: {text}"
    );
    drop(runtime);
}

#[test]
fn a_denied_kind_inside_a_served_group_stays_visible() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/projectcontour.io",
        200,
        group_doc(CONTOUR_GROUP, "v1"),
    );
    script.route(
        "GET",
        "/apis/projectcontour.io/v1/httpproxies?",
        200,
        serde_json::json!({
            "kind": "HTTPProxyList",
            "items": [httpproxy_item()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/projectcontour.io/v1/tlscertificatedelegations?",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { proxies::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    let KindSet::Served { items, denied, .. } = &inventory.contour else {
        panic!("the answered kind keeps its rows: {:?}", inventory.contour);
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        *denied, 1,
        "the denied sibling kind must not vanish behind the served one"
    );
    let page = proxies::table_page(&inventory).expect("a served controller is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("some kinds are denied for this account"),
        "the partial denial is a labelled row: {text}"
    );
    drop(runtime);
}
