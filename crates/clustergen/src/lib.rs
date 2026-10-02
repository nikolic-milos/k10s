//! Deterministic synthetic cluster generation for benchmarks and tests.
//!
//! Same seed, same cluster, on every platform: generation draws from a seeded
//! ChaCha8 stream and nothing here may consult a clock, a HashMap iteration
//! order, or a thread. Scenes stay shaped like clusters people actually run
//! -- names are unique within a namespace, statefulsets use ordinals, fan-out
//! scenarios exist to stress exactly the axis their name says. This crate is
//! a producer of the ingestion contract, so `k10s-world` needs it only as a
//! dev-dependency.

#[cfg(test)]
mod gen_test;

pub mod stream;

use std::sync::{Arc, LazyLock};

use k10s_core::{KindId, ReasonId, State, ToolId};
use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scenario {
    #[default]
    Platform,
    Observability,
    Data,
    NsFanOut,
    WlFanOut,
}

impl Scenario {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "platform" => Some(Scenario::Platform),
            "observability" => Some(Scenario::Observability),
            "data" => Some(Scenario::Data),
            "ns-fanout" => Some(Scenario::NsFanOut),
            "wl-fanout" => Some(Scenario::WlFanOut),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Scenario::Platform => "platform",
            Scenario::Observability => "observability",
            Scenario::Data => "data",
            Scenario::NsFanOut => "ns-fanout",
            Scenario::WlFanOut => "wl-fanout",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GenConfig {
    pub seed: u64,
    /// The object budget, a band rather than an equality: generation stops on
    /// the first workload that crosses it, so a cluster overshoots by at most
    /// one workload and its children.
    pub target_objects: u32,
    pub scenario: Scenario,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PodSpec {
    pub name: String,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SatSpec {
    pub name: String,
    pub kind: KindId,
    pub detail: Arc<str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkloadSpec {
    pub name: String,
    pub kind: KindId,
    pub tool: ToolId,
    pub pods: Vec<PodSpec>,
    pub sats: Vec<SatSpec>,
    pub deps: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NsSpec {
    pub name: String,
    pub workloads: Vec<WorkloadSpec>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ClusterSpec {
    pub namespaces: Vec<NsSpec>,
    pub cross_deps: Vec<(u32, u32)>,
    pub total_workloads: u32,
    pub total_pods: u32,
    pub total_sats: u32,
    pub total_edges: u32,
}

struct Archetype {
    stem: &'static str,
    tool: ToolId,
    kind: KindId,
    replicas: &'static [u32],
    pvc_sizes: &'static [&'static str],
    svc_p: f64,
    cm_p: f64,
    secret_p: f64,
}

const fn arch(
    stem: &'static str,
    tool: ToolId,
    kind: KindId,
    replicas: &'static [u32],
    pvc_sizes: &'static [&'static str],
    svc_p: f64,
    cm_p: f64,
    secret_p: f64,
) -> Archetype {
    Archetype {
        stem,
        tool,
        kind,
        replicas,
        pvc_sizes,
        svc_p,
        cm_p,
        secret_p,
    }
}

use ToolId as T;
const DEP: KindId = KindId::DEPLOYMENT;
const STS: KindId = KindId::STATEFUL_SET;
const DS: KindId = KindId::DAEMON_SET;

const MONITORING: &[Archetype] = &[
    arch(
        "prometheus",
        T::PROMETHEUS,
        STS,
        &[1, 2],
        &["64Gi", "128Gi"],
        0.95,
        0.9,
        0.2,
    ),
    arch(
        "alertmanager",
        T::PROMETHEUS,
        STS,
        &[1, 3],
        &["1Gi"],
        0.9,
        0.9,
        0.1,
    ),
    arch("grafana", T::GRAFANA, DEP, &[1, 2], &[], 0.95, 0.9, 0.8),
    arch(
        "node-exporter",
        T::PROMETHEUS,
        DS,
        &[6, 9, 12],
        &[],
        0.3,
        0.3,
        0.0,
    ),
    arch(
        "kube-state-metrics",
        T::KUBERNETES,
        DEP,
        &[1],
        &[],
        0.9,
        0.1,
        0.0,
    ),
    arch(
        "otel-collector",
        T::OPEN_TELEMETRY,
        DS,
        &[6, 9],
        &[],
        0.5,
        0.9,
        0.1,
    ),
    arch("jaeger", T::JAEGER, DEP, &[1, 2], &[], 0.9, 0.6, 0.1),
];

const LOGGING: &[Archetype] = &[
    arch(
        "elasticsearch",
        T::ELASTICSEARCH,
        STS,
        &[3, 5],
        &["64Gi", "128Gi"],
        0.9,
        0.6,
        0.5,
    ),
    arch("kibana", T::KIBANA, DEP, &[1, 2], &[], 0.9, 0.6, 0.3),
    arch(
        "fluent-bit",
        T::FLUENT_BIT,
        DS,
        &[6, 9, 12],
        &[],
        0.2,
        0.9,
        0.1,
    ),
    arch("fluentd", T::FLUENTD, DS, &[6, 9], &[], 0.2, 0.9, 0.1),
];

const DATA: &[Archetype] = &[
    arch(
        "postgres",
        T::POSTGRES,
        STS,
        &[1, 3],
        &["32Gi", "64Gi", "128Gi"],
        0.9,
        0.5,
        0.9,
    ),
    arch(
        "mariadb",
        T::MARIA_DB,
        STS,
        &[1, 3],
        &["32Gi", "64Gi"],
        0.9,
        0.5,
        0.9,
    ),
    arch("mongodb", T::MONGO_DB, STS, &[3], &["64Gi"], 0.9, 0.5, 0.9),
    arch(
        "redis",
        T::REDIS,
        STS,
        &[1, 3],
        &["8Gi", "16Gi"],
        0.9,
        0.5,
        0.5,
    ),
    arch(
        "clickhouse",
        T::CLICK_HOUSE,
        STS,
        &[2, 4],
        &["128Gi"],
        0.9,
        0.6,
        0.5,
    ),
    arch(
        "cassandra",
        T::CASSANDRA,
        STS,
        &[3, 5],
        &["64Gi"],
        0.9,
        0.6,
        0.5,
    ),
];

const MESSAGING: &[Archetype] = &[
    arch(
        "kafka",
        T::KAFKA,
        STS,
        &[3, 5],
        &["64Gi", "128Gi"],
        0.9,
        0.7,
        0.3,
    ),
    arch(
        "rabbitmq",
        T::RABBIT_MQ,
        STS,
        &[3],
        &["16Gi"],
        0.9,
        0.6,
        0.5,
    ),
    arch("nats", T::NATS, STS, &[3], &["8Gi"], 0.9, 0.5, 0.2),
];

const INGRESS: &[Archetype] = &[
    arch("ingress-nginx", T::NGINX, DEP, &[2, 3], &[], 0.95, 0.8, 0.4),
    arch("traefik", T::TRAEFIK, DEP, &[2], &[], 0.95, 0.8, 0.3),
    arch("istiod", T::ISTIO, DEP, &[1, 2], &[], 0.9, 0.7, 0.3),
    arch("envoy-gateway", T::ENVOY, DEP, &[2], &[], 0.9, 0.7, 0.2),
];

const SECURITY: &[Archetype] = &[
    arch("vault", T::VAULT, STS, &[3], &["8Gi"], 0.9, 0.5, 0.95),
    arch("keycloak", T::KEYCLOAK, STS, &[2], &["8Gi"], 0.9, 0.6, 0.9),
    arch("consul", T::CONSUL, STS, &[3], &["8Gi"], 0.9, 0.6, 0.4),
    arch("etcd", T::ETCD, STS, &[3, 5], &["8Gi"], 0.9, 0.2, 0.5),
];

const CI: &[Archetype] = &[
    arch(
        "jenkins",
        T::JENKINS,
        STS,
        &[1],
        &["32Gi", "64Gi"],
        0.9,
        0.6,
        0.7,
    ),
    arch("argocd", T::ARGO_CD, DEP, &[2], &[], 0.9, 0.8, 0.6),
    arch("flux", T::FLUX, DEP, &[1, 2], &[], 0.5, 0.7, 0.4),
    arch("temporal", T::TEMPORAL, DEP, &[2, 3], &[], 0.9, 0.6, 0.4),
    arch("airflow", T::AIRFLOW, DEP, &[2, 3], &[], 0.9, 0.9, 0.6),
];

const STORAGE: &[Archetype] = &[
    arch("minio", T::MINIO, STS, &[4], &["128Gi"], 0.9, 0.4, 0.8),
    arch("harbor", T::HARBOR, STS, &[1, 2], &["128Gi"], 0.9, 0.6, 0.7),
];

const THEMES: &[(&str, &str, &[Archetype])] = &[
    ("monitoring", "observability", MONITORING),
    ("logging", "logging", LOGGING),
    ("data", "databases", DATA),
    ("messaging", "streaming", MESSAGING),
    ("ingress", "edge", INGRESS),
    ("security", "security", SECURITY),
    ("ci", "cicd", CI),
    ("storage", "registry", STORAGE),
];

const EMBEDS: &[(&Archetype, f64)] = &[
    (&DATA[0], 0.30),
    (&DATA[3], 0.35),
    (&DATA[1], 0.10),
    (&DATA[2], 0.08),
    (&MESSAGING[1], 0.10),
    (&MESSAGING[2], 0.08),
    (&INGRESS[0], 0.15),
    (&MONITORING[6], 0.08),
];

fn scenario_themes(s: Scenario) -> &'static [&'static str] {
    match s {
        Scenario::Platform => &[
            "monitoring",
            "logging",
            "data",
            "messaging",
            "ingress",
            "security",
            "ci",
            "storage",
        ],
        Scenario::Observability => &["monitoring", "logging", "ingress"],
        Scenario::Data => &["data", "messaging", "storage", "monitoring"],
        Scenario::NsFanOut => &["ingress", "security"],
        Scenario::WlFanOut => &["data", "ingress"],
    }
}

fn embed_multiplier(s: Scenario, tool: ToolId) -> f64 {
    match s {
        Scenario::Platform => 1.0,
        Scenario::Observability => match tool {
            T::JAEGER | T::PROMETHEUS | T::OPEN_TELEMETRY => 3.0,
            _ => 0.7,
        },
        Scenario::Data => match tool {
            T::POSTGRES | T::REDIS | T::MARIA_DB | T::MONGO_DB | T::RABBIT_MQ | T::NATS => 2.0,
            _ => 0.5,
        },
        Scenario::NsFanOut | Scenario::WlFanOut => 0.5,
    }
}

enum FanAxis {
    Namespace,
    Workload,
}

struct FanOut {
    ns_name: &'static str,
    axis: FanAxis,
    budget_frac: f64,
}

const FAN_NS_MIN_REPLICAS: u32 = 1;
const FAN_NS_MAX_REPLICAS: u32 = 3;
const FAN_WL_SIBLINGS: u32 = 4;
const FAN_WL_SAT_HEADROOM: u32 = 4;

fn fan_out(s: Scenario) -> Option<FanOut> {
    match s {
        Scenario::Platform | Scenario::Observability | Scenario::Data => None,
        Scenario::NsFanOut => Some(FanOut {
            ns_name: "monorepo-prod",
            axis: FanAxis::Namespace,
            budget_frac: 0.96,
        }),
        Scenario::WlFanOut => Some(FanOut {
            ns_name: "shard-prod",
            axis: FanAxis::Workload,
            budget_frac: 0.40,
        }),
    }
}

const NS_WORDS: &[&str] = &[
    "payments",
    "checkout",
    "auth",
    "billing",
    "search",
    "ingest",
    "storefront",
    "ml",
    "training",
    "media",
    "email",
    "notify",
    "warehouse",
    "analytics",
    "crawler",
    "identity",
    "catalog",
    "orders",
    "shipping",
    "inventory",
    "reco",
    "fraud",
    "support",
    "chat",
    "video",
    "livestream",
    "cdn",
    "backup",
    "webhooks",
    "gateway",
    "ledger",
    "risk",
];

const NS_SUFFIX: &[&str] = &[
    "-prod", "-prod", "-staging", "-dev", "-eu", "-us", "-canary",
];

const ROLE_WORDS: &[&str] = &[
    "api",
    "worker",
    "web",
    "sync",
    "proxy",
    "server",
    "agent",
    "job",
    "cron",
    "exporter",
    "operator",
    "controller",
    "indexer",
    "consumer",
    "producer",
    "scheduler",
    "resolver",
    "router",
    "shard",
    "gateway",
];

const REPLICA_TABLE: &[(u32, u32)] = &[
    (1, 300),
    (2, 200),
    (3, 250),
    (5, 100),
    (8, 60),
    (12, 40),
    (20, 30),
    (50, 15),
    (200, 5),
];

fn sample_replicas(rng: &mut ChaCha8Rng) -> u32 {
    let total: u32 = REPLICA_TABLE.iter().map(|(_, w)| w).sum();
    let mut pick = rng.random_range(0..total);
    for &(count, weight) in REPLICA_TABLE {
        if pick < weight {
            return count;
        }
        pick -= weight;
    }
    1
}

fn sample_kind(rng: &mut ChaCha8Rng) -> KindId {
    match rng.random_range(0..100u32) {
        0..70 => KindId::DEPLOYMENT,
        70..80 => KindId::STATEFUL_SET,
        80..90 => KindId::DAEMON_SET,
        _ => KindId::JOB,
    }
}

fn sample_state(rng: &mut ChaCha8Rng) -> State {
    match rng.random_range(0..1000u32) {
        0..920 => State::of(ReasonId::RUNNING),
        920..960 => State::of(ReasonId::NOT_READY),
        960..990 => State::of(ReasonId::CRASH_LOOP_BACK_OFF),
        _ => State::of(ReasonId::UNKNOWN),
    }
}

fn pod_suffix(rng: &mut ChaCha8Rng) -> String {
    const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(11);
    for i in 0..11 {
        if i == 5 {
            s.push('-');
        } else {
            s.push(ALNUM[rng.random_range(0..ALNUM.len())] as char);
        }
    }
    s
}

const PVC_SIZES: &[(&str, u32)] = &[
    ("1Gi", 10),
    ("8Gi", 25),
    ("16Gi", 30),
    ("32Gi", 20),
    ("64Gi", 10),
    ("128Gi", 5),
];

const DETAIL_TEXT: [&str; 22] = [
    "1Gi",
    "8Gi",
    "16Gi",
    "32Gi",
    "64Gi",
    "128Gi",
    "ClusterIP",
    "NodePort",
    "LoadBalancer",
    "2 keys",
    "3 keys",
    "4 keys",
    "5 keys",
    "6 keys",
    "7 keys",
    "8 keys",
    "9 keys",
    "10 keys",
    "11 keys",
    "12 keys",
    "13 keys",
    "opaque",
];
const KEY_DETAIL_START: usize = 9;
const OPAQUE_DETAIL: usize = 21;

static DETAILS: LazyLock<[Arc<str>; DETAIL_TEXT.len()]> =
    LazyLock::new(|| DETAIL_TEXT.map(Arc::from));

fn shared_detail(text: &str) -> Arc<str> {
    let index = match text {
        "1Gi" => 0,
        "8Gi" => 1,
        "16Gi" => 2,
        "32Gi" => 3,
        "64Gi" => 4,
        "128Gi" => 5,
        "ClusterIP" => 6,
        "NodePort" => 7,
        "LoadBalancer" => 8,
        _ => panic!("the generator requested an undeclared attachment detail {text}"),
    };
    DETAILS[index].clone()
}

fn key_detail(keys: u32) -> Arc<str> {
    let offset = usize::try_from(keys.saturating_sub(2))
        .expect("the key count fits in usize on every supported target");
    DETAILS
        .get(KEY_DETAIL_START + offset)
        .filter(|_| (2..=13).contains(&keys))
        .unwrap_or_else(|| panic!("the generator requested an out-of-range key count {keys}"))
        .clone()
}

fn sample_pvc_size(rng: &mut ChaCha8Rng) -> &'static str {
    let total: u32 = PVC_SIZES.iter().map(|(_, w)| w).sum();
    let mut pick = rng.random_range(0..total);
    for &(size, weight) in PVC_SIZES {
        if pick < weight {
            return size;
        }
        pick -= weight;
    }
    "16Gi"
}

struct SatProfile<'a> {
    pvc_sizes: &'a [&'a str],
    svc_p: f64,
    cm_p: f64,
    secret_p: f64,
}

