//! What a completion actually writes: a key opens the container it names, a
//! value is quoted by its schema kind rather than by its spelling, and a
//! hostile label is quoted rather than allowed to break the document.

use super::*;
use crate::syntax::{CursorPosition, Syntax};

fn item(label: &str, kind: CompletionKind) -> Completion {
    Completion {
        label: label.to_string(),
        detail: String::new(),
        documentation: String::new(),
        required: false,
        kind,
        score: 0,
    }
}

// The context the view has when it accepts: derived from the real cursor
// shape, so a test cannot invent a prefix the language would not produce.
fn context(text: &str, language: LanguageKind) -> (Rope, CursorContext) {
    let rope = Rope::from(text);
    let mut syntax = Syntax::new(language);
    syntax.reparse(&rope);
    let context = syntax.context_at(&rope, text.len());
    (rope, context)
}

fn applied(text: &str, edit: &CompletionEdit) -> (String, usize) {
    let mut out = String::from(&text[..edit.range.start]);
    out.push_str(&edit.text);
    let caret = edit.range.start + edit.caret;
    out.push_str(&text[edit.range.end..]);
    (out, caret)
}

#[test]
fn a_yaml_key_writes_its_colon_and_opens_the_container_it_names() {
    let text = "spec:\n  tem";
    let (rope, cursor) = context(text, LanguageKind::Yaml);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("template", CompletionKind::Key(Slot::Mapping)),
        text.len(),
    );
    let (out, caret) = applied(text, &edit);
    assert_eq!(out, "spec:\n  template:\n    ");
    assert_eq!(caret, out.len(), "the caret waits inside the new mapping");
    assert!(edit.reopen);

    let edit = completion_edit(
        &rope,
        &cursor,
        &item("containers", CompletionKind::Key(Slot::Sequence)),
        text.len(),
    );
    assert_eq!(applied(text, &edit).0, "spec:\n  containers:\n    - ");

    let edit = completion_edit(
        &rope,
        &cursor,
        &item("replicas", CompletionKind::Key(Slot::Scalar)),
        text.len(),
    );
    assert_eq!(applied(text, &edit).0, "spec:\n  replicas: ");
    assert!(!edit.reopen);
}

#[test]
fn a_json_key_arrives_quoted_and_never_doubles_its_delimiters() {
    let text = "{\n  \"th";
    let (rope, cursor) = context(text, LanguageKind::Json);
    assert_eq!(cursor.prefix, "th", "the prefix is the string content");
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("theme", CompletionKind::Key(Slot::Scalar)),
        text.len(),
    );
    let (out, caret) = applied(text, &edit);
    assert_eq!(
        out, "{\n  \"theme\": ",
        "the opening quote the user typed is part of the edit, not doubled"
    );
    assert_eq!(caret, out.len());

    // Editing a key that already has its quotes and its colon.
    let whole = "{\n  \"th\": 1\n}";
    let rope = Rope::from(whole);
    let mut syntax = Syntax::json();
    syntax.reparse(&rope);
    let at = whole.find("\": 1").expect("fixture");
    let cursor = syntax.context_at(&rope, at);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("theme", CompletionKind::Key(Slot::Scalar)),
        at,
    );
    let mut out = String::from(&whole[..edit.range.start]);
    out.push_str(&edit.text);
    out.push_str(&whole[edit.range.end..]);
    assert_eq!(
        out, "{\n  \"theme\": 1\n}",
        "the closing quote is replaced, and the existing colon is left alone"
    );
}

#[test]
fn a_value_ending_in_a_structural_character_keeps_its_closing_quote() {
    // The text alone cannot answer this: what precedes the final quote of
    // `"x:"` is a colon, which is exactly where a string may begin, so a
    // textual rule reads that quote as opening one and swallows the
    // terminator. The tree knows the string is closed.
    let text = "{\n  \"a\": \"x:\"";
    let (rope, cursor) = context(text, LanguageKind::Json);
    assert_eq!(cursor.prefix, "", "nothing typed yet");
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("theme", CompletionKind::Key(Slot::Scalar)),
        text.len(),
    );
    assert!(edit.range.is_empty(), "an insertion, not a replacement");
    assert_eq!(applied(text, &edit).0, "{\n  \"a\": \"x:\"\"theme\": ");
}

