//! One object fetched as editable YAML: what the editor opens.
//!
//! The object arrives as JSON through the same fetch the describe view uses
//! -- a Secret is structurally metadata-only and the document says so -- and
//! is rendered to real YAML by a deliberately conservative emitter: every
//! scalar the YAML 1.1 type-resolution table would tag as anything but a
//! string -- its booleans down to a lone `y`, its five integer bases, its
//! floats and infinities, its timestamps, `<<` and `=` -- is double-quoted,
//! as is anything carrying an indicator, a colon, a comment, a control
//! character or a line break, whether it lands as a value or as a key.
//! Multi-line strings become literal blocks only when that round-trips
//! exactly, numbers are spelled so 1.1 reads them back as numbers, and
//! everything else falls back to quoting; the tests prove it by reading the
//! emitter's own output back with an independent 1.1 reader. The document is
//! capped in bytes and depth; an object too large to edit is a labelled
//! failure, never a silent truncation, because a truncated manifest in an
//! editor is a lie.
//!
//! Two fields are apply-machinery bookkeeping rather than the object, and both
//! come out: `managedFields`, and the `last-applied-configuration` annotation.
//! The annotation comes out for three reasons that agree -- it is a 4 KiB
//! single-line blob nobody edits, an apply that carried it would make k10s the
//! manager of a field that describes a different client's applies, and leaving
//! it in makes it the one line that differs between the live object and the
//! base document in every three-way diff, since the base by construction never
//! contains itself. It is not discarded: decoded and rendered through this same
//! emitter, it *is* the diff's base document, which is why both documents are
//! comparable line for line.

use kube::Client;

use crate::describe::{DescribeRequest, fetch_object, is_secret, stamp_identity};
use crate::discover::KindTarget;
use crate::read::{Fetched, classify};

const MAX_YAML_BYTES: usize = 2 << 20;
pub(crate) const MAX_DEPTH: usize = 64;
const INDENT: &str = "  ";
pub(crate) const SECRET_NOTE: &str = "# values withheld: k10s reads Secret metadata only";

pub(crate) const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub title: String,
    pub yaml: String,
    pub api_version: String,
    pub kind: String,
    // The `last-applied-configuration` annotation, decoded and rendered through
    // the same emitter as `yaml` so a diff can compare them line for line.
    // Absent on every object no client-side apply ever touched, which includes
    // everything server-side apply created -- a two-way diff, and the view says
    // so rather than pretending to a base it does not have.
    pub last_applied: Option<String>,
    // Threaded from discovery rather than guessed: what the server will accept
    // here, and whether an apply may carry a status block.
    pub patchable: bool,
    pub status_subresource: bool,
    // Whose object this text is, as the server states it in the very response
    // that produced the text. An apply's answer carries the same field, and a
    // server-side apply *creates* an absent object, so this is what tells an
    // update from a recreation without a second round trip. Absent when the
    // server sent none, which has to read as "cannot tell": the alternative is a
    // client that announces a recreation because a field was missing.
    pub uid: Option<String>,
}

pub(crate) async fn fetch_manifest(
    client: &Client,
    targets: &[KindTarget],
    request: &DescribeRequest,
) -> Fetched<Manifest> {
    let Some(target) = targets.iter().find(|target| target.id == request.kind) else {
        return Fetched::Failed {
            what: "manifest",
            why: "this kind is not served by the connected cluster".to_string(),
        };
    };
    let mut value =
        match fetch_object(client, target, request.namespace.as_deref(), &request.name).await {
            Ok(value) => value,
            Err(error) => return classify("manifest", &error),
        };
    let uid = uid_of(&value);
    let (yaml, last_applied) = match document(target, &mut value) {
        Ok(rendered) => rendered,
        Err(reason) => {
            return Fetched::Failed {
                what: "manifest",
                why: reason.to_string(),
            };
        }
    };
    Fetched::Ok(Manifest {
        uid,
        title: format!("{}.yaml", request.name),
        yaml,
        api_version: target.resource.api_version.clone(),
        kind: target.kind().to_string(),
        last_applied,
        patchable: target.patchable,
        status_subresource: target.status_subresource,
    })
}

