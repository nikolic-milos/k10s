//! Parsing Application / ApplicationSet fixture JSON into an inventory, the
//! field cap, the 404/403 classification, and the exact merge-patch bodies
//! Argo already honours.

use super::*;
use crate::discover;
use kube::discovery::{ApiCapabilities, ApiResource, Scope};
use serde_json::json;

fn application_json() -> serde_json::Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Application",
        "metadata": {"name": "guestbook", "namespace": "argocd"},
        "spec": {
            "project": "default",
            "source": {
                "repoURL": "https://github.com/argoproj/argocd-example-apps.git",
                "targetRevision": "HEAD",
                "path": "guestbook"
            },
            "destination": {
                "server": "https://kubernetes.default.svc",
                "namespace": "guestbook"
            }
        },
        "status": {
            "sync": {
                "status": "OutOfSync",
                "revision": "a1b2c3d",
                "comparedTo": {
                    "source": {
                        "repoURL": "https://github.com/argoproj/argocd-example-apps.git",
                        "targetRevision": "HEAD"
                    },
                    "destination": {
                        "server": "https://kubernetes.default.svc",
                        "namespace": "guestbook"
                    }
                }
            },
            "health": {"status": "Degraded"},
            "resources": [
                {
                    "group": "apps",
                    "version": "v1",
                    "kind": "Deployment",
                    "namespace": "guestbook",
                    "name": "guestbook-ui",
                    "status": "OutOfSync",
                    "health": {"status": "Degraded"}
                },
                {
                    "group": "",
                    "version": "v1",
                    "kind": "Service",
                    "namespace": "guestbook",
                    "name": "guestbook-ui",
                    "status": "Synced",
                    "health": {"status": "Healthy"}
                }
            ]
        }
    })
}

fn applicationset_json() -> serde_json::Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "ApplicationSet",
        "metadata": {"name": "guestbook-set", "namespace": "argocd"},
        "spec": {
            "template": {
                "spec": {
                    "source": {
                        "repoURL": "https://github.com/argoproj/argocd-example-apps.git",
                        "targetRevision": "stable",
                        "path": "guestbook"
                    },
                    "destination": {
                        "name": "in-cluster",
                        "namespace": "guestbook"
                    }
                }
            }
        }
    })
}

