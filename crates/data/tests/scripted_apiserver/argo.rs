//! Argo CD inventory listed as Application / ApplicationSet CRs, with refresh
//! and sync proven as the merge-patch bodies Argo already honours.

use crate::*;
use k10s_data::discover;
use k10s_data::read::Fetched;
use kube::discovery::{ApiCapabilities, ApiResource, Scope};

fn argo_target(kind: &str, plural: &str, ops: &[&str]) -> k10s_data::discover::KindTarget {
    let mut catalog = k10s_core::Catalog::new();
    discover::intern(
        &mut catalog,
        ApiResource {
            group: "argoproj.io".to_string(),
            version: "v1alpha1".to_string(),
            api_version: "argoproj.io/v1alpha1".to_string(),
            kind: kind.to_string(),
            plural: plural.to_string(),
        },
        &ApiCapabilities {
            scope: Scope::Namespaced,
            subresources: Vec::new(),
            operations: ops.iter().map(|op| (*op).to_string()).collect(),
        },
    )
}

fn application_item() -> String {
    r#"{"metadata":{"name":"guestbook","namespace":"argocd"},
        "spec":{"source":{"repoURL":"https://github.com/argoproj/argocd-example-apps.git",
                          "targetRevision":"HEAD","path":"guestbook"},
                "destination":{"server":"https://kubernetes.default.svc","namespace":"guestbook"}},
        "status":{"sync":{"status":"Synced","revision":"abc123"},
                  "health":{"status":"Healthy"},
                  "resources":[{"group":"apps","kind":"Deployment","namespace":"guestbook",
                                "name":"guestbook-ui","status":"Synced"}]}}"#
        .to_string()
}

fn status(code: u16, reason: &str) -> String {
    format!(
        r#"{{"kind":"Status","apiVersion":"v1","status":"Failure","code":{code},"reason":"{reason}","message":"{reason}"}}"#
    )
}

#[test]
fn a_cluster_without_argoproj_kinds_is_unserved_and_does_not_list() {
    let script = Script::default();
    let runtime = runtime();
    let fetched = runtime
        .block_on(async { k10s_data::argo::fetch_inventory(&script.client(), &[], None).await });
    let Fetched::Ok(inventory) = fetched else {
        panic!("absence is Ok with served false: {fetched:?}");
    };
    assert!(!inventory.served);
    assert!(inventory.applications.is_empty());
    assert!(
        script.requests_for("argoproj.io").is_empty(),
        "discovery miss must not probe a hardcoded path: {:?}",
        script.seen()
    );
    drop(runtime);
}

#[test]
fn a_404_list_is_unserved_and_a_403_list_is_denied() {
    let runtime = runtime();
    let apps = argo_target("Application", "applications", &["get", "list", "patch"]);

    let missing = Script::default();
    missing.route(
        "GET",
        "/apis/argoproj.io/v1alpha1/applications?",
        404,
        status(404, "NotFound"),
    );
    let fetched = runtime.block_on(async {
        k10s_data::argo::fetch_inventory(&missing.client(), std::slice::from_ref(&apps), None).await
    });
    let Fetched::Ok(inventory) = fetched else {
        panic!("a 404 is unserved, not an error: {fetched:?}");
    };
    assert!(!inventory.served);

    let denied = Script::default();
    denied.route(
        "GET",
        "/apis/argoproj.io/v1alpha1/applications?",
        403,
        status(403, "Forbidden"),
    );
    let fetched = runtime.block_on(async {
        k10s_data::argo::fetch_inventory(&denied.client(), &[apps], None).await
    });
    assert_eq!(
        fetched,
        Fetched::Denied {
            what: "argo applications"
        }
    );
    drop(runtime);
}

