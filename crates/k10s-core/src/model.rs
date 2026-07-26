//! The open model: interned dense ids for kinds, vendors and state reasons.
//!
//! Closed enums could not name a CronJob, an Ingress, a Node or any CRD, and
//! `k10s-map` matched them exhaustively, so adding a kind meant editing the
//! painter. Identity is now an integer and presentation is a table lookup.
//!
//! The contract that makes this safe for the frame path: an id stays a plain
//! integer. No `Arc<str>`, no trait object and no string comparison below the
//! [`Catalog`], because the paint loop indexes tables by these ids once per
//! visible node. The catalog itself is startup-and-discovery machinery and must
//! never be consulted while painting.
//!
//! Built-ins occupy a dense prefix with stable constants, so the generator and
//! the tests keep compile-time names. Anything a cluster reports that we do not
//! know about is appended at runtime and renders through a fallback.

use std::collections::HashMap;
use std::sync::Arc;

/// Where a kind sits in the four-level scene, independent of what it is.
///
/// The scene stays four levels deep, reinterpreted as a role hierarchy rather
/// than a kind hierarchy: a Deployment and a CRD can both be owners, and a PVC
/// and a Service can both be attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// A namespace today, a cluster once clusters are super-regions.
    Scope,
    /// The thing that owns instances: Deployment, StatefulSet, a CRD.
    Owner,
    /// A single instance: a Pod.
    Instance,
    /// Attached to an owner rather than owning anything: PVC, Service, Secret.
    Attached,
}

/// Severity is deliberately closed: it is the rollup axis, and a lattice with a
/// variable number of levels cannot be `max`ed. The open part of a state is its
/// [`ReasonId`], so `CrashLoopBackOff` is a distinct reason at [`Severity::Err`]
/// rather than collapsing into a generic warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum Severity {
    #[default]
    Ok = 0,
    Unknown = 1,
    Warn = 2,
    Err = 3,
}

impl Severity {
    pub fn is_unhealthy(self) -> bool {
        matches!(self, Severity::Warn | Severity::Err)
    }

    /// The four-level rollup. Associative and commutative, so a parent can fold
    /// its children in any order.
    pub fn rollup(self, other: Severity) -> Severity {
        if other > self { other } else { self }
    }

    pub fn rank(self) -> u8 {
        self as u8
    }
}

/// A dense id for a resource kind. A GVK for a real cluster, a built-in for the
/// generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KindId(pub u32);

/// A dense id for a vendor whose branding we can present. [`ToolId::NONE`] means
/// "no vendor", which is the common case and why it holds slot zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ToolId(pub u16);

/// A dense id for the reason a thing is in the state it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ReasonId(pub u32);

/// A severity paired with the reason that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct State {
    pub severity: Severity,
    pub reason: ReasonId,
}

impl State {
    pub const OK: State = State {
        severity: Severity::Ok,
        reason: ReasonId::RUNNING,
    };

    /// Takes the severity a built-in reason implies, so callers cannot pair
    /// `CrashLoopBackOff` with `Ok`.
    pub fn of(reason: ReasonId) -> State {
        State {
            severity: reason_severity(reason),
            reason,
        }
    }

    pub fn is_unhealthy(self) -> bool {
        self.severity.is_unhealthy()
    }
}

/// Declares a dense registry: a static table plus one stable constant per entry,
/// generated together so an id can never drift from its metadata.
macro_rules! registry {
    (
        $Id:ident : $repr:ty, $Info:ty, $TABLE:ident, $COUNT:ident,
        $( $konst:ident => $info:expr ),+ $(,)?
    ) => {
        pub static $TABLE: &[$Info] = &[ $( $info ),+ ];
        /// How many ids are compiled in. Everything at or above this was
        /// discovered at runtime and has no static presentation.
        pub const $COUNT: $repr = $TABLE.len() as $repr;
        registry_consts!($Id, 0, $($konst),+);
    };
}

macro_rules! registry_consts {
    ($Id:ident, $i:expr, $head:ident $(, $tail:ident)*) => {
        impl $Id {
            pub const $head: $Id = $Id($i);
        }
        registry_consts!($Id, $i + 1 $(, $tail)*);
    };
    ($Id:ident, $i:expr) => {};
}

