//! The structure layer: highlight spans never overlap even where captures
//! nest, incremental edits keep the tree in step with the rope, and the cursor
//! path resolves the key, prefix and siblings that completion filters on.
//! Parse errors surface as bounded ranges rather than as an absent tree.

use super::*;
use crate::buffer::{Buffer, EditGroup, SelectionIntent};

const MANIFEST: &str = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  labels:\n    app: web\nspec:\n  replicas: 3\n  template:\n    spec:\n      containers:\n        - name: web\n          image: nginx:1.27\n        - name: sidecar\n          image: envoy\n";

fn parsed(text: &str) -> (Rope, Syntax) {
    let rope = Rope::from(text);
    let mut syntax = Syntax::yaml();
    syntax.reparse(&rope);
    (rope, syntax)
}

fn keys(context: &CursorContext) -> Vec<String> {
    context
        .path
        .iter()
        .map(|segment| match segment {
            PathSeg::Key(key) => key.clone(),
            PathSeg::Index(index) => format!("[{index}]"),
        })
        .collect()
}

#[test]
fn a_clean_manifest_parses_and_highlights_keys_as_properties() {
    let (rope, syntax) = parsed(MANIFEST);
    let spans = syntax.highlights(&rope, 0..rope.len());
    assert!(!spans.is_empty());
    let api_version = spans
        .iter()
        .find(|(range, _)| range.start == 0)
        .expect("the first key is highlighted");
    assert_eq!(api_version.1, TokenKind::Property);
    assert_eq!(api_version.0.end, "apiVersion".len());
    assert!(
        spans.iter().any(|(_, kind)| *kind == TokenKind::Number),
        "replicas: 3 yields a number token"
    );
}

#[test]
fn highlight_spans_never_overlap_even_when_captures_nest() {
    let (rope, syntax) = parsed("script: |\n  line one\n  line two\nflag: true\n");
    let spans = syntax.highlights(&rope, 0..rope.len());
    for pair in spans.windows(2) {
        assert!(
            pair[0].0.end <= pair[1].0.start,
            "{:?} overlaps {:?}",
            pair[0],
            pair[1]
        );
    }
    assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Boolean));
}

#[test]
fn incremental_edits_keep_the_tree_in_step_with_the_rope() {
    let mut buffer = Buffer::new(MANIFEST);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    let offset = MANIFEST.find("replicas: 3").expect("fixture") + "replicas: ".len();
    let splices = buffer.edit(
        vec![(offset..offset + 1, "12".to_string())],
        EditGroup::Other,
        SelectionIntent::Collapse,
    );
    syntax.edit(buffer.rope(), &splices);
    let spans = syntax.highlights(buffer.rope(), 0..buffer.rope().len());
    let number = spans
        .iter()
        .find(|(range, _)| range.start == offset)
        .expect("the replacement is still a number token");
    assert_eq!(number.1, TokenKind::Number);
    assert_eq!(number.0.end - number.0.start, 2);
}

#[test]
fn multi_document_streams_split_on_the_marker() {
    let (rope, syntax) = parsed("a: 1\n---\nb: 2\n---\nc: 3\n");
    assert_eq!(syntax.document_ranges(&rope).len(), 3);
    assert_eq!(syntax.document_index_at(&rope, 1), 0);
    assert_eq!(syntax.document_index_at(&rope, 10), 1);
    assert_eq!(syntax.document_index_at(&rope, rope.len() - 1), 2);
}

#[test]
fn the_cursor_path_descends_mappings_and_sequence_items() {
    let (rope, syntax) = parsed(MANIFEST);
    let offset = MANIFEST.find("image: nginx").expect("fixture");
    let context = syntax.context_at(&rope, offset);
    assert_eq!(
        keys(&context),
        ["spec", "template", "spec", "containers", "[0]"]
    );
    assert_eq!(context.position, CursorPosition::Key);
    let second = MANIFEST.find("image: envoy").expect("fixture");
    let context = syntax.context_at(&rope, second);
    assert_eq!(
        keys(&context),
        ["spec", "template", "spec", "containers", "[1]"]
    );
}

