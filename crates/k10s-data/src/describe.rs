//! One object rendered for a person: field-level text, the owner chain, and
//! the event history joined by `involvedObject`.
//!
//! The object arrives as JSON and is rendered deterministically (keys sorted,
//! the conventional apiVersion/kind/metadata/spec front matter first, status
//! last) with `managedFields` dropped and hard caps on line count, line
//! length, and depth -- a describe document is bounded by construction, like
//! every other buffer in the repo. A Secret is fetched as
//! `PartialObjectMetadata`, so its values never enter the process, and the
//! document says so instead of showing an absence. Owners are walked upward
//! through `ownerReferences` (metadata-only fetches, cycle-guarded); a denial
//! anywhere degrades into a labelled line, never a lost document.

use k8s_openapi::api::core::v1::Event;
use kube::Client;
use kube::api::{Api, GetParams, ListParams, Request};

use crate::discover::KindTarget;
use crate::read::{Fetched, classify, collection_path};

use k10s_core::KindId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribeRequest {
    pub kind: KindId,
    pub namespace: Option<String>,
    pub name: String,
    pub uid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    pub title: String,
    pub lines: Vec<String>,
}

const MAX_LINES: usize = 4_000;
const MAX_LINE_CHARS: usize = 2_000;
const MAX_DEPTH: usize = 32;
const MAX_OWNER_HOPS: usize = 8;
const MAX_EVENTS: usize = 20;

pub(crate) async fn fetch_describe(
    client: &Client,
    targets: &[KindTarget],
    request: &DescribeRequest,
) -> Fetched<Described> {
    let Some(target) = targets.iter().find(|t| t.id == request.kind) else {
        return Fetched::Failed {
            what: "describe",
            why: "this kind is not served by the connected cluster".to_string(),
        };
    };

    let object =
        match fetch_object(client, target, request.namespace.as_deref(), &request.name).await {
            Ok(value) => value,
            Err(error) => return classify("describe", &error),
        };

    let mut doc = Doc::new();
    if is_secret(target) {
        doc.push(0, "# values withheld: k10s reads Secret metadata only");
    }
    render_object(&object, &mut doc);

    doc.blank();
    doc.push(0, "controlled by:");
    let owners = owner_chain(client, targets, request.namespace.as_deref(), &object).await;
    if owners.is_empty() {
        doc.push(1, "(nothing; this object stands alone)");
    }
    for line in owners {
        doc.push(1, &line);
    }

    doc.blank();
    doc.push(0, "events:");
    for line in event_lines(client, request).await {
        doc.push(1, &line);
    }

    Fetched::Ok(Described {
        title: format!("{} {}", target.kind(), request.name),
        lines: doc.lines,
    })
}

pub(crate) async fn fetch_object(
    client: &Client,
    target: &KindTarget,
    namespace: Option<&str>,
    name: &str,
) -> Result<serde_json::Value, kube::Error> {
    let request = Request::new(collection_path(target, namespace));
    let http_request = if is_secret(target) {
        request.get_metadata(name, &GetParams::default())
    } else {
        request.get(name, &GetParams::default())
    }
    .map_err(kube::Error::BuildRequest)?;
    let mut value: serde_json::Value = client.request(http_request).await?;
    stamp_identity(target, &mut value);
    Ok(value)
}

// A Secret's wire shape says PartialObjectMetadata because that is what was
// fetched, and some servers omit kind on single objects -- including on the
// object an apply hands back. The document names what the object is either way,
// or the editor's copy and the server's answer would differ in their first two
// lines and every diff between them would say so.
pub(crate) fn stamp_identity(target: &KindTarget, value: &mut serde_json::Value) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    if is_secret(target) || !map.contains_key("kind") {
        map.insert("kind".into(), serde_json::Value::from(target.kind()));
        map.insert(
            "apiVersion".into(),
            serde_json::Value::from(target.resource.api_version.as_str()),
        );
    }
}

pub(crate) fn is_secret(target: &KindTarget) -> bool {
    target.group().is_empty() && target.kind() == "Secret"
}

