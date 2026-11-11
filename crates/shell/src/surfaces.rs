//! The surface menu projects existing navigation and cached source presence.
//!
//! Opening it performs no discovery. Unknown and blocked sources stay named;
//! only a source already known to be absent is omitted. Navigation and the
//! searchable picker consume the same static entries and registered actions.

use k10s_provider::ToolPresence;

use crate::activity::{ACTIVITIES, Activity, ActivityId, RailState, lit};
use crate::workspace::ObservabilityNavigation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Surface {
    pub(crate) activity: Activity,
    pub(crate) notice: Option<&'static str>,
}

pub(crate) fn surface(activity: Activity, navigation: &ObservabilityNavigation) -> Option<Surface> {
    let presence = match activity.id {
        ActivityId::Observe => Some(navigation.observe),
        ActivityId::Traces => Some(navigation.traces),
        ActivityId::Collectors => Some(navigation.collectors),
        _ => None,
    };
    let notice = match presence {
        Some(Some(ToolPresence::Missing)) => return None,
        Some(Some(ToolPresence::Blocked)) => Some("Unavailable"),
        Some(None) => Some("Not checked"),
        Some(Some(ToolPresence::Ready)) | None => None,
    };
    Some(Surface { activity, notice })
}

pub(crate) fn entries(navigation: &ObservabilityNavigation) -> Vec<Surface> {
    ACTIVITIES
        .iter()
        .filter_map(|activity| surface(*activity, navigation))
        .collect()
}

pub(crate) fn initial_selection(entries: &[Surface], state: RailState) -> usize {
    entries
        .iter()
        .position(|entry| lit(entry.activity.id, state))
        .unwrap_or(0)
}

pub(crate) fn search_terms(id: ActivityId) -> &'static str {
    match id {
        ActivityId::Observe => "Prometheus Grafana Loki metrics charts logs",
        ActivityId::Traces => "Tempo Jaeger traces spans",
        ActivityId::Collectors => "OpenTelemetry OTEL Collectors pipelines",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_selection_follows_the_focused_surface_and_handles_known_absence() {
        use crate::activity::{BottomSlot, CenterSlot, LeftSlot};
        let menu = entries(&ObservabilityNavigation::default());
        for (state, id) in [
            (
                RailState {
                    map_active: true,
                    ..Default::default()
                },
                ActivityId::Starmap,
            ),
            (
                RailState {
                    center: Some(CenterSlot::Observe),
                    ..Default::default()
                },
                ActivityId::Observe,
            ),
            (
                RailState {
                    center: Some(CenterSlot::Traces),
                    ..Default::default()
                },
                ActivityId::Traces,
            ),
            (
                RailState {
                    left: Some(LeftSlot::Nodes),
                    ..Default::default()
                },
                ActivityId::Nodes,
            ),
            (
                RailState {
                    bottom: Some(BottomSlot::Forwards),
                    ..Default::default()
                },
                ActivityId::Forwards,
            ),
        ] {
            assert_eq!(menu[initial_selection(&menu, state)].activity.id, id);
        }
        let absent = entries(&ObservabilityNavigation {
            traces: Some(ToolPresence::Missing),
            ..Default::default()
        });
        assert_eq!(
            initial_selection(
                &absent,
                RailState {
                    center: Some(CenterSlot::Traces),
                    ..Default::default()
                }
            ),
            0
        );
        assert_eq!(initial_selection(&[], RailState::default()), 0);
    }

    fn entry(id: ActivityId, navigation: &ObservabilityNavigation) -> Option<Surface> {
        entries(navigation)
            .into_iter()
            .find(|entry| entry.activity.id == id)
    }

    #[test]
    fn unknown_sources_are_named_without_claiming_readiness() {
        let navigation = ObservabilityNavigation::default();
        for id in [
            ActivityId::Observe,
            ActivityId::Traces,
            ActivityId::Collectors,
        ] {
            assert_eq!(entry(id, &navigation).unwrap().notice, Some("Not checked"));
        }
        assert_eq!(
            entry(ActivityId::Resources, &navigation).unwrap().notice,
            None
        );
    }

    #[test]
    fn missing_sources_disappear_and_blocked_sources_remain_labelled() {
        let navigation = ObservabilityNavigation {
            observe: Some(ToolPresence::Missing),
            traces: Some(ToolPresence::Blocked),
            collectors: Some(ToolPresence::Ready),
        };
        assert!(entry(ActivityId::Observe, &navigation).is_none());
        assert_eq!(
            entry(ActivityId::Traces, &navigation).unwrap().notice,
            Some("Unavailable")
        );
        assert_eq!(
            entry(ActivityId::Collectors, &navigation).unwrap().notice,
            None
        );
        assert!(entry(ActivityId::Starmap, &navigation).is_some());
    }

    #[test]
    fn context_reset_restores_unknown_source_entries() {
        let navigation = ObservabilityNavigation {
            observe: Some(ToolPresence::Missing),
            traces: Some(ToolPresence::Missing),
            collectors: Some(ToolPresence::Missing),
        };
        assert!(entry(ActivityId::Traces, &navigation).is_none());
        assert_eq!(
            entry(ActivityId::Traces, &ObservabilityNavigation::default())
                .unwrap()
                .notice,
            Some("Not checked")
        );
    }
}
