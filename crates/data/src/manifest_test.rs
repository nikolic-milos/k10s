// "Cannot tell" is a third answer, and it has to stay one: a reviewer that
// reads a missing uid as a mismatch announces that an object was deleted and
// recreated because a field was absent from a response.
#[test]
fn an_object_with_no_usable_uid_has_none_rather_than_a_blank_one() {
    let uid = |json: &str| uid_of(&serde_json::from_str(json).unwrap());
    assert_eq!(uid(r#"{"metadata":{"uid":"abc"}}"#).as_deref(), Some("abc"));
    assert_eq!(uid(r#"{"metadata":{"uid":""}}"#), None);
    assert_eq!(uid(r#"{"metadata":{"uid":7}}"#), None);
    assert_eq!(uid(r#"{"metadata":{}}"#), None);
    assert_eq!(uid(r#"{}"#), None);
}

use crate::manifest::*;

fn emitted(value: &serde_json::Value) -> String {
    let mut out = String::new();
    emit_document(&mut out, value).expect("test fixtures fit the caps");
    out
}

fn yaml_of(json: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(json).expect("test fixtures parse");
    emitted(&value)
}

// An independent reader for exactly the subset the emitter writes: block
// mappings, block sequences, `{}` and `[]`, double-quoted scalars, literal
// blocks and bare scalars. It resolves scalars from the YAML 1.1
// tag-resolution regexes at yaml.org/type and never consults
// `needs_quoting` -- a reader that shared the emitter's blind spots would
// prove nothing.
struct Reader<'a> {
    lines: Vec<&'a str>,
    at: usize,
}

struct Pair<'a> {
    key: String,
    bare: bool,
    rest: &'a str,
}

fn read(document: &str) -> Result<serde_json::Value, String> {
    if document.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    let body = document.strip_suffix('\n').unwrap_or(document);
    let mut reader = Reader {
        lines: body.split('\n').collect(),
        at: 0,
    };
    let value = reader.node(0)?;
    match reader.lines.get(reader.at) {
        Some(line) => Err(format!("{line:?} was left unread")),
        None => Ok(value),
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn split_pair(content: &str) -> Result<Option<Pair<'_>>, String> {
    if content.starts_with('"') {
        let (key, rest) = read_quoted(content)?;
        return Ok(rest.strip_prefix(':').map(|rest| Pair {
            key,
            bare: false,
            rest,
        }));
    }
    if let Some(at) = content.find(": ") {
        return Ok(Some(Pair {
            key: content[..at].to_string(),
            bare: true,
            rest: &content[at + 1..],
        }));
    }
    Ok(content.strip_suffix(':').map(|key| Pair {
        key: key.to_string(),
        bare: true,
        rest: "",
    }))
}

fn pair_key(pair: &Pair<'_>) -> Result<String, String> {
    if !pair.bare {
        return Ok(pair.key.clone());
    }
    match resolve(&pair.key)? {
        serde_json::Value::String(text) => Ok(text),
        other => Err(format!(
            "the bare key {:?} resolves to {other}, not to a string",
            pair.key
        )),
    }
}

// Only the escapes `quoted()` writes are accepted: an emitter that grew a
// new one fails the round-trip instead of being guessed at. The emitter
// never wraps a quoted scalar, so there is no line folding to undo.
fn read_quoted(text: &str) -> Result<(String, &str), String> {
    let body = text
        .strip_prefix('"')
        .ok_or_else(|| format!("{text:?} does not open a quoted scalar"))?;
    let mut out = String::new();
    let mut characters = body.char_indices();
    while let Some((at, character)) = characters.next() {
        match character {
            '"' => return Ok((out, &body[at + 1..])),
            '\\' => {
                let (_, escape) = characters
                    .next()
                    .ok_or_else(|| format!("{text:?} ends inside an escape"))?;
                match escape {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'u' => {
                        let mut code = String::new();
                        for _ in 0..4 {
                            let (_, digit) = characters
                                .next()
                                .ok_or_else(|| format!("{text:?} ends inside a \\u escape"))?;
                            code.push(digit);
                        }
                        let point = u32::from_str_radix(&code, 16)
                            .map_err(|_| format!("\\u{code} is not hexadecimal"))?;
                        out.push(
                            char::from_u32(point)
                                .ok_or_else(|| format!("\\u{code} is not a character"))?,
                        );
                    }
                    other => return Err(format!("the reader rejects the escape \\{other}")),
                }
            }
            plain => out.push(plain),
        }
    }
    Err(format!("{text:?} never closes its quote"))
}

impl<'a> Reader<'a> {
    fn node(&mut self, indent: usize) -> Result<serde_json::Value, String> {
        let line = self
            .lines
            .get(self.at)
            .copied()
            .ok_or_else(|| "the document ends where a node was promised".to_string())?;
        if indent_of(line) != indent {
            return Err(format!("{line:?} is not indented to column {indent}"));
        }
        let content = &line[indent..];
        if content == "-" || content.starts_with("- ") {
            return self.sequence(indent);
        }
        if split_pair(content)?.is_some() {
            return self.mapping(indent, None);
        }
        self.at += 1;
        self.read_scalar(content, indent)
    }

    fn mapping(
        &mut self,
        indent: usize,
        seed: Option<(String, serde_json::Value)>,
    ) -> Result<serde_json::Value, String> {
        let mut map = serde_json::Map::new();
        if let Some((key, value)) = seed {
            map.insert(key, value);
        }
        while let Some(line) = self.lines.get(self.at).copied() {
            let column = indent_of(line);
            if column < indent {
                break;
            }
            if column > indent {
                return Err(format!("{line:?} over-indents inside a mapping"));
            }
            let Some(pair) = split_pair(&line[indent..])? else {
                break;
            };
            let key = pair_key(&pair)?;
            self.at += 1;
            let value = self.value(pair.rest, indent)?;
            if map.insert(key, value).is_some() {
                return Err(format!("{:?} is a duplicate key", pair.key));
            }
        }
        Ok(serde_json::Value::Object(map))
    }

    fn sequence(&mut self, indent: usize) -> Result<serde_json::Value, String> {
        let mut items = Vec::new();
        while let Some(line) = self.lines.get(self.at).copied() {
            if indent_of(line) != indent {
                break;
            }
            let content = &line[indent..];
            if content == "-" {
                self.at += 1;
                items.push(self.nested(indent)?);
                continue;
            }
            let Some(entry) = content.strip_prefix("- ") else {
                break;
            };
            self.at += 1;
            match split_pair(entry)? {
                Some(pair) => {
                    let key = pair_key(&pair)?;
                    let first = self.value(pair.rest, indent + 2)?;
                    items.push(self.mapping(indent + 2, Some((key, first)))?);
                }
                None => items.push(self.read_scalar(entry, indent)?),
            }
        }
        Ok(serde_json::Value::Array(items))
    }

    // What hangs under a `key:` or a bare `-`: whatever sits on the next
    // line, which must out-indent the indicator's own column.
    fn nested(&mut self, column: usize) -> Result<serde_json::Value, String> {
        let next = self
            .lines
            .get(self.at)
            .copied()
            .ok_or_else(|| format!("nothing follows the indicator at column {column}"))?;
        let deeper = indent_of(next);
        if deeper <= column {
            return Err(format!("nothing is nested under column {column}"));
        }
        self.node(deeper)
    }

    fn value(&mut self, rest: &str, column: usize) -> Result<serde_json::Value, String> {
        if rest.is_empty() {
            return self.nested(column);
        }
        let content = rest
            .strip_prefix(' ')
            .ok_or_else(|| format!("{rest:?} does not follow its colon with a space"))?;
        self.read_scalar(content, column)
    }

    fn read_scalar(&mut self, content: &str, floor: usize) -> Result<serde_json::Value, String> {
        match content {
            "{}" => return Ok(serde_json::Value::Object(serde_json::Map::new())),
            "[]" => return Ok(serde_json::Value::Array(Vec::new())),
            "|" => return self.read_block(true, floor).map(serde_json::Value::String),
            "|-" => return self.read_block(false, floor).map(serde_json::Value::String),
            _ => {}
        }
        if content.starts_with('"') {
            let (text, rest) = read_quoted(content)?;
            if !rest.is_empty() {
                return Err(format!("{rest:?} trails a quoted scalar"));
            }
            return Ok(serde_json::Value::String(text));
        }
        resolve(content)
    }

    // Literal block scalars: the first content line fixes the indentation,
    // `|` clips one trailing break and `|-` strips them all.
    fn read_block(&mut self, clip: bool, floor: usize) -> Result<String, String> {
        let mut block_indent: Option<usize> = None;
        let mut lines: Vec<&str> = Vec::new();
        while let Some(line) = self.lines.get(self.at).copied() {
            if line.is_empty() {
                lines.push("");
                self.at += 1;
                continue;
            }
            let column = indent_of(line);
            match block_indent {
                None if column <= floor => break,
                None => block_indent = Some(column),
                Some(known) if column < known => break,
                Some(_) => {}
            }
            let known = block_indent.unwrap_or(column);
            lines.push(&line[known..]);
            self.at += 1;
        }
        if block_indent.is_none() {
            return Err(format!(
                "a literal block below column {floor} has no content"
            ));
        }
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        let mut text = lines.join("\n");
        if clip {
            text.push('\n');
        }
        Ok(text)
    }
}

// YAML 1.1 tag resolution, transcribed from the regexes at yaml.org/type:
// bool, int, float, null, merge, value, timestamp, and !!str for everything
// else. The tags JSON cannot hold come back as errors so a round-trip
// failure names the tag instead of quietly coercing it.
fn resolve(text: &str) -> Result<serde_json::Value, String> {
    if text.is_empty() || matches!(text, "~" | "null" | "Null" | "NULL") {
        return Ok(serde_json::Value::Null);
    }
    if matches!(
        text,
        "y" | "Y" | "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON"
    ) {
        return Ok(serde_json::Value::Bool(true));
    }
    if matches!(
        text,
        "n" | "N" | "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF"
    ) {
        return Ok(serde_json::Value::Bool(false));
    }
    if text == "<<" {
        return Err("`<<` resolves to !!merge, which folds the mapping it keys".to_string());
    }
    if text == "=" {
        return Err("`=` resolves to !!value, not to a string".to_string());
    }
    if matches!(
        text,
        ".inf"
            | ".Inf"
            | ".INF"
            | "+.inf"
            | "+.Inf"
            | "+.INF"
            | "-.inf"
            | "-.Inf"
            | "-.INF"
            | ".nan"
            | ".NaN"
            | ".NAN"
    ) {
        return Err(format!("{text:?} resolves to a !!float JSON cannot hold"));
    }
    if let Some(int) = yaml_int(text) {
        return Ok(int);
    }
    if let Some(float) = yaml_float(text) {
        return serde_json::Number::from_f64(float)
            .map(serde_json::Value::Number)
            .ok_or_else(|| format!("{text:?} resolves to a !!float JSON cannot hold"));
    }
    if yaml_timestamp(text) {
        return Err(format!("{text:?} resolves to !!timestamp, not to a string"));
    }
    Ok(serde_json::Value::String(text.to_string()))
}

fn all_are(text: &str, allowed: impl Fn(char) -> bool) -> bool {
    !text.is_empty() && text.chars().all(allowed)
}

fn without_underscores(text: &str) -> String {
    text.chars().filter(|character| *character != '_').collect()
}

fn split_sign(text: &str) -> (i64, &str) {
    match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    }
}

fn radix_value(digits: &str, base: u32) -> i64 {
    i64::from_str_radix(&without_underscores(digits), base).unwrap_or(i64::MAX)
}

// [-+]?0b[0-1_]+ | [-+]?0[0-7_]+ | [-+]?(0|[1-9][0-9_]*)
// | [-+]?0x[0-9a-fA-F_]+ | [-+]?[1-9][0-9_]*(:[0-5]?[0-9])+
fn yaml_int(text: &str) -> Option<serde_json::Value> {
    let (sign, body) = split_sign(text);
    if let Some(digits) = body.strip_prefix("0b") {
        let shaped = all_are(digits, |character| matches!(character, '0' | '1' | '_'));
        return shaped.then(|| (sign * radix_value(digits, 2)).into());
    }
    if let Some(digits) = body.strip_prefix("0x") {
        let shaped = all_are(digits, |character| {
            character.is_ascii_hexdigit() || character == '_'
        });
        return shaped.then(|| (sign * radix_value(digits, 16)).into());
    }
    if body.len() > 1
        && body.starts_with('0')
        && all_are(&body[1..], |character| {
            ('0'..='7').contains(&character) || character == '_'
        })
    {
        return Some((sign * radix_value(&body[1..], 8)).into());
    }
    if body.contains(':') && !body.contains('.') {
        return base_sixty(body, false).map(|value| (sign * value as i64).into());
    }
    let decimal = match body.chars().next()? {
        '0' => body.len() == 1,
        '1'..='9' => all_are(body, |character| {
            character.is_ascii_digit() || character == '_'
        }),
        _ => false,
    };
    if !decimal {
        return None;
    }
    if body.contains('_') {
        return Some((sign * radix_value(body, 10)).into());
    }
    // Plain base 10 is also valid JSON, so serde_json rebuilds the exact
    // Number variant the fixture parsed into.
    let json = if sign < 0 {
        format!("-{body}")
    } else {
        body.to_string()
    };
    serde_json::from_str::<serde_json::Value>(&json).ok()
}

// [-+]?([0-9][0-9_]*)?\.[0-9_]*([eE][-+][0-9]+)?
// | [-+]?[0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]*
fn yaml_float(text: &str) -> Option<f64> {
    let (sign, body) = split_sign(text);
    if body.contains(':') {
        return base_sixty(body, true).map(|value| value * sign as f64);
    }
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (body, None),
    };
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-'])?;
        if !all_are(digits, |character| character.is_ascii_digit()) {
            return None;
        }
    }
    let (whole, fraction) = mantissa.split_once('.')?;
    let whole_shaped = whole.is_empty()
        || (whole.starts_with(|character: char| character.is_ascii_digit())
            && all_are(whole, |character| {
                character.is_ascii_digit() || character == '_'
            }));
    let fraction_shaped = fraction.is_empty()
        || all_are(fraction, |character| {
            character.is_ascii_digit() || character == '_'
        });
    if !whole_shaped || !fraction_shaped {
        return None;
    }
    without_underscores(text).parse::<f64>().ok()
}

// The base-60 rows of both the int and float tables, which differ only in
// the leading digit and the trailing fraction.
fn base_sixty(body: &str, fractional: bool) -> Option<f64> {
    let (groups, fraction) = match body.split_once('.') {
        Some((groups, fraction)) => (groups, Some(fraction)),
        None => (body, None),
    };
    if fraction.is_some() != fractional {
        return None;
    }
    let mut groups = groups.split(':');
    let head = groups.next()?;
    let head_shaped = match head.chars().next()? {
        '0' => fractional,
        '1'..='9' => true,
        _ => false,
    } && all_are(head, |character| {
        character.is_ascii_digit() || character == '_'
    });
    if !head_shaped {
        return None;
    }
    let mut value = without_underscores(head).parse::<f64>().ok()?;
    let mut seen = 0usize;
    for group in groups {
        let shaped = match group.as_bytes() {
            [ones] => ones.is_ascii_digit(),
            [tens, ones] => (b'0'..=b'5').contains(tens) && ones.is_ascii_digit(),
            _ => false,
        };
        if !shaped {
            return None;
        }
        value = value * 60.0 + group.parse::<f64>().ok()?;
        seen += 1;
    }
    if seen == 0 {
        return None;
    }
    match fraction {
        Some(fraction) => {
            let digits = without_underscores(fraction);
            Some(value + format!("0.{digits}").parse::<f64>().ok()?)
        }
        None => Some(value),
    }
}

struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn byte(&mut self, wanted: u8) -> bool {
        let hit = self.bytes.get(self.at) == Some(&wanted);
        if hit {
            self.at += 1;
        }
        hit
    }

    fn one_of(&mut self, wanted: &[u8]) -> bool {
        let hit = self
            .bytes
            .get(self.at)
            .is_some_and(|byte| wanted.contains(byte));
        if hit {
            self.at += 1;
        }
        hit
    }

    fn digits(&mut self, least: usize, most: usize) -> bool {
        let start = self.at;
        while self.at - start < most && self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        self.at - start >= least
    }

    fn blanks(&mut self, least: usize) -> bool {
        let start = self.at;
        while self
            .bytes
            .get(self.at)
            .is_some_and(|&byte| matches!(byte, b' ' | b'\t'))
        {
            self.at += 1;
        }
        self.at - start >= least
    }

    fn fraction(&mut self) -> bool {
        if self.byte(b'.') {
            self.digits(0, usize::MAX);
        }
        true
    }

    fn zone(&mut self) -> bool {
        let mark = self.at;
        self.blanks(0);
        if self.byte(b'Z') {
            return true;
        }
        if self.one_of(b"+-") && self.digits(1, 2) {
            if self.byte(b':') && !self.digits(2, 2) {
                return false;
            }
            return true;
        }
        self.at = mark;
        true
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

// [0-9]{4}-[0-9]{2}-[0-9]{2}, or [0-9]{4}-[0-9]{1,2}-[0-9]{1,2} followed by
// ([Tt]|[ \t]+) h:mm:ss (\.[0-9]*)? ([ \t]*(Z|[-+]h{1,2}(:mm)?))?
fn yaml_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut date = Scan { bytes, at: 0 };
    if date.digits(4, 4)
        && date.byte(b'-')
        && date.digits(2, 2)
        && date.byte(b'-')
        && date.digits(2, 2)
        && date.done()
    {
        return true;
    }
    let mut scan = Scan { bytes, at: 0 };
    scan.digits(4, 4)
        && scan.byte(b'-')
        && scan.digits(1, 2)
        && scan.byte(b'-')
        && scan.digits(1, 2)
        && (scan.one_of(b"Tt") || scan.blanks(1))
        && scan.digits(1, 2)
        && scan.byte(b':')
        && scan.digits(2, 2)
        && scan.byte(b':')
        && scan.digits(2, 2)
        && scan.fraction()
        && scan.zone()
        && scan.done()
}

fn secret_target() -> crate::discover::KindTarget {
    let mut catalog = k10s_core::Catalog::new();
    crate::discover::intern(
        &mut catalog,
        kube::discovery::ApiResource {
            group: String::new(),
            version: "v1".to_string(),
            api_version: "v1".to_string(),
            kind: "Secret".to_string(),
            plural: "secrets".to_string(),
        },
        &kube::discovery::ApiCapabilities {
            scope: kube::discovery::Scope::Namespaced,
            subresources: Vec::new(),
            operations: vec!["get".into(), "patch".into()],
        },
    )
}

// The one invariant in this crate that is a rule rather than a preference:
// a Secret's values never enter a document. A real server serves secrets
// with a patch verb, so an apply's response arrives carrying them, and the
// note at the top of the document would otherwise be a lie printed above
// the very thing it denies.
#[test]
fn a_secret_document_withholds_values_whatever_the_object_arrived_carrying() {
    let target = secret_target();
    let mut value = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": "api-token", "namespace": "prod"},
        "type": "Opaque",
        "data": {"token": "c3VwZXItc2VjcmV0"},
        "stringData": {"plain": "super-secret"}
    });
    let (yaml, _) = document(&target, &mut value).expect("the document renders");
    assert!(yaml.starts_with(SECRET_NOTE), "the note leads: {yaml}");
    assert!(
        !yaml.contains("c3VwZXItc2VjcmV0"),
        "no encoded value: {yaml}"
    );
    assert!(!yaml.contains("super-secret"), "and no plain one: {yaml}");
    assert!(
        !yaml.contains("\ndata:") && !yaml.contains("\nstringData:"),
        "and no block where one would be: {yaml}"
    );
    assert!(
        yaml.contains("name: api-token") && yaml.contains("type: Opaque"),
        "everything that is not a value stays: {yaml}"
    );
}

// The subtler half of the same rule. `metadata.annotations` survives a
// `PartialObjectMetadata` fetch, so a Secret written by `kubectl apply`
// carries its own declared values inside the annotation -- and that
// annotation becomes a diff's base document.
#[test]
fn a_secret_base_document_withholds_the_values_its_annotation_carries() {
    let target = secret_target();
    let declared = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": "api-token", "namespace": "prod"},
        "data": {"token": "c3VwZXItc2VjcmV0"}
    })
    .to_string();
    let mut value = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": "api-token",
            "namespace": "prod",
            "annotations": {LAST_APPLIED: declared, "team": "platform"}
        }
    });
    let (yaml, base) = document(&target, &mut value).expect("the document renders");
    assert!(
        !yaml.contains("c3VwZXItc2VjcmV0"),
        "not in the object: {yaml}"
    );
    let base = base.expect("the annotation was there, so there is a base");
    assert!(
        !base.contains("c3VwZXItc2VjcmV0"),
        "and not in the base either: {base}"
    );
    assert!(
        base.contains("name: api-token"),
        "the base is still a usable document: {base}"
    );
}