async fn owner_chain(
    client: &Client,
    targets: &[KindTarget],
    namespace: Option<&str>,
    object: &serde_json::Value,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut current = object.clone();
    for _ in 0..MAX_OWNER_HOPS {
        let Some(owner) = controller_of(&current) else {
            break;
        };
        if seen.contains(&owner.uid) {
            lines.push(format!(
                "{} {} (owner cycle; stopping here)",
                owner.kind, owner.name
            ));
            break;
        }
        seen.push(owner.uid.clone());

        let Some(target) = targets
            .iter()
            .find(|t| t.group() == owner.group && t.kind() == owner.kind)
        else {
            lines.push(format!(
                "{} {} (kind not served by this cluster)",
                owner.kind, owner.name
            ));
            break;
        };
        let request = Request::new(collection_path(target, namespace));
        let fetched = match request.get_metadata(&owner.name, &GetParams::default()) {
            Ok(http_request) => client.request::<serde_json::Value>(http_request).await,
            Err(error) => Err(kube::Error::BuildRequest(error)),
        };
        match fetched {
            Ok(value) => {
                lines.push(format!("{} {}", owner.kind, owner.name));
                current = value;
            }
            Err(error) => {
                lines.push(format!(
                    "{} {} ({})",
                    owner.kind,
                    owner.name,
                    match &error {
                        kube::Error::Api(response) if response.code == 403 =>
                            "access denied for this account".to_string(),
                        kube::Error::Api(response) if response.code == 404 =>
                            "no longer exists".to_string(),
                        other => crate::connect::describe(other as &dyn std::error::Error),
                    }
                ));
                break;
            }
        }
    }
    lines
}

struct OwnerRef {
    group: String,
    kind: String,
    name: String,
    uid: String,
}

