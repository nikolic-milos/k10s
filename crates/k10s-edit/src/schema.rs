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
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 600;
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
    pub(crate) types: HashMap<String, Arc<SchemaNode>>,
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
    let declared = declared_type(schema);
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

// The type a schema declares, which JSON Schema also spells as a list. The
// `null` member of such a list is nullability and `declares_null` has already
// read it; what is left is the shape, and dropping it because it arrived in a
// list left hand-written CRDs indexed but unchecked -- no scalar kind, no
// enum, no completion -- while looking exactly like a schema that had been
// read.
fn declared_type(schema: &serde_json::Value) -> Option<&str> {
    match schema.get("type") {
        Some(serde_json::Value::String(name)) => Some(name.as_str()),
        Some(serde_json::Value::Array(entries)) => entries
            .iter()
            .filter_map(serde_json::Value::as_str)
            .find(|name| *name != "null"),
        _ => None,
    }
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
                // A name two members both require is one requirement, not two:
                // the validator reports one diagnostic per entry here.
                for name in member_required {
                    if !required.iter().any(|kept| kept == name) {
                        required.push(name.clone());
                    }
                }
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
#[path = "schema_fixtures.rs"]
pub(crate) mod fixtures;