#[test]
fn front_matter_leads_and_status_trails() {
    let yaml = yaml_of(
        r#"{"status":{"phase":"Running"},"metadata":{"name":"web"},"kind":"Pod","zebra":1,"apiVersion":"v1"}"#,
    );
    let lines: Vec<&str> = yaml.lines().collect();
    assert_eq!(lines[0], "apiVersion: v1");
    assert_eq!(lines[1], "kind: Pod");
    assert_eq!(lines[2], "metadata:");
    assert!(yaml.trim_end().ends_with("phase: Running"));
    let zebra = yaml.find("zebra").expect("zebra rendered");
    assert!(zebra < yaml.find("status:").expect("status rendered"));
}

// Every row is a scalar a YAML 1.1 parser could take for something other
// than the string it is, or a shape that has to survive as a bare or
// literal one. Each is round-tripped as a value and as a mapping key.
const ADVERSARIAL: &[&str] = &[
    "y",
    "Y",
    "n",
    "N",
    "yes",
    "Yes",
    "YES",
    "no",
    "No",
    "NO",
    "on",
    "On",
    "ON",
    "off",
    "Off",
    "OFF",
    "true",
    "True",
    "TRUE",
    "false",
    "False",
    "FALSE",
    "~",
    "null",
    "Null",
    "NULL",
    ".inf",
    ".Inf",
    ".INF",
    "+.inf",
    "-.inf",
    ".nan",
    ".NaN",
    ".NAN",
    "<<",
    "=",
    "0",
    "1",
    "-1",
    "+1",
    "0b1010",
    "0b1_0",
    "017",
    "0_17",
    "0o17",
    "0x1A",
    "0x1a_b",
    "1_000",
    "1:30",
    "1:30:45",
    "08",
    "0.5",
    ".5",
    "5.",
    "1e5",
    "1.0e+100",
    "1:30.5",
    "1_0.5e+3",
    "0.0.0.0",
    "2026-08-02",
    "2026-8-2",
    "2026-08-02T10:00:00Z",
    "2026-08-02t10:00:00.123456Z",
    "2026-08-02 10:00:00 -05:00",
    "2026-08-02T10:00:00.5+01:30",
    "",
    " ",
    "  ",
    "\t",
    " x",
    "x ",
    "x\ty",
    "\u{a0}",
    "-",
    "- x",
    "-x",
    "?",
    "? x",
    ":",
    ":x",
    "x:",
    "x: y",
    "x:y",
    ",",
    ",x",
    "[",
    "]",
    "{",
    "}",
    "{}",
    "[]",
    "{a: b}",
    "[1, 2]",
    "#",
    "# x",
    "x #y",
    "x#y",
    "&a",
    "*a",
    "!tag",
    "|",
    "|x",
    ">",
    ">x",
    "%x",
    "@x",
    "`x",
    "'x'",
    "\"x\"",
    "x'y",
    "x\"y",
    "\\",
    "a\\b",
    "---",
    "...",
    "--- x",
    "...x",
    "a\nb",
    "a\nb\n",
    "a\n\nb",
    "a\n\nb\n",
    "a\n\n",
    "\na",
    "a \nb",
    "a\n b",
    "a\n b\n",
    "a\nb\n\n",
    "line\n  indented\n",
    "a\tb\nc",
    "a\u{0}b",
    "a\u{7}b",
    "a\rb",
    "a\r\nb",
    "\u{7f}",
    "a\u{85}b",
    "a\u{2028}b",
    "a\u{2029}b",
    "a\u{feff}b",
    "plain",
    "nginx:1.27",
    "8Gi",
    "v1.27",
    "日本語",
    "héllo",
    "🎉 party",
    "Deployment/web",
    "kubectl.kubernetes.io/last-applied-configuration",
    "a,b",
    "a b",
];

