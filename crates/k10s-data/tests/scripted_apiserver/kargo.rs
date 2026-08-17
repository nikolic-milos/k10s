//! Kargo CRs listed through kube Request, with refresh proven as the
//! merge-patch of `kargo.akuity.io/refresh`.

use crate::*;
use k10s_data::kargo::{self, Confirm, KindSet};
use k10s_data::read::Fetched;

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn kargo_group() -> String {
    r#"{"kind":"APIGroup","name":"kargo.akuity.io",
        "versions":[{"groupVersion":"kargo.akuity.io/v1alpha1","version":"v1alpha1"}],
        "preferredVersion":{"groupVersion":"kargo.akuity.io/v1alpha1","version":"v1alpha1"}}"#
        .to_string()
}

fn stage_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "test", "namespace": "demo" },
        "spec": {
            "requestedFreight": [{
                "origin": { "kind": "Warehouse", "name": "app" }
            }]
        },
        "status": {
            "health": { "status": "Healthy" },
            "freightSummary": "abc123",
            "freightHistory": [{
                "items": { "Warehouse/app": { "name": "abc123" } },
                "verificationHistory": [{ "phase": "Successful" }]
            }]
        }
    })
}

fn target() -> kargo::Resource {
    kargo::Resource {
        kind: kargo::Kind::Stage,
        version: "v1alpha1".into(),
        name: "test".into(),
        namespace: "demo".into(),
        uid: String::new(),
        phase: String::new(),
        health: String::new(),
        freight: String::new(),
        verified: String::new(),
        warehouse: String::new(),
    }
}

#[test]
fn a_404_kargo_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { kargo::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.stages, KindSet::NotServed));
    assert!(
        script.requests_for("stages").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_kargo_group_is_denied_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/kargo.akuity.io",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { kargo::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.stages, KindSet::Denied));
    assert!(
        inventory.stages.served(),
        "403 is Denied, not served: false"
    );
    drop(runtime);
}

#[test]
fn kargo_objects_are_listed_from_the_crs_and_refresh_is_the_documented_annotation() {
    let script = Script::default();
    script.route("GET", "/apis/kargo.akuity.io", 200, kargo_group());
    script.route(
        "GET",
        "/apis/kargo.akuity.io/v1alpha1/stages?",
        200,
        serde_json::json!({
            "kind": "StageList",
            "items": [stage_item()]
        })
        .to_string(),
    );
    script.route(
        "PATCH",
        "/apis/kargo.akuity.io/v1alpha1/namespaces/demo/stages/test",
        200,
        stage_item().to_string(),
    );

    let runtime = runtime();
    let (inventory, preview, sent) = runtime.block_on(async {
        let client = script.client();
        (
            kargo::fetch(&client, None).await,
            kargo::refresh(&client, &target(), false).await,
            kargo::refresh(&client, &target(), true).await,
        )
    });
    let Fetched::Ok(inventory) = inventory else {
        panic!("a served listing must resolve");
    };
    let stage = &inventory.stages.items()[0];
    assert_eq!(stage.name, "test");
    assert_eq!(stage.health, "Healthy");
    assert_eq!(stage.freight, "abc123");
    assert_eq!(stage.verified, "Successful");
    assert_eq!(stage.warehouse, "Warehouse/app");
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
    assert_eq!(patches[0].content_type, "application/merge-patch+json");
    let body: serde_json::Value = serde_json::from_str(&patches[0].body).expect("json");
    assert!(
        body.pointer("/metadata/annotations/kargo.akuity.io~1refresh")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|at| !at.is_empty()),
        "refresh writes the documented annotation: {}",
        patches[0].body
    );
    drop(runtime);
}
