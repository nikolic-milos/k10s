use k10s_core::{Health, SatKind, Tool, WorkloadKind};
use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scenario {
    #[default]
    Platform,
    Observability,
    Data,
}

impl Scenario {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "platform" => Some(Scenario::Platform),
            "observability" => Some(Scenario::Observability),
            "data" => Some(Scenario::Data),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Scenario::Platform => "platform",
            Scenario::Observability => "observability",
            Scenario::Data => "data",
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
    pub health: Health,
}

#[derive(Debug, Clone)]
pub struct SatSpec {
    pub name: String,
    pub kind: SatKind,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct WorkloadSpec {
    pub name: String,
    pub kind: WorkloadKind,
    pub tool: Tool,
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
    pub total_workloads: u32,
    pub total_pods: u32,
    pub total_sats: u32,
    pub total_edges: u32,
}

struct Archetype {
    stem: &'static str,
    tool: Tool,
    kind: WorkloadKind,
    replicas: &'static [u32],
    pvc_sizes: &'static [&'static str],
    svc_p: f64,
    cm_p: f64,
    secret_p: f64,
}

const fn arch(
    stem: &'static str,
    tool: Tool,
    kind: WorkloadKind,
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

use Tool as T;
use WorkloadKind::{DaemonSet as Ds, Deployment as Dep, StatefulSet as Sts};

const MONITORING: &[Archetype] = &[
    arch(
        "prometheus",
        T::Prometheus,
        Sts,
        &[1, 2],
        &["64Gi", "128Gi"],
        0.95,
        0.9,
        0.2,
    ),
    arch(
        "alertmanager",
        T::Prometheus,
        Sts,
        &[1, 3],
        &["1Gi"],
        0.9,
        0.9,
        0.1,
    ),
    arch("grafana", T::Grafana, Dep, &[1, 2], &[], 0.95, 0.9, 0.8),
    arch(
        "node-exporter",
        T::Prometheus,
        Ds,
        &[6, 9, 12],
        &[],
        0.3,
        0.3,
        0.0,
    ),
    arch(
        "kube-state-metrics",
        T::Kubernetes,
        Dep,
        &[1],
        &[],
        0.9,
        0.1,
        0.0,
    ),
    arch(
        "otel-collector",
        T::OpenTelemetry,
        Ds,
        &[6, 9],
        &[],
        0.5,
        0.9,
        0.1,
    ),
    arch("jaeger", T::Jaeger, Dep, &[1, 2], &[], 0.9, 0.6, 0.1),
];

const LOGGING: &[Archetype] = &[
    arch(
        "elasticsearch",
        T::Elasticsearch,
        Sts,
        &[3, 5],
        &["64Gi", "128Gi"],
        0.9,
        0.6,
        0.5,
    ),
    arch("kibana", T::Kibana, Dep, &[1, 2], &[], 0.9, 0.6, 0.3),
    arch(
        "fluent-bit",
        T::FluentBit,
        Ds,
        &[6, 9, 12],
        &[],
        0.2,
        0.9,
        0.1,
    ),
    arch("fluentd", T::Fluentd, Ds, &[6, 9], &[], 0.2, 0.9, 0.1),
];

const DATA: &[Archetype] = &[
    arch(
        "postgres",
        T::Postgres,
        Sts,
        &[1, 3],
        &["32Gi", "64Gi", "128Gi"],
        0.9,
        0.5,
        0.9,
    ),
    arch(
        "mariadb",
        T::MariaDb,
        Sts,
        &[1, 3],
        &["32Gi", "64Gi"],
        0.9,
        0.5,
        0.9,
    ),
    arch("mongodb", T::MongoDb, Sts, &[3], &["64Gi"], 0.9, 0.5, 0.9),
    arch(
        "redis",
        T::Redis,
        Sts,
        &[1, 3],
        &["8Gi", "16Gi"],
        0.9,
        0.5,
        0.5,
    ),
    arch(
        "clickhouse",
        T::ClickHouse,
        Sts,
        &[2, 4],
        &["128Gi"],
        0.9,
        0.6,
        0.5,
    ),
    arch(
        "cassandra",
        T::Cassandra,
        Sts,
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
        T::Kafka,
        Sts,
        &[3, 5],
        &["64Gi", "128Gi"],
        0.9,
        0.7,
        0.3,
    ),
    arch("rabbitmq", T::RabbitMq, Sts, &[3], &["16Gi"], 0.9, 0.6, 0.5),
    arch("nats", T::Nats, Sts, &[3], &["8Gi"], 0.9, 0.5, 0.2),
];

const INGRESS: &[Archetype] = &[
    arch("ingress-nginx", T::Nginx, Dep, &[2, 3], &[], 0.95, 0.8, 0.4),
    arch("traefik", T::Traefik, Dep, &[2], &[], 0.95, 0.8, 0.3),
    arch("istiod", T::Istio, Dep, &[1, 2], &[], 0.9, 0.7, 0.3),
    arch("envoy-gateway", T::Envoy, Dep, &[2], &[], 0.9, 0.7, 0.2),
];

const SECURITY: &[Archetype] = &[
    arch("vault", T::Vault, Sts, &[3], &["8Gi"], 0.9, 0.5, 0.95),
    arch("keycloak", T::Keycloak, Sts, &[2], &["8Gi"], 0.9, 0.6, 0.9),
    arch("consul", T::Consul, Sts, &[3], &["8Gi"], 0.9, 0.6, 0.4),
    arch("etcd", T::Etcd, Sts, &[3, 5], &["8Gi"], 0.9, 0.2, 0.5),
];

const CI: &[Archetype] = &[
    arch(
        "jenkins",
        T::Jenkins,
        Sts,
        &[1],
        &["32Gi", "64Gi"],
        0.9,
        0.6,
        0.7,
    ),
    arch("argocd", T::ArgoCd, Dep, &[2], &[], 0.9, 0.8, 0.6),
    arch("flux", T::Flux, Dep, &[1, 2], &[], 0.5, 0.7, 0.4),
    arch("temporal", T::Temporal, Dep, &[2, 3], &[], 0.9, 0.6, 0.4),
    arch("airflow", T::Airflow, Dep, &[2, 3], &[], 0.9, 0.9, 0.6),
];

const STORAGE: &[Archetype] = &[
    arch("minio", T::Minio, Sts, &[4], &["128Gi"], 0.9, 0.4, 0.8),
    arch("harbor", T::Harbor, Sts, &[1, 2], &["128Gi"], 0.9, 0.6, 0.7),
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
    }
}

