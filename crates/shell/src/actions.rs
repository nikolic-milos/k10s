//! Every command the shell defines, and every context one can be scoped to.
//!
//! The names here are load-bearing beyond Rust. `actions!` stamps each struct
//! with the `k10s_shell` namespace, so an action is known to the keymap file,
//! the command palette and the settings schema as `"k10s_shell::OpenPalette"` --
//! a string the shared filter in `palette` admits to both the palette and the
//! keymap schema, and the keymap loader hands to `App::build_action`. A rename
//! needs an alias for the old name or it silently unbinds a person's keymap.
//! Which keys reach these is [`crate::bindings`]; which of them a
//! given surface listens for is the element that declares the context.

use gpui::actions;

actions!(
    k10s_shell,
    [
        ToggleInspector,
        ClearSelection,
        OpenPalette,
        ChooseCluster,
        OpenBrowser,
        OpenNodes,
        ShowStarmap,
        OpenForwards,
        OpenReleases,
        OpenArgo,
        OpenFlux,
        OpenDay2,
        OpenObserve,
        OpenPolicy,
        OpenHarbor,
        OpenMesh,
        OpenTraces,
        OpenEcosystem,
        SilenceSelection,
        NextFamily,
        PrevFamily,
        RevealHelm,
        HelmDiff,
        HelmRollback,
        ToggleTerminal,
        Quit,
        DescribeSelection,
        LogsSelection,
        ExecSelection,
        AttachSelection,
        NextItem,
        PrevItem,
        CloseItem,
        ToggleLeftDock,
        ToggleRightDock,
        ToggleBottomDock,
        RowUp,
        RowDown,
        RowPageUp,
        RowPageDown,
        RowHome,
        RowEnd,
        OpenRow,
        LogsRow,
        ExecRow,
        TalosDmesg,
        TalosServices,
        Refresh,
        LoadMore,
        StartForward,
        StopForward,
        EnterFilter,
        Back,
        DocScrollUp,
        DocScrollDown,
        DocPageUp,
        DocPageDown,
        DocHome,
        DocEnd,
        EnterSearch,
        NextMatch,
        PrevMatch,
        ToggleFollow,
        ToggleTimestamps,
        CycleContainer,
        TogglePrevious,
        Reload,
        CancelDoc,
        CommitInput,
        CancelInput,
        DeleteInputChar,
        EditSelection,
        EditRow,
        EditorUp,
        EditorDown,
        EditorLeft,
        EditorRight,
        EditorWordLeft,
        EditorWordRight,
        EditorHome,
        EditorEnd,
        EditorPageUp,
        EditorPageDown,
        EditorDocStart,
        EditorDocEnd,
        EditorSelectUp,
        EditorSelectDown,
        EditorSelectLeft,
        EditorSelectRight,
        EditorSelectWordLeft,
        EditorSelectWordRight,
        EditorSelectHome,
        EditorSelectEnd,
        EditorSelectAll,
        EditorBackspace,
        EditorDelete,
        EditorNewline,
        EditorTab,
        EditorShiftTab,
        EditorUndo,
        EditorRedo,
        EditorDeleteLine,
        EditorToggleComment,
        EditorCursorAbove,
        EditorCursorBelow,
        EditorSelectNext,
        EditorComplete,
        EditorFind,
        EditorReplace,
        EditorReplaceAll,
        EditorToggleRegex,
        EditorCancel,
        EditorSave,
        EditorSaveAs,
        OpenFile,
        OpenFolder,
        NewFile,
        FindFile,
        FindCluster,
        LoadSavedView,
        OpenSettings,
        OpenKeymap,
        PickParent,
        // Compares the buffer with the copy of the object this editor read,
        // not with the cluster, so it is not named "live"; the old name stays
        // an alias so a keymap that bound it still works.
        #[action(deprecated_aliases = ["k10s_shell::DiffAgainstLive"])]
        DiffAgainstLoadedCopy,
        ApplyDryRun,
        ApplyToCluster,
        ForceApply,
        NextChange,
        PrevChange,
        ToggleFolded,
        // Takes a hunk as the copy this editor read has it, for the same reason
        // as the comparison's own name; the old name stays an alias.
        #[action(deprecated_aliases = ["k10s_shell::KeepTheirs"])]
        TakeFromLoadedCopy,
    ]
);

/// Every key context the shell sets. One list: the binding-scope invariant test
/// reads it, the keymap file's schema offers it as an enum, and the template a
/// person is handed names it, so a new context cannot be added to one without
/// the others. A slice rather than an array, because a hand-written length is a
/// second thing to keep in step for no benefit.
pub const KEY_CONTEXTS: &[&str] = &[
    "Workspace",
    "Browse",
    "Forwards",
    "Releases",
    "Ecosystem",
    "Doc",
    "Diff",
    "Editor",
    "Typing",
    "Terminal",
    "Palette",
    "Map",
];

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Action as _;

    /// The namespace comes from the first argument to `actions!`, not from the
    /// module the invocation sits in -- which is what makes moving this list
    /// between modules safe, and is worth an executable claim rather than an
    /// argument. The palette's shared filter matches the `"k10s_shell::"`
    /// prefix, and the keymap loader hands names to `App::build_action`, so a
    /// namespace that quietly became `"actions::"` would unbind every user
    /// keymap and empty the command palette with nothing failing to compile.
    #[test]
    fn an_action_is_named_for_the_crate_and_not_for_this_module() {
        assert_eq!(OpenPalette.name(), "k10s_shell::OpenPalette");
        assert_eq!(EditorSave.name(), "k10s_shell::EditorSave");
        assert_eq!(RowDown.name(), "k10s_shell::RowDown");
        assert_eq!(
            ToggleInspector::name_for_type(),
            "k10s_shell::ToggleInspector"
        );
        for name in [
            OpenPalette.name(),
            ChooseCluster.name(),
            TakeFromLoadedCopy.name(),
            PickParent.name(),
        ] {
            assert!(
                name.starts_with("k10s_shell::"),
                "{name} would not survive the prefix filter the palette and the \
                 settings schema select on"
            );
        }
    }
}