/// Static metadata for a built-in kind. Runtime-discovered kinds carry the same
/// fields owned, in [`KindEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindInfo {
    /// Stable machine name. Used by settings, saved views and tests.
    pub slug: &'static str,
    /// What a badge shows when there is no room for the full kind.
    pub short: &'static str,
    pub role: Role,
    pub group: &'static str,
    pub version: &'static str,
    pub kind: &'static str,
}

const fn kind(
    slug: &'static str,
    short: &'static str,
    role: Role,
    group: &'static str,
    version: &'static str,
    k: &'static str,
) -> KindInfo {
    KindInfo {
        slug,
        short,
        role,
        group,
        version,
        kind: k,
    }
}

registry! {
    KindId: u32, KindInfo, BUILTIN_KINDS, BUILTIN_KIND_COUNT,
    DEPLOYMENT   => kind("deployment", "deploy", Role::Owner, "apps", "v1", "Deployment"),
    STATEFUL_SET => kind("statefulset", "sts", Role::Owner, "apps", "v1", "StatefulSet"),
    DAEMON_SET   => kind("daemonset", "ds", Role::Owner, "apps", "v1", "DaemonSet"),
    JOB          => kind("job", "job", Role::Owner, "batch", "v1", "Job"),
    CRON_JOB     => kind("cronjob", "cron", Role::Owner, "batch", "v1", "CronJob"),
    POD          => kind("pod", "pod", Role::Instance, "", "v1", "Pod"),
    NAMESPACE    => kind("namespace", "ns", Role::Scope, "", "v1", "Namespace"),
    VOLUME       => kind("persistentvolumeclaim", "pvc", Role::Attached, "", "v1", "PersistentVolumeClaim"),
    SERVICE      => kind("service", "svc", Role::Attached, "", "v1", "Service"),
    CONFIG_MAP   => kind("configmap", "cm", Role::Attached, "", "v1", "ConfigMap"),
    SECRET       => kind("secret", "secret", Role::Attached, "", "v1", "Secret"),
    INGRESS      => kind("ingress", "ing", Role::Attached, "networking.k8s.io", "v1", "Ingress"),
    NODE         => kind("node", "node", Role::Scope, "", "v1", "Node"),
}

/// Static metadata for a built-in vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolInfo {
    pub slug: &'static str,
    pub display: &'static str,
}

const fn tool(slug: &'static str, display: &'static str) -> ToolInfo {
    ToolInfo { slug, display }
}

registry! {
    ToolId: u16, ToolInfo, BUILTIN_TOOLS, BUILTIN_TOOL_COUNT,
    NONE           => tool("none", "None"),
    AIRFLOW        => tool("airflow", "Airflow"),
    ARGO_CD        => tool("argocd", "Argo CD"),
    CASSANDRA      => tool("cassandra", "Cassandra"),
    CLICK_HOUSE    => tool("clickhouse", "ClickHouse"),
    CONSUL         => tool("consul", "Consul"),
    ELASTICSEARCH  => tool("elasticsearch", "Elasticsearch"),
    ENVOY          => tool("envoy", "Envoy"),
    ETCD           => tool("etcd", "etcd"),
    FLUENT_BIT     => tool("fluentbit", "Fluent Bit"),
    FLUENTD        => tool("fluentd", "Fluentd"),
    FLUX           => tool("flux", "Flux"),
    GRAFANA        => tool("grafana", "Grafana"),
    HARBOR         => tool("harbor", "Harbor"),
    ISTIO          => tool("istio", "Istio"),
    JAEGER         => tool("jaeger", "Jaeger"),
    JENKINS        => tool("jenkins", "Jenkins"),
    KAFKA          => tool("kafka", "Kafka"),
    KEYCLOAK       => tool("keycloak", "Keycloak"),
    KIBANA         => tool("kibana", "Kibana"),
    KUBERNETES     => tool("kubernetes", "Kubernetes"),
    MARIA_DB       => tool("mariadb", "MariaDB"),
    MINIO          => tool("minio", "MinIO"),
    MONGO_DB       => tool("mongodb", "MongoDB"),
    MY_SQL         => tool("mysql", "MySQL"),
    NATS           => tool("nats", "NATS"),
    NGINX          => tool("nginx", "nginx"),
    OPEN_TELEMETRY => tool("opentelemetry", "OpenTelemetry"),
    POSTGRES       => tool("postgres", "PostgreSQL"),
    PROMETHEUS     => tool("prometheus", "Prometheus"),
    RABBIT_MQ      => tool("rabbitmq", "RabbitMQ"),
    REDIS          => tool("redis", "Redis"),
    TEMPORAL       => tool("temporal", "Temporal"),
    TRAEFIK        => tool("traefik", "Traefik"),
    VAULT          => tool("vault", "Vault"),
}

