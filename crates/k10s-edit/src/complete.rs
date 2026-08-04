//! Completion and validation: where the cursor context meets the schema.
//!
//! `complete` answers one cursor position from the index -- keys of the
//! mapping the cursor sits in with what is already present filtered out and
//! required fields surfaced first, enum and boolean values in value
//! position, and the two special cases every manifest starts with:
//! `apiVersion` completes from the cluster's own group-versions and `kind`
//! from the kinds that apiVersion actually serves. `validate` walks whole
//! documents against the same index and reports unknown fields, enum and
//! type mismatches, and missing required fields as bounded, labelled
//! diagnostics; an absent schema silences validation rather than inventing
//! errors, because half the CRDs in a real cluster have no schema at all.
//! Whether `null` belongs anywhere is the schema's answer and never this
//! file's, for every shape and both spellings: `size: null` and the `size:`
//! that means it reach the same question.

use std::ops::Range;
use std::sync::Arc;

use crate::rope::Rope;
use crate::schema::{ScalarKind, SchemaIndex, SchemaNode, Shape};
use crate::syntax::{
    CursorContext, CursorPosition, PathSeg, ScalarClass, Syntax, mapping_under, scalar_class,
    scalar_text, sequence_under,
};

const MAX_COMPLETIONS: usize = 128;
const MAX_DIAGNOSTICS: usize = 200;
const MAX_VALIDATE_DEPTH: usize = 64;
const MAX_ENUM_IN_MESSAGE: usize = 6;

// What accepting a completion has to write, as a shape rather than as a
// type-label string: whether a key opens a mapping, a list, or takes a scalar,
// and whether a value is one the language must quote. The insertion builder
// reads this; re-deriving it from `detail` is how a JSON file ended up with
// YAML punctuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Key(Slot),
    Value { quoted: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Mapping,
    Sequence,
    Scalar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub detail: String,
    pub documentation: String,
    pub required: bool,
    pub kind: CompletionKind,
    pub(crate) score: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocMeta {
    pub api_version: Option<String>,
    pub kind: Option<String>,
}

pub fn doc_meta(rope: &Rope, syntax: &Syntax, document_index: usize) -> DocMeta {
    DocMeta {
        api_version: syntax.scalar_at(
            rope,
            document_index,
            &[PathSeg::Key("apiVersion".to_string())],
        ),
        kind: syntax.scalar_at(rope, document_index, &[PathSeg::Key("kind".to_string())]),
    }
}

pub fn complete(
    index: &SchemaIndex,
    meta: &DocMeta,
    context: &CursorContext,
    existing: &[String],
) -> Vec<Completion> {
    complete_scoped(index, Scope::Cluster(meta), context, existing)
}

// Completion against one fixed schema root -- a settings or keymap file,
// where the document's schema is known by identity rather than resolved
// from apiVersion/kind.
pub fn complete_with_root(
    index: &SchemaIndex,
    root: &Arc<SchemaNode>,
    context: &CursorContext,
    existing: &[String],
) -> Vec<Completion> {
    complete_scoped(index, Scope::Fixed(root), context, existing)
}

// How a document finds its schema root: resolved from the cluster catalog
// by apiVersion/kind, or fixed by file identity. New editable file kinds
// add a scope, not a second completion engine.
enum Scope<'a> {
    Cluster(&'a DocMeta),
    Fixed(&'a Arc<SchemaNode>),
}

impl Scope<'_> {
    fn root(&self, index: &SchemaIndex) -> Option<Arc<SchemaNode>> {
        match self {
            Scope::Cluster(meta) => resolve_root(index, meta),
            Scope::Fixed(root) => Some((*root).clone()),
        }
    }
}

fn complete_scoped(
    index: &SchemaIndex,
    scope: Scope<'_>,
    context: &CursorContext,
    existing: &[String],
) -> Vec<Completion> {
    let mut candidates = raw_candidates(index, &scope, context, existing);
    candidates.retain_mut(|candidate| match fuzzy(&context.prefix, &candidate.label) {
        Some(score) => {
            candidate.score = score;
            true
        }
        None => false,
    });
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.required.cmp(&a.required))
            .then(a.label.cmp(&b.label))
    });
    candidates.truncate(MAX_COMPLETIONS);
    candidates
}

fn raw_candidates(
    index: &SchemaIndex,
    scope: &Scope<'_>,
    context: &CursorContext,
    existing: &[String],
) -> Vec<Completion> {
    if context.position == CursorPosition::Value {
        return value_candidates(index, scope, context);
    }
    let Some(root) = scope.root(index) else {
        // Only an unidentified YAML manifest gets the apiVersion/kind/metadata
        // skeleton; a plain text file or a JSON document is not one.
        if context.path.is_empty()
            && matches!(scope, Scope::Cluster(_))
            && context.language == crate::syntax::LanguageKind::Yaml
        {
            return seed_candidates(existing);
        }
        return Vec::new();
    };
    let Some(node) = index.lookup(&root, &context.path) else {
        return Vec::new();
    };
    let node = match &node.shape {
        Shape::Array { items } => match items {
            Some(items) => index.deref(items),
            None => return Vec::new(),
        },
        Shape::Union(members) => {
            return members
                .iter()
                .flat_map(|member| object_candidates(index, &index.deref(member), existing))
                .collect();
        }
        _ => node,
    };
    object_candidates(index, &node, existing)
}

