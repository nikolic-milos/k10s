//! The cluster's own schemas, parsed into a bounded index the editor walks.
//!
//! Input is untrusted display text: OpenAPI v3 documents arrive from
//! whatever server the kubeconfig names and CRD schemas from whoever wrote
//! the CRD, so every conversion is capped -- types per document, properties
//! per object, enum values, description length, nesting depth -- and the
//! caps degrade to `Any` rather than erroring, because a partially indexed
//! schema still completes better than none. References stay lazy: a `$ref`
//! is a name resolved at walk time with a hop bound, so recursive schemas
//! (`JSONSchemaProps`) cost nothing until a path actually crosses them.
//! Nothing here fetches; the data plane hands JSON text across the seam.
//!
//! Two policies are carried rather than inferred, because guessing either one
//! silently invents or hides diagnostics. [`Additional`] keeps
//! `additionalProperties` as written -- a schema, `true`, `false`, or unstated,
//! with `x-kubernetes-preserve-unknown-fields` read as the open policy the
//! apiserver treats it as -- so a map that declares open contents is not
//! reported field by field, and an `allOf` this file could not finish merging
//! is open for the same reason: a `$ref` member's properties are a name
//! resolved at walk time, so the merged table is incomplete, and an incomplete
//! table must not be the one that names strangers.
//! And [`SchemaNode::nullable`] says whether `null` belongs there: a CRD's
//! structural schema is enforced by the apiserver, so `nullable` means exactly
//! what it says, while Kubernetes' own built-in types decode `null` as the
//! zero value for every field (`creationTimestamp: null` is what kubectl
//! itself writes), so an OpenAPI document from the cluster is nullable
//! throughout -- stated once, at conversion, instead of being an accident of
//! the validator.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::syntax::PathSeg;

const MAX_TYPES_PER_DOCUMENT: usize = 4096;
const MAX_PROPERTIES: usize = 512;
const MAX_ENUM_VALUES: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 600;
const MAX_CONVERT_DEPTH: usize = 64;
const MAX_REF_HOPS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    Str,
    Integer,
    Number,
    Boolean,
    IntOrString,
    Null,
}

impl ScalarKind {
    pub fn label(self) -> &'static str {
        match self {
            ScalarKind::Str => "string",
            ScalarKind::Integer => "integer",
            ScalarKind::Number => "number",
            ScalarKind::Boolean => "boolean",
            ScalarKind::IntOrString => "int-or-string",
            ScalarKind::Null => "null",
        }
    }
}

// What an object says about fields its property table does not name. Dropping
// this distinction is what turns a legitimately open map into a column of
// "unknown field" warnings, and what lets a closed one accept anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Additional {
    // Nothing declared. Kubernetes prunes what its schema does not name, so
    // extras on a schema that names properties do not belong; a `type: object`
    // that names none is a free-form map (`selector`, `labels`).
    Unstated,
    // `additionalProperties: true` or `{}`: extras belong, with no shape.
    Any,
    // `additionalProperties: false`: nothing but the named properties.
    Deny,
    Schema(Arc<SchemaNode>),
}