#[test]
fn a_value_cursor_names_its_key_and_prefix() {
    let text = "spec:\n  imagePullPolicy: Alw";
    let (rope, syntax) = parsed(text);
    let context = syntax.context_at(&rope, text.len());
    assert_eq!(context.position, CursorPosition::Value);
    assert_eq!(context.value_key.as_deref(), Some("imagePullPolicy"));
    assert_eq!(context.prefix, "Alw");
    assert_eq!(
        keys(&context),
        ["spec", "imagePullPolicy"],
        "a value path ends with its key so schema resolution is one lookup"
    );
}

#[test]
fn a_half_typed_key_resolves_to_its_parent_mapping() {
    let text = "spec:\n  replicas: 3\n  temp";
    let (rope, syntax) = parsed(text);
    let context = syntax.context_at(&rope, text.len());
    assert_eq!(context.position, CursorPosition::Key);
    assert_eq!(context.prefix, "temp");
    assert_eq!(keys(&context), ["spec"]);
}

#[test]
fn an_empty_line_between_keys_still_finds_the_mapping_by_indent() {
    let text = "spec:\n  template:\n    spec:\n      ";
    let (rope, syntax) = parsed(text);
    let context = syntax.context_at(&rope, text.len());
    assert_eq!(keys(&context), ["spec", "template", "spec"]);
    assert_eq!(context.position, CursorPosition::Key);
    assert_eq!(context.prefix, "");
}

#[test]
fn mapping_keys_at_lists_the_siblings_for_completion_filtering() {
    let (rope, syntax) = parsed(MANIFEST);
    let top = syntax.mapping_keys_at(&rope, 0, &[]);
    assert_eq!(top, ["apiVersion", "kind", "metadata", "spec"]);
    let spec = syntax.mapping_keys_at(&rope, 0, &[PathSeg::Key("spec".into())]);
    assert_eq!(spec, ["replicas", "template"]);
    let container = syntax.mapping_keys_at(
        &rope,
        0,
        &[
            PathSeg::Key("spec".into()),
            PathSeg::Key("template".into()),
            PathSeg::Key("spec".into()),
            PathSeg::Key("containers".into()),
            PathSeg::Index(1),
        ],
    );
    assert_eq!(container, ["name", "image"]);
}

#[test]
fn parse_errors_surface_as_bounded_ranges() {
    let (_, syntax) = parsed("a: [1, 2\nb: }\n");
    let errors = syntax.error_ranges();
    assert!(!errors.is_empty());
    assert!(errors.len() <= MAX_ERROR_RANGES);
    for range in &errors {
        assert!(range.start < range.end, "every error range is visible");
    }
}

const SETTINGS_JSON: &str = "{\n  // the workspace theme\n  \"theme\": \"one-dark\",\n  \"left_dock_width\": 260,\n  \"panels\": [\"files\", \"kinds\"]\n}\n";

fn parsed_json(text: &str) -> (Rope, Syntax) {
    let rope = Rope::from(text);
    let mut syntax = Syntax::json();
    syntax.reparse(&rope);
    (rope, syntax)
}

#[test]
fn json_parses_with_comments_and_highlights_keys_as_properties() {
    let (rope, syntax) = parsed_json(SETTINGS_JSON);
    let spans = syntax.highlights(&rope, 0..rope.len());
    let theme_key = SETTINGS_JSON.find("\"theme\"").expect("fixture");
    assert!(
        spans
            .iter()
            .any(|(range, kind)| range.start == theme_key && *kind == TokenKind::Property),
        "a pair key is a property: {spans:?}"
    );
    assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Comment));
    assert!(spans.iter().any(|(_, kind)| *kind == TokenKind::Number));
}

#[test]
fn a_json_root_is_its_own_single_document() {
    let (rope, syntax) = parsed_json(SETTINGS_JSON);
    assert_eq!(syntax.document_ranges(&rope).len(), 1);
    assert_eq!(
        syntax.scalar_at(&rope, 0, &[PathSeg::Key("theme".into())]),
        Some("one-dark".to_string())
    );
    assert_eq!(
        syntax.scalar_at(
            &rope,
            0,
            &[PathSeg::Key("panels".into()), PathSeg::Index(1)]
        ),
        Some("kinds".to_string())
    );
}

