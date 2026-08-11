//! Every precondition a press has to clear, in one place.
//!
//! Applying is a second deliberate press and forcing is a third: the gate arms
//! under the *name* of the thing being asked, so a press meant for one question
//! never answers the other, and any recompute disarms because what was being
//! confirmed is no longer what is on screen.
//!
//! [`refuse`] is one pure function and none of these checks is anywhere else.
//! That is not tidiness. The two preconditions that ever failed were the two
//! that lived somewhere else: the force check sat inside a key handler, so it
//! guarded one way in while the palette went round it. A precondition reachable
//! by one route is a precondition, and by two routes is a suggestion.
//!
//! Nothing here draws or talks to a cluster, so all of it is tested without a
//! window -- which is the point of it being its own module rather than the top
//! of [`crate::diff`].

use k10s_edit::diff::{self, Origin, Verdict};

use crate::diff::Mode;
use crate::editor::BufferStamp;

/// Which destructive question is armed. One bit could not tell an apply from a
/// force, and a press meant for one would answer the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Armed {
    Apply,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Ask,
    Go,
}

/// Which request owns the wire. A bare flag was not enough: the clear lived
/// after the guard that discards a reply belonging to a superseded comparison,
/// so a recompute during an apply stranded the flag and every later apply was
/// refused as "already in flight". A ticket separates the two questions -- who
/// holds the wire, and whose answer is still wanted -- and only the first one
/// decides when it is released.
#[derive(Debug, Default)]
pub(crate) struct Flight {
    holder: Option<u64>,
    // What the holder is. A dry run and an apply travel the same seam and only
    // one of them writes, so a message that calls a dry run "an apply" tells
    // the user a write is happening that is not.
    holds_apply: bool,
    // A comparison asked for while the wire was held. A dry run changes
    // nothing, so the honest answer to "compare this again" is to do it when
    // the wire frees rather than to drop the question and leave the tab under a
    // message about a request that has since finished. An apply is never
    // queued: a write has to be the press the user just made.
    owed: bool,
    issued: u64,
}

impl Flight {
    /// A ticket, or None when a request is already out.
    pub(crate) fn take(&mut self, dry_run: bool) -> Option<u64> {
        if self.holder.is_some() {
            return None;
        }
        self.issued += 1;
        self.holder = Some(self.issued);
        self.holds_apply = !dry_run;
        self.holder
    }

    /// Remember that a comparison was asked for while the wire was busy.
    pub(crate) fn owe_a_comparison(&mut self) {
        self.owed = true;
    }

    /// Release, but only by the request that holds it: a reply from a request
    /// that was already superseded must not hand the wire to nobody. True when
    /// a comparison was asked for meanwhile and is now owed.
    pub(crate) fn release(&mut self, ticket: u64) -> bool {
        if self.holder != Some(ticket) {
            return false;
        }
        self.holder = None;
        self.holds_apply = false;
        std::mem::take(&mut self.owed)
    }

    pub(crate) fn busy(&self) -> bool {
        self.holder.is_some()
    }

    /// What is on the wire, named the way a person would name it.
    pub(crate) fn holder(&self) -> &'static str {
        if self.holds_apply {
            return "an apply";
        }
        "a dry run"
    }
}

/// Everything a press has to be true about before a write leaves this view,
/// gathered so that [`refuse`] can be a pure function over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ready<'a> {
    /// Discovery says the server takes a patch for this kind at all.
    pub(crate) patchable: bool,
    /// Why the pruner would not build a payload, if it would not.
    pub(crate) blocked: &'a [&'static str],
    /// What the comparison on screen concluded.
    pub(crate) verdict: Verdict,
    /// The buffer this review was made of.
    pub(crate) reviewed: BufferStamp,
    /// The editor's buffer now, or None when the editor is gone.
    pub(crate) editor: Option<BufferStamp>,
    /// The buffer the server has answered a dry run for.
    pub(crate) dry_run: Option<BufferStamp>,
    /// How many fields a conflict named, which is the only thing a force may
    /// take.
    pub(crate) conflicts: usize,
    /// What holds the wire, if anything.
    pub(crate) in_flight: Option<&'static str>,
}

