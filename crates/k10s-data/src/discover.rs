//! Discovery: what this cluster serves, interned so a CRD is a real [`KindId`].
//!
//! kube-rs owns the protocol (`/api`, `/apis`, per-group `APIResourceList`, and
//! the aggregated discovery document). What lives here is policy, and policy is
//! the part that can be tested with no cluster:
//!
//! - **Role assignment.** The scene stays four levels deep as a *role* hierarchy,
//!   so every kind has to land on scope, owner, instance or attached. Discovery
//!   cannot tell us which, so [`role_of`] decides, and anything unrecognised
//!   becomes an owner, matching `k10s_core::kind_role`'s own fallback.
//! - **Fidelity.** [`fidelity_of`] says whether a kind is watched as a whole
//!   object or as metadata only. Metadata-only is the default and full objects
//!   are the exception, listed one by one with the field that justifies them.
//!   This is what makes secret hygiene structural rather than careful: a Secret
//!   is never requested as a whole object, so its values are not in the process
//!   to leak.
//! - **The watch set.** Interning every served kind is cheap and is what makes a
//!   CRD nameable. *Watching* every served kind would open two hundred streams
//!   against a cluster to draw a map of eleven, so the two sets are separate.

use k10s_core::{Catalog, KindId, Role};
use kube::Client;
use kube::discovery::{ApiCapabilities, ApiResource, Discovery, Scope, verbs};

/// Whether a kind is watched whole or as metadata only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    /// `metadata` only, via the API server's `PartialObjectMetadata` projection.
    /// The bytes never leave the API server, so this is a privacy property and
    /// not just a bandwidth one.
    Metadata,
    /// The whole object, because the payload needs a field outside `metadata`.
    Full,
}

/// One kind the cluster serves, with everything needed to watch it.
#[derive(Debug, Clone)]
pub struct KindTarget {
    pub id: KindId,
    pub resource: ApiResource,
    pub role: Role,
    pub namespaced: bool,
    /// From the discovery document's verb list, before RBAC has an opinion.
    pub listable: bool,
    pub watchable: bool,
}

impl KindTarget {
    pub fn group(&self) -> &str {
        &self.resource.group
    }

    pub fn kind(&self) -> &str {
        &self.resource.kind
    }

    /// The plural resource name, which is what an RBAC rule names.
    pub fn plural(&self) -> &str {
        &self.resource.plural
    }
}

/// A kind we intend to watch, and how.
#[derive(Debug, Clone)]
pub struct WatchTarget {
    pub target: KindTarget,
    pub fidelity: Fidelity,
    /// True for kinds watched only to resolve ownership, never emitted. A
    /// ReplicaSet is the case that matters: without it a Deployment's pods have
    /// no path back to the Deployment, and with it as a visible owner every
    /// Deployment appears twice.
    pub pass_through: bool,
}

/// Everything discovery learned.
#[derive(Debug, Clone, Default)]
pub struct Discovered {
    pub targets: Vec<KindTarget>,
    /// `gitVersion` as the server reports it, for the report and for deciding
    /// nothing else: version-gating behaviour is how a client breaks on the next
    /// release.
    pub server_version: Option<String>,
    /// Whether the two-request aggregated document was available. A cluster that
    /// falls back pays one request per group and fails discovery entirely if any
    /// aggregated APIService is unhealthy, which is worth reporting.
    pub aggregated: bool,
}

impl Discovered {
    pub fn find(&self, group: &str, kind: &str) -> Option<&KindTarget> {
        self.targets
            .iter()
            .find(|t| t.group() == group && t.kind() == kind)
    }
}

/// A kind the phase-C slice watches, and why it needs the fidelity it asks for.
struct CoreKind {
    group: &'static str,
    kind: &'static str,
}

/// The watch set. Small on purpose: a correct map of these eleven kinds is worth
/// more than a partial map of everything the cluster serves, and every additional
/// kind is a stream, a cache and a per-frame cost.
const CORE_WATCH: &[CoreKind] = &[
    CoreKind {
        group: "",
        kind: "Namespace",
    },
    CoreKind {
        group: "apps",
        kind: "Deployment",
    },
    CoreKind {
        group: "apps",
        kind: "StatefulSet",
    },
    CoreKind {
        group: "apps",
        kind: "DaemonSet",
    },
    CoreKind {
        group: "batch",
        kind: "CronJob",
    },
    CoreKind {
        group: "batch",
        kind: "Job",
    },
    // Watched to walk Pod -> ReplicaSet -> Deployment, never emitted.
    CoreKind {
        group: "apps",
        kind: "ReplicaSet",
    },
    CoreKind {
        group: "",
        kind: "Pod",
    },
    CoreKind {
        group: "",
        kind: "Service",
    },
    CoreKind {
        group: "",
        kind: "PersistentVolumeClaim",
    },
    CoreKind {
        group: "",
        kind: "ConfigMap",
    },
    CoreKind {
        group: "",
        kind: "Secret",
    },
];

