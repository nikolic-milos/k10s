//! Field extraction, caps, the document, 404/403 classification, and the
//! planted-token drop. A cluster is not required.

use super::*;
use serde_json::json;

const PLANTED: &str = "PLANTED-TOKEN-9f3a";

fn connection_json() -> Value {
    json!({
        "metadata": { "name": "vault", "namespace": "vault-system", "uid": "uid-conn" },
        "spec": {
            "address": "https://vault.example.com:8200",
            "token": PLANTED,
            "headers": { "X-Vault-Token": PLANTED }
        },
        "status": { "valid": true, "conditions": [{ "type": "Ready", "status": "True" }] }
    })
}

fn auth_json() -> Value {
    json!({
        "metadata": { "name": "kube", "namespace": "vault-system" },
        "spec": {
            "method": "kubernetes",
            "mount": "kubernetes",
            "token": PLANTED,
            "jwt": PLANTED,
            "auth": { "token": PLANTED, "jwt": PLANTED },
            "kubernetes": { "role": "default", "jwt": PLANTED }
        },
        "status": { "valid": true }
    })
}

fn static_secret_json() -> Value {
    json!({
        "metadata": {
            "name": "app",
            "namespace": "prod",
            "labels": { "app.kubernetes.io/name": "openbao" }
        },
        "spec": {
            "mount": "secret",
            "path": "app/config",
            "type": "kv-v2",
            "refreshAfter": "60s",
            "data": { "password": PLANTED },
            "ciphertext": PLANTED,
            "token": PLANTED
        },
        "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
    })
}

fn dynamic_secret_json() -> Value {
    json!({
        "metadata": { "name": "db", "namespace": "prod" },
        "spec": {
            "mount": "database",
            "path": "creds/app",
            "refreshAfter": "1h",
            "data": { "password": PLANTED }
        },
        "status": { "conditions": [{ "type": "Ready", "status": "False" }] }
    })
}

fn pki_secret_json() -> Value {
    json!({
        "metadata": { "name": "www", "namespace": "prod" },
        "spec": {
            "mount": "pki",
            "role": "example",
            "expiryOffset": "12h"
        },
        "status": { "conditions": [{ "type": "Ready", "status": "Unknown" }] }
    })
}

fn resource_from(kind: Kind, group: &str, version: &str, value: Value) -> Resource {
    parse_item(kind, group, version, value).expect("the fixture is a Vault object")
}

fn leak_surface(inventory: &Inventory) -> String {
    let mut text = format!("{inventory:?}");
    if let Some(page) = table_page(inventory) {
        for row in &page.rows {
            for cell in &row.cells {
                text.push('\n');
                text.push_str(cell);
            }
        }
    }
    text.push('\n');
    text.push_str(&render(inventory).join("\n"));
    text
}

