//! A failed decode is a visible inventory result, including beside readable rows.

use k10s_data::browse::TablePage;
use k10s_data::{
    alertmanager, cilium, cilium_control, cnpg, eso, flux, gateway, helm, kargo, kyverno, tetragon,
    traefik, vault, velero,
};

fn check_unreadable(page: TablePage, kind: &str, count: usize) {
    assert!(
        !page.truncated,
        "a decode failure is distinct from a page ceiling"
    );
    if count == 0 {
        assert!(page.rows.is_empty(), "a quiet inventory needs no warning");
        return;
    }
    assert_eq!(page.rows.len(), 1, "{kind}: {page:?}");
    let row = &page.rows[0];
    assert_eq!(row.cells.len(), page.columns.len());
    assert_eq!(row.cells[0], kind);
    assert_eq!(row.name, kind);
    assert_eq!(row.namespace, None);
    assert_eq!(row.uid, format!("unreadable:{kind}"));
    let message = format!(
        "{count} {} could not be decoded",
        if count == 1 { "object" } else { "objects" }
    );
    assert!(row.cells.contains(&message), "{kind}: {row:?}");
}

macro_rules! kind_table {
    ($test:ident, $module:ident, $field:ident, $kind:literal $(, $extra:ident: $value:expr)*) => {
        #[test]
        fn $test() {
            for count in [0, 1, 3] {
                let inventory = $module::Inventory {
                    $field: $module::KindSet::Served {
                        items: Vec::new(),
                        truncated: false,
                        unreadable: count,
                    },
                    $($extra: $value,)*
                    ..Default::default()
                };
                let page = $module::table_page(&inventory).expect("the kind is served");
                check_unreadable(page, $kind, count);
            }
        }
    };
}

kind_table!(
    flux_keeps_decode_failures,
    flux,
    git_repositories,
    "GitRepository"
);
kind_table!(velero_keeps_decode_failures, velero, backups, "Backup");
kind_table!(cnpg_keeps_decode_failures, cnpg, clusters, "Cluster");
kind_table!(eso_keeps_decode_failures, eso, secret_stores, "SecretStore");
kind_table!(
    vault_keeps_decode_failures,
    vault,
    connections,
    "VaultConnection"
);
kind_table!(kargo_keeps_decode_failures, kargo, stages, "Stage");
kind_table!(
    kyverno_keeps_decode_failures,
    kyverno,
    cluster_policies,
    "ClusterPolicy"
);
kind_table!(traefik_keeps_decode_failures, traefik, ingress_routes, "IngressRoute",
    group: traefik::GroupState::Served);
kind_table!(gateway_keeps_decode_failures, gateway, gateways, "Gateway", served: true);
kind_table!(cilium_control_keeps_decode_failures, cilium_control, envoy_configs,
    "CiliumEnvoyConfig", group: cilium_control::GroupState::Served);
kind_table!(
    tetragon_keeps_policy_decode_failures,
    tetragon,
    tracing_policies,
    "TracingPolicy"
);
kind_table!(
    tetragon_keeps_pod_info_decode_failures,
    tetragon,
    pod_infos,
    "PodInfo"
);

#[test]
fn cilium_keeps_decode_failures() {
    for count in [0, 1, 3] {
        let inventory = cilium::Inventory {
            network_policies: cilium::KindSet::Served {
                items: Vec::new(),
                truncated: false,
                labels_clipped: false,
                unreadable: count,
            },
            ..Default::default()
        };
        check_unreadable(
            cilium::table_page(&inventory).expect("the kind is served"),
            "CiliumNetworkPolicy",
            count,
        );
    }
}

#[test]
fn helm_keeps_release_decode_failures() {
    for count in [0, 1, 3] {
        let releases = helm::Releases {
            unreadable: count,
            ..Default::default()
        };
        check_unreadable(helm::table_page(&releases), "Helm release Secret", count);
    }
}

#[test]
fn alertmanager_names_the_omitted_count_without_inventing_a_decode_count() {
    for count in [0, 1, 3] {
        let alerts = alertmanager::Alerts {
            dropped: count,
            truncated: count > 0,
            ..Default::default()
        };
        let page = alertmanager::table_page(Some(&alerts)).expect("Alertmanager answered");
        assert_eq!(page.truncated, count > 0);
        if count == 0 {
            assert!(page.rows.is_empty());
            continue;
        }
        assert_eq!(page.rows.len(), 1, "{page:?}");
        let row = &page.rows[0];
        assert_eq!(row.cells.len(), page.columns.len());
        assert_eq!(row.uid, "omitted:alerts");
        assert_eq!(row.namespace, None);
        assert!(
            row.cells.contains(&format!(
                "{count} {} not shown (unreadable or beyond the limit)",
                if count == 1 { "alert" } else { "alerts" }
            )),
            "{row:?}"
        );
    }
}

#[test]
fn alert_decode_failures_do_not_claim_the_page_ceiling_was_hit() {
    for count in [1, alertmanager::MAX_ALERTS + 1] {
        let bytes = serde_json::to_vec(&vec![serde_json::Value::Null; count]).unwrap();
        let alerts = alertmanager::parse_alerts(&bytes).expect("an array is a readable response");
        assert!(alerts.items.is_empty());
        assert_eq!(alerts.dropped, count);
        assert_eq!(alerts.truncated, count > alertmanager::MAX_ALERTS);
        let page = alertmanager::table_page(Some(&alerts)).expect("the response is served");
        assert_eq!(page.rows.len(), 1, "the failed decode remains visible");
        assert_eq!(page.truncated, count > alertmanager::MAX_ALERTS);
    }
}

#[test]
fn readable_objects_keep_their_identity_beside_a_decode_failure() {
    let inventory = cnpg::Inventory {
        clusters: cnpg::KindSet::Served {
            items: vec![cnpg::Resource {
                kind: cnpg::Kind::Cluster,
                version: "v1".to_string(),
                name: "database".to_string(),
                namespace: "prod".to_string(),
                uid: "uid-db".to_string(),
                instances: 1,
                ready_instances: 1,
                primary: "database-1".to_string(),
                phase: "Cluster in healthy state".to_string(),
                postgres_version: String::new(),
                superuser_secret: String::new(),
                cluster: String::new(),
                schedule: String::new(),
                pooler_type: String::new(),
            }],
            truncated: true,
            unreadable: 2,
        },
        ..Default::default()
    };
    let page = cnpg::table_page(&inventory).expect("the kind is served");
    assert!(
        page.truncated,
        "the existing page ceiling is also preserved"
    );
    assert_eq!(page.rows.len(), 2, "{page:?}");
    assert_eq!(page.rows[0].uid, "unreadable:Cluster");
    assert_eq!(page.rows[0].cells[3], "2 objects could not be decoded");
    assert_eq!(page.rows[1].uid, "uid-db");
    assert_eq!(page.rows[1].name, "database");
    assert_eq!(page.rows[1].namespace.as_deref(), Some("prod"));
    assert_eq!(page.rows[1].cells[3], "Cluster in healthy state");
}