#[test]
fn a_json_cursor_derives_key_and_value_contexts() {
    let text = "{\n  \"theme\": \"one";
    let (rope, syntax) = parsed_json(text);
    let context = syntax.context_at(&rope, text.len());
    assert_eq!(context.position, CursorPosition::Value);
    assert_eq!(context.value_key.as_deref(), Some("theme"));
    assert_eq!(context.prefix, "one");
    assert_eq!(keys(&context), ["theme"]);

    let text = "{\n  \"theme\": \"one-dark\",\n  \"le";
    let (rope, syntax) = parsed_json(text);
    let context = syntax.context_at(&rope, text.len());
    assert_eq!(context.position, CursorPosition::Key);
    assert_eq!(context.prefix, "le");
    assert!(
        keys(&context).is_empty(),
        "a top-level key completes at the root: {:?}",
        context.path
    );
}

#[test]
fn a_json_value_prefix_survives_colons_inside_the_string() {
    // The keymap file's action names contain "::" -- a line-based key
    // heuristic cuts the prefix at that colon and then completes the
    // action name onto its own tail.
    let text = "[\n  {\n    \"bindings\": {\n      \"ctrl-x\": \"k10s_shell::Edi";
    let (rope, syntax) = parsed_json(text);
    let context = syntax.context_at(&rope, text.len());
    assert_eq!(context.position, CursorPosition::Value);
    assert_eq!(
        context.prefix, "k10s_shell::Edi",
        "the whole string content is the prefix, colons and all"
    );
    assert_eq!(context.value_key.as_deref(), Some("ctrl-x"));
    assert_eq!(
        keys(&context),
        ["[0]", "bindings", "ctrl-x"],
        "the path reaches the bindings map even though the parse is broken"
    );
}

#[test]
fn a_json_key_position_is_not_confused_by_a_previous_value() {
    let text = "{\n  \"url\": \"http://example.com:8080/x\",\n  \"th";
    let (rope, syntax) = parsed_json(text);
    let context = syntax.context_at(&rope, text.len());
    assert_eq!(
        context.position,
        CursorPosition::Key,
        "a colon inside the previous value does not make this a value"
    );
    assert_eq!(context.prefix, "th");
    assert_eq!(context.value_key, None);
}

#[test]
fn an_empty_json_value_completes_with_no_prefix() {
    let text = "{\n  \"theme\": \"";
    let (rope, syntax) = parsed_json(text);
    let context = syntax.context_at(&rope, text.len());
    assert_eq!(context.position, CursorPosition::Value);
    assert_eq!(context.prefix, "");
    assert_eq!(context.value_key.as_deref(), Some("theme"));
}

#[test]
fn json_mapping_keys_list_for_completion_filtering() {
    let (rope, syntax) = parsed_json(SETTINGS_JSON);
    assert_eq!(
        syntax.mapping_keys_at(&rope, 0, &[]),
        ["theme", "left_dock_width", "panels"]
    );
}

#[test]
fn plain_text_answers_everything_with_graceful_empties() {
    let rope = Rope::from("just some notes\nwith lines\n");
    let mut syntax = Syntax::new(LanguageKind::Plain);
    syntax.reparse(&rope);
    assert!(!syntax.is_parsed());
    assert!(syntax.highlights(&rope, 0..rope.len()).is_empty());
    assert!(syntax.error_ranges().is_empty());
    assert_eq!(syntax.document_ranges(&rope).len(), 1);
}

#[test]
fn language_kind_follows_the_file_extension() {
    assert_eq!(LanguageKind::from_file_name("web.yaml"), LanguageKind::Yaml);
    assert_eq!(LanguageKind::from_file_name("WEB.YML"), LanguageKind::Yaml);
    assert_eq!(
        LanguageKind::from_file_name("settings.json"),
        LanguageKind::Json
    );
    assert_eq!(
        LanguageKind::from_file_name("notes.txt"),
        LanguageKind::Plain
    );
    assert_eq!(
        LanguageKind::from_file_name("Makefile"),
        LanguageKind::Plain
    );
}

#[test]
fn undo_shaped_full_reparse_recovers_from_any_tree_state() {
    let mut buffer = Buffer::new(MANIFEST);
    let mut syntax = Syntax::yaml();
    syntax.reparse(buffer.rope());
    buffer.edit(
        vec![(0..10, "x".to_string())],
        EditGroup::Other,
        SelectionIntent::Collapse,
    );
    buffer.undo();
    syntax.reparse(buffer.rope());
    let context = syntax.context_at(buffer.rope(), MANIFEST.find("name: web").expect("fixture"));
    assert_eq!(keys(&context), ["metadata"]);
}
