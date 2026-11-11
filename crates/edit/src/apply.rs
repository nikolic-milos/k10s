//! Turning an edited manifest into a server-side apply payload.
//!
//! An apply declares intent, and intent is not the whole object. Every field
//! the API server writes for itself has to come out first, or `k10s` becomes
//! that field's manager and the next apply by anything else conflicts with a
//! claim nobody made. `metadata.resourceVersion` is the sharpest of them: left
//! in, it turns every apply into an optimistic-lock precondition that fails
//! whenever anything moved the object since the fetch -- including a status
//! update the user never saw. `status` comes out only when discovery says the
//! kind has a status subresource, because on a custom resource that has none,
//! `status` is the author's own field and removing it would remove intent.
//!
//! The prune is a text edit over the syntax tree rather than a parse and
//! reprint, because the payload is sent as `application/apply-patch+yaml`
//! carrying the buffer's own bytes: the API server stays the only thing in the
//! chain that resolves YAML, so nothing here can disagree with it about what
//! `on` or `y` means. A field this cannot remove without reshaping the document
//! -- one inside a flow mapping, or one that is the only thing in its document
//! -- is left in place and named in [`Payload::kept`], never silently mangled,
//! and a review that does not repeat those names is a review that is missing
//! part of what it is describing.
//!
//! The bytes are reachable only through [`Payload::sendable`], which hands back
//! the reasons instead when there are any. That is deliberate: a blocked
//! payload's `yaml` is the document exactly as it stands, unpruned, and the one
//! thing it must never do is reach the API server -- including as a dry run,
//! whose whole purpose is to be the review of the bytes that follow it. A rule
//! enforced by an `if` at a call site is a rule the next call site forgets.

use std::ops::Range;

use crate::rope::Rope;
use crate::syntax::{PathSeg, Resolved, Syntax};

/// What a declarative apply must never claim, because the API server writes it.
const SERVER_OWNED: &[&[&str]] = &[
    &["metadata", "resourceVersion"],
    &["metadata", "uid"],
    &["metadata", "generation"],
    &["metadata", "creationTimestamp"],
    &["metadata", "selfLink"],
    &["metadata", "managedFields"],
    &["metadata", "deletionTimestamp"],
    &["metadata", "deletionGracePeriodSeconds"],
    &[
        "metadata",
        "annotations",
        "kubectl.kubernetes.io/last-applied-configuration",
    ],
];

