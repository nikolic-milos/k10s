use k10s_core::{Catalog, KindId, Role};
use kube::Client;
use kube::discovery::{ApiCapabilities, ApiResource, Discovery, Scope, verbs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    Metadata,
    Full,
}

#[derive(Debug, Clone)]
pub struct KindTarget {
    pub id: KindId,
    pub resource: ApiResource,
    pub role: Role,
    pub namespaced: bool,
    pub listable: bool,
    pub watchable: bool,
    // What the server says it will accept here, as distinct from what this
    // account is allowed to do: an apply affordance that is missing because the
    // kind has no patch verb is a different labelled state from one the RBAC
    // probe denied, and conflating them would tell a person to fix the wrong
    // thing.
    pub patchable: bool,
    // Whether `status` is a subresource of its own, which is what decides
    // whether an apply may carry a status block at all.
    pub status_subresource: bool,
}

impl KindTarget {
    pub fn group(&self) -> &str {
        &self.resource.group
    }

    pub fn kind(&self) -> &str {
        &self.resource.kind
    }

    pub fn plural(&self) -> &str {
        &self.resource.plural
    }
}

#[derive(Debug, Clone)]
pub struct WatchTarget {
    pub target: KindTarget,
    pub fidelity: Fidelity,
    pub pass_through: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Discovered {
    pub targets: Vec<KindTarget>,
    pub server_version: Option<String>,
    pub aggregated: bool,
}

impl Discovered {
    pub fn find(&self, group: &str, kind: &str) -> Option<&KindTarget> {
        self.targets
            .iter()
            .find(|t| t.group() == group && t.kind() == kind)
    }
}

struct CoreKind {
    group: &'static str,
    kind: &'static str,
}

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

pub fn is_pass_through(group: &str, kind: &str) -> bool {
    (group, kind) == ("apps", "ReplicaSet")
}

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

pub fn fidelity_of(group: &str, kind: &str) -> Fidelity {
    match (group, kind) {
        ("", "Pod") => Fidelity::Full,
        ("", "Service") => Fidelity::Full,
        ("", "PersistentVolumeClaim") => Fidelity::Full,
        _ => Fidelity::Metadata,
    }
}

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

pub fn intern(catalog: &mut Catalog, resource: ApiResource, caps: &ApiCapabilities) -> KindTarget {
    let role = role_of(&resource.group, &resource.kind);
    let id = catalog.intern_gvk_as(&resource.group, &resource.version, &resource.kind, role);
    KindTarget {
        id,
        role,
        namespaced: caps.scope == Scope::Namespaced,
        listable: caps.supports_operation(verbs::LIST),
        watchable: caps.supports_operation(verbs::WATCH),
        patchable: caps.supports_operation(verbs::PATCH),
        status_subresource: caps
            .subresources
            .iter()
            .any(|(subresource, _)| subresource.plural == "status"),
        resource,
    }
}

pub fn watch_set(discovered: &Discovered) -> Vec<WatchTarget> {
    let mut out = Vec::new();
    for want in CORE_WATCH {
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
