//! Overlay identities shared by discovery, queries, saved views and painting.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Sync,
    Metrics,
    Policy,
    Netpol,
    MeshDeclared,
    MeshObserved,
}

impl OverlayKind {
    pub const ALL: [OverlayKind; 6] = [
        OverlayKind::Sync,
        OverlayKind::Metrics,
        OverlayKind::Policy,
        OverlayKind::Netpol,
        OverlayKind::MeshDeclared,
        OverlayKind::MeshObserved,
    ];

    /// Short HUD badge. These six strings are the overlay's name; they must
    /// not be reused as health, LOD, or each other's copy.
    pub fn badge(self) -> &'static str {
        match self {
            OverlayKind::Sync => "SYNC",
            OverlayKind::Metrics => "METRICS",
            OverlayKind::Policy => "POLICY",
            OverlayKind::Netpol => "NETPOL",
            OverlayKind::MeshDeclared => "MESH DECLARED",
            OverlayKind::MeshObserved => "MESH OBSERVED",
        }
    }

    /// What this overlay is, in one line. Declared mesh and observed mesh are
    /// different sentences on purpose.
    pub fn blurb(self) -> &'static str {
        match self {
            OverlayKind::Sync => "GitOps desired versus live",
            OverlayKind::Metrics => "series from in-cluster queries",
            OverlayKind::Policy => "live policy report findings; a blocked apply is not here",
            OverlayKind::Netpol => "NetworkPolicy isolation, declared not observed",
            OverlayKind::MeshDeclared => "can reach, per policy",
            OverlayKind::MeshObserved => "did reach, per telemetry",
        }
    }

    pub fn legend_title(self) -> &'static str {
        self.badge()
    }

    pub fn legend_aria(self) -> &'static str {
        match self {
            OverlayKind::Sync => "GitOps sync legend",
            OverlayKind::Metrics => "Metrics legend",
            OverlayKind::Policy => "Policy legend",
            OverlayKind::Netpol => "NetworkPolicy isolation legend",
            OverlayKind::MeshDeclared => "Declared mesh legend",
            OverlayKind::MeshObserved => "Observed mesh legend",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            OverlayKind::Sync => "sync",
            OverlayKind::Metrics => "metrics",
            OverlayKind::Policy => "policy",
            OverlayKind::Netpol => "netpol",
            OverlayKind::MeshDeclared => "mesh-declared",
            OverlayKind::MeshObserved => "mesh-observed",
        }
    }

    pub fn parse(text: &str) -> Option<OverlayKind> {
        match text.trim().to_ascii_lowercase().as_str() {
            "sync" => Some(OverlayKind::Sync),
            "metrics" => Some(OverlayKind::Metrics),
            "policy" => Some(OverlayKind::Policy),
            "netpol" | "networkpolicy" | "network-policy" => Some(OverlayKind::Netpol),
            "mesh-declared" | "mesh_declared" | "declared" => Some(OverlayKind::MeshDeclared),
            "mesh-observed" | "mesh_observed" | "observed" => Some(OverlayKind::MeshObserved),
            _ => None,
        }
    }

    pub fn next(self) -> Option<OverlayKind> {
        match self {
            OverlayKind::Sync => Some(OverlayKind::Metrics),
            OverlayKind::Metrics => Some(OverlayKind::Policy),
            OverlayKind::Policy => Some(OverlayKind::Netpol),
            OverlayKind::Netpol => Some(OverlayKind::MeshDeclared),
            OverlayKind::MeshDeclared => Some(OverlayKind::MeshObserved),
            OverlayKind::MeshObserved => None,
        }
    }
}
