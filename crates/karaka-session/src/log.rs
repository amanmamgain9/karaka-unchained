use karaka_model::Message;

use crate::derive;
use crate::event::{Entry, Seq, SessionEvent};

/// One session's events, in the order they happened. (dsh: `Session`.)
///
/// Events are only ever appended: nothing is edited or removed. Everything
/// else about the session, such as the history the model sees, is read from
/// these events.
#[derive(Clone, Debug, Default)]
pub struct SessionLog {
    entries: Vec<Entry>,
}

impl SessionLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `event` to the end of the log and returns its seq: the number of
    /// events before it.
    pub fn append(&mut self, event: SessionEvent) -> Seq {
        // `usize` to `u64` never loses anything on the platforms Rust runs on.
        let seq = Seq(self.entries.len() as u64);
        self.entries.push(Entry { seq, event });
        seq
    }

    /// Every event so far, in order, each with its seq.
    pub fn events(&self) -> &[Entry] {
        &self.entries
    }

    /// How many events the log holds. This is also the next event's seq.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The history to send the model, derived from the events.
    /// (dsh: `deriveMessages`.)
    ///
    /// - A user message or an assistant message becomes one message.
    /// - Tool results in a row, which answer one assistant message's calls,
    ///   become one user message, in the order they were logged.
    /// - A call that has no result is answered with
    ///   [`interrupted_result`](crate::interrupted_result), so providers
    ///   accept the history. The log itself is not changed.
    ///
    /// See `derive.rs` for each rule and how it compares with dsh.
    ///
    /// It's built anew on each call. dsh caches it and extends it per new
    /// event; that can come later if derivation shows up as slow.
    pub fn messages(&self) -> Vec<Message> {
        derive::messages(&self.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::SessionLog;
    use crate::{Seq, SessionEvent};

    #[test]
    fn seqs_count_up_from_zero_without_gaps() {
        let mut log = SessionLog::new();
        let seqs: Vec<Seq> = (0..5)
            .map(|n| log.append(SessionEvent::user(format!("message {n}"))))
            .collect();
        assert_eq!(seqs, [Seq(0), Seq(1), Seq(2), Seq(3), Seq(4)]);
    }

    #[test]
    fn each_entry_holds_the_seq_it_was_given() {
        let mut log = SessionLog::new();
        log.append(SessionEvent::user("first"));
        log.append(SessionEvent::user("second"));

        assert_eq!(log.len(), 2);
        for (index, entry) in log.events().iter().enumerate() {
            assert_eq!(entry.seq, Seq(index as u64));
        }
        assert_eq!(log.events()[1].event, SessionEvent::user("second"));
    }

    #[test]
    fn a_new_log_is_empty() {
        let log = SessionLog::new();
        assert!(log.is_empty());
        assert!(log.messages().is_empty());
    }
}
