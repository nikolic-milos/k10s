//! Field extraction, caps, the document, 404/403 classification, and the
//! merge-patch bytes Flux already honours. A cluster is not required.

use super::*;
use crate::read::Fetched;
use kube::client::Body;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::Service;

fn git_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "podinfo", "namespace": "flux-system" },
        "spec": {
            "url": "https://github.com/stefanprodan/podinfo",
            "suspend": false
        },
        "status": {
            "conditions": [
                { "type": "Ready", "status": "True", "reason": "GitOperationSucceeded" }
            ],
            "artifact": { "revision": "master@sha1:abc123" }
        }
    })
}

fn kustomization_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "apps", "namespace": "flux-system" },
        "spec": {
            "suspend": true,
            "sourceRef": {
                "kind": "GitRepository",
                "name": "podinfo",
                "namespace": "flux-system"
            }
        },
        "status": {
            "conditions": [{ "type": "Ready", "status": "False" }],
            "lastAppliedRevision": "master@sha1:abc123"
        }
    })
}

fn helm_json() -> serde_json::Value {
    serde_json::json!({
        "metadata": { "name": "ingress", "namespace": "prod" },
        "spec": {
            "chart": {
                "spec": {
                    "sourceRef": { "kind": "HelmRepository", "name": "bitnami" }
                }
            }
        },
        "status": {
            "conditions": [{ "type": "Ready", "status": "True" }],
            "lastAttemptedRevision": "4.11.3"
        }
    })
}

fn resource_from(kind: Kind, version: &str, value: serde_json::Value) -> Resource {
    parse_item(kind, version, value).expect("the fixture is a Flux object")
}

