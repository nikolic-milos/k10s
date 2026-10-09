//! Discovery-driven inventories of installed tools.

use super::observe::fetch_alertmanager_optional;
use super::{Fetched, Reader};
use crate::browse::TablePage;
use crate::integrations::{
    alertmanager, argo, cilium, cilium_control, cnpg, eso, falco, flux, gateway, harbor, ingress,
    kargo, kyverno, mesh, otel, policy, proxies, tetragon, traefik, vault, velero,
};
use crate::reach;
use crate::reach::{ReachSettings, ToolKind};
use kube::Client;

impl Reader {
    /// Argo Applications already published on the cluster. Absence is
    /// [`argo::Inventory::served`] = false, not an error.
    pub fn fetch_argo(&self, reply: impl FnOnce(Fetched<argo::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        let targets = self.targets.clone();
        self.handle.spawn(async move {
            reply(argo::fetch_inventory(&client, &targets, None).await);
        });
    }

    /// Flux CRs already published on the cluster. Absence is
    /// [`flux::Inventory::served`] = false, not an error.
    pub fn fetch_flux(&self, reply: impl FnOnce(Fetched<flux::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(flux::fetch(&client, None).await);
        });
    }

    /// CloudNativePG CRs already published on the cluster. Absence is
    /// [`cnpg::Inventory::served`] = false, not an error.
    pub fn fetch_cnpg(&self, reply: impl FnOnce(Fetched<cnpg::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(cnpg::fetch(&client, None).await);
        });
    }

    /// Velero CRs already published on the cluster. Absence is
    /// [`velero::Inventory::served`] = false, not an error.
    pub fn fetch_velero(&self, reply: impl FnOnce(Fetched<velero::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(velero::fetch(&client, None).await);
        });
    }

    pub fn fetch_cilium(&self, reply: impl FnOnce(Fetched<cilium::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(cilium::fetch(&client, None).await);
        });
    }

    pub fn fetch_cilium_control(
        &self,
        reply: impl FnOnce(Fetched<cilium_control::Inventory>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(cilium_control::fetch(&client, None).await);
        });
    }

    pub fn fetch_falco(&self, reply: impl FnOnce(Fetched<falco::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(falco::fetch(&client, None).await);
        });
    }

    pub fn fetch_tetragon(
        &self,
        reply: impl FnOnce(Fetched<tetragon::Inventory>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(tetragon::fetch(&client, None).await);
        });
    }

    pub fn fetch_traefik(&self, reply: impl FnOnce(Fetched<traefik::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(traefik::fetch(&client, None).await);
        });
    }

    pub fn fetch_gateway(&self, reply: impl FnOnce(Fetched<gateway::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(gateway::fetch(&client, None).await);
        });
    }

    pub fn fetch_ingress(&self, reply: impl FnOnce(Fetched<ingress::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(ingress::fetch(&client, None).await);
        });
    }

    pub fn fetch_proxies(&self, reply: impl FnOnce(Fetched<proxies::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(proxies::fetch(&client, None).await);
        });
    }

    pub fn fetch_kyverno(&self, reply: impl FnOnce(Fetched<kyverno::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(kyverno::fetch(&client, None).await);
        });
    }

    pub fn fetch_eso(&self, reply: impl FnOnce(Fetched<eso::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(eso::fetch(&client, None).await);
        });
    }

    pub fn fetch_vault(&self, reply: impl FnOnce(Fetched<vault::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(vault::fetch(&client, None).await);
        });
    }

    pub fn fetch_kargo(&self, reply: impl FnOnce(Fetched<kargo::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(kargo::fetch(&client, None).await);
        });
    }

    pub fn fetch_otel(&self, reply: impl FnOnce(Fetched<otel::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(otel::fetch_inventory(&client, None).await);
        });
    }

    /// The Collector document reads only safe metadata and observed process
    /// health, with the same admission and elapsed deadline as Inspector reads.
    pub fn fetch_collectors(
        &self,
        reply: impl FnOnce(Fetched<Option<TablePage>>) + Send + 'static,
    ) -> crate::telemetry::RequestStop {
        let client = self.interactive.clone();
        let deadline = tokio::time::Instant::now() + crate::connect::INTERACTIVE_DEADLINE;
        self.telemetry.submit_until(
            &self.handle,
            deadline,
            async move {
                match otel::fetch_inventory(&client, None).await {
                    Fetched::Ok(inventory) => Fetched::Ok(otel::table_page(&inventory)),
                    Fetched::Denied { what } => Fetched::Denied { what },
                    Fetched::Failed { what, why } => Fetched::Failed { what, why },
                }
            },
            reply,
        )
    }

    /// Every ecosystem family, fetched concurrently and reduced to its own
    /// table. A family whose answer is [`Fetched::Ok`]`(None)` is not on
    /// this cluster and stays hidden; Denied and Failed stay labelled per
    /// family so one broken adapter cannot hide the other fifteen.
    pub fn fetch_ecosystem(&self, reply: impl FnOnce(Vec<EcosystemFamily>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(ecosystem_families(&client).await);
        });
    }

    pub fn fetch_policy_reports(
        &self,
        reply: impl FnOnce(Fetched<policy::Inventory>) + Send + 'static,
    ) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(policy::fetch_reports(&client).await);
        });
    }

    pub fn fetch_harbor(&self, reply: impl FnOnce(Fetched<harbor::Inventory>) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            let reach = reach::bind(&client, ToolKind::Harbor, &ReachSettings::default()).await;
            reply(harbor::fetch(&client, &reach).await);
        });
    }

    pub fn fetch_mesh_declared(&self, reply: impl FnOnce(mesh::MeshInventory) + Send + 'static) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            reply(mesh::inventory(&client).await);
        });
    }
}
/// One ecosystem family reduced to its table. The id is the stable key a
/// shell surface joins presentation onto; nothing here names a pane.
#[derive(Debug, Clone, PartialEq)]
pub struct EcosystemFamily {
    pub id: &'static str,
    pub answer: Fetched<Option<TablePage>>,
}

