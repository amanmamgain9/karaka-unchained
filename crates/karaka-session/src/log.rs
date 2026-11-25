use std::error::Error;
use std::fmt;

use karaka_model::Message;

use crate::checks;
use crate::derive::{self, History};
use crate::event::{Entry, Seq, SessionEvent, Time};
use crate::repair;
use crate::request::{RequestContext, RequestHeader, canonical_header};
use crate::surface::SurfaceFold;

/// One session's events, in the order they happened. (dsh v4: `Session`.)
///
/// Events are only ever appended: nothing is edited or removed. Everything
/// else about the session, such as the history the model sees, is read from
/// these events.
///
/// Every event is checked as it joins, whether it is appended or restored
/// from storage: its seq must be next, and its surface operation must fit
/// the surface so far (see `surface.rs`). So a log always derives.
#[derive(Clone, Debug, Default)]
pub struct SessionLog {
    entries: Vec<Entry>,
    /// Kept up to date on each append, so derivation doesn't refold the log.
    surface: SurfaceFold,
}

/// Why an event was refused. The log is unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogError {
    /// The event's seq isn't the next one. (dsh v4: "session event seq ...
    /// is not contiguous".)
    NotContiguous { expected: Seq, found: Seq },
    /// The event doesn't fit the log, such as a replacement of a node that
    /// isn't on the surface. The reason uses v4's wording.
    Invalid { seq: Seq, reason: String },
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotContiguous { expected, found } => {
                write!(
                    f,
                    "session event seq {found} is not contiguous; expected {expected}"
                )
            }
            Self::Invalid { seq, reason } => write!(f, "session event at seq {seq}: {reason}"),
        }
    }
}

impl Error for LogError {}