fn round_trip(value: &serde_json::Value, what: &str) {
    let document = emitted(value);
    match read(&document) {
        Ok(read_back) => {
            assert_eq!(
                &read_back, value,
                "{what} came back changed from\n{document}"
            );
        }
        Err(reason) => panic!("{what} did not read back: {reason}\n{document}"),
    }
}

#[test]
fn ambiguous_scalars_are_quoted_so_yaml_reads_them_as_strings() {
    for text in ADVERSARIAL {
        round_trip(
            &serde_json::json!({ "field": text }),
            &format!("the value {text:?}"),
        );
        let mut map = serde_json::Map::new();
        map.insert(
            (*text).to_string(),
            serde_json::Value::String("value".to_string()),
        );
        round_trip(
            &serde_json::Value::Object(map),
            &format!("the key {text:?}"),
        );
    }
}

#[test]
fn scalars_no_yaml_type_claims_stay_bare() {
    let yaml = yaml_of(
        r#"{"a":"plain","b":"nginx:1.27","c":"8Gi","d":"x#y","e":"v1.27","f":"日本語","g":"Deployment/web"}"#,
    );
    assert_eq!(
        yaml,
        "a: plain\nb: nginx:1.27\nc: 8Gi\nd: x#y\ne: v1.27\nf: 日本語\ng: Deployment/web\n"
    );
}

