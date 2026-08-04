//! Accepting a completion, in the language the buffer actually is.
//!
//! One pure builder owns the whole insertion: the range it replaces, the text
//! it writes, where the caret lands inside that text, and whether the result
//! is a container worth completing into again. Everything language-shaped
//! lives here -- YAML writes `key:` and an indented continuation, JSON writes
//! a quoted key, a colon, and a brace block -- because a view that appends
//! YAML punctuation into a JSON file produces a document neither parser
//! accepts. The range is computed rather than assumed: a JSON token being
//! typed owns the quotes of the string it sits in -- both of them, and only
//! when they are that string's -- and a key that already has its colon must
//! not grow a second.
//!
//! Labels come from the cluster's own schemas, which are untrusted display
//! text, so a label that would not survive as a bare scalar is quoted and
//! escaped instead of breaking the document. That holds for a value the schema
//! calls a number as much as for a key: bare is for what reads back bare.

use std::ops::Range;

use crate::buffer::INDENT;
use crate::complete::{Completion, CompletionKind, Slot};
use crate::rope::Rope;
use crate::syntax::{CursorContext, LanguageKind, StringSite};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionEdit {
    // The whole span the insertion owns, in buffer bytes.
    pub range: Range<usize>,
    pub text: String,
    // Where the caret belongs, as a byte offset into `text`.
    pub caret: usize,
    // Whether this opened a container the user is now inside, so completion
    // should offer that container's keys straight away.
    pub reopen: bool,
}

impl CompletionEdit {
    fn ending_at(range: Range<usize>, text: String) -> CompletionEdit {
        CompletionEdit {
            caret: text.len(),
            range,
            text,
            reopen: false,
        }
    }
}

pub fn completion_edit(
    rope: &Rope,
    context: &CursorContext,
    item: &Completion,
    cursor: usize,
) -> CompletionEdit {
    match context.language {
        LanguageKind::Json => json_edit(rope, context, item, cursor),
        LanguageKind::Yaml | LanguageKind::Plain => yaml_edit(rope, context, item, cursor),
    }
}

fn yaml_edit(
    rope: &Rope,
    context: &CursorContext,
    item: &Completion,
    cursor: usize,
) -> CompletionEdit {
    let range = cursor.saturating_sub(context.prefix.len())..cursor;
    let slot = match item.kind {
        // Whether a value is quoted is the schema's answer and not the word's:
        // `true` under a boolean schema is the boolean, and quoting it would
        // make the validator report the type mismatch the editor just wrote.
        // The label still has to read back as what the schema called it: a
        // string-kinded one is checked for ambiguity, which is what a string
        // enum member spelled `y` needs, and a bare one for being a literal.
        CompletionKind::Value { quoted: false } => {
            return CompletionEdit::ending_at(range, bare_value(&item.label));
        }
        CompletionKind::Value { quoted: true } => {
            return CompletionEdit::ending_at(range, yaml_scalar(&item.label));
        }
        CompletionKind::Key(slot) => slot,
    };
    let label = yaml_scalar(&item.label);
    if opens_a_value(rope, cursor) {
        return CompletionEdit::ending_at(range, label);
    }
    let indent = line_indent(rope, cursor);
    let text = match slot {
        Slot::Scalar => format!("{label}: "),
        Slot::Mapping => format!("{label}:\n{indent}{INDENT}"),
        Slot::Sequence => format!("{label}:\n{indent}{INDENT}- "),
    };
    CompletionEdit {
        reopen: slot != Slot::Scalar,
        ..CompletionEdit::ending_at(range, text)
    }
}

fn json_edit(
    rope: &Rope,
    context: &CursorContext,
    item: &Completion,
    cursor: usize,
) -> CompletionEdit {
    // The token being typed starts after its opening quote, and the string it
    // sits in runs on to a closing one; the edit owns both delimiters so it can
    // write its own. A quote before the token is only the token's when it opens
    // a string: with nothing typed yet it is just as likely the quote that
    // closed the value above, and swallowing that leaves the value unterminated
    // and the rest of its content orphaned after the replacement.
    let token_start = cursor.saturating_sub(context.prefix.len());
    // A non-empty prefix means the quote before the token opened it by
    // construction. With an empty prefix the text cannot tell an opening quote
    // from a closing one -- `{"a": "x:"` ends in both a `:` and a quote, and
    // reading that as an opening quote eats the value's terminator -- so the
    // tree answers where it can, and the text answers only where the tree has
    // no string node at all, which is the quote just typed.
    let owns_quote = !context.prefix.is_empty()
        || match context.string_site {
            StringSite::Inside => true,
            StringSite::After => false,
            StringSite::Outside => opens_a_string(rope, token_start.saturating_sub(1)),
        };
    let opening = match token_start.checked_sub(1) {
        Some(before) if rope.char_at(before) == Some('"') && owns_quote => Some(before),
        _ => None,
    };
    let (start, end) = match opening {
        Some(quote) => (quote, string_end(rope, cursor)),
        None => (token_start, cursor),
    };
    let range = start..end;
    let slot = match item.kind {
        CompletionKind::Value { quoted } => {
            let text = if quoted {
                json_string(&item.label)
            } else {
                bare_value(&item.label)
            };
            return CompletionEdit::ending_at(range, text);
        }
        CompletionKind::Key(slot) => slot,
    };
    let key = json_string(&item.label);
    if opens_a_value(rope, end) {
        return CompletionEdit::ending_at(range, key);
    }
    let indent = line_indent(rope, cursor);
    match slot {
        Slot::Scalar => CompletionEdit::ending_at(range, format!("{key}: ")),
        Slot::Mapping => block(range, format!("{key}: {{"), &indent, "}", true),
        Slot::Sequence => block(range, format!("{key}: ["), &indent, "]", false),
    }
}