impl SessionLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// A log holding `entries`, as storage gave them back: every seq and
    /// time kept, nothing added. Fails on the first entry that doesn't fit.
    ///
    /// v4 appends a `session/end-seed` marker when it replays a stored log;
    /// the code that resumes a session from storage adds it
    /// (`DurableSessions` in `karaka-session-persistence`), so nothing is
    /// added here. Repair is separate too: see [`repair`](Self::repair).
    pub fn restore(entries: impl IntoIterator<Item = Entry>) -> Result<Self, LogError> {
        let mut log = Self::new();
        for entry in entries {
            log.push(entry)?;
        }
        Ok(log)
    }

    /// Adds `event` to the end of the log, stamped with the current time,
    /// and returns its seq: the number of events before it.
    pub fn append(&mut self, event: SessionEvent) -> Result<Seq, LogError> {
        self.push(Entry {
            seq: self.next_seq(),
            time: Time::now(),
            ignorable: false,
            event,
        })
    }

    /// Adds every event in `events`, in order, or none of them if one is
    /// refused. Returns their seqs.
    pub fn append_all(
        &mut self,
        events: impl IntoIterator<Item = SessionEvent>,
    ) -> Result<Vec<Seq>, LogError> {
        let length = self.entries.len();
        let surface = self.surface.clone();
        let mut seqs = Vec::new();
        for event in events {
            match self.append(event) {
                Ok(seq) => seqs.push(seq),
                Err(error) => {
                    // Undo the batch's earlier events.
                    self.entries.truncate(length);
                    self.surface = surface;
                    return Err(error);
                }
            }
        }
        Ok(seqs)
    }

    /// Adds `entry` exactly as it is, keeping its time and `ignorable`
    /// marker. Its seq must be the next one.
    pub fn push(&mut self, entry: Entry) -> Result<Seq, LogError> {
        let expected = self.next_seq();
        if entry.seq != expected {
            return Err(LogError::NotContiguous {
                expected,
                found: entry.seq,
            });
        }
        let invalid = |reason| LogError::Invalid {
            seq: entry.seq,
            reason,
        };
        checks::check_entry(&self.entries, &entry).map_err(invalid)?;
        self.surface.admit(&self.entries, &entry).map_err(invalid)?;
        let seq = entry.seq;
        self.entries.push(entry);
        Ok(seq)
    }

    /// The events that would close the log's open last turn, if a crash
    /// left one open. Changes nothing; see `repair.rs`.
    pub fn repair_closers(&self) -> Vec<Entry> {
        repair::interrupted_turn_closers(&self.entries)
    }

    /// Appends the events that close the log's open last turn, if any, and
    /// returns them. Run it when a stored log is loaded, before the session
    /// continues, and never while a turn is live: a live turn's running
    /// tools would be answered as interrupted.
    pub fn repair(&mut self) -> Result<Vec<Entry>, LogError> {
        let closers = self.repair_closers();
        for closer in &closers {
            self.push(closer.clone())?;
        }
        Ok(closers)
    }

    /// Every event so far, in order, each with its seq.
    pub fn events(&self) -> &[Entry] {
        &self.entries
    }

    /// The events from `seq` on, `seq` included. Empty when `seq` is the
    /// next seq or later. A follower passes the next seq it expects, so it
    /// gets exactly what it hasn't seen.
    pub fn events_since(&self, seq: Seq) -> &[Entry] {
        let start = usize::try_from(seq.0)
            .map_or(self.entries.len(), |start| start.min(self.entries.len()));
        &self.entries[start..]
    }

    /// How many events the log holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The seq the next event will get.
    pub fn next_seq(&self) -> Seq {
        // `usize` to `u64` never loses anything on the platforms Rust runs on.
        Seq(self.entries.len() as u64)
    }

    /// The seqs of the events the model sees, in order. (dsh v4:
    /// `session.surface.nodes`.)
    pub fn surface_nodes(&self) -> &[Seq] {
        self.surface.nodes()
    }

    /// The system prompt and conversation to send the model, derived from
    /// the events. (dsh v4: `deriveMessages`.) See `derive.rs` for the rules.
    ///
    /// It's built anew on each call. v4 caches it and extends it per new
    /// node; that can come later if derivation shows up as slow.
    pub fn history(&self) -> History {
        derive::history(&self.entries, self.surface.nodes())
    }

    /// Just the conversation part of [`history`](Self::history).
    pub fn messages(&self) -> Vec<Message> {
        self.history().messages
    }

    /// The request header in force: the latest `request/header`'s, in
    /// canonical form. `None` before the first. (dsh v4:
    /// `Session.requestHeader`, which folds with `foldRequestHeader`.)
    pub fn request_header(&self) -> Option<RequestHeader> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                SessionEvent::RequestHeader(data) => Some(canonical_header(&data.header)),
                _ => None,
            })
    }

    /// The latest `request/context`. `None` before the first. (dsh v4:
    /// `Session.requestContext`.)
    pub fn request_context(&self) -> Option<RequestContext> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                SessionEvent::RequestContext(context) => Some(context.clone()),
                _ => None,
            })
    }

    /// How many replacements the surface has taken so far. A request built
    /// after a replacement starts a new series of messages. (dsh v4:
    /// `session.surface.contentGeneration`.)
    pub fn surface_generation(&self) -> u64 {
        self.surface.generation()
    }
}

/// Checks the parts of one event that v4 checks on its own, apart from the
/// log (dsh v4: `validateSessionEventData` in `surface.ts`).
#[cfg(test)]
mod tests {
    use super::{LogError, SessionLog};
    use crate::message::{UserMessage, UserSource};
    use crate::{Entry, Seq, SessionEvent, SurfaceIntent, SurfaceOp, Time, TurnEndReason};

    fn user(text: &str) -> SessionEvent {
        SessionEvent::user(UserMessage::text(text, UserSource::user(), text))
    }

