//! CloudNativePG CRs listed through kube Request. A planted password on a
//! fixture Cluster must not appear in the inventory.

use crate::*;
use k10s_data::cnpg::{self, KindSet};
use k10s_data::read::Fetched;

const PLANTED_PASSWORD: &str = "planted-s3cret-must-not-leak";

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn cnpg_group() -> String {
    r#"{"kind":"APIGroup","name":"postgresql.cnpg.io",
        "versions":[{"groupVersion":"postgresql.cnpg.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"postgresql.cnpg.io/v1","version":"v1"}}"#
        .to_string()
}

fn cluster_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "app", "namespace": "data" },
        "spec": {
            "instances": 3,
            "superuserSecret": { "name": "app-superuser", "password": PLANTED_PASSWORD }
        },
        "status": {
            "instances": 3,
            "readyInstances": 2,
            "currentPrimary": "app-1",
            "phase": "Cluster in healthy state",
            "pgDataImageInfo": { "majorVersion": 16 },
            "password": PLANTED_PASSWORD
        }
    })
}

#[test]
fn a_404_cnpg_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { cnpg::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.clusters, KindSet::NotServed));
    assert!(
        script.requests_for("clusters").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_cnpg_group_is_denied_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/postgresql.cnpg.io",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { cnpg::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.clusters, KindSet::Denied));
    assert!(
        inventory.clusters.served(),
        "403 is Denied, not served: false"
    );
    drop(runtime);
}

#[test]
fn cnpg_objects_are_listed_from_the_crs_and_a_password_stays_off_the_row() {
    let script = Script::default();
    script.route("GET", "/apis/postgresql.cnpg.io", 200, cnpg_group());
    script.route(
        "GET",
        "/apis/postgresql.cnpg.io/v1/clusters?",
        200,
        serde_json::json!({
            "kind": "ClusterList",
            "items": [cluster_item()]
        })
        .to_string(),
    );

    let runtime = runtime();
    let fetched = runtime.block_on(async { cnpg::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a served listing must resolve");
    };
    let cluster = &inventory.clusters.items()[0];
    assert_eq!(cluster.name, "app");
    assert_eq!(cluster.ready_instances, 2);
    assert_eq!(cluster.instances, 3);
    assert_eq!(cluster.primary, "app-1");
    assert_eq!(cluster.postgres_version, "16");
    assert_eq!(cluster.superuser_secret, "app-superuser");
    let shown = format!("{cluster:?}");
    assert!(
        !shown.contains(PLANTED_PASSWORD),
        "a planted password must not be carried: {shown}"
    );
    let page = cnpg::table_page(&inventory).expect("a served group is a table");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(!text.contains(PLANTED_PASSWORD), "{text}");
    drop(runtime);
}
