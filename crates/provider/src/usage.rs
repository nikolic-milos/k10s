use k10s_core::KindId;

/// CPU in millicores, mirrored across the seam so usage and its bounds never
/// travel as unlabelled numbers. `Display` renders for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Millicores(pub u64);

impl std::fmt::Display for Millicores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 < 1000 {
            return write!(f, "{}m", self.0);
        }
        let cores = format!("{:.2}", self.0 as f64 / 1000.0);
        let cores = cores.trim_end_matches('0').trim_end_matches('.');
        if cores == "1" {
            write!(f, "1 core")
        } else {
            write!(f, "{cores} cores")
        }
    }
}

/// Memory in bytes, mirrored for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(pub u64);

impl std::fmt::Display for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const KI: f64 = 1024.0;
        let b = self.0 as f64;
        if b >= KI * KI * KI {
            write!(f, "{:.1}Gi", b / (KI * KI * KI))
        } else if b >= KI * KI {
            write!(f, "{:.0}Mi", b / (KI * KI))
        } else if b >= KI {
            write!(f, "{:.0}Ki", b / KI)
        } else {
            write!(f, "{}", self.0)
        }
    }
}

// A usage poll names a pod or a workload; the provider owns how the numbers
// are obtained and at what cadence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRequest {
    pub namespace: String,
    pub target: UsageTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageTarget {
    Pod { name: String },
    Workload { kind: KindId, name: String },
}

// Which endpoint produced the numbers: the display says so when the answer
// came the degraded way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    MetricsServer,
    Kubelet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSample {
    // None is "not measured yet", never zero: a kubelet's first sample has no
    // CPU rate and a pod no source has scraped has neither number.
    pub cpu: Option<Millicores>,
    pub memory: Option<Bytes>,
    // From the pod specs. A limit is only a number when every running
    // container carries one; a request only when something declares one.
    pub cpu_request: Option<Millicores>,
    pub cpu_limit: Option<Millicores>,
    pub memory_request: Option<Bytes>,
    pub memory_limit: Option<Bytes>,
    pub source: UsageSource,
    // What the numbers cover, so a partial sum can never pass as the whole.
    pub pods_measured: usize,
    pub pods_total: usize,
    pub truncated: bool,
}

// Usage is a state, not a number: a cluster without a metrics source is
// Absent with the reason, a 403 is Denied, and neither may render as zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageOutcome {
    Usage(UsageSample),
    Denied(&'static str),
    Failed(String),
    Absent(String),
}