fn target(kind: &str, plural: &str, ops: &[&str]) -> KindTarget {
    let mut catalog = k10s_core::Catalog::new();
    discover::intern(
        &mut catalog,
        ApiResource {
            group: GROUP.to_string(),
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

fn api_error(code: u16) -> kube::Error {
    kube::Error::Api(Box::new(kube::core::Status {
        code,
        reason: "Failure".to_string(),
        message: "no".to_string(),
        ..Default::default()
    }))
}

#[test]
fn an_application_fixture_keeps_dest_sync_health_source_and_resource_refs() {
    let app = application_from_value(application_json()).expect("the fixture is an Application");
    assert_eq!(app.name, "guestbook");
    assert_eq!(app.namespace, "argocd");
    assert_eq!(app.destination.namespace, "guestbook");
    assert_eq!(app.destination.server, "https://kubernetes.default.svc");
    assert_eq!(app.sync, "OutOfSync");
    assert_eq!(app.health, "Degraded");
    assert_eq!(app.sources.len(), 1);
    assert_eq!(
        app.sources[0].repo,
        "https://github.com/argoproj/argocd-example-apps.git"
    );
    assert_eq!(app.sources[0].revision, "HEAD");
    assert_eq!(app.sources[0].path, "guestbook");
    assert_eq!(app.resources.len(), 2);
    assert_eq!(app.resources[0].kind, "Deployment");
    assert_eq!(app.resources[0].name, "guestbook-ui");
    assert_eq!(app.resources[0].sync, "OutOfSync");
    assert_eq!(app.resources[1].kind, "Service");
    assert_eq!(app.resources[1].health, "Healthy");
}

#[test]
fn drift_exposes_the_refs_the_cr_already_has_and_does_not_invent_a_three_way() {
    let app = application_from_value(application_json()).unwrap();
    assert_eq!(
        app.drift.desired.repo,
        "https://github.com/argoproj/argocd-example-apps.git"
    );
    assert_eq!(app.drift.desired.revision, "HEAD");
    assert_eq!(
        app.drift.compared.repo,
        "https://github.com/argoproj/argocd-example-apps.git"
    );
    assert_eq!(app.drift.compared.revision, "HEAD");
    assert_eq!(app.drift.live_revision, "a1b2c3d");
    let rendered = format!("{:?}", app.drift);
    assert!(
        !rendered.contains("last-applied"),
        "three-way lives in k10s-edit, not here: {rendered}"
    );
}

#[test]
fn an_applicationset_fixture_reads_dest_and_source_from_the_template() {
    let set =
        applicationset_from_value(applicationset_json()).expect("the fixture is an ApplicationSet");
    assert_eq!(set.name, "guestbook-set");
    assert_eq!(set.namespace, "argocd");
    assert_eq!(set.destination.name, "in-cluster");
    assert_eq!(set.destination.namespace, "guestbook");
    assert_eq!(set.sources[0].revision, "stable");
}

#[test]
fn multi_source_applications_keep_every_repo() {
    let app = application_from_value(json!({
        "metadata": {"name": "multi", "namespace": "argocd"},
        "spec": {
            "sources": [
                {"repoURL": "https://git.example/app", "targetRevision": "main"},
                {"repoURL": "https://git.example/chart", "chart": "web", "targetRevision": "1.2.3"}
            ],
            "destination": {"namespace": "prod"}
        }
    }))
    .unwrap();
    assert_eq!(app.sources.len(), 2);
    assert_eq!(app.sources[1].chart, "web");
    assert_eq!(app.drift.desired.repo, "https://git.example/app");
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 10);
    let app = application_from_value(json!({
        "metadata": {"name": huge, "namespace": "argocd"},
        "spec": {
            "source": {"repoURL": huge, "targetRevision": huge},
            "destination": {"namespace": huge, "server": huge}
        },
        "status": {
            "sync": {"status": huge, "revision": huge},
            "health": {"status": huge},
            "resources": [{"kind": huge, "name": huge, "status": huge}]
        }
    }))
    .unwrap();
    for field in [
        &app.name,
        &app.sync,
        &app.health,
        &app.sources[0].repo,
        &app.destination.namespace,
        &app.drift.live_revision,
        &app.resources[0].kind,
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
fn a_page_larger_than_eight_mebibytes_is_refused() {
    let huge = "x".repeat(MAX_PAGE_BYTES + 1);
    match parse_page(&huge) {
        Err(PageError::TooLarge) => {}
        other => panic!("an oversize page must be refused: {other:?}"),
    }
}

#[test]
fn a_list_page_of_fixture_json_yields_the_items() {
    let page = json!({
        "kind": "ApplicationList",
        "apiVersion": "argoproj.io/v1alpha1",
        "metadata": {},
        "items": [application_json(), {"metadata": {"name": ""}}]
    });
    let items = parse_page(&page.to_string()).expect("a list page parses");
    let (apps, truncated) = applications_from_items(items);
    assert_eq!(apps.len(), 1, "an item with no name is not an Application");
    assert!(!truncated);
    assert_eq!(apps[0].name, "guestbook");
}

#[test]
fn taking_more_than_two_thousand_apps_sets_truncated() {
    let items = (0..=MAX_APPS).map(|i| {
        json!({
            "metadata": {"name": format!("app-{i}"), "namespace": "argocd"},
            "spec": {"destination": {"namespace": "prod"}}
        })
    });
    let (apps, truncated) = applications_from_items(items);
    assert!(truncated, "the 2001st app is a cap, not a silent drop");
    assert_eq!(apps.len(), MAX_APPS);
}

#[test]
fn a_cluster_without_the_argoproj_kinds_is_unserved() {
    assert!(argo_kinds(&[]).is_none());
    let inventory = Inventory::unserved();
    assert!(!inventory.served);
    assert!(
        render(&inventory).is_empty(),
        "an unserved inventory stays invisible"
    );
}

#[test]
fn discovery_of_applications_alone_is_enough_to_serve() {
    let apps = target(APPLICATION, "applications", &["get", "list", "patch"]);
    let targets = [apps];
    let kinds = argo_kinds(&targets).expect("Application is enough");
    assert!(kinds.applications.is_some());
    assert!(kinds.application_sets.is_none());
}

#[test]
fn a_missing_kind_or_a_kind_without_patch_refuses_the_action_before_the_wire() {
    match gate_action(&[], "argo refresh") {
        Err(Fetched::Failed { why, .. }) => {
            assert!(why.contains("not served"), "{why}");
        }
        other => panic!("no Application kind is a labelled failure: {other:?}"),
    }
    let listed = target(APPLICATION, "applications", &["get", "list"]);
    match gate_action(&[listed], "argo sync") {
        Err(Fetched::Failed { why, .. }) => {
            assert!(why.contains("without a patch verb"), "{why}");
        }
        other => panic!("no patch verb is not a 403: {other:?}"),
    }
    let patchable = target(APPLICATION, "applications", &["get", "list", "patch"]);
    assert!(gate_action(&[patchable], "argo refresh").is_ok());
}

#[test]
fn a_list_404_is_absence_and_a_list_403_is_denied() {
    assert!(matches!(list_miss(&api_error(404)), Some(ListMiss::Absent)));
    assert!(matches!(list_miss(&api_error(403)), Some(ListMiss::Denied)));
    assert!(matches!(list_miss(&api_error(401)), Some(ListMiss::Denied)));
    assert!(list_miss(&api_error(500)).is_none());
}

#[test]
fn a_refresh_patch_is_the_annotation_argo_honours() {
    assert_eq!(
        refresh_patch(Refresh::Hard),
        json!({
            "metadata": {
                "annotations": {
                    "argocd.argoproj.io/refresh": "hard"
                }
            }
        })
    );
    assert_eq!(
        refresh_patch(Refresh::Normal)["metadata"]["annotations"][REFRESH_ANNOTATION],
        "normal"
    );
}

#[test]
fn a_sync_patch_is_operation_sync_on_the_application_not_under_spec() {
    let patch = sync_patch();
    assert_eq!(patch, json!({"operation": {"sync": {}}}));
    assert!(
        patch.get("spec").is_none(),
        "the CRD stores the requested operation on the Application, not spec.operation: {patch}"
    );
}

#[test]
fn an_unserved_inventory_has_no_table_so_the_pane_stays_invisible() {
    assert!(
        table_page(&Inventory::unserved()).is_none(),
        "served=false is absence, not an empty list"
    );
    let empty = table_page(&Inventory {
        served: true,
        ..Inventory::default()
    })
    .expect("CRDs with no Applications are a served empty table");
    assert!(empty.rows.is_empty());
}

#[test]
fn the_table_lists_applications_from_the_fixture() {
    let app = application_from_value(application_json()).unwrap();
    let set = applicationset_from_value(applicationset_json()).unwrap();
    let page = table_page(&Inventory {
        applications: vec![app],
        application_sets: vec![set],
        truncated: true,
        served: true,
        patchable: true,
    })
    .expect("served inventory is a table");
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0].cells[0], "Application");
    assert_eq!(page.rows[0].name, "guestbook");
    assert_eq!(page.rows[0].cells[3], "OutOfSync");
    assert_eq!(page.rows[0].cells[4], "Degraded");
    assert_eq!(page.rows[1].cells[0], "ApplicationSet");
    assert!(page.truncated);
}

#[test]
fn an_unserved_inventory_renders_nothing_and_a_served_empty_one_says_what_it_looked_at() {
    assert!(render(&Inventory::unserved()).is_empty());
    let lines = render(&Inventory {
        served: true,
        ..Inventory::default()
    });
    assert_eq!(lines[0], "no Argo CD Applications are in this cluster");
    let text = lines.join("\n");
    assert!(text.contains("Application and ApplicationSet"), "{text}");
    assert!(text.contains("no Argo API token"), "{text}");
}

#[test]
fn a_history_renders_sync_health_source_live_rev_and_resource_refs() {
    let app = application_from_value(application_json()).unwrap();
    let set = applicationset_from_value(applicationset_json()).unwrap();
    let lines = render(&Inventory {
        applications: vec![app],
        application_sets: vec![set],
        truncated: true,
        served: true,
        patchable: true,
    });
    let text = lines.join("\n");
    assert!(
        text.starts_with("1 application, 1 application set"),
        "{text}"
    );
    assert!(
        text.contains("argocd/guestbook  OutOfSync  Degraded"),
        "{text}"
    );
    assert!(
        text.contains("https://github.com/argoproj/argocd-example-apps.git@HEAD"),
        "{text}"
    );
    assert!(text.contains("dest guestbook"), "{text}");
    assert!(text.contains("live a1b2c3d"), "{text}");
    assert!(
        text.contains("apps/Deployment/guestbook/guestbook-ui  OutOfSync  Degraded"),
        "{text}"
    );
    assert!(text.contains("argocd/guestbook-set"), "{text}");
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
}
