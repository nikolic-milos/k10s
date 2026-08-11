use std::sync::Arc;

use crate::rope::Rope;
use crate::schema::{ScalarKind, SchemaIndex, SchemaNode, Shape};
use crate::syntax::{CursorContext, Syntax};

use crate::complete::*;
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
    let text =
        "{\n  // a comment survives\n  \"theme\": \"one-dark\",\n  \"left_dock_width\": 260,\n}\n";
    let rope = Rope::from(text);
    let mut syntax = Syntax::json();
    syntax.reparse(&rope);
    let diagnostics = validate_with_root(&SchemaIndex::new(), &rope, &syntax, &root);
    assert!(
        diagnostics.is_empty(),
        "comments and a trailing comma are the loader's own dialect: {diagnostics:?}"
    );

    let bad =
        "{\n  \"theme\": \"solarized\",\n  \"left_dock_width\": \"wide\",\n  \"mystery\": 1\n}\n";
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