const STATUS: &[&str] = &["status"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Payload {
    // The bytes to send, one document, server-owned fields removed -- or, when
    // `blocked` is not empty, the document unpruned. Private because those two
    // are indistinguishable as a `String` and only one of them may be sent;
    // `sendable` is the only way out.
    yaml: String,
    /// Which fields were removed, dotted, in document order -- the UI names
    /// them rather than quietly editing what the user is about to send.
    pub pruned: Vec<String>,
    /// Fields found but left in place because removing them would have
    /// reshaped the document. These are going on the wire, so a review that
    /// names what was removed has to name these too.
    pub kept: Vec<String>,
    /// Structural reasons this document cannot be pruned safely at all. A
    /// caller must never put such a payload on the wire, including as a dry
    /// run: the bytes would not be the document the review describes.
    pub blocked: Vec<&'static str>,
}

impl Payload {
    /// The bytes to send, or the reasons this document must not be sent. A
    /// blocked payload's bytes are the document as it stands, unpruned, which
    /// is exactly what must never reach the API server -- so they are only
    /// reachable through here.
    pub fn sendable(&self) -> Result<&str, &[&'static str]> {
        if self.blocked.is_empty() {
            return Ok(&self.yaml);
        }
        Err(&self.blocked)
    }
}

pub fn payload(
    rope: &Rope,
    syntax: &Syntax,
    document_index: usize,
    status_subresource: bool,
) -> Payload {
    let documents = syntax.document_ranges(rope);
    let Some(span) = documents.get(document_index).cloned() else {
        return Payload {
            blocked: vec!["the selected YAML document no longer exists"],
            ..Payload::default()
        };
    };
    let mut blocked = Vec::new();
    if documents.len() != 1 {
        blocked.push("a cluster apply needs exactly one YAML document");
    }
    if syntax
        .error_ranges()
        .iter()
        .any(|error| error.start < span.end && span.start < error.end)
    {
        blocked.push("the YAML document has syntax errors");
    }
    if syntax.has_ambiguous_yaml_structure(rope, document_index) {
        blocked.push("encoded, tagged, merged, or aliased YAML keys cannot be pruned safely");
    }
    for (path, reason) in [
        (
            &["metadata"][..],
            "metadata must be a literal mapping before it can be applied",
        ),
        (
            &["metadata", "annotations"][..],
            "metadata.annotations must be a literal mapping before it can be applied",
        ),
    ] {
        let segments: Vec<PathSeg> = path
            .iter()
            .map(|key| PathSeg::Key((*key).to_string()))
            .collect();
        if matches!(
            syntax.pair_at(rope, document_index, &segments),
            Resolved::Pair(_)
        ) && !syntax.is_mapping_at(rope, document_index, &segments)
        {
            blocked.push(reason);
        }
    }
    if !blocked.is_empty() {
        return Payload {
            yaml: rope.slice_to_string(span),
            blocked,
            ..Payload::default()
        };
    }
    let mut found: Vec<(Range<usize>, String)> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    let mut repeated = false;
    let wanted = SERVER_OWNED
        .iter()
        .copied()
        .chain(status_subresource.then_some(STATUS));
    for path in wanted {
        match locate(rope, syntax, document_index, path) {
            Located::Removable(range, name)
                if span.start <= range.start && range.end <= span.end =>
            {
                found.push((range, name));
            }
            Located::Removable(_, name) | Located::Inline(name) => kept.push(name),
            Located::Repeated => repeated = true,
            Located::Absent => {}
        }
    }
    // Removing the first of two identically spelled keys leaves the second one
    // behind *and* leaves the document valid, so it slips past the strict
    // validation that would otherwise have caught the duplicate: the copy the
    // prune reports as gone becomes the live one. Nothing here can tell which
    // copy the server would have read, so it removes neither.
    if repeated {
        blocked.push("a field an apply must remove is written twice in one mapping");
        return Payload {
            yaml: rope.slice_to_string(span),
            blocked,
            ..Payload::default()
        };
    }
    let removals = flatten(found);
    let mut yaml = rope.slice_to_string(span.clone());
    for (range, _) in removals.iter().rev() {
        yaml.replace_range(range.start - span.start..range.end - span.start, "");
    }
    Payload {
        yaml,
        pruned: removals.into_iter().map(|(_, name)| name).collect(),
        kept,
        blocked,
    }
}

enum Located {
    Removable(Range<usize>, String),
    Inline(String),
    // This field, or a mapping on the way to it, is spelled twice. Which copy
    // the server reads is not knowable from here, so nothing is removed.
    Repeated,
    Absent,
}

fn locate(rope: &Rope, syntax: &Syntax, document_index: usize, path: &[&str]) -> Located {
    let segments: Vec<PathSeg> = path
        .iter()
        .map(|key| PathSeg::Key((*key).to_string()))
        .collect();
    let pair = match syntax.pair_at(rope, document_index, &segments) {
        Resolved::Pair(pair) => pair,
        Resolved::Repeated => return Located::Repeated,
        Resolved::Absent => return Located::Absent,
    };
    // The last pair of a mapping cannot come out alone. `annotations:` with
    // nothing under it is not an empty map in an apply, it is null -- a request
    // to delete every annotation -- so the mapping's own pair goes instead.
    if pair.siblings == 1 {
        if path.len() == 1 {
            return Located::Inline(path.join("."));
        }
        return locate(rope, syntax, document_index, &path[..path.len() - 1]);
    }
    match line_span(rope, &pair.bytes) {
        Some(range) => Located::Removable(range, path.join(".")),
        None => Located::Inline(path.join(".")),
    }
}

// The pair widened to whole lines: the indentation before its key, and the line
// break after its last line. A pair that does not own its lines -- one inside a
// flow mapping, one sharing a line with a sequence dash -- cannot be removed
// this way without changing what surrounds it, and says so instead. A trailing
// comment is content the field owns, so it leaves with the field.
fn line_span(rope: &Rope, bytes: &Range<usize>) -> Option<Range<usize>> {
    let first = rope.byte_to_point(bytes.start).row;
    let start = rope.line_start(first);
    let before = rope.slice_to_string(start..bytes.start);
    if !before.chars().all(|character| character.is_whitespace()) {
        return None;
    }
    let last = rope.byte_to_point(bytes.end).row;
    let end = rope.line_start(last) + rope.line_len(last);
    let after = rope.slice_to_string(bytes.end..end);
    let trailing = after.trim_start();
    if !trailing.is_empty() && !trailing.starts_with('#') {
        return None;
    }
    Some(start..(end + 1).min(rope.len()))
}

// One escalated parent and one of its own children can both be located; the
// wider range subsumes the narrower, and splicing both would cut twice.
fn flatten(mut found: Vec<(Range<usize>, String)>) -> Vec<(Range<usize>, String)> {
    found.sort_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.end)));
    let mut kept: Vec<(Range<usize>, String)> = Vec::new();
    for (range, name) in found {
        let covered = kept
            .iter()
            .any(|(existing, _)| existing.start <= range.start && range.end <= existing.end);
        if covered {
            continue;
        }
        kept.push((range, name));
    }
    kept
}

#[cfg(test)]
#[path = "apply_test.rs"]
mod tests;