#[test]
fn applications_are_listed_cluster_wide_and_refresh_patches_the_annotation() {
    let script = Script::default();
    script.route(
        "GET",
        "/apis/argoproj.io/v1alpha1/applications?",
        200,
        format!(
            r#"{{"kind":"ApplicationList","apiVersion":"argoproj.io/v1alpha1",
                 "metadata":{{"continue":"page-2"}},"items":[{}]}}"#,
            application_item()
        ),
    );
    script.route(
        "GET",
        "/apis/argoproj.io/v1alpha1/applications?",
        200,
        r#"{"kind":"ApplicationList","apiVersion":"argoproj.io/v1alpha1","metadata":{},"items":[]}"#,
    );
    script.route(
        "GET",
        "/apis/argoproj.io/v1alpha1/applicationsets?",
        200,
        r#"{"kind":"ApplicationSetList","apiVersion":"argoproj.io/v1alpha1","metadata":{},"items":[]}"#,
    );
    script.route(
        "PATCH",
        "/apis/argoproj.io/v1alpha1/namespaces/argocd/applications/guestbook",
        200,
        "{}",
    );

    let runtime = runtime();
    let targets = vec![
        argo_target("Application", "applications", &["get", "list", "patch"]),
        argo_target(
            "ApplicationSet",
            "applicationsets",
            &["get", "list", "patch"],
        ),
    ];
    let fetched = runtime.block_on(async {
        k10s_data::argo::fetch_inventory(&script.client(), &targets, None).await
    });
    let Fetched::Ok(inventory) = fetched else {
        panic!("the listing must resolve: {fetched:?}");
    };
    assert!(inventory.served);
    assert!(inventory.patchable);
    assert_eq!(inventory.applications.len(), 1);
    assert_eq!(inventory.applications[0].name, "guestbook");
    assert_eq!(inventory.applications[0].sync, "Synced");
    assert_eq!(inventory.applications[0].drift.live_revision, "abc123");

    let lists = script.requests_for("/apis/argoproj.io/v1alpha1/applications?");
    assert_eq!(lists.len(), 2, "two pages, both asked for: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );

    let refreshed = runtime.block_on(async {
        k10s_data::argo::refresh(
            &script.client(),
            &targets,
            "argocd",
            "guestbook",
            k10s_data::argo::Refresh::Hard,
        )
        .await
    });
    assert_eq!(refreshed, Fetched::Ok(()));
    let patches = script
        .seen()
        .into_iter()
        .filter(|seen| seen.method == "PATCH")
        .collect::<Vec<_>>();
    assert_eq!(patches.len(), 1, "{patches:?}");
    assert!(
        patches[0]
            .content_type
            .contains("application/merge-patch+json"),
        "{}",
        patches[0].content_type
    );
    let body: serde_json::Value = serde_json::from_str(&patches[0].body).expect("patch JSON");
    assert_eq!(
        body,
        serde_json::json!({
            "metadata": {
                "annotations": {
                    "argocd.argoproj.io/refresh": "hard"
                }
            }
        })
    );
    drop(runtime);
}

#[test]
fn a_sync_sends_operation_sync_and_a_403_on_the_patch_is_denied() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/argoproj.io/v1alpha1/namespaces/argocd/applications/guestbook",
        200,
        "{}",
    );
    let runtime = runtime();
    let targets = vec![argo_target(
        "Application",
        "applications",
        &["get", "list", "patch"],
    )];
    let synced = runtime.block_on(async {
        k10s_data::argo::sync(&script.client(), &targets, "argocd", "guestbook").await
    });
    assert_eq!(synced, Fetched::Ok(()));
    let body: serde_json::Value = serde_json::from_str(&script.seen()[0].body).expect("patch JSON");
    assert_eq!(body, serde_json::json!({"operation": {"sync": {}}}));
    assert!(body.get("spec").is_none());

    let denied = Script::default();
    denied.route(
        "PATCH",
        "/apis/argoproj.io/v1alpha1/namespaces/argocd/applications/guestbook",
        403,
        status(403, "Forbidden"),
    );
    let fetched = runtime.block_on(async {
        k10s_data::argo::refresh(
            &denied.client(),
            &targets,
            "argocd",
            "guestbook",
            k10s_data::argo::Refresh::Normal,
        )
        .await
    });
    assert_eq!(
        fetched,
        Fetched::Denied {
            what: "argo refresh"
        }
    );
    drop(runtime);
}
