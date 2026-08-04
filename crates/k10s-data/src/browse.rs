//! Arbitrary-kind browsing through server-side `Table` printing.
//!
//! A list view asks the API server to render columns (`Accept:
//! application/json;as=Table`) instead of shipping whole objects, which is
//! both the cold-start lever §5.1 names and the reason a Secret row can never
//! carry a value: the server's Table for secrets prints name, type, and a
//! count. One page per fetch, bounded by `PAGE_LIMIT`; a continue token
//! surfaces on the page for the caller to hand back explicitly -- the next
//! page is asked for, never chased. Cluster-wide lists of namespaced kinds
//! gain a leading namespace column here, so every consumer sees the same
//! shape kubectl users expect.

use kube::Client;
use kube::api::{ListParams, Request};
use serde::Deserialize;

use crate::discover::KindTarget;
use crate::read::{Fetched, classify, collection_path};

pub(crate) const TABLE_ACCEPT: &str =
    "application/json;as=Table;v=v1;g=meta.k8s.io, application/json";

const PAGE_LIMIT: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumn {
    pub name: String,
    // Columns the server marks priority > 0 are "wide" detail; the pane may
    // hide them when narrow, but the data plane always carries them.
    pub wide: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub cells: Vec<String>,
    pub name: String,
    pub namespace: Option<String>,
    pub uid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TablePage {
    pub columns: Vec<TableColumn>,
    pub rows: Vec<TableRow>,
    pub truncated: bool,
    // Present exactly when one more page can be requested by handing it
    // back; the node table is truncated without one because its per-node
    // scans cannot resume from a token.
    pub continue_token: Option<String>,
}

#[derive(Deserialize)]
struct WireTable {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    metadata: WireListMeta,
    #[serde(default, rename = "columnDefinitions")]
    column_definitions: Vec<WireColumn>,
    #[serde(default)]
    rows: Vec<WireRow>,
}

#[derive(Deserialize, Default)]
struct WireListMeta {
    #[serde(default, rename = "continue")]
    cont: String,
}

#[derive(Deserialize)]
struct WireColumn {
    #[serde(default)]
    name: String,
    #[serde(default)]
    priority: i32,
}

#[derive(Deserialize)]
struct WireRow {
    #[serde(default)]
    cells: Vec<serde_json::Value>,
    #[serde(default)]
    object: WireObject,
}

#[derive(Deserialize, Default)]
struct WireObject {
    #[serde(default)]
    metadata: WireMeta,
}

#[derive(Deserialize, Default)]
struct WireMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    uid: String,
}

pub(crate) async fn fetch_table(
    client: &Client,
    target: &KindTarget,
    namespace: Option<&str>,
    continue_token: Option<&str>,
) -> Fetched<TablePage> {
    let path = collection_path(target, namespace);
    let mut params = ListParams::default().limit(PAGE_LIMIT);
    if let Some(token) = continue_token {
        params = params.continue_token(token);
    }
    let mut request = match Request::new(path).list(&params) {
        Ok(request) => request,
        Err(error) => {
            return Fetched::Failed {
                what: "table",
                why: error.to_string(),
            };
        }
    };
    request.headers_mut().insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static(TABLE_ACCEPT),
    );
    match client.request::<WireTable>(request).await {
        Ok(wire) => shape(wire, target.namespaced && namespace.is_none()),
        Err(error) => classify("table", &error),
    }
}

fn shape(wire: WireTable, add_namespace: bool) -> Fetched<TablePage> {
    if wire.kind != "Table" {
        return Fetched::Failed {
            what: "table",
            why: format!(
                "the server answered with {:?} instead of a Table; falling back to whole \
                 objects is refused by design",
                wire.kind
            ),
        };
    }
    let mut columns: Vec<TableColumn> = Vec::with_capacity(wire.column_definitions.len() + 1);
    if add_namespace {
        columns.push(TableColumn {
            name: "Namespace".to_string(),
            wide: false,
        });
    }
    columns.extend(wire.column_definitions.into_iter().map(|c| TableColumn {
        name: c.name,
        wide: c.priority > 0,
    }));
    let rows = wire
        .rows
        .into_iter()
        .map(|row| {
            let mut cells: Vec<String> = Vec::with_capacity(row.cells.len() + 1);
            if add_namespace {
                cells.push(row.object.metadata.namespace.clone().unwrap_or_default());
            }
            cells.extend(row.cells.iter().map(cell_text));
            TableRow {
                cells,
                name: row.object.metadata.name,
                namespace: row.object.metadata.namespace,
                uid: row.object.metadata.uid,
            }
        })
        .collect();
    let continue_token = (!wire.metadata.cont.is_empty()).then_some(wire.metadata.cont);
    Fetched::Ok(TablePage {
        columns,
        rows,
        truncated: continue_token.is_some(),
        continue_token,
    })
}

