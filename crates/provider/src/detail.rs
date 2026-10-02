#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub when: String,
    pub kind: String,
    pub reason: String,
    pub message: String,
    pub count: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Detail {
    Events(Vec<EventRow>),
    /// Tail lines, and why they are these lines: a crash tail names the
    /// container that exited, so a quiet pane is never mistaken for a quiet
    /// process.
    Log(Vec<String>, Option<String>),
    Denied(&'static str),
    Failed(String),
}
