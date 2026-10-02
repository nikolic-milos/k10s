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
#[path = "insert_test.rs"]
mod tests;