impl Additional {
    // The schema for a field the property table does not name, or None when
    // such a field does not belong here at all. `named` is whether the object
    // names any property.
    pub fn for_unnamed(&self, named: bool) -> Option<Arc<SchemaNode>> {
        match self {
            Additional::Schema(node) => Some(node.clone()),
            Additional::Any => Some(SchemaNode::any()),
            Additional::Unstated if !named => Some(SchemaNode::any()),
            Additional::Unstated | Additional::Deny => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Object {
        properties: BTreeMap<String, Arc<SchemaNode>>,
        required: Vec<String>,
        additional: Additional,
    },
    Array {
        items: Option<Arc<SchemaNode>>,
    },
    Scalar {
        kind: ScalarKind,
        values: Vec<String>,
    },
    Reference(String),
    // Any one of these shapes. OpenAPI spells it `oneOf`/`anyOf`; the keymap
    // file needs it because a binding value is a string, a two-element array,
    // or null. Completion offers every member, validation accepts any.
    Union(Vec<Arc<SchemaNode>>),
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaNode {
    pub description: String,
    pub shape: Shape,
    // Whether `null` is a value this node accepts. See the module contract:
    // stated at conversion, never inferred at validation time.
    pub nullable: bool,
}

impl SchemaNode {
    pub fn any() -> Arc<SchemaNode> {
        Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Any,
            nullable: true,
        })
    }

    // A schema whose only value is `null`, so a union can say "or null" in the
    // one place the schema author meant it.
    pub fn null() -> Arc<SchemaNode> {
        Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Scalar {
                kind: ScalarKind::Null,
                values: Vec::new(),
            },
            nullable: true,
        })
    }

    pub fn type_label(&self) -> String {
        match &self.shape {
            Shape::Object { .. } => "object".to_string(),
            Shape::Array { items } => match items {
                Some(items) => format!("[]{}", items.type_label()),
                None => "array".to_string(),
            },
            Shape::Scalar { kind, values } if values.is_empty() => kind.label().to_string(),
            Shape::Scalar { kind, .. } => format!("{} enum", kind.label()),
            Shape::Reference(name) => name.rsplit('.').next().unwrap_or(name).to_string(),
            Shape::Union(members) => members
                .iter()
                .map(|member| member.type_label())
                .filter(|label| !label.is_empty())
                .collect::<Vec<_>>()
                .join(" | "),
            Shape::Any => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GvkEntry {
    pub api_version: String,
    pub kind: String,
    type_name: String,
}

#[derive(Debug, Default)]
pub struct SchemaIndex {
    types: HashMap<String, Arc<SchemaNode>>,
    gvks: Vec<GvkEntry>,
    api_versions: BTreeSet<String>,
}

impl SchemaIndex {
    pub fn new() -> SchemaIndex {
        SchemaIndex::default()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.api_versions.is_empty()
    }

    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    pub fn add_api_version(&mut self, group_version: &str) {
        let trimmed = group_version.trim();
        if !trimmed.is_empty() {
            self.api_versions.insert(trimmed.to_string());
        }
    }

    pub fn api_versions(&self) -> impl Iterator<Item = &str> {
        self.api_versions.iter().map(String::as_str)
    }

    pub fn kinds_for(&self, api_version: &str) -> Vec<String> {
        let mut kinds: Vec<String> = self
            .gvks
            .iter()
            .filter(|entry| entry.api_version == api_version)
            .map(|entry| entry.kind.clone())
            .collect();
        kinds.sort();
        kinds.dedup();
        kinds
    }

    pub fn resolve_gvk(&self, api_version: &str, kind: &str) -> Option<Arc<SchemaNode>> {
        let entry = self
            .gvks
            .iter()
            .find(|entry| entry.api_version == api_version && entry.kind == kind)?;
        self.types.get(&entry.type_name).cloned()
    }

    pub fn deref(&self, node: &Arc<SchemaNode>) -> Arc<SchemaNode> {
        let mut current = node.clone();
        for _ in 0..MAX_REF_HOPS {
            let Shape::Reference(name) = &current.shape else {
                return current;
            };
            match self.types.get(name) {
                Some(target) => {
                    let describe = target.description.is_empty() && !current.description.is_empty();
                    let widen = current.nullable && !target.nullable;
                    let carried = if describe || widen {
                        Arc::new(SchemaNode {
                            description: if describe {
                                current.description.clone()
                            } else {
                                target.description.clone()
                            },
                            shape: target.shape.clone(),
                            nullable: target.nullable || current.nullable,
                        })
                    } else {
                        target.clone()
                    };
                    current = carried;
                }
                None => return SchemaNode::any(),
            }
        }
        SchemaNode::any()
    }

    pub fn lookup(&self, root: &Arc<SchemaNode>, path: &[PathSeg]) -> Option<Arc<SchemaNode>> {
        let mut node = self.deref(root);
        for segment in path {
            node = match (&node.shape, segment) {
                (
                    Shape::Object {
                        properties,
                        additional,
                        ..
                    },
                    PathSeg::Key(key),
                ) => {
                    let child = properties
                        .get(key)
                        .cloned()
                        .or_else(|| additional.for_unnamed(!properties.is_empty()))?;
                    self.deref(&child)
                }
                (Shape::Array { items }, PathSeg::Index(_)) => {
                    let items = items.clone()?;
                    self.deref(&items)
                }
                (Shape::Union(members), _) => {
                    let mut resolved = None;
                    for member in members {
                        if let Some(found) = self.lookup(member, std::slice::from_ref(segment)) {
                            resolved = Some(found);
                            break;
                        }
                    }
                    resolved?
                }
                (Shape::Any, _) => return Some(SchemaNode::any()),
                _ => return None,
            };
        }
        Some(node)
    }

    pub fn add_openapi_document(&mut self, json: &str) -> Result<usize, String> {
        let document: serde_json::Value =
            serde_json::from_str(json).map_err(|error| format!("openapi parse: {error}"))?;
        let Some(schemas) = document
            .pointer("/components/schemas")
            .and_then(serde_json::Value::as_object)
        else {
            return Err("openapi document has no components.schemas".to_string());
        };
        let mut added = 0;
        for (name, schema) in schemas.iter().take(MAX_TYPES_PER_DOCUMENT) {
            let node = convert(schema, 0, Nulls::ZeroValue);
            for gvk in gvk_annotations(schema) {
                self.api_versions.insert(gvk.api_version.clone());
                self.gvks.push(GvkEntry {
                    api_version: gvk.api_version,
                    kind: gvk.kind,
                    type_name: name.clone(),
                });
            }
            self.types.insert(name.clone(), node);
            added += 1;
        }
        Ok(added)
    }

    pub fn add_crd_list(&mut self, json: &str) -> Result<usize, String> {
        let document: serde_json::Value =
            serde_json::from_str(json).map_err(|error| format!("crd list parse: {error}"))?;
        let Some(items) = document.get("items").and_then(serde_json::Value::as_array) else {
            return Err("crd list has no items".to_string());
        };
        let mut added = 0;
        for item in items.iter().take(MAX_TYPES_PER_DOCUMENT) {
            let Some(group) = item
                .pointer("/spec/group")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let Some(kind) = item
                .pointer("/spec/names/kind")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let Some(versions) = item
                .pointer("/spec/versions")
                .and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            for version in versions {
                let Some(version_name) = version.get("name").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                if version.get("served").and_then(serde_json::Value::as_bool) == Some(false) {
                    continue;
                }
                let api_version = format!("{group}/{version_name}");
                let type_name = format!("crd.{api_version}.{kind}");
                let node = version
                    .pointer("/schema/openAPIV3Schema")
                    .map(|schema| augment_crd_root(convert(schema, 0, Nulls::Declared)))
                    .unwrap_or_else(SchemaNode::any);
                self.types.insert(type_name.clone(), node);
                self.api_versions.insert(api_version.clone());
                self.gvks.push(GvkEntry {
                    api_version,
                    kind: kind.to_string(),
                    type_name,
                });
                added += 1;
            }
        }
        Ok(added)
    }
}

struct GvkAnnotation {
    api_version: String,
    kind: String,
}

fn gvk_annotations(schema: &serde_json::Value) -> Vec<GvkAnnotation> {
    let Some(entries) = schema
        .get("x-kubernetes-group-version-kind")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let group = entry.get("group").and_then(serde_json::Value::as_str)?;
            let version = entry.get("version").and_then(serde_json::Value::as_str)?;
            let kind = entry.get("kind").and_then(serde_json::Value::as_str)?;
            let api_version = if group.is_empty() {
                version.to_string()
            } else {
                format!("{group}/{version}")
            };
            Some(GvkAnnotation {
                api_version,
                kind: kind.to_string(),
            })
        })
        .collect()
}

