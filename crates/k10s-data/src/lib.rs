use crossbeam_channel::Sender;
use k10s_core::Severity;

#[derive(Debug, Clone, Copy)]
pub enum ObjKind {
    Namespace,
    Deployment,
    StatefulSet,
    DaemonSet,
    Job,
    Service,
    Pod,
}

#[derive(Debug, Clone, Copy)]
pub enum Op {
    Upsert,
    Delete,
}

#[derive(Debug, Clone)]
pub struct ObjEvent {
    pub kind: ObjKind,
    pub namespace: String,
    pub name: String,
    pub op: Op,
    pub health: Severity,
}

pub struct DataPlane {
    runtime: tokio::runtime::Runtime,
}

impl DataPlane {
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }
}

pub fn spawn(events: Sender<ObjEvent>) -> std::io::Result<DataPlane> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("k10s-data")
        .enable_all()
        .build()?;
    let _ = events;
    Ok(DataPlane { runtime })
}