    #[test]
    fn seqs_count_up_from_zero_without_gaps() {
        let mut log = SessionLog::new();
        let seqs: Vec<Seq> = (0..5)
            .map(|n| log.append(user(&format!("message {n}"))).unwrap())
            .collect();
        assert_eq!(seqs, [Seq(0), Seq(1), Seq(2), Seq(3), Seq(4)]);
        for (index, entry) in log.events().iter().enumerate() {
            assert_eq!(entry.seq, Seq(index as u64));
        }
        assert_eq!(log.events()[1].event, user("message 1"));
    }

    #[test]
    fn a_new_log_is_empty() {
        let log = SessionLog::new();
        assert!(log.is_empty());
        assert_eq!(log.next_seq(), Seq(0));
        assert!(log.messages().is_empty());
        assert_eq!(log.history().system, None);
    }

    // dsh v4 session.spec.ts: "validates seed events: rejects a
    // non-contiguous seq".
    #[test]
    fn restoring_refuses_a_gap_in_seqs() {
        let entries = vec![
            Entry {
                seq: Seq(0),
                time: Time(1),
                ignorable: false,
                event: SessionEvent::turn_start(1),
            },
            Entry {
                seq: Seq(5),
                time: Time(2),
                ignorable: false,
                event: SessionEvent::turn_end(1, TurnEndReason::Completed),
            },
        ];
        let error = SessionLog::restore(entries).expect_err("a gap is refused");
        assert_eq!(
            error,
            LogError::NotContiguous {
                expected: Seq(1),
                found: Seq(5)
            }
        );
        assert_eq!(
            error.to_string(),
            "session event seq 5 is not contiguous; expected 1"
        );
    }

    #[test]
    fn restoring_keeps_every_time_and_marker() {
        let entries = vec![Entry {
            seq: Seq(0),
            time: Time(42),
            ignorable: true,
            event: SessionEvent::turn_start(1),
        }];
        let log = SessionLog::restore(entries.clone()).unwrap();
        assert_eq!(log.events(), entries);
    }

    #[test]
    fn a_refused_event_in_a_batch_leaves_the_log_as_it_was() {
        let mut log = SessionLog::new();
        log.append(user("kept")).unwrap();
        // A replacement of a node that doesn't exist yet.
        let bad = SessionEvent::UserMessage {
            message: UserMessage::text("bad", UserSource::user(), "bad"),
            surface: SurfaceIntent {
                op: SurfaceOp::Replace {
                    start: Seq(7),
                    end: Seq(7),
                },
                sources: vec![Seq(7)],
            },
        };
        let error = log
            .append_all([user("a"), user("b"), bad])
            .expect_err("the batch is refused");
        assert!(matches!(error, LogError::Invalid { seq: Seq(3), .. }));
        assert_eq!(log.len(), 1);
        assert_eq!(log.surface_nodes(), [Seq(0)]);
        // The next append still gets the next seq.
        assert_eq!(log.append(user("next")), Ok(Seq(1)));
    }

    #[test]
    fn events_since_gives_the_events_from_a_seq_on() {
        let mut log = SessionLog::new();
        log.append_all([user("a"), user("b"), user("c")]).unwrap();
        let seqs = |from| {
            log.events_since(Seq(from))
                .iter()
                .map(|entry| entry.seq.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(seqs(0), [0, 1, 2]);
        assert_eq!(seqs(2), [2]);
        assert_eq!(seqs(3), [] as [u64; 0]);
        assert_eq!(seqs(99), [] as [u64; 0]);
    }

    #[test]
    fn repair_appends_the_closers_once() {
        let mut log = SessionLog::new();
        log.append_all([SessionEvent::turn_start(1), SessionEvent::step_start(1, 1)])
            .unwrap();
        let closers = log.repair().unwrap();
        assert_eq!(closers.len(), 2);
        assert_eq!(log.events()[2..], closers);
        // The turn is closed now, so there is nothing more to repair.
        assert_eq!(log.repair(), Ok(Vec::new()));
    }
}
