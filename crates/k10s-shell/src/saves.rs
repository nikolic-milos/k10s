//! One buffer's saves, strictly ordered.
//!
//! Spawning a write per press left the order to the executor: an older write
//! could rename last while the newer buffer was already marked clean, so the
//! file on disk was not what the editor said was saved. One write in flight,
//! one queued behind it, and the queued one is always the newest text.
//!
//! Pure, so the ordering is tested without a window.

use std::path::PathBuf;

// One buffer's saves, strictly ordered. Spawning a write per press left the
// order to the executor: an older write could rename last while the newer
// buffer was already marked clean, so the file on disk was not what the editor
// said was saved. One write in flight, one queued behind it, and the queued one
// is always the newest text. Pure, so the ordering is tested without a window.
#[derive(Debug, Default)]
pub(crate) struct SaveQueue {
    // Which write owns the flight, so only that write can hand it on. A save
    // the queue has abandoned is still running, and letting it advance the
    // queue would start a second write beside one already in progress -- the
    // very race the queue exists to prevent.
    flight: Option<u64>,
    pending: Option<PendingSave>,
    issued: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSave {
    pub(crate) path: PathBuf,
    pub(crate) text: String,
    pub(crate) version: u64,
    // Which buffer this text came from. A reload or a save-as replaces the
    // buffer, and its versions restart, so a completion that arrives afterwards
    // describes a document that no longer exists: marking that version clean
    // hands the tab a false answer, and adopting its stamp invents a conflict.
    pub(crate) generation: u64,
    // Which flight this write is, so the queue can tell the write it is waiting
    // for from one it has already let go of.
    pub(crate) ticket: u64,
}

impl SaveQueue {
    // What to start now: the request itself when nothing is running, or
    // nothing, with the request held as the one that follows.
    pub(crate) fn request(&mut self, mut save: PendingSave) -> Option<PendingSave> {
        if self.flight.is_some() {
            // Only the newest text is worth writing; the presses in between
            // asked for versions this one supersedes.
            self.pending = Some(save);
            return None;
        }
        save.ticket = self.take_off();
        Some(save)
    }

    // A write finished, so the queued one may start -- but only if this is the
    // write the queue is actually waiting for.
    pub(crate) fn finished(&mut self, ticket: u64) -> Option<PendingSave> {
        if self.flight != Some(ticket) {
            return None;
        }
        match self.pending.take() {
            Some(mut next) => {
                next.ticket = self.take_off();
                Some(next)
            }
            None => {
                self.flight = None;
                None
            }
        }
    }

    // The queue lets go: a conflict needs answering first, or the work was
    // discarded. Whatever is still running keeps running -- it cannot be
    // recalled -- but it no longer speaks for the queue.
    pub(crate) fn abandon(&mut self) {
        self.flight = None;
        self.pending = None;
    }

    fn take_off(&mut self) -> u64 {
        self.issued += 1;
        self.flight = Some(self.issued);
        self.issued
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(version: u64, text: &str) -> PendingSave {
        PendingSave {
            path: PathBuf::from("/work/a.yaml"),
            text: text.to_string(),
            version,
            generation: 1,
            ticket: 0,
        }
    }

    fn same_save(left: &PendingSave, right: &PendingSave) -> bool {
        (&left.path, &left.text, left.version) == (&right.path, &right.text, right.version)
    }

    #[test]
    fn an_abandoned_write_cannot_hand_the_queue_to_a_second_one() {
        // The write a reload abandons is still running: it cannot be recalled.
        // If it were still allowed to advance the queue when it finishes, it
        // would start a save beside the one already in flight -- two writes
        // racing, which is what the queue exists to prevent.
        let mut queue = SaveQueue::default();
        let first = queue.request(save(1, "one")).expect("starts");
        queue.abandon();
        let second = queue
            .request(save(2, "two"))
            .expect("starts, nothing owns the flight");
        assert_ne!(first.ticket, second.ticket);
        assert!(
            queue.finished(first.ticket).is_none(),
            "the abandoned write hands on nothing"
        );
        assert!(
            queue.request(save(3, "three")).is_none(),
            "and the flight is still owned, so a third press queues rather than racing"
        );
        let next = queue
            .finished(second.ticket)
            .expect("the owner hands on the queued text");
        assert!(same_save(&next, &save(3, "three")));
    }

    #[test]
    fn saves_of_one_buffer_run_one_at_a_time_and_the_last_text_wins() {
        // Spawning a write per press left the order to the executor: an older
        // write could rename last while the newest version was marked clean.
        let mut queue = SaveQueue::default();
        let first = queue
            .request(save(1, "one"))
            .expect("the first press starts");
        assert!(same_save(&first, &save(1, "one")));
        assert!(
            queue.request(save(2, "two")).is_none(),
            "the second waits for it rather than racing it"
        );
        assert!(
            queue.request(save(3, "three")).is_none(),
            "and so does the third"
        );
        let next = queue
            .finished(first.ticket)
            .expect("the queued text follows");
        assert!(
            same_save(&next, &save(3, "three")),
            "only the newest text is worth writing"
        );
        assert!(
            queue.finished(next.ticket).is_none(),
            "then the queue is empty"
        );
        let later = queue
            .request(save(4, "four"))
            .expect("a later press starts");
        assert!(same_save(&later, &save(4, "four")));
    }

    #[test]
    fn a_conflict_empties_the_queue_so_the_overwrite_is_a_deliberate_press() {
        let mut queue = SaveQueue::default();
        let first = queue.request(save(1, "one")).expect("starts");
        queue.request(save(2, "two"));
        queue.abandon();
        assert!(
            queue.finished(first.ticket).is_none(),
            "nothing follows a save that asked a question instead of writing"
        );
        let resumed = queue
            .request(save(3, "three"))
            .expect("a deliberate press starts");
        assert!(same_save(&resumed, &save(3, "three")));
    }
}