/// Static metadata for a built-in reason, including the severity it implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasonInfo {
    pub slug: &'static str,
    /// What the API server calls it, which is what a user recognises.
    pub display: &'static str,
    pub severity: Severity,
}

const fn reason(slug: &'static str, display: &'static str, severity: Severity) -> ReasonInfo {
    ReasonInfo {
        slug,
        display,
        severity,
    }
}

registry! {
    ReasonId: u32, ReasonInfo, BUILTIN_REASONS, BUILTIN_REASON_COUNT,
    UNKNOWN              => reason("unknown", "Unknown", Severity::Unknown),
    RUNNING              => reason("running", "Running", Severity::Ok),
    SUCCEEDED            => reason("succeeded", "Succeeded", Severity::Ok),
    PENDING              => reason("pending", "Pending", Severity::Warn),
    NOT_READY            => reason("notready", "NotReady", Severity::Warn),
    PROGRESSING          => reason("progressing", "Progressing", Severity::Warn),
    TERMINATING          => reason("terminating", "Terminating", Severity::Warn),
    CRASH_LOOP_BACK_OFF  => reason("crashloopbackoff", "CrashLoopBackOff", Severity::Err),
    IMAGE_PULL_BACK_OFF  => reason("imagepullbackoff", "ImagePullBackOff", Severity::Err),
    OOM_KILLED           => reason("oomkilled", "OOMKilled", Severity::Err),
    EVICTED              => reason("evicted", "Evicted", Severity::Err),
    FAILED               => reason("failed", "Failed", Severity::Err),
}

/// The severity a reason implies. Falls back to [`Severity::Unknown`] for a
/// reason the cluster reported that we have no static entry for.
pub fn reason_severity(reason: ReasonId) -> Severity {
    match BUILTIN_REASONS.get(reason.0 as usize) {
        Some(info) => info.severity,
        None => Severity::Unknown,
    }
}

/// The short badge label for a kind, falling back to a marker for a kind
/// discovered at runtime. Static-only, so the frame path can call it without a
/// catalog; use [`Catalog::kind_short`] when discovered kinds must read well.
pub fn kind_short(id: KindId) -> &'static str {
    match BUILTIN_KINDS.get(id.0 as usize) {
        Some(info) => info.short,
        None => "?",
    }
}

/// The role a kind plays, falling back to [`Role::Owner`] so an unknown kind
/// still lands somewhere paintable rather than being dropped.
pub fn kind_role(id: KindId) -> Role {
    match BUILTIN_KINDS.get(id.0 as usize) {
        Some(info) => info.role,
        None => Role::Owner,
    }
}

impl KindId {
    /// Whether this id has compiled-in presentation. False means the map draws
    /// it through the fallback.
    pub fn is_builtin(self) -> bool {
        self.0 < BUILTIN_KIND_COUNT
    }
}

impl ToolId {
    pub fn is_builtin(self) -> bool {
        self.0 < BUILTIN_TOOL_COUNT
    }

    pub fn is_none(self) -> bool {
        self == ToolId::NONE
    }
}

/// An owned kind entry, so runtime-discovered kinds carry the same metadata as
/// built-ins.
#[derive(Debug, Clone)]
pub struct KindEntry {
    pub slug: Arc<str>,
    pub short: Arc<str>,
    pub role: Role,
    pub group: Arc<str>,
    pub version: Arc<str>,
    pub kind: Arc<str>,
}

/// Interns kinds, vendors and reasons discovered at runtime, keeping built-ins
/// at their compiled-in ids.
///
/// Not for the frame path. Discovery and ingestion own this; the scene carries
/// only the ids it hands out.
/// Group, version, kind. Interned as `Arc<str>` so the entry and the lookup key
/// share one allocation per string.
type GvkKey = (Arc<str>, Arc<str>, Arc<str>);

#[derive(Debug, Clone)]
pub struct Catalog {
    kinds: Vec<KindEntry>,
    by_gvk: HashMap<GvkKey, KindId>,
    tools: Vec<Arc<str>>,
    by_tool: HashMap<Arc<str>, ToolId>,
    reasons: Vec<Arc<str>>,
    by_reason: HashMap<Arc<str>, ReasonId>,
}