#[test]
fn every_json_number_shape_round_trips() {
    for number in [
        serde_json::json!(0),
        serde_json::json!(1),
        serde_json::json!(-1),
        serde_json::json!(u64::MAX),
        serde_json::json!(i64::MIN),
        serde_json::json!(0.5),
        serde_json::json!(-0.5),
        serde_json::json!(3.0),
        serde_json::json!(1e100),
        serde_json::json!(1e-7),
        serde_json::json!(-1.5e-8),
        serde_json::json!(f64::MAX),
        serde_json::json!(f64::MIN_POSITIVE),
        serde_json::json!(5e-324),
    ] {
        let what = format!("the number {number}");
        round_trip(&serde_json::json!({ "number": number }), &what);
    }
}

#[test]
fn a_whole_object_round_trips_through_the_reader() {
    let value = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "web",
            "creationTimestamp": "2026-08-02T10:00:00Z",
            "labels": {"app": "web", "y": "n", "8080": "port"},
            "annotations": {}
        },
        "spec": {
            "containers": [
                {
                    "name": "web",
                    "image": "nginx:1.27",
                    "args": ["--flag", "-v", "", "on"],
                    "ports": [{"containerPort": 80, "protocol": "TCP"}],
                    "resources": {"limits": {"memory": "8Gi", "cpu": "1.5"}}
                },
                {"name": "sidecar", "command": ["/bin/sh", "-c", "echo hi\nsleep 1\n"]}
            ],
            "nodeSelector": {},
            "matrix": [[1, 2], [3], []]
        },
        "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}
    });
    round_trip(&value, "a whole object");
}

