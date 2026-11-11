use k10s_core::KindId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindRow {
    pub id: KindId,
    // "deployments.apps" -- the name a kubectl user would type.
    pub display: String,
    pub kind: String,
    pub namespaced: bool,
    // The probe said no: shown disabled with the reason, still openable, and
    // the table answers with the denial the server actually returns.
    pub forbidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumn {
    pub name: String,
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
    // Present exactly when the server offers a next page; handing it back to
    // `fetch_table` loads that page. The node table truncates without one.
    pub continue_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableOutcome {
    Table(TablePage),
    /// The adapter is not on this cluster. The view stays invisible rather
    /// than opening an empty pane that looks broken.
    Absent,
    Denied(&'static str),
    Failed(String),
}

/// One ecosystem family's answer. [`TableOutcome::Absent`] means the family
/// is not on this cluster and its row stays hidden; the id is the stable
/// key the ecosystem pane joins its presentation onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcosystemEntry {
    pub id: &'static str,
    pub outcome: TableOutcome,
}