fn kind_profile(kind: KindId) -> SatProfile<'static> {
    let svc_p = match kind {
        KindId::DEPLOYMENT | KindId::STATEFUL_SET => 0.55,
        KindId::DAEMON_SET | KindId::JOB => 0.08,
        _ => 0.3,
    };
    SatProfile {
        pvc_sizes: &[],
        svc_p,
        cm_p: 0.5,
        secret_p: 0.35,
    }
}

fn gen_sats(
    rng: &mut ChaCha8Rng,
    name: &str,
    kind: KindId,
    replicas: u32,
    profile: &SatProfile,
) -> Vec<SatSpec> {
    let mut sats = Vec::new();
    if kind == KindId::STATEFUL_SET {
        let size = if profile.pvc_sizes.is_empty() {
            sample_pvc_size(rng)
        } else {
            profile.pvc_sizes[rng.random_range(0..profile.pvc_sizes.len())]
        };
        for i in 0..replicas {
            sats.push(SatSpec {
                name: format!("pvc/data-{name}-{i}"),
                kind: KindId::VOLUME,
                detail: shared_detail(size),
            });
        }
    }
    if rng.random_bool(profile.svc_p) {
        let detail = match rng.random_range(0..100u32) {
            0..80 => "ClusterIP",
            80..92 => "NodePort",
            _ => "LoadBalancer",
        };
        sats.push(SatSpec {
            name: format!("svc/{name}"),
            kind: KindId::SERVICE,
            detail: shared_detail(detail),
        });
    }
    if rng.random_bool(profile.cm_p) {
        for i in 0..rng.random_range(1..=2u32) {
            let cm_name = if i == 0 {
                format!("cm/{name}-config")
            } else {
                format!("cm/{name}-config-{}", i + 1)
            };
            sats.push(SatSpec {
                name: cm_name,
                kind: KindId::CONFIG_MAP,
                detail: key_detail(rng.random_range(2..14u32)),
            });
        }
    }
    if rng.random_bool(profile.secret_p) {
        sats.push(SatSpec {
            name: format!("secret/{name}-creds"),
            kind: KindId::SECRET,
            detail: DETAILS[OPAQUE_DETAIL].clone(),
        });
    }
    sats
}