#[test]
fn a_document_that_is_not_a_mapping_still_round_trips() {
    for value in [
        serde_json::json!([1, "two", {"three": true}, ["deep"], [], {}]),
        serde_json::json!("a\nb\n"),
        serde_json::json!("y"),
        serde_json::json!(null),
        serde_json::json!(true),
        serde_json::json!(7),
        serde_json::json!({}),
        serde_json::json!([]),
    ] {
        let what = format!("the document {value}");
        round_trip(&value, &what);
    }
}

#[test]
fn the_round_trip_reader_resolves_bare_scalars_the_emitter_has_to_quote() {
    assert_eq!(
        read("a: y\n").expect("`y` reads back"),
        serde_json::json!({"a": true}),
        "a bare `y` is the YAML 1.1 boolean the old emitter let through"
    );
    assert_eq!(
        read("a: n\n").expect("`n` reads back"),
        serde_json::json!({"a": false})
    );
    assert_eq!(
        read("a: 017\n").expect("octal reads back"),
        serde_json::json!({"a": 15})
    );
    assert_eq!(
        read("a: 1_000\n").expect("separated digits read back"),
        serde_json::json!({"a": 1000})
    );
    assert_eq!(
        read("a: 1:30\n").expect("base 60 reads back"),
        serde_json::json!({"a": 90})
    );
    assert_eq!(
        read("a: 0x1A\n").expect("hexadecimal reads back"),
        serde_json::json!({"a": 26})
    );
    for bare in [
        "<<",
        "=",
        ".inf",
        "-.inf",
        ".NaN",
        "2026-08-02",
        "2026-08-02T10:00:00Z",
        "2026-08-02 10:00:00 -05:00",
    ] {
        let document = format!("a: {bare}\n");
        assert!(
            read(&document).is_err(),
            "bare {bare:?} must not read back as a string"
        );
    }
    assert!(
        read("y: a\n").is_err(),
        "a bare boolean key must not read back as a string"
    );
}

