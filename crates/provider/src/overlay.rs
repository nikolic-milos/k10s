use k10s_core::Severity;

#[derive(Debug, Clone, PartialEq)]
pub struct OverlayStamp {
    pub uid: String,
    pub namespace: String,
    pub name: String,
    pub tint: Option<Severity>,
    pub samples: Vec<(i64, f64)>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OverlayOutcome {
    Ready {
        stamps: Vec<OverlayStamp>,
        truncated: bool,
        note: Option<String>,
    },
    Denied(&'static str),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodPostureView {
    pub ingress_isolated: bool,
    pub ingress_policies: usize,
    pub ingress_names: Vec<String>,
    pub ingress_truncated: bool,
    pub egress_isolated: bool,
    pub egress_policies: usize,
    pub egress_names: Vec<String>,
    pub egress_truncated: bool,
    pub ports: Vec<String>,
    pub completeness: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostureOutcome {
    Ready(PodPostureView),
    Missing,
    Denied(&'static str),
    Failed(String),
}
