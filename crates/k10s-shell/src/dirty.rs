//! Unsaved work, and the three moments that can throw it away.
//!
//! Overwriting a file that changed underneath, reloading over unsaved work, and
//! closing a buffer that has not been written. Each arms on the first press and
//! fires on the second, and any edit disarms -- typing means the user is not
//! answering the question any more. Each arms under its *own* name, because one
//! shared bit could not tell them apart and a warning about a reload was once
//! answered by the next close: the buffer went away on a single press.
//!
//! Pure, so all of it is tested without a window. A fourth destructive moment --
//! the apply the diff view confirms -- is keyed by `BufferStamp` instead, because
//! it has to survive the buffer being replaced.

use crate::fs::Stamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveStep {
    Write,
    Confirm(Overwrite),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overwrite {
    // Read, then changed underneath us.
    ChangedOnDisk,
    // Never read by this buffer, but something is already there.
    AlreadyExists,
}

impl Overwrite {
    pub(crate) fn note(self) -> &'static str {
        match self {
            Overwrite::ChangedOnDisk => "the file changed on disk; ctrl-s again to overwrite",
            Overwrite::AlreadyExists => "that file already exists; ctrl-s again to overwrite",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseStep {
    Close,
    Warn,
}

// Which destructive action is armed. One shared bit could not tell them apart,
// so a warning about a reload was answered by the next close: the buffer went
// away on a single press. Each action arms its own name, and a press that
// answers a different question re-asks rather than firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Armed {
    Close,
    Reload,
    Overwrite,
}

// Unsaved work, and the three destructive moments that need a second press:
// overwriting a file that changed underneath, reloading over unsaved work, and
// closing a buffer that has not been written. Each arms on the first press and
// fires on the second, and any edit disarms -- typing means the user is not
// answering the question any more. Pure, so the rules are tested without a
// window.
#[derive(Debug, Default)]
pub(crate) struct DirtyState {
    clean_version: Option<u64>,
    disk_stamp: Option<Stamp>,
    armed: Option<Armed>,
}

impl DirtyState {
    pub(crate) fn is_dirty(&self, version: u64) -> bool {
        self.clean_version != Some(version)
    }

    /// Whether this buffer has ever had a clean point at all. A buffer whose
    /// load never landed has none, and is dirty by definition -- there is
    /// nothing in it to lose, and a close that warns about it is warning about
    /// an empty document.
    pub(crate) fn never_loaded(&self) -> bool {
        self.clean_version.is_none()
    }

    pub(crate) fn mark_clean(&mut self, version: u64, stamp: Option<Stamp>) {
        self.clean_version = Some(version);
        if stamp.is_some() {
            self.disk_stamp = stamp;
        }
        self.armed = None;
    }

    pub(crate) fn forget_disk(&mut self) {
        self.disk_stamp = None;
        if self.armed == Some(Armed::Overwrite) {
            self.armed = None;
        }
    }

    pub(crate) fn edited(&mut self) {
        self.armed = None;
    }

    // `on_disk` is the stamp read right now, or None when the path cannot be
    // stamped at all -- deleted out from under us, or never written. Neither
    // is a conflict: writing recreates exactly what the buffer holds.
    pub(crate) fn save_step(&mut self, on_disk: Option<Stamp>) -> SaveStep {
        if self.armed == Some(Armed::Overwrite) {
            self.armed = None;
            return SaveStep::Write;
        }
        let overwrite = match (self.disk_stamp, on_disk) {
            (Some(known), Some(now)) if known != now => Some(Overwrite::ChangedOnDisk),
            (None, Some(_)) => Some(Overwrite::AlreadyExists),
            _ => None,
        };
        match overwrite {
            Some(reason) => {
                self.armed = Some(Armed::Overwrite);
                SaveStep::Confirm(reason)
            }
            None => SaveStep::Write,
        }
    }

    // Reloading throws away unsaved work exactly like closing does, so it asks
    // the same way -- under its own name.
    pub(crate) fn reload_step(&mut self, version: u64) -> CloseStep {
        self.destructive_step(version, Armed::Reload)
    }

    pub(crate) fn close_step(&mut self, version: u64) -> CloseStep {
        self.destructive_step(version, Armed::Close)
    }

    fn destructive_step(&mut self, version: u64, action: Armed) -> CloseStep {
        if !self.is_dirty(version) {
            return CloseStep::Close;
        }
        if self.armed == Some(action) {
            self.armed = None;
            return CloseStep::Close;
        }
        self.armed = Some(action);
        CloseStep::Warn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_buffer_is_clean_until_it_is_edited_again() {
        let mut dirty = DirtyState::default();
        assert!(dirty.is_dirty(0), "a buffer with no clean point is dirty");
        dirty.mark_clean(7, Some(100));
        assert!(!dirty.is_dirty(7));
        assert!(dirty.is_dirty(8), "one more edit and it is dirty again");
        // Cleanliness here is a version identity, not a comparison of bytes,
        // and `Buffer::restore` counts an undo as a revision of its own. So a
        // buffer undone back to the saved *text* arrives at a version the clean
        // point never saw and keeps its dot: the tab over-reports rather than
        // under-reports, which is the direction that cannot lose work.
        assert!(
            dirty.is_dirty(9),
            "an undo is a new version, not a return to an old one"
        );
        assert!(
            !dirty.is_dirty(7),
            "only the clean version itself reads clean"
        );
    }

    #[test]
    fn a_buffer_that_never_loaded_says_so_and_stops_saying_it_once_it_has() {
        let mut dirty = DirtyState::default();
        assert!(
            dirty.never_loaded(),
            "an editor whose read was denied has no clean point and nothing to lose"
        );
        dirty.mark_clean(0, None);
        assert!(
            !dirty.never_loaded(),
            "a buffer with a clean point owns its own answer about unsaved work"
        );
        dirty.edited();
        assert!(
            !dirty.never_loaded(),
            "editing after a load is exactly the work a close must warn about"
        );
    }

    #[test]
    fn an_external_change_costs_a_second_press_before_it_is_overwritten() {
        let mut dirty = DirtyState::default();
        dirty.mark_clean(1, Some(100));
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk),
            "the disk moved under us"
        );
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Write,
            "the second press is the confirmation"
        );
        dirty.mark_clean(2, Some(200));
        assert_eq!(dirty.save_step(Some(200)), SaveStep::Write, "in step again");
    }

    #[test]
    fn typing_disarms_a_pending_overwrite_or_close_confirmation() {
        let mut dirty = DirtyState::default();
        dirty.mark_clean(1, Some(100));
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk)
        );
        dirty.edited();
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk),
            "an edit after the warning re-asks rather than silently overwriting"
        );

        let mut dirty = DirtyState::default();
        assert_eq!(dirty.close_step(5), CloseStep::Warn);
        dirty.edited();
        assert_eq!(
            dirty.close_step(5),
            CloseStep::Warn,
            "typing after the warning re-arms the guard"
        );
        assert_eq!(dirty.close_step(5), CloseStep::Close);
    }

    #[test]
    fn a_never_written_or_deleted_file_saves_without_a_prompt() {
        let mut dirty = DirtyState::default();
        assert_eq!(
            dirty.save_step(None),
            SaveStep::Write,
            "a new file has nothing to conflict with"
        );
        dirty.mark_clean(1, Some(100));
        assert_eq!(
            dirty.save_step(None),
            SaveStep::Write,
            "a file deleted underneath is recreated, not queried"
        );
    }

    #[test]
    fn a_clean_buffer_closes_on_the_first_press() {
        let mut dirty = DirtyState::default();
        dirty.mark_clean(3, Some(1));
        assert_eq!(dirty.close_step(3), CloseStep::Close);
    }

    #[test]
    fn adopting_a_path_forgets_the_previous_files_stamp() {
        let mut dirty = DirtyState::default();
        dirty.mark_clean(1, Some(100));
        dirty.forget_disk();
        assert_eq!(
            dirty.save_step(None),
            SaveStep::Write,
            "save-as onto a new file must not inherit the old file's conflict"
        );
    }

    #[test]
    fn saving_onto_a_file_this_buffer_never_read_asks_first() {
        let mut dirty = DirtyState::default();
        assert_eq!(
            dirty.save_step(Some(42)),
            SaveStep::Confirm(Overwrite::AlreadyExists),
            "save-as onto somebody else's file must not be silent"
        );
        assert_eq!(dirty.save_step(Some(42)), SaveStep::Write);
    }

    #[test]
    fn reloading_over_unsaved_work_asks_the_same_way_closing_does() {
        let mut dirty = DirtyState::default();
        assert_eq!(dirty.reload_step(3), CloseStep::Warn);
        assert_eq!(dirty.reload_step(3), CloseStep::Close);
        dirty.mark_clean(4, Some(1));
        assert_eq!(
            dirty.reload_step(4),
            CloseStep::Close,
            "a clean buffer reloads without a question"
        );
    }

    #[test]
    fn a_buffer_that_has_never_been_loaded_is_dirty_which_is_why_opening_cannot_ask() {
        // A fresh view has no clean point, so it is dirty by definition. The
        // constructors therefore have to load unguarded: routing them through
        // the reload action left every file open, empty, behind a warning about
        // unsaved changes it did not have.
        let mut dirty = DirtyState::default();
        assert!(dirty.is_dirty(0), "no clean point yet");
        assert_eq!(
            dirty.reload_step(0),
            CloseStep::Warn,
            "which is exactly what the guard would have answered on open"
        );
    }

    #[test]
    fn reloading_and_closing_do_not_answer_each_others_questions() {
        // One shared bit could not tell them apart, so the warning about a
        // reload was answered by the next close and the buffer went away on a
        // single press.
        let mut dirty = DirtyState::default();
        assert_eq!(dirty.reload_step(3), CloseStep::Warn);
        assert_eq!(
            dirty.close_step(3),
            CloseStep::Warn,
            "a close does not inherit the reload's armed confirmation"
        );
        assert_eq!(dirty.close_step(3), CloseStep::Close);

        let mut dirty = DirtyState::default();
        dirty.mark_clean(1, Some(100));
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk)
        );
        assert_eq!(
            dirty.close_step(2),
            CloseStep::Warn,
            "and neither does a close inherit an armed overwrite"
        );
        assert_eq!(
            dirty.save_step(Some(200)),
            SaveStep::Confirm(Overwrite::ChangedOnDisk),
            "the close re-armed, so the overwrite asks again"
        );
    }
}