fn planted_inventory() -> Inventory {
    Inventory {
        connections: KindSet::Served {
            items: vec![resource_from(
                Kind::VaultConnection,
                HASHICORP_GROUP,
                "v1beta1",
                connection_json(),
            )],
            truncated: false,
            unreadable: 0,
        },
        auths: KindSet::Served {
            items: vec![resource_from(
                Kind::VaultAuth,
                HASHICORP_GROUP,
                "v1beta1",
                auth_json(),
            )],
            truncated: false,
            unreadable: 0,
        },
        static_secrets: KindSet::Served {
            items: vec![resource_from(
                Kind::VaultStaticSecret,
                HASHICORP_GROUP,
                "v1beta1",
                static_secret_json(),
            )],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    }
}

#[test]
fn a_connection_keeps_the_address_and_drops_the_token() {
    let resource = resource_from(
        Kind::VaultConnection,
        HASHICORP_GROUP,
        "v1beta1",
        connection_json(),
    );
    assert_eq!(resource.address, "https://vault.example.com:8200");
    assert_eq!(resource.ready, "True");
    assert!(!resource.openbao);
    assert_eq!(resource.group, HASHICORP_GROUP);
}

#[test]
fn an_auth_keeps_the_method_type() {
    let resource = resource_from(Kind::VaultAuth, HASHICORP_GROUP, "v1beta1", auth_json());
    assert_eq!(resource.auth_method, "kubernetes");
    assert_eq!(resource.ready, "True");
}

#[test]
fn a_static_secret_keeps_the_path_and_marks_openbao_from_labels() {
    let resource = resource_from(
        Kind::VaultStaticSecret,
        HASHICORP_GROUP,
        "v1beta1",
        static_secret_json(),
    );
    assert_eq!(resource.secret_path, "secret/app/config");
    assert_eq!(resource.refresh, "60s");
    assert!(resource.openbao);
}

#[test]
fn a_dynamic_secret_and_a_pki_secret_keep_path_or_role() {
    let dynamic = resource_from(
        Kind::VaultDynamicSecret,
        HASHICORP_GROUP,
        "v1beta1",
        dynamic_secret_json(),
    );
    assert_eq!(dynamic.secret_path, "database/creds/app");
    assert_eq!(dynamic.refresh, "1h");
    let pki = resource_from(
        Kind::VaultPKISecret,
        HASHICORP_GROUP,
        "v1beta1",
        pki_secret_json(),
    );
    assert_eq!(pki.secret_path, "pki/example");
    assert_eq!(
        pki.refresh, "12h",
        "a VaultPKISecret's cadence is spec.expiryOffset"
    );
}

#[test]
fn refresh_interval_is_no_vso_field_and_confers_no_cadence() {
    let mut value = pki_secret_json();
    value["spec"]["expiryOffset"] = json!("");
    value["spec"]["refreshInterval"] = json!("12h");
    let resource = resource_from(Kind::VaultPKISecret, HASHICORP_GROUP, "v1beta1", value);
    assert_eq!(resource.refresh, "");
}

#[test]
fn an_openbao_annotation_marks_openbao_and_a_bare_object_is_vault() {
    let mut value = connection_json();
    value["metadata"]["annotations"] = json!({ "meta.helm.sh/release-name": "openbao" });
    let annotated = resource_from(Kind::VaultConnection, HASHICORP_GROUP, "v1beta1", value);
    assert!(annotated.openbao);
    let bare = resource_from(
        Kind::VaultConnection,
        HASHICORP_GROUP,
        "v1beta1",
        connection_json(),
    );
    assert!(!bare.openbao);
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::VaultConnection, HASHICORP_GROUP, "v1beta1", json!({})).is_none());
}

#[test]
fn a_planted_token_does_not_appear_in_debug_or_table_cells() {
    let inventory = planted_inventory();
    let text = leak_surface(&inventory);
    assert!(
        !text.contains(PLANTED),
        "a planted token, jwt, or ciphertext must not reach Debug, table, or render: {text}"
    );
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = json!({
        "metadata": { "name": huge, "namespace": huge },
        "spec": {
            "address": huge,
            "method": huge,
            "mount": huge,
            "path": huge,
            "refreshAfter": huge
        }
    });
    let resource = resource_from(Kind::VaultStaticSecret, HASHICORP_GROUP, "v1beta1", value);
    for field in [
        &resource.name,
        &resource.namespace,
        &resource.address,
        &resource.auth_method,
        &resource.secret_path,
        &resource.refresh,
    ] {
        assert!(
            field.chars().count() <= MAX_FIELD_CHARS + 1,
            "every field is clipped where it is carried: {} chars",
            field.chars().count()
        );
        assert!(field.ends_with('\u{2026}'), "and looks clipped");
    }
}

#[test]
fn the_listing_cap_is_stated_when_it_bites() {
    let values =
        (0..=MAX_OBJECTS).map(|index| json!({ "metadata": { "name": format!("secret-{index}") } }));
    let (items, truncated, unreadable) =
        collect_items(Kind::VaultStaticSecret, HASHICORP_GROUP, "v1beta1", values);
    assert_eq!(items.len(), MAX_OBJECTS);
    assert!(truncated);
    assert_eq!(unreadable, 0);
}

