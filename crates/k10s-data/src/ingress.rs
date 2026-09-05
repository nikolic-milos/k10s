//! Ingress and IngressClass from `networking.k8s.io/v1`.
//!
//! These kinds are core: a normal cluster serves them, so an empty Ingress
//! list is a cluster with no objects, not an absent API. This module lists
//! through typed `k8s-openapi` objects the same way NetworkPolicy is listed.
//! TLS is secret names only; certificate bytes never enter the inventory.
//!
//! A live k3s that already runs Traefik answers one default IngressClass
//! named `traefik` and zero Ingress objects. That shape is served.

use std::collections::BTreeMap;

use k8s_openapi::api::networking::v1::{
    Ingress as ApiIngress, IngressBackend, IngressClass as ApiClass, IngressSpec, IngressStatus,
};
use kube::Client;
use kube::api::{Api, ListParams};
use serde::de::DeserializeOwned;

use crate::browse::{TableColumn, TablePage, TableRow};
use crate::read::{Fetched, classify};

pub const WHAT: &str = "ingress";
pub const DEFAULT_CLASS_ANNOTATION: &str = "ingressclass.kubernetes.io/is-default-class";
pub const CLASS_ANNOTATION: &str = "kubernetes.io/ingress.class";

const PAGE_LIMIT: u32 = 200;
pub const MAX_OBJECTS: usize = 2_000;
pub const MAX_FIELD_CHARS: usize = 200;
const MAX_HOSTS: usize = 32;
const MAX_PATHS: usize = 64;
const MAX_TLS_SECRETS: usize = 16;
const MAX_ADDRESSES: usize = 8;

/// One IngressClass, with the default annotation reduced to a flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    pub name: String,
    pub uid: String,
    pub controller: String,
    pub is_default: bool,
}

/// A service (or resource) backend on one path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Backend {
    pub service: String,
    pub port: String,
}

/// One host + path + backend triple from `spec.rules` or the default backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Path {
    pub host: String,
    pub path: String,
    pub backend: Backend,
}

/// One Ingress, reduced to what an inventory shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingress {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub class: String,
    pub hosts: Vec<String>,
    pub paths: Vec<Path>,
    pub tls_secrets: Vec<String>,
    pub address: String,
}

/// IngressClass and Ingress as they stand on this cluster.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    pub classes: Vec<Class>,
    pub ingresses: Vec<Ingress>,
    /// The cluster-scoped IngressClass list answered 403 while Ingress did
    /// not — a common RBAC shape. The readable side is still shown.
    pub classes_denied: bool,
    /// The Ingress list answered 403 while IngressClass did not.
    pub ingresses_denied: bool,
    pub truncated: bool,
}

enum ListOutcome<T> {
    Ok { items: Vec<T>, truncated: bool },
    Denied,
    Failed(String),
}

fn clipped(text: String) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_FIELD_CHARS).collect();
    cut.push('\u{2026}');
    cut
}

fn push_unique(out: &mut Vec<String>, text: String) {
    if text.is_empty() || out.iter().any(|have| have == &text) {
        return;
    }
    out.push(text);
}

fn is_default_class(annotations: Option<&BTreeMap<String, String>>) -> bool {
    annotations
        .and_then(|annotations| annotations.get(DEFAULT_CLASS_ANNOTATION))
        .is_some_and(|value| value == "true")
}

fn class_name(spec: &IngressSpec, annotations: Option<&BTreeMap<String, String>>) -> String {
    if let Some(name) = spec
        .ingress_class_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        return clipped(name.to_string());
    }
    clipped(
        annotations
            .and_then(|annotations| annotations.get(CLASS_ANNOTATION))
            .cloned()
            .unwrap_or_default(),
    )
}

fn backend_of(backend: &IngressBackend) -> Backend {
    if let Some(service) = &backend.service {
        let port = match &service.port {
            Some(port) => {
                if let Some(number) = port.number {
                    clipped(number.to_string())
                } else {
                    clipped(port.name.clone().unwrap_or_default())
                }
            }
            None => String::new(),
        };
        return Backend {
            service: clipped(service.name.clone()),
            port,
        };
    }
    if let Some(resource) = &backend.resource {
        return Backend {
            service: clipped(format!("{}/{}", resource.kind, resource.name)),
            port: String::new(),
        };
    }
    Backend::default()
}