async fn ecosystem_families(client: &Client) -> Vec<EcosystemFamily> {
    macro_rules! family {
        ($id:literal, $module:ident) => {
            async {
                EcosystemFamily {
                    id: $id,
                    answer: match $module::fetch(client, None).await {
                        Fetched::Ok(inventory) => Fetched::Ok($module::table_page(&inventory)),
                        Fetched::Denied { what } => Fetched::Denied { what },
                        Fetched::Failed { what, why } => Fetched::Failed { what, why },
                    },
                }
            }
        };
    }
    let collectors = async {
        EcosystemFamily {
            id: "otel",
            answer: match otel::fetch_inventory(client, None).await {
                Fetched::Ok(inventory) => Fetched::Ok(otel::table_page(&inventory)),
                Fetched::Denied { what } => Fetched::Denied { what },
                Fetched::Failed { what, why } => Fetched::Failed { what, why },
            },
        }
    };
    let alertmanager = async {
        EcosystemFamily {
            id: "alertmanager",
            answer: match fetch_alertmanager_optional(client).await {
                Fetched::Ok(alerts) => Fetched::Ok(alertmanager::table_page(alerts.as_ref())),
                Fetched::Denied { what } => Fetched::Denied { what },
                Fetched::Failed { what, why } => Fetched::Failed { what, why },
            },
        }
    };
    let families = tokio::join!(
        family!("cilium", cilium),
        family!("cilium-control", cilium_control),
        family!("tetragon", tetragon),
        family!("falco", falco),
        family!("traefik", traefik),
        family!("gateway", gateway),
        family!("ingress", ingress),
        family!("proxies", proxies),
        family!("kyverno", kyverno),
        family!("eso", eso),
        family!("vault", vault),
        family!("velero", velero),
        family!("cnpg", cnpg),
        family!("kargo", kargo),
        collectors,
        alertmanager,
    );
    let (
        cilium,
        cilium_control,
        tetragon,
        falco,
        traefik,
        gateway,
        ingress,
        proxies,
        kyverno,
        eso,
        vault,
        velero,
        cnpg,
        kargo,
        otel,
        alertmanager,
    ) = families;
    vec![
        cilium,
        cilium_control,
        tetragon,
        falco,
        traefik,
        gateway,
        ingress,
        proxies,
        kyverno,
        eso,
        vault,
        velero,
        cnpg,
        kargo,
        otel,
        alertmanager,
    ]
}
