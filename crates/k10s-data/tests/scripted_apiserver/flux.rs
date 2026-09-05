//! Flux CRs listed through kube Request, with suspend and reconcile-now proven
//! as the merge-patch bodies Flux already honours.

use crate::*;
use k10s_data::flux::{self, KindSet};
use k10s_data::read::Fetched;

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

fn source_group() -> String {
    r#"{"kind":"APIGroup","name":"source.toolkit.fluxcd.io",
        "versions":[{"groupVersion":"source.toolkit.fluxcd.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"source.toolkit.fluxcd.io/v1","version":"v1"}}"#
        .to_string()
}

fn git_item() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "podinfo", "namespace": "flux-system" },
        "spec": { "url": "https://github.com/stefanprodan/podinfo" },
        "status": {
            "conditions": [{ "type": "Ready", "status": "True" }],
            "artifact": { "revision": "master@sha1:abc123" }
        }
    })
}

fn target() -> flux::Resource {
    flux::Resource {
        kind: flux::Kind::GitRepository,
        version: "v1".into(),
        name: "podinfo".into(),
        namespace: "flux-system".into(),
        uid: String::new(),
        ready: "True".into(),
        suspended: false,
        last_applied_revision: String::new(),
        source_ref: String::new(),
    }
}

#[test]
fn a_404_flux_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime.block_on(async { flux::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.git_repositories, KindSet::NotServed));
    assert!(
        script.requests_for("gitrepositories").is_empty(),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_403_flux_group_is_denied_not_absent() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/source.toolkit.fluxcd.io",
        403,
        status(403, "Forbidden"),
    );
    let runtime = runtime();
    let fetched = runtime.block_on(async { flux::fetch(&script.client(), None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a forbidden group is Denied on that kind: {fetched:?}");
    };
    assert!(matches!(inventory.git_repositories, KindSet::Denied));
    assert!(
        inventory.git_repositories.served(),
        "403 is Denied, not served: false"
    );
    drop(runtime);
}

#[test]
fn flux_objects_are_listed_from_the_crs_and_actions_are_merge_patches() {
    let script = Script::default();
    script.route("GET", "/apis/source.toolkit.fluxcd.io", 200, source_group());
    script.route(
        "GET",
        "/apis/source.toolkit.fluxcd.io/v1/gitrepositories?",
        200,
        serde_json::json!({
            "kind": "GitRepositoryList",
            "items": [git_item()]
        })
        .to_string(),
    );
    script.route(
        "PATCH",
        "/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/podinfo",
        200,
        git_item().to_string(),
    );
    script.route(
        "PATCH",
        "/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/podinfo",
        200,
        git_item().to_string(),
    );

    let runtime = runtime();
    let (inventory, suspend, denied, resume) = runtime.block_on(async {
        let client = script.client();
        (
            flux::fetch(&client, None).await,
            flux::set_suspended(&client, &target(), true, true).await,
            flux::reconcile_now(&client, &target(), false).await,
            flux::set_suspended(&client, &target(), false, true).await,
        )
    });
    let Fetched::Ok(inventory) = inventory else {
        panic!("a served listing must resolve");
    };
    let git = &inventory.git_repositories.items()[0];
    assert_eq!(git.name, "podinfo");
    assert_eq!(git.last_applied_revision, "master@sha1:abc123");
    assert_eq!(git.source_ref, "https://github.com/stefanprodan/podinfo");
    assert_eq!(suspend, Fetched::Ok(()));
    assert_eq!(
        denied,
        Fetched::Denied {
            what: "flux gitrepositories"
        }
    );
    assert_eq!(resume, Fetched::Ok(()));

    let patches = script
        .seen()
        .into_iter()
        .filter(|seen| seen.method == "PATCH")
        .collect::<Vec<_>>();
    assert_eq!(
        patches.len(),
        2,
        "the denied reconcile must not PATCH: {patches:?}"
    );
    for patch in &patches {
        assert_eq!(patch.content_type, "application/merge-patch+json");
    }
    assert_eq!(patches[0].body, r#"{"spec":{"suspend":true}}"#);
    assert_eq!(patches[1].body, r#"{"spec":{"suspend":false}}"#);
    drop(runtime);
}