fn controller_of(object: &serde_json::Value) -> Option<OwnerRef> {
    let refs = object.get("metadata")?.get("ownerReferences")?.as_array()?;
    let owner = refs
        .iter()
        .find(|r| r.get("controller").and_then(|c| c.as_bool()) == Some(true))
        .or_else(|| refs.first())?;
    let text = |key: &str| {
        owner
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let api_version = text("apiVersion");
    let group = match api_version.split_once('/') {
        Some((group, _)) => group.to_string(),
        None => String::new(),
    };
    Some(OwnerRef {
        group,
        kind: text("kind"),
        name: text("name"),
        uid: text("uid"),
    })
}

async fn event_lines(client: &Client, request: &DescribeRequest) -> Vec<String> {
    let api: Api<Event> = match request.namespace.as_deref() {
        Some(namespace) => Api::namespaced(client.clone(), namespace),
        None => Api::all(client.clone()),
    };
    let selector = if request.uid.is_empty() {
        format!("involvedObject.name={}", request.name)
    } else {
        format!("involvedObject.uid={}", request.uid)
    };
    let params = ListParams::default().fields(&selector).limit(64);
    match api.list(&params).await {
        Ok(list) => {
            let mut events = list.items;
            if events.is_empty() {
                return vec!["(none recorded)".to_string()];
            }
            events.sort_by_key(|event| std::cmp::Reverse(stamp(event)));
            events
                .into_iter()
                .take(MAX_EVENTS)
                .map(|event| {
                    format!(
                        "{} {} x{}  {}  ({})",
                        event.type_.as_deref().unwrap_or("?"),
                        event.reason.as_deref().unwrap_or("?"),
                        event.count.unwrap_or(1),
                        event.message.as_deref().unwrap_or(""),
                        stamp(&event),
                    )
                })
                .collect()
        }
        Err(error) => match classify::<()>("events", &error) {
            Fetched::Denied { .. } => vec!["access denied for this account".to_string()],
            Fetched::Failed { why, .. } => vec![why],
            Fetched::Ok(()) => unreachable!("classify never returns Ok"),
        },
    }
}

fn stamp(event: &Event) -> String {
    event
        .last_timestamp
        .as_ref()
        .map(|t| t.0.to_string())
        .or_else(|| event.event_time.as_ref().map(|t| t.0.to_string()))
        .unwrap_or_default()
}

struct Doc {
    lines: Vec<String>,
    truncated: bool,
}

impl Doc {
    fn new() -> Doc {
        Doc {
            lines: Vec::new(),
            truncated: false,
        }
    }

    fn blank(&mut self) {
        self.push(0, "");
    }

    fn push(&mut self, indent: usize, text: &str) {
        if self.truncated {
            return;
        }
        if self.lines.len() >= MAX_LINES {
            self.truncated = true;
            self.lines
                .push(format!("... truncated at {MAX_LINES} lines"));
            return;
        }
        let mut line = "  ".repeat(indent);
        if text.chars().count() > MAX_LINE_CHARS {
            let cut: String = text.chars().take(MAX_LINE_CHARS).collect();
            line.push_str(&cut);
            line.push('\u{2026}');
        } else {
            line.push_str(text);
        }
        self.lines.push(line);
    }
}

// Front matter first, status last, everything else alphabetical: the order a
// person scans for, and deterministic whatever map order serde chose.
fn top_level_order(keys: &mut Vec<&String>) {
    const FRONT: [&str; 4] = ["apiVersion", "kind", "metadata", "spec"];
    keys.sort_by_key(|key| {
        let front = FRONT
            .iter()
            .position(|k| k == key)
            .map(|i| i as i32)
            .unwrap_or(i32::MAX - 1);
        let back = if *key == "status" { 1 } else { 0 };
        (back, front, (*key).clone())
    });
}

fn render_object(value: &serde_json::Value, doc: &mut Doc) {
    let Some(map) = value.as_object() else {
        doc.push(0, &scalar_text(value));
        return;
    };
    let mut keys: Vec<&String> = map.keys().collect();
    top_level_order(&mut keys);
    for key in keys {
        match map[key].as_object() {
            // metadata.managedFields is apply-machinery bookkeeping, dropped
            // exactly there -- a field of the same name anywhere else is data.
            Some(fields) if key == "metadata" => {
                doc.push(0, "metadata:");
                let mut inner: Vec<&String> = fields.keys().collect();
                inner.sort();
                for field in inner {
                    if field != "managedFields" {
                        render_entry(field, &fields[field], 1, doc);
                    }
                }
            }
            _ => render_entry(key, &map[key], 0, doc),
        }
    }
}

fn render_entry(key: &str, value: &serde_json::Value, indent: usize, doc: &mut Doc) {
    if indent >= MAX_DEPTH {
        doc.push(indent, &format!("{key}: (depth capped)"));
        return;
    }
    match value {
        serde_json::Value::Object(map) if map.is_empty() => {
            doc.push(indent, &format!("{key}: {{}}"));
        }
        serde_json::Value::Object(map) => {
            doc.push(indent, &format!("{key}:"));
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for inner in keys {
                render_entry(inner, &map[inner], indent + 1, doc);
            }
        }
        serde_json::Value::Array(items) if items.is_empty() => {
            doc.push(indent, &format!("{key}: []"));
        }
        serde_json::Value::Array(items) => {
            doc.push(indent, &format!("{key}:"));
            for item in items {
                render_item(item, indent + 1, doc);
            }
        }
        serde_json::Value::String(s) if s.contains('\n') => {
            doc.push(indent, &format!("{key}: |"));
            for line in s.lines() {
                doc.push(indent + 1, line);
            }
        }
        scalar => doc.push(indent, &format!("{key}: {}", scalar_text(scalar))),
    }
}

fn render_item(item: &serde_json::Value, indent: usize, doc: &mut Doc) {
    match item {
        serde_json::Value::Object(map) if !map.is_empty() => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut first = true;
            for inner in keys {
                if first {
                    // The first field rides the dash so a list of objects
                    // reads like YAML rather than a staircase.
                    match &map[inner] {
                        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                            doc.push(indent, "-");
                            render_entry(inner, &map[inner], indent + 1, doc);
                        }
                        scalar => {
                            doc.push(indent, &format!("- {inner}: {}", scalar_text(scalar)));
                        }
                    }
                    first = false;
                } else {
                    render_entry(inner, &map[inner], indent + 1, doc);
                }
            }
        }
        other => doc.push(indent, &format!("- {}", scalar_text(other))),
    }
}

