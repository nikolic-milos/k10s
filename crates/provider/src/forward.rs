// A forward start request: a pod row forwards its first valid TCP port, a
// service row resolves through its selector and targetPort. Port choice and
// bounds live with the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub namespace: String,
    pub name: String,
    pub service: bool,
    /// An explicitly discovered Service port, or the first declared TCP port.
    pub service_port: Option<u16>,
    /// The catalogue Service identity reviewed before a browser handoff.
    pub expected_service: Option<std::sync::Arc<k10s_core::ServiceIdentity>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardState {
    Opening,
    Active,
    // Labelled and left visible until closed; a forward that vanishes reads
    // as one that still works.
    Dead(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardTerminal {
    Dead(String),
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRow {
    pub id: u64,
    pub namespace: String,
    pub pod: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub expected_service: Option<std::sync::Arc<k10s_core::ServiceIdentity>>,
    pub state: ForwardState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardOutcome {
    Opened(ForwardRow),
    Denied(&'static str),
    Failed(String),
}

/// A pending handoff. The provider releases its claim on discard and retains
/// the managed forward on a successful handoff.
#[must_use]
pub struct ForwardLease {
    row: ForwardRow,
    finish: Option<Box<dyn FnOnce(bool) + Send>>,
}

impl ForwardLease {
    /// `finish` receives true to retain the forward, or false to release the
    /// pending claim without retaining it.
    pub fn new(row: ForwardRow, finish: impl FnOnce(bool) + Send + 'static) -> Self {
        Self {
            row,
            finish: Some(Box::new(finish)),
        }
    }

    /// A reused forward stays under its existing registry ownership.
    pub fn reused(row: ForwardRow) -> Self {
        Self { row, finish: None }
    }

    pub fn row(&self) -> &ForwardRow {
        &self.row
    }

    pub fn keep(mut self) {
        if let Some(finish) = self.finish.take() {
            finish(true);
        }
    }
}

impl std::fmt::Debug for ForwardLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardLease")
            .field("row", &self.row)
            .finish()
    }
}

impl Drop for ForwardLease {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish(false);
        }
    }
}

#[derive(Debug)]
pub enum ForwardStartOutcome {
    Started(ForwardLease),
    Denied(&'static str),
    Failed(String),
}
