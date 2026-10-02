//! Field extraction, caps, the document, 404/403 classification, and the
//! structural exclusion of a planted Postgres password. A cluster is not
//! required.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

const PLANTED_PASSWORD: &str = "planted-s3cret-must-not-leak";

fn cluster_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "app", "namespace": "data" },
        "spec": {
            "instances": 3,
            "imageName": "ghcr.io/cloudnative-pg/postgresql:16.4",
            "superuserSecret": { "name": "app-superuser", "password": PLANTED_PASSWORD },
            "password": PLANTED_PASSWORD
        },
        "status": {
            "instances": 3,
            "readyInstances": 3,
            "currentPrimary": "app-1",
            "phase": "Cluster in healthy state",
            "image": "ghcr.io/cloudnative-pg/postgresql:16.4",
            "pgDataImageInfo": { "image": "ghcr.io/cloudnative-pg/postgresql:16.4", "majorVersion": 16 },
            "password": PLANTED_PASSWORD
        }
    })
}

fn backup_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "app-backup", "namespace": "data" },
        "spec": { "cluster": { "name": "app" }, "method": "barmanObjectStore" },
        "status": { "phase": "completed" }
    })
}

fn scheduled_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "app-nightly", "namespace": "data" },
        "spec": {
            "schedule": "0 0 0 * * *",
            "cluster": { "name": "app" }
        }
    })
}

fn pooler_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "app-rw", "namespace": "data" },
        "spec": {
            "cluster": { "name": "app" },
            "instances": 2,
            "type": "rw"
        },
        "status": { "instances": 2 }
    })
}

fn resource_from(kind: Kind, value: serde_json::Value) -> Resource {
    parse_item(kind, "v1", value).expect("the fixture is a CNPG object")
}

fn assert_no_password(text: &str) {
    assert!(
        !text.contains(PLANTED_PASSWORD),
        "a planted password must not appear: {text}"
    );
}

#[test]
fn a_cluster_keeps_instances_primary_phase_version_and_secret_name() {
    let resource = resource_from(Kind::Cluster, cluster_json());
    assert_eq!(resource.name, "app");
    assert_eq!(resource.namespace, "data");
    assert_eq!(resource.instances, 3);
    assert_eq!(resource.ready_instances, 3);
    assert_eq!(resource.primary, "app-1");
    assert_eq!(resource.phase, "Cluster in healthy state");
    assert_eq!(resource.postgres_version, "16");
    assert_eq!(resource.superuser_secret, "app-superuser");
}

