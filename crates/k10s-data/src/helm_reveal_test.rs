//! Revealing one revision's values and manifest into scratch buffers, and
//! the inventory type that still has nowhere to put them.

use super::*;
use std::io::Write;

use base64::Engine;
use k10s_core::{Catalog, KindId};
use kube::discovery::{ApiCapabilities, ApiResource, Scope};

use crate::helm::decode;

fn release_json(name: &str, revision: u32, config: &str, defaults: &str, manifest: &str) -> String {
    format!(
        r#"{{"name":"{name}","namespace":"prod","version":{revision},
               "info":{{"first_deployed":"2026-07-01T09:00:00Z",
                        "last_deployed":"2026-08-01T10:22:31Z",
                        "status":"deployed","description":"Upgrade complete",
                        "notes":"NOTES.txt says SUPERSECRET-NOTES"}},
               "chart":{{"metadata":{{"name":"ingress-nginx","version":"4.11.3",
                                     "appVersion":"1.11.3"}},
                         "values":{defaults}}},
               "config":{config},
               "manifest":{manifest},
               "hooks":[{{"manifest":"SUPERSECRET-HOOK"}}]}}"#
    )
}

fn secret_fixture() -> String {
    let manifest = serde_json::to_string(
        "---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ingress\n  namespace: prod\ndata:\n  token: SUPERSECRET-MANIFEST\n",
    )
    .unwrap();
    release_json(
        "ingress-nginx",
        4,
        r#"{"adminPassword":"SUPERSECRET-USER","replicas":1}"#,
        r#"{"password":"SUPERSECRET-DEFAULT","replicas":2}"#,
        &manifest,
    )
}