fn object_candidates(
    index: &SchemaIndex,
    node: &Arc<SchemaNode>,
    existing: &[String],
) -> Vec<Completion> {
    let Shape::Object {
        properties,
        required,
        ..
    } = &node.shape
    else {
        return Vec::new();
    };
    properties
        .iter()
        .filter(|(key, _)| !existing.iter().any(|present| present == *key))
        .map(|(key, child)| {
            let resolved = index.deref(child);
            Completion {
                label: key.clone(),
                detail: resolved.type_label(),
                documentation: if child.description.is_empty() {
                    resolved.description.clone()
                } else {
                    child.description.clone()
                },
                required: required.contains(key),
                kind: CompletionKind::Key(slot_of(&resolved)),
                score: 0,
            }
        })
        .collect()
}

// What a key's value is, so accepting it opens the right container. A union is
// a scalar as far as insertion goes: the user picks which member to write.
fn slot_of(node: &Arc<SchemaNode>) -> Slot {
    match &node.shape {
        Shape::Object { .. } => Slot::Mapping,
        Shape::Array { .. } => Slot::Sequence,
        _ => Slot::Scalar,
    }
}

fn value_candidates(
    index: &SchemaIndex,
    scope: &Scope<'_>,
    context: &CursorContext,
) -> Vec<Completion> {
    if let Scope::Cluster(meta) = scope {
        if context.path == [PathSeg::Key("apiVersion".to_string())] {
            return index
                .api_versions()
                .map(|version| Completion {
                    label: version.to_string(),
                    detail: "apiVersion".to_string(),
                    documentation: String::new(),
                    required: false,
                    kind: CompletionKind::Value { quoted: true },
                    score: 0,
                })
                .collect();
        }
        if context.path == [PathSeg::Key("kind".to_string())] {
            let kinds = match &meta.api_version {
                Some(api_version) => index.kinds_for(api_version),
                None => Vec::new(),
            };
            return kinds
                .into_iter()
                .map(|kind| Completion {
                    label: kind,
                    detail: "kind".to_string(),
                    documentation: String::new(),
                    required: false,
                    kind: CompletionKind::Value { quoted: true },
                    score: 0,
                })
                .collect();
        }
    }
    let Some(root) = scope.root(index) else {
        return Vec::new();
    };
    let Some(node) = index.lookup(&root, &context.path) else {
        return Vec::new();
    };
    if let Shape::Union(members) = &node.shape {
        return members
            .iter()
            .flat_map(|member| scalar_candidates(&index.deref(member)))
            .collect();
    }
    scalar_candidates(&node)
}

fn scalar_candidates(node: &Arc<SchemaNode>) -> Vec<Completion> {
    match &node.shape {
        Shape::Scalar { kind, values } if !values.is_empty() => values
            .iter()
            .map(|value| Completion {
                label: value.clone(),
                detail: kind.label().to_string(),
                documentation: node.description.clone(),
                required: false,
                kind: CompletionKind::Value {
                    quoted: quotes_value(*kind),
                },
                score: 0,
            })
            .collect(),
        Shape::Scalar {
            kind: ScalarKind::Boolean,
            ..
        } => ["true", "false"]
            .iter()
            .map(|value| Completion {
                label: (*value).to_string(),
                detail: "boolean".to_string(),
                documentation: node.description.clone(),
                required: false,
                kind: CompletionKind::Value { quoted: false },
                score: 0,
            })
            .collect(),
        _ => Vec::new(),
    }
}

// Whether the value is a string as far as a quoting language is concerned. An
// int-or-string enum member is spelled as text, so it quotes too.
fn quotes_value(kind: ScalarKind) -> bool {
    matches!(kind, ScalarKind::Str | ScalarKind::IntOrString)
}

fn resolve_root(index: &SchemaIndex, meta: &DocMeta) -> Option<Arc<SchemaNode>> {
    let api_version = meta.api_version.as_deref()?;
    let kind = meta.kind.as_deref()?;
    index.resolve_gvk(api_version, kind)
}

fn seed_candidates(existing: &[String]) -> Vec<Completion> {
    [
        ("apiVersion", "group/version of this object's schema"),
        ("kind", "the object kind; completes once apiVersion is set"),
        ("metadata", "standard object metadata"),
    ]
    .iter()
    .filter(|(key, _)| !existing.iter().any(|present| present == key))
    .map(|(key, documentation)| Completion {
        label: (*key).to_string(),
        detail: String::new(),
        documentation: (*documentation).to_string(),
        required: *key != "metadata",
        kind: CompletionKind::Key(if *key == "metadata" {
            Slot::Mapping
        } else {
            Slot::Scalar
        }),
        score: 0,
    })
    .collect()
}