impl Default for Catalog {
    fn default() -> Self {
        Catalog::new()
    }
}

impl Catalog {
    pub fn new() -> Self {
        let mut c = Catalog {
            kinds: Vec::with_capacity(BUILTIN_KINDS.len()),
            by_gvk: HashMap::with_capacity(BUILTIN_KINDS.len()),
            tools: Vec::with_capacity(BUILTIN_TOOLS.len()),
            by_tool: HashMap::with_capacity(BUILTIN_TOOLS.len()),
            reasons: Vec::with_capacity(BUILTIN_REASONS.len()),
            by_reason: HashMap::with_capacity(BUILTIN_REASONS.len()),
        };
        for info in BUILTIN_KINDS {
            let entry = KindEntry {
                slug: info.slug.into(),
                short: info.short.into(),
                role: info.role,
                group: info.group.into(),
                version: info.version.into(),
                kind: info.kind.into(),
            };
            let key = (
                entry.group.clone(),
                entry.version.clone(),
                entry.kind.clone(),
            );
            c.by_gvk.insert(key, KindId(c.kinds.len() as u32));
            c.kinds.push(entry);
        }
        for info in BUILTIN_TOOLS {
            c.by_tool
                .insert(info.slug.into(), ToolId(c.tools.len() as u16));
            c.tools.push(info.slug.into());
        }
        for info in BUILTIN_REASONS {
            c.by_reason
                .insert(info.display.into(), ReasonId(c.reasons.len() as u32));
            c.reasons.push(info.display.into());
        }
        c
    }

    /// Interns a GVK, returning its existing id if it is already known. A CRD
    /// gets `Role::Owner` unless it is namespaced-attached, which discovery
    /// cannot tell us, so callers refine it with [`Catalog::intern_gvk_as`].
    pub fn intern_gvk(&mut self, group: &str, version: &str, kind: &str) -> KindId {
        self.intern_gvk_as(group, version, kind, Role::Owner)
    }

    pub fn intern_gvk_as(&mut self, group: &str, version: &str, kind: &str, role: Role) -> KindId {
        let key: GvkKey = (group.into(), version.into(), kind.into());
        if let Some(&id) = self.by_gvk.get(&key) {
            return id;
        }
        let id = KindId(self.kinds.len() as u32);
        let slug: Arc<str> = if group.is_empty() {
            kind.to_ascii_lowercase().into()
        } else {
            format!("{}.{}", kind.to_ascii_lowercase(), group).into()
        };
        self.kinds.push(KindEntry {
            slug,
            short: shorten(kind).into(),
            role,
            group: key.0.clone(),
            version: key.1.clone(),
            kind: key.2.clone(),
        });
        self.by_gvk.insert(key, id);
        id
    }

    pub fn intern_tool(&mut self, slug: &str) -> ToolId {
        if let Some(&id) = self.by_tool.get(slug) {
            return id;
        }
        let id = ToolId(self.tools.len() as u16);
        let slug: Arc<str> = slug.into();
        self.tools.push(slug.clone());
        self.by_tool.insert(slug, id);
        id
    }

    /// Interns a reason by the name the API server uses, so an unrecognised
    /// `reason` field becomes a first-class id instead of collapsing to Unknown.
    pub fn intern_reason(&mut self, display: &str) -> ReasonId {
        if let Some(&id) = self.by_reason.get(display) {
            return id;
        }
        let id = ReasonId(self.reasons.len() as u32);
        let display: Arc<str> = display.into();
        self.reasons.push(display.clone());
        self.by_reason.insert(display, id);
        id
    }

    pub fn kind(&self, id: KindId) -> Option<&KindEntry> {
        self.kinds.get(id.0 as usize)
    }

    pub fn kind_short(&self, id: KindId) -> &str {
        self.kinds.get(id.0 as usize).map_or("?", |e| &e.short)
    }

    pub fn tool_slug(&self, id: ToolId) -> &str {
        self.tools.get(id.0 as usize).map_or("none", |s| s)
    }

    pub fn reason_display(&self, id: ReasonId) -> &str {
        self.reasons.get(id.0 as usize).map_or("Unknown", |s| s)
    }

    pub fn kind_count(&self) -> usize {
        self.kinds.len()
    }
}

