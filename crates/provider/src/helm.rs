/// Bytes that just left a reveal Scratch. Open them into an editor scratch
/// and drop this value. Never a table cell, a saved view, or a log line --
/// which is why `Debug` below is written by hand to redact the payloads.
#[derive(PartialEq, Eq)]
pub struct HelmReveal {
    pub name: String,
    pub namespace: String,
    pub revision: u32,
    pub config: String,
    pub chart_values: String,
    pub manifest: String,
}

impl std::fmt::Debug for HelmReveal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelmReveal")
            .field("name", &self.name)
            .field("namespace", &self.namespace)
            .field("revision", &self.revision)
            .field("config", &"[redacted]")
            .field("chart_values", &"[redacted]")
            .field("manifest", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HelmRevealOutcome {
    Revealed(HelmReveal),
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelmRollbackOutcome {
    Report { note: String, lines: Vec<String> },
    Denied(&'static str),
    Failed(String),
}
