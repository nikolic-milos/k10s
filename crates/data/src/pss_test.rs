//! Fixture pods: each tell fires on its own, inheritance is container-over-pod,
//! and JSON agrees with the typed spec.

use super::*;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EphemeralContainer, HostPathVolumeSource, Pod, PodSecurityContext,
    PodSpec, SecurityContext, Volume,
};
use serde_json::json;

fn restricted_spec() -> PodSpec {
    PodSpec {
        containers: vec![Container {
            name: "app".to_string(),
            security_context: Some(SecurityContext {
                run_as_user: Some(1000),
                privileged: Some(false),
                capabilities: Some(Capabilities {
                    drop: Some(vec!["ALL".to_string()]),
                    add: Some(Vec::new()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }],
        host_network: Some(false),
        host_pid: Some(false),
        host_ipc: Some(false),
        security_context: Some(PodSecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn fixture_pod(spec: Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "api", "namespace": "prod", "uid": "pod-api" },
        "spec": spec
    })
}

#[test]
fn a_restricted_pod_has_no_tells() {
    let tells = from_spec(&restricted_spec());
    assert_eq!(tells, Tells::default());
    assert!(!tells.any());
    assert!(tells.labels().is_empty());
}

#[test]
fn privileged_host_path_and_host_namespaces_each_fire() {
    let privileged = from_spec(&PodSpec {
        containers: vec![Container {
            name: "app".to_string(),
            security_context: Some(SecurityContext {
                privileged: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    });
    assert!(privileged.privileged);
    assert_eq!(privileged.labels(), vec!["privileged"]);

    let host_path = from_spec(&PodSpec {
        containers: vec![Container {
            name: "app".to_string(),
            ..Default::default()
        }],
        volumes: Some(vec![Volume {
            name: "host".to_string(),
            host_path: Some(HostPathVolumeSource {
                path: "/etc".to_string(),
                type_: None,
            }),
            ..Default::default()
        }]),
        ..Default::default()
    });
    assert!(host_path.host_path);
    assert!(!host_path.privileged);

    let namespaces = from_spec(&PodSpec {
        containers: vec![Container {
            name: "app".to_string(),
            ..Default::default()
        }],
        host_network: Some(true),
        host_pid: Some(true),
        host_ipc: Some(true),
        ..Default::default()
    });
    assert!(namespaces.host_network && namespaces.host_pid && namespaces.host_ipc);
}

#[test]
fn run_as_user_zero_and_added_capabilities_fire() {
    let root = from_spec(&PodSpec {
        containers: vec![Container {
            name: "app".to_string(),
            security_context: Some(SecurityContext {
                run_as_user: Some(0),
                capabilities: Some(Capabilities {
                    add: Some(vec!["NET_ADMIN".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    });
    assert!(root.run_as_user_zero);
    assert!(root.capabilities_add);
    assert_eq!(root.labels(), vec!["runAsUser 0", "capabilities add"]);
}

#[test]
fn a_pod_level_run_as_user_zero_applies_until_a_container_overrides_it() {
    let inherited = from_spec(&PodSpec {
        security_context: Some(PodSecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        }),
        containers: vec![Container {
            name: "app".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    });
    assert!(inherited.run_as_user_zero);

    let overridden = from_spec(&PodSpec {
        security_context: Some(PodSecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        }),
        containers: vec![Container {
            name: "app".to_string(),
            security_context: Some(SecurityContext {
                run_as_user: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    });
    assert!(!overridden.run_as_user_zero);

    let no_containers = from_spec(&PodSpec {
        security_context: Some(PodSecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert!(no_containers.run_as_user_zero);
}

#[test]
fn init_and_ephemeral_containers_are_inspected() {
    let init = from_spec(&PodSpec {
        containers: vec![Container {
            name: "app".to_string(),
            ..Default::default()
        }],
        init_containers: Some(vec![Container {
            name: "migrate".to_string(),
            security_context: Some(SecurityContext {
                privileged: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    });
    assert!(init.privileged);

    let ephemeral = from_spec(&PodSpec {
        containers: vec![Container {
            name: "app".to_string(),
            ..Default::default()
        }],
        ephemeral_containers: Some(vec![EphemeralContainer {
            name: "dbg".to_string(),
            security_context: Some(SecurityContext {
                capabilities: Some(Capabilities {
                    add: Some(vec!["SYS_PTRACE".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    });
    assert!(ephemeral.capabilities_add);
}

#[test]
fn json_pod_and_bare_spec_agree_with_the_typed_pod() {
    let spec = json!({
        "hostNetwork": true,
        "hostPID": true,
        "hostIPC": false,
        "securityContext": { "runAsUser": 0 },
        "containers": [{
            "name": "app",
            "securityContext": {
                "privileged": true,
                "capabilities": { "add": ["NET_RAW"] }
            }
        }],
        "volumes": [{ "name": "host", "hostPath": { "path": "/var/run" } }]
    });
    let typed: PodSpec = serde_json::from_value(spec.clone()).expect("spec");
    let from_typed = from_spec(&typed);
    let from_json = from_value(&fixture_pod(spec.clone()));
    let from_bare = from_value(&spec);
    let pod = Pod {
        spec: Some(typed),
        ..Default::default()
    };
    assert_eq!(from_typed, from_json);
    assert_eq!(from_typed, from_bare);
    assert_eq!(from_typed, from_pod(&pod));
    assert!(from_typed.privileged);
    assert!(from_typed.host_path);
    assert!(from_typed.host_network);
    assert!(from_typed.host_pid);
    assert!(!from_typed.host_ipc);
    assert!(from_typed.run_as_user_zero);
    assert!(from_typed.capabilities_add);
    assert!(from_typed.any());
}

#[test]
fn garbage_json_is_no_tells_rather_than_a_panic() {
    assert_eq!(from_value(&json!("nope")), Tells::default());
    assert_eq!(from_value(&json!([1, 2, 3])), Tells::default());
    assert_eq!(from_value(&json!({"spec": 4})), Tells::default());
    let empty_pod = from_value(&json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "bare" }
    }));
    assert_eq!(empty_pod, Tells::default());
}
