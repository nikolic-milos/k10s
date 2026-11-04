//! A disposable account proves Grafana API reads without ConfigMap access.

use super::{kube_runtime, said};
use base64::Engine;
use k10s_data::integrations::grafana::{self, QueryDialect};
use k10s_data::reach::{self, ToolKind, ToolReach};
use k10s_data::read::Fetched;
use serde_json::{Value, json};
use std::fs::{self, DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

const CHECK_BUDGET: Duration = Duration::from_secs(180);
const CLEANUP_BUDGET: Duration = Duration::from_secs(60);

struct OwnedObject {
    collection: String,
    name: String,
    uid: String,
}

struct IdentityFixture {
    admin: kube::Client,
    name: String,
    owned: Vec<OwnedObject>,
    pending: Option<(String, String)>,
    pending_scratch: Option<PathBuf>,
    directory: Option<(PathBuf, (u64, u64))>,
    config: Option<(PathBuf, (u64, u64))>,
}

async fn request(
    client: &kube::Client,
    message: http::Request<Vec<u8>>,
    deadline: Instant,
) -> Result<Value, String> {
    tokio::time::timeout_at(deadline, client.request(message))
        .await
        .map_err(|_| "the Grafana RBAC fixture exceeded its deadline".to_string())?
        .map_err(|error| said(&error))
}

fn get(collection: &str, name: &str) -> http::Request<Vec<u8>> {
    kube::api::Request::new(collection)
        .get(name, &kube::api::GetParams::default())
        .expect("a fixed metadata GET")
}

fn created_identity<'a>(
    created: &'a Value,
    requested: &Value,
    collection: &str,
) -> Result<(&'a str, &'a str), &'static str> {
    let expected_name = requested["metadata"]["name"]
        .as_str()
        .ok_or("the fixture request omitted its name")?;
    let expected_kind = requested["kind"]
        .as_str()
        .ok_or("the fixture request omitted its kind")?;
    let expected_version = requested["apiVersion"]
        .as_str()
        .ok_or("the fixture request omitted its API version")?;
    let name = created["metadata"]["name"]
        .as_str()
        .ok_or("a created object returned no name; ownership is unresolved")?;
    let uid = created["metadata"]["uid"]
        .as_str()
        .ok_or("a created object returned no UID; ownership is unresolved")?;
    let namespace = match created["metadata"].get("namespace") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.is_empty() => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err("the created fixture namespace is invalid; ownership is unresolved"),
    };
    let expected_namespace = collection
        .split_once("/namespaces/")
        .and_then(|(_, suffix)| suffix.split_once('/').map(|(name, _)| name));
    if name != expected_name
        || uid.is_empty()
        || created["kind"].as_str() != Some(expected_kind)
        || created["apiVersion"].as_str() != Some(expected_version)
        || namespace != expected_namespace
    {
        return Err(
            "the created fixture identity did not match its request; ownership is unresolved",
        );
    }
    Ok((name, uid))
}

