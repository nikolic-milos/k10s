//! Field extraction, caps, the document, 404/403 classification, and the
//! apply bytes of a confirmed Velero Backup CR. A cluster is not required.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

fn backup_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "nightly", "namespace": "velero" },
        "spec": {
            "includedNamespaces": ["prod", "staging"],
            "storageLocation": "default"
        },
        "status": {
            "phase": "Completed",
            "warnings": 2,
            "errors": 0,
            "startTimestamp": "2026-08-14T01:00:00Z",
            "completionTimestamp": "2026-08-14T01:04:00Z"
        }
    })
}

fn restore_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "nightly-restore", "namespace": "velero" },
        "spec": {
            "backupName": "nightly",
            "includedNamespaces": ["prod"]
        },
        "status": { "phase": "Completed", "warnings": 0, "errors": 1 }
    })
}

fn schedule_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "nightly", "namespace": "velero" },
        "spec": {
            "schedule": "0 1 * * *",
            "template": {
                "includedNamespaces": ["prod"],
                "storageLocation": "default"
            }
        },
        "status": { "phase": "Enabled" }
    })
}

fn bsl_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "default", "namespace": "velero" },
        "spec": {
            "provider": "aws",
            "objectStorage": { "bucket": "cluster-backups" },
            "credential": { "name": "cloud-credentials", "key": "cloud" },
            "config": { "accessKey": "AKIA-PLANTED-NOT-A-SECRET" }
        },
        "status": { "phase": "Available" }
    })
}

fn resource_from(kind: Kind, value: serde_json::Value) -> Resource {
    parse_item(kind, "v1", value).expect("the fixture is a Velero object")
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
fn a_backup_keeps_phase_counts_timestamps_storage_and_namespaces() {
    let resource = resource_from(Kind::Backup, backup_json());
    assert_eq!(resource.name, "nightly");
    assert_eq!(resource.namespace, "velero");
    assert_eq!(resource.phase, "Completed");
    assert_eq!(resource.warnings, 2);
    assert_eq!(resource.errors, 0);
    assert_eq!(resource.started, "2026-08-14T01:00:00Z");
    assert_eq!(resource.completed, "2026-08-14T01:04:00Z");
    assert_eq!(resource.storage_location, "default");
    assert_eq!(resource.included_namespaces, "prod, staging");
}

#[test]
fn a_restore_keeps_the_backup_name_the_cr_already_has() {
    let resource = resource_from(Kind::Restore, restore_json());
    assert_eq!(resource.backup_name, "nightly");
    assert_eq!(resource.errors, 1);
    assert_eq!(resource.included_namespaces, "prod");
}

#[test]
fn a_restore_has_no_storage_column_because_restorespec_defines_none() {
    let mut value = restore_json();
    value["spec"]["storageLocation"] = serde_json::json!("planted");
    let resource = resource_from(Kind::Restore, value);
    assert!(
        resource.storage_location.is_empty(),
        "spec.storageLocation is not a RestoreSpec field, so it is never a Restore's storage: {}",
        resource.storage_location
    );
}

#[test]
fn a_schedule_keeps_its_cron_and_template_storage() {
    let resource = resource_from(Kind::Schedule, schedule_json());
    assert_eq!(resource.schedule, "0 1 * * *");
    assert_eq!(resource.storage_location, "default");
    assert_eq!(resource.included_namespaces, "prod");
    assert_eq!(resource.phase, "Enabled");
}

#[test]
fn a_bsl_keeps_provider_bucket_and_credential_secret_name_only() {
    let resource = resource_from(Kind::BackupStorageLocation, bsl_json());
    assert_eq!(resource.storage_location, "aws/cluster-backups");
    assert_eq!(resource.credential_secret, "cloud-credentials");
    assert_eq!(resource.phase, "Available");
    let shown = format!("{resource:?}");
    assert!(
        !shown.contains("AKIA-PLANTED-NOT-A-SECRET"),
        "BSL config credentials must not be carried: {shown}"
    );
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::Backup, "v1", serde_json::json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = serde_json::json!({
        "metadata": { "name": huge, "namespace": "velero" },
        "spec": {
            "includedNamespaces": [huge],
            "storageLocation": huge
        },
        "status": { "phase": huge, "startTimestamp": huge }
    });
    let resource = resource_from(Kind::Backup, value);
    for field in [
        &resource.name,
        &resource.phase,
        &resource.storage_location,
        &resource.included_namespaces,
        &resource.started,
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
fn a_missing_group_is_invisible_and_a_forbidden_one_is_denied() {
    assert!(matches!(
        after_group(&api_error(404)),
        GroupAnswer::NotServed
    ));
    assert!(
        matches!(after_group(&api_error(403)), GroupAnswer::Denied),
        "a 403 is Denied, never an empty inventory that looks like Velero is absent"
    );
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

fn header(request: &http::Request<Vec<u8>>, name: http::header::HeaderName) -> &str {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

#[test]
fn a_confirmed_backup_is_an_apply_of_a_velero_backup_cr() {
    let request = backup_apply_request(&backup_doc()).expect("an apply builds");
    assert_eq!(request.method(), http::Method::PATCH);
    assert_eq!(
        header(&request, http::header::CONTENT_TYPE),
        "application/apply-patch+yaml"
    );
    assert!(
        request
            .uri()
            .path()
            .contains("/apis/velero.io/v1/namespaces/velero/backups/adhoc"),
        "the object path Velero already serves: {}",
        request.uri()
    );
    assert!(
        request
            .uri()
            .query()
            .is_some_and(|query| query.contains("fieldManager=k10s")),
        "SSA names us: {}",
        request.uri()
    );
    let body: serde_json::Value = serde_json::from_slice(request.body()).expect("json");
    assert_eq!(body["kind"], "Backup");
    assert_eq!(body["apiVersion"], "velero.io/v1");
    assert_eq!(body["metadata"]["name"], "adhoc");
    assert_eq!(body["spec"]["storageLocation"], "default");
    assert_eq!(
        body["spec"]["includedNamespaces"],
        serde_json::json!(["prod"])
    );
    let encoded = std::str::from_utf8(request.body()).expect("utf-8");
    assert!(
        !encoded.contains("velero backup create"),
        "this is a Backup CR, not the Velero CLI: {encoded}"
    );
}

#[test]
fn an_unserved_velero_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "a 404 group is absence, not an empty list"
    );
}

#[test]
fn a_denied_velero_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        backups: KindSet::Denied,
        ..Inventory::default()
    })
    .expect("Denied is served, so the table exists");
    let text = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("access denied for this account"),
        "a 403 stays labelled: {text}"
    );
    assert!(text.contains("Backup"), "{text}");
}