fn embed_multiplier(s: Scenario, tool: Tool) -> f64 {
    match s {
        Scenario::Platform => 1.0,
        Scenario::Observability => match tool {
            T::Jaeger | T::Prometheus | T::OpenTelemetry => 3.0,
            _ => 0.7,
        },
        Scenario::Data => match tool {
            T::Postgres | T::Redis | T::MariaDb | T::MongoDb | T::RabbitMq | T::Nats => 2.0,
            _ => 0.5,
        },
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

fn sample_kind(rng: &mut ChaCha8Rng) -> WorkloadKind {
    match rng.random_range(0..100u32) {
        0..70 => WorkloadKind::Deployment,
        70..80 => WorkloadKind::StatefulSet,
        80..90 => WorkloadKind::DaemonSet,
        _ => WorkloadKind::Job,
    }
}

fn sample_health(rng: &mut ChaCha8Rng) -> Health {
    match rng.random_range(0..1000u32) {
        0..920 => Health::Ok,
        920..960 => Health::Warn,
        960..990 => Health::Err,
        _ => Health::Unknown,
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

fn kind_profile(kind: WorkloadKind) -> SatProfile<'static> {
    let svc_p = match kind {
        WorkloadKind::Deployment | WorkloadKind::StatefulSet => 0.55,
        WorkloadKind::DaemonSet | WorkloadKind::Job => 0.08,
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
    kind: WorkloadKind,
    replicas: u32,
    profile: &SatProfile,
) -> Vec<SatSpec> {
    let mut sats = Vec::new();
    if kind == WorkloadKind::StatefulSet {
        let size = if profile.pvc_sizes.is_empty() {
            sample_pvc_size(rng)
        } else {
            profile.pvc_sizes[rng.random_range(0..profile.pvc_sizes.len())]
        };
        for i in 0..replicas {
            sats.push(SatSpec {
                name: format!("pvc/data-{name}-{i}"),
                kind: SatKind::Volume,
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
            kind: SatKind::Service,
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
                kind: SatKind::ConfigMap,
                detail: format!("{} keys", rng.random_range(2..14u32)),
            });
        }
    }
    if rng.random_bool(profile.secret_p) {
        sats.push(SatSpec {
            name: format!("secret/{name}-creds"),
            kind: SatKind::Secret,
            detail: "opaque".to_string(),
        });
    }
    sats
}

fn gen_pods(rng: &mut ChaCha8Rng, name: &str, kind: WorkloadKind, replicas: u32) -> Vec<PodSpec> {
    (0..replicas)
        .map(|i| PodSpec {
            name: if kind == WorkloadKind::StatefulSet {
                format!("{name}-{i}")
            } else {
                format!("{name}-{}", pod_suffix(rng))
            },
            health: sample_health(rng),
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
                WorkloadKind::Job => 1,
                WorkloadKind::DaemonSet => rng.random_range(3..12),
                _ => sample_replicas(&mut rng),
            };
            let pods = gen_pods(&mut rng, &name, kind, replicas);
            let sats = gen_sats(&mut rng, &name, kind, replicas, &kind_profile(kind));
            let wl = WorkloadSpec {
                name,
                kind,
                tool: Tool::None,
                pods,
                sats,
                deps: Vec::new(),
            };
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

    spec
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
            if wl.kind == WorkloadKind::StatefulSet {
                sts_pods += wl.pods.len() as u32;
                let vols = wl.sats.iter().filter(|s| s.kind == SatKind::Volume).count() as u32;
                assert_eq!(vols, wl.pods.len() as u32, "one PVC per sts replica");
                pvc += vols;
            } else {
                assert!(
                    wl.sats.iter().all(|s| s.kind != SatKind::Volume),
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
        assert_eq!(prom.tool, Tool::Prometheus);
        assert_eq!(prom.kind, WorkloadKind::StatefulSet);
        let obs2 = generate(&GenConfig {
            seed: 42,
            target_objects: 20_000,
            scenario: Scenario::Observability,
        });
        assert_eq!(obs.total_pods, obs2.total_pods);
    }

    #[test]
    fn sts_pods_use_ordinals() {
        let spec = generate(&cfg(42, 20_000));
        let sts = spec
            .namespaces
            .iter()
            .flat_map(|n| &n.workloads)
            .find(|w| w.kind == WorkloadKind::StatefulSet)
            .expect("some sts");
        assert!(sts.pods[0].name.ends_with("-0"), "{}", sts.pods[0].name);
    }
}
