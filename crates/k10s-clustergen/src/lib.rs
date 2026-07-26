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
    pub target_objects: u32,
    pub scenario: Scenario,
}

#[derive(Debug, Clone)]
pub struct PodSpec {
    pub name: String,
    pub state: State,
}

#[derive(Debug, Clone)]
pub struct SatSpec {
    pub name: String,
    pub kind: KindId,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct WorkloadSpec {
    pub name: String,
    pub kind: KindId,
    pub tool: ToolId,
    pub pods: Vec<PodSpec>,
    pub sats: Vec<SatSpec>,
    pub deps: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct NsSpec {
    pub name: String,
    pub workloads: Vec<WorkloadSpec>,
}

#[derive(Debug, Default, Clone)]
pub struct ClusterSpec {
    pub namespaces: Vec<NsSpec>,
    /// Links between workloads in *different* namespaces, as pairs of global
    /// workload indices (namespace order, then workload order within it).
    ///
    /// Separate from `WorkloadSpec::deps`, which is namespace-local by
    /// construction and so could never express a cross-namespace edge. Without
    /// these the scene's `cross_edges` range is permanently empty and the
    /// culler's cross-region path is dead code.
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
// Terse column values for the archetype tables below. Associated constants cannot
// be brought into scope with `use`, so these are local aliases.
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

/// One draw against the same thresholds as before, so generated output is
/// unchanged. What is new is that the severity now travels with the reason that
/// produced it, which is what the closed `Health` enum could not express:
/// `CrashLoopBackOff` is distinct from a generic warning.
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
    // A match on KindId cannot be exhaustive by construction, which is the point:
    // an unrecognised kind takes a middle default instead of failing to compile.
    // No generated output changes, because only the four sampled kinds reach here.
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
                detail: size.to_string(),
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
            detail: detail.to_string(),
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
                detail: format!("{} keys", rng.random_range(2..14u32)),
            });
        }
    }
    if rng.random_bool(profile.secret_p) {
        sats.push(SatSpec {
            name: format!("secret/{name}-creds"),
            kind: KindId::SECRET,
            detail: "opaque".to_string(),
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
        for _ in 0..wl_count {
            let role = ROLE_WORDS[rng.random_range(0..ROLE_WORDS.len())];
            let svc = NS_WORDS[rng.random_range(0..NS_WORDS.len())];
            let name = format!("{svc}-{role}");
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

/// Links a few workloads across namespace boundaries.
///
/// Drawn after every namespace exists, which is also why it cannot perturb the
/// layout: it consumes rng only once all geometry-determining draws are done, so
/// the committed layout fingerprints are unaffected.
fn gen_cross_deps(rng: &mut ChaCha8Rng, spec: &mut ClusterSpec) {
    let ns_count = spec.namespaces.len();
    if ns_count < 2 {
        return;
    }
    // Global index of each namespace's first workload.
    let mut ns_first = Vec::with_capacity(ns_count);
    let mut running = 0u32;
    for ns in &spec.namespaces {
        ns_first.push(running);
        running += ns.workloads.len() as u32;
    }
    if running == 0 {
        return;
    }

    // Roughly one link per namespace: enough that the cross range is genuinely
    // exercised at every scale, few enough that it cannot dominate edge counts.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(seed: u64, target_objects: u32) -> GenConfig {
        GenConfig {
            seed,
            target_objects,
            scenario: Scenario::Platform,
        }
    }

    #[test]
    fn deterministic() {
        let a = generate(&cfg(42, 5000));
        let b = generate(&cfg(42, 5000));
        assert_eq!(a.namespaces.len(), b.namespaces.len());
        assert_eq!(a.total_pods, b.total_pods);
        assert_eq!(a.total_sats, b.total_sats);
        assert_eq!(
            a.namespaces[0].workloads[0].name,
            b.namespaces[0].workloads[0].name
        );
        let sat_a = a
            .namespaces
            .iter()
            .flat_map(|n| &n.workloads)
            .find_map(|w| w.sats.first());
        let sat_b = b
            .namespaces
            .iter()
            .flat_map(|n| &n.workloads)
            .find_map(|w| w.sats.first());
        assert_eq!(sat_a.map(|s| &s.name), sat_b.map(|s| &s.name));
    }

    #[test]
    fn hits_target_roughly() {
        let spec = generate(&cfg(7, 50_000));
        let total =
            spec.namespaces.len() as u32 + spec.total_workloads + spec.total_pods + spec.total_sats;
        assert!((50_000..50_600).contains(&total), "total = {total}");
    }

    #[test]
    fn satellite_mix_is_consistent() {
        let spec = generate(&cfg(42, 50_000));
        assert!(spec.total_sats > 0);
        let mut pvc = 0u32;
        let mut sts_pods = 0u32;
        for wl in spec.namespaces.iter().flat_map(|n| &n.workloads) {
            if wl.kind == KindId::STATEFUL_SET {
                sts_pods += wl.pods.len() as u32;
                let vols = wl.sats.iter().filter(|s| s.kind == KindId::VOLUME).count() as u32;
                assert_eq!(vols, wl.pods.len() as u32, "one PVC per sts replica");
                pvc += vols;
            } else {
                assert!(
                    wl.sats.iter().all(|s| s.kind != KindId::VOLUME),
                    "only StatefulSets own PVCs"
                );
            }
        }
        assert_eq!(pvc, sts_pods);
        for sat in spec
            .namespaces
            .iter()
            .flat_map(|n| &n.workloads)
            .flat_map(|w| &w.sats)
        {
            assert!(!sat.name.is_empty() && !sat.detail.is_empty());
        }
    }

    #[test]
    fn scenarios_shape_the_cluster() {
        let platform = generate(&cfg(42, 20_000));
        assert!(
            platform
                .namespaces
                .iter()
                .any(|n| n.name == "observability")
        );
        assert!(platform.namespaces.iter().any(|n| n.name == "databases"));

        let obs = generate(&GenConfig {
            seed: 42,
            target_objects: 20_000,
            scenario: Scenario::Observability,
        });
        assert!(obs.namespaces.iter().any(|n| n.name == "observability"));
        assert!(!obs.namespaces.iter().any(|n| n.name == "streaming"));

        let prom = obs
            .namespaces
            .iter()
            .find(|n| n.name == "observability")
            .and_then(|n| n.workloads.iter().find(|w| w.name == "prometheus-server"))
            .expect("observability runs prometheus-server");
        assert_eq!(prom.tool, ToolId::PROMETHEUS);
        assert_eq!(prom.kind, KindId::STATEFUL_SET);
        let obs2 = generate(&GenConfig {
            seed: 42,
            target_objects: 20_000,
            scenario: Scenario::Observability,
        });
        assert_eq!(obs.total_pods, obs2.total_pods);
    }

    fn fan_cfg(scenario: Scenario, target_objects: u32) -> GenConfig {
        GenConfig {
            seed: 42,
            target_objects,
            scenario,
        }
    }

    fn widest_ns(spec: &ClusterSpec) -> &NsSpec {
        spec.namespaces
            .iter()
            .max_by_key(|n| n.workloads.len())
            .expect("some namespace")
    }

    fn deepest_wl(spec: &ClusterSpec) -> &WorkloadSpec {
        spec.namespaces
            .iter()
            .flat_map(|n| &n.workloads)
            .max_by_key(|w| w.pods.len())
            .expect("some workload")
    }

    #[test]
    fn ns_fan_out_concentrates_workloads_in_one_namespace() {
        let spec = generate(&fan_cfg(Scenario::NsFanOut, 25_000));
        let hot = widest_ns(&spec);
        assert_eq!(hot.name, "monorepo-prod");
        assert!(
            hot.workloads.len() >= 4_000,
            "fan-out degree = {}",
            hot.workloads.len()
        );

        let rest: usize = spec
            .namespaces
            .iter()
            .filter(|n| n.name != hot.name)
            .map(|n| n.workloads.len())
            .max()
            .unwrap_or(0);
        assert!(
            hot.workloads.len() > rest * 20,
            "hot {} vs widest other {rest}",
            hot.workloads.len()
        );
        assert!(
            spec.namespaces.len() <= 16,
            "few namespaces, got {}",
            spec.namespaces.len()
        );
        assert!(
            hot.workloads.iter().all(|w| w.pods.len() <= 3),
            "the budget must buy workload count, not pods"
        );

        let total =
            spec.namespaces.len() as u32 + spec.total_workloads + spec.total_pods + spec.total_sats;
        assert!((25_000..25_600).contains(&total), "total = {total}");
    }

    #[test]
    fn wl_fan_out_concentrates_pods_on_one_workload() {
        let spec = generate(&fan_cfg(Scenario::WlFanOut, 25_000));
        let hot = deepest_wl(&spec);
        assert_eq!(hot.name, "shard-prod-shard");
        assert_eq!(hot.kind, KindId::STATEFUL_SET);
        assert!(
            hot.pods.len() >= 4_000,
            "fan-out degree = {}",
            hot.pods.len()
        );

        let vols = hot.sats.iter().filter(|s| s.kind == KindId::VOLUME).count();
        assert_eq!(vols, hot.pods.len(), "the sat ring fans out with the pods");

        let shard_ns = spec
            .namespaces
            .iter()
            .find(|n| n.name == "shard-prod")
            .expect("shard-prod exists");
        assert!(
            shard_ns.workloads.len() <= FAN_WL_SIBLINGS as usize + 1,
            "few workloads, got {}",
            shard_ns.workloads.len()
        );

        let second = spec
            .namespaces
            .iter()
            .flat_map(|n| &n.workloads)
            .filter(|w| w.name != hot.name)
            .map(|w| w.pods.len())
            .max()
            .unwrap_or(0);
        assert!(hot.pods.len() > second * 20, "hot vs second {second}");

        let total =
            spec.namespaces.len() as u32 + spec.total_workloads + spec.total_pods + spec.total_sats;
        assert!((25_000..25_600).contains(&total), "total = {total}");
    }

    #[test]
    fn fan_out_degree_scales_with_the_object_budget() {
        let mut ns_degrees = Vec::new();
        let mut wl_degrees = Vec::new();
        for target in [2_000u32, 6_000, 25_000] {
            ns_degrees.push(
                widest_ns(&generate(&fan_cfg(Scenario::NsFanOut, target)))
                    .workloads
                    .len(),
            );
            wl_degrees.push(
                deepest_wl(&generate(&fan_cfg(Scenario::WlFanOut, target)))
                    .pods
                    .len(),
            );
        }
        assert!(
            ns_degrees.windows(2).all(|w| w[1] > w[0] * 2),
            "ns degrees {ns_degrees:?}"
        );
        assert!(
            wl_degrees.windows(2).all(|w| w[1] > w[0] * 2),
            "wl degrees {wl_degrees:?}"
        );
    }

    #[test]
    fn fan_out_scenarios_are_deterministic() {
        for scenario in [Scenario::NsFanOut, Scenario::WlFanOut] {
            let a = generate(&fan_cfg(scenario, 12_000));
            let b = generate(&fan_cfg(scenario, 12_000));
            assert_eq!(a.namespaces.len(), b.namespaces.len());
            assert_eq!(a.total_workloads, b.total_workloads);
            assert_eq!(a.total_pods, b.total_pods);
            assert_eq!(a.total_sats, b.total_sats);
            assert_eq!(a.total_edges, b.total_edges);
            assert_eq!(
                widest_ns(&a).workloads.len(),
                widest_ns(&b).workloads.len(),
                "{}",
                scenario.as_str()
            );
            assert_eq!(deepest_wl(&a).pods.len(), deepest_wl(&b).pods.len());
            for (x, y) in a.namespaces.iter().zip(&b.namespaces) {
                assert_eq!(x.name, y.name);
                assert_eq!(x.workloads.len(), y.workloads.len());
            }
            let names_a: Vec<&str> = widest_ns(&a).workloads.iter().map(|w| &*w.name).collect();
            let names_b: Vec<&str> = widest_ns(&b).workloads.iter().map(|w| &*w.name).collect();
            assert_eq!(names_a, names_b);
        }
    }

    #[test]
    fn fan_out_workload_names_stay_unique_inside_the_hot_namespace() {
        for scenario in [Scenario::NsFanOut, Scenario::WlFanOut] {
            let spec = generate(&fan_cfg(scenario, 25_000));
            for ns in &spec.namespaces {
                let mut names: Vec<&str> = ns.workloads.iter().map(|w| &*w.name).collect();
                let count = names.len();
                names.sort_unstable();
                names.dedup();
                if ns.name == "monorepo-prod" || ns.name == "shard-prod" {
                    assert_eq!(names.len(), count, "{} has duplicate names", ns.name);
                }
            }
        }
    }

    #[test]
    fn fan_out_scenarios_round_trip_through_parse() {
        for scenario in [Scenario::NsFanOut, Scenario::WlFanOut] {
            assert_eq!(Scenario::parse(scenario.as_str()), Some(scenario));
        }
        assert_eq!(Scenario::parse("ns-fanout"), Some(Scenario::NsFanOut));
        assert_eq!(Scenario::parse("wl-fanout"), Some(Scenario::WlFanOut));
        assert_eq!(Scenario::parse("fanout"), None);
    }

    #[test]
    fn sts_pods_use_ordinals() {
        let spec = generate(&cfg(42, 20_000));
        let sts = spec
            .namespaces
            .iter()
            .flat_map(|n| &n.workloads)
            .find(|w| w.kind == KindId::STATEFUL_SET)
            .expect("some sts");
        assert!(sts.pods[0].name.ends_with("-0"), "{}", sts.pods[0].name);
    }
}