fn gen_pods(rng: &mut ChaCha8Rng, name: &str, kind: KindId, replicas: u32) -> Vec<PodSpec> {
    (0..replicas)
        .map(|i| PodSpec {
            name: if kind == KindId::STATEFUL_SET {
                format!("{name}-{i}")
            } else {
                format!("{name}-{}", pod_suffix(rng))
            },
            state: sample_state(rng),
        })
        .collect()
}

fn theme_workload_name(a: &Archetype) -> String {
    match a.stem {
        "prometheus" => "prometheus-server".into(),
        "alertmanager" => "alertmanager".into(),
        "grafana" => "grafana".into(),
        "node-exporter" => "node-exporter".into(),
        "kube-state-metrics" => "kube-state-metrics".into(),
        "otel-collector" => "otel-collector".into(),
        "jaeger" => "jaeger-query".into(),
        "elasticsearch" => "elasticsearch-master".into(),
        "kibana" => "kibana".into(),
        "fluent-bit" => "fluent-bit".into(),
        "fluentd" => "fluentd".into(),
        "postgres" => "postgres-primary".into(),
        "mariadb" => "mariadb-primary".into(),
        "mongodb" => "mongodb-replica".into(),
        "redis" => "redis-master".into(),
        "clickhouse" => "clickhouse-shard".into(),
        "cassandra" => "cassandra".into(),
        "kafka" => "kafka-broker".into(),
        "rabbitmq" => "rabbitmq".into(),
        "nats" => "nats".into(),
        "ingress-nginx" => "ingress-nginx-controller".into(),
        "traefik" => "traefik".into(),
        "istiod" => "istiod".into(),
        "envoy-gateway" => "envoy-gateway".into(),
        "vault" => "vault".into(),
        "keycloak" => "keycloak".into(),
        "consul" => "consul-server".into(),
        "etcd" => "etcd".into(),
        "jenkins" => "jenkins".into(),
        "argocd" => "argocd-server".into(),
        "flux" => "flux-source-controller".into(),
        "temporal" => "temporal-frontend".into(),
        "airflow" => "airflow-scheduler".into(),
        "minio" => "minio".into(),
        "harbor" => "harbor-core".into(),
        other => other.into(),
    }
}

