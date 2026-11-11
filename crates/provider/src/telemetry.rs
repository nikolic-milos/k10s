use crate::DescribeRequest;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FetchTiming {
    pub bytes: usize,
    pub network_ns: u64,
    pub decode_ns: u64,
    pub projection_ns: u64,
    pub queue_ns: u64,
    pub total_ns: u64,
    pub requests: usize,
    pub total_bytes: usize,
    pub binding_network_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryWindow {
    pub start_ms: i64,
    pub end_ms: i64,
    pub step_ms: u64,
}

impl QueryWindow {
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.start_ms < 0
            || self.end_ms < self.start_ms
            || self.step_ms == 0
            || self.end_ms > (u64::MAX / 1_000_000) as i64
        {
            return Err("the query range or resolution is invalid");
        }
        if self.end_ms - self.start_ms > 86_400_000
            || (self.end_ms - self.start_ms) as u64 / self.step_ms > 4_999
        {
            return Err("the query exceeds one day or 5000 samples per series");
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodIdentity {
    pub name: String,
    pub uid: String,
    pub node: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveScope {
    pub request: DescribeRequest,
    pub namespace: String,
    pub pods: Vec<PodIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeOutcome {
    Ready(ObserveScope),
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromRequest {
    pub expr: String,
    pub window: QueryWindow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LokiRequest {
    pub query: String,
    pub window: QueryWindow,
    pub limit: usize,
}

/// A view owns this guard for exactly as long as its result is wanted.
pub struct RequestStop(Option<Box<dyn FnOnce() + Send>>);
impl RequestStop {
    pub fn new(stop: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(stop)))
    }
    pub fn noop() -> Self {
        Self(None)
    }
}
impl Drop for RequestStop {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop();
        }
    }
}

pub fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

pub fn exact_pattern<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut pattern = String::from("^(?:");
    for (index, name) in names.into_iter().enumerate() {
        if index > 0 {
            pattern.push('|');
        }
        for character in name.chars() {
            if "\\.^$|?*+()[]{}".contains(character) {
                pattern.push('\\');
            }
            pattern.push(character);
        }
    }
    pattern.push_str(")$");
    pattern
}

impl ObserveScope {
    /// kube_pod_info carries UID, including across same-name recreations.
    pub fn readiness_query(&self) -> Option<String> {
        if self.pods.is_empty() {
            return None;
        }
        let pods = quote(&exact_pattern(
            self.pods.iter().map(|pod| pod.name.as_str()),
        ));
        let uids = quote(&exact_pattern(self.pods.iter().map(|pod| pod.uid.as_str())));
        let ns = quote(&self.namespace);
        Some(format!(
            "kube_pod_status_ready{{namespace={ns},pod=~{pods},condition=\"true\"}} * on(namespace,pod) group_left(uid) kube_pod_info{{namespace={ns},pod=~{pods},uid=~{uids}}}"
        ))
    }

    /// Existing label conventions are explicit alternatives. UID-less lines
    /// remain a name association in the view, never a UID correlation.
    pub fn logs_query(&self) -> Option<String> {
        if self.pods.is_empty() {
            return None;
        }
        let pods = quote(&exact_pattern(
            self.pods.iter().map(|pod| pod.name.as_str()),
        ));
        let ns = quote(&self.namespace);
        Some(format!("{{namespace={ns},pod=~{pods}}}"))
    }

    pub fn otlp_logs_query(&self) -> Option<String> {
        if self.pods.is_empty() {
            return None;
        }
        let pods = quote(&exact_pattern(
            self.pods.iter().map(|pod| pod.name.as_str()),
        ));
        let uids = quote(&exact_pattern(self.pods.iter().map(|pod| pod.uid.as_str())));
        Some(format!(
            "{{k8s_namespace_name={},k8s_pod_name=~{pods}}} | k8s_pod_uid=~{uids}",
            quote(&self.namespace)
        ))
    }
}
