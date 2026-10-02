//! Field extraction, caps, the document, 404/403 classification, and the
//! merge-patch bytes of `kargo.akuity.io/refresh`. A cluster is not required.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

fn stage_json() -> serde_json::Value {
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
                "id": "col-1",
                "items": {
                    "Warehouse/app": {
                        "name": "abc123",
                        "origin": { "kind": "Warehouse", "name": "app" }
                    }
                },
                "verificationHistory": [{ "phase": "Successful" }]
            }],
            "lastPromotion": { "status": { "phase": "Succeeded" } }
        }
    })
}

fn warehouse_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "app", "namespace": "demo" },
        "spec": {
            "subscriptions": [
                { "git": { "repoURL": "https://github.com/example/app" } }
            ]
        },
        "status": {
            "conditions": [{ "type": "Ready", "status": "True" }],
            "lastFreightID": "abc123"
        }
    })
}

fn freight_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "abc123", "namespace": "demo" },
        "origin": { "kind": "Warehouse", "name": "app" },
        "status": {
            "verifiedIn": { "test": { "verifiedAt": "2026-08-14T00:00:00Z" } }
        }
    })
}

fn project_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "demo" },
        "status": {
            "conditions": [{ "type": "Ready", "status": "True" }]
        }
    })
}

fn resource_from(kind: Kind, value: serde_json::Value) -> Resource {
    parse_item(kind, "v1alpha1", value).expect("the fixture is a Kargo object")
}

fn target() -> Resource {
    Resource {
        kind: Kind::Stage,
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
fn a_stage_keeps_health_freight_verified_and_warehouse_origin() {
    let resource = resource_from(Kind::Stage, stage_json());
    assert_eq!(resource.name, "test");
    assert_eq!(resource.namespace, "demo");
    assert_eq!(resource.health, "Healthy");
    assert_eq!(resource.freight, "abc123");
    assert_eq!(resource.verified, "Successful");
    assert_eq!(resource.warehouse, "Warehouse/app");
    assert_eq!(resource.phase, "Succeeded");
}

#[test]
fn a_running_promotion_outranks_the_last_completed_one() {
    let mut value = stage_json();
    value["status"]["currentPromotion"] = serde_json::json!({
        "name": "test.01",
        "status": { "phase": "Running" }
    });
    let resource = resource_from(Kind::Stage, value);
    assert_eq!(
        resource.phase, "Running",
        "while a promotion runs, the phase is not the previous one's Succeeded"
    );
}

#[test]
fn a_current_promotion_without_a_status_still_reads_running() {
    let mut value = stage_json();
    value["status"]["currentPromotion"] = serde_json::json!({ "name": "test.01" });
    let resource = resource_from(Kind::Stage, value);
    assert_eq!(
        resource.phase, "Running",
        "currentPromotion references the currently Running promotion even before its status lands"
    );
}

#[test]
fn a_warehouse_keeps_its_subscription_and_last_freight() {
    let resource = resource_from(Kind::Warehouse, warehouse_json());
    assert_eq!(resource.warehouse, "https://github.com/example/app");
    assert_eq!(resource.freight, "abc123");
    assert_eq!(resource.phase, "True");
}

#[test]
fn a_freight_keeps_origin_and_verified_stages() {
    let resource = resource_from(Kind::Freight, freight_json());
    assert_eq!(resource.warehouse, "Warehouse/app");
    assert_eq!(resource.verified, "test");
}

#[test]
fn a_project_keeps_its_ready_condition() {
    let resource = resource_from(Kind::Project, project_json());
    assert_eq!(resource.name, "demo");
    assert!(resource.namespace.is_empty());
    assert_eq!(resource.phase, "True");
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::Stage, "v1alpha1", serde_json::json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = serde_json::json!({
        "metadata": { "name": huge, "namespace": "demo" },
        "spec": {
            "requestedFreight": [{ "origin": { "kind": "Warehouse", "name": huge } }]
        },
        "status": {
            "health": { "status": huge },
            "freightSummary": huge
        }
    });
    let resource = resource_from(Kind::Stage, value);
    for field in [
        &resource.name,
        &resource.health,
        &resource.freight,
        &resource.warehouse,
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
        "a 403 is Denied, never an empty inventory that looks like Kargo is absent"
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
fn rfc3339_is_utc_from_the_unix_epoch() {
    assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(1_704_067_200, 0), "2024-01-01T00:00:00Z");
}

#[test]
fn refresh_is_the_annotation_kargo_documents() {
    let at = "2026-08-15T02:00:00Z";
    let request = refresh_request(&target(), at).expect("a patch builds");
    assert_eq!(request.method(), http::Method::PATCH);
    assert_eq!(
        header(&request, http::header::CONTENT_TYPE),
        "application/merge-patch+json"
    );
    assert!(
        request
            .uri()
            .path()
            .contains("/apis/kargo.akuity.io/v1alpha1/namespaces/demo/stages/test"),
        "the object path Kargo already serves: {}",
        request.uri()
    );
    let body: serde_json::Value = serde_json::from_slice(request.body()).expect("json");
    assert_eq!(
        body.pointer("/metadata/annotations/kargo.akuity.io~1refresh")
            .and_then(serde_json::Value::as_str),
        Some(at)
    );
    let encoded = std::str::from_utf8(request.body()).expect("utf-8");
    assert!(
        !encoded.contains("promote"),
        "refresh is not a promotion API: {encoded}"
    );
}

#[test]
fn an_unserved_kargo_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "a 404 group is absence, not an empty list"
    );
}

#[test]
fn a_denied_kargo_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        stages: KindSet::Denied,
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
    assert!(text.contains("Stage"), "{text}");
}

