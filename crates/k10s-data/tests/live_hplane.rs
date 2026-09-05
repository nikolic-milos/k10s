//! Live H-plane presence: which ecosystem groups this lab serves.
//!
//! Ignored by default: the unit suites keep the no-network discipline and this
//! one is a network test. It needs a cluster named by `KUBECONFIG`.
//!
//! ```text
//! KUBECONFIG=/home/Losmi/.rancher/k3s/k3s.yaml cargo test -p k10s-data \
//!   --test live_hplane -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` matches the other live binaries. This file talks to the
//! cluster through `kube::Client` only. It does not call unlanded adapter
//! modules and it does not install anything. Lab CRD fixtures live in
//! `tests/fixtures/hplane/` and are applied by that directory's `apply.sh`.
//!
//! Gateway API is expected from k3s Traefik. cilium / kyverno / velero / cnpg /
//! eso / kargo / vault are served if and only if the lab CRDs are still
//! present. Helm's Traefik release Secret must still exist. The node must be
//! Ready.
//!
//! Every lab fixture carries a planted canary annotation (`PLANTED-live-canary`
//! or a family-specific token), and the lab holds a Gateway/HTTPRoute/
//! ListenerSet/ReferenceGrant chain, an Ingress whose TLS Secret holds planted
//! bytes, and a Traefik Middleware with basicAuth. The canary assertions here
//! are only meaningful because those tokens really exist on the wire: an
//! adapter that kept raw metadata or fetched the Secret would fail this suite,
//! not pass it vacuously.

use k10s_data::cilium;
use k10s_data::cnpg;
use k10s_data::eso;
use k10s_data::falco;
use k10s_data::gateway;
use k10s_data::ingress;
use k10s_data::kargo;
use k10s_data::kyverno;
use k10s_data::otel;
use k10s_data::proxies;
use k10s_data::read::Fetched;
use k10s_data::tetragon;
use k10s_data::traefik;
use k10s_data::vault;
use k10s_data::velero;
use kube::Client;
use kube::api::{ListParams, Request};
use serde_json::Value;

fn kube_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

async fn connect() -> Client {
    assert!(
        std::env::var_os("KUBECONFIG").is_some(),
        "this test needs KUBECONFIG to name a real cluster; see the module comment"
    );
    Client::try_default()
        .await
        .expect("a client from KUBECONFIG")
}

async fn group_served(client: &Client, group: &str) -> bool {
    let path = format!("/apis/{group}");
    let request = http::Request::get(&path)
        .body(Vec::new())
        .expect("a group request");
    match client.request::<Value>(request).await {
        Ok(_) => true,
        Err(kube::Error::Api(response)) if response.code == 404 => false,
        Err(error) => panic!("{group} group probe failed: {error}"),
    }
}