/// The kinds watched only to resolve ownership.
pub fn is_pass_through(group: &str, kind: &str) -> bool {
    // A ReplicaSet is a Deployment's implementation detail, and showing it as a
    // workload doubles every Deployment on the map. A bare ReplicaSet (no
    // Deployment above it) is rare enough to accept as invisible for now.
    (group, kind) == ("apps", "ReplicaSet")
}

/// Which of the four scene roles a kind plays.
///
/// Unrecognised kinds, which is every CRD, become owners. That matches
/// `k10s_core::kind_role`'s fallback and is the only choice that keeps an unknown
/// kind paintable: an instance with no owner and an attachment with no owner both
/// have nowhere to sit.
pub fn role_of(group: &str, kind: &str) -> Role {
    match (group, kind) {
        ("", "Namespace") | ("", "Node") => Role::Scope,
        ("", "Pod") => Role::Instance,
        ("", "Service")
        | ("", "PersistentVolumeClaim")
        | ("", "PersistentVolume")
        | ("", "ConfigMap")
        | ("", "Secret")
        | ("", "Endpoints")
        | ("", "ServiceAccount")
        | ("", "LimitRange")
        | ("", "ResourceQuota")
        | ("discovery.k8s.io", "EndpointSlice")
        | ("networking.k8s.io", "Ingress")
        | ("networking.k8s.io", "NetworkPolicy")
        | ("policy", "PodDisruptionBudget")
        | ("autoscaling", "HorizontalPodAutoscaler") => Role::Attached,
        _ => Role::Owner,
    }
}

/// Whether a kind needs its whole object.
///
/// The list is short and every entry names the field that earns it. Anything not
/// listed is metadata-only, which is why a Secret cannot leak a value: there is
/// no code path that asks the API server for one.
pub fn fidelity_of(group: &str, kind: &str) -> Fidelity {
    match (group, kind) {
        // `status.containerStatuses` for the reason a pod is unhealthy, and
        // `spec.volumes`/`envFrom` to know which attachments a workload uses.
        ("", "Pod") => Fidelity::Full,
        // `spec.selector`, to attach a Service to the workload it fronts.
        ("", "Service") => Fidelity::Full,
        // `status.capacity`, which is the only interesting thing about a claim.
        ("", "PersistentVolumeClaim") => Fidelity::Full,
        _ => Fidelity::Metadata,
    }
}

/// Runs discovery and interns every served kind into `catalog`.
///
/// Tries the aggregated document first: two requests instead of one per group,
/// and immune to a single unhealthy aggregated APIService taking the whole
/// discovery down with it. Falls back to per-group discovery for servers older
/// than 1.26.
pub async fn discover(client: &Client, catalog: &mut Catalog) -> Result<Discovered, kube::Error> {
    let (discovery, aggregated) = match Discovery::new(client.clone()).run_aggregated().await {
        Ok(d) => (d, true),
        Err(_) => (Discovery::new(client.clone()).run().await?, false),
    };

    let mut targets = Vec::new();
    for group in discovery.groups() {
        for (resource, caps) in group.recommended_resources() {
            targets.push(intern(catalog, resource, &caps));
        }
    }
    // Deterministic order, so a report and a set of Capability events read the
    // same way twice against the same cluster.
    targets.sort_by(|a, b| {
        (a.group(), a.kind())
            .cmp(&(b.group(), b.kind()))
            .then(a.id.0.cmp(&b.id.0))
    });

    let server_version = client
        .apiserver_version()
        .await
        .ok()
        .map(|info| info.git_version);

    Ok(Discovered {
        targets,
        server_version,
        aggregated,
    })
}

/// Interns one discovered resource. Split out so the interning policy can be
/// tested against hand-built discovery data.
pub fn intern(catalog: &mut Catalog, resource: ApiResource, caps: &ApiCapabilities) -> KindTarget {
    let role = role_of(&resource.group, &resource.kind);
    let id = catalog.intern_gvk_as(&resource.group, &resource.version, &resource.kind, role);
    KindTarget {
        id,
        role,
        namespaced: caps.scope == Scope::Namespaced,
        listable: caps.supports_operation(verbs::LIST),
        watchable: caps.supports_operation(verbs::WATCH),
        resource,
    }
}

