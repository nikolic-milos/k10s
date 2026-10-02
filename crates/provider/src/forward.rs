// A forward start request: a pod row forwards its first declared port, a
// service row resolves through its selector and targetPort. Port choice and
// bounds live with the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub namespace: String,
    pub name: String,
    pub service: bool,
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
pub struct ForwardRow {
    pub id: u64,
    pub namespace: String,
    pub pod: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub state: ForwardState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardOutcome {
    Opened(ForwardRow),
    Denied(&'static str),
    Failed(String),
}
