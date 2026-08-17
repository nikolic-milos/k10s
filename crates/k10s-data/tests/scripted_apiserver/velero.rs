//! Velero CRs listed through kube Request, with a confirmed Backup apply
//! proven as the SSA body of a velero.io Backup CR.

use crate::*;
use k10s_data::read::Fetched;
use k10s_data::velero::{self, BackupDocument, Confirm, KindSet};

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn velero_group() -> String {
    r#"{"kind":"APIGroup","name":"velero.io",
        "versions":[{"groupVersion":"velero.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"velero.io/v1","version":"v1"}}"#
        .to_string()
}

fn backup_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "nightly", "namespace": "velero" },
        "spec": {
            "includedNamespaces": ["prod"],
            "storageLocation": "default"
        },
        "status": {
            "phase": "Completed",
            "warnings": 2,
            "errors": 0,
            "startTimestamp": "2026-08-14T01:00:00Z"
        }
    })
}

fn backup_doc() -> BackupDocument {
    BackupDocument {
        name: "adhoc".into(),
        namespace: "velero".into(),
        included_namespaces: vec!["prod".into()],
        storage_location: "default".into(),
    }
}

#[test]
fn a_404_velero_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { velero::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.backups, KindSet::NotServed));
    assert!(
        script.requests_for("backups").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_velero_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/velero.io", 403, status(403, "Forbidden"));
    let runtime = runtime();
    let fetched = runtime.block_on(async { velero::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.backups, KindSet::Denied));
    assert!(
        inventory.backups.served(),
        "403 is Denied, not served: false"
    );
    drop(runtime);
}

#[test]
fn velero_objects_are_listed_from_the_crs_and_a_backup_apply_is_ssa() {
    let script = Script::default();
    script.route("GET", "/apis/velero.io", 200, velero_group());
    script.route(
        "GET",
        "/apis/velero.io/v1/backups?",
        200,
        serde_json::json!({
            "kind": "BackupList",
            "items": [backup_item()]
        })
        .to_string(),
    );
    script.route(
        "PATCH",
        "/apis/velero.io/v1/namespaces/velero/backups/adhoc",
        200,
        backup_item().to_string(),
    );

    let runtime = runtime();
    let (inventory, preview, sent) = runtime.block_on(async {
        let client = script.client();
        (
            velero::fetch(&client, None).await,
            velero::apply_backup(&client, &backup_doc(), false).await,
            velero::apply_backup(&client, &backup_doc(), true).await,
        )
    });
    let Fetched::Ok(inventory) = inventory else {
        panic!("a served listing must resolve");
    };
    let backup = &inventory.backups.items()[0];
    assert_eq!(backup.name, "nightly");
    assert_eq!(backup.phase, "Completed");
    assert_eq!(backup.warnings, 2);
    assert_eq!(backup.storage_location, "default");
    assert_eq!(preview, Fetched::Ok(Confirm::Needed));
    assert_eq!(sent, Fetched::Ok(Confirm::Sent));

    let patches = script
        .seen()
        .into_iter()
        .filter(|seen| seen.method == "PATCH")
        .collect::<Vec<_>>();
    assert_eq!(
        patches.len(),
        1,
        "confirm=false must not PATCH: {patches:?}"
    );
    assert_eq!(patches[0].content_type, "application/apply-patch+yaml");
    let body: serde_json::Value = serde_json::from_str(&patches[0].body).expect("json");
    assert_eq!(body["kind"], "Backup");
    assert_eq!(body["apiVersion"], "velero.io/v1");
    assert_eq!(body["spec"]["storageLocation"], "default");
    drop(runtime);
}