// Cells are heterogeneous JSON; a person reads them as text. Strings pass
// through, scalars print, and anything structured stays compact JSON rather
// than pretending to be a scalar.
fn cell_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(json: &str) -> WireTable {
        serde_json::from_str(json).expect("the fixture parses")
    }

    #[test]
    fn a_table_keeps_server_columns_and_reads_row_identity_from_the_metadata() {
        let page = shape(
            wire(
                r#"{"kind":"Table","apiVersion":"meta.k8s.io/v1",
                    "metadata":{"resourceVersion":"1000"},
                    "columnDefinitions":[
                        {"name":"Name","type":"string","priority":0},
                        {"name":"Ready","type":"string","priority":0},
                        {"name":"Containers","type":"string","priority":1}],
                    "rows":[{"cells":["api-1","1/1",2],
                             "object":{"kind":"PartialObjectMetadata",
                                       "metadata":{"name":"api-1","namespace":"prod","uid":"uid-1"}}}]}"#,
            ),
            false,
        );
        let Fetched::Ok(page) = page else {
            panic!("expected a page, got {page:?}");
        };
        assert_eq!(
            page.columns
                .iter()
                .map(|c| (c.name.as_str(), c.wide))
                .collect::<Vec<_>>(),
            [("Name", false), ("Ready", false), ("Containers", true)]
        );
        assert_eq!(page.rows[0].cells, ["api-1", "1/1", "2"]);
        assert_eq!(page.rows[0].name, "api-1");
        assert_eq!(page.rows[0].namespace.as_deref(), Some("prod"));
        assert_eq!(page.rows[0].uid, "uid-1");
        assert!(!page.truncated);
    }

    #[test]
    fn a_cluster_wide_list_of_a_namespaced_kind_gains_a_namespace_column() {
        let page = shape(
            wire(
                r#"{"kind":"Table","metadata":{},
                    "columnDefinitions":[{"name":"Name","type":"string"}],
                    "rows":[{"cells":["api"],
                             "object":{"metadata":{"name":"api","namespace":"prod","uid":"u"}}}]}"#,
            ),
            true,
        );
        let Fetched::Ok(page) = page else {
            panic!("{page:?}")
        };
        assert_eq!(page.columns[0].name, "Namespace");
        assert_eq!(page.rows[0].cells, ["prod", "api"]);
    }

    #[test]
    fn a_continue_token_marks_the_page_truncated_instead_of_chasing_it() {
        let page = shape(
            wire(
                r#"{"kind":"Table","metadata":{"continue":"tok-1"},"columnDefinitions":[],"rows":[]}"#,
            ),
            false,
        );
        let Fetched::Ok(page) = page else {
            panic!("{page:?}")
        };
        assert!(page.truncated);
        assert_eq!(
            page.continue_token.as_deref(),
            Some("tok-1"),
            "the token is surfaced for an explicit next-page request"
        );

        let done = shape(
            wire(r#"{"kind":"Table","metadata":{},"columnDefinitions":[],"rows":[]}"#),
            false,
        );
        let Fetched::Ok(done) = done else {
            panic!("{done:?}")
        };
        assert!(!done.truncated);
        assert_eq!(done.continue_token, None);
    }

    #[test]
    fn a_server_that_ignores_the_table_accept_is_a_labelled_failure() {
        let outcome = shape(
            wire(r#"{"kind":"DeploymentList","metadata":{},"rows":[]}"#),
            false,
        );
        let Fetched::Failed { what, why } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(what, "table");
        assert!(why.contains("DeploymentList"), "{why}");
    }

    #[test]
    fn cells_read_as_text_whatever_json_the_server_printed() {
        assert_eq!(cell_text(&serde_json::json!("1/1")), "1/1");
        assert_eq!(cell_text(&serde_json::json!(3)), "3");
        assert_eq!(cell_text(&serde_json::json!(true)), "true");
        assert_eq!(cell_text(&serde_json::Value::Null), "");
        assert_eq!(cell_text(&serde_json::json!(["a", "b"])), r#"["a","b"]"#);
    }
}
