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
mod tests {
    use super::*;

    fn built(text: &str) -> (Rope, Syntax) {
        let rope = Rope::from(text);
        let mut syntax = Syntax::yaml();
        syntax.reparse(&rope);
        (rope, syntax)
    }

    fn applied(text: &str, status_subresource: bool) -> Payload {
        let (rope, syntax) = built(text);
        payload(&rope, &syntax, 0, status_subresource)
    }

    // Line continuations would eat the indentation, and indentation is the
    // document's structure.
    const POD: &str = concat!(
        "apiVersion: v1\n",
        "kind: Pod\n",
        "metadata:\n",
        "  creationTimestamp: \"2026-08-02T10:00:00Z\"\n",
        "  name: web\n",
        "  namespace: prod\n",
        "  resourceVersion: \"4821\"\n",
        "  uid: 0f2c-1\n",
        "spec:\n",
        "  containers:\n",
        "    - image: nginx:1.27\n",
        "      name: web\n",
        "status:\n",
        "  phase: Running\n",
    );

    #[test]
    fn server_owned_metadata_comes_out_and_intent_stays() {
        let payload = applied(POD, true);
        assert_eq!(
            payload.yaml,
            concat!(
                "apiVersion: v1\n",
                "kind: Pod\n",
                "metadata:\n",
                "  name: web\n",
                "  namespace: prod\n",
                "spec:\n",
                "  containers:\n",
                "    - image: nginx:1.27\n",
                "      name: web\n",
            )
        );
        assert_eq!(
            payload.pruned,
            vec![
                "metadata.creationTimestamp",
                "metadata.resourceVersion",
                "metadata.uid",
                "status",
            ],
            "named in document order so the UI can say what it sent"
        );
        assert!(payload.kept.is_empty());
    }

    #[test]
    fn status_stays_when_the_kind_has_no_status_subresource() {
        let payload = applied(POD, false);
        assert!(
            payload.yaml.contains("status:\n  phase: Running\n"),
            "a kind with no status subresource owns its own status: {}",
            payload.yaml
        );
        assert!(!payload.pruned.contains(&"status".to_string()));
    }

    #[test]
    fn a_multi_line_field_leaves_with_every_line_it_owns() {
        let text = concat!(
            "kind: Pod\n",
            "metadata:\n",
            "  managedFields:\n",
            "    - apiVersion: v1\n",
            "      manager: kubectl\n",
            "      operation: Apply\n",
            "  name: web\n",
        );
        let payload = applied(text, false);
        assert_eq!(payload.yaml, "kind: Pod\nmetadata:\n  name: web\n");
        assert_eq!(payload.pruned, vec!["metadata.managedFields"]);
    }

    #[test]
    fn the_last_applied_annotation_is_pruned_with_its_neighbours_intact() {
        let text = concat!(
            "kind: Pod\n",
            "metadata:\n",
            "  annotations:\n",
            "    kubectl.kubernetes.io/last-applied-configuration: \"{}\"\n",
            "    team: platform\n",
            "  name: web\n",
        );
        let payload = applied(text, false);
        assert_eq!(
            payload.yaml,
            "kind: Pod\nmetadata:\n  annotations:\n    team: platform\n  name: web\n"
        );
        assert_eq!(
            payload.pruned,
            vec!["metadata.annotations.kubectl.kubernetes.io/last-applied-configuration"]
        );
    }

    // A mapping emptied by the prune would be `annotations:` with no value,
    // which an apply reads as null and acts on: the parent goes instead.
    #[test]
    fn an_annotation_that_is_the_only_one_takes_its_mapping_with_it() {
        let text = concat!(
            "kind: Pod\n",
            "metadata:\n",
            "  annotations:\n",
            "    kubectl.kubernetes.io/last-applied-configuration: \"{}\"\n",
            "  name: web\n",
        );
        let payload = applied(text, false);
        assert_eq!(payload.yaml, "kind: Pod\nmetadata:\n  name: web\n");
        assert_eq!(payload.pruned, vec!["metadata.annotations"]);
    }

