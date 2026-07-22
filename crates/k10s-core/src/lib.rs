pub mod layout;

use std::sync::Arc;

use arc_swap::ArcSwap;

pub use k10s_atlas::{BlockNode, CellNode, Edge, Rect, RegionNode, Scene, Totals};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Health {
    Ok,
    Warn,
    Err,
    Unknown,
}

impl Health {
    pub fn is_unhealthy(self) -> bool {
        matches!(self, Health::Warn | Health::Err)
    }

    pub fn severity(self) -> u8 {
        match self {
            Health::Ok => 0,
            Health::Unknown => 1,
            Health::Warn => 2,
            Health::Err => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    Deployment,
    StatefulSet,
    DaemonSet,
    Job,
}

impl WorkloadKind {
    pub fn short(&self) -> &'static str {
        match self {
            WorkloadKind::Deployment => "deploy",
            WorkloadKind::StatefulSet => "sts",
            WorkloadKind::DaemonSet => "ds",
            WorkloadKind::Job => "job",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    None,
    Airflow,
    ArgoCd,
    Cassandra,
    ClickHouse,
    Consul,
    Elasticsearch,
    Envoy,
    Etcd,
    FluentBit,
    Fluentd,
    Flux,
    Grafana,
    Harbor,
    Istio,
    Jaeger,
    Jenkins,
    Kafka,
    Keycloak,
    Kibana,
    Kubernetes,
    MariaDb,
    Minio,
    MongoDb,
    MySql,
    Nats,
    Nginx,
    OpenTelemetry,
    Postgres,
    Prometheus,
    RabbitMq,
    Redis,
    Temporal,
    Traefik,
    Vault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SatKind {
    Volume,
    Service,
    ConfigMap,
    Secret,
}

impl SatKind {
    pub fn short(&self) -> &'static str {
        match self {
            SatKind::Volume => "pvc",
            SatKind::Service => "svc",
            SatKind::ConfigMap => "cm",
            SatKind::Secret => "secret",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NsExt {
    pub unhealthy_frac: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct WlExt {
    pub kind: WorkloadKind,
    pub tool: Tool,
    pub health: Health,
    pub ns: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct PodExt {
    pub health: Health,
}

#[derive(Debug, Clone)]
pub struct SatExt {
    pub kind: SatKind,
    pub detail: Arc<str>,
}

pub type NsNode = RegionNode<NsExt>;
pub type WorkloadNode = BlockNode<WlExt>;
pub type PodNode = CellNode<PodExt>;
pub type SatNode = CellNode<SatExt>;

pub type EdgeInst = Edge;

pub type SceneSnapshot = Scene<NsExt, WlExt, PodExt, SatExt>;

pub type SharedScene = Arc<ArcSwap<SceneSnapshot>>;

pub fn new_shared_scene() -> SharedScene {
    Arc::new(ArcSwap::from_pointee(SceneSnapshot::default()))
}

#[derive(Debug, Clone, Copy)]
pub enum WorldCtrl {
    SetChurn(bool),
    Shutdown,
}