fn scalar_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(json: serde_json::Value) -> Vec<String> {
        let mut doc = Doc::new();
        render_object(&json, &mut doc);
        doc.lines
    }

    #[test]
    fn front_matter_leads_and_status_trails_whatever_the_map_order_was() {
        let lines = rendered(serde_json::json!({
            "status": {"phase": "Running"},
            "kind": "Pod",
            "zebra": true,
            "apiVersion": "v1",
            "spec": {"nodeName": "n1"},
            "metadata": {"name": "api-1"},
        }));
        let keys: Vec<&str> = lines
            .iter()
            .filter(|l| !l.starts_with(' '))
            .map(|l| l.split(':').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            ["apiVersion", "kind", "metadata", "spec", "zebra", "status"]
        );
    }

    #[test]
    fn nested_maps_indent_and_keys_sort_deterministically() {
        let lines = rendered(serde_json::json!({
            "spec": {"b": 2, "a": 1}
        }));
        assert_eq!(lines, ["spec:", "  a: 1", "  b: 2"]);
    }

    #[test]
    fn arrays_read_as_yaml_items_with_the_first_field_on_the_dash() {
        let lines = rendered(serde_json::json!({
            "spec": {"containers": [{"name": "app", "image": "nginx"}], "empty": []}
        }));
        assert_eq!(
            lines,
            [
                "spec:",
                "  containers:",
                "    - image: nginx",
                "      name: app",
                "  empty: []",
            ]
        );
    }

    #[test]
    fn a_multiline_string_becomes_a_block_not_one_unreadable_line() {
        let lines = rendered(serde_json::json!({
            "data": {"config.yaml": "a: 1\nb: 2"}
        }));
        assert_eq!(lines, ["data:", "  config.yaml: |", "    a: 1", "    b: 2"]);
    }

    #[test]
    fn managed_fields_are_dropped_from_the_metadata_only() {
        let lines = rendered(serde_json::json!({
            "metadata": {"name": "api", "managedFields": [{"manager": "kubectl"}]},
            "spec": {"managedFields": "a real field name elsewhere"},
        }));
        assert!(lines.iter().any(|l| l.contains("name: api")));
        assert!(!lines.iter().any(|l| l.contains("kubectl")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("a real field name")),
            "only metadata.managedFields is presentation noise: {lines:?}"
        );
    }

    #[test]
    fn the_document_is_bounded_in_lines_length_and_depth() {
        let mut doc = Doc::new();
        for i in 0..(MAX_LINES + 10) {
            doc.push(0, &format!("line {i}"));
        }
        assert_eq!(doc.lines.len(), MAX_LINES + 1);
        assert!(doc.lines.last().unwrap().contains("truncated"));

        let mut doc = Doc::new();
        doc.push(0, &"x".repeat(MAX_LINE_CHARS + 50));
        assert_eq!(doc.lines[0].chars().count(), MAX_LINE_CHARS + 1);

        let mut nested = serde_json::json!("leaf");
        for _ in 0..(MAX_DEPTH + 4) {
            nested = serde_json::json!({ "k": nested });
        }
        let mut doc = Doc::new();
        render_object(&nested, &mut doc);
        assert!(
            doc.lines.iter().any(|l| l.contains("depth capped")),
            "{:?}",
            doc.lines.last()
        );
    }

    #[test]
    fn the_controller_reference_wins_over_the_first_listed_owner() {
        let object = serde_json::json!({
            "metadata": {"ownerReferences": [
                {"apiVersion": "v1", "kind": "Bystander", "name": "b", "uid": "u-b"},
                {"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "api-7f9",
                 "uid": "u-rs", "controller": true},
            ]}
        });
        let owner = controller_of(&object).expect("an owner");
        assert_eq!(owner.kind, "ReplicaSet");
        assert_eq!(owner.group, "apps");
        assert_eq!(owner.uid, "u-rs");

        let fallback = serde_json::json!({
            "metadata": {"ownerReferences": [
                {"apiVersion": "v1", "kind": "Bystander", "name": "b", "uid": "u-b"},
            ]}
        });
        assert_eq!(
            controller_of(&fallback).expect("an owner").kind,
            "Bystander"
        );
        assert!(controller_of(&serde_json::json!({"metadata": {}})).is_none());
    }
}
