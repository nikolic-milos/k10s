//! ESO CRs listed through kube Request: group probe, 404 vs 403, the wire
//! path, a planted token, table presence, and paging.

use crate::*;
use k10s_data::eso::{self, KindSet};
use k10s_data::read::Fetched;

const PLANTED: &str = "PLANTED-TOKEN-9f3a";

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn group_v1() -> String {
    r#"{"kind":"APIGroup","name":"external-secrets.io",
        "versions":[{"groupVersion":"external-secrets.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"external-secrets.io/v1","version":"v1"}}"#
        .to_string()
}

fn store_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "vault-backend", "namespace": "prod" },
        "spec": {
            "provider": {
                "vault": {
                    "server": "https://vault.example.com",
                    "auth": { "token": PLANTED }
                }
            }
        },
        "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
    })
}

fn external_secret_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "db", "namespace": "prod" },
        "spec": {
            "refreshInterval": "1h",
            "secretStoreRef": { "name": "vault-backend", "kind": "SecretStore" },
            "target": { "name": "db-creds" },
            "data": [
                { "secretKey": "username", "remoteRef": { "key": PLANTED } },
                { "secretKey": "password", "remoteRef": { "key": PLANTED } }
            ]
        },
        "status": {
            "conditions": [{ "type": "Ready", "status": "True" }],
            "binding": { "name": "db-creds" }
        }
    })
}

fn list(kind: &str, items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": format!("{kind}List"),
        "apiVersion": "external-secrets.io/v1",
        "metadata": {},
        "items": items
    })
    .to_string()
}

#[test]
fn a_404_eso_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { eso::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.external_secrets, KindSet::NotServed));
    assert!(eso::table_page(&inventory).is_none());
    assert!(
        script.requests_for("externalsecrets").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_eso_group_is_denied_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/external-secrets.io",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { eso::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.secret_stores, KindSet::Denied));
    assert!(inventory.secret_stores.served());
    assert!(eso::table_page(&inventory).is_some());
    assert!(
        script.requests_for("secretstores").is_empty(),
        "a 403 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn eso_objects_are_listed_from_the_crs_and_a_planted_token_does_not_leak() {
    let script = Script::default();
    script.route("GET", "/apis/external-secrets.io", 200, group_v1());
    script.route(
        "GET",
        "/apis/external-secrets.io/v1/secretstores?",
        200,
        list("SecretStore", &[store_item()]),
    );
    script.route(
        "GET",
        "/apis/external-secrets.io/v1/clustersecretstores?",
        200,
        list("ClusterSecretStore", &[]),
    );
    script.route(
        "GET",
        "/apis/external-secrets.io/v1/externalsecrets?",
        200,
        serde_json::json!({
            "kind": "ExternalSecretList",
            "metadata": { "continue": "page-2" },
            "items": [external_secret_item()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/external-secrets.io/v1/externalsecrets?",
        200,
        list(
            "ExternalSecret",
            &[serde_json::json!({
                "metadata": { "name": "cache", "namespace": "prod" },
                "spec": {
                    "secretStoreRef": { "kind": "SecretStore", "name": "vault-backend" },
                    "target": { "name": "cache-creds" }
                }
            })],
        ),
    );
    script.route(
        "GET",
        "/apis/external-secrets.io/v1/clusterexternalsecrets?",
        200,
        list("ClusterExternalSecret", &[]),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { eso::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    let store = &inventory.secret_stores.items()[0];
    assert_eq!(store.store_type, "vault");
    let secrets = inventory.external_secrets.items();
    assert_eq!(
        secrets
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["db", "cache"]
    );
    assert_eq!(secrets[0].target_secret, "db-creds");
    assert_eq!(secrets[0].key_names, vec!["username", "password"]);
    assert!(eso::table_page(&inventory).is_some());

    let lists = script.requests_for("/externalsecrets?");
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
    assert!(
        script
            .seen()
            .iter()
            .any(|request| request.path == "/apis/external-secrets.io"
                || request.path.starts_with("/apis/external-secrets.io?")),
        "the group document is probed: {:?}",
        script.seen()
    );

    let mut surface = format!("{inventory:?}");
    if let Some(page) = eso::table_page(&inventory) {
        for row in page.rows {
            for cell in row.cells {
                surface.push('\n');
                surface.push_str(&cell);
            }
        }
    }
    surface.push('\n');
    surface.push_str(&eso::render(&inventory).join("\n"));
    assert!(
        !surface.contains(PLANTED),
        "a planted token must not survive fetch into Debug or table cells: {surface}"
    );
    drop(runtime);
}