async fn crd_present(client: &Client, name: &str) -> bool {
    let request = Request::new("/apis/apiextensions.k8s.io/v1/customresourcedefinitions")
        .get(name, &kube::api::GetParams::default())
        .expect("a crd get");
    match client.request::<Value>(request).await {
        Ok(_) => true,
        Err(kube::Error::Api(response)) if response.code == 404 => false,
        Err(error) => panic!("crd {name} get failed: {error}"),
    }
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn gateway_group_is_served() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        assert!(
            group_served(&client, "gateway.networking.k8s.io").await,
            "k3s Traefik already serves gateway.networking.k8s.io"
        );
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn fixture_groups_are_served_iff_their_crds_stayed() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        // group, one CRD that apply.sh installs for that group
        let families = [
            ("cilium.io", "ciliumnetworkpolicies.cilium.io"),
            ("kyverno.io", "clusterpolicies.kyverno.io"),
            ("velero.io", "backups.velero.io"),
            ("postgresql.cnpg.io", "clusters.postgresql.cnpg.io"),
            ("external-secrets.io", "secretstores.external-secrets.io"),
            ("kargo.akuity.io", "stages.kargo.akuity.io"),
            (
                "secrets.hashicorp.com",
                "vaultconnections.secrets.hashicorp.com",
            ),
        ];
        for (group, crd) in families {
            let served = group_served(&client, group).await;
            let stayed = crd_present(&client, crd).await;
            assert_eq!(
                served, stayed,
                "{group} served={served} but crd {crd} present={stayed}"
            );
        }
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn helm_traefik_release_secret_still_exists() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        let params = ListParams::default()
            .labels("owner=helm,name=traefik")
            .fields("type=helm.sh/release.v1");
        let request = Request::new("/api/v1/namespaces/kube-system/secrets")
            .list(&params)
            .expect("a secret list");
        let list = client
            .request::<Value>(request)
            .await
            .expect("Helm release Secrets list");
        let items = list
            .get("items")
            .and_then(Value::as_array)
            .expect("a Secret list");
        assert!(
            items.iter().any(|item| {
                item.pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.starts_with("sh.helm.release.v1.traefik."))
            }),
            "k3s Traefik Helm release Secret must still exist: {items:?}"
        );
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn cnpg_lists_the_lab_cluster_and_never_a_password() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        let fetched = cnpg::fetch(&client, None).await;
        let Fetched::Ok(inventory) = fetched else {
            panic!("a served postgresql.cnpg.io group is Ok, not a failure: {fetched:?}");
        };
        assert!(
            inventory.served(),
            "the lab CRD makes postgresql.cnpg.io served, not absent"
        );
        let page = cnpg::table_page(&inventory).expect("served is a table");
        let shown = format!("{inventory:?}{page:?}");
        assert!(
            !shown.contains("password") || shown.contains("password is never fetched"),
            "a password field reached the CNPG inventory: {shown}"
        );
        let names: Vec<_> = inventory
            .clusters
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            names.contains(&("k10s-hplane", "k10s-hplane-cnpg")),
            "the lab Cluster must list: {names:?}"
        );
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn velero_lists_the_lab_backup_and_never_a_credential() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        let fetched = velero::fetch(&client, None).await;
        let Fetched::Ok(inventory) = fetched else {
            panic!("a served velero.io group is Ok, not a failure: {fetched:?}");
        };
        assert!(
            inventory.served(),
            "the lab CRD makes velero.io served, not absent"
        );
        let page = velero::table_page(&inventory).expect("served is a table");
        let shown = format!("{inventory:?}{page:?}");
        assert!(
            !shown.contains("AKIA") && !shown.contains("secretAccessKey"),
            "a credential reached the Velero inventory: {shown}"
        );
        let names: Vec<_> = inventory
            .backups
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            names.contains(&("k10s-hplane", "k10s-hplane-backup")),
            "the lab Backup must list: {names:?}"
        );
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn fixture_inventories_list_lab_objects_and_never_a_secret() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;

        let cilium = ok(cilium::fetch(&client, None).await, "cilium");
        assert!(cilium.served(), "the lab CRD makes cilium.io served");
        let cnp_names: Vec<_> = cilium
            .network_policies
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            cnp_names.contains(&("k10s-hplane", "k10s-hplane-cnp")),
            "the lab CiliumNetworkPolicy must list: {cnp_names:?}"
        );

        let kyverno = ok(kyverno::fetch(&client, None).await, "kyverno");
        assert!(kyverno.served(), "the lab CRD makes kyverno.io served");
        let cluster_names: Vec<_> = kyverno
            .cluster_policies
            .items()
            .iter()
            .map(|item| item.name.as_str())
            .collect();
        assert!(
            cluster_names.contains(&"k10s-hplane-clusterpolicy"),
            "the lab ClusterPolicy must list: {cluster_names:?}"
        );
        let policy_names: Vec<_> = kyverno
            .policies
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            policy_names.contains(&("k10s-hplane", "k10s-hplane-policy")),
            "the lab Policy must list: {policy_names:?}"
        );

        let eso = ok(eso::fetch(&client, None).await, "eso");
        assert!(eso.served(), "the lab CRD makes external-secrets.io served");
        let stores: Vec<_> = eso
            .secret_stores
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            stores.contains(&("k10s-hplane", "k10s-hplane-store")),
            "the lab SecretStore must list: {stores:?}"
        );
        let externals: Vec<_> = eso
            .external_secrets
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            externals.contains(&("k10s-hplane", "k10s-hplane-external")),
            "the lab ExternalSecret must list: {externals:?}"
        );

        let kargo = ok(kargo::fetch(&client, None).await, "kargo");
        assert!(kargo.served(), "the lab CRD makes kargo.akuity.io served");
        let stages: Vec<_> = kargo
            .stages
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            stages.contains(&("k10s-hplane", "k10s-hplane-stage")),
            "the lab Stage must list: {stages:?}"
        );

        let vault = ok(vault::fetch(&client, None).await, "vault");
        assert!(
            vault.served(),
            "the lab CRD makes secrets.hashicorp.com served"
        );
        let connections: Vec<_> = vault
            .connections
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            connections.contains(&("k10s-hplane", "k10s-hplane-vault")),
            "the lab VaultConnection must list: {connections:?}"
        );

        let otel = ok(otel::fetch(&client, None).await, "otel");
        assert!(otel.served(), "the lab CRD makes opentelemetry.io served");
        let collectors: Vec<_> = otel
            .collectors
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            collectors.contains(&("k10s-hplane", "k10s-hplane-otel")),
            "the lab OpenTelemetryCollector must list: {collectors:?}"
        );

        let shown = format!("{cilium:?}{kyverno:?}{eso:?}{kargo:?}{vault:?}{otel:?}");
        assert!(
            !shown.contains("PLANTED")
                && !shown.contains("password")
                && !shown.contains("tokenSecret")
                && !shown.contains("AKIA"),
            "a secret reached an H-plane inventory: {shown}"
        );
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn core_and_absent_families_stay_labelled() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;

        let gateway = ok(gateway::fetch(&client, None).await, "gateway");
        assert!(
            gateway.served,
            "k3s Traefik already serves gateway.networking.k8s.io"
        );

        let ingress = ok(ingress::fetch(&client, None).await, "ingress");
        assert!(
            ingress.classes.iter().any(|class| class.name == "traefik"),
            "k3s names the default IngressClass traefik: {:?}",
            ingress.classes
        );

        let traefik = ok(traefik::fetch(&client, None).await, "traefik");
        let _ = traefik.served();

        let falco = ok(falco::fetch(&client, None).await, "falco");
        assert!(!falco.served(), "Falco was not installed on this lab");

        let tetragon = ok(tetragon::fetch(&client, None).await, "tetragon");
        assert!(!tetragon.served(), "Tetragon was not installed on this lab");

        let proxies = ok(proxies::fetch(&client, None).await, "proxies");
        assert!(
            !proxies.served(),
            "Contour/Kong/NGINX CRDs were not installed on this lab"
        );
    });
}