#[test]
fn a_served_kargo_fixture_is_one_row_per_object() {
    let stage = resource_from(Kind::Stage, stage_json());
    let page = table_page(&Inventory {
        stages: KindSet::Served {
            items: vec![stage],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "test");
    assert_eq!(page.rows[0].cells[0], "Stage");
    assert_eq!(page.rows[0].cells[4], "Healthy");
    assert_eq!(page.rows[0].cells[5], "abc123");
}

#[test]
fn a_missing_kargo_group_renders_as_not_installed_rather_than_empty() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served());
    assert_eq!(lines[0], "Kargo is not served by this cluster");
    let text = lines.join("\n");
    assert!(text.contains("Stage"), "{text}");
    assert!(
        text.contains("kargo.akuity.io/refresh"),
        "an empty answer names the refresh the controller honours: {text}"
    );
}

#[test]
fn a_history_renders_health_freight_and_warehouse() {
    let stage = resource_from(Kind::Stage, stage_json());
    let lines = render(&Inventory {
        stages: KindSet::Served {
            items: vec![stage],
            truncated: true,
            unreadable: 1,
        },
        warehouses: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.starts_with("1 Kargo object"), "{text}");
    assert!(text.contains("demo/test"), "{text}");
    assert!(
        text.contains(
            "Stage  Succeeded  Healthy  freight abc123  verified Successful  Warehouse/app"
        ),
        "{text}"
    );
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
    assert!(
        text.contains("kargo warehouses: access denied for this account"),
        "a 403 is a labelled denial, not an absent kind: {text}"
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
        refresh(&client, &target(), false).await,
        Fetched::Ok(Confirm::Needed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_on_freight_or_project_fails_and_never_reaches_the_wire() {
    let client = kube::Client::new(PanicOnCall, "default");
    for kind in [Kind::Freight, Kind::Project] {
        let fetched = refresh(&client, &Resource { kind, ..target() }, true).await;
        let Fetched::Failed { what, why } = fetched else {
            panic!(
                "Kargo ignores the annotation on {kind:?}, so Sent is a false success: {fetched:?}"
            );
        };
        assert_eq!(what, kind.what());
        assert!(
            why.contains(REFRESH_ANNOTATION),
            "the failure names the annotation that is not honoured: {why}"
        );
    }
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

fn kargo_group() -> String {
    r#"{"kind":"APIGroup","name":"kargo.akuity.io",
        "versions":[{"groupVersion":"kargo.akuity.io/v1alpha1","version":"v1alpha1"}],
        "preferredVersion":{"groupVersion":"kargo.akuity.io/v1alpha1","version":"v1alpha1"}}"#
        .to_string()
}

fn list(kind: &str, items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": format!("{kind}List"),
        "apiVersion": "kargo.akuity.io/v1alpha1",
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
    assert!(matches!(inventory.stages, KindSet::NotServed));
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("stages")),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_the_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/kargo.akuity.io", 403, STATUS_403);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied on that kind, not a whole-fetch failure");
    };
    assert!(matches!(inventory.stages, KindSet::Denied));
    assert!(
        inventory.stages.served(),
        "403 is Denied, not served: false"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_extracts_inventory_fields_and_follows_a_continue_token() {
    let script = Script::default();
    script.route("GET", "/apis/kargo.akuity.io", 200, kargo_group());
    script.route(
        "GET",
        "/apis/kargo.akuity.io/v1alpha1/stages?",
        200,
        serde_json::json!({
            "kind": "StageList",
            "metadata": { "continue": "page-2" },
            "items": [stage_json()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/kargo.akuity.io/v1alpha1/stages?",
        200,
        list(
            "Stage",
            &[serde_json::json!({
                "metadata": { "name": "prod", "namespace": "demo" },
                "status": { "health": { "status": "Unhealthy" } }
            })],
        ),
    );
    script.route(
        "GET",
        "/apis/kargo.akuity.io/v1alpha1/warehouses?",
        200,
        list("Warehouse", &[warehouse_json()]),
    );
    script.route(
        "GET",
        "/apis/kargo.akuity.io/v1alpha1/freights?",
        200,
        list("Freight", &[freight_json()]),
    );
    script.route(
        "GET",
        "/apis/kargo.akuity.io/v1alpha1/projects?",
        200,
        list("Project", &[project_json()]),
    );

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    let stages = inventory.stages.items();
    assert_eq!(
        stages
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["test", "prod"]
    );
    assert_eq!(stages[0].freight, "abc123");
    assert_eq!(
        inventory.warehouses.items()[0].warehouse,
        "https://github.com/example/app"
    );
    assert_eq!(inventory.freight.items()[0].verified, "test");
    assert_eq!(inventory.projects.items()[0].name, "demo");

    let lists: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("/stages"))
        .collect();
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_sends_the_merge_patch_bytes_only_when_confirmed() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/kargo.akuity.io/v1alpha1/namespaces/demo/stages/test",
        200,
        stage_json().to_string(),
    );

    let client = script.client();
    assert_eq!(
        refresh(&client, &target(), false).await,
        Fetched::Ok(Confirm::Needed)
    );
    assert_eq!(
        refresh_at(&client, &target(), true, "2026-08-15T02:00:00Z").await,
        Fetched::Ok(Confirm::Sent)
    );

    let seen = script.seen();
    assert_eq!(seen.len(), 1, "confirm=false must not PATCH: {seen:?}");
    assert_eq!(seen[0].method, "PATCH");
    assert_eq!(seen[0].content_type, "application/merge-patch+json");
    let body: serde_json::Value = serde_json::from_str(&seen[0].body).expect("json");
    assert_eq!(
        body.pointer("/metadata/annotations/kargo.akuity.io~1refresh"),
        Some(&serde_json::json!("2026-08-15T02:00:00Z"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forbidden_refresh_is_denied() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/kargo.akuity.io/v1alpha1/namespaces/demo/stages/test",
        403,
        STATUS_403,
    );
    assert_eq!(
        refresh(&script.client(), &target(), true).await,
        Fetched::Denied {
            what: "kargo stages"
        }
    );
}
