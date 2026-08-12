//! How schema answers land in the per-workspace store, without a provider or
//! a window. The rule with real bite is once-only: the store is shared by
//! every editor in the workspace precisely so a second editor on the same
//! group version fetches nothing, and `next_document_url` is where that
//! promise is kept.

use crate::editor::SchemaStore;
use crate::editor_io::{SchemaDoc, absorb_catalog, absorb_schema_text, next_document_url};
use crate::provider::{SchemaCatalogOutcome, SchemaSource, SchemaTextOutcome};

fn catalog() -> SchemaCatalogOutcome {
    SchemaCatalogOutcome::Catalog(vec![
        SchemaSource {
            group_version: "apps/v1".to_string(),
            url: "/openapi/v3/apis/apps/v1".to_string(),
        },
        SchemaSource {
            group_version: "v1".to_string(),
            url: "/openapi/v3/api/v1".to_string(),
        },
    ])
}

#[test]
fn a_catalog_registers_every_group_version_before_it_is_stored() {
    let mut store = SchemaStore::new();
    absorb_catalog(&mut store, catalog());
    let known: Vec<&str> = store.index.api_versions().collect();
    assert!(
        known.contains(&"apps/v1") && known.contains(&"v1"),
        "a document the index cannot name must never be offered for fetching: {known:?}"
    );
    assert!(store.first_note().is_none());
}

#[test]
fn a_denied_catalog_is_worded_for_the_person_reading_the_notes() {
    let mut store = SchemaStore::new();
    absorb_catalog(&mut store, SchemaCatalogOutcome::Denied("schema catalog"));
    assert_eq!(
        store.first_note(),
        Some("schema catalog: access denied for this account")
    );
}

#[test]
fn a_group_versions_document_is_fetched_at_most_once_per_store() {
    let mut store = SchemaStore::new();
    absorb_catalog(&mut store, catalog());

    assert_eq!(
        next_document_url(&mut store, "apps/v1").as_deref(),
        Some("/openapi/v3/apis/apps/v1")
    );
    assert_eq!(
        next_document_url(&mut store, "apps/v1"),
        None,
        "a second editor on the same group version fetches nothing"
    );
    assert_eq!(
        next_document_url(&mut store, "v1").as_deref(),
        Some("/openapi/v3/api/v1"),
        "another group version still owes its own fetch"
    );
    assert_eq!(
        next_document_url(&mut store, "batch/v1"),
        None,
        "a group version the catalog does not name has no URL to fetch"
    );
}

#[test]
fn schema_text_that_will_not_parse_is_a_note_that_names_which_document() {
    let mut store = SchemaStore::new();
    absorb_schema_text(
        &mut store,
        SchemaTextOutcome::Text("not json".to_string()),
        SchemaDoc::OpenApi,
    );
    let note = store.first_note().expect("a bad document is a note");
    assert!(note.starts_with("schema document: "), "{note}");

    let mut store = SchemaStore::new();
    absorb_schema_text(
        &mut store,
        SchemaTextOutcome::Text("not json".to_string()),
        SchemaDoc::CrdList,
    );
    let note = store.first_note().expect("a bad CRD list is a note");
    assert!(note.starts_with("CRD schemas: "), "{note}");
}

#[test]
fn the_denied_and_failed_arms_are_shared_between_both_documents() {
    for kind in [SchemaDoc::OpenApi, SchemaDoc::CrdList] {
        let mut store = SchemaStore::new();
        absorb_schema_text(&mut store, SchemaTextOutcome::Denied("schemas"), kind);
        assert_eq!(
            store.first_note(),
            Some("schemas: access denied for this account"),
            "{kind:?}"
        );

        let mut store = SchemaStore::new();
        absorb_schema_text(
            &mut store,
            SchemaTextOutcome::Failed("timed out".to_string()),
            kind,
        );
        assert_eq!(store.first_note(), Some("timed out"), "{kind:?}");
    }
}