// One object rendered the one way: bookkeeping out, Secret values named as
// withheld, keys in the emitter's order. The apply path renders the server's
// answer through here too, so a diff between what the editor opened and what the
// cluster would store compares two documents spelled identically.
pub(crate) fn document(
    target: &KindTarget,
    value: &mut serde_json::Value,
) -> Result<(String, Option<String>), &'static str> {
    stamp_identity(target, value);
    let declared = strip_bookkeeping(value);
    let mut yaml = String::new();
    if is_secret(target) {
        // The read path never fetches a Secret's values -- it asks for
        // `PartialObjectMetadata` -- so this note used to be true by where the
        // object came from. An apply's response does not come from there: the
        // server echoes the whole merged object, values included. Withholding
        // them here makes the note true by construction for every caller
        // instead of true by the accident of one caller's request shape.
        withhold_values(value);
        yaml.push_str(SECRET_NOTE);
        yaml.push('\n');
    }
    emit_document(&mut yaml, value)?;
    // The annotation is part of `ObjectMeta`, so it survives the metadata-only
    // fetch that keeps a Secret's values out of the read path -- and what it
    // holds is the whole object as it was *declared*, values included. The base
    // document of a diff is a document like any other and withholds them too.
    Ok((
        yaml,
        declared
            .as_deref()
            .and_then(|declared| render(declared, is_secret(target))),
    ))
}

// The object's own identity, as the server states it in the response the
// document was rendered from. It is read here rather than taken from the
// selection that opened the editor, because the two answer different questions:
// a selection names what a person clicked, possibly some time ago, and this
// names what the text on screen actually is. An empty string is no identity.
pub(crate) fn uid_of(value: &serde_json::Value) -> Option<String> {
    let uid = value.get("metadata")?.get("uid")?.as_str()?;
    (!uid.is_empty()).then(|| uid.to_string())
}

fn withhold_values(value: &mut serde_json::Value) {
    if let Some(map) = value.as_object_mut() {
        map.remove("data");
        map.remove("stringData");
    }
}

// Take the two apply-machinery fields out of the object, handing back whatever
// the annotation held. An annotation map emptied by the removal goes with it:
// `annotations: {}` is not what the object looked like before kubectl wrote
// there, and it is one more line for a diff to report.
pub(crate) fn strip_bookkeeping(value: &mut serde_json::Value) -> Option<String> {
    let metadata = value
        .get_mut("metadata")
        .and_then(serde_json::Value::as_object_mut)?;
    metadata.remove("managedFields");
    let annotations = metadata
        .get_mut("annotations")
        .and_then(serde_json::Value::as_object_mut)?;
    let declared = annotations.remove(LAST_APPLIED);
    if annotations.is_empty() {
        metadata.remove("annotations");
    }
    match declared {
        Some(serde_json::Value::String(text)) => Some(text),
        // Anything else there is not a last-applied configuration, and a diff
        // base that is not one is worse than none.
        _ => None,
    }
}

// The annotation is a JSON document. It renders through the same emitter as the
// live object -- same key order, same quoting -- because a base document spelled
// differently would diff against every line of the object it is the base of. A
// blob that does not parse, or is too large to render, is no base at all.
fn render(declared: &str, secret: bool) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(declared).ok()?;
    strip_bookkeeping(&mut value);
    if secret {
        withhold_values(&mut value);
    }
    let mut yaml = String::new();
    emit_document(&mut yaml, &value).ok()?;
    Some(yaml)
}

pub(crate) fn emit_document(
    out: &mut String,
    value: &serde_json::Value,
) -> Result<(), &'static str> {
    match value {
        serde_json::Value::Object(map) if map.is_empty() => out.push_str("{}\n"),
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            top_level_order(&mut keys);
            for key in keys {
                emit_pair(out, key, &map[key], 0)?;
            }
        }
        serde_json::Value::Array(items) if items.is_empty() => out.push_str("[]\n"),
        serde_json::Value::Array(items) => {
            for item in items {
                emit_item(out, item, 0)?;
            }
        }
        scalar_value => {
            emit_scalar(out, scalar_value, 0);
            out.push('\n');
        }
    }
    check(out, 0)
}

// Nested keys render alphabetically -- kubectl's own order, and deterministic
// whatever map implementation serde_json's feature set resolved to.
fn sorted(map: &serde_json::Map<String, serde_json::Value>) -> Vec<(&String, &serde_json::Value)> {
    let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    entries
}

// Front matter first, status last, everything else alphabetical -- the same
// order the describe view renders, so the two documents agree.
fn top_level_order(keys: &mut Vec<&String>) {
    const FRONT: [&str; 4] = ["apiVersion", "kind", "metadata", "spec"];
    keys.sort_by_key(|key| {
        let front = FRONT
            .iter()
            .position(|front_key| front_key == key)
            .map(|index| index as i32)
            .unwrap_or(i32::MAX - 1);
        let back = if *key == "status" { 1 } else { 0 };
        (back, front, (*key).clone())
    });
}