    // tree-sitter hangs a comment on whichever node it follows: one *above* the
    // sole pair belongs to the enclosing `annotations:` pair, one *below* it is
    // a named child of the mapping itself. Counting named children therefore
    // told the escalation the mapping had two entries when it had one, and
    // `annotations:` went out with no value -- which an apply reads as a request
    // to delete every annotation. Both placements are pinned because only the
    // second one ever failed.
    #[test]
    fn a_comment_beside_the_only_annotation_does_not_stand_in_for_a_sibling() {
        for text in [
            concat!(
                "kind: Pod\n",
                "metadata:\n",
                "  annotations:\n",
                "    # the only annotation\n",
                "    kubectl.kubernetes.io/last-applied-configuration: \"{}\"\n",
                "  name: web\n",
            ),
            concat!(
                "kind: Pod\n",
                "metadata:\n",
                "  annotations:\n",
                "    kubectl.kubernetes.io/last-applied-configuration: \"{}\"\n",
                "    # the only annotation\n",
                "  name: web\n",
            ),
        ] {
            let payload = applied(text, false);
            assert!(
                !payload.yaml.contains("annotations:"),
                "a mapping the prune emptied is null on the wire: {}",
                payload.yaml
            );
            assert_eq!(payload.pruned, vec!["metadata.annotations"]);
        }
    }

    #[test]
    fn a_metadata_holding_only_server_owned_fields_goes_whole() {
        let text = "kind: Pod\nmetadata:\n  uid: 0f2c-1\nspec: {}\n";
        let payload = applied(text, false);
        assert_eq!(payload.yaml, "kind: Pod\nspec: {}\n");
        assert_eq!(payload.pruned, vec!["metadata"]);
    }

    #[test]
    fn a_field_inside_a_flow_mapping_is_kept_and_named() {
        let text = "kind: Pod\nmetadata: {name: web, uid: 0f2c-1}\nspec: {}\n";
        let payload = applied(text, false);
        assert_eq!(
            payload.yaml, text,
            "removing one entry of an inline map would reshape the line"
        );
        assert_eq!(payload.kept, vec!["metadata.uid"]);
        assert!(payload.pruned.is_empty());
    }

    #[test]
    fn a_document_that_is_only_status_keeps_it_rather_than_emptying_itself() {
        let text = "status:\n  phase: Running\n";
        let payload = applied(text, true);
        assert_eq!(payload.yaml, text);
        assert_eq!(payload.kept, vec!["status"]);
    }

    #[test]
    fn a_comment_riding_a_pruned_field_leaves_with_it() {
        let text = "kind: Pod\nmetadata:\n  uid: 0f2c-1 # server-assigned\n  name: web\n";
        let payload = applied(text, false);
        assert_eq!(payload.yaml, "kind: Pod\nmetadata:\n  name: web\n");
    }

    #[test]
    fn a_document_with_nothing_to_prune_is_handed_back_unchanged() {
        let text = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: settings\ndata:\n  a: b\n";
        let payload = applied(text, true);
        assert_eq!(payload.yaml, text);
        assert!(payload.pruned.is_empty());
        assert!(payload.kept.is_empty());
    }

    #[test]
    fn only_the_named_document_is_sent() {
        let text = "kind: Pod\nmetadata:\n  name: one\n  uid: a\n---\nkind: Pod\nmetadata:\n  name: two\n  uid: b\n";
        let (rope, syntax) = built(text);
        let second = payload(&rope, &syntax, 1, false);
        assert!(
            second.yaml.contains("name: two") && !second.yaml.contains("name: one"),
            "one apply sends one object: {}",
            second.yaml
        );
        assert!(second.yaml.contains("uid: b"));
        assert!(second.pruned.is_empty());
        assert_eq!(
            second.blocked,
            vec!["a cluster apply needs exactly one YAML document"]
        );
    }

    #[test]
    fn a_field_missing_from_the_document_is_not_reported_as_pruned() {
        let text = "kind: Pod\nmetadata:\n  name: web\n  namespace: prod\n";
        let payload = applied(text, true);
        assert_eq!(payload.yaml, text);
        assert!(payload.pruned.is_empty());
    }

    #[test]
    fn a_document_without_a_final_newline_still_prunes_its_last_field() {
        let text = "kind: Pod\nmetadata:\n  name: web\n  uid: a";
        let payload = applied(text, false);
        assert_eq!(payload.yaml, "kind: Pod\nmetadata:\n  name: web\n");
        assert_eq!(payload.pruned, vec!["metadata.uid"]);
    }

