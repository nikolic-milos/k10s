//! Field extraction, caps, the document, 404/403 classification, and the
//! planted-token drop. A cluster is not required.

use super::*;
use serde_json::json;

const PLANTED: &str = "PLANTED-TOKEN-9f3a";

fn secret_store_json() -> Value {
    json!({
        "metadata": { "name": "vault-backend", "namespace": "prod", "uid": "uid-ss" },
        "spec": {
            "refreshInterval": 3600,
            "provider": {
                "vault": {
                    "server": "https://vault.example.com",
                    "auth": {
                        "token": PLANTED,
                        "tokenSecretRef": { "name": "vault-token", "key": "token" }
                    }
                }
            }
        },
        "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
    })
}

fn cluster_store_json() -> Value {
    json!({
        "metadata": { "name": "aws-cluster" },
        "spec": { "provider": { "aws": { "service": "SecretsManager" } } },
        "status": { "conditions": [{ "type": "Ready", "status": "False" }] }
    })
}

fn external_secret_json() -> Value {
    json!({
        "metadata": { "name": "db", "namespace": "prod", "uid": "uid-es" },
        "spec": {
            "refreshInterval": "1h",
            "secretStoreRef": { "name": "vault-backend", "kind": "SecretStore" },
            "target": { "name": "db-creds" },
            "data": [
                {
                    "secretKey": "username",
                    "remoteRef": { "key": PLANTED, "property": "username" }
                },
                {
                    "secretKey": "password",
                    "remoteRef": { "key": PLANTED, "property": "password" }
                }
            ]
        },
        "status": {
            "conditions": [{ "type": "Ready", "status": "True" }],
            "binding": { "name": "db-creds" }
        }
    })
}

fn cluster_external_secret_json() -> Value {
    json!({
        "metadata": { "name": "cluster-db" },
        "spec": {
            "externalSecretSpec": {
                "refreshInterval": "30m",
                "secretStoreRef": { "kind": "ClusterSecretStore", "name": "aws-cluster" },
                "target": { "name": "cluster-db-creds" },
                "data": [{ "secretKey": "password", "remoteRef": { "key": PLANTED } }]
            }
        },
        "status": { "conditions": [{ "type": "Ready", "status": "Unknown" }] }
    })
}

fn resource_from(kind: Kind, version: &str, value: Value) -> Resource {
    parse_item(kind, version, value).expect("the fixture is an ESO object")
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
        secret_stores: KindSet::Served {
            items: vec![resource_from(Kind::SecretStore, "v1", secret_store_json())],
            truncated: false,
            unreadable: 0,
        },
        external_secrets: KindSet::Served {
            items: vec![resource_from(
                Kind::ExternalSecret,
                "v1",
                external_secret_json(),
            )],
            truncated: false,
            unreadable: 0,
        },
        cluster_external_secrets: KindSet::Served {
            items: vec![resource_from(
                Kind::ClusterExternalSecret,
                "v1",
                cluster_external_secret_json(),
            )],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    }
}

#[test]
fn a_secret_store_keeps_the_driver_and_drops_provider_auth() {
    let resource = resource_from(Kind::SecretStore, "v1", secret_store_json());
    assert_eq!(resource.name, "vault-backend");
    assert_eq!(resource.namespace, "prod");
    assert_eq!(resource.store_type, "vault");
    assert_eq!(resource.ready, "True");
    assert_eq!(
        resource.refresh_interval, "3600s",
        "a store's refreshInterval is an integer number of seconds on the \
         wire; a store that sets it must still list"
    );
    assert!(resource.target_secret.is_empty());
    assert!(resource.key_names.is_empty());
}

#[test]
fn an_external_secret_keeps_the_target_name_and_spec_data_key_names() {
    let resource = resource_from(Kind::ExternalSecret, "v1", external_secret_json());
    assert_eq!(resource.store_type, "SecretStore/vault-backend");
    assert_eq!(resource.refresh_interval, "1h");
    assert_eq!(resource.target_secret, "db-creds");
    assert_eq!(resource.key_names, vec!["username", "password"]);
    assert_eq!(resource.ready, "True");
}

#[test]
fn a_cluster_external_secret_reads_the_nested_spec() {
    let resource = resource_from(
        Kind::ClusterExternalSecret,
        "v1",
        cluster_external_secret_json(),
    );
    assert_eq!(resource.namespace, "");
    assert_eq!(resource.store_type, "ClusterSecretStore/aws-cluster");
    assert_eq!(resource.refresh_interval, "30m");
    assert_eq!(resource.target_secret, "cluster-db-creds");
    assert_eq!(resource.key_names, vec!["password"]);
    assert_eq!(resource.ready, "Unknown");
}

#[test]
fn a_cluster_secret_store_is_cluster_scoped() {
    let resource = resource_from(Kind::ClusterSecretStore, "v1", cluster_store_json());
    assert_eq!(resource.namespace, "");
    assert_eq!(resource.store_type, "aws");
    assert_eq!(resource.ready, "False");
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::ExternalSecret, "v1", json!({})).is_none());
}