fn gzipped(json: &str) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(json.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

fn as_the_api_server_sends_it(json: &str) -> String {
    let engine = base64::engine::general_purpose::STANDARD;
    engine.encode(engine.encode(gzipped(json)))
}

fn target(
    group: &str,
    version: &str,
    kind: &str,
    plural: &str,
    namespaced: bool,
    patchable: bool,
) -> KindTarget {
    let mut catalog = Catalog::new();
    let mut operations = vec!["get".into(), "list".into()];
    if patchable {
        operations.push("patch".into());
    }
    crate::discover::intern(
        &mut catalog,
        ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: if group.is_empty() {
                version.to_string()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.to_string(),
            plural: plural.to_string(),
        },
        &ApiCapabilities {
            scope: if namespaced {
                Scope::Namespaced
            } else {
                Scope::Cluster
            },
            subresources: Vec::new(),
            operations,
        },
    )
}

#[test]
fn revealing_a_revision_does_not_put_values_on_a_helm_revision() {
    let json = secret_fixture();
    assert!(
        json.contains("SUPERSECRET"),
        "the fixture has to contain what must not land on the inventory type"
    );
    let encoded = as_the_api_server_sends_it(&json);
    let stored = decode(&encoded).expect("inventory decode");
    let inventory = format!("{:?}", stored.revision);
    assert!(
        !inventory.contains("SUPERSECRET"),
        "helm::Revision has nowhere to put values, so they cannot appear on it: {inventory}"
    );
    assert!(
        !inventory.contains("manifest"),
        "and no field named for the rendered documents: {inventory}"
    );

    let revealed = reveal_payload(&encoded).expect("reveal");
    let config = revealed.config().as_str().expect("utf-8");
    let defaults = revealed.chart_values().as_str().expect("utf-8");
    let manifest = revealed.manifest().as_str().expect("utf-8");
    assert!(
        config.contains("SUPERSECRET-USER"),
        "user values live in the scratch buffer: {config}"
    );
    assert!(
        defaults.contains("SUPERSECRET-DEFAULT"),
        "chart defaults live in the other scratch buffer: {defaults}"
    );
    assert!(
        manifest.contains("SUPERSECRET-MANIFEST"),
        "and the rendered documents in the third: {manifest}"
    );
    assert!(
        !config.contains("SUPERSECRET-HOOK")
            && !defaults.contains("SUPERSECRET-HOOK")
            && !manifest.contains("SUPERSECRET-HOOK"),
        "hooks are not a revealed field"
    );
    assert!(
        !config.contains("SUPERSECRET-NOTES")
            && !defaults.contains("SUPERSECRET-NOTES")
            && !manifest.contains("SUPERSECRET-NOTES"),
        "notes are not a revealed field"
    );
    assert_eq!(revealed.name, "ingress-nginx");
    assert_eq!(revealed.namespace, "prod");
    assert_eq!(revealed.revision, 4);
}

#[test]
fn a_compression_bomb_is_refused_on_the_reveal_path_too() {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let block = vec![b'a'; 1 << 20];
    for _ in 0..(MAX_PAYLOAD_BYTES / block.len()) + 2 {
        encoder.write_all(&block).unwrap();
    }
    let bomb = encoder.finish().unwrap();
    assert!(
        bomb.len() < 64 << 10,
        "the compressed side is small: {} bytes",
        bomb.len()
    );
    let engine = base64::engine::general_purpose::STANDARD;
    assert_eq!(
        reveal_payload(&engine.encode(bomb)).err(),
        Some("this release's payload is larger than this view decodes")
    );
}

#[test]
fn a_values_diff_is_owned_text_and_not_an_inventory_field() {
    let manifest =
        serde_json::to_string("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n").unwrap();
    let older = reveal_payload(&as_the_api_server_sends_it(&release_json(
        "app",
        3,
        r#"{"replicas":1,"password":"SUPERSECRET-USER"}"#,
        r#"{}"#,
        &manifest,
    )))
    .expect("older");
    let newer = reveal_payload(&as_the_api_server_sends_it(&release_json(
        "app",
        4,
        r#"{"replicas":2,"password":"SUPERSECRET-USER"}"#,
        r#"{}"#,
        &manifest,
    )))
    .expect("newer");
    let text = diff_values(&older, &newer);
    assert!(text.contains("--- user values, revision 3"), "{text}");
    assert!(text.contains("+++ user values, revision 4"), "{text}");
    assert!(text.contains("-  \"replicas\": 1"), "{text}");
    assert!(text.contains("+  \"replicas\": 2"), "{text}");

    let stored = decode(&as_the_api_server_sends_it(&release_json(
        "app",
        4,
        r#"{"replicas":2,"password":"SUPERSECRET-USER"}"#,
        r#"{}"#,
        &manifest,
    )))
    .expect("inventory");
    let inventory = format!("{:?}", stored.revision);
    assert!(
        !inventory.contains("SUPERSECRET"),
        "the diff string is owned and separate from helm::Revision: {inventory}"
    );
    assert_eq!(
        diff_values(&newer, &newer),
        "the user values of revision 4 and revision 4 are identical"
    );
}

#[test]
fn rollback_is_labelled_as_not_helm_rollback_and_does_not_plan_hooks() {
    let manifest = "---\n# Source: chart/templates/cm.yaml\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ingress\n  namespace: prod\ndata:\n  token: SUPERSECRET-MANIFEST\n---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: ingress\nspec:\n  replicas: 1\n";
    let encoded = as_the_api_server_sends_it(&release_json(
        "ingress-nginx",
        3,
        r#"{}"#,
        r#"{}"#,
        &serde_json::to_string(manifest).unwrap(),
    ));
    let revealed = reveal_payload(&encoded).expect("reveal");
    assert!(
        !revealed
            .manifest()
            .as_str()
            .unwrap()
            .contains("SUPERSECRET-HOOK"),
        "hooks stay out of the manifest that rollback would apply"
    );

    let targets = vec![
        target("", "v1", "ConfigMap", "configmaps", true, true),
        target("apps", "v1", "Deployment", "deployments", true, true),
    ];
    let planned = plan_rollback(&targets, "prod", revealed.manifest().as_str().unwrap());
    assert_eq!(planned.len(), 2, "two stored documents, no hook document");
    let Planned::Apply(cm) = &planned[0] else {
        panic!("the ConfigMap is applyable: {}", planned[0].skip_why());
    };
    assert_eq!(cm.name, "ingress");
    assert_eq!(cm.kind, KindId::CONFIG_MAP);
    assert_eq!(cm.namespace.as_deref(), Some("prod"));
    assert!(!cm.dry_run);
    assert!(!cm.force);
    assert!(
        cm.yaml.contains("SUPERSECRET-MANIFEST"),
        "the stored document is what is applied"
    );
    let Planned::Apply(deploy) = &planned[1] else {
        panic!("the Deployment is applyable");
    };
    assert_eq!(deploy.name, "ingress");
    assert_eq!(deploy.kind, KindId::DEPLOYMENT);
    assert_eq!(
        deploy.namespace.as_deref(),
        Some("prod"),
        "a namespaced document without metadata.namespace takes the release's"
    );

    let report = RollbackReport::wrap(Vec::new());
    assert_eq!(report.note, "not helm rollback (hooks will not run)");
    assert_eq!(apply::FIELD_MANAGER, "k10s");
}

#[test]
fn a_kind_the_cluster_does_not_serve_is_skipped_rather_than_applied() {
    let docs =
        split_manifest("apiVersion: example.com/v1\nkind: Widget\nmetadata:\n  name: extra\n");
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].kind, "Widget");
    assert_eq!(docs[0].name, "extra");

    let planned = plan_rollback(
        &[target("", "v1", "ConfigMap", "configmaps", true, true)],
        "prod",
        "apiVersion: example.com/v1\nkind: Widget\nmetadata:\n  name: extra\n",
    );
    let Planned::Skip { name, kind, why } = &planned[0] else {
        panic!("an unknown kind is not applied");
    };
    assert_eq!(name, "extra");
    assert_eq!(kind, "Widget");
    assert!(why.contains("not served"), "{why}");
}

#[test]
fn helm_template_is_the_binary_on_path_or_a_labelled_absence() {
    assert_eq!(find_on_path("helm", ""), None);
    match helm_binary() {
        HelmBinary::Ok(path) => {
            assert!(
                path.ends_with("helm"),
                "a present binary is named, not executed: {path:?}"
            );
        }
        HelmBinary::Absent { why } => {
            assert_eq!(why, "helm binary not on PATH");
        }
    }
}

impl Planned {
    fn skip_why(&self) -> String {
        match self {
            Planned::Skip { why, .. } => why.clone(),
            Planned::Apply(_) => "applied".to_string(),
        }
    }
}