// A JSON container written open, with the caret on its own indented line and
// the closing bracket back at the key's indent -- the shape a person would
// have typed.
fn block(
    range: Range<usize>,
    open: String,
    indent: &str,
    close: &str,
    reopen: bool,
) -> CompletionEdit {
    let mut text = open;
    text.push('\n');
    text.push_str(indent);
    text.push_str(INDENT);
    let caret = text.len();
    text.push('\n');
    text.push_str(indent);
    text.push_str(close);
    CompletionEdit {
        range,
        text,
        caret,
        reopen,
    }
}

// Whether a quote at `offset` begins a string rather than ending one, read from
// the text because the tree has no node there. In JSON a string starts only
// where a key or a value may: after `:` `,` `[` `{` or at the document start.
fn opens_a_string(rope: &Rope, offset: usize) -> bool {
    let mut at = offset;
    while at > 0 {
        let previous = rope.prev_char_offset(at);
        match rope.char_at(previous) {
            Some(character) if character.is_whitespace() => at = previous,
            Some(':' | ',' | '[' | '{') => return true,
            _ => return false,
        }
    }
    true
}

// Whether the cursor already sits on a pair that has its separator, so the
// insertion is replacing a key in place rather than writing a new one.
fn opens_a_value(rope: &Rope, offset: usize) -> bool {
    let mut at = offset;
    while let Some(character) = rope.char_at(at) {
        match character {
            ' ' | '\t' => at = rope.next_char_offset(at),
            ':' => return true,
            _ => return false,
        }
    }
    false
}

// The end of the string literal the cursor is inside: past its closing quote,
// or the cursor itself while the string is still open. A JSON string holds no
// literal newline, so a line ending stops the search rather than letting the
// edit reach into the pair below.
fn string_end(rope: &Rope, cursor: usize) -> usize {
    let mut at = cursor;
    while let Some(character) = rope.char_at(at) {
        at = rope.next_char_offset(at);
        match character {
            '\n' => return cursor,
            '"' => return at,
            '\\' => at = rope.next_char_offset(at),
            _ => {}
        }
    }
    cursor
}

fn line_indent(rope: &Rope, offset: usize) -> String {
    let row = rope.byte_to_point(offset).row;
    let start = rope.line_start(row);
    let mut indent = String::new();
    let mut at = start;
    while let Some(character) = rope.char_at(at) {
        if character != ' ' && character != '\t' {
            break;
        }
        indent.push(character);
        at = rope.next_char_offset(at);
    }
    indent
}

// Anything a YAML 1.1 parser would not read back as the string it is written
// as, gets quoted. This is a deliberately narrow allow-list rather than a
// resolver: a Kubernetes field name is ASCII, starts with a letter, and
// continues with letters, digits, dot, dash, slash or underscore, and every
// label outside that shape -- a leading digit, a leading `.`, a `<<`, a colon,
// anything non-ASCII -- is quoted instead of reasoned about. The word list is
// the YAML 1.1 boolean and null spellings, which are letters and so would
// otherwise pass: a CRD property named `y` written bare is a `true` key.
fn yaml_scalar(text: &str) -> String {
    const RESOLVES_OTHERWISE: [&str; 11] = [
        "y", "n", "yes", "no", "on", "off", "true", "false", "null", "nan", "inf",
    ];
    let starts = text
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    let body = text.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/')
    });
    let word = text.to_ascii_lowercase();
    if starts && body && !RESOLVES_OTHERWISE.contains(&word.as_str()) {
        return text.to_string();
    }
    json_string(text)
}

// A value whose schema kind is not a string, written bare only when it truly
// reads back as one scalar. `quoted` is false for every non-string kind, and an
// `enum` member of an integer or a boolean property is whatever text the schema
// carried -- the converter takes the entry as it found it -- so a label that is
// not a literal is quoted like any other string instead of being pasted into
// the document as syntax.
fn bare_value(label: &str) -> String {
    if matches!(label, "true" | "false" | "null") || is_a_json_number(label) {
        return label.to_string();
    }
    json_string(label)
}

// JSON's own number grammar, which is narrower than what a float parse accepts:
// no leading `+`, no leading zero, no bare `.`, and digits are required after
// the point and after the exponent.
fn is_a_json_number(text: &str) -> bool {
    let mut rest = text.strip_prefix('-').unwrap_or(text);
    let integer = leading_digits(rest);
    if integer.is_empty() || (integer.len() > 1 && integer.starts_with('0')) {
        return false;
    }
    rest = &rest[integer.len()..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = leading_digits(fraction);
        if digits.is_empty() {
            return false;
        }
        rest = &fraction[digits.len()..];
    }
    if let Some(exponent) = rest.strip_prefix(['e', 'E']) {
        let signed = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        let digits = leading_digits(signed);
        if digits.is_empty() {
            return false;
        }
        rest = &signed[digits.len()..];
    }
    rest.is_empty()
}

fn leading_digits(text: &str) -> &str {
    let end = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    &text[..end]
}

// A double-quoted scalar, which JSON and YAML spell the same way.
fn json_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            control if control.is_control() => {
                quoted.push_str(&format!("\\u{:04x}", control as u32));
            }
            plain => quoted.push(plain),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
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
}