#[test]
fn a_planted_token_does_not_appear_in_debug_or_table_cells() {
    let inventory = planted_inventory();
    let text = leak_surface(&inventory);
    assert!(
        !text.contains(PLANTED),
        "a planted token in spec.data or provider auth must not reach Debug, table, or render: {text}"
    );
    let store = &inventory.secret_stores.items()[0];
    assert_eq!(store.store_type, "vault");
    assert_ne!(store.store_type, PLANTED);
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = json!({
        "metadata": { "name": huge, "namespace": huge },
        "spec": {
            "refreshInterval": huge,
            "secretStoreRef": { "kind": huge, "name": huge },
            "target": { "name": huge },
            "data": [{ "secretKey": huge }]
        }
    });
    let resource = resource_from(Kind::ExternalSecret, "v1", value);
    for field in [
        &resource.name,
        &resource.namespace,
        &resource.refresh_interval,
        &resource.store_type,
        &resource.target_secret,
        &resource.key_names[0],
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
        (0..=MAX_OBJECTS).map(|index| json!({ "metadata": { "name": format!("es-{index}") } }));
    let (items, truncated, unreadable) = collect_items(Kind::ExternalSecret, "v1", values);
    assert_eq!(items.len(), MAX_OBJECTS);
    assert!(truncated);
    assert_eq!(unreadable, 0);
}

#[test]
fn spec_data_key_names_stop_at_the_key_cap() {
    let entries: Vec<Value> = (0..=MAX_KEY_NAMES)
        .map(|index| json!({ "secretKey": format!("key{index}") }))
        .collect();
    let value = json!({
        "metadata": { "name": "many" },
        "spec": { "data": entries }
    });
    let resource = resource_from(Kind::ExternalSecret, "v1", value);
    assert_eq!(resource.key_names.len(), MAX_KEY_NAMES);
}

#[test]
fn key_names_come_from_spec_data_and_no_status_field_can_invent_them() {
    let value = json!({
        "metadata": { "name": "db" },
        "spec": {
            "data": [{ "secretKey": "password", "remoteRef": { "key": "db/creds" } }],
            "dataFrom": [{ "extract": { "key": "db/all" } }]
        },
        "status": {
            "syncedKeys": ["fabricated"],
            "keys": ["fabricated"],
            "secretKeys": ["fabricated"]
        }
    });
    let resource = resource_from(Kind::ExternalSecret, "v1", value);
    assert_eq!(
        resource.key_names,
        vec!["password"],
        "no ESO version publishes key names in status, and dataFrom keys \
         cannot be known without reading the generated Secret"
    );
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
fn store_lists_are_cluster_scoped_and_namespaced_lists_honour_a_namespace() {
    assert_eq!(
        collection_url(Kind::ClusterSecretStore, "v1", Some("prod")),
        "/apis/external-secrets.io/v1/clustersecretstores"
    );
    assert_eq!(
        collection_url(Kind::SecretStore, "v1", Some("prod")),
        "/apis/external-secrets.io/v1/namespaces/prod/secretstores"
    );
    assert_eq!(
        collection_url(Kind::ExternalSecret, "v1beta1", None),
        "/apis/external-secrets.io/v1beta1/externalsecrets"
    );
}

#[test]
fn versions_try_the_group_document_then_v1_and_v1beta1() {
    assert_eq!(
        versions_for(Kind::ExternalSecret, &["v1".into()]),
        vec!["v1".to_string(), "v1beta1".to_string()]
    );
    assert_eq!(
        versions_for(Kind::SecretStore, &["v1beta1".into()]),
        vec!["v1beta1".to_string(), "v1".to_string()]
    );
}

#[test]
fn an_unserved_eso_inventory_has_no_table() {
    assert!(table_page(&Inventory::default()).is_none());
}

#[test]
fn a_denied_eso_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        external_secrets: KindSet::Denied,
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
    assert!(text.contains("ExternalSecret"), "{text}");
}

#[test]
fn a_served_eso_fixture_is_one_row_per_object() {
    let secret = resource_from(Kind::ExternalSecret, "v1", external_secret_json());
    let page = table_page(&Inventory {
        external_secrets: KindSet::Served {
            items: vec![secret],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "db");
    assert_eq!(page.rows[0].cells[6], "db-creds");
    assert_eq!(page.rows[0].cells[7], "username, password");
    assert!(!page.rows[0].cells.join(" ").contains(PLANTED));
}

#[test]
fn a_missing_eso_group_renders_as_not_installed_rather_than_empty() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served());
    assert_eq!(
        lines[0],
        "External Secrets Operator is not served by this cluster"
    );
    let text = lines.join("\n");
    assert!(text.contains("ExternalSecret"), "{text}");
    assert!(text.contains("never fetched"), "{text}");
}

#[test]
fn a_history_renders_store_target_keys_and_states_a_cap() {
    let inventory = planted_inventory();
    let lines = render(&Inventory {
        secret_stores: inventory.secret_stores,
        external_secrets: KindSet::Served {
            items: planted_inventory().external_secrets.items().to_vec(),
            truncated: true,
            unreadable: 1,
        },
        cluster_secret_stores: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.contains("prod/db"), "{text}");
    assert!(text.contains("secret db-creds"), "{text}");
    assert!(text.contains("keys username, password"), "{text}");
    assert!(text.contains("stopped at"), "{text}");
    assert!(
        text.contains("eso clustersecretstores: access denied for this account"),
        "{text}"
    );
    assert!(!text.contains(PLANTED), "{text}");
}