impl IdentityFixture {
    fn new(admin: kube::Client) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the current clock")
            .as_nanos();
        Self {
            admin,
            name: format!("k10s-grafana-rbac-{stamp}-{}", std::process::id()),
            owned: Vec::new(),
            pending: None,
            pending_scratch: None,
            directory: None,
            config: None,
        }
    }

    async fn create(
        &mut self,
        collection: &str,
        mut object: Value,
        deadline: Instant,
    ) -> Result<(), String> {
        object["metadata"]["name"] = self.name.clone().into();
        self.pending = Some((collection.to_string(), self.name.clone()));
        eprintln!(
            "Grafana RBAC fixture create intent: {collection}/{}",
            self.name
        );
        let message = kube::api::Request::new(collection)
            .create(
                &kube::api::PostParams::default(),
                serde_json::to_vec(&object).expect("a credential-free fixture object"),
            )
            .expect("a fixed fixture CREATE");
        let result = tokio::time::timeout_at(deadline, self.admin.request::<Value>(message)).await;
        let created = match result {
            Ok(Ok(created)) => created,
            Ok(Err(kube::Error::Api(status))) if status.code == 409 => {
                self.pending = None;
                return Err(
                    "a generated fixture name collided; the existing object was preserved".into(),
                );
            }
            Ok(Err(kube::Error::Api(status))) if matches!(status.code, 401 | 403 | 422) => {
                self.pending = None;
                return Err(status.message);
            }
            Ok(Err(error)) => return Err(said(&error)),
            Err(_) => return Err("a fixture CREATE timed out; its write remains uncertain".into()),
        };
        let (name, uid) = created_identity(&created, &object, collection)?;
        self.owned.push(OwnedObject {
            collection: collection.to_string(),
            name: name.to_string(),
            uid: uid.to_string(),
        });
        self.pending = None;
        eprintln!("Grafana RBAC fixture owns {collection}/{name} UID {uid}");
        Ok(())
    }

    async fn grant(
        &mut self,
        found: &reach::FoundService,
        deadline: Instant,
    ) -> Result<(), String> {
        self.create(
            "/api/v1/namespaces",
            json!({"apiVersion":"v1","kind":"Namespace","metadata":{}}),
            deadline,
        )
        .await?;
        self.create(&format!("/api/v1/namespaces/{}/serviceaccounts", self.name), json!({
            "apiVersion":"v1","kind":"ServiceAccount","metadata":{},"automountServiceAccountToken":false
        }), deadline).await?;
        self.create(
            "/apis/rbac.authorization.k8s.io/v1/clusterroles",
            json!({
                "apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{},
                "rules":[{"apiGroups":[""],"resources":["services"],"verbs":["list"]}]
            }),
            deadline,
        )
        .await?;
        self.create("/apis/rbac.authorization.k8s.io/v1/clusterrolebindings", json!({
            "apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding","metadata":{},
            "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":self.name},
            "subjects":[{"kind":"ServiceAccount","name":self.name,"namespace":self.name}]
        }), deadline).await?;
        let health_port = found
            .port_name
            .clone()
            .unwrap_or_else(|| found.port.to_string());
        let health_name = format!("{}:{health_port}", found.name);
        let query_name = format!("{}:{}", found.name, found.port);
        self.create(&format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{}/roles", found.namespace), json!({
            "apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{},
            "rules":[
                {"apiGroups":[""],"resources":["services"],"resourceNames":[found.name],"verbs":["get"]},
                {"apiGroups":[""],"resources":["services/proxy"],"resourceNames":[health_name,query_name],"verbs":["get"]}
            ]
        }), deadline).await?;
        self.create(
            &format!(
                "/apis/rbac.authorization.k8s.io/v1/namespaces/{}/rolebindings",
                found.namespace
            ),
            json!({
                "apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{},
                "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":self.name},
                "subjects":[{"kind":"ServiceAccount","name":self.name,"namespace":self.name}]
            }),
            deadline,
        )
        .await
    }

    fn scratch_config(&mut self, admin: &kube::Config, token: String) -> Result<PathBuf, String> {
        if admin.accept_invalid_certs || admin.proxy_url.is_some() {
            return Err("the lab fixture requires verified direct API-server TLS".into());
        }
        let roots = admin
            .root_cert
            .as_ref()
            .ok_or("the lab CA bundle is required")?;
        let mut pem = String::new();
        for root in roots {
            pem.push_str("-----BEGIN CERTIFICATE-----\n");
            let encoded = base64::engine::general_purpose::STANDARD.encode(root);
            for line in encoded.as_bytes().chunks(64) {
                pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
                pem.push('\n');
            }
            pem.push_str("-----END CERTIFICATE-----\n");
        }
        let directory = Path::new("/tmp").join(&self.name);
        DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|error| error.to_string())?;
        self.pending_scratch = Some(directory.clone());
        let meta = fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
        self.directory = Some((directory.clone(), (meta.dev(), meta.ino())));
        self.pending_scratch = None;
        if !meta.is_dir() || meta.mode() & 0o777 != 0o700 {
            return Err("the scratch directory is not private 0700".into());
        }
        let path = directory.join("token-only.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| error.to_string())?;
        self.pending_scratch = Some(path.clone());
        let meta = file.metadata().map_err(|error| error.to_string())?;
        self.config = Some((path.clone(), (meta.dev(), meta.ino())));
        self.pending_scratch = None;
        if !meta.is_file() || meta.mode() & 0o777 != 0o600 {
            return Err("the scratch config is not private 0600".into());
        }
        let mut cluster = json!({
            "server":admin.cluster_url.to_string(),
            "certificate-authority-data":base64::engine::general_purpose::STANDARD.encode(pem.as_bytes())
        });
        if let Some(name) = &admin.tls_server_name {
            cluster["tls-server-name"] = name.clone().into();
        }
        let config = json!({
            "apiVersion":"v1","kind":"Config",
            "clusters":[{"name":"lab","cluster":cluster}],
            "users":[{"name":"fixture","user":{"token":token}}],
            "contexts":[{"name":"fixture","context":{"cluster":"lab","user":"fixture","namespace":self.name}}],
            "current-context":"fixture"
        });
        serde_json::to_writer(&mut file, &config)
            .map_err(|_| "writing the private credential config failed".to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        Ok(path)
    }

    async fn cleanup(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + CLEANUP_BUDGET;
        let mut errors = Vec::new();
        for owned in self.owned.iter().rev() {
            let message = kube::api::Request::new(&owned.collection)
                .delete(
                    &owned.name,
                    &kube::api::DeleteParams {
                        preconditions: Some(kube::api::Preconditions {
                            uid: Some(owned.uid.clone()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .expect("a UID-precondition fixture DELETE");
            let deletion =
                tokio::time::timeout_at(deadline, self.admin.request::<Value>(message)).await;
            if let Ok(Err(kube::Error::Api(status))) = &deletion {
                if status.code != 404 && status.code != 409 {
                    errors.push(format!(
                        "delete {}/{} UID {}: {}",
                        owned.collection, owned.name, owned.uid, status.message
                    ));
                }
            } else if !matches!(deletion, Ok(Ok(_))) {
                errors.push(format!(
                    "delete {}/{} UID {} did not complete",
                    owned.collection, owned.name, owned.uid
                ));
            }
            loop {
                let result = tokio::time::timeout_at(
                    deadline,
                    self.admin
                        .request::<Value>(get(&owned.collection, &owned.name)),
                )
                .await;
                match result {
                    Ok(Err(kube::Error::Api(status))) if status.code == 404 => break,
                    Ok(Ok(current))
                        if current["metadata"]["uid"]
                            .as_str()
                            .is_some_and(|uid| !uid.is_empty() && uid != owned.uid) =>
                    {
                        break;
                    }
                    Ok(Ok(current))
                        if current["metadata"]["uid"].as_str() == Some(owned.uid.as_str())
                            && Instant::now() < deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    }
                    _ => {
                        errors.push(format!(
                            "cleanup unresolved: {}/{} UID {}",
                            owned.collection, owned.name, owned.uid
                        ));
                        break;
                    }
                }
            }
        }
        if let Some(path) = &self.pending_scratch {
            errors.push(format!("unresolved scratch ownership: {}", path.display()));
        }
        if let Some((collection, name)) = &self.pending {
            errors.push(format!(
                "unresolved CREATE: {collection}/{name}; no UID was adopted"
            ));
        }
        for (slot, directory) in [(&self.config, false), (&self.directory, true)] {
            if let Some((path, identity)) = slot {
                let result = (|| {
                    let current = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
                    if current.file_type().is_symlink()
                        || (current.dev(), current.ino()) != *identity
                    {
                        return Err("scratch identity changed; entry was preserved".to_string());
                    }
                    if directory {
                        fs::remove_dir(path)
                    } else {
                        fs::remove_file(path)
                    }
                    .map_err(|error| error.to_string())
                })();
                if let Err(error) = result {
                    errors.push(format!("cleanup {}: {error}", path.display()));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

async fn access(
    client: &kube::Client,
    attributes: Value,
    deadline: Instant,
) -> Result<bool, String> {
    let body = json!({"apiVersion":"authorization.k8s.io/v1","kind":"SelfSubjectAccessReview","spec":{"resourceAttributes":attributes}});
    let message = kube::api::Request::new("/apis/authorization.k8s.io/v1/selfsubjectaccessreviews")
        .create(
            &kube::api::PostParams::default(),
            serde_json::to_vec(&body).expect("an authorization review"),
        )
        .expect("a self-access review CREATE");
    request(client, message, deadline).await?["status"]["allowed"]
        .as_bool()
        .ok_or_else(|| "self-access review omitted its decision".to_string())
}

async fn check_catalog(fixture: &mut IdentityFixture, config: &kube::Config) -> Result<(), String> {
    let admin = fixture.admin.clone();
    let deadline = Instant::now() + CHECK_BUDGET;
    let found =
        match tokio::time::timeout_at(deadline, reach::find_services(&admin, ToolKind::Grafana))
            .await
        {
            Ok(Fetched::Ok(mut found)) if found.len() == 1 => {
                found.pop().expect("one Grafana Service")
            }
            _ => {
                return Err(
                    "the live fixture requires exactly one discovered Grafana Service".to_string(),
                );
            }
        };
    let service_path = format!("/api/v1/namespaces/{}/services", found.namespace);
    let original = request(&admin, get(&service_path, &found.name), deadline).await?;
    let service_uid = original["metadata"]["uid"]
        .as_str()
        .ok_or("Grafana Service has no UID")?
        .to_string();
    eprintln!(
        "Grafana RBAC fixture source: {}/{} UID {service_uid}, health port {:?}, query port {}",
        found.namespace, found.name, found.port_name, found.port
    );
    let bound = match tokio::time::timeout_at(
        deadline,
        reach::bind(&admin, ToolKind::Grafana, &Default::default()),
    )
    .await
    {
        Ok(ToolReach::Bound(bound)) => bound,
        _ => return Err("the current Grafana API is not bound".into()),
    };
    let hits =
        match tokio::time::timeout_at(deadline, grafana::fetch_search(&admin, &bound, &[])).await {
            Ok(Fetched::Ok(page)) => page.hits,
            _ => return Err("the current Grafana API search is not readable".into()),
        };
    let mut expected = None;
    for hit in hits.into_iter().take(grafana::MAX_NAMED_FROM_SEARCH) {
        if hit.uid.is_empty() {
            continue;
        }
        let fetched =
            tokio::time::timeout_at(deadline, grafana::fetch_dashboard(&admin, &bound, &hit.uid))
                .await;
        if let Ok(Fetched::Ok(dashboard)) = fetched {
            for panel in &dashboard.panels {
                if panel.unsupported.is_none()
                    && matches!(
                        panel.kind,
                        grafana::PanelKind::Timeseries | grafana::PanelKind::Stat
                    )
                    && let Some(query) = panel.queries.iter().find(|query| {
                        query.unsupported.is_none()
                            && !query.expr.contains('$')
                            && query.dialect == QueryDialect::PromQL
                            && query.datasource.is_some()
                    })
                {
                    expected = Some((
                        dashboard.uid.clone(),
                        panel.id,
                        query.ref_id.clone(),
                        query.expr.clone(),
                    ));
                    break;
                }
            }
            if expected.is_some() {
                break;
            }
        }
    }
    let expected = expected
        .ok_or("the current API catalogue has no supported query within its fetch bound")?;
    fixture.grant(&found, deadline).await?;
    use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
    use k8s_openapi::api::core::v1::ServiceAccount;
    let accounts: kube::Api<ServiceAccount> = kube::Api::namespaced(admin.clone(), &fixture.name);
    let issued = tokio::time::timeout_at(
        deadline,
        accounts.create_token_request(
            &fixture.name,
            &kube::api::PostParams::default(),
            &TokenRequest {
                metadata: Default::default(),
                spec: Some(TokenRequestSpec {
                    audiences: None,
                    bound_object_ref: None,
                    expiration_seconds: Some(600),
                }),
                status: None,
            },
        ),
    )
    .await
    .map_err(|_| "TokenRequest timed out; credential details omitted")?
    .map_err(|_| "TokenRequest failed; credential details omitted")?;
    let status = issued.status.ok_or("TokenRequest returned no status")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the current clock is invalid")?
        .as_secs() as i64;
    let expiration = status
        .expiration_timestamp
        .ok_or("TokenRequest returned no expiration timestamp")?;
    let token = status
        .token
        .filter(|token| !token.is_empty())
        .ok_or("TokenRequest returned no token")?;
    let lifetime = expiration.0.as_second() - now;
    if !(240..=900).contains(&lifetime) {
        return Err("the issued credential has an unexpected lifetime".into());
    }
    let path = fixture.scratch_config(config, token)?;
    let kubeconfig = kube::config::Kubeconfig::read_from(&path)
        .map_err(|_| "reading the token-only config failed; details omitted")?;
    let mut restricted = kube::Config::from_custom_kubeconfig(
        kubeconfig,
        &kube::config::KubeConfigOptions::default(),
    )
    .await
    .map_err(|_| "loading the token-only config failed; details omitted")?;
    restricted.default_retry = false;
    restricted.connect_timeout = Some(Duration::from_secs(4));
    restricted.read_timeout = Some(Duration::from_secs(4));
    restricted.write_timeout = Some(Duration::from_secs(4));
    let restricted = kube::Client::try_from(restricted)
        .map_err(|_| "building the token-only client failed; details omitted")?;
    let subject = kube::api::Request::new("/apis/authentication.k8s.io/v1/selfsubjectreviews")
        .create(
            &kube::api::PostParams::default(),
            serde_json::to_vec(
                &json!({"apiVersion":"authentication.k8s.io/v1","kind":"SelfSubjectReview"}),
            )
            .expect("a subject review"),
        )
        .expect("a self-subject review CREATE");
    let subject = request(&restricted, subject, deadline).await?;
    let account_uid = fixture
        .owned
        .iter()
        .find(|owned| owned.collection.ends_with("/serviceaccounts"))
        .map(|owned| owned.uid.as_str())
        .ok_or("the generated ServiceAccount UID was not recorded")?;
    if subject["status"]["userInfo"]["uid"].as_str() != Some(account_uid) {
        return Err(
            "the authenticated subject does not have the created ServiceAccount UID".into(),
        );
    }
    if subject["status"]["userInfo"]["username"].as_str()
        != Some(format!("system:serviceaccount:{}:{}", fixture.name, fixture.name).as_str())
    {
        return Err(
            "the token-only client did not authenticate as the generated ServiceAccount".into(),
        );
    }
    let health_name = format!(
        "{}:{}",
        found.name,
        found
            .port_name
            .clone()
            .unwrap_or_else(|| found.port.to_string())
    );
    let query_name = format!("{}:{}", found.name, found.port);
    loop {
        for resource in ["configmaps", "secrets"] {
            for verb in ["get", "list", "watch"] {
                for ns in [&fixture.name, &found.namespace] {
                    if access(
                        &restricted,
                        json!({"group":"","resource":resource,"verb":verb,"namespace":ns}),
                        deadline,
                    )
                    .await?
                    {
                        return Err(format!(
                            "the disposable identity unexpectedly permits {verb} {resource} in {ns}"
                        ));
                    }
                }
            }
            if access(
                &restricted,
                json!({"group":"","resource":resource,"verb":"list"}),
                deadline,
            )
            .await?
            {
                return Err(format!(
                    "the disposable identity unexpectedly permits cluster-wide {resource}"
                ));
            }
        }
        let listed = access(
            &restricted,
            json!({"group":"","resource":"services","verb":"list"}),
            deadline,
        )
        .await?;
        let identity = access(&restricted, json!({"group":"","resource":"services","verb":"get","namespace":found.namespace,"name":found.name}), deadline).await?;
        let health = access(&restricted, json!({"group":"","resource":"services","subresource":"proxy","verb":"get","namespace":found.namespace,"name":health_name}), deadline).await?;
        let query = access(&restricted, json!({"group":"","resource":"services","subresource":"proxy","verb":"get","namespace":found.namespace,"name":query_name}), deadline).await?;
        if listed && identity && health && query {
            break;
        }
        if Instant::now() >= deadline {
            return Err("the generated RBAC grants did not become effective".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !matches!(
        tokio::time::timeout_at(
            deadline,
            grafana::fetch_provisioned_from_configmaps(&restricted)
        )
        .await,
        Ok(Fetched::Denied { what: "configmaps" })
    ) {
        return Err(
            "the real provisioning request did not produce the expected ConfigMap denial".into(),
        );
    }
    let (sink, _events) = crossbeam_channel::bounded(k10s_data::DEFAULT_EVENT_SINK_CAPACITY);
    let sync = tokio::time::timeout_at(
        deadline,
        k10s_data::sync_from(
            k10s_data::connect::Clients {
                sync: restricted.clone(),
                interactive: restricted,
            },
            &fixture.name,
            &k10s_data::Options {
                context: Some("fixture".into()),
                kubeconfig: Some(path),
                probe_namespaces: vec![fixture.name.clone()],
                sync_timeout: Duration::from_secs(30),
            },
            sink,
            std::sync::Arc::new(k10s_data::IngestMetrics::default()),
        ),
    )
    .await
    .map_err(|_| "the restricted Reader sync timed out")?
    .map_err(|_| "the restricted Reader sync failed; credential details omitted")?;
    let (send, receive) = tokio::sync::oneshot::channel();
    let _stop = sync.reader.observe_grafana(move |answer| {
        let _ = send.send(answer);
    });
    let answer = tokio::time::timeout_at(deadline, receive)
        .await
        .map_err(|_| "the native catalogue timed out")?
        .map_err(|_| "the native catalogue callback was dropped")?;
    let Fetched::Ok(catalog) = answer else {
        return Err("the native API catalogue did not survive the ConfigMap denial".into());
    };
    if !catalog.served
        || !catalog.errors.iter().any(|notice| {
            notice == "provisioned dashboards: configmaps: access denied for this account"
        })
    {
        return Err(
            "the native catalogue omitted its served state or specific ConfigMap denial".into(),
        );
    }
    let kept = catalog.dashboards.iter().any(|dashboard| {
        dashboard.uid == expected.0
            && dashboard.panels.iter().any(|panel| {
                panel.id == expected.1
                    && panel.unsupported.is_none()
                    && panel.queries.iter().any(|query| {
                        query.ref_id == expected.2
                            && query.expr == expected.3
                            && query.unsupported.is_none()
                    })
            })
    });
    if !kept {
        return Err("the real API panel query was lost across the denied provisioning read".into());
    }
    let current = request(&admin, get(&service_path, &found.name), deadline).await?;
    if current["metadata"]["uid"].as_str() != Some(service_uid.as_str())
        || current["spec"] != original["spec"]
    {
        return Err(
            "the observed Grafana Service identity or spec changed during the proof".into(),
        );
    }
    eprintln!(
        "Grafana ConfigMap-denied/API-allowed proof: disposable subject={}, dashboard={}, panel={:?}, target={}",
        fixture.name, expected.0, expected.1, expected.2
    );
    Ok::<(), String>(())
}

#[test]
fn fixture_adopts_only_the_requested_identity_with_a_nonempty_uid() {
    let collection = "/api/v1/namespaces/lab/serviceaccounts";
    let requested =
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"fixture"}});
    let valid = json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"fixture","namespace":"lab","uid":"owned-uid"}});
    assert_eq!(
        created_identity(&valid, &requested, collection),
        Ok(("fixture", "owned-uid"))
    );
    for invalid in [
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"other","namespace":"lab","uid":"owned-uid"}}),
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"fixture","namespace":"lab","uid":""}}),
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"fixture","namespace":"lab"}}),
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"namespace":"lab","uid":"owned-uid"}}),
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"fixture","namespace":"other","uid":"owned-uid"}}),
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"fixture","uid":"owned-uid"}}),
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"fixture","namespace":7,"uid":"owned-uid"}}),
        json!({"apiVersion":"v2","kind":"ServiceAccount","metadata":{"name":"fixture","namespace":"lab","uid":"owned-uid"}}),
        json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"fixture","namespace":"lab","uid":"owned-uid"}}),
    ] {
        assert!(created_identity(&invalid, &requested, collection).is_err());
    }
    let requested = json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":"fixture"}});
    let cluster_object = json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":"fixture","uid":"owned-uid"}});
    assert_eq!(
        created_identity(&cluster_object, &requested, "/api/v1/namespaces"),
        Ok(("fixture", "owned-uid"))
    );
    let mut wrong_namespace = cluster_object;
    wrong_namespace["metadata"]["namespace"] = "other".into();
    assert!(created_identity(&wrong_namespace, &requested, "/api/v1/namespaces").is_err());
}

