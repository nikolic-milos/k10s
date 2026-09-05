//! Pod Security tells: privileged, hostPath, hostNetwork, from the pod spec.
//!
//! A tell is a fact the spec asked for, not a PSS profile verdict. Restricted,
//! Baseline, and Privileged are admission labels; this module only names the
//! fields an overlay can colour. No kube client: a Pod JSON or a typed spec is
//! enough, and garbage JSON is no tells rather than a panic.

use k8s_openapi::api::core::v1::{Pod, PodSpec, SecurityContext};
use serde::Deserialize;
use serde_json::Value;

/// Fields a Pod Security overlay can stamp. Each flag is "this spec asks for it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tells {
    pub privileged: bool,
    pub host_path: bool,
    pub host_network: bool,
    pub host_pid: bool,
    pub host_ipc: bool,
    pub run_as_user_zero: bool,
    pub capabilities_add: bool,
}

impl Tells {
    pub fn any(self) -> bool {
        self.privileged
            || self.host_path
            || self.host_network
            || self.host_pid
            || self.host_ipc
            || self.run_as_user_zero
            || self.capabilities_add
    }

    pub fn labels(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.privileged {
            out.push("privileged");
        }
        if self.host_path {
            out.push("hostPath");
        }
        if self.host_network {
            out.push("hostNetwork");
        }
        if self.host_pid {
            out.push("hostPID");
        }
        if self.host_ipc {
            out.push("hostIPC");
        }
        if self.run_as_user_zero {
            out.push("runAsUser 0");
        }
        if self.capabilities_add {
            out.push("capabilities add");
        }
        out
    }
}

pub fn from_pod(pod: &Pod) -> Tells {
    pod.spec.as_ref().map(from_spec).unwrap_or_default()
}

pub fn from_value(value: &Value) -> Tells {
    let spec = value.get("spec").unwrap_or(value);
    match PodSpec::deserialize(spec) {
        Ok(spec) => from_spec(&spec),
        Err(_) => Tells::default(),
    }
}

pub fn from_spec(spec: &PodSpec) -> Tells {
    let mut tells = Tells {
        host_network: spec.host_network == Some(true),
        host_pid: spec.host_pid == Some(true),
        host_ipc: spec.host_ipc == Some(true),
        host_path: spec
            .volumes
            .iter()
            .flatten()
            .any(|volume| volume.host_path.is_some()),
        ..Tells::default()
    };
    let pod_user = spec
        .security_context
        .as_ref()
        .and_then(|context| context.run_as_user);
    let mut saw_container = false;
    for container in &spec.containers {
        saw_container = true;
        apply_security(&mut tells, container.security_context.as_ref());
        apply_run_as_user(&mut tells, container.security_context.as_ref(), pod_user);
    }
    for container in spec.init_containers.iter().flatten() {
        saw_container = true;
        apply_security(&mut tells, container.security_context.as_ref());
        apply_run_as_user(&mut tells, container.security_context.as_ref(), pod_user);
    }
    for container in spec.ephemeral_containers.iter().flatten() {
        saw_container = true;
        apply_security(&mut tells, container.security_context.as_ref());
        apply_run_as_user(&mut tells, container.security_context.as_ref(), pod_user);
    }
    if !saw_container {
        apply_run_as_user(&mut tells, None, pod_user);
    }
    tells
}

fn apply_security(tells: &mut Tells, context: Option<&SecurityContext>) {
    let Some(context) = context else {
        return;
    };
    if context.privileged == Some(true) {
        tells.privileged = true;
    }
    if context
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.add.as_ref())
        .is_some_and(|add| !add.is_empty())
    {
        tells.capabilities_add = true;
    }
}

fn apply_run_as_user(tells: &mut Tells, context: Option<&SecurityContext>, pod_user: Option<i64>) {
    let user = context.and_then(|context| context.run_as_user).or(pod_user);
    if user == Some(0) {
        tells.run_as_user_zero = true;
    }
}

#[cfg(test)]
#[path = "pss_test.rs"]
mod tests;
