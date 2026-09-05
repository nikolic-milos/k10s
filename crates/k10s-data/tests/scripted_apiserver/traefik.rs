//! Traefik CRs listed through kube Request: group probe, empty served table,
//! per-kind 404 and 403 with the group served, the live IngressRoute shape,
//! and a planted middleware secret that must not leak.

use crate::*;
use k10s_data::read::Fetched;
use k10s_data::traefik::{self, GROUP, INGRESS_CONTROLLER, Kind, KindSet, parse_item, table_page};

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn traefik_group() -> String {
    r#"{"kind":"APIGroup","name":"traefik.io",
        "versions":[{"groupVersion":"traefik.io/v1alpha1","version":"v1alpha1"}],
        "preferredVersion":{"groupVersion":"traefik.io/v1alpha1","version":"v1alpha1"}}"#
        .to_string()
}

fn empty_list(kind: &str) -> String {
    format!(r#"{{"kind":"{kind}List","apiVersion":"traefik.io/v1alpha1","items":[]}}"#)
}

fn ingressroute_item() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "traefik.io/v1alpha1",
        "kind": "IngressRoute",
        "metadata": {
            "name": "hplane-dns",
            "namespace": "k10s-hplane",
            "uid": "42e2e2bf-23a4-4a43-88d2-adfc1beb0f0d"
        },
        "spec": {
            "entryPoints": ["web"],
            "routes": [{
                "kind": "Rule",
                "match": "Host(`hplane.k10s.lab`) && PathPrefix(`/dns`)",
                "middlewares": [{ "name": "strip" }],
                "services": [{
                    "name": "kube-dns",
                    "namespace": "kube-system",
                    "port": 9153
                }]
            }],
            "tls": { "secretName": "hplane-tls" }
        }
    })
}

fn middleware_with_planted_password() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "auth", "namespace": "prod", "uid": "uid-mw" },
        "spec": {
            "basicAuth": {
                "users": ["admin:$apr1$SUPERSECRET-PLANTED-htpasswd"],
                "secret": "htpasswd-secret"
            },
            "headers": {
                "customRequestHeaders": {
                    "Authorization": "Bearer SUPERSECRET-PLANTED-htpasswd"
                }
            }
        }
    })
}

fn script_empty_kinds(script: &Script) {
    for kind in Kind::ALL {
        script.route(
            "GET",
            &format!("/apis/traefik.io/v1alpha1/{}?", kind.plural()),
            200,
            empty_list(kind.as_str()),
        );
    }
}

fn script_kinds_with(script: &Script, filled: Kind, items: &[serde_json::Value]) {
    for kind in Kind::ALL {
        let body = if *kind == filled {
            serde_json::json!({
                "kind": format!("{}List", kind.as_str()),
                "apiVersion": "traefik.io/v1alpha1",
                "items": items
            })
            .to_string()
        } else {
            empty_list(kind.as_str())
        };
        script.route(
            "GET",
            &format!("/apis/traefik.io/v1alpha1/{}?", kind.plural()),
            200,
            body,
        );
    }
}

