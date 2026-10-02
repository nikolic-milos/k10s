use k10s_core::KindId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    pub previous: bool,
}

// Logs for a whole workload: the provider finds the pods (label selector or
// ownership, its choice) and merges their follows into one feed, each line
// naming its pod. The pod-count bound lives with the implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadLogRequest {
    pub namespace: String,
    pub kind: KindId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogChunk {
    Lines(Vec<String>),
    Ended(String),
    Denied(&'static str),
    Failed(String),
}

// Dropping the guard cancels the follow; the adapter decides what that means.
pub struct LogStop(Option<Box<dyn FnOnce() + Send>>);

impl LogStop {
    pub fn new(stop: impl FnOnce() + Send + 'static) -> LogStop {
        LogStop(Some(Box::new(stop)))
    }

    pub fn noop() -> LogStop {
        LogStop(None)
    }
}

impl Drop for LogStop {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop();
        }
    }
}