#[test]
fn a_json_object_key_opens_a_brace_block_with_the_caret_inside() {
    let text = "{\n  \"b";
    let (rope, cursor) = context(text, LanguageKind::Json);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("bindings", CompletionKind::Key(Slot::Mapping)),
        text.len(),
    );
    let (out, caret) = applied(text, &edit);
    assert_eq!(out, "{\n  \"bindings\": {\n    \n  }");
    assert_eq!(&out[caret..], "\n  }", "the caret is on the empty line");
    assert!(edit.reopen);

    let edit = completion_edit(
        &rope,
        &cursor,
        &item("panels", CompletionKind::Key(Slot::Sequence)),
        text.len(),
    );
    assert_eq!(applied(text, &edit).0, "{\n  \"panels\": [\n    \n  ]");
}

#[test]
fn a_json_value_is_quoted_only_when_it_is_a_string() {
    let text = "{\n  \"theme\": \"on";
    let (rope, cursor) = context(text, LanguageKind::Json);
    assert_eq!(cursor.position, CursorPosition::Value);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("one-dark", CompletionKind::Value { quoted: true }),
        text.len(),
    );
    assert_eq!(applied(text, &edit).0, "{\n  \"theme\": \"one-dark\"");

    let text = "{\n  \"vim\": tr";
    let (rope, cursor) = context(text, LanguageKind::Json);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("true", CompletionKind::Value { quoted: false }),
        text.len(),
    );
    assert_eq!(
        applied(text, &edit).0,
        "{\n  \"vim\": true",
        "a boolean is not a string"
    );
}

#[test]
fn a_yaml_value_replaces_only_the_prefix_it_is_completing() {
    let text = "spec:\n  imagePullPolicy: Alw";
    let (rope, cursor) = context(text, LanguageKind::Yaml);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("Always", CompletionKind::Value { quoted: true }),
        text.len(),
    );
    assert_eq!(
        applied(text, &edit).0,
        "spec:\n  imagePullPolicy: Always",
        "YAML values stay plain scalars"
    );
}

#[test]
fn a_yaml_value_is_quoted_by_its_schema_kind_and_not_by_its_spelling() {
    let text = "spec:\n  paused: ";
    let (rope, cursor) = context(text, LanguageKind::Yaml);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("true", CompletionKind::Value { quoted: false }),
        text.len(),
    );
    assert_eq!(
        applied(text, &edit).0,
        "spec:\n  paused: true",
        "a boolean under a boolean schema is the boolean, not the word"
    );

    // The same spelling under a string schema is a string, and bare it
    // would read back as a boolean.
    let text = "spec:\n  flag: ";
    let (rope, cursor) = context(text, LanguageKind::Yaml);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("y", CompletionKind::Value { quoted: true }),
        text.len(),
    );
    assert_eq!(applied(text, &edit).0, "spec:\n  flag: \"y\"");
}

#[test]
fn a_label_yaml_would_not_read_back_as_a_string_is_quoted() {
    // Every one of these is a letter or a digit away from looking safe, and
    // every one of them resolves to something other than a string in YAML
    // 1.1. A CRD may name a property any of them.
    let text = "spec:\n  ";
    let (rope, cursor) = context(text, LanguageKind::Yaml);
    for label in [
        "y", "N", "on", "OFF", "true", "null", "NaN", ".inf", "8080", "1.5", "<<", "=", "a:b",
        "a b", "naïve",
    ] {
        let edit = completion_edit(
            &rope,
            &cursor,
            &item(label, CompletionKind::Key(Slot::Scalar)),
            text.len(),
        );
        assert!(
            edit.text.starts_with('"'),
            "{label} must be quoted, got {}",
            edit.text
        );
    }
    for label in [
        "imagePullPolicy",
        "kubectl.kubernetes.io/x",
        "a-b_c",
        "yes-man",
    ] {
        let edit = completion_edit(
            &rope,
            &cursor,
            &item(label, CompletionKind::Key(Slot::Scalar)),
            text.len(),
        );
        assert_eq!(
            edit.text,
            format!("{label}: "),
            "an ordinary field name stays plain"
        );
    }
}