fn instantiate(rng: &mut ChaCha8Rng, a: &Archetype, name: String) -> WorkloadSpec {
    let replicas = a.replicas[rng.random_range(0..a.replicas.len())];
    let pods = gen_pods(rng, &name, a.kind, replicas);
    let profile = SatProfile {
        pvc_sizes: a.pvc_sizes,
        svc_p: a.svc_p,
        cm_p: a.cm_p,
        secret_p: a.secret_p,
    };
    let sats = gen_sats(rng, &name, a.kind, replicas, &profile);
    WorkloadSpec {
        name,
        kind: a.kind,
        tool: a.tool,
        pods,
        sats,
        deps: Vec::new(),
    }
}

fn plain_workload(rng: &mut ChaCha8Rng, name: String, kind: KindId, replicas: u32) -> WorkloadSpec {
    let pods = gen_pods(rng, &name, kind, replicas);
    let sats = gen_sats(rng, &name, kind, replicas, &kind_profile(kind));
    WorkloadSpec {
        name,
        kind,
        tool: ToolId::NONE,
        pods,
        sats,
        deps: Vec::new(),
    }
}

fn push_workload(
    spec: &mut ClusterSpec,
    objects: &mut u32,
    wl: WorkloadSpec,
    out: &mut Vec<WorkloadSpec>,
) {
    *objects += 1 + wl.pods.len() as u32 + wl.sats.len() as u32;
    spec.total_workloads += 1;
    spec.total_pods += wl.pods.len() as u32;
    spec.total_sats += wl.sats.len() as u32;
    out.push(wl);
}

