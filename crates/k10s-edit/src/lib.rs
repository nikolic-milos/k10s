//! The editor engine: a small rope, multi-cursor edits, YAML structure, and
//! cluster-schema completion -- no gpui, no LSP, no embedded editor crate.
//!
//! §5.2's constraint is structural: the buffer is a deliberately small
//! persistent rope (`rope`), mutation is multi-cursor transactions with
//! snapshot undo (`buffer`), search shares the shell's labelled-regex
//! discipline (`search`), structure is an incrementally reparsed
//! tree-sitter YAML tree with multi-document awareness (`syntax`), and
//! completion and validation walk the cluster's own OpenAPI v3 and CRD
//! schemas parsed into a bounded index (`schema`, `complete`), and accepting a
//! completion is one pure per-language edit builder (`insert`). Comparing a
//! buffer with the cluster is a three-way line diff whose third document is
//! what was last applied (`diff`), and turning one into something safe to send
//! is a text prune over the tree (`apply`). Everything here is a pure state
//! machine a thin gpui view projects.

pub mod apply;
pub mod buffer;
pub mod complete;
pub mod diff;
pub mod insert;
pub mod rope;
pub mod schema;
pub mod search;
pub mod syntax;

pub use apply::Payload;
pub use buffer::{Buffer, EditGroup, Motion, Selection, SelectionIntent, Splice};
pub use complete::{
    Completion, CompletionKind, Diagnostic, DiagnosticSeverity, DocMeta, Slot, doc_meta, validate,
};
pub use diff::{Diff, Hunk, Origin, Side, Sides, Verdict, three_way};
pub use insert::{CompletionEdit, completion_edit};
pub use rope::{Point, Rope};
pub use schema::SchemaIndex;
pub use search::{Replacement, SearchState};
pub use syntax::{
    CursorContext, CursorPosition, LanguageKind, Pair, PathSeg, Resolved, Syntax, TokenKind,
};
