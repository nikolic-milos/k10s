//! What leaves a document before it is applied and what must stay. The prune
//! fails closed: aliases, merge keys, escaped reserved keys and a field written
//! twice in one mapping block the payload rather than being half-removed, and a
//! syntax error is never pruned into a different document.

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