fn gen_deps(rng: &mut ChaCha8Rng, workloads: &mut [WorkloadSpec], total_edges: &mut u32) {
    let n = workloads.len() as u32;
    if n <= 1 {
        return;
    }
    for i in 0..n {
        if rng.random_bool(0.3) {
            for _ in 0..rng.random_range(1..=3u32) {
                let target = rng.random_range(0..n);
                if target != i && !workloads[i as usize].deps.contains(&target) {
                    workloads[i as usize].deps.push(target);
                    *total_edges += 1;
                }
            }
        }
    }
}

fn gen_fan_out(
    rng: &mut ChaCha8Rng,
    cfg: &GenConfig,
    fan: &FanOut,
    spec: &mut ClusterSpec,
    objects: &mut u32,
) {
    let budget = (cfg.target_objects as f64 * fan.budget_frac) as u32;
    let stop = objects.saturating_add(budget).min(cfg.target_objects);
    let mut workloads = Vec::new();

    match fan.axis {
        FanAxis::Namespace => {
            let mut i = 0u32;
            loop {
                let role = ROLE_WORDS[rng.random_range(0..ROLE_WORDS.len())];
                let svc = NS_WORDS[rng.random_range(0..NS_WORDS.len())];
                let replicas = rng.random_range(FAN_NS_MIN_REPLICAS..=FAN_NS_MAX_REPLICAS);
                let wl = plain_workload(
                    rng,
                    format!("{svc}-{role}-{i}"),
                    KindId::DEPLOYMENT,
                    replicas,
                );
                push_workload(spec, objects, wl, &mut workloads);
                i += 1;
                if *objects >= stop {
                    break;
                }
            }
        }
        FanAxis::Workload => {
            for i in 0..FAN_WL_SIBLINGS {
                let role = ROLE_WORDS[rng.random_range(0..ROLE_WORDS.len())];
                let replicas = rng.random_range(FAN_NS_MIN_REPLICAS..=FAN_NS_MAX_REPLICAS);
                let wl = plain_workload(
                    rng,
                    format!("{}-{role}-{i}", fan.ns_name),
                    KindId::DEPLOYMENT,
                    replicas,
                );
                push_workload(spec, objects, wl, &mut workloads);
                if *objects >= stop {
                    break;
                }
            }
            let left = stop
                .saturating_sub(*objects)
                .saturating_sub(FAN_WL_SAT_HEADROOM);
            let wl = plain_workload(
                rng,
                format!("{}-shard", fan.ns_name),
                KindId::STATEFUL_SET,
                (left / 2).max(1),
            );
            push_workload(spec, objects, wl, &mut workloads);
        }
    }

    gen_deps(rng, &mut workloads, &mut spec.total_edges);
    *objects += 1;
    spec.namespaces.push(NsSpec {
        name: fan.ns_name.to_string(),
        workloads,
    });
}