// Whether `null` is legal where the schema does not say so. See the module
// contract: the apiserver enforces a CRD's structural schema literally, while
// its own built-in types decode `null` as a zero value everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nulls {
    Declared,
    ZeroValue,
}

fn convert(schema: &serde_json::Value, depth: usize, nulls: Nulls) -> Arc<SchemaNode> {
    if depth > MAX_CONVERT_DEPTH {
        return SchemaNode::any();
    }
    let description = schema
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(cap_description)
        .unwrap_or_default();
    let nullable = nulls == Nulls::ZeroValue || declares_null(schema);
    if let Some(reference) = schema.get("$ref").and_then(serde_json::Value::as_str) {
        let name = reference
            .rsplit('/')
            .next()
            .unwrap_or(reference)
            .to_string();
        return Arc::new(SchemaNode {
            description,
            shape: Shape::Reference(name),
            nullable,
        });
    }
    if schema
        .get("x-kubernetes-int-or-string")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return Arc::new(SchemaNode {
            description,
            shape: Shape::Scalar {
                kind: ScalarKind::IntOrString,
                values: Vec::new(),
            },
            nullable,
        });
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(members) = schema.get(key).and_then(serde_json::Value::as_array)
            && !members.is_empty()
        {
            let converted: Vec<Arc<SchemaNode>> = members
                .iter()
                .take(MAX_PROPERTIES)
                .map(|member| convert(member, depth + 1, nulls))
                .collect();
            return Arc::new(SchemaNode {
                description,
                shape: Shape::Union(converted),
                nullable,
            });
        }
    }
    if let Some(all_of) = schema.get("allOf").and_then(serde_json::Value::as_array) {
        let merged = merge_all_of(all_of, depth, nulls);
        return Arc::new(SchemaNode {
            description: if description.is_empty() {
                merged.description.clone()
            } else {
                description
            },
            shape: merged.shape.clone(),
            nullable: nullable || merged.nullable,
        });
    }
    let declared = schema.get("type").and_then(serde_json::Value::as_str);
    let shape = match declared {
        Some("object") => object_shape(schema, depth, nulls),
        None if schema.get("properties").is_some() => object_shape(schema, depth, nulls),
        Some("array") => Shape::Array {
            items: schema
                .get("items")
                .map(|items| convert(items, depth + 1, nulls)),
        },
        Some("string") => scalar_shape(schema, ScalarKind::Str),
        Some("integer") => scalar_shape(schema, ScalarKind::Integer),
        Some("number") => scalar_shape(schema, ScalarKind::Number),
        Some("boolean") => scalar_shape(schema, ScalarKind::Boolean),
        _ => Shape::Any,
    };
    Arc::new(SchemaNode {
        description,
        shape,
        nullable,
    })
}