fn address_of(status: Option<&IngressStatus>) -> String {
    let Some(points) = status
        .and_then(|status| status.load_balancer.as_ref())
        .and_then(|status| status.ingress.as_ref())
    else {
        return String::new();
    };
    let mut parts = Vec::new();
    for point in points.iter().take(MAX_ADDRESSES) {
        if let Some(ip) = point.ip.as_deref().filter(|ip| !ip.is_empty()) {
            parts.push(ip.to_string());
        }
        if let Some(hostname) = point.hostname.as_deref().filter(|name| !name.is_empty()) {
            parts.push(hostname.to_string());
        }
    }
    clipped(parts.join(","))
}

fn from_class(object: ApiClass) -> Option<Class> {
    let name = object.metadata.name.filter(|name| !name.is_empty())?;
    Some(Class {
        name: clipped(name),
        uid: clipped(object.metadata.uid.unwrap_or_default()),
        controller: clipped(
            object
                .spec
                .and_then(|spec| spec.controller)
                .unwrap_or_default(),
        ),
        is_default: is_default_class(object.metadata.annotations.as_ref()),
    })
}

fn from_ingress(object: ApiIngress) -> Option<Ingress> {
    let name = object.metadata.name.filter(|name| !name.is_empty())?;
    let spec = object.spec.unwrap_or_default();
    let class = class_name(&spec, object.metadata.annotations.as_ref());
    let mut hosts = Vec::new();
    let mut paths = Vec::new();
    if let Some(backend) = &spec.default_backend {
        paths.push(Path {
            host: String::new(),
            path: String::new(),
            backend: backend_of(backend),
        });
    }
    for rule in spec.rules.as_deref().unwrap_or_default() {
        if let Some(host) = rule.host.as_deref().filter(|host| !host.is_empty())
            && hosts.len() < MAX_HOSTS
        {
            push_unique(&mut hosts, clipped(host.to_string()));
        }
        let host = clipped(rule.host.clone().unwrap_or_default());
        let Some(http) = &rule.http else {
            continue;
        };
        for path in &http.paths {
            if paths.len() >= MAX_PATHS {
                break;
            }
            paths.push(Path {
                host: host.clone(),
                path: clipped(path.path.clone().unwrap_or_default()),
                backend: backend_of(&path.backend),
            });
        }
    }
    let mut tls_secrets = Vec::new();
    for tls in spec.tls.as_deref().unwrap_or_default() {
        if tls_secrets.len() >= MAX_TLS_SECRETS {
            break;
        }
        if let Some(secret) = tls.secret_name.as_deref().filter(|name| !name.is_empty()) {
            push_unique(&mut tls_secrets, clipped(secret.to_string()));
        }
    }
    Some(Ingress {
        name: clipped(name),
        namespace: clipped(object.metadata.namespace.unwrap_or_default()),
        uid: clipped(object.metadata.uid.unwrap_or_default()),
        class,
        hosts,
        paths,
        tls_secrets,
        address: address_of(object.status.as_ref()),
    })
}

fn backend_label(backend: &Backend) -> String {
    match (backend.service.as_str(), backend.port.as_str()) {
        ("", _) => String::new(),
        (service, "") => service.to_string(),
        (service, port) => format!("{service}:{port}"),
    }
}

fn path_label(path: &Path) -> String {
    let dest = backend_label(&path.backend);
    match (path.host.as_str(), path.path.as_str(), dest.as_str()) {
        ("", "", "") => String::new(),
        ("", "", dest) => dest.to_string(),
        ("", path, dest) => format!("{path} -> {dest}"),
        (host, "", dest) => format!("{host} -> {dest}"),
        (host, path, dest) => format!("{host}{path} -> {dest}"),
    }
}

fn join_clipped(parts: &[String]) -> String {
    clipped(parts.join(", "))
}

async fn list_pages<K>(api: Api<K>, cap: usize) -> ListOutcome<K>
where
    K: kube::Resource + Clone + DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let mut items = Vec::new();
    let mut token: Option<String> = None;
    let mut truncated = false;
    loop {
        let mut params = ListParams::default().limit(PAGE_LIMIT);
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(error) if items.is_empty() => {
                return match classify::<()>(WHAT, &error) {
                    Fetched::Denied { .. } => ListOutcome::Denied,
                    Fetched::Failed { why, .. } => ListOutcome::Failed(why),
                    Fetched::Ok(()) => unreachable!("classify never succeeds"),
                };
            }
            Err(error) => {
                return ListOutcome::Failed(crate::connect::describe(
                    &error as &(dyn std::error::Error + 'static),
                ));
            }
        };
        for item in page.items {
            if items.len() == cap {
                truncated = true;
                break;
            }
            items.push(item);
        }
        let cont = page.metadata.continue_.filter(|token| !token.is_empty());
        token = if truncated { None } else { cont };
        if token.is_none() {
            break;
        }
    }
    ListOutcome::Ok { items, truncated }
}