pub fn generate(cfg: &GenConfig) -> ClusterSpec {
    let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
    let mut spec = ClusterSpec::default();
    let mut objects: u32 = 0;

    for &(theme_id, ns_label, archetypes) in THEMES {
        if !scenario_themes(cfg.scenario).contains(&theme_id) || objects >= cfg.target_objects {
            continue;
        }
        let mut workloads = Vec::with_capacity(archetypes.len());
        for a in archetypes {
            let wl = instantiate(&mut rng, a, theme_workload_name(a));
            push_workload(&mut spec, &mut objects, wl, &mut workloads);
        }
        gen_deps(&mut rng, &mut workloads, &mut spec.total_edges);
        objects += 1;
        spec.namespaces.push(NsSpec {
            name: ns_label.to_string(),
            workloads,
        });
    }

    if let Some(fan) = fan_out(cfg.scenario) {
        gen_fan_out(&mut rng, cfg, &fan, &mut spec, &mut objects);
    }

    let mut ns_i = 0usize;
    while objects < cfg.target_objects {
        let word = NS_WORDS[rng.random_range(0..NS_WORDS.len())];
        let suffix = NS_SUFFIX[rng.random_range(0..NS_SUFFIX.len())];
        let ns_name = if ns_i < NS_WORDS.len() {
            format!("{word}{suffix}")
        } else {
            format!("{word}{suffix}-{ns_i}")
        };

        let wl_count = 2 + (rng.random::<f32>().powi(3) * 60.0) as usize;
        let mut workloads = Vec::with_capacity(wl_count);
        let mut names = std::collections::HashMap::with_capacity(wl_count);
        for _ in 0..wl_count {
            let role_index = rng.random_range(0..ROLE_WORDS.len());
            let service_index = rng.random_range(0..NS_WORDS.len());
            let role = ROLE_WORDS[role_index];
            let svc = NS_WORDS[service_index];
            // A namespace cannot hold two workloads of one name; disambiguate
            // collisions with an ordinal the way people actually do.
            let ordinal = names.entry((service_index, role_index)).or_insert(0usize);
            *ordinal += 1;
            let name = match *ordinal {
                1 => format!("{svc}-{role}"),
                ordinal => format!("{svc}-{role}-{ordinal}"),
            };
            let kind = sample_kind(&mut rng);
            let replicas = match kind {
                KindId::JOB => 1,
                KindId::DAEMON_SET => rng.random_range(3..12),
                _ => sample_replicas(&mut rng),
            };
            let wl = plain_workload(&mut rng, name, kind, replicas);
            push_workload(&mut spec, &mut objects, wl, &mut workloads);

            if objects >= cfg.target_objects {
                break;
            }
        }

        if objects < cfg.target_objects {
            for &(a, p) in EMBEDS {
                let p = (p * embed_multiplier(cfg.scenario, a.tool)).min(0.9);
                if rng.random_bool(p) {
                    let wl = instantiate(&mut rng, a, format!("{word}-{}", a.stem));
                    push_workload(&mut spec, &mut objects, wl, &mut workloads);
                }
            }
        }

        gen_deps(&mut rng, &mut workloads, &mut spec.total_edges);
        objects += 1;
        spec.namespaces.push(NsSpec {
            name: ns_name,
            workloads,
        });
        ns_i += 1;
    }

    gen_cross_deps(&mut rng, &mut spec);

    spec
}

fn gen_cross_deps(rng: &mut ChaCha8Rng, spec: &mut ClusterSpec) {
    let ns_count = spec.namespaces.len();
    if ns_count < 2 {
        return;
    }
    let mut ns_first = Vec::with_capacity(ns_count);
    let mut running = 0u32;
    for ns in &spec.namespaces {
        ns_first.push(running);
        running += ns.workloads.len() as u32;
    }
    if running == 0 {
        return;
    }

    let wanted = ns_count.min(64);
    for _ in 0..wanted {
        let a_ns = rng.random_range(0..ns_count);
        let b_ns = rng.random_range(0..ns_count);
        if a_ns == b_ns {
            continue;
        }
        let (an, bn) = (
            spec.namespaces[a_ns].workloads.len() as u32,
            spec.namespaces[b_ns].workloads.len() as u32,
        );
        if an == 0 || bn == 0 {
            continue;
        }
        let a = ns_first[a_ns] + rng.random_range(0..an);
        let b = ns_first[b_ns] + rng.random_range(0..bn);
        if spec.cross_deps.contains(&(a, b)) {
            continue;
        }
        spec.cross_deps.push((a, b));
        spec.total_edges += 1;
    }
}