// `nullable: true` is how OpenAPI 3.0 and CRD structural schemas spell it;
// a JSON Schema `type` list including "null" is how some hand-written CRDs do.
fn declares_null(schema: &serde_json::Value) -> bool {
    if schema.get("nullable").and_then(serde_json::Value::as_bool) == Some(true) {
        return true;
    }
    schema
        .get("type")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|types| types.iter().any(|entry| entry.as_str() == Some("null")))
}

// How much an `additionalProperties` policy admits, so a merge can take the
// most permissive member rather than the first one to state anything. `Any`
// admits every unnamed field and a schema admits the ones that match it;
// neither `Deny` nor silence admits any, and silence ranks below `Deny` because
// it is the absence of a policy -- a stated closure is information, and only a
// merge where nothing was stated at all stays a free-form map.
fn merge_rank(additional: &Additional) -> u8 {
    match additional {
        Additional::Any => 3,
        Additional::Schema(_) => 2,
        Additional::Deny => 1,
        Additional::Unstated => 0,
    }
}

fn merge_all_of(members: &[serde_json::Value], depth: usize, nulls: Nulls) -> Arc<SchemaNode> {
    let converted: Vec<Arc<SchemaNode>> = members
        .iter()
        .map(|member| convert(member, depth + 1, nulls))
        .collect();
    if converted.len() == 1 {
        return converted.into_iter().next().expect("length was checked");
    }
    let mut properties = BTreeMap::new();
    let mut required = Vec::new();
    let mut additional = Additional::Unstated;
    let mut merged_any_object = false;
    let mut dropped_a_member = false;
    let mut nullable = false;
    for member in &converted {
        nullable |= member.nullable;
        match &member.shape {
            Shape::Object {
                properties: member_properties,
                required: member_required,
                additional: member_additional,
            } => {
                merged_any_object = true;
                for (key, value) in member_properties {
                    properties
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
                required.extend(member_required.iter().cloned());
                // The most permissive member wins, whichever order the members
                // arrive in: an `allOf` member that opens the map opens it for
                // the merge too.
                if merge_rank(member_additional) > merge_rank(&additional) {
                    additional = member_additional.clone();
                }
            }
            // A member that constrains nothing has nothing to merge.
            Shape::Any => {}
            // Everything else holds properties this merge cannot see: a `$ref`
            // is a name resolved at walk time, and a union has no single table.
            _ => dropped_a_member = true,
        }
    }
    if merged_any_object {
        // A member that could not be merged means the property table is
        // incomplete, and reporting unknown fields from an incomplete table is
        // worse than reporting none: every field inherited through the `$ref`
        // would be named a stranger. The inline properties still complete, and
        // the inline `required` still holds -- an `allOf` requires every member.
        let additional = if dropped_a_member {
            Additional::Any
        } else {
            additional
        };
        return Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Object {
                properties,
                required,
                additional,
            },
            nullable,
        });
    }
    converted
        .into_iter()
        .find(|member| member.shape != Shape::Any)
        .unwrap_or_else(SchemaNode::any)
}

fn object_shape(schema: &serde_json::Value, depth: usize, nulls: Nulls) -> Shape {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .take(MAX_PROPERTIES)
                .map(|(key, value)| (key.clone(), convert(value, depth + 1, nulls)))
                .collect()
        })
        .unwrap_or_default();
    let required = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // `true` and `{}` both mean "extras allowed, shape unspecified"; `false`
    // means none. Dropping the booleans reported every legitimate extra field.
    let additional = match schema.get("additionalProperties") {
        Some(serde_json::Value::Bool(true)) => Additional::Any,
        Some(serde_json::Value::Bool(false)) => Additional::Deny,
        Some(serde_json::Value::Object(map)) if map.is_empty() => Additional::Any,
        Some(value) if value.is_object() => Additional::Schema(convert(value, depth + 1, nulls)),
        // `x-kubernetes-preserve-unknown-fields: true` is the apiserver's own
        // marker for "extras belong here": it turns off the pruning that makes
        // silence mean closed, so it is not silence any more.
        _ if schema
            .get("x-kubernetes-preserve-unknown-fields")
            .and_then(serde_json::Value::as_bool)
            == Some(true) =>
        {
            Additional::Any
        }
        // Nothing stated, or a non-object non-boolean that is not a policy we
        // can read: keep the Kubernetes default rather than inventing one.
        _ => Additional::Unstated,
    };
    Shape::Object {
        properties,
        required,
        additional,
    }
}