#[test]
fn real_numbers_and_booleans_stay_bare() {
    let yaml = yaml_of(r#"{"replicas":3,"paused":false,"ratio":0.5,"nothing":null}"#);
    assert!(yaml.contains("replicas: 3\n"));
    assert!(yaml.contains("paused: false\n"));
    assert!(yaml.contains("ratio: 0.5\n"));
    assert!(yaml.contains("nothing: null\n"));
}

#[test]
fn sequences_ride_the_dash_with_nested_maps_aligned() {
    let yaml = yaml_of(
        r#"{"containers":[{"image":"nginx:1.27","name":"web","ports":[{"containerPort":80}]},{"name":"sidecar"}],"empty":[],"emptyMap":{}}"#,
    );
    let expected = "containers:\n  - image: nginx:1.27\n    name: web\n    ports:\n      - containerPort: 80\n  - name: sidecar\nempty: []\nemptyMap: {}\n";
    assert_eq!(yaml, expected);
}

#[test]
fn multiline_strings_become_literal_blocks_that_round_trip() {
    let yaml = yaml_of(r#"{"script":"line one\nline two\n","partial":"a\nb"}"#);
    assert!(yaml.contains("script: |\n  line one\n  line two\n"));
    assert!(yaml.contains("partial: |-\n  a\n  b\n"));
}

#[test]
fn hostile_multiline_strings_fall_back_to_escaped_quotes() {
    let yaml = yaml_of(
        "{\"trailing\":\"a \\nb\",\"control\":\"a\\u0007b\",\"doubled\":\"a\\n\\n\",\"separator\":\"a\\u2028b\"}",
    );
    assert!(yaml.contains(r#"trailing: "a \nb""#));
    assert!(yaml.contains(r#"control: "a\u0007b""#));
    assert!(yaml.contains(r#"doubled: "a\n\n""#));
    assert!(yaml.contains(r#"separator: "a\u2028b""#));
}

#[test]
fn keys_that_look_like_numbers_are_quoted_too() {
    let yaml = yaml_of(
        r#"{"metadata":{"annotations":{"8080":"port","kubectl.kubernetes.io/last-applied-configuration":"{}"}}}"#,
    );
    assert!(yaml.contains("\"8080\": port"));
    assert!(yaml.contains("kubectl.kubernetes.io/last-applied-configuration: \"{}\""));
}

#[test]
fn nested_sequences_indent_under_a_bare_dash() {
    let yaml = yaml_of(r#"{"matrix":[[1,2],[3]]}"#);
    assert_eq!(yaml, "matrix:\n  -\n    - 1\n    - 2\n  -\n    - 3\n");
}

#[test]
fn an_oversized_object_is_a_labelled_failure_not_a_truncation() {
    let big = "x".repeat(3 << 20);
    let value = serde_json::json!({ "data": big });
    let mut out = String::new();
    let result = emit_document(&mut out, &value);
    assert!(result.is_err(), "2 MiB is the cap");
}

#[test]
fn an_object_deeper_than_the_editor_renders_is_a_labelled_failure_too() {
    let mut value = serde_json::json!("leaf");
    for _ in 0..MAX_DEPTH + 6 {
        value = serde_json::json!({ "nest": value });
    }
    let mut out = String::new();
    let result = emit_document(&mut out, &value);
    assert!(result.is_err(), "{MAX_DEPTH} levels is the cap");
    let mut shallow = serde_json::json!("leaf");
    for _ in 0..MAX_DEPTH - 1 {
        shallow = serde_json::json!({ "nest": shallow });
    }
    round_trip(&shallow, "an object nested to the cap");
}