/// Derives a badge label for a kind nobody compiled in: the leading capitals of
/// a CamelCase kind, so `VirtualMachineInstance` reads as `vmi`.
fn shorten(kind: &str) -> String {
    let caps: String = kind
        .chars()
        .filter(|c| c.is_ascii_uppercase())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if caps.len() >= 2 {
        caps.chars().take(4).collect()
    } else {
        kind.chars()
            .take(4)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_constants_match_their_table_slots() {
        // The registry macro generates ids and metadata together; this pins that
        // the hand-written constant names still line up with the intended slugs,
        // because a reordered table would silently repoint every stored id.
        assert_eq!(
            BUILTIN_KINDS[KindId::DEPLOYMENT.0 as usize].slug,
            "deployment"
        );
        assert_eq!(
            BUILTIN_KINDS[KindId::VOLUME.0 as usize].slug,
            "persistentvolumeclaim"
        );
        assert_eq!(BUILTIN_KINDS[KindId::NODE.0 as usize].slug, "node");
        assert_eq!(BUILTIN_TOOLS[ToolId::NONE.0 as usize].slug, "none");
        assert_eq!(
            BUILTIN_TOOLS[ToolId::PROMETHEUS.0 as usize].slug,
            "prometheus"
        );
        assert_eq!(BUILTIN_TOOLS[ToolId::VAULT.0 as usize].slug, "vault");
        assert_eq!(
            BUILTIN_REASONS[ReasonId::UNKNOWN.0 as usize].slug,
            "unknown"
        );
        assert_eq!(
            BUILTIN_REASONS[ReasonId::CRASH_LOOP_BACK_OFF.0 as usize].slug,
            "crashloopbackoff"
        );
        assert_eq!(BUILTIN_KIND_COUNT as usize, BUILTIN_KINDS.len());
        assert_eq!(BUILTIN_TOOL_COUNT as usize, BUILTIN_TOOLS.len());
        assert_eq!(BUILTIN_REASON_COUNT as usize, BUILTIN_REASONS.len());
    }

    #[test]
    fn every_builtin_slug_is_unique() {
        for table in [
            BUILTIN_KINDS.iter().map(|i| i.slug).collect::<Vec<_>>(),
            BUILTIN_TOOLS.iter().map(|i| i.slug).collect(),
            BUILTIN_REASONS.iter().map(|i| i.slug).collect(),
        ] {
            let mut sorted = table.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), table.len(), "duplicate slug in {table:?}");
        }
    }

    #[test]
    fn none_holds_tool_slot_zero() {
        // ToolId::default() is what a workload with no vendor gets, and the map
        // presentation table relies on slot zero being the generic entry.
        assert_eq!(ToolId::default(), ToolId::NONE);
        assert_eq!(ToolId::NONE.0, 0);
        assert!(ToolId::NONE.is_none());
    }

    #[test]
    fn severity_rollup_is_a_max_and_order_free() {
        use Severity::*;
        assert_eq!(Ok.rollup(Err), Err);
        assert_eq!(Err.rollup(Ok), Err);
        assert_eq!(Unknown.rollup(Warn), Warn);
        assert_eq!(Warn.rollup(Unknown), Warn);
        assert_eq!(Ok.rollup(Ok), Ok);
        for a in [Ok, Unknown, Warn, Err] {
            for b in [Ok, Unknown, Warn, Err] {
                assert_eq!(a.rollup(b), b.rollup(a), "{a:?}/{b:?} not commutative");
                for c in [Ok, Unknown, Warn, Err] {
                    assert_eq!(
                        a.rollup(b).rollup(c),
                        a.rollup(b.rollup(c)),
                        "{a:?}/{b:?}/{c:?} not associative"
                    );
                }
            }
        }
        assert!(!Ok.is_unhealthy() && !Unknown.is_unhealthy());
        assert!(Warn.is_unhealthy() && Err.is_unhealthy());
    }

    #[test]
    fn state_takes_the_severity_its_reason_implies() {
        assert_eq!(State::of(ReasonId::RUNNING).severity, Severity::Ok);
        assert_eq!(State::of(ReasonId::PENDING).severity, Severity::Warn);
        assert_eq!(
            State::of(ReasonId::CRASH_LOOP_BACK_OFF).severity,
            Severity::Err
        );
        assert_eq!(State::of(ReasonId::OOM_KILLED).severity, Severity::Err);
        // The distinction the closed Health enum could not express.
        assert_ne!(
            State::of(ReasonId::CRASH_LOOP_BACK_OFF).reason,
            State::of(ReasonId::FAILED).reason
        );
        assert_eq!(State::OK.severity, Severity::Ok);
        // A reason nobody compiled in is Unknown, never accidentally Ok.
        assert_eq!(reason_severity(ReasonId(9999)), Severity::Unknown);
    }

    #[test]
    fn catalog_returns_builtin_ids_for_builtin_gvks() {
        let mut c = Catalog::new();
        assert_eq!(c.intern_gvk("apps", "v1", "Deployment"), KindId::DEPLOYMENT);
        assert_eq!(c.intern_gvk("", "v1", "Pod"), KindId::POD);
        assert_eq!(
            c.intern_gvk("networking.k8s.io", "v1", "Ingress"),
            KindId::INGRESS
        );
        assert_eq!(c.intern_tool("prometheus"), ToolId::PROMETHEUS);
        assert_eq!(
            c.intern_reason("CrashLoopBackOff"),
            ReasonId::CRASH_LOOP_BACK_OFF
        );
        assert_eq!(c.kind_count(), BUILTIN_KINDS.len());
    }

    #[test]
    fn crds_append_past_the_builtins_and_intern_once() {
        let mut c = Catalog::new();
        let before = c.kind_count();
        let vmi = c.intern_gvk("kubevirt.io", "v1", "VirtualMachineInstance");
        assert!(!vmi.is_builtin(), "a CRD must not land on a builtin id");
        assert_eq!(vmi.0, before as u32);
        assert_eq!(
            c.intern_gvk("kubevirt.io", "v1", "VirtualMachineInstance"),
            vmi
        );
        assert_eq!(c.kind_count(), before + 1, "interned twice");

        // Same kind name in a different group is a different kind.
        let other = c.intern_gvk("example.com", "v1", "VirtualMachineInstance");
        assert_ne!(other, vmi);

        let e = c.kind(vmi).expect("interned kind is retrievable");
        assert_eq!(&*e.kind, "VirtualMachineInstance");
        assert_eq!(&*e.slug, "virtualmachineinstance.kubevirt.io");
        assert_eq!(&*e.short, "vmi");
        assert_eq!(e.role, Role::Owner);

        let svc = c.intern_gvk_as("example.com", "v1", "Thing", Role::Attached);
        assert_eq!(c.kind(svc).unwrap().role, Role::Attached);
    }

    #[test]
    fn unknown_ids_fall_back_instead_of_panicking() {
        // The frame path must render a kind it has never seen, not crash.
        let unknown = KindId(9_999);
        assert!(!unknown.is_builtin());
        assert_eq!(kind_short(unknown), "?");
        assert_eq!(kind_role(unknown), Role::Owner);
        assert!(!ToolId(9_999).is_builtin());

        let c = Catalog::new();
        assert_eq!(c.kind_short(unknown), "?");
        assert_eq!(c.tool_slug(ToolId(9_999)), "none");
        assert_eq!(c.reason_display(ReasonId(9_999)), "Unknown");
        assert!(c.kind(unknown).is_none());
    }

    #[test]
    fn shorten_reads_well_for_camel_case_and_single_words() {
        assert_eq!(shorten("VirtualMachineInstance"), "vmi");
        assert_eq!(shorten("ResourceClaim"), "rc");
        assert_eq!(shorten("GatewayClass"), "gc");
        assert_eq!(shorten("ApplicationSetGeneratorThing"), "asgt");
        assert_eq!(shorten("Gateway"), "gate");
        assert_eq!(shorten("Foo"), "foo");
    }

    #[test]
    fn builtin_roles_are_what_the_scene_expects() {
        assert_eq!(kind_role(KindId::DEPLOYMENT), Role::Owner);
        assert_eq!(kind_role(KindId::POD), Role::Instance);
        assert_eq!(kind_role(KindId::NAMESPACE), Role::Scope);
        for attached in [
            KindId::VOLUME,
            KindId::SERVICE,
            KindId::CONFIG_MAP,
            KindId::SECRET,
        ] {
            assert_eq!(kind_role(attached), Role::Attached, "{attached:?}");
        }
        assert_eq!(kind_short(KindId::VOLUME), "pvc");
        assert_eq!(kind_short(KindId::STATEFUL_SET), "sts");
    }
}