#[test]
fn a_404_traefik_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { traefik::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.ingress_routes, KindSet::NotServed));
    assert!(table_page(&inventory).is_none(), "a 404 group has no table");
    assert!(
        script.requests_for("ingressroutes").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_traefik_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 403, status(403, "Forbidden"));
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        200,
        serde_json::json!({
            "kind": "IngressClassList",
            "items": [{
                "metadata": {
                    "name": "traefik",
                    "annotations": { "ingressclass.kubernetes.io/is-default-class": "true" }
                },
                "spec": { "controller": "traefik.io/ingress-controller" }
            }]
        })
        .to_string(),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { traefik::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied: {fetched:?}");
    };
    assert!(matches!(inventory.group, traefik::GroupState::Denied));
    assert!(inventory.served(), "403 is Denied, not served: false");
    let page = table_page(&inventory).expect("Denied is a labelled table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("access denied for this account"), "{text}");
    let words = traefik::render(&inventory).join("\n");
    assert!(
        words.contains("traefik.io: access denied for this account"),
        "{words}"
    );
    assert!(
        words.contains("default IngressClass is traefik"),
        "the IngressClass fact a denied account still learned is said: {words}"
    );
    assert!(
        script.requests_for("ingressroutes").is_empty(),
        "a 403 group must not list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_served_group_with_zero_objects_is_an_empty_table() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    script_empty_kinds(&script);
    let runtime = runtime();
    let fetched = runtime.block_on(async { traefik::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("empty served lists must resolve: {fetched:?}");
    };
    assert!(inventory.group.is_served());
    assert!(matches!(
        inventory.ingress_routes,
        KindSet::Served { ref items, .. } if items.is_empty()
    ));
    let page = table_page(&inventory).expect("served plus zero rows is a table");
    assert!(
        page.rows.is_empty(),
        "empty served tables are not Absent: {page:?}"
    );
    let seen = script.seen();
    assert!(
        seen.iter()
            .any(|seen| seen.path == "/apis/traefik.io"
                || seen.path.starts_with("/apis/traefik.io?")),
        "the group document is probed: {seen:?}"
    );
    assert!(
        seen.iter().any(|seen| seen
            .path
            .starts_with("/apis/traefik.io/v1alpha1/ingressroutes?")),
        "lists use traefik.io/v1alpha1: {seen:?}"
    );
    drop(runtime);
}

// A partial install: the Helm chart lets operators skip CRDs, and older
// installs predate IngressRouteUDP.
#[test]
fn a_404_on_one_kind_leaves_the_other_nine_listed() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    for kind in Kind::ALL
        .iter()
        .filter(|kind| **kind != Kind::IngressRouteUDP)
    {
        script.route(
            "GET",
            &format!("/apis/traefik.io/v1alpha1/{}?", kind.plural()),
            200,
            empty_list(kind.as_str()),
        );
    }
    let runtime = runtime();
    let fetched = runtime.block_on(async { traefik::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a partial install must resolve: {fetched:?}");
    };
    assert!(inventory.group.is_served());
    assert!(inventory.served());
    assert!(
        matches!(inventory.ingress_routes_udp, KindSet::NotServed),
        "the one absent CRD is invisible, not broken: {:?}",
        inventory.ingress_routes_udp
    );
    assert!(inventory.ingress_routes.served());
    let page = table_page(&inventory).expect("the group is served, so the table stays");
    assert!(
        page.rows
            .iter()
            .all(|row| !row.cells.concat().contains("IngressRouteUDP")),
        "no phantom or denial row for the missing kind: {:?}",
        page.rows
    );
    assert_eq!(
        script.requests_for("ingressrouteudps").len(),
        1,
        "the 404 came from the list, asked once: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_on_one_kind_is_a_denial_row_that_does_not_hide_the_others() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    for kind in Kind::ALL {
        if *kind == Kind::IngressRouteUDP {
            script.route(
                "GET",
                "/apis/traefik.io/v1alpha1/ingressrouteudps?",
                403,
                status(403, "Forbidden"),
            );
        } else {
            script.route(
                "GET",
                &format!("/apis/traefik.io/v1alpha1/{}?", kind.plural()),
                200,
                empty_list(kind.as_str()),
            );
        }
    }
    let runtime = runtime();
    let fetched = runtime.block_on(async { traefik::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("one forbidden kind must not fail the fetch: {fetched:?}");
    };
    assert!(inventory.group.is_served());
    assert!(
        matches!(inventory.ingress_routes_udp, KindSet::Denied),
        "a 403 on one list is Denied for that kind alone: {:?}",
        inventory.ingress_routes_udp
    );
    assert!(inventory.ingress_routes.served());
    let page = table_page(&inventory).expect("the group is served, so the table stays");
    assert_eq!(page.rows.len(), 1, "{:?}", page.rows);
    assert_eq!(page.rows[0].uid, "denied:IngressRouteUDP");
    assert!(
        page.rows[0]
            .cells
            .concat()
            .contains("access denied for this account"),
        "{:?}",
        page.rows[0]
    );
    let words = traefik::render(&inventory).join("\n");
    assert!(
        words.contains("traefik ingressrouteudps: access denied for this account"),
        "{words}"
    );
    drop(runtime);
}

#[test]
fn an_ingressroute_list_keeps_host_pathprefix_and_service_port() {
    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    script_kinds_with(&script, Kind::IngressRoute, &[ingressroute_item()]);
    script.route(
        "GET",
        "/apis/networking.k8s.io/v1/ingressclasses?",
        200,
        serde_json::json!({
            "kind": "IngressClassList",
            "items": [{
                "metadata": {
                    "name": "traefik",
                    "annotations": { "ingressclass.kubernetes.io/is-default-class": "true" }
                },
                "spec": { "controller": "traefik.io/ingress-controller" }
            }]
        })
        .to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { traefik::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    let item = &inventory.ingress_routes.items()[0];
    assert_eq!(item.name, "hplane-dns");
    assert_eq!(item.namespace, "k10s-hplane");
    assert_eq!(item.entrypoints, vec!["web"]);
    assert_eq!(
        item.routes,
        vec!["Host(`hplane.k10s.lab`) && PathPrefix(`/dns`)"]
    );
    assert_eq!(item.services[0].name, "kube-dns");
    assert_eq!(item.services[0].namespace, "kube-system");
    assert_eq!(item.services[0].port, "9153");
    assert_eq!(item.middlewares, vec!["strip"]);
    assert_eq!(item.tls_secret, "hplane-tls");
    let class = inventory
        .default_ingress_class
        .as_ref()
        .expect("the default IngressClass is Traefik; no Ingress is required");
    assert_eq!(class.name, "traefik");
    assert_eq!(class.controller, INGRESS_CONTROLLER);

    let page = table_page(&inventory).expect("a served inventory is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].cells[5], "kube-system/kube-dns:9153");
    assert!(
        script
            .requests_for("/apis/traefik.io/v1alpha1/ingressroutes")
            .len()
            == 1,
        "one list on the live path: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_planted_middleware_password_does_not_leak_from_the_list() {
    let fixture = middleware_with_planted_password();
    assert!(
        fixture.to_string().contains("SUPERSECRET-PLANTED-htpasswd"),
        "the fixture has to contain what must not come out"
    );
    let parsed = parse_item(Kind::Middleware, fixture.clone()).expect("middleware parses");
    assert!(
        !format!("{parsed:?}").contains("SUPERSECRET-PLANTED-htpasswd"),
        "parse drops the users before Resource exists"
    );

    let script = Script::default();
    script.route("GET", "/apis/traefik.io", 200, traefik_group());
    script_kinds_with(&script, Kind::Middleware, &[fixture]);

    let runtime = runtime();
    let fetched = runtime.block_on(async { traefik::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served middleware list must resolve: {fetched:?}");
    };
    let debug = format!("{inventory:?}");
    assert!(
        !debug.contains("SUPERSECRET-PLANTED-htpasswd"),
        "Debug of the inventory must not carry the planted password: {debug}"
    );
    assert!(!debug.contains("admin:$apr1"), "{debug}");
    let page = table_page(&inventory).expect("served middleware is a table");
    let cells = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !cells.contains("SUPERSECRET-PLANTED-htpasswd"),
        "table cells must not carry the planted password: {cells}"
    );
    assert!(cells.contains("basicAuth"), "{cells}");
    assert_eq!(inventory.middlewares.items()[0].tls_secret, "");
    drop(runtime);
}

#[test]
fn the_group_constant_is_the_live_api_group() {
    assert_eq!(GROUP, "traefik.io");
    assert_eq!(traefik::VERSION, "v1alpha1");
}