pub fn fuzzy(prefix: &str, candidate: &str) -> Option<i64> {
    if prefix.is_empty() {
        return Some(0);
    }
    let prefix_lower = prefix.to_lowercase();
    let candidate_lower = candidate.to_lowercase();
    let mut score: i64 = 0;
    if candidate.starts_with(prefix) {
        score += 100;
    }
    if candidate_lower.starts_with(&prefix_lower) {
        score += 50;
    }
    let mut haystack = candidate_lower.chars().enumerate().peekable();
    let mut previous_index: Option<usize> = None;
    let candidate_chars: Vec<char> = candidate.chars().collect();
    for needle in prefix_lower.chars() {
        let mut found = None;
        for (index, hay) in haystack.by_ref() {
            if hay == needle {
                found = Some(index);
                break;
            }
        }
        let index = found?;
        score += match previous_index {
            Some(previous) if index == previous + 1 => 3,
            _ => 0,
        };
        let boundary = index == 0
            || candidate_chars
                .get(index.wrapping_sub(1))
                .is_some_and(|before| matches!(before, '-' | '_' | '.' | '/'))
            || candidate_chars
                .get(index)
                .is_some_and(|here| here.is_uppercase());
        if boundary {
            score += 4;
        } else if let Some(previous) = previous_index {
            score -= (index - previous - 1) as i64;
        }
        previous_index = Some(index);
    }
    Some(score)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range<usize>,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

pub fn validate(index: &SchemaIndex, rope: &Rope, syntax: &Syntax) -> Vec<Diagnostic> {
    let mut diagnostics = syntax_diagnostics(rope, syntax);
    if index.is_empty() {
        cap(&mut diagnostics);
        return diagnostics;
    }
    for (document_index, document) in syntax.document_nodes().into_iter().enumerate() {
        if diagnostics.len() >= MAX_DIAGNOSTICS {
            break;
        }
        let meta = doc_meta(rope, syntax, document_index);
        let (Some(api_version), Some(kind)) = (&meta.api_version, &meta.kind) else {
            continue;
        };
        let Some(root) = index.resolve_gvk(api_version, kind) else {
            diagnostics.push(Diagnostic {
                range: document.byte_range().start..document.byte_range().start + 1,
                severity: DiagnosticSeverity::Warning,
                message: format!("no schema for {api_version} {kind} in this cluster"),
            });
            continue;
        };
        check_node(index, rope, document, &root, 0, &mut diagnostics);
    }
    cap(&mut diagnostics);
    diagnostics
}

// Every list the editor shows is bounded, and a bound the user cannot see is
// a wrong count presented as a fact.
fn cap(diagnostics: &mut Vec<Diagnostic>) {
    if diagnostics.len() <= MAX_DIAGNOSTICS {
        return;
    }
    diagnostics.truncate(MAX_DIAGNOSTICS - 1);
    let anchor = diagnostics
        .last()
        .map(|last| last.range.clone())
        .unwrap_or(0..1);
    diagnostics.push(Diagnostic {
        range: anchor,
        severity: DiagnosticSeverity::Warning,
        message: format!("more problems below; only the first {MAX_DIAGNOSTICS} are shown"),
    });
}

// Validation against one fixed schema root: every document checks against
// the given root, the shape a settings or keymap file has.
pub fn validate_with_root(
    index: &SchemaIndex,
    rope: &Rope,
    syntax: &Syntax,
    root: &Arc<SchemaNode>,
) -> Vec<Diagnostic> {
    let mut diagnostics = syntax_diagnostics(rope, syntax);
    for document in syntax.document_nodes() {
        if diagnostics.len() >= MAX_DIAGNOSTICS {
            break;
        }
        check_node(index, rope, document, root, 0, &mut diagnostics);
    }
    cap(&mut diagnostics);
    diagnostics
}

fn syntax_diagnostics(rope: &Rope, syntax: &Syntax) -> Vec<Diagnostic> {
    let label = match syntax.language() {
        crate::syntax::LanguageKind::Yaml => "YAML syntax error",
        crate::syntax::LanguageKind::Json => "JSON syntax error",
        crate::syntax::LanguageKind::Plain => return Vec::new(),
    };
    let json = syntax.language() == crate::syntax::LanguageKind::Json;
    syntax
        .error_ranges()
        .into_iter()
        .filter(|range| !(json && crate::syntax::is_trailing_comma(rope, range)))
        .map(|range| Diagnostic {
            range,
            severity: DiagnosticSeverity::Error,
            message: label.to_string(),
        })
        .collect()
}

fn check_node(
    index: &SchemaIndex,
    rope: &Rope,
    node: tree_sitter::Node<'_>,
    schema: &Arc<SchemaNode>,
    depth: usize,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if depth > MAX_VALIDATE_DEPTH || diagnostics.len() >= MAX_DIAGNOSTICS {
        return;
    }
    let schema = index.deref(schema);
    match &schema.shape {
        Shape::Object {
            properties,
            required,
            additional,
        } => {
            let Some(mapping) = mapping_under(node) else {
                // A scalar or a list where an object belongs is a mismatch.
                // Returning quietly here read as acceptance, and it is also
                // what let a union claim a member it never engaged.
                report_mismatch(rope, node, &schema, "an object", diagnostics);
                return;
            };
            let mut present: Vec<String> = Vec::new();
            let mut walker = mapping.walk();
            let pairs: Vec<tree_sitter::Node<'_>> = mapping.named_children(&mut walker).collect();
            for pair in pairs {
                let Some(key_node) = pair.child_by_field_name("key") else {
                    continue;
                };
                let key = scalar_text(rope, key_node);
                present.push(key.clone());
                let child_schema = properties
                    .get(&key)
                    .cloned()
                    .or_else(|| additional.for_unnamed(!properties.is_empty()));
                match child_schema {
                    Some(child_schema) => match pair.child_by_field_name("value") {
                        Some(value) => {
                            check_node(index, rope, value, &child_schema, depth + 1, diagnostics);
                        }
                        // `size:` with nothing after it is how YAML spells
                        // `size: null`, so it answers to the same nullability
                        // policy. A key whose value is an indented block has a
                        // value node and never lands here; a JSON pair without
                        // one is a syntax error the parser already reported,
                        // not an implicit null.
                        None => {
                            if matches!(pair.kind(), "block_mapping_pair" | "flow_pair")
                                && let Some(message) = null_refusal(index, &child_schema, depth)
                            {
                                diagnostics.push(Diagnostic {
                                    range: key_node.byte_range(),
                                    severity: DiagnosticSeverity::Warning,
                                    message,
                                });
                            }
                        }
                    },
                    None => {
                        diagnostics.push(Diagnostic {
                            range: key_node.byte_range(),
                            severity: DiagnosticSeverity::Warning,
                            message: format!("unknown field \"{key}\""),
                        });
                    }
                }
                if diagnostics.len() >= MAX_DIAGNOSTICS {
                    return;
                }
            }
            for missing in required {
                if !present.iter().any(|key| key == missing) {
                    let anchor = mapping.byte_range().start;
                    diagnostics.push(Diagnostic {
                        range: anchor..anchor + 1,
                        severity: DiagnosticSeverity::Warning,
                        message: format!("missing required field \"{missing}\""),
                    });
                }
            }
        }
        Shape::Array { items } => {
            let Some(sequence) = sequence_under(node) else {
                report_mismatch(rope, node, &schema, "a list", diagnostics);
                return;
            };
            let Some(items) = items else {
                return;
            };
            let mut walker = sequence.walk();
            let children: Vec<tree_sitter::Node<'_>> = sequence
                .named_children(&mut walker)
                .filter(|item| !item.is_extra())
                .collect();
            for item in children {
                let inner = if item.kind() == "block_sequence_item" {
                    item.named_child(0)
                } else {
                    Some(item)
                };
                if let Some(inner) = inner {
                    check_node(index, rope, inner, items, depth + 1, diagnostics);
                }
                if diagnostics.len() >= MAX_DIAGNOSTICS {
                    return;
                }
            }
        }
        Shape::Scalar { kind, values } => {
            if mapping_under(node).is_some() || sequence_under(node).is_some() {
                diagnostics.push(Diagnostic {
                    range: node.byte_range(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("expected a {}", kind.label()),
                });
                return;
            }
            let Some(class) = scalar_class(rope, node) else {
                return;
            };
            if class == ScalarClass::Null {
                if !schema.nullable {
                    diagnostics.push(Diagnostic {
                        range: node.byte_range(),
                        severity: DiagnosticSeverity::Warning,
                        message: format!("null is not a {}", kind.label()),
                    });
                }
                return;
            }
            if *kind == ScalarKind::Null {
                diagnostics.push(Diagnostic {
                    range: node.byte_range(),
                    severity: DiagnosticSeverity::Warning,
                    message: "expected null".to_string(),
                });
                return;
            }
            let text = scalar_text(rope, node);
            if !values.is_empty() && !values.contains(&text) {
                let sample: Vec<&str> = values
                    .iter()
                    .take(MAX_ENUM_IN_MESSAGE)
                    .map(String::as_str)
                    .collect();
                let ellipsis = if values.len() > MAX_ENUM_IN_MESSAGE {
                    ", …"
                } else {
                    ""
                };
                diagnostics.push(Diagnostic {
                    range: node.byte_range(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("expected one of {}{}", sample.join(", "), ellipsis),
                });
                return;
            }
            let matches_kind = match kind {
                ScalarKind::Str => class == ScalarClass::Str,
                ScalarKind::Integer => class == ScalarClass::Int,
                ScalarKind::Number => matches!(class, ScalarClass::Int | ScalarClass::Float),
                ScalarKind::Boolean => class == ScalarClass::Bool,
                ScalarKind::IntOrString => matches!(class, ScalarClass::Int | ScalarClass::Str),
                ScalarKind::Null => class == ScalarClass::Null,
            };
            if !matches_kind {
                diagnostics.push(Diagnostic {
                    range: node.byte_range(),
                    severity: DiagnosticSeverity::Warning,
                    message: match kind {
                        ScalarKind::Str => "expected a string; quote this value".to_string(),
                        _ => format!("expected a {}", kind.label()),
                    },
                });
            }
        }
        Shape::Union(members) => {
            // The Object, Array and Scalar arms all answer `nullable` before
            // they answer shape; a union that declares it may be null has to
            // too, or its members report a value the union itself allowed.
            if schema.nullable && scalar_class(rope, node) == Some(ScalarClass::Null) {
                return;
            }
            let mut best: Option<Vec<Diagnostic>> = None;
            for member in members {
                let mut attempt = Vec::new();
                check_node(index, rope, node, member, depth + 1, &mut attempt);
                if attempt.is_empty() {
                    return;
                }
                if best.as_ref().is_none_or(|kept| attempt.len() < kept.len()) {
                    best = Some(attempt);
                }
            }
            if let Some(mut closest) = best {
                // No member accepted it, so report the nearest miss rather
                // than every member's complaint.
                diagnostics.append(&mut closest);
            }
        }
        Shape::Reference(_) | Shape::Any => {}
    }
}

// Whether `null` belongs in a slot, and if it does not, how to say so. A key
// with no value is the same `null` as one written out, so both spellings ask
// this one question and get the wording the explicit path uses. A union refuses
// only when every member does, and the first member's refusal is the one the
// explicit path would have kept as the nearest miss.
fn null_refusal(index: &SchemaIndex, schema: &Arc<SchemaNode>, depth: usize) -> Option<String> {
    if depth > MAX_VALIDATE_DEPTH {
        return None;
    }
    let schema = index.deref(schema);
    if schema.nullable {
        return None;
    }
    match &schema.shape {
        Shape::Object { .. } => Some("null is not an object".to_string()),
        Shape::Array { .. } => Some("null is not a list".to_string()),
        Shape::Scalar {
            kind: ScalarKind::Null,
            ..
        } => None,
        Shape::Scalar { kind, .. } => Some(format!("null is not a {}", kind.label())),
        Shape::Union(members) => {
            let mut refusal = None;
            for member in members {
                let member_refusal = null_refusal(index, member, depth + 1)?;
                if refusal.is_none() {
                    refusal = Some(member_refusal);
                }
            }
            refusal
        }
        Shape::Reference(_) | Shape::Any => None,
    }
}

// A value that is not the container the schema names. `null` is its own answer
// rather than a type error: it is legal exactly where the schema says it is,
// which for a CRD is `nullable` and for a built-in type is everywhere. A node
// the parser could not classify at all is left to the syntax diagnostics.
fn report_mismatch(
    rope: &Rope,
    node: tree_sitter::Node<'_>,
    schema: &Arc<SchemaNode>,
    wanted: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if scalar_class(rope, node) == Some(ScalarClass::Null) {
        if !schema.nullable {
            diagnostics.push(Diagnostic {
                range: node.byte_range(),
                severity: DiagnosticSeverity::Warning,
                message: format!("null is not {wanted}"),
            });
        }
        return;
    }
    if scalar_class(rope, node).is_some()
        || mapping_under(node).is_some()
        || sequence_under(node).is_some()
    {
        diagnostics.push(Diagnostic {
            range: node.byte_range(),
            severity: DiagnosticSeverity::Warning,
            message: format!("expected {wanted}"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::fixtures;

    fn index() -> SchemaIndex {
        let mut index = SchemaIndex::new();
        index
            .add_openapi_document(fixtures::APPS_V1_DOC)
            .expect("fixture parses");
        index
            .add_crd_list(fixtures::CRD_LIST)
            .expect("fixture parses");
        index.add_api_version("v1");
        index
    }

    fn context_for(text: &str, offset: usize) -> (Rope, Syntax, CursorContext) {
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        let context = syntax.context_at(&rope, offset);
        (rope, syntax, context)
    }

    fn deployment_meta() -> DocMeta {
        DocMeta {
            api_version: Some("apps/v1".to_string()),
            kind: Some("Deployment".to_string()),
        }
    }

    fn labels(completions: &[Completion]) -> Vec<&str> {
        completions
            .iter()
            .map(|completion| completion.label.as_str())
            .collect()
    }

    #[test]
    fn container_keys_complete_with_present_fields_filtered() {
        let text = "apiVersion: apps/v1\nkind: Deployment\nspec:\n  template:\n    spec:\n      containers:\n        - name: web\n          im";
        let (rope, syntax, context) = context_for(text, text.len());
        let existing = syntax.mapping_keys_at(&rope, 0, &context.path);
        let completions = complete(&index(), &deployment_meta(), &context, &existing);
        assert_eq!(labels(&completions), ["image", "imagePullPolicy"]);
        assert!(
            completions[0].documentation.starts_with("Container image"),
            "docs ride the completion: {:?}",
            completions[0].documentation
        );
        assert!(
            !labels(&completions).contains(&"name"),
            "a present key is not offered again"
        );
    }

    #[test]
    fn completion_survives_a_parse_recovered_as_a_bare_error_root() {
        let mut text = String::from(
            "apiVersion: apps/v1\nkind: Deployment\nspec:\n  template:\n    spec:\n      containers:\n",
        );
        for index in 0..40 {
            text.push_str(&format!(
                "        - name: worker-{index}\n          image: app:1.{index}\n"
            ));
        }
        text.push_str("        - name: extra\n          im");
        let (rope, syntax, context) = context_for(&text, text.len());
        let meta = doc_meta(&rope, &syntax, context.document_index);
        assert_eq!(
            meta.api_version.as_deref(),
            Some("apps/v1"),
            "the half-typed key wraps the whole stream in an ERROR root, and \
             the document meta must survive that"
        );
        let existing = syntax.mapping_keys_at(&rope, context.document_index, &context.path);
        let completions = complete(&index(), &meta, &context, &existing);
        assert_eq!(labels(&completions), ["image", "imagePullPolicy"]);
    }

    #[test]
    fn enum_values_complete_in_value_position() {
        let text = "apiVersion: apps/v1\nkind: Deployment\nspec:\n  template:\n    spec:\n      containers:\n        - imagePullPolicy: ";
        let (_, _, context) = context_for(text, text.len());
        let completions = complete(&index(), &deployment_meta(), &context, &[]);
        assert_eq!(labels(&completions), ["Always", "IfNotPresent", "Never"]);
    }

    #[test]
    fn booleans_complete_without_an_enum() {
        let text = "apiVersion: apps/v1\nkind: Deployment\nspec:\n  paused: ";
        let (_, _, context) = context_for(text, text.len());
        let completions = complete(&index(), &deployment_meta(), &context, &[]);
        assert_eq!(labels(&completions), ["false", "true"]);
        let typed = format!("{text}t");
        let (_, _, context) = context_for(&typed, typed.len());
        let completions = complete(&index(), &deployment_meta(), &context, &[]);
        assert_eq!(
            labels(&completions),
            ["true"],
            "the prefix filters false out"
        );
    }

    #[test]
    fn api_version_completes_from_the_cluster_catalog() {
        let text = "apiVersion: app";
        let (_, _, context) = context_for(text, text.len());
        let completions = complete(&index(), &DocMeta::default(), &context, &[]);
        assert_eq!(labels(&completions)[0], "apps/v1");
    }

    #[test]
    fn kind_completes_for_the_documents_api_version() {
        let text = "apiVersion: example.com/v1\nkind: ";
        let (rope, syntax, context) = context_for(text, text.len());
        let meta = doc_meta(&rope, &syntax, 0);
        let completions = complete(&index(), &meta, &context, &[]);
        assert_eq!(labels(&completions), ["Widget"]);
    }

    #[test]
    fn an_empty_document_seeds_the_manifest_skeleton() {
        let (_, _, context) = context_for("", 0);
        let completions = complete(&index(), &DocMeta::default(), &context, &[]);
        assert_eq!(labels(&completions), ["apiVersion", "kind", "metadata"]);
    }

    #[test]
    fn required_fields_sort_before_optional_ones_at_equal_score() {
        let text = "apiVersion: apps/v1\nkind: Deployment\nspec:\n  ";
        let (_, _, context) = context_for(text, text.len());
        let completions = complete(&index(), &deployment_meta(), &context, &[]);
        let selector = labels(&completions)
            .iter()
            .position(|label| *label == "selector")
            .expect("selector offered");
        let paused = labels(&completions)
            .iter()
            .position(|label| *label == "paused")
            .expect("paused offered");
        assert!(
            selector < paused,
            "required selector outranks optional paused"
        );
    }

    #[test]
    fn fuzzy_prefers_prefix_then_boundaries_and_rejects_non_subsequences() {
        assert!(fuzzy("img", "image").is_some());
        assert!(fuzzy("ipp", "imagePullPolicy").expect("subsequence") > 0);
        assert!(fuzzy("xyz", "image").is_none());
        let prefix = fuzzy("image", "imagePullPolicy").expect("prefix");
        let scattered = fuzzy("iaeuly", "imagePullPolicy").unwrap_or(i64::MIN);
        assert!(prefix > scattered, "a real prefix outranks a scatter");
    }

    #[test]
    fn a_crd_document_validates_against_its_structural_schema() {
        let text = "apiVersion: example.com/v1\nkind: Widget\nspec:\n  size: three\n  mode: turbo\n  extra: 1\n";
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        let diagnostics = validate(&index(), &rope, &syntax);
        let messages: Vec<&str> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("expected a integer")
                    || message.contains("expected a int")),
            "size: three is a type mismatch: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("expected one of auto, manual")),
            "mode: turbo violates the enum: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("unknown field \"extra\"")),
            "extra is unknown: {messages:?}"
        );
    }

    #[test]
    fn missing_required_fields_anchor_on_their_mapping() {
        let text = "apiVersion: apps/v1\nkind: Deployment\nspec:\n  replicas: 1\n";
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        let diagnostics = validate(&index(), &rope, &syntax);
        let missing: Vec<&str> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.starts_with("missing required"))
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert!(
            missing.iter().any(|message| message.contains("selector"))
                && missing.iter().any(|message| message.contains("template")),
            "spec requires selector and template: {missing:?}"
        );
    }

    #[test]
    fn an_unknown_gvk_is_one_labelled_warning_not_noise() {
        let text = "apiVersion: unknown.io/v9\nkind: Mystery\nspec:\n  anything: goes\n";
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        let diagnostics = validate(&index(), &rope, &syntax);
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("no schema for unknown.io/v9 Mystery")
        );
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Warning);
    }

    #[test]
    fn a_quoted_number_satisfies_a_string_schema() {
        let text =
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  labels:\n    version: \"1.27\"\n";
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        let diagnostics = validate(&index(), &rope, &syntax);
        assert!(
            diagnostics.is_empty(),
            "quoting makes it a string: {diagnostics:?}"
        );
        let unquoted =
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  labels:\n    version: 1.27\n";
        let rope = Rope::from(unquoted);
        syntax.reparse(&rope);
        let diagnostics = validate(&index(), &rope, &syntax);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("quote this value")),
            "an unquoted float under a string schema warns: {diagnostics:?}"
        );
    }

    #[test]
    fn multi_document_streams_validate_each_document_alone() {
        let text = "apiVersion: apps/v1\nkind: Deployment\nspec:\n  replicas: yes\n---\napiVersion: example.com/v1\nkind: Widget\nspec:\n  size: 3\n";
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        let diagnostics = validate(&index(), &rope, &syntax);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("expected a integer")),
            "replicas: yes fails in document one: {diagnostics:?}"
        );
        let in_second_document = text.find("size: 3").expect("fixture");
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.range.start >= in_second_document),
            "the valid second document stays clean: {diagnostics:?}"
        );
    }

    fn settings_root() -> Arc<SchemaNode> {
        use crate::schema::Shape;
        use std::collections::BTreeMap;
        let theme = Arc::new(SchemaNode {
            description: "the workspace theme".to_string(),
            shape: Shape::Scalar {
                kind: crate::schema::ScalarKind::Str,
                values: vec!["one-dark".to_string()],
            },
            nullable: false,
        });
        let width = Arc::new(SchemaNode {
            description: "left dock width in pixels".to_string(),
            shape: Shape::Scalar {
                kind: crate::schema::ScalarKind::Number,
                values: Vec::new(),
            },
            nullable: false,
        });
        let mut properties = BTreeMap::new();
        properties.insert("theme".to_string(), theme);
        properties.insert("left_dock_width".to_string(), width);
        Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Object {
                properties,
                required: Vec::new(),
                additional: crate::schema::Additional::Deny,
            },
            nullable: false,
        })
    }

    fn json_context(text: &str) -> (Rope, Syntax, CursorContext) {
        let rope = Rope::from(text);
        let mut syntax = Syntax::json();
        syntax.reparse(&rope);
        let context = syntax.context_at(&rope, text.len());
        (rope, syntax, context)
    }

    #[test]
    fn a_fixed_root_completes_json_keys_and_enum_values() {
        let root = settings_root();
        let (rope, syntax, context) = json_context("{\n  \"th");
        let existing = syntax.mapping_keys_at(&rope, 0, &context.path);
        let completions = complete_with_root(&SchemaIndex::new(), &root, &context, &existing);
        assert_eq!(
            labels(&completions)[0],
            "theme",
            "the prefix match outranks the weak subsequence"
        );
        assert_eq!(completions[0].documentation, "the workspace theme");

        let (_, _, context) = json_context("{\n  \"theme\": \"on");
        let completions = complete_with_root(&SchemaIndex::new(), &root, &context, &[]);
        assert_eq!(labels(&completions), ["one-dark"]);
    }

    #[test]
    fn a_fixed_root_validates_json_and_tolerates_jsonc() {
        let root = settings_root();
        let text = "{\n  // a comment survives\n  \"theme\": \"one-dark\",\n  \"left_dock_width\": 260,\n}\n";
        let rope = Rope::from(text);
        let mut syntax = Syntax::json();
        syntax.reparse(&rope);
        let diagnostics = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root);
        assert!(
            diagnostics.is_empty(),
            "comments and a trailing comma are the loader's own dialect: {diagnostics:?}"
        );

        let bad = "{\n  \"theme\": \"solarized\",\n  \"left_dock_width\": \"wide\",\n  \"mystery\": 1\n}\n";
        let rope = Rope::from(bad);
        syntax.reparse(&rope);
        let diagnostics = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root);
        let messages: Vec<&str> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert!(
            messages.iter().any(|message| message.contains("one-dark")),
            "the theme enum is enforced: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("expected a number")),
            "a quoted width is a type mismatch: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("unknown field \"mystery\"")),
            "unknown settings are named: {messages:?}"
        );
    }

    #[test]
    fn a_trailing_comma_is_tolerated_but_a_stray_one_is_not() {
        let root = settings_root();
        let mut syntax = Syntax::json();
        let clean = Rope::from("{\n  \"theme\": \"one-dark\",\n}\n");
        syntax.reparse(&clean);
        assert!(
            validate_with_root(&SchemaIndex::new(), &clean, &syntax, &root).is_empty(),
            "a trailing comma is the loader's own dialect"
        );
        let stray = Rope::from("{\n  \"theme\": \"one-dark\",,\n  \"left_dock_width\": 1\n}\n");
        syntax.reparse(&stray);
        let diagnostics = validate_with_root(&SchemaIndex::new(), &stray, &syntax, &root);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error),
            "a comma with a key after it is a real syntax error: {diagnostics:?}"
        );
    }

    #[test]
    fn a_keymap_binding_accepts_a_string_an_array_or_null() {
        use crate::schema::Shape;
        use std::collections::BTreeMap;
        let action = Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Scalar {
                kind: crate::schema::ScalarKind::Str,
                values: vec!["k10s_shell::EditorSave".to_string()],
            },
            nullable: false,
        });
        let with_args = Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Array { items: None },
            nullable: false,
        });
        // Unbinding a key is spelled `null`, so the union says so: acceptance
        // by a scalar arm that ignored nulls was an accident either way.
        let value = Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Union(vec![action, with_args, SchemaNode::null()]),
            nullable: false,
        });
        let root = Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Object {
                properties: BTreeMap::new(),
                required: Vec::new(),
                additional: crate::schema::Additional::Schema(value),
            },
            nullable: false,
        });
        let mut syntax = Syntax::json();
        for body in [
            "{ \"ctrl-s\": \"k10s_shell::EditorSave\" }",
            "{ \"ctrl-s\": [\"k10s_shell::EditorSave\", {}] }",
            "{ \"ctrl-s\": null }",
        ] {
            let rope = Rope::from(body);
            syntax.reparse(&rope);
            let diagnostics = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root);
            assert!(
                diagnostics.is_empty(),
                "every documented binding form must validate: {body} -> {diagnostics:?}"
            );
        }
        let rope = Rope::from("{ \"ctrl-s\": 12 }");
        syntax.reparse(&rope);
        assert!(
            !validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root).is_empty(),
            "a number is still not a binding"
        );
    }

    #[test]
    fn a_union_offers_every_members_values_for_completion() {
        use crate::schema::Shape;
        let left = Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Scalar {
                kind: crate::schema::ScalarKind::Str,
                values: vec!["alpha".to_string()],
            },
            nullable: false,
        });
        let right = Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Scalar {
                kind: crate::schema::ScalarKind::Str,
                values: vec!["beta".to_string()],
            },
            nullable: false,
        });
        let root = Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Object {
                properties: std::collections::BTreeMap::from([(
                    "mode".to_string(),
                    Arc::new(SchemaNode {
                        description: String::new(),
                        shape: Shape::Union(vec![left, right]),
                        nullable: false,
                    }),
                )]),
                required: Vec::new(),
                additional: crate::schema::Additional::Deny,
            },
            nullable: false,
        });
        let (_, _, context) = json_context("{\n  \"mode\": \"");
        let completions = complete_with_root(&SchemaIndex::new(), &root, &context, &[]);
        assert_eq!(labels(&completions), ["alpha", "beta"]);
    }

    #[test]
    fn a_plain_text_buffer_is_not_offered_a_manifest_skeleton() {
        let rope = Rope::from("");
        let mut syntax = Syntax::new(crate::syntax::LanguageKind::Plain);
        syntax.reparse(&rope);
        let context = syntax.context_at(&rope, 0);
        assert!(
            complete(&index(), &DocMeta::default(), &context, &[]).is_empty(),
            "apiVersion/kind belong to a YAML manifest, not to notes.txt"
        );
    }

    fn messages(text: &str) -> Vec<String> {
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        validate(&index(), &rope, &syntax)
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    #[test]
    fn a_scalar_where_a_container_belongs_is_a_named_mismatch() {
        // Returning quietly read as acceptance: the editor said nothing about a
        // document the apiserver would refuse outright.
        let found = messages("apiVersion: apps/v1\nkind: Deployment\nspec: 3\n");
        assert!(
            found.iter().any(|message| message == "expected an object"),
            "a number is not a DeploymentSpec: {found:?}"
        );
        let found = messages(
            "apiVersion: apps/v1\nkind: Deployment\nspec:\n  template:\n    spec:\n      containers:\n        - name: web\n          ports: 80\n",
        );
        assert!(
            found.iter().any(|message| message == "expected a list"),
            "a number is not a port list: {found:?}"
        );
    }

    #[test]
    fn null_is_accepted_where_the_schema_says_it_may_be() {
        // Kubernetes' own types decode null as the zero value, which is why
        // kubectl writes `creationTimestamp: null` into files people then open.
        let found = messages("apiVersion: apps/v1\nkind: Deployment\nspec:\n  replicas: null\n");
        assert!(
            !found.iter().any(|message| message.contains("null")),
            "a built-in field takes null as unset: {found:?}"
        );
        // A CRD's structural schema is enforced literally, so there it depends
        // on the declaration.
        let found = messages("apiVersion: example.com/v1\nkind: Widget\nspec:\n  size: null\n");
        assert!(
            found
                .iter()
                .any(|message| message == "null is not a integer"),
            "a non-nullable CRD field refuses null: {found:?}"
        );
        let found =
            messages("apiVersion: example.com/v1\nkind: Widget\nspec:\n  size: 1\n  tint: null\n");
        assert!(
            !found.iter().any(|message| message.contains("null")),
            "and `nullable: true` means what it says: {found:?}"
        );
    }

    #[test]
    fn an_open_map_keeps_its_contents_and_a_closed_one_names_the_stranger() {
        // `additionalProperties: true` was dropped on conversion, so every
        // legitimate entry of an open map came back as an unknown field.
        let found = messages(
            "apiVersion: example.com/v1\nkind: Widget\nspec:\n  size: 1\n  labels:\n    anything: 3\n",
        );
        assert!(
            found.is_empty(),
            "an open map declares its contents legal: {found:?}"
        );
        let found = messages(
            "apiVersion: example.com/v1\nkind: Widget\nspec:\n  size: 1\n  sealed:\n    on: true\n    nope: 1\n",
        );
        assert!(
            found
                .iter()
                .any(|message| message == "unknown field \"nope\""),
            "and `additionalProperties: false` closes it: {found:?}"
        );
    }

    #[test]
    fn a_nullable_union_accepts_null_and_a_plain_one_does_not() {
        // Every other arm answers `nullable` before it answers shape, so a
        // union that ignored it reported a value the schema itself declared.
        let member = |kind: ScalarKind| {
            Arc::new(SchemaNode {
                description: String::new(),
                shape: Shape::Scalar {
                    kind,
                    values: Vec::new(),
                },
                nullable: false,
            })
        };
        let root = |nullable: bool| {
            Arc::new(SchemaNode {
                description: String::new(),
                shape: Shape::Object {
                    properties: std::collections::BTreeMap::from([(
                        "target".to_string(),
                        Arc::new(SchemaNode {
                            description: String::new(),
                            shape: Shape::Union(vec![
                                member(ScalarKind::Str),
                                member(ScalarKind::Integer),
                            ]),
                            nullable,
                        }),
                    )]),
                    required: Vec::new(),
                    additional: crate::schema::Additional::Deny,
                },
                nullable: false,
            })
        };
        let rope = Rope::from("target: null\n");
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        let found = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root(true));
        assert!(
            found.is_empty(),
            "`nullable: true` on the union is the schema's own answer: {found:?}"
        );
        let found = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root(false));
        assert!(
            found
                .iter()
                .any(|diagnostic| diagnostic.message == "null is not a string"),
            "and a union that never said so still refuses null: {found:?}"
        );
    }

    #[test]
    fn a_key_with_no_value_is_the_null_it_means_in_yaml() {
        // `size:` with nothing after it is `size: null`, so warning about one
        // spelling and staying silent about the other made the rule look random.
        let found = messages("apiVersion: example.com/v1\nkind: Widget\nspec:\n  size:\n");
        assert!(
            found
                .iter()
                .any(|message| message == "null is not a integer"),
            "an empty value is the null a non-nullable field refuses: {found:?}"
        );
        let found = messages(
            "apiVersion: example.com/v1\nkind: Widget\nspec:\n  size: 1\n  tint:\n  sealed:\n    on: true\n",
        );
        assert!(
            found.is_empty(),
            "`nullable: true` takes it, and a key whose value is an indented \
             block has a value: {found:?}"
        );
    }

    #[test]
    fn an_empty_index_reports_syntax_errors_only() {
        let text = "a: [1, 2\n";
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        let diagnostics = validate(&SchemaIndex::new(), &rope, &syntax);
        assert!(!diagnostics.is_empty());
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error),
            "no schema means no schema warnings: {diagnostics:?}"
        );
    }
}