fn emit_pair(
    out: &mut String,
    key: &str,
    value: &serde_json::Value,
    depth: usize,
) -> Result<(), &'static str> {
    check(out, depth)?;
    push_indent(out, depth);
    out.push_str(&scalar(key));
    emit_pair_value(out, value, depth)
}

fn emit_pair_value(
    out: &mut String,
    value: &serde_json::Value,
    depth: usize,
) -> Result<(), &'static str> {
    match value {
        serde_json::Value::Object(map) if map.is_empty() => out.push_str(": {}\n"),
        serde_json::Value::Object(map) => {
            out.push_str(":\n");
            for (child_key, child) in sorted(map) {
                emit_pair(out, child_key, child, depth + 1)?;
            }
        }
        serde_json::Value::Array(items) if items.is_empty() => out.push_str(": []\n"),
        serde_json::Value::Array(items) => {
            out.push_str(":\n");
            for item in items {
                emit_item(out, item, depth + 1)?;
            }
        }
        scalar_value => {
            out.push_str(": ");
            emit_scalar(out, scalar_value, depth);
            out.push('\n');
        }
    }
    Ok(())
}

fn emit_item(
    out: &mut String,
    value: &serde_json::Value,
    depth: usize,
) -> Result<(), &'static str> {
    check(out, depth)?;
    push_indent(out, depth);
    match value {
        serde_json::Value::Object(map) if map.is_empty() => out.push_str("- {}\n"),
        serde_json::Value::Object(map) => {
            let mut first = true;
            for (child_key, child) in sorted(map) {
                if first {
                    out.push_str("- ");
                    out.push_str(&scalar(child_key));
                    emit_pair_value(out, child, depth + 1)?;
                    first = false;
                } else {
                    emit_pair(out, child_key, child, depth + 1)?;
                }
            }
        }
        serde_json::Value::Array(items) if items.is_empty() => out.push_str("- []\n"),
        serde_json::Value::Array(items) => {
            out.push_str("-\n");
            for item in items {
                emit_item(out, item, depth + 1)?;
            }
        }
        scalar_value => {
            out.push_str("- ");
            emit_scalar(out, scalar_value, depth);
            out.push('\n');
        }
    }
    Ok(())
}

fn emit_scalar(out: &mut String, value: &serde_json::Value, depth: usize) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(true) => out.push_str("true"),
        serde_json::Value::Bool(false) => out.push_str("false"),
        serde_json::Value::Number(number) => out.push_str(&number_text(number)),
        serde_json::Value::String(text) => emit_string(out, text, depth),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            unreachable!("containers are handled by the pair and item emitters")
        }
    }
}

// serde_json prints extreme f64 magnitudes as `1e100`, and YAML 1.1's float row
// wants both a fraction dot and a signed exponent -- lacking either, a
// conforming parser hands back the string "1e100". Respell rather than quote: a
// quoted number would come back as a string too.
fn number_text(number: &serde_json::Number) -> String {
    let text = number.to_string();
    let Some((mantissa, exponent)) = text.split_once(['e', 'E']) else {
        return text;
    };
    let signed = exponent.starts_with(['+', '-']);
    let dot = if mantissa.contains('.') { "" } else { ".0" };
    let sign = if signed { "" } else { "+" };
    format!("{mantissa}{dot}e{sign}{exponent}")
}

fn emit_string(out: &mut String, text: &str, depth: usize) {
    if text.contains('\n') && block_safe(text) {
        if text.ends_with('\n') {
            out.push('|');
        } else {
            out.push_str("|-");
        }
        for line in text.trim_end_matches('\n').split('\n') {
            out.push('\n');
            if !line.is_empty() {
                push_indent(out, depth + 1);
                out.push_str(line);
            }
        }
        return;
    }
    out.push_str(&scalar(text));
}

fn block_safe(text: &str) -> bool {
    if text.starts_with([' ', '\n']) || text.ends_with("\n\n") {
        return false;
    }
    text.chars()
        .all(|character| character == '\n' || plain_char(character))
        && !text
            .split('\n')
            .any(|line| line.ends_with(' ') || line.ends_with('\t'))
}