/// Why this press must not become a request, or None when it may. Every rule
/// that guards the wire is here and nowhere else: the two that were not here
/// were the two that let a write through -- a force whose precondition lived in
/// a key handler, and a comparison the diff had refused to make.
///
/// The sentences say what the press did, not what the status line already says
/// standing beside them: a refusal's own reason is the summary, and a blocked
/// payload's reasons are their own piece of that line.
pub(crate) fn refuse(wanted: Armed, at: Ready<'_>) -> Option<String> {
    if !at.patchable {
        return Some("the server serves this kind without a patch verb".to_string());
    }
    // The reasons themselves are already on the status line, standing rather
    // than one-shot, so this says what the press did and not what the line
    // beside it says.
    if !at.blocked.is_empty() {
        return Some("this document cannot be applied, so nothing was sent".to_string());
    }
    // A refusal is not agreement. Zero rows and zero counts mean the comparison
    // never happened, and applying what nobody compared is the thing the whole
    // view exists to prevent. The refusal's own sentence is the summary.
    if matches!(at.verdict, Verdict::Refused(_)) {
        return Some("nothing here has been reviewed, so there is nothing to apply".to_string());
    }
    match at.editor {
        None => {
            return Some("the editor this diff came from is gone; nothing to apply".to_string());
        }
        // The diff *is* the review. If the buffer moved since it was made, what
        // is on screen is not what would be sent, and the only honest answer is
        // to say so and re-compare -- never to send text nobody looked at.
        Some(stamp) if stamp != at.reviewed => {
            return Some(
                "the buffer changed after this comparison; r compares it again".to_string(),
            );
        }
        Some(_) => {}
    }
    // And the dry run *is* the diff's right-hand side. Opening the view against
    // live alone reaches a write whose payload the server never saw, defaulting
    // and admission included, which is a review of a guess.
    if at.dry_run != Some(at.reviewed) {
        return Some(
            "the server has not been asked what this would store; ctrl-alt-r asks it".to_string(),
        );
    }
    if wanted == Armed::Force && at.conflicts == 0 {
        return Some("nothing is owned elsewhere, so there is nothing to force".to_string());
    }
    if let Some(holder) = at.in_flight {
        return Some(format!("{holder} is already in flight"));
    }
    None
}

/// What a press that edits the buffer -- rather than the cluster -- has to be
/// true about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Keepable {
    /// Which comparison is on screen. This is the load-bearing one: in
    /// [`Mode::DryRun`] the right-hand document is what the *server* answered,
    /// not the editor's buffer, so the hunk's ranges point into a document the
    /// editor does not have. An edit built from them would splice bytes at
    /// offsets that mean nothing where they land.
    pub(crate) mode: Option<Mode>,
    /// The hunk the reader is on, if there is one.
    pub(crate) origin: Option<Origin>,
    pub(crate) reviewed: BufferStamp,
    pub(crate) editor: Option<BufferStamp>,
}

/// Why this hunk cannot be taken into the buffer, or None when it can.
///
/// Kept apart from [`refuse`] because nothing here reaches the cluster and none
/// of that function's rules apply: an unpatchable kind, a blocked payload and a
/// missing dry run all say nothing about whether a person may edit their own
/// text. What is shared is the rule that a review is of one buffer -- ranges
/// derived from a comparison of text the editor has since changed splice at the
/// wrong bytes, which in an editor is worse than a refusal.
pub(crate) fn refuse_keep(at: Keepable) -> Option<String> {
    match at.editor {
        None => {
            return Some("the editor this diff came from is gone; nothing to edit".to_string());
        }
        Some(stamp) if stamp != at.reviewed => {
            return Some(
                "the buffer changed after this comparison; r compares it again".to_string(),
            );
        }
        Some(_) => {}
    }
    if at.mode != Some(Mode::Local) {
        return Some(
            "this compares the server's own answer, not the cluster's; ctrl-alt-d compares \
             against live"
                .to_string(),
        );
    }
    match at.origin {
        None | Some(Origin::Common) => {
            Some("nothing here differs from the cluster; n moves to the next change".to_string())
        }
        Some(_) => None,
    }
}

/// What taking one hunk did, in the words of the classification it came from.
/// The dry run that authorised an apply is void afterwards -- the bytes it
/// answered for are not the bytes in the buffer any more -- so the sentence
/// names the key that asks again.
pub(crate) fn kept_note(origin: Origin, keep: &diff::Keep) -> String {
    let what = match origin {
        Origin::Theirs => "kept the cluster's change",
        Origin::Mine => "put the cluster's own text back",
        Origin::Conflict => "took the cluster's side of the conflict",
        Origin::Undeclared => "took the value the cluster holds",
        Origin::Common => "changed nothing",
    };
    let lines = match (keep.taken, keep.dropped) {
        (0, dropped) => format!("dropped {}", lines_of(dropped)),
        (taken, 0) => format!("added {}", lines_of(taken)),
        (taken, dropped) => format!("{} in place of {dropped}", lines_of(taken)),
    };
    format!("{what}: {lines}; ctrl-alt-r asks the server about the result")
}

pub(crate) fn lines_of(count: usize) -> String {
    if count == 1 {
        return "1 line".to_string();
    }
    format!("{count} lines")
}

