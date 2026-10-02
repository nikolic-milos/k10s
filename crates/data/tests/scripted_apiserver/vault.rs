//! Vault / OpenBao CRs listed through kube Request: the group probe, 404 vs
//! 403, the wire path, a planted token, table presence, and paging.

use crate::*;
use k10s_data::read::Fetched;
use k10s_data::vault::{self, HASHICORP_GROUP, KindSet};

const PLANTED: &str = "PLANTED-TOKEN-9f3a";

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn hashicorp_group() -> String {
    r#"{"kind":"APIGroup","name":"secrets.hashicorp.com",
        "versions":[{"groupVersion":"secrets.hashicorp.com/v1beta1","version":"v1beta1"}],
        "preferredVersion":{"groupVersion":"secrets.hashicorp.com/v1beta1","version":"v1beta1"}}"#
        .to_string()
}

fn connection_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "vault", "namespace": "vault-system" },
        "spec": {
            "address": "https://vault.example.com:8200",
            "token": PLANTED,
            "headers": { "X-Vault-Token": PLANTED }
        },
        "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
    })
}

fn static_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": {
            "name": "app",
            "namespace": "prod",
            "labels": { "app.kubernetes.io/name": "openbao" }
        },
        "spec": {
            "mount": "secret",
            "path": "app/config",
            "refreshAfter": "60s",
            "data": { "password": PLANTED },
            "ciphertext": PLANTED
        },
        "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
    })
}

fn list(kind: &str, items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": format!("{kind}List"),
        "apiVersion": "secrets.hashicorp.com/v1beta1",
        "metadata": {},
        "items": items
    })
    .to_string()
}

fn script_empty_kind(script: &Script, plural: &str) {
    script.route(
        "GET",
        &format!("/apis/secrets.hashicorp.com/v1beta1/{plural}?"),
        200,
        list("List", &[]),
    );
}

#[test]
fn a_404_on_the_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { vault::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.connections, KindSet::NotServed));
    assert!(vault::table_page(&inventory).is_none());
    assert!(
        script.requests_for("vaultconnections").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    let groups: Vec<_> = script.seen().into_iter().map(|seen| seen.path).collect();
    assert!(
        groups
            .iter()
            .any(|path| path == "/apis/secrets.hashicorp.com"
                || path.starts_with("/apis/secrets.hashicorp.com?")),
        "hashicorp is probed: {groups:?}"
    );
    assert!(
        groups
            .iter()
            .all(|path| !path.starts_with("/apis/openbao.org")),
        "openbao.org was never an OpenBao Secrets Operator group, so it is not probed: {groups:?}"
    );
    drop(runtime);
}

#[test]
fn a_403_hashicorp_group_is_denied_on_every_kind() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/secrets.hashicorp.com",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { vault::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.connections, KindSet::Denied));
    assert!(inventory.connections.served());
    assert!(vault::table_page(&inventory).is_some());
    assert!(
        script.requests_for("vaultconnections").is_empty(),
        "a 403 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn vault_objects_are_listed_from_the_crs_and_a_planted_token_does_not_leak() {
    let script = Script::default();
    script.route("GET", "/apis/secrets.hashicorp.com", 200, hashicorp_group());
    script.route(
        "GET",
        "/apis/secrets.hashicorp.com/v1beta1/vaultconnections?",
        200,
        serde_json::json!({
            "kind": "VaultConnectionList",
            "metadata": { "continue": "page-2" },
            "items": [connection_item()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/secrets.hashicorp.com/v1beta1/vaultconnections?",
        200,
        list(
            "VaultConnection",
            &[serde_json::json!({
                "metadata": { "name": "backup", "namespace": "vault-system" },
                "spec": { "address": "https://vault-backup.example.com:8200" }
            })],
        ),
    );
    script_empty_kind(&script, "vaultauths");
    script.route(
        "GET",
        "/apis/secrets.hashicorp.com/v1beta1/vaultstaticsecrets?",
        200,
        list("VaultStaticSecret", &[static_item()]),
    );
    script_empty_kind(&script, "vaultdynamicsecrets");
    script_empty_kind(&script, "vaultpkisecrets");

    let runtime = runtime();
    let fetched = runtime.block_on(async { vault::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve: {fetched:?}");
    };
    assert_eq!(
        inventory
            .connections
            .items()
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["vault", "backup"]
    );
    assert_eq!(
        inventory.connections.items()[0].address,
        "https://vault.example.com:8200"
    );
    let static_secret = &inventory.static_secrets.items()[0];
    assert_eq!(static_secret.secret_path, "secret/app/config");
    assert!(static_secret.openbao);
    assert!(vault::table_page(&inventory).is_some());

    let lists = script.requests_for("vaultconnections");
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );

    let mut surface = format!("{inventory:?}");
    if let Some(page) = vault::table_page(&inventory) {
        for row in page.rows {
            for cell in row.cells {
                surface.push('\n');
                surface.push_str(&cell);
            }
        }
    }
    surface.push('\n');
    surface.push_str(&vault::render(&inventory).join("\n"));
    assert!(
        !surface.contains(PLANTED),
        "a planted token must not survive fetch into Debug or table cells: {surface}"
    );
    drop(runtime);
}

#[test]
fn an_openbao_install_is_attributed_from_labels_without_a_second_group_probe() {
    let script = Script::default();
    script.route("GET", "/apis/secrets.hashicorp.com", 200, hashicorp_group());
    let mut item = connection_item();
    item["metadata"]["labels"] = serde_json::json!({ "app.kubernetes.io/name": "openbao" });
    script.route(
        "GET",
        "/apis/secrets.hashicorp.com/v1beta1/vaultconnections?",
        200,
        list("VaultConnection", &[item]),
    );
    script_empty_kind(&script, "vaultauths");
    script_empty_kind(&script, "vaultstaticsecrets");
    script_empty_kind(&script, "vaultdynamicsecrets");
    script_empty_kind(&script, "vaultpkisecrets");

    let runtime = runtime();
    let fetched = runtime.block_on(async { vault::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("the hashicorp group serving is enough: {fetched:?}");
    };
    assert!(inventory.served());
    let connection = &inventory.connections.items()[0];
    assert_eq!(connection.group, HASHICORP_GROUP);
    assert!(connection.openbao);
    assert!(vault::table_page(&inventory).is_some());
    let groups: Vec<_> = script.seen().into_iter().map(|seen| seen.path).collect();
    assert!(
        groups
            .iter()
            .all(|path| !path.starts_with("/apis/openbao.org")),
        "the archived OpenBao operator served secrets.hashicorp.com, not openbao.org: {groups:?}"
    );
    drop(runtime);
}