// A character YAML 1.1 folds into a line break, or strips as a byte order mark,
// cannot ride a bare or literal scalar: it would come back as `\n` or vanish.
// C0 and C1 controls answer `is_control`; these three do not.
fn plain_char(character: char) -> bool {
    !character.is_control() && !matches!(character, '\u{2028}' | '\u{2029}' | '\u{feff}')
}

pub(crate) fn scalar(text: &str) -> String {
    if needs_quoting(text) {
        quoted(text)
    } else {
        text.to_string()
    }
}

// Every scalar YAML 1.1 resolves to a word rather than a pattern: the whole
// bool row down to a lone `y`, the null row, the float row's infinities and
// not-a-numbers, the merge key and the value key. The int, float and timestamp
// patterns live in `resolves_as_number` and `resolves_as_timestamp`.
const RESOLVED_WORDS: [&str; 40] = [
    "y", "Y", "yes", "Yes", "YES", "n", "N", "no", "No", "NO", "true", "True", "TRUE", "false",
    "False", "FALSE", "on", "On", "ON", "off", "Off", "OFF", "~", "null", "Null", "NULL", ".inf",
    ".Inf", ".INF", "+.inf", "+.Inf", "+.INF", "-.inf", "-.Inf", "-.INF", ".nan", ".NaN", ".NAN",
    "<<", "=",
];

// The YAML 1.1 indicator characters. `-`, `?` and `:` indicate only when a space
// follows, but a leading one is quoted regardless: nothing k8s puts in a scalar
// opens with them, and guessing is how emitters get this wrong.
const INDICATORS: [char; 19] = [
    '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`',
];

pub(crate) fn needs_quoting(text: &str) -> bool {
    if text.is_empty() || text.trim() != text {
        return true;
    }
    if RESOLVED_WORDS.contains(&text) || resolves_as_number(text) || resolves_as_timestamp(text) {
        return true;
    }
    let first = text.chars().next().expect("emptiness was checked");
    if INDICATORS.contains(&first) {
        return true;
    }
    if text.starts_with("---") || text.starts_with("...") {
        return true;
    }
    text.contains(": ")
        || text.ends_with(':')
        || text.contains(" #")
        || text.chars().any(|character| !plain_char(character))
}

// The int and float rows of the YAML 1.1 type table: integers in base 2, 8, 10,
// 16 and 60, and decimal and base-60 floats. Underscores are digit separators
// there, so a run of digits and separators resolves as a number even when Rust
// will not parse it; Rust's own float parser then catches the exponent shapes
// 1.1 spells out plus a few it does not.
fn resolves_as_number(text: &str) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    if let Some(digits) = body.strip_prefix("0b") {
        return every(digits, |character| matches!(character, '0' | '1' | '_'));
    }
    if let Some(digits) = body.strip_prefix("0x") {
        return every(digits, |character| {
            character.is_ascii_hexdigit() || character == '_'
        });
    }
    // `0o` is YAML 1.2's octal spelling rather than 1.1's bare leading zero;
    // covering both spares the editor guessing which parser opens the file.
    if let Some(digits) = body.strip_prefix("0o") {
        return every(digits, |character| {
            ('0'..='7').contains(&character) || character == '_'
        });
    }
    if !body.starts_with(|character: char| character.is_ascii_digit() || character == '.') {
        return false;
    }
    every(body, |character| {
        character.is_ascii_digit() || matches!(character, '_' | ':' | '.' | '-' | '+' | 'e' | 'E')
    }) || text.parse::<f64>().is_ok()
}

// Every 1.1 timestamp form -- `2026-08-02`, `2026-8-2T10:00:00.5-05:00`,
// `2026-08-02 10:00:00 Z` -- opens with four digits and a dash. Treating that
// prefix as the whole test over-quotes a scalar like `2026-rework` and
// mis-resolves none, and `creationTimestamp` is on every object k8s serves.
fn resolves_as_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() > 4 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-'
}

fn every(text: &str, allowed: impl Fn(char) -> bool) -> bool {
    !text.is_empty() && text.chars().all(allowed)
}

pub(crate) fn quoted(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            escaped if !plain_char(escaped) => {
                quoted.push_str(&format!("\\u{:04x}", escaped as u32));
            }
            plain => quoted.push(plain),
        }
    }
    quoted.push('"');
    quoted
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

fn check(out: &str, depth: usize) -> Result<(), &'static str> {
    if depth > MAX_DEPTH {
        return Err("this object nests deeper than the editor renders");
    }
    if out.len() > MAX_YAML_BYTES {
        return Err("this object is larger than the 2 MiB the editor opens");
    }
    Ok(())
}