/// The two-press latch in front of every write. Pure, so the rule is tested
/// without a window.
#[derive(Debug, Default)]
pub(crate) struct ApplyGate {
    armed: Option<Armed>,
}

impl ApplyGate {
    pub(crate) fn step(&mut self, wanted: Armed) -> Step {
        if self.armed == Some(wanted) {
            self.armed = None;
            return Step::Go;
        }
        self.armed = Some(wanted);
        Step::Ask
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = None;
    }

    pub(crate) fn armed(&self) -> Option<Armed> {
        self.armed
    }
}

// What a reply has to be settled against: the comparison the request was made
// from, rather than whatever is on screen when the answer lands. A recompute
// during a real apply must not let the reply describe a different document --
// or, worse, reload a different buffer over unsaved work.
#[derive(Debug, Clone)]
pub(crate) struct Sent {
    pub(crate) generation: u64,
    pub(crate) stamp: BufferStamp,
    pub(crate) dry_run: bool,
    // What the prune did to the bytes that went out, worded for after the fact.
    pub(crate) note: String,
    // Which object the live document was read from *when this went out*. A real
    // apply's reply always speaks, so reading the field live when the answer
    // lands compares the server's answer against whatever a recompute has since
    // pointed the view at -- and a recompute during a slow apply then reports a
    // recreation that did not happen, over a write that did. This is the field
    // this struct exists to be: settle against the request, not against the
    // screen.
    pub(crate) uid: Option<String>,
}

impl Sent {
    // Whose answer still reaches the user. A dry run speaks only for the
    // comparison it was asked about, so a recompute discards it. A real apply
    // *happened*, and what the server said about it is the user's whatever the
    // view is comparing now: the guard that discarded both turned an admission
    // webhook's refusal into an empty status bar next to a fresh local diff.
    pub(crate) fn still_speaks(&self, generation: u64) -> bool {
        !self.dry_run || self.generation == generation
    }
}

/// Whether an answer from the server is about the object the review was made of.
///
/// A server-side apply *creates* what is absent, so applying a document whose
/// object was deleted between the read and the press brings it back instead of
/// failing -- `kubectl apply`'s behaviour, and not what the person pressing the
/// key is thinking about. The uid is what distinguishes the two, and it costs no
/// second round trip: the answer to the apply carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Identity {
    /// Both uids are known and equal, so whatever else changed, this is the same
    /// object.
    Same,
    /// Both are known and differ: the object the server answered about is not the
    /// one this document was read from.
    Different,
    /// One side carried no uid. Silence, not a guess: an unfounded claim that an
    /// object was replaced sends someone looking for a deletion nobody made, and
    /// the missing-field case is exactly where that claim would come from.
    Unknown,
}

pub(crate) fn identity(read: Option<&str>, answered: Option<&str>) -> Identity {
    match (read, answered) {
        (Some(read), Some(answered)) if read == answered => Identity::Same,
        (Some(_), Some(_)) => Identity::Different,
        _ => Identity::Unknown,
    }
}

/// What a *write* that landed on a different object has to say for itself. Which
/// of the two mechanisms produced it -- the object was deleted and this apply
/// recreated it, or someone else replaced it first and this apply updated the
/// replacement -- is not knowable from here, and both share the sentence that
/// matters: the object on screen is gone and this went somewhere else.
pub(crate) fn landed_note(read: Option<&str>, answered: Option<&str>) -> &'static str {
    match identity(read, answered) {
        Identity::Different => recreated_note(),
        Identity::Same | Identity::Unknown => "",
    }
}

pub(crate) fn recreated_note() -> &'static str {
    "; it did not update the object this was opened from -- that one is gone, so the write landed \
     on a new object with a different uid"
}

/// And what a *dry run* about a different object says. Nothing has been written,
/// so the point is not what happened but that the left-hand side of the
/// comparison describes an object that no longer exists.
pub(crate) fn stale_object_note() -> &'static str {
    "the server answered about a different object than this was opened from -- that one is gone, \
     so the live side of this comparison is out of date; open the object again"
}

/// What a dry-run answer means once the comparison against it has been made:
/// the line to show, or -- when there is no comparison -- the line to show
/// *and* the fact that nothing was reviewed. Not two branches on a boolean,
/// because a comparison nobody could make is not a comparison that found
/// nothing, and reading it as one told the user that applying a document the
/// diff had refused to review would change nothing.
pub(crate) fn reviewed(verdict: Verdict) -> Result<&'static str, String> {
    match verdict {
        Verdict::Differs => Ok("ctrl-s applies this"),
        Verdict::Agreed => Ok("the cluster already holds this; applying changes nothing"),
        Verdict::Refused(reason) => Err(format!("the server answered, but {reason}")),
    }
}