#[test]
fn a_planted_password_is_not_in_debug_table_or_document() {
    let fixture = cluster_json().to_string();
    assert!(
        fixture.contains(PLANTED_PASSWORD),
        "the fixture must actually plant a password or the exclusion is vacuous"
    );
    let resource = resource_from(Kind::Cluster, cluster_json());
    assert_no_password(&format!("{resource:?}"));
    let inventory = Inventory {
        clusters: KindSet::Served {
            items: vec![resource],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    assert_no_password(&format!("{inventory:?}"));
    let page = table_page(&inventory).expect("a served cluster is a table");
    let cells = page
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert_no_password(&cells);
    assert!(cells.contains("app-superuser"), "{cells}");
    assert_no_password(&render(&inventory).join("\n"));
}

#[test]
fn a_backup_and_schedule_keep_the_cluster_ref() {
    let backup = resource_from(Kind::Backup, backup_json());
    assert_eq!(backup.cluster, "app");
    assert_eq!(backup.phase, "completed");
    let scheduled = resource_from(Kind::ScheduledBackup, scheduled_json());
    assert_eq!(scheduled.schedule, "0 0 0 * * *");
    assert_eq!(scheduled.cluster, "app");
}

#[test]
fn a_pooler_keeps_instances_type_and_cluster() {
    let pooler = resource_from(Kind::Pooler, pooler_json());
    assert_eq!(pooler.instances, 2);
    assert_eq!(pooler.pooler_type, "rw");
    assert_eq!(pooler.cluster, "app");
}

#[test]
fn a_pooler_states_an_instance_count_and_never_a_ready_fraction() {
    let inventory = Inventory {
        poolers: KindSet::Served {
            items: vec![resource_from(Kind::Pooler, pooler_json())],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    };
    let page = table_page(&inventory).expect("a served pooler is a table");
    assert_eq!(page.rows[0].cells[4], "2");
    let text = render(&inventory).join("\n");
    assert!(text.contains("Pooler"), "{text}");
    assert!(text.contains("2 instances"), "{text}");
    assert!(
        !text.contains("0/2"),
        "PoolerStatus has no readyInstances, so a healthy Pooler must not show 0/N ready: {text}"
    );
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::Cluster, "v1", serde_json::json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = serde_json::json!({
        "metadata": { "name": huge, "namespace": "data" },
        "spec": {
            "imageName": huge,
            "superuserSecret": { "name": huge }
        },
        "status": { "phase": huge, "currentPrimary": huge }
    });
    let resource = resource_from(Kind::Cluster, value);
    for field in [
        &resource.name,
        &resource.phase,
        &resource.primary,
        &resource.postgres_version,
        &resource.superuser_secret,
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
        "a 403 is Denied, never an empty inventory that looks like CNPG is absent"
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

#[test]
fn an_unserved_cnpg_inventory_has_no_table() {
    assert!(
        table_page(&Inventory::default()).is_none(),
        "a 404 group is absence, not an empty list"
    );
}

#[test]
fn a_denied_cnpg_kind_is_a_labelled_row_not_an_empty_pane() {
    let page = table_page(&Inventory {
        clusters: KindSet::Denied,
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
    assert!(text.contains("Cluster"), "{text}");
}

#[test]
fn a_served_cnpg_fixture_is_one_row_per_object() {
    let cluster = resource_from(Kind::Cluster, cluster_json());
    let page = table_page(&Inventory {
        clusters: KindSet::Served {
            items: vec![cluster],
            truncated: false,
            unreadable: 0,
        },
        ..Inventory::default()
    })
    .expect("a served kind is a table");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].name, "app");
    assert_eq!(page.rows[0].cells[0], "Cluster");
    assert_eq!(page.rows[0].cells[4], "3/3");
    assert_eq!(page.rows[0].cells[5], "app-1");
    assert_eq!(page.rows[0].cells[7], "app-superuser");
}

#[test]
fn a_missing_cnpg_group_renders_as_not_installed_rather_than_empty() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served());
    assert_eq!(lines[0], "CloudNativePG is not served by this cluster");
    let text = lines.join("\n");
    assert!(text.contains("Cluster"), "{text}");
    assert!(
        text.contains("nothing is installed to find them"),
        "an empty answer names the reason it could be wrong: {text}"
    );
    assert!(text.contains("password is never fetched"), "{text}");
}

#[test]
fn a_history_renders_instances_primary_and_version() {
    let cluster = resource_from(Kind::Cluster, cluster_json());
    let lines = render(&Inventory {
        clusters: KindSet::Served {
            items: vec![cluster],
            truncated: true,
            unreadable: 1,
        },
        backups: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.starts_with("1 CloudNativePG object"), "{text}");
    assert!(text.contains("data/app"), "{text}");
    assert!(
        text.contains(
            "Cluster  Cluster in healthy state  3/3 ready  primary app-1  16  secret app-superuser"
        ),
        "{text}"
    );
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
    assert!(
        text.contains("cnpg backups: access denied for this account"),
        "a 403 is a labelled denial, not an absent kind: {text}"
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

fn cnpg_group() -> String {
    r#"{"kind":"APIGroup","name":"postgresql.cnpg.io",
        "versions":[{"groupVersion":"postgresql.cnpg.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"postgresql.cnpg.io/v1","version":"v1"}}"#
        .to_string()
}

fn list(kind: &str, items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": format!("{kind}List"),
        "apiVersion": "postgresql.cnpg.io/v1",
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
    assert!(matches!(inventory.clusters, KindSet::NotServed));
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("clusters")),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| seen.method == "GET" && seen.content_type.is_empty()),
        "a CNPG fetch only reads, and a read carries no body: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_the_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/postgresql.cnpg.io", 403, STATUS_403);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied on that kind, not a whole-fetch failure");
    };
    assert!(matches!(inventory.clusters, KindSet::Denied));
    assert!(
        inventory.clusters.served(),
        "403 is Denied, not served: false"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_extracts_inventory_fields_and_follows_a_continue_token() {
    let script = Script::default();
    script.route("GET", "/apis/postgresql.cnpg.io", 200, cnpg_group());
    script.route(
        "GET",
        "/apis/postgresql.cnpg.io/v1/clusters?",
        200,
        serde_json::json!({
            "kind": "ClusterList",
            "metadata": { "continue": "page-2" },
            "items": [cluster_json()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/postgresql.cnpg.io/v1/clusters?",
        200,
        list(
            "Cluster",
            &[serde_json::json!({
                "metadata": { "name": "other", "namespace": "data" },
                "spec": { "instances": 1 },
                "status": { "phase": "Setting up primary" }
            })],
        ),
    );
    script.route(
        "GET",
        "/apis/postgresql.cnpg.io/v1/poolers?",
        200,
        list("Pooler", &[pooler_json()]),
    );

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    let clusters = inventory.clusters.items();
    assert_eq!(
        clusters
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["app", "other"]
    );
    assert_eq!(clusters[0].primary, "app-1");
    assert_eq!(clusters[0].superuser_secret, "app-superuser");
    assert_no_password(&format!("{:?}", inventory.clusters));
    assert_eq!(inventory.poolers.items()[0].pooler_type, "rw");

    let lists: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("/clusters"))
        .collect();
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
}