    #[test]
    fn an_escaped_reserved_key_is_blocked_instead_of_bypassing_the_prune() {
        for text in [
            concat!(
                "kind: Pod\n",
                "metadata:\n",
                "  name: web\n",
                "  \"resource\\u0056ersion\": \"42\"\n",
            ),
            concat!(
                "kind: Pod\n",
                "\"meta\\u0064ata\":\n",
                "  name: web\n",
                "  uid: abc\n",
            ),
        ] {
            let payload = applied(text, false);
            assert_eq!(payload.yaml, text);
            assert_eq!(
                payload.blocked,
                vec!["encoded, tagged, merged, or aliased YAML keys cannot be pruned safely"]
            );
            assert!(payload.pruned.is_empty());
        }
    }

    #[test]
    fn aliases_and_merge_keys_fail_closed() {
        let text = concat!(
            "kind: Pod\n",
            "template: &meta\n",
            "  name: web\n",
            "  uid: abc\n",
            "metadata: *meta\n",
        );
        let payload = applied(text, false);
        assert!(
            payload
                .blocked
                .contains(&"encoded, tagged, merged, or aliased YAML keys cannot be pruned safely"),
            "{:?}",
            payload.blocked
        );
    }

    // Removing the first copy would leave the second as the live value while
    // the review said the field was gone -- and it would leave a document
    // strict validation now accepts, so the server would not catch it either.
    #[test]
    fn a_field_written_twice_in_one_mapping_is_blocked_rather_than_half_removed() {
        let text = concat!(
            "apiVersion: v1\n",
            "kind: ConfigMap\n",
            "metadata:\n",
            "  name: settings\n",
            "  resourceVersion: \"100\"\n",
            "  resourceVersion: \"4821\"\n",
            "data:\n",
            "  a: b\n",
        );
        let payload = applied(text, false);
        assert_eq!(
            payload.blocked,
            vec!["a field an apply must remove is written twice in one mapping"]
        );
        assert!(payload.pruned.is_empty());
        assert_eq!(
            payload.sendable(),
            Err(&["a field an apply must remove is written twice in one mapping"][..]),
            "and the unpruned bytes are not reachable"
        );
    }

    // The same hazard one hop up: resolution takes the first `metadata`, so the
    // second one keeps every server-owned field in it.
    #[test]
    fn a_mapping_on_the_way_to_the_field_written_twice_blocks_as_well() {
        let text = concat!(
            "kind: Pod\n",
            "metadata:\n",
            "  name: web\n",
            "metadata:\n",
            "  uid: 0f2c-1\n",
            "  name: web\n",
        );
        let payload = applied(text, false);
        assert!(
            payload
                .blocked
                .contains(&"a field an apply must remove is written twice in one mapping"),
            "{:?}",
            payload.blocked
        );
        assert!(payload.pruned.is_empty());
    }

    // One key spelled two ways is still one key: `scalar_text` unquotes, so the
    // duplicate has to be found after that and not before.
    #[test]
    fn a_duplicate_spelled_with_quotes_is_still_a_duplicate() {
        let text = "kind: Pod\nmetadata:\n  uid: a\n  \"uid\": b\n  name: web\n";
        let payload = applied(text, false);
        assert!(
            payload
                .blocked
                .contains(&"a field an apply must remove is written twice in one mapping"),
            "{:?}",
            payload.blocked
        );
    }

    // The bytes of a blocked payload are the document unpruned, which is the
    // one thing that must not reach the API server -- not even as a dry run.
    #[test]
    fn a_blocked_payload_hands_back_its_reasons_instead_of_its_bytes() {
        let text = "kind: Pod\nmetadata:\n  name: one\n  uid: a\n---\nkind: Pod\n";
        let (rope, syntax) = built(text);
        let payload = payload(&rope, &syntax, 0, false);
        assert_eq!(
            payload.sendable(),
            Err(&["a cluster apply needs exactly one YAML document"][..])
        );

        let clean = applied("kind: Pod\nmetadata:\n  name: web\n  uid: a\n", false);
        assert_eq!(
            clean.sendable(),
            Ok("kind: Pod\nmetadata:\n  name: web\n"),
            "and a payload with no reasons hands back the pruned bytes"
        );
    }

    #[test]
    fn syntax_errors_are_never_pruned_into_a_different_document() {
        let text = "kind: Pod\nmetadata:\n  uid: [\n";
        let payload = applied(text, false);
        assert_eq!(payload.yaml, text);
        assert!(
            payload
                .blocked
                .contains(&"the YAML document has syntax errors")
        );
        assert!(payload.pruned.is_empty());
    }
}