#[test]
fn a_missing_group_is_invisible_and_a_forbidden_one_is_denied() {
    assert!(matches!(
        after_group(&api_error(404)),
        GroupAnswer::NotServed
    ));
    assert!(matches!(after_group(&api_error(403)), GroupAnswer::Denied));
    assert!(matches!(after_group(&api_error(401)), GroupAnswer::Denied));
    assert!(matches!(
        after_group(&api_error(500)),
        GroupAnswer::Failed(_)
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
fn lists_are_namespaced_when_a_namespace_is_named() {
    assert_eq!(
        collection_url(HASHICORP_GROUP, Kind::VaultConnection, "v1beta1", None),
        "/apis/secrets.hashicorp.com/v1beta1/vaultconnections"
    );
    assert_eq!(
        collection_url(
            HASHICORP_GROUP,
            Kind::VaultStaticSecret,
            "v1beta1",
            Some("prod")
        ),
        "/apis/secrets.hashicorp.com/v1beta1/namespaces/prod/vaultstaticsecrets"
    );
}

#[test]
fn versions_try_the_group_document_then_v1beta1_and_v1() {
    assert_eq!(
        versions_for(Kind::VaultConnection, &["v1beta1".into()]),
        vec!["v1beta1".to_string(), "v1".to_string()]
    );
}

#[test]
fn an_unserved_vault_inventory_has_no_table() {
    assert!(table_page(&Inventory::default()).is_none());
}

#[test]
fn a_denied_vault_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        connections: KindSet::Denied,
        ..Inventory::default()
    })
    .expect("Denied is served, so the table exists");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("access denied for this account"), "{text}");
    assert!(text.contains("VaultConnection"), "{text}");
}

#[test]
fn a_served_vault_fixture_is_one_row_per_object() {
    let connection = resource_from(
        Kind::VaultConnection,
        HASHICORP_GROUP,
        "v1beta1",
        connection_json(),
    );
    let page = table_page(&Inventory {
        connections: KindSet::Served {
            items: vec![connection],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "vault");
    assert_eq!(page.rows[0].cells[4], "Vault");
    assert_eq!(page.rows[0].cells[5], "https://vault.example.com:8200");
    assert!(!page.rows[0].cells.join(" ").contains(PLANTED));
}

#[test]
fn a_missing_vault_group_renders_as_not_installed_rather_than_empty() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served());
    assert_eq!(
        lines[0],
        "Vault and OpenBao Secrets Operator CRs are not served by this cluster"
    );
    let text = lines.join("\n");
    assert!(text.contains("VaultConnection"), "{text}");
    assert!(text.contains("secrets.hashicorp.com"), "{text}");
    assert!(
        !text.contains("openbao.org"),
        "openbao.org is not an OpenBao Secrets Operator group: {text}"
    );
    assert!(text.contains("HTTP API is not spoken"), "{text}");
}

#[test]
fn a_history_renders_address_path_vendor_and_states_a_cap() {
    let inventory = planted_inventory();
    let lines = render(&Inventory {
        connections: inventory.connections,
        static_secrets: KindSet::Served {
            items: planted_inventory().static_secrets.items().to_vec(),
            truncated: true,
            unreadable: 1,
        },
        dynamic_secrets: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.contains("vault-system/vault"), "{text}");
    assert!(text.contains("https://vault.example.com:8200"), "{text}");
    assert!(text.contains("secret/app/config"), "{text}");
    assert!(text.contains("OpenBao"), "{text}");
    assert!(text.contains("stopped at"), "{text}");
    assert!(
        text.contains("vault dynamic secrets: access denied for this account"),
        "{text}"
    );
    assert!(!text.contains(PLANTED), "{text}");
}