#[test]
fn a_served_velero_fixture_is_one_row_per_object() {
    let backup = resource_from(Kind::Backup, backup_json());
    let page = table_page(&Inventory {
        backups: KindSet::Served {
            items: vec![backup],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "nightly");
    assert_eq!(page.rows[0].cells[0], "Backup");
    assert_eq!(page.rows[0].cells[3], "Completed");
    assert_eq!(page.rows[0].cells[4], "2");
}

#[test]
fn a_missing_velero_group_renders_as_not_installed_rather_than_empty() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served());
    assert_eq!(lines[0], "Velero is not served by this cluster");
    let text = lines.join("\n");
    assert!(text.contains("Backup"), "{text}");
    assert!(
        text.contains("nothing is installed to find them"),
        "an empty answer names the reason it could be wrong: {text}"
    );
    assert!(text.contains("tarball"), "{text}");
}

#[test]
fn a_history_renders_phase_counts_storage_and_schedule() {
    let backup = resource_from(Kind::Backup, backup_json());
    let schedule = resource_from(Kind::Schedule, schedule_json());
    let lines = render(&Inventory {
        backups: KindSet::Served {
            items: vec![backup],
            truncated: true,
            unreadable: 1,
        },
        schedules: KindSet::Served {
            items: vec![schedule],
            truncated: false,
            unreadable: 0,
        },
        restores: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.starts_with("2 Velero objects"), "{text}");
    assert!(text.contains("velero/nightly"), "{text}");
    assert!(
        text.contains("Backup  Completed  2 warnings  0 errors  default  prod, staging"),
        "{text}"
    );
    assert!(text.contains("0 1 * * *"), "{text}");
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
    assert!(
        text.contains("velero restores: access denied for this account"),
        "a 403 is a labelled denial, not an absent kind: {text}"
    );
}

#[test]
fn a_denied_kind_does_not_hide_that_every_readable_object_failed_to_decode() {
    let lines = render(&Inventory {
        backups: KindSet::Served {
            items: vec![],
            truncated: false,
            unreadable: 1,
        },
        restores: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(
        text.contains("failed to decode"),
        "the undecodable count is stated even beside a denial: {text}"
    );
    assert!(
        text.contains("velero restores: access denied for this account"),
        "{text}"
    );
}

struct PanicOnCall;

impl Service<http::Request<Body>> for PanicOnCall {
    type Response = http::Response<Body>;
    type Error = tower::BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<Body>) -> Self::Future {
        panic!("confirm=false must not touch the wire");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirm_false_does_not_touch_the_wire() {
    let client = kube::Client::new(PanicOnCall, "default");
    assert_eq!(
        apply_backup(&client, &backup_doc(), false).await,
        Fetched::Ok(Confirm::Needed)
    );
}

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    content_type: String,
    body: String,
}

struct Route {
    method: &'static str,
    matches: String,
    status: u16,
    body: String,
    used: bool,
}

#[derive(Default)]
struct State {
    routes: Vec<Route>,
    seen: Vec<Seen>,
}

#[derive(Clone, Default)]
struct Script {
    state: Arc<Mutex<State>>,
}

impl Script {
    fn route(
        &self,
        method: &'static str,
        matches: &str,
        status: u16,
        body: impl Into<String>,
    ) -> &Self {
        self.state.lock().expect("script lock").routes.push(Route {
            method,
            matches: matches.to_string(),
            status,
            body: body.into(),
            used: false,
        });
        self
    }

    fn seen(&self) -> Vec<Seen> {
        self.state.lock().expect("script lock").seen.clone()
    }

    fn client(&self) -> kube::Client {
        kube::Client::new(self.clone(), "default")
    }
}

impl Service<http::Request<Body>> for Script {
    type Response = http::Response<Body>;
    type Error = tower::BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let method = req.method().to_string();
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default();
        let content_type = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let (at, answer) = {
            let mut state = self.state.lock().expect("script lock");
            let at = state.seen.len();
            state.seen.push(Seen {
                method: method.clone(),
                path: path.clone(),
                content_type,
                body: String::new(),
            });
            let routable = path.replacen("?&", "?", 1);
            let hit = state.routes.iter_mut().find(|route| {
                !route.used && route.method == method && routable.starts_with(&route.matches)
            });
            let answer = match hit {
                Some(route) => {
                    route.used = true;
                    Some((route.status, route.body.clone()))
                }
                None => Some((
                    404,
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":404,"reason":"NotFound","message":"unscripted"}"#
                        .to_string(),
                )),
            };
            (at, answer)
        };
        let shared = self.state.clone();
        let body = req.into_body();
        Box::pin(async move {
            let read = match http_body_util::BodyExt::collect(body).await {
                Ok(collected) => String::from_utf8_lossy(&collected.to_bytes()).to_string(),
                Err(_) => String::new(),
            };
            if let Some(seen) = shared.lock().expect("script lock").seen.get_mut(at) {
                seen.body = read;
            }
            let (status, response) = answer.expect("every scripted call answers");
            Ok(http::Response::builder()
                .status(status)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(response.into_bytes()))
                .expect("a response"))
        })
    }
}

const STATUS_403: &str = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":403,"reason":"Forbidden","message":"forbidden"}"#;

fn velero_group() -> String {
    r#"{"kind":"APIGroup","name":"velero.io",
        "versions":[{"groupVersion":"velero.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"velero.io/v1","version":"v1"}}"#
        .to_string()
}

fn list(kind: &str, items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": format!("{kind}List"),
        "apiVersion": "velero.io/v1",
        "metadata": {},
        "items": items
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_on_the_group_is_invisible_and_does_not_list() {
    let script = Script::default();
    let fetched = fetch(&script.client(), None).await;
    let Fetched::Ok(inventory) = fetched else {
        panic!("a missing group is not a failure: {fetched:?}");
    };
    assert!(!inventory.served());
    assert!(matches!(inventory.backups, KindSet::NotServed));
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("backups")),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_the_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/velero.io", 403, STATUS_403);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied on that kind, not a whole-fetch failure");
    };
    assert!(matches!(inventory.backups, KindSet::Denied));
    assert!(
        inventory.backups.served(),
        "403 is Denied, not served: false"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_extracts_inventory_fields_and_follows_a_continue_token() {
    let script = Script::default();
    script.route("GET", "/apis/velero.io", 200, velero_group());
    script.route(
        "GET",
        "/apis/velero.io/v1/backups?",
        200,
        serde_json::json!({
            "kind": "BackupList",
            "metadata": { "continue": "page-2" },
            "items": [backup_json()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/velero.io/v1/backups?",
        200,
        list(
            "Backup",
            &[serde_json::json!({
                "metadata": { "name": "weekly", "namespace": "velero" },
                "spec": { "storageLocation": "default" },
                "status": { "phase": "InProgress" }
            })],
        ),
    );
    script.route(
        "GET",
        "/apis/velero.io/v1/schedules?",
        200,
        list("Schedule", &[schedule_json()]),
    );

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    let backups = inventory.backups.items();
    assert_eq!(
        backups
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["nightly", "weekly"]
    );
    assert_eq!(backups[0].warnings, 2);
    assert_eq!(inventory.schedules.items()[0].schedule, "0 1 * * *");

    let lists: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("/backups?"))
        .collect();
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_backup_sends_the_apply_bytes_only_when_confirmed() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/velero.io/v1/namespaces/velero/backups/adhoc",
        200,
        backup_json().to_string(),
    );

    let client = script.client();
    assert_eq!(
        apply_backup(&client, &backup_doc(), false).await,
        Fetched::Ok(Confirm::Needed)
    );
    assert_eq!(
        apply_backup(&client, &backup_doc(), true).await,
        Fetched::Ok(Confirm::Sent)
    );

    let seen = script.seen();
    assert_eq!(seen.len(), 1, "confirm=false must not PATCH: {seen:?}");
    assert_eq!(seen[0].method, "PATCH");
    assert_eq!(seen[0].content_type, "application/apply-patch+yaml");
    let body: serde_json::Value = serde_json::from_str(&seen[0].body).expect("json");
    assert_eq!(body["kind"], "Backup");
    assert_eq!(body["apiVersion"], "velero.io/v1");
    assert_eq!(body["spec"]["storageLocation"], "default");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forbidden_apply_is_denied() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/velero.io/v1/namespaces/velero/backups/adhoc",
        403,
        STATUS_403,
    );
    assert_eq!(
        apply_backup(&script.client(), &backup_doc(), true).await,
        Fetched::Denied {
            what: "velero backups"
        }
    );
}