#[test]
#[ignore = "needs the real Grafana lab and permission to create disposable RBAC fixtures"]
fn native_grafana_catalog_survives_configmap_denial_with_a_token_only_identity() {
    let runtime = kube_runtime();
    let mut config = runtime.block_on(kube::Config::infer()).unwrap_or_else(|_| {
        panic!("the lab admin connection could not be read; credential details omitted")
    });
    config.default_retry = false;
    config.connect_timeout = Some(Duration::from_secs(4));
    config.read_timeout = Some(Duration::from_secs(4));
    config.write_timeout = Some(Duration::from_secs(4));
    let admin = runtime
        .block_on(async { kube::Client::try_from(config.clone()) })
        .unwrap_or_else(|_| {
            panic!("the lab admin client could not be built; credential details omitted")
        });
    let mut fixture = IdentityFixture::new(admin);
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(check_catalog(&mut fixture, &config))
    }));
    let cleanup = runtime.block_on(fixture.cleanup());
    if let Err(error) = cleanup {
        if let Ok(Err(check)) = &checked {
            eprintln!("Grafana RBAC fixture check failed: {check}");
        }
        panic!("Grafana RBAC fixture cleanup is unresolved: {error}");
    }
    match checked {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("Grafana RBAC fixture failed: {error}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