/// List IngressClass (cluster) and Ingress (optionally one namespace).
///
/// A 403 on either list is [`Fetched::Denied`]. A 404 is Failed: these kinds
/// are core, so absence is not "not installed".
pub async fn fetch(client: &Client, namespace: Option<&str>) -> Fetched<Inventory> {
    let classes_api: Api<ApiClass> = Api::all(client.clone());
    let ingresses_api: Api<ApiIngress> = match namespace {
        Some(namespace) => Api::namespaced(client.clone(), namespace),
        None => Api::all(client.clone()),
    };
    let (classes, ingresses) = tokio::join!(
        list_pages(classes_api, MAX_OBJECTS),
        list_pages(ingresses_api, MAX_OBJECTS),
    );
    if let ListOutcome::Failed(why) = classes {
        return Fetched::Failed { what: WHAT, why };
    }
    if let ListOutcome::Failed(why) = ingresses {
        return Fetched::Failed { what: WHAT, why };
    }
    let classes_denied = matches!(classes, ListOutcome::Denied);
    let ingresses_denied = matches!(ingresses, ListOutcome::Denied);
    if classes_denied && ingresses_denied {
        return Fetched::Denied { what: WHAT };
    }
    let mut truncated = false;
    let mut class_rows = Vec::new();
    if let ListOutcome::Ok {
        items,
        truncated: t,
    } = classes
    {
        truncated |= t;
        class_rows.extend(items.into_iter().filter_map(from_class));
    }
    let mut ingress_rows = Vec::new();
    if let ListOutcome::Ok {
        items,
        truncated: t,
    } = ingresses
    {
        truncated |= t;
        ingress_rows.extend(items.into_iter().filter_map(from_ingress));
    }
    Fetched::Ok(Inventory {
        classes: class_rows,
        ingresses: ingress_rows,
        classes_denied,
        ingresses_denied,
        truncated,
    })
}

fn denied_row(kind: &str) -> TableRow {
    TableRow {
        cells: vec![
            kind.to_string(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "access denied for this account".to_string(),
        ],
        name: kind.to_string(),
        namespace: None,
        uid: format!("denied:{kind}"),
    }
}

/// Always `Some`. Ingress and IngressClass are core: an empty list is a
/// table, not a hidden pane.
pub fn table_page(inventory: &Inventory) -> Option<TablePage> {
    let columns = [
        "Kind",
        "Name",
        "Namespace",
        "Class",
        "Hosts",
        "Paths",
        "Backend",
        "TLS",
        "Address",
    ]
    .iter()
    .map(|name| TableColumn {
        name: (*name).to_string(),
        wide: false,
    })
    .collect();
    let mut rows = Vec::new();
    if inventory.classes_denied {
        rows.push(denied_row("IngressClass"));
    }
    if inventory.ingresses_denied {
        rows.push(denied_row("Ingress"));
    }
    for class in &inventory.classes {
        let class_cell = if class.is_default {
            if class.controller.is_empty() {
                "default".to_string()
            } else {
                format!("{} (default)", class.controller)
            }
        } else {
            class.controller.clone()
        };
        let uid = if class.uid.is_empty() {
            format!("IngressClass/{}", class.name)
        } else {
            class.uid.clone()
        };
        rows.push(TableRow {
            cells: vec![
                "IngressClass".to_string(),
                class.name.clone(),
                String::new(),
                class_cell,
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                if class.is_default {
                    "default".to_string()
                } else {
                    String::new()
                },
            ],
            name: class.name.clone(),
            namespace: None,
            uid,
        });
    }
    for ingress in &inventory.ingresses {
        let mut backends = Vec::new();
        for path in &ingress.paths {
            push_unique(&mut backends, backend_label(&path.backend));
        }
        let paths: Vec<String> = ingress.paths.iter().map(path_label).collect();
        let uid = if ingress.uid.is_empty() {
            format!("Ingress/{}/{}", ingress.namespace, ingress.name)
        } else {
            ingress.uid.clone()
        };
        rows.push(TableRow {
            cells: vec![
                "Ingress".to_string(),
                ingress.name.clone(),
                ingress.namespace.clone(),
                ingress.class.clone(),
                join_clipped(&ingress.hosts),
                join_clipped(&paths),
                join_clipped(&backends),
                join_clipped(&ingress.tls_secrets),
                ingress.address.clone(),
            ],
            name: ingress.name.clone(),
            namespace: Some(ingress.namespace.clone()),
            uid,
        });
    }
    Some(TablePage {
        columns,
        rows,
        truncated: inventory.truncated,
        continue_token: None,
    })
}

#[cfg(test)]
#[path = "ingress_test.rs"]
mod tests;