fn scalar_shape(schema: &serde_json::Value, kind: ScalarKind) -> Shape {
    let values = schema
        .get("enum")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .take(MAX_ENUM_VALUES)
                .map(|entry| match entry.as_str() {
                    Some(text) => text.to_string(),
                    None => entry.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Shape::Scalar { kind, values }
}

fn augment_crd_root(node: Arc<SchemaNode>) -> Arc<SchemaNode> {
    let Shape::Object {
        properties,
        required,
        additional,
    } = &node.shape
    else {
        return node;
    };
    let mut properties = properties.clone();
    for (key, description) in [
        ("apiVersion", "group/version of this object's schema"),
        ("kind", "the kind this custom resource declares"),
    ] {
        properties.entry(key.to_string()).or_insert_with(|| {
            Arc::new(SchemaNode {
                description: description.to_string(),
                shape: Shape::Scalar {
                    kind: ScalarKind::Str,
                    values: Vec::new(),
                },
                nullable: false,
            })
        });
    }
    properties.entry("metadata".to_string()).or_insert_with(|| {
        Arc::new(SchemaNode {
            description: "standard object metadata".to_string(),
            shape: Shape::Any,
            nullable: true,
        })
    });
    Arc::new(SchemaNode {
        description: node.description.clone(),
        shape: Shape::Object {
            properties,
            required: required.clone(),
            additional: additional.clone(),
        },
        nullable: node.nullable,
    })
}

fn cap_description(text: &str) -> String {
    let mut capped: String = text.chars().take(MAX_DESCRIPTION_CHARS).collect();
    if capped.len() < text.len() {
        capped.push('…');
    }
    capped
}

#[cfg(test)]
pub(crate) mod fixtures {
    pub const APPS_V1_DOC: &str = r##"{
      "openapi": "3.0.0",
      "components": { "schemas": {
        "io.k8s.api.apps.v1.Deployment": {
          "description": "Deployment enables declarative updates for Pods and ReplicaSets.",
          "type": "object",
          "properties": {
            "apiVersion": { "type": "string", "description": "APIVersion defines the versioned schema." },
            "kind": { "type": "string" },
            "metadata": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta" }] },
            "spec": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.api.apps.v1.DeploymentSpec" }], "description": "Specification of the desired behavior of the Deployment." }
          },
          "x-kubernetes-group-version-kind": [{ "group": "apps", "version": "v1", "kind": "Deployment" }]
        },
        "io.k8s.api.apps.v1.DeploymentSpec": {
          "type": "object",
          "required": ["selector", "template"],
          "properties": {
            "replicas": { "type": "integer", "description": "Number of desired pods." },
            "paused": { "type": "boolean" },
            "selector": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.LabelSelector" }] },
            "template": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.api.core.v1.PodTemplateSpec" }] }
          }
        },
        "io.k8s.api.core.v1.PodTemplateSpec": {
          "type": "object",
          "properties": {
            "metadata": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta" }] },
            "spec": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.api.core.v1.PodSpec" }] }
          }
        },
        "io.k8s.api.core.v1.PodSpec": {
          "type": "object",
          "required": ["containers"],
          "properties": {
            "containers": { "type": "array", "items": { "$ref": "#/components/schemas/io.k8s.api.core.v1.Container" }, "description": "List of containers belonging to the pod." },
            "hostNetwork": { "type": "boolean" },
            "restartPolicy": { "type": "string", "enum": ["Always", "OnFailure", "Never"], "description": "Restart policy for all containers within the pod." }
          }
        },
        "io.k8s.api.core.v1.Container": {
          "type": "object",
          "required": ["name"],
          "properties": {
            "name": { "type": "string", "description": "Name of the container." },
            "image": { "type": "string", "description": "Container image name." },
            "imagePullPolicy": { "type": "string", "enum": ["Always", "Never", "IfNotPresent"], "description": "Image pull policy." },
            "ports": { "type": "array", "items": { "$ref": "#/components/schemas/io.k8s.api.core.v1.ContainerPort" } }
          }
        },
        "io.k8s.api.core.v1.ContainerPort": {
          "type": "object",
          "required": ["containerPort"],
          "properties": {
            "containerPort": { "type": "integer" },
            "name": { "type": "string" },
            "protocol": { "type": "string", "enum": ["TCP", "UDP", "SCTP"] }
          }
        },
        "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta": {
          "type": "object",
          "properties": {
            "name": { "type": "string", "description": "Name must be unique within a namespace." },
            "namespace": { "type": "string" },
            "labels": { "type": "object", "additionalProperties": { "type": "string" } },
            "annotations": { "type": "object", "additionalProperties": { "type": "string" } }
          }
        },
        "io.k8s.apimachinery.pkg.apis.meta.v1.LabelSelector": {
          "type": "object",
          "properties": {
            "matchLabels": { "type": "object", "additionalProperties": { "type": "string" } }
          }
        }
      } }
    }"##;

    pub const CRD_LIST: &str = r#"{
      "kind": "CustomResourceDefinitionList",
      "apiVersion": "apiextensions.k8s.io/v1",
      "items": [{
        "metadata": { "name": "widgets.example.com" },
        "spec": {
          "group": "example.com",
          "names": { "kind": "Widget", "plural": "widgets" },
          "scope": "Namespaced",
          "versions": [{
            "name": "v1",
            "served": true,
            "storage": true,
            "schema": { "openAPIV3Schema": {
              "type": "object",
              "properties": {
                "spec": {
                  "type": "object",
                  "required": ["size"],
                  "properties": {
                    "size": { "type": "integer", "description": "How many widget units." },
                    "mode": { "type": "string", "enum": ["auto", "manual"] },
                    "tint": { "type": "string", "nullable": true },
                    "labels": { "type": "object", "additionalProperties": true },
                    "sealed": {
                      "type": "object",
                      "properties": { "on": { "type": "boolean" } },
                      "additionalProperties": false
                    }
                  }
                }
              }
            } }
          }, {
            "name": "v2alpha1",
            "served": false,
            "storage": false
          }]
        }
      }]
    }"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> SchemaIndex {
        let mut index = SchemaIndex::new();
        index
            .add_openapi_document(fixtures::APPS_V1_DOC)
            .expect("the fixture parses");
        index
            .add_crd_list(fixtures::CRD_LIST)
            .expect("the fixture parses");
        index
    }

    // The converted `spec` of a one-version CRD, so a conversion question can
    // be asked of one fixed path.
    fn crd_spec(spec_schema: &str) -> Arc<SchemaNode> {
        let mut index = SchemaIndex::new();
        index
            .add_crd_list(&format!(
                r#"{{"items":[{{"spec":{{
                    "group":"example.com",
                    "names":{{"kind":"Probe"}},
                    "versions":[{{"name":"v1","served":true,"schema":{{"openAPIV3Schema":{{
                        "type":"object",
                        "properties":{{"spec":{spec_schema}}}
                    }}}}}}]
                }}}}]}}"#
            ))
            .expect("the fixture parses");
        let root = index
            .resolve_gvk("example.com/v1", "Probe")
            .expect("the version indexes");
        let Shape::Object { properties, .. } = &index.deref(&root).shape else {
            panic!("the CRD root is an object");
        };
        properties.get("spec").cloned().expect("spec is declared")
    }

    fn path(segments: &[&str]) -> Vec<PathSeg> {
        segments
            .iter()
            .map(|segment| match segment.strip_prefix('[') {
                Some(rest) => PathSeg::Index(
                    rest.trim_end_matches(']')
                        .parse()
                        .expect("test indices are numbers"),
                ),
                None => PathSeg::Key((*segment).to_string()),
            })
            .collect()
    }

    #[test]
    fn a_gvk_resolves_through_the_annotation_to_its_type() {
        let index = index();
        let deployment = index
            .resolve_gvk("apps/v1", "Deployment")
            .expect("the annotation maps apps/v1 Deployment");
        assert!(deployment.description.starts_with("Deployment enables"));
    }

    #[test]
    fn a_deep_path_crosses_refs_arrays_and_enums() {
        let index = index();
        let root = index.resolve_gvk("apps/v1", "Deployment").expect("fixture");
        let policy = index
            .lookup(
                &root,
                &path(&[
                    "spec",
                    "template",
                    "spec",
                    "containers",
                    "[0]",
                    "imagePullPolicy",
                ]),
            )
            .expect("the path resolves across four refs and an array");
        let Shape::Scalar { kind, values } = &policy.shape else {
            panic!("imagePullPolicy is an enum scalar, got {policy:?}");
        };
        assert_eq!(*kind, ScalarKind::Str);
        assert_eq!(values, &["Always", "Never", "IfNotPresent"]);
    }

    #[test]
    fn all_of_wrapped_refs_keep_the_outer_description() {
        let index = index();
        let root = index.resolve_gvk("apps/v1", "Deployment").expect("fixture");
        let Shape::Object { properties, .. } = &index.deref(&root).shape else {
            panic!("a Deployment is an object");
        };
        let spec = properties.get("spec").expect("spec exists");
        assert!(
            spec.description.starts_with("Specification of the desired"),
            "the allOf wrapper's description survives: {:?}",
            spec.description
        );
        let resolved = index.deref(spec);
        assert!(matches!(resolved.shape, Shape::Object { .. }));
        assert!(
            resolved.description.starts_with("Specification"),
            "deref carries the wrapper description onto the bare target"
        );
    }

    #[test]
    fn additional_properties_answer_arbitrary_label_keys() {
        let index = index();
        let root = index.resolve_gvk("apps/v1", "Deployment").expect("fixture");
        let label = index
            .lookup(&root, &path(&["metadata", "labels", "app"]))
            .expect("labels take arbitrary keys");
        assert!(matches!(
            label.shape,
            Shape::Scalar {
                kind: ScalarKind::Str,
                ..
            }
        ));
    }

    #[test]
    fn a_missing_property_is_none_not_any() {
        let index = index();
        let root = index.resolve_gvk("apps/v1", "Deployment").expect("fixture");
        assert_eq!(index.lookup(&root, &path(&["spec", "replicaCount"])), None);
    }

    #[test]
    fn a_served_crd_version_indexes_and_an_unserved_one_does_not() {
        let index = index();
        let widget = index
            .resolve_gvk("example.com/v1", "Widget")
            .expect("the served version indexes");
        assert!(
            index
                .resolve_gvk("example.com/v2alpha1", "Widget")
                .is_none()
        );
        let size = index
            .lookup(&widget, &path(&["spec", "size"]))
            .expect("the structural schema resolves");
        assert!(size.description.starts_with("How many"));
        let Shape::Object { properties, .. } = &index.deref(&widget).shape else {
            panic!("the CRD root is an object");
        };
        assert!(
            properties.contains_key("apiVersion") && properties.contains_key("metadata"),
            "the CRD root is augmented with the implicit object fields"
        );
    }

    #[test]
    fn kinds_and_api_versions_serve_the_completion_lists() {
        let mut index = index();
        index.add_api_version("v1");
        index.add_api_version("batch/v1");
        assert_eq!(index.kinds_for("apps/v1"), ["Deployment"]);
        assert_eq!(index.kinds_for("example.com/v1"), ["Widget"]);
        let versions: Vec<&str> = index.api_versions().collect();
        assert!(versions.contains(&"apps/v1"));
        assert!(versions.contains(&"example.com/v1"));
        assert!(versions.contains(&"batch/v1"));
    }

    #[test]
    fn an_unresolvable_ref_degrades_to_any_never_a_panic() {
        let mut index = SchemaIndex::new();
        index
            .add_openapi_document(
                r##"{"components":{"schemas":{
                    "a.b.Loop": {"type":"object","properties":{"next":{"$ref":"#/components/schemas/a.b.Missing"}}}
                }}}"##,
            )
            .expect("parses");
        let root = index.types.get("a.b.Loop").cloned().expect("indexed");
        let next = index
            .lookup(&root, &path(&["next", "anything", "deeper"]))
            .expect("Any absorbs any deeper path");
        assert_eq!(next.shape, Shape::Any);
    }

    #[test]
    fn recursive_schemas_stay_walkable_under_the_hop_bound() {
        let mut index = SchemaIndex::new();
        index
            .add_openapi_document(
                r##"{"components":{"schemas":{
                    "a.b.Node": {"type":"object","properties":{"child":{"$ref":"#/components/schemas/a.b.Node"}}}
                }}}"##,
            )
            .expect("parses");
        let root = index.types.get("a.b.Node").cloned().expect("indexed");
        let deep = index.lookup(&root, &path(&["child", "child", "child", "child", "child"]));
        assert!(deep.is_some(), "recursion resolves level by level");
    }

    #[test]
    fn malformed_documents_are_labelled_errors() {
        let mut index = SchemaIndex::new();
        assert!(index.add_openapi_document("not json").is_err());
        assert!(
            index
                .add_openapi_document(r#"{"openapi":"3.0.0"}"#)
                .is_err()
        );
        assert!(index.add_crd_list(r#"{"kind":"List"}"#).is_err());
        assert!(index.is_empty());
    }

    #[test]
    fn descriptions_are_capped_as_untrusted_display_text() {
        let mut index = SchemaIndex::new();
        let long = "x".repeat(10_000);
        index
            .add_openapi_document(&format!(
                r#"{{"components":{{"schemas":{{
                    "a.b.C": {{"type":"string","description":"{long}"}}
                }}}}}}"#
            ))
            .expect("parses");
        let node = index.types.get("a.b.C").expect("indexed");
        assert!(node.description.chars().count() <= MAX_DESCRIPTION_CHARS + 1);
    }

    #[test]
    fn an_all_of_with_an_unmergeable_member_stops_claiming_to_be_closed() {
        // A `$ref` member resolves by name at walk time, so the merged property
        // table holds only the inline members' fields. Claiming to be closed
        // from that table named every inherited field an unknown one.
        let spec = crd_spec(
            r##"{"allOf":[
                {"$ref":"#/definitions/Base"},
                {"type":"object","properties":{"extra":{"type":"string"}}}
            ]}"##,
        );
        let Shape::Object {
            properties,
            additional,
            ..
        } = &spec.shape
        else {
            panic!("an allOf with an object member merges to an object, got {spec:?}");
        };
        assert!(
            properties.contains_key("extra"),
            "the inline member's properties still serve completion"
        );
        assert_eq!(
            *additional,
            Additional::Any,
            "and the incomplete table reports nothing rather than strangers"
        );
    }

    #[test]
    fn preserve_unknown_fields_opens_the_object_that_marks_itself() {
        let spec = crd_spec(
            r#"{"type":"object","x-kubernetes-preserve-unknown-fields":true,
                "properties":{"size":{"type":"integer"}}}"#,
        );
        let Shape::Object {
            properties,
            additional,
            ..
        } = &spec.shape
        else {
            panic!("the marked schema is still an object, got {spec:?}");
        };
        assert!(properties.contains_key("size"), "named properties survive");
        assert_eq!(
            *additional,
            Additional::Any,
            "the apiserver's own marker turns pruning off, so extras belong"
        );
    }

    #[test]
    fn the_most_permissive_all_of_member_decides_in_either_order() {
        let additional_of = |spec: &str| {
            let node = crd_spec(spec);
            let Shape::Object { additional, .. } = &node.shape else {
                panic!("an allOf of objects merges to an object, got {node:?}");
            };
            additional.clone()
        };
        let closed = r#"{"type":"object","properties":{"a":{"type":"string"}},"additionalProperties":false}"#;
        let open =
            r#"{"type":"object","properties":{"b":{"type":"string"}},"additionalProperties":true}"#;
        let silent = r#"{"type":"object","properties":{"c":{"type":"string"}}}"#;
        for (first, second) in [(closed, open), (open, closed)] {
            assert_eq!(
                additional_of(&format!(r#"{{"allOf":[{first},{second}]}}"#)),
                Additional::Any,
                "the open member wins whichever order it arrives in"
            );
        }
        for (first, second) in [(closed, silent), (silent, closed)] {
            assert_eq!(
                additional_of(&format!(r#"{{"allOf":[{first},{second}]}}"#)),
                Additional::Deny,
                "and a stated closure outranks silence, which states nothing"
            );
        }
    }

    #[test]
    fn nullable_survives_a_ref_hop_the_crd_root_and_array_items() {
        let mut index = SchemaIndex::new();
        for (name, nullable) in [("Plain", false), ("Nullable", true)] {
            index.types.insert(
                name.to_string(),
                Arc::new(SchemaNode {
                    description: String::new(),
                    shape: Shape::Scalar {
                        kind: ScalarKind::Str,
                        values: Vec::new(),
                    },
                    nullable,
                }),
            );
        }
        let reference = |name: &str, nullable: bool| {
            Arc::new(SchemaNode {
                description: String::new(),
                shape: Shape::Reference(name.to_string()),
                nullable,
            })
        };
        assert!(
            index.deref(&reference("Plain", true)).nullable,
            "a nullable `$ref` keeps it across the hop"
        );
        assert!(
            index.deref(&reference("Nullable", false)).nullable,
            "and the target's own declaration is not dropped either"
        );

        let spec = crd_spec(
            r#"{"type":"object","nullable":true,"properties":{
                "hosts":{"type":"array","items":{"type":"string","nullable":true}},
                "ports":{"type":"array","items":{"type":"integer"}}}}"#,
        );
        // `spec` is read back out of the augmented root, so its flag surviving
        // is what says the root augmentation rebuilt the object without
        // dropping what its properties declared.
        assert!(spec.nullable, "the object's own declaration survives");
        let Shape::Object { properties, .. } = &spec.shape else {
            panic!("spec is an object, got {spec:?}");
        };
        for (key, expected) in [("hosts", true), ("ports", false)] {
            let node = properties.get(key).expect("the array is declared");
            let Shape::Array { items: Some(items) } = &node.shape else {
                panic!("{key} is an array with items, got {node:?}");
            };
            assert_eq!(
                items.nullable, expected,
                "an array's items carry their own declaration, not the array's"
            );
        }
    }
}
