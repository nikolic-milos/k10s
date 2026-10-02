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
