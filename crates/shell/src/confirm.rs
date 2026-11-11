//! One write per confirmation, and a view that holds until the write answers.
//!
//! A destructive press is asked, then confirmed on the same subject, and the
//! confirmation is spent the moment it is taken: [`Latch::press`] hands out the
//! stored request exactly once and every press after it is refused until the
//! answer has landed and the person has moved off the subject and back. A
//! burst of presses during a slow write sends one write, and moving off the row
//! never releases a write that is out: only its answer does.
//!
//! The lines a person reads are formed once, at the transition that makes them
//! true, and stored in the state that shows them. A status slot keeps its
//! combined text until those fields change, so an unchanged render clones an
//! `Arc`. Nothing here draws or talks to a cluster; the only gpui item is
//! [`SharedString`].

use gpui::SharedString;

/// Shown beside an answer on its own row: the next press there is refused, and
/// this says how to ask again. It says nothing about the server, because it
/// also follows a dropped reply, a deadline or an answer that is no verdict.
pub(crate) const ASK_AGAIN: &str = "(move off this row and back to ask again)";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Latch<T> {
    Idle,
    // The ask answered: the next press on `subject` sends `sends`. Both lines
    // were formed when the ask landed.
    Armed {
        subject: String,
        sends: T,
        armed: SharedString,
        sent: SharedString,
    },
    // A write is out. Nothing is sent again until its answer settles it.
    Sent {
        subject: String,
        line: SharedString,
    },
    // The write answered while its subject was selected; a press there is
    // refused until the person moves off it.
    Answered {
        subject: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Press<T> {
    Ask,
    Go(T),
    Refused,
}

impl<T> Latch<T> {
    pub(crate) fn press(&mut self, subject: &str) -> Press<T> {
        match std::mem::replace(self, Latch::Idle) {
            Latch::Armed {
                subject: armed,
                sends,
                sent,
                ..
            } if armed == subject => {
                *self = Latch::Sent {
                    subject: armed,
                    line: sent,
                };
                Press::Go(sends)
            }
            Latch::Sent { subject, line } => {
                *self = Latch::Sent { subject, line };
                Press::Refused
            }
            Latch::Answered { subject: answered } if answered == subject => {
                *self = Latch::Answered { subject: answered };
                Press::Refused
            }
            Latch::Idle | Latch::Armed { .. } | Latch::Answered { .. } => Press::Ask,
        }
    }

    /// The ask's answer landed on the subject still selected. Only from Idle:
    /// an ask that returns while a write is out, or after its answer, arms
    /// nothing.
    pub(crate) fn arm(
        &mut self,
        subject: String,
        sends: T,
        armed: SharedString,
        sent: SharedString,
    ) {
        if matches!(self, Latch::Idle) {
            *self = Latch::Armed {
                subject,
                sends,
                armed,
                sent,
            };
        }
    }

    /// A view opened while a write it did not send is still out: it refuses
    /// presses and shows the write's line until the answer lands. Only from
    /// Idle.
    pub(crate) fn resume(&mut self, subject: String, line: SharedString) {
        if matches!(self, Latch::Idle) {
            *self = Latch::Sent { subject, line };
        }
    }

    /// Every movement. A write that is out is kept: moving off its row does
    /// not answer it.
    pub(crate) fn disarm(&mut self) {
        if !matches!(self, Latch::Sent { .. }) {
            *self = Latch::Idle;
        }
    }

    /// A write's answer, with the subject selected when it lands. On its own
    /// row the latch holds as Answered; elsewhere it is Idle, because the
    /// person already moved. Outside Sent nothing changes.
    pub(crate) fn settle(&mut self, selected: Option<&str>) {
        if let Latch::Sent { subject, .. } = self {
            *self = if selected == Some(subject.as_str()) {
                Latch::Answered {
                    subject: std::mem::take(subject),
                }
            } else {
                Latch::Idle
            };
        }
    }
}

/// What a view's status slot shows, projected once from the latch and the
/// view's own messages. Every field is optional and drawn in this order, each
/// on its own line: the view's label directly under the rows it is about, then
/// the line, the hint and a note.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) line: Option<SharedString>,
    pub(crate) hint: Option<&'static str>,
    pub(crate) note: Option<SharedString>,
    pub(crate) label: Option<SharedString>,
}

impl Slot {
    /// Armed and Sent show their stored line, with `status` as a note under
    /// it so a message written while a write is out is neither hidden nor
    /// lost; Answered shows the answer (in `status`) and the hint; Idle shows
    /// `status`. The label, a listing's denial or failure, always shows.
    pub(crate) fn of<T>(
        latch: &Latch<T>,
        status: Option<&SharedString>,
        label: Option<&SharedString>,
    ) -> Slot {
        let (line, hint, note) = match latch {
            Latch::Armed { armed, .. } => (Some(armed.clone()), None, status.cloned()),
            Latch::Sent { line, .. } => (Some(line.clone()), None, status.cloned()),
            Latch::Answered { .. } => (status.cloned(), Some(ASK_AGAIN), None),
            Latch::Idle => (status.cloned(), None, None),
        };
        Slot {
            line,
            hint,
            note,
            label: label.cloned(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.line.is_none() && self.hint.is_none() && self.note.is_none() && self.label.is_none()
    }

    fn text(&self) -> Option<SharedString> {
        if self.is_empty() {
            return None;
        }
        let words = [
            self.label.as_deref(),
            self.line.as_deref(),
            self.hint,
            self.note.as_deref(),
        ];
        let count = words.iter().flatten().count();
        match count {
            1 => Some(
                self.label
                    .as_ref()
                    .or(self.line.as_ref())
                    .or(self.note.as_ref())
                    .cloned()
                    .unwrap_or_else(|| self.hint.unwrap().into()),
            ),
            _ => {
                let bytes =
                    words.iter().flatten().map(|word| word.len()).sum::<usize>() + count - 1;
                let mut text = String::with_capacity(bytes);
                let mut pieces = words.into_iter().flatten();
                text.push_str(pieces.next().unwrap());
                for piece in pieces {
                    text.push('\n');
                    text.push_str(piece);
                }
                Some(text.into())
            }
        }
    }
}

/// A single text child lets additional fields share the same layout element.
/// Join only when a field changes; a lone message
/// already has the shared storage the view needs.
#[derive(Default)]
pub(crate) struct StatusText {
    slot: Option<Slot>,
    text: Option<SharedString>,
}

impl StatusText {
    pub(crate) fn text(&mut self, slot: Slot) -> Option<SharedString> {
        if self.slot.as_ref() != Some(&slot) {
            self.text = slot.text();
            self.slot = Some(slot);
        }
        self.text.clone()
    }
}

/// Whether a view may say its listing is empty: only with no rows, no message
/// and no label. A denied listing beside "nothing here" would read as a quiet
/// cluster.
pub(crate) fn nothing_listed(
    rows: usize,
    status: Option<&SharedString>,
    label: Option<&SharedString>,
) -> bool {
    rows == 0 && status.is_none() && label.is_none()
}

/// Where a confirmed write lands, in the words every confirmation uses. A
/// kubeconfig context always has a name, so only in-cluster is None; an empty
/// name is printed as one rather than read as in-cluster.
pub(crate) fn write_target(context: Option<&str>) -> String {
    match context {
        None => "this cluster's in-cluster account".to_string(),
        Some("") => "context \"\"".to_string(),
        Some(context) => format!("context {context}"),
    }
}

#[cfg(test)]
#[path = "confirm_test.rs"]
mod tests;