fn ok<T>(fetched: Fetched<T>, what: &str) -> T {
    match fetched {
        Fetched::Ok(value) => value,
        Fetched::Denied { what: denied } => {
            panic!("{what} must not be Denied on this lab account: {denied}")
        }
        Fetched::Failed { what: failed, why } => {
            panic!("{what} 404 is absence, not Failed ({failed}: {why})")
        }
    }
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn gateway_lists_the_lab_chain_and_never_a_planted_canary() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        let gateway = ok(gateway::fetch(&client, None).await, "gateway");
        assert!(
            gateway.served,
            "k3s Traefik serves gateway.networking.k8s.io"
        );
        let of = |set: &k10s_data::gateway::KindSet| -> Vec<(String, String)> {
            set.items()
                .iter()
                .map(|item| (item.namespace.clone(), item.name.clone()))
                .collect()
        };
        assert!(
            of(&gateway.gateway_classes).contains(&(String::new(), "k10s-lab".to_string())),
            "the lab GatewayClass must list: {:?}",
            of(&gateway.gateway_classes)
        );
        assert!(
            of(&gateway.gateways)
                .contains(&("k10s-hplane".to_string(), "k10s-hplane-gw".to_string())),
            "the lab Gateway must list: {:?}",
            of(&gateway.gateways)
        );
        assert!(
            of(&gateway.http_routes)
                .contains(&("k10s-hplane".to_string(), "k10s-hplane-route".to_string())),
            "the lab HTTPRoute must list: {:?}",
            of(&gateway.http_routes)
        );
        assert!(
            of(&gateway.listener_sets).contains(&(
                "k10s-hplane".to_string(),
                "k10s-hplane-listeners".to_string()
            )),
            "the lab ListenerSet must list: {:?}",
            of(&gateway.listener_sets)
        );
        assert!(
            of(&gateway.reference_grants)
                .contains(&("k10s-hplane".to_string(), "k10s-hplane-grant".to_string())),
            "the lab ReferenceGrant must list: {:?}",
            of(&gateway.reference_grants)
        );
        let page = gateway::table_page(&gateway).expect("served is a table");
        let shown = format!("{gateway:?}{page:?}");
        assert!(
            !shown.contains("PLANTED"),
            "an annotation canary reached the Gateway inventory: {shown}"
        );
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn ingress_lists_the_lab_ingress_and_never_fetches_its_tls_secret() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        let ingress = ok(ingress::fetch(&client, None).await, "ingress");
        let lab = ingress
            .ingresses
            .iter()
            .find(|item| item.namespace == "k10s-hplane" && item.name == "k10s-hplane-ingress")
            .expect("the lab Ingress must list");
        assert_eq!(
            lab.tls_secrets,
            vec!["k10s-hplane-tls-canary".to_string()],
            "the TLS entry names its Secret; the name is not the credential"
        );
        let page = ingress::table_page(&ingress).expect("core kinds always have a table");
        let shown = format!("{ingress:?}{page:?}");
        assert!(
            !shown.contains("PLANTED") && !shown.contains("UExBTlRFRC"),
            "the lab TLS Secret's planted bytes reached the Ingress inventory: {shown}"
        );
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn traefik_lists_the_lab_route_and_middleware_and_never_a_planted_canary() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        let traefik = ok(traefik::fetch(&client, None).await, "traefik");
        assert!(traefik.served(), "k3s serves traefik.io");
        let routes: Vec<_> = traefik
            .ingress_routes
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            routes.contains(&("k10s-hplane", "hplane-dns")),
            "the lab IngressRoute must list: {routes:?}"
        );
        let middlewares: Vec<_> = traefik
            .middlewares
            .items()
            .iter()
            .map(|item| (item.namespace.as_str(), item.name.as_str()))
            .collect();
        assert!(
            middlewares.contains(&("k10s-hplane", "k10s-hplane-mw")),
            "the lab Middleware must list: {middlewares:?}"
        );
        let page = traefik::table_page(&traefik).expect("served is a table");
        let shown = format!("{traefik:?}{page:?}");
        assert!(
            !shown.contains("PLANTED"),
            "an annotation canary reached the Traefik inventory: {shown}"
        );
    });
}

#[test]
#[ignore = "needs a live cluster; see the module comment"]
fn node_is_ready() {
    let runtime = kube_runtime();
    runtime.block_on(async {
        let client = connect().await;
        let request = Request::new("/api/v1/nodes")
            .list(&ListParams::default())
            .expect("a node list");
        let list = client.request::<Value>(request).await.expect("nodes list");
        let items = list
            .get("items")
            .and_then(Value::as_array)
            .expect("a Node list");
        assert!(!items.is_empty(), "the lab has a node");
        let ready = items.iter().any(|node| {
            node.pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|condition| {
                    condition.get("type").and_then(Value::as_str) == Some("Ready")
                        && condition.get("status").and_then(Value::as_str) == Some("True")
                })
        });
        assert!(ready, "a lab node must be Ready: {items:?}");
    });
}