#[test]
fn a_cursor_inside_a_json_string_replaces_the_whole_string() {
    // Mid-string is where a person lands when they change their mind about
    // a value they already finished typing.
    let text = "{\n  \"theme\": \"one-dark\"\n}";
    let rope = Rope::from(text);
    let mut syntax = Syntax::json();
    syntax.reparse(&rope);
    let at = text.find("-dark").expect("fixture");
    let cursor = syntax.context_at(&rope, at);
    assert_eq!(cursor.prefix, "one", "the caret is inside the value");
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("solarized", CompletionKind::Value { quoted: true }),
        at,
    );
    let (out, caret) = applied(text, &edit);
    assert_eq!(out, "{\n  \"theme\": \"solarized\"\n}");
    assert_eq!(&out[caret..], "\n}", "the caret lands after the value");
    assert!(
        serde_json::from_str::<serde_json::Value>(&out).is_ok(),
        "an orphaned tail of the replaced string would not parse: {out}"
    );
}

#[test]
fn a_json_key_after_a_finished_value_keeps_that_value_closed() {
    // The comma is missing and completion was asked for anyway: the quote
    // before the cursor closes the previous value, and consuming it would
    // leave that value unterminated.
    let text = "{\n  \"theme\": \"one-dark\"";
    let (rope, cursor) = context(text, LanguageKind::Json);
    assert_eq!(cursor.prefix, "", "there is nothing being typed yet");
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("left_dock_width", CompletionKind::Key(Slot::Scalar)),
        text.len(),
    );
    assert_eq!(
        edit.range,
        text.len()..text.len(),
        "an insertion, not a replacement"
    );
    assert_eq!(
        applied(text, &edit).0,
        "{\n  \"theme\": \"one-dark\"\"left_dock_width\": ",
        "the finished value stays whole; the comma is the user's to add"
    );

    // Nothing typed here either, but this quote opens the value, so it is
    // the insertion's to write and must not be left doubled.
    let text = "{\n  \"theme\": \"";
    let (rope, cursor) = context(text, LanguageKind::Json);
    assert_eq!(cursor.position, CursorPosition::Value);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("one-dark", CompletionKind::Value { quoted: true }),
        text.len(),
    );
    assert_eq!(applied(text, &edit).0, "{\n  \"theme\": \"one-dark\"");
}

#[test]
fn a_bare_value_label_is_written_bare_only_when_it_is_a_literal() {
    // `quoted` is false for every non-string scalar kind, and nothing
    // upstream promises that an enum member of an integer or boolean field
    // is spelled like one -- the schema hands the entry over as it found it.
    let text = "spec:\n  size: ";
    let (rope, cursor) = context(text, LanguageKind::Yaml);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("1\nevil: true", CompletionKind::Value { quoted: false }),
        text.len(),
    );
    assert_eq!(
        applied(text, &edit).0,
        "spec:\n  size: \"1\\nevil: true\"",
        "a label that is not a number does not get to write a key"
    );

    let json = "{\n  \"width\": ";
    let (rope, cursor) = context(json, LanguageKind::Json);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("1, \"evil\": 2", CompletionKind::Value { quoted: false }),
        json.len(),
    );
    let (out, _) = applied(json, &edit);
    assert_eq!(
        out, "{\n  \"width\": \"1, \\\"evil\\\": 2\"",
        "a label that is not a number does not get to write a pair"
    );

    for label in ["0", "-1", "1.5", "-0.5e-3", "2E10", "true", "false", "null"] {
        let edit = completion_edit(
            &rope,
            &cursor,
            &item(label, CompletionKind::Value { quoted: false }),
            json.len(),
        );
        assert_eq!(edit.text, label, "a real literal still arrives bare");
    }
    for label in [
        "01", "+1", ".5", "1.", "1e", "0x10", "NaN", "inf", "1 2", "1,2", "",
    ] {
        let edit = completion_edit(
            &rope,
            &cursor,
            &item(label, CompletionKind::Value { quoted: false }),
            json.len(),
        );
        assert!(
            edit.text.starts_with('"'),
            "{label} is not a literal, got {}",
            edit.text
        );
    }
}

#[test]
fn a_hostile_schema_label_is_quoted_rather_than_breaking_the_document() {
    // Labels are the cluster's own text: a CRD can name a property
    // anything at all, and a bare colon or newline would end the document
    // somewhere the user did not ask for.
    let text = "spec:\n  ";
    let (rope, cursor) = context(text, LanguageKind::Yaml);
    let edit = completion_edit(
        &rope,
        &cursor,
        &item("odd: key\nwith", CompletionKind::Key(Slot::Scalar)),
        text.len(),
    );
    assert_eq!(
        applied(text, &edit).0,
        "spec:\n  \"odd: key\\nwith\": ",
        "quoted and escaped, so the mapping still ends where it should"
    );
}