/// The kinds to watch, in the order they should be listed.
///
/// Scopes first, then owners, then pass-throughs, then instances, then
/// attachments: the order the assembler resolves parents in, so an initial sync
/// that arrives roughly in this order needs less buffering.
pub fn watch_set(discovered: &Discovered) -> Vec<WatchTarget> {
    let mut out = Vec::new();
    for want in CORE_WATCH {
        // A kind the cluster does not serve is invisible, not broken: an old
        // server with no `batch/v1 CronJob` should simply have no CronJobs.
        let Some(target) = discovered.find(want.group, want.kind) else {
            continue;
        };
        if !target.listable || !target.watchable {
            continue;
        }
        out.push(WatchTarget {
            target: target.clone(),
            fidelity: fidelity_of(want.group, want.kind),
            pass_through: is_pass_through(want.group, want.kind),
        });
    }
    out.sort_by_key(|w| role_order(w.target.role));
    out
}

fn role_order(role: Role) -> u8 {
    match role {
        Role::Scope => 0,
        Role::Owner => 1,
        Role::Instance => 2,
        Role::Attached => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(scope: Scope, ops: &[&str]) -> ApiCapabilities {
        ApiCapabilities {
            scope,
            subresources: Vec::new(),
            operations: ops.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn resource(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
        ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: if group.is_empty() {
                version.to_string()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.to_string(),
            plural: plural.to_string(),
        }
    }

    fn discovered(items: &[(ApiResource, ApiCapabilities)]) -> (Discovered, Catalog) {
        let mut catalog = Catalog::new();
        let mut targets: Vec<KindTarget> = items
            .iter()
            .map(|(r, c)| intern(&mut catalog, r.clone(), c))
            .collect();
        targets.sort_by(|a, b| (a.group(), a.kind()).cmp(&(b.group(), b.kind())));
        (
            Discovered {
                targets,
                server_version: None,
                aggregated: true,
            },
            catalog,
        )
    }

    const RW: &[&str] = &["get", "list", "watch"];

    fn core_cluster() -> Vec<(ApiResource, ApiCapabilities)> {
        vec![
            (
                resource("", "v1", "Namespace", "namespaces"),
                caps(Scope::Cluster, RW),
            ),
            (
                resource("", "v1", "Pod", "pods"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("", "v1", "Secret", "secrets"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("", "v1", "Service", "services"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("", "v1", "ConfigMap", "configmaps"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("", "v1", "PersistentVolumeClaim", "persistentvolumeclaims"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("apps", "v1", "Deployment", "deployments"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("apps", "v1", "ReplicaSet", "replicasets"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("apps", "v1", "StatefulSet", "statefulsets"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("apps", "v1", "DaemonSet", "daemonsets"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("batch", "v1", "Job", "jobs"),
                caps(Scope::Namespaced, RW),
            ),
            (
                resource("batch", "v1", "CronJob", "cronjobs"),
                caps(Scope::Namespaced, RW),
            ),
        ]
    }

    #[test]
    fn builtin_gvks_land_on_their_compiled_in_ids() {
        // If discovery interned `apps/v1 Deployment` as a fresh id, every
        // presentation table would miss and the map would paint fallbacks for
        // the most common kind in Kubernetes.
        let (d, _) = discovered(&core_cluster());
        assert_eq!(d.find("apps", "Deployment").unwrap().id, KindId::DEPLOYMENT);
        assert_eq!(d.find("", "Pod").unwrap().id, KindId::POD);
        assert_eq!(d.find("", "Namespace").unwrap().id, KindId::NAMESPACE);
        assert_eq!(d.find("", "Secret").unwrap().id, KindId::SECRET);
        assert_eq!(
            d.find("", "PersistentVolumeClaim").unwrap().id,
            KindId::VOLUME
        );
    }

    #[test]
    fn a_crd_becomes_a_real_kind_id_with_a_derived_badge() {
        // The whole point of the open model, checked at the discovery boundary
        // rather than only in the catalog's own tests.
        let mut items = core_cluster();
        items.push((
            resource(
                "kubevirt.io",
                "v1",
                "VirtualMachineInstance",
                "virtualmachineinstances",
            ),
            caps(Scope::Namespaced, RW),
        ));
        let (d, catalog) = discovered(&items);
        let vmi = d.find("kubevirt.io", "VirtualMachineInstance").unwrap();
        assert!(!vmi.id.is_builtin());
        assert_eq!(catalog.kind_short(vmi.id), "vmi");
        assert_eq!(vmi.role, Role::Owner, "an unknown kind must be paintable");
        assert_eq!(vmi.plural(), "virtualmachineinstances");
    }

    #[test]
    fn roles_put_every_builtin_where_the_scene_expects_it() {
        assert_eq!(role_of("", "Namespace"), Role::Scope);
        assert_eq!(role_of("", "Node"), Role::Scope);
        assert_eq!(role_of("", "Pod"), Role::Instance);
        for (g, k) in [
            ("", "Service"),
            ("", "Secret"),
            ("", "ConfigMap"),
            ("", "PersistentVolumeClaim"),
            ("networking.k8s.io", "Ingress"),
            ("autoscaling", "HorizontalPodAutoscaler"),
        ] {
            assert_eq!(role_of(g, k), Role::Attached, "{g}/{k}");
        }
        for (g, k) in [
            ("apps", "Deployment"),
            ("batch", "CronJob"),
            ("argoproj.io", "Application"),
            ("example.com", "Anything"),
        ] {
            assert_eq!(role_of(g, k), Role::Owner, "{g}/{k}");
        }
    }

    #[test]
    fn only_the_kinds_that_need_a_field_outside_metadata_are_fetched_whole() {
        // This is the secret-hygiene invariant expressed as a policy test: if a
        // future edit adds Secret to the full list, this fails.
        assert_eq!(fidelity_of("", "Pod"), Fidelity::Full);
        assert_eq!(fidelity_of("", "Service"), Fidelity::Full);
        assert_eq!(fidelity_of("", "PersistentVolumeClaim"), Fidelity::Full);
        assert_eq!(fidelity_of("", "Secret"), Fidelity::Metadata);
        assert_eq!(fidelity_of("", "ConfigMap"), Fidelity::Metadata);
        assert_eq!(fidelity_of("apps", "Deployment"), Fidelity::Metadata);
        assert_eq!(fidelity_of("helm.sh", "Anything"), Fidelity::Metadata);

        let (d, _) = discovered(&core_cluster());
        for w in watch_set(&d) {
            if w.target.kind() == "Secret" {
                assert_eq!(
                    w.fidelity,
                    Fidelity::Metadata,
                    "a Secret must never be watched whole"
                );
            }
        }
    }

    #[test]
    fn the_watch_set_lists_scopes_before_owners_before_instances() {
        // The assembler resolves parents in this order, and a stream that
        // arrives in it needs the least buffering.
        let (d, _) = discovered(&core_cluster());
        let set = watch_set(&d);
        let roles: Vec<Role> = set.iter().map(|w| w.target.role).collect();
        let mut sorted = roles.clone();
        sorted.sort_by_key(|r| role_order(*r));
        assert_eq!(roles, sorted);
        assert_eq!(roles.first(), Some(&Role::Scope));
        assert_eq!(set.len(), 12, "every core kind of a full cluster");
        assert_eq!(
            set.iter().filter(|w| w.pass_through).count(),
            1,
            "only ReplicaSet is a pass-through"
        );
    }

    #[test]
    fn a_kind_the_cluster_does_not_serve_is_absent_not_an_error() {
        // "Invisible rather than broken" at the discovery boundary: an old
        // server with no batch/v1 CronJob just has no CronJobs.
        let items: Vec<_> = core_cluster()
            .into_iter()
            .filter(|(r, _)| r.kind != "CronJob")
            .collect();
        let (d, _) = discovered(&items);
        let set = watch_set(&d);
        assert!(set.iter().all(|w| w.target.kind() != "CronJob"));
        assert_eq!(set.len(), 11);
    }

    #[test]
    fn a_kind_that_cannot_be_watched_is_left_out_of_the_watch_set() {
        // Not every served resource is watchable, and opening a watch on one is
        // an error loop rather than a stream.
        let items: Vec<_> = core_cluster()
            .into_iter()
            .map(|(r, c)| {
                if r.kind == "Secret" {
                    (r, caps(Scope::Namespaced, &["get", "list"]))
                } else {
                    (r, c)
                }
            })
            .collect();
        let (d, _) = discovered(&items);
        assert!(watch_set(&d).iter().all(|w| w.target.kind() != "Secret"));
        // But it is still interned and still reportable.
        assert!(d.find("", "Secret").is_some());
        assert!(!d.find("", "Secret").unwrap().watchable);
    }

    #[test]
    fn scope_comes_from_discovery_not_from_a_guess() {
        let (d, _) = discovered(&core_cluster());
        assert!(!d.find("", "Namespace").unwrap().namespaced);
        assert!(d.find("", "Pod").unwrap().namespaced);
    }
}
