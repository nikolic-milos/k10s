#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    pub command: Vec<String>,
    /// Attach to the running process (stdin, no TTY command) rather than
    /// exec a shell. The transport still takes this request; a backend that
    /// does not yet distinguish the two ignores the flag and execs.
    pub attach: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvent {
    Output(Vec<u8>),
    Ended(String),
    Denied(&'static str),
    Failed(String),
}

// The live half of an exec: keystrokes and resizes go down it, dropping it
// terminates the remote session.
pub trait ExecSession: Send {
    fn write(&self, bytes: &[u8]);
    fn resize(&self, cols: u16, rows: u16);
}

pub struct NullExecSession;

impl ExecSession for NullExecSession {
    fn write(&self, _: &[u8]) {}
    fn resize(&self, _: u16, _: u16) {}
}
