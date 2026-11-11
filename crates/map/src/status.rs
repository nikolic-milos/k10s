use gpui::SharedString;
use k10s_core::{BUILTIN_REASONS, ReasonId, Severity};

pub(crate) fn label(severity: Severity) -> &'static str {
    match severity {
        Severity::Ok => "Healthy",
        Severity::Warn => "Warning",
        Severity::Err => "Critical",
        Severity::Unknown => "Unknown",
    }
}

pub(crate) fn reason_label(reason: ReasonId) -> &'static str {
    BUILTIN_REASONS
        .get(reason.0 as usize)
        .map_or("Status unavailable", |info| info.display)
}

pub(crate) fn workload_label(children: usize, severity: Severity) -> &'static str {
    if children == 0 {
        "0 observed pods"
    } else {
        label(severity)
    }
}

pub(crate) fn workload_severity(children: usize, severity: Severity) -> Severity {
    if children == 0 {
        Severity::Unknown
    } else {
        severity
    }
}

pub(crate) fn icon(severity: Severity) -> (SharedString, &'static [u8]) {
    let (key, data): (&str, &[u8]) = match severity {
        Severity::Ok => (
            "map/status/ok.svg",
            br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="m7 12 3 3 7-7"/></svg>"#,
        ),
        Severity::Warn => (
            "map/status/warn.svg",
            br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 7v6m0 4h.01"/></svg>"#,
        ),
        Severity::Err => (
            "map/status/error.svg",
            br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="m8 8 8 8m0-8-8 8"/></svg>"#,
        ),
        Severity::Unknown => (
            "map/status/unknown.svg",
            br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M9 9a3 3 0 0 1 6 0c0 2-3 2-3 4m0 4h.01"/></svg>"#,
        ),
    };
    (SharedString::new_static(key), data)
}
