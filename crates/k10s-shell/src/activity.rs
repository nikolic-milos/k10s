//! The left activity rail: which surfaces exist, and which icon is lit.
//!
//! The rail is a table of slots, not a pile of buttons in `render`. Adding a
//! panel is one row; the dock still holds the panel. Brand glyphs on the map
//! (`tools/*.svg`) stay on the map. Chrome icons come from the Zed set the
//! window already paints.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityId {
    Starmap,
    Resources,
    Nodes,
    Find,
    Releases,
    Forwards,
    Terminal,
    Inspector,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityGroup {
    Primary,
    Trailing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Activity {
    pub id: ActivityId,
    pub icon: &'static str,
    pub label: &'static str,
    pub element_id: &'static str,
    pub group: ActivityGroup,
}

pub(crate) const ACTIVITIES: &[Activity] = &[
    Activity {
        id: ActivityId::Starmap,
        icon: "icons/star.svg",
        label: "Starmap",
        element_id: "activity-starmap",
        group: ActivityGroup::Primary,
    },
    Activity {
        id: ActivityId::Resources,
        icon: "icons/file_tree.svg",
        label: "Resources",
        element_id: "activity-resources",
        group: ActivityGroup::Primary,
    },
    Activity {
        id: ActivityId::Nodes,
        icon: "icons/server.svg",
        label: "Nodes",
        element_id: "activity-nodes",
        group: ActivityGroup::Primary,
    },
    Activity {
        id: ActivityId::Find,
        icon: "icons/magnifying_glass.svg",
        label: "Find in cluster",
        element_id: "activity-find",
        group: ActivityGroup::Primary,
    },
    Activity {
        id: ActivityId::Releases,
        icon: "icons/box.svg",
        label: "Helm releases",
        element_id: "activity-releases",
        group: ActivityGroup::Primary,
    },
    Activity {
        id: ActivityId::Forwards,
        icon: "icons/forward_arrow.svg",
        label: "Port forwards",
        element_id: "activity-forwards",
        group: ActivityGroup::Primary,
    },
    Activity {
        id: ActivityId::Terminal,
        icon: "icons/terminal_alt.svg",
        label: "Terminal",
        element_id: "activity-terminal",
        group: ActivityGroup::Trailing,
    },
    Activity {
        id: ActivityId::Inspector,
        icon: "icons/info.svg",
        label: "Inspector",
        element_id: "activity-inspector",
        group: ActivityGroup::Trailing,
    },
    Activity {
        id: ActivityId::Settings,
        icon: "icons/settings.svg",
        label: "Settings",
        element_id: "activity-settings",
        group: ActivityGroup::Trailing,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeftSlot {
    Resources,
    Nodes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BottomSlot {
    Terminal,
    Forwards,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RailState {
    pub map_active: bool,
    pub left: Option<LeftSlot>,
    pub bottom: Option<BottomSlot>,
    pub inspector_open: bool,
}

pub(crate) fn lit(id: ActivityId, state: RailState) -> bool {
    match id {
        ActivityId::Starmap => state.map_active,
        ActivityId::Resources => state.left == Some(LeftSlot::Resources),
        ActivityId::Nodes => state.left == Some(LeftSlot::Nodes),
        ActivityId::Find => false,
        ActivityId::Releases => false,
        ActivityId::Forwards => state.bottom == Some(BottomSlot::Forwards),
        ActivityId::Terminal => state.bottom == Some(BottomSlot::Terminal),
        ActivityId::Inspector => state.inspector_open,
        ActivityId::Settings => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_slot_has_a_distinct_id_and_icon() {
        let mut ids = Vec::new();
        let mut icons = Vec::new();
        let mut elements = Vec::new();
        for slot in ACTIVITIES {
            assert!(
                slot.icon.starts_with("icons/") && slot.icon.ends_with(".svg"),
                "chrome icons are the Zed set: {}",
                slot.icon
            );
            assert!(!ids.contains(&slot.id), "duplicate activity {:?}", slot.id);
            assert!(!icons.contains(&slot.icon), "duplicate icon {}", slot.icon);
            assert!(
                !elements.contains(&slot.element_id),
                "duplicate element id {}",
                slot.element_id
            );
            ids.push(slot.id);
            icons.push(slot.icon);
            elements.push(slot.element_id);
        }
        assert_eq!(ids.len(), 9);
    }

    #[test]
    fn only_the_open_surface_is_lit() {
        let idle = RailState {
            map_active: true,
            ..RailState::default()
        };
        assert!(lit(ActivityId::Starmap, idle));
        assert!(!lit(ActivityId::Resources, idle));
        assert!(!lit(ActivityId::Find, idle));

        let browse = RailState {
            map_active: true,
            left: Some(LeftSlot::Resources),
            ..RailState::default()
        };
        assert!(lit(ActivityId::Resources, browse));
        assert!(!lit(ActivityId::Nodes, browse));

        let term = RailState {
            map_active: false,
            bottom: Some(BottomSlot::Terminal),
            inspector_open: true,
            ..RailState::default()
        };
        assert!(lit(ActivityId::Terminal, term));
        assert!(!lit(ActivityId::Forwards, term));
        assert!(lit(ActivityId::Inspector, term));
        assert!(!lit(ActivityId::Starmap, term));
    }
}
