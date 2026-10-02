//! How schema answers land in the per-connection store, without a provider or
//! a window. The rule with real bite is once-only: the store is shared by
//! every editor in the workspace precisely so a second editor on the same
//! group version fetches nothing, and `next_document_url` is where that
//! promise is kept. The rules beside it are about identity and failure: an
//! answer belongs to the connection it was asked of, and a fetch that never
//! arrived has to be askable again.

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

fn openapi() -> SchemaDoc {
    SchemaDoc::OpenApi("apps/v1".to_string())
}

#[test]
fn a_catalog_registers_every_group_version_before_it_is_stored() {
    let mut store = SchemaStore::new();
    let epoch = store.epoch();
    absorb_catalog(&mut store, catalog(), epoch);
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
    let epoch = store.epoch();
    absorb_catalog(
        &mut store,
        SchemaCatalogOutcome::Denied("schema catalog"),
        epoch,
    );
    assert_eq!(
        store.first_note(),
        Some("schema catalog: access denied for this account")
    );
}

#[test]
fn a_group_versions_document_is_fetched_at_most_once_per_store() {
    let mut store = SchemaStore::new();
    let epoch = store.epoch();
    absorb_catalog(&mut store, catalog(), epoch);

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
    let epoch = store.epoch();
    absorb_schema_text(
        &mut store,
        SchemaTextOutcome::Text("not json".to_string()),
        &openapi(),
        epoch,
    );
    let note = store.first_note().expect("a bad document is a note");
    assert!(note.starts_with("schema document: "), "{note}");

    let mut store = SchemaStore::new();
    let epoch = store.epoch();
    absorb_schema_text(
        &mut store,
        SchemaTextOutcome::Text("not json".to_string()),
        &SchemaDoc::CrdList,
        epoch,
    );
    let note = store.first_note().expect("a bad CRD list is a note");
    assert!(note.starts_with("CRD schemas: "), "{note}");
}

#[test]
fn the_denied_and_failed_arms_are_shared_between_both_documents() {
    for kind in [openapi(), SchemaDoc::CrdList] {
        let mut store = SchemaStore::new();
        let epoch = store.epoch();
        absorb_schema_text(
            &mut store,
            SchemaTextOutcome::Denied("schemas"),
            &kind,
            epoch,
        );
        assert_eq!(
            store.first_note(),
            Some("schemas: access denied for this account"),
            "{kind:?}"
        );

        let mut store = SchemaStore::new();
        let epoch = store.epoch();
        absorb_schema_text(
            &mut store,
            SchemaTextOutcome::Failed("timed out".to_string()),
            &kind,
            epoch,
        );
        assert_eq!(store.first_note(), Some("timed out"), "{kind:?}");
    }
}

#[test]
fn a_retired_store_owes_every_fetch_again() {
    let mut store = SchemaStore::new();
    let epoch = store.epoch();
    absorb_catalog(&mut store, catalog(), epoch);
    store.requested_catalog = true;
    store.requested_crds = true;
    assert!(next_document_url(&mut store, "apps/v1").is_some());
    store.note("something the previous cluster said".to_string());

    store.retire();

    assert!(
        !store.requested_catalog && !store.requested_crds,
        "the next editor must ask the cluster it is actually looking at"
    );
    assert!(
        store.first_note().is_none(),
        "a note about the previous cluster is not about this one"
    );
    assert_eq!(
        store.index.api_versions().count(),
        0,
        "the index describes one API server and the window has left it"
    );
    let epoch = store.epoch();
    absorb_catalog(&mut store, catalog(), epoch);
    assert_eq!(
        next_document_url(&mut store, "apps/v1").as_deref(),
        Some("/openapi/v3/apis/apps/v1"),
        "the document this group version owes is owed again to the new cluster"
    );
}

#[test]
fn an_answer_from_the_previous_cluster_never_lands_in_the_store_that_replaced_it() {
    let mut store = SchemaStore::new();
    let asked_at = store.epoch();
    store.retire();

    absorb_catalog(&mut store, catalog(), asked_at);
    assert_eq!(
        store.index.api_versions().count(),
        0,
        "a catalog in flight across a cluster switch describes the cluster that left"
    );

    absorb_schema_text(
        &mut store,
        SchemaTextOutcome::Failed("timed out".to_string()),
        &SchemaDoc::CrdList,
        asked_at,
    );
    assert!(
        store.first_note().is_none(),
        "a failure from the previous cluster is not this cluster's note"
    );
}

#[test]
fn a_fetch_that_failed_can_be_asked_for_again() {
    let mut store = SchemaStore::new();
    let epoch = store.epoch();
    store.requested_catalog = true;
    absorb_catalog(
        &mut store,
        SchemaCatalogOutcome::Failed("connection reset".to_string()),
        epoch,
    );
    assert!(
        !store.requested_catalog,
        "one transient failure must not leave the workspace validating blind until restart"
    );

    store.requested_crds = true;
    absorb_schema_text(
        &mut store,
        SchemaTextOutcome::Denied("CRD schemas"),
        &SchemaDoc::CrdList,
        epoch,
    );
    assert!(!store.requested_crds);

    absorb_catalog(&mut store, catalog(), epoch);
    assert!(next_document_url(&mut store, "apps/v1").is_some());
    absorb_schema_text(
        &mut store,
        SchemaTextOutcome::Failed("connection reset".to_string()),
        &openapi(),
        epoch,
    );
    assert_eq!(
        next_document_url(&mut store, "apps/v1").as_deref(),
        Some("/openapi/v3/apis/apps/v1"),
        "a document whose fetch failed is still owed"
    );
}