fn target() -> Resource {
    Resource {
        kind: Kind::GitRepository,
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
fn a_git_repository_keeps_its_url_ready_condition_and_artifact_revision() {
    let resource = resource_from(Kind::GitRepository, "v1", git_json());
    assert_eq!(resource.name, "podinfo");
    assert_eq!(resource.namespace, "flux-system");
    assert_eq!(resource.ready, "True");
    assert!(!resource.suspended);
    assert_eq!(resource.last_applied_revision, "master@sha1:abc123");
    assert_eq!(
        resource.source_ref,
        "https://github.com/stefanprodan/podinfo"
    );
}

#[test]
fn a_kustomization_keeps_its_source_ref_and_last_applied_revision() {
    let resource = resource_from(Kind::Kustomization, "v1", kustomization_json());
    assert!(resource.suspended);
    assert_eq!(resource.ready, "False");
    assert_eq!(resource.source_ref, "GitRepository/flux-system/podinfo");
    assert_eq!(resource.last_applied_revision, "master@sha1:abc123");
}

#[test]
fn a_helm_release_falls_back_to_last_attempted_revision_and_chart_source() {
    let resource = resource_from(Kind::HelmRelease, "v2", helm_json());
    assert_eq!(resource.last_applied_revision, "4.11.3");
    assert_eq!(resource.source_ref, "HelmRepository/bitnami");
}

#[test]
fn a_helm_release_prefers_chart_ref_when_both_are_set() {
    let mut value = helm_json();
    value["spec"]["chartRef"] = serde_json::json!({
        "kind": "OCIRepository",
        "name": "charts",
        "namespace": "flux-system"
    });
    let resource = resource_from(Kind::HelmRelease, "v2", value);
    assert_eq!(resource.source_ref, "OCIRepository/flux-system/charts");
}

#[test]
fn an_oci_repository_reads_the_same_shape_as_a_git_repository() {
    let mut value = git_json();
    value["spec"]["url"] = serde_json::json!("oci://ghcr.io/stefanprodan/manifests/podinfo");
    let resource = resource_from(Kind::OCIRepository, "v1", value);
    assert_eq!(
        resource.source_ref,
        "oci://ghcr.io/stefanprodan/manifests/podinfo"
    );
    assert_eq!(resource.last_applied_revision, "master@sha1:abc123");
}

#[test]
fn a_nameless_object_is_not_an_inventory_row() {
    assert!(parse_item(Kind::GitRepository, "v1", serde_json::json!({})).is_none());
}

#[test]
fn one_enormous_field_is_clipped_where_it_is_carried() {
    let huge = "a".repeat(6 << 20);
    let value = serde_json::json!({
        "metadata": { "name": huge, "namespace": "flux-system" },
        "spec": { "url": huge },
        "status": { "artifact": { "revision": huge } }
    });
    let resource = resource_from(Kind::GitRepository, "v1", value);
    for field in [
        &resource.name,
        &resource.source_ref,
        &resource.last_applied_revision,
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
        "a 403 is Denied, never an empty inventory that looks like Flux is absent"
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
fn rfc3339_is_utc_from_the_unix_epoch() {
    assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(1_704_067_200, 0), "2024-01-01T00:00:00Z");
    assert_eq!(
        rfc3339(1_704_067_200, 123_456_789),
        "2024-01-01T00:00:00.123456789Z"
    );
}

fn header(request: &http::Request<Vec<u8>>, name: http::header::HeaderName) -> &str {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

#[test]
fn a_suspend_is_a_merge_patch_of_spec_suspend() {
    let request = suspend_request(&target(), true).expect("a patch builds");
    assert_eq!(request.method(), http::Method::PATCH);
    assert_eq!(
        header(&request, http::header::CONTENT_TYPE),
        "application/merge-patch+json"
    );
    assert!(
        request.uri().path().contains(
            "/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/podinfo"
        ),
        "the object path Flux already serves: {}",
        request.uri()
    );
    assert_eq!(request.body(), br#"{"spec":{"suspend":true}}"#);
}

#[test]
fn a_resume_clears_suspend_the_same_way() {
    let request = suspend_request(&target(), false).expect("a patch builds");
    assert_eq!(request.body(), br#"{"spec":{"suspend":false}}"#);
    assert_eq!(
        header(&request, http::header::CONTENT_TYPE),
        "application/merge-patch+json"
    );
}

#[test]
fn reconcile_now_sets_the_annotation_flux_already_honours() {
    let at = "2026-08-13T16:31:00Z";
    let request = reconcile_request(&target(), at).expect("a patch builds");
    assert_eq!(request.method(), http::Method::PATCH);
    assert_eq!(
        header(&request, http::header::CONTENT_TYPE),
        "application/merge-patch+json"
    );
    let body: serde_json::Value = serde_json::from_slice(request.body()).expect("json");
    assert_eq!(
        body.pointer("/metadata/annotations/reconcile.fluxcd.io~1requestedAt")
            .and_then(serde_json::Value::as_str),
        Some(at)
    );
    let encoded = std::str::from_utf8(request.body()).expect("utf-8");
    assert!(
        !encoded.contains("forceAt"),
        "no other reconcile API is invented: {encoded}"
    );
}

#[test]
fn a_missing_flux_group_renders_as_not_installed_rather_than_empty() {
    let lines = render(&Inventory::default());
    assert!(!Inventory::default().served());
    assert_eq!(lines[0], "Flux is not served by this cluster");
    let text = lines.join("\n");
    assert!(text.contains("GitRepository"), "{text}");
    assert!(
        text.contains("nothing is installed to find them"),
        "an empty answer names the reason it could be wrong: {text}"
    );
}

#[test]
fn an_inventory_that_could_not_read_anything_does_not_claim_there_is_nothing() {
    let lines = render(&Inventory {
        git_repositories: KindSet::Served {
            items: Vec::new(),
            truncated: false,
            unreadable: 3,
        },
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(
        !text.contains("no Flux objects are stored"),
        "three are stored and were seen: {text}"
    );
    assert!(lines[0].contains("though some are stored"), "{text}");
}

#[test]
fn a_history_renders_ready_suspend_source_and_revision() {
    let git = resource_from(Kind::GitRepository, "v1", git_json());
    let apps = resource_from(Kind::Kustomization, "v1", kustomization_json());
    let lines = render(&Inventory {
        git_repositories: KindSet::Served {
            items: vec![git],
            truncated: true,
            unreadable: 2,
        },
        kustomizations: KindSet::Served {
            items: vec![apps],
            truncated: false,
            unreadable: 0,
        },
        helm_releases: KindSet::Denied,
        ..Inventory::default()
    });
    let text = lines.join("\n");
    assert!(text.starts_with("2 Flux objects"), "{text}");
    assert!(text.contains("flux-system/podinfo"), "{text}");
    assert!(
        text.contains(
            "GitRepository  Ready  https://github.com/stefanprodan/podinfo  master@sha1:abc123"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "Kustomization  not ready  suspended  GitRepository/flux-system/podinfo  master@sha1:abc123"
        ),
        "{text}"
    );
    assert!(text.contains("stopped at"), "a cap is stated: {text}");
    assert!(
        text.contains("2 Flux objects could not be decoded and are not shown"),
        "{text}"
    );
    assert!(
        text.contains("flux helmreleases: access denied for this account"),
        "a 403 is a labelled denial, not an absent kind: {text}"
    );
    assert!(
        !text.contains("OCIRepository"),
        "a kind the group did not serve stays invisible: {text}"
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
        panic!("a denied Flux action must not touch the wire");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_capability_does_not_touch_the_wire() {
    let client = kube::Client::new(PanicOnCall, "default");
    assert_eq!(
        set_suspended(&client, &target(), true, false).await,
        Fetched::Denied {
            what: "flux gitrepositories"
        }
    );
    assert_eq!(
        reconcile_now(&client, &target(), false).await,
        Fetched::Denied {
            what: "flux gitrepositories"
        }
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

fn source_group() -> String {
    r#"{"kind":"APIGroup","name":"source.toolkit.fluxcd.io",
        "versions":[{"groupVersion":"source.toolkit.fluxcd.io/v1","version":"v1"}],
        "preferredVersion":{"groupVersion":"source.toolkit.fluxcd.io/v1","version":"v1"}}"#
        .to_string()
}

fn list(items: &[serde_json::Value]) -> String {
    serde_json::json!({
        "kind": "GitRepositoryList",
        "apiVersion": "source.toolkit.fluxcd.io/v1",
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
    assert!(
        !inventory.git_repositories.served(),
        "404 on the group is served: false"
    );
    assert!(matches!(inventory.git_repositories, KindSet::NotServed));
    assert!(matches!(inventory.kustomizations, KindSet::NotServed));
    assert!(
        script
            .seen()
            .iter()
            .all(|seen| !seen.path.contains("gitrepositories")),
        "a 404 group must not be chased into a list: {:?}",
        script.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_403_on_the_group_is_denied_not_absent() {
    let script = Script::default();
    script.route("GET", "/apis/source.toolkit.fluxcd.io", 403, STATUS_403);
    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a forbidden group is Denied on that kind, not a whole-fetch failure");
    };
    assert!(
        matches!(inventory.git_repositories, KindSet::Denied),
        "{:?}",
        inventory.git_repositories
    );
    assert!(
        inventory.git_repositories.served(),
        "403 is Denied, not served: false"
    );
    assert!(matches!(inventory.kustomizations, KindSet::NotServed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_extracts_inventory_fields_and_follows_a_continue_token() {
    let script = Script::default();
    script.route("GET", "/apis/source.toolkit.fluxcd.io", 200, source_group());
    script.route(
        "GET",
        "/apis/source.toolkit.fluxcd.io/v1/gitrepositories?",
        200,
        serde_json::json!({
            "kind": "GitRepositoryList",
            "metadata": { "continue": "page-2" },
            "items": [git_json()]
        })
        .to_string(),
    );
    script.route(
        "GET",
        "/apis/source.toolkit.fluxcd.io/v1/gitrepositories?",
        200,
        list(&[serde_json::json!({
            "metadata": { "name": "web", "namespace": "flux-system" },
            "spec": { "url": "https://github.com/example/web" },
            "status": { "conditions": [{ "type": "Ready", "status": "Unknown" }] }
        })]),
    );
    script.route(
        "GET",
        "/apis/kustomize.toolkit.fluxcd.io",
        200,
        r#"{"kind":"APIGroup","name":"kustomize.toolkit.fluxcd.io",
            "versions":[{"groupVersion":"kustomize.toolkit.fluxcd.io/v1","version":"v1"}],
            "preferredVersion":{"groupVersion":"kustomize.toolkit.fluxcd.io/v1","version":"v1"}}"#,
    );
    script.route(
        "GET",
        "/apis/kustomize.toolkit.fluxcd.io/v1/kustomizations?",
        200,
        serde_json::json!({
            "kind": "KustomizationList",
            "items": [kustomization_json()]
        })
        .to_string(),
    );

    let Fetched::Ok(inventory) = fetch(&script.client(), None).await else {
        panic!("a served listing must resolve");
    };
    let git = inventory.git_repositories.items();
    assert_eq!(
        git.iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["podinfo", "web"]
    );
    assert_eq!(git[0].last_applied_revision, "master@sha1:abc123");
    let apps = &inventory.kustomizations.items()[0];
    assert!(apps.suspended);
    assert_eq!(apps.source_ref, "GitRepository/flux-system/podinfo");
    assert!(matches!(inventory.oci_repositories, KindSet::NotServed));

    let lists: Vec<_> = script
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("gitrepositories"))
        .collect();
    assert_eq!(lists.len(), 2, "two pages: {lists:?}");
    assert!(
        lists[1].path.contains("continue=page-2"),
        "the second page is asked for with the token the first one carried: {}",
        lists[1].path
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspend_and_reconcile_send_the_merge_patch_bytes() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/podinfo",
        200,
        git_json().to_string(),
    );
    script.route(
        "PATCH",
        "/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/podinfo",
        200,
        git_json().to_string(),
    );
    script.route(
        "PATCH",
        "/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/podinfo",
        200,
        git_json().to_string(),
    );

    let client = script.client();
    assert_eq!(
        set_suspended(&client, &target(), true, true).await,
        Fetched::Ok(())
    );
    assert_eq!(
        set_suspended(&client, &target(), false, true).await,
        Fetched::Ok(())
    );
    assert_eq!(
        reconcile_at(&client, &target(), true, "2026-08-13T16:31:00Z").await,
        Fetched::Ok(())
    );

    let seen = script.seen();
    assert_eq!(seen.len(), 3, "{seen:?}");
    for request in &seen {
        assert_eq!(request.method, "PATCH");
        assert_eq!(request.content_type, "application/merge-patch+json");
    }
    assert_eq!(seen[0].body, r#"{"spec":{"suspend":true}}"#);
    assert_eq!(seen[1].body, r#"{"spec":{"suspend":false}}"#);
    let reconcile: serde_json::Value = serde_json::from_str(&seen[2].body).expect("json");
    assert_eq!(
        reconcile.pointer("/metadata/annotations/reconcile.fluxcd.io~1requestedAt"),
        Some(&serde_json::json!("2026-08-13T16:31:00Z"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forbidden_patch_is_denied() {
    let script = Script::default();
    script.route(
        "PATCH",
        "/apis/source.toolkit.fluxcd.io/v1/namespaces/flux-system/gitrepositories/podinfo",
        403,
        STATUS_403,
    );
    assert_eq!(
        set_suspended(&script.client(), &target(), true, true).await,
        Fetched::Denied {
            what: "flux gitrepositories"
        }
    );
}
