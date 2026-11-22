//! One model call's live frames. (dsh: `AssistantStreamAttempt` in
//! `agent-loop/src/assistant-stream.ts`.)
//!
//! The loop makes one for each attempt at a call, so a retried call is a
//! new attempt with an id of its own. It sends `start` once the stream is
//! open, a `chunk` for each event, and `end` once the call has settled:
//! `committed`, naming the seq, only after the call's `assistant/message` or
//! `assistant/attempt` is in the log, or `abandoned` if it never gets there.
//! An attempt that started and is dropped without settling sends
//! `abandoned` as it goes, as dsh's `if (!live.ended) live.abandon()` does.
//!
//! dsh also folds the chunks into the compact `stream` records that its
//! `assistant/message` and `assistant/attempt` embed. That isn't ported: the
//! logged `stream` stays empty.

use std::sync::atomic::{AtomicU64, Ordering};

use karaka_agent::{AssistantStreamFrame, FrameListeners, SettledAs, StreamChunk, StreamOutcome};
use karaka_session::{LogError, Seq, Time};

/// The counters an agent keeps for its frames.
#[derive(Debug, Default)]
pub(crate) struct FrameCounters {
    /// Attempts so far. (dsh: `assistantAttemptCounter`.)
    attempts: AtomicU64,
    /// Frames so far, across every attempt. (dsh:
    /// `assistantStreamRevision`.)
    revisions: AtomicU64,
}

/// One attempt at a model call, and the frames it has sent.
pub(crate) struct LiveAttempt<'a> {
    listeners: &'a FrameListeners,
    counters: &'a FrameCounters,
    attempt_id: String,
    turn: u64,
    step: u64,
    /// Chunks sent so far.
    index: u64,
    started: bool,
    ended: bool,
}

impl<'a> LiveAttempt<'a> {
    /// A new attempt of session `session`'s agent, numbered after the last.
    /// It sends nothing until [`start`](Self::start).
    pub(crate) fn new(
        listeners: &'a FrameListeners,
        counters: &'a FrameCounters,
        session: &str,
        turn: u64,
        step: u64,
    ) -> Self {
        let attempt = counters.attempts.fetch_add(1, Ordering::Relaxed) + 1;
        Self {
            listeners,
            counters,
            attempt_id: format!("{session}:{attempt}"),
            turn,
            step,
            index: 0,
            started: false,
            ended: false,
        }
    }

    /// The stream is open. (dsh: `start`.)
    pub(crate) fn start(&mut self) {
        self.started = true;
        self.emit(|attempt_id, revision, live| AssistantStreamFrame::Start {
            attempt_id,
            revision,
            turn: live.turn,
            step: live.step,
        });
    }

    /// One item of the stream arrived: an event, or the failure that ends
    /// it. (dsh: `push`.)
    pub(crate) fn push(&mut self, chunk: StreamChunk) {
        let index = self.index;
        self.index += 1;
        self.emit(|attempt_id, revision, _| AssistantStreamFrame::Chunk {
            attempt_id,
            revision,
            index,
            time: Time::now(),
            chunk,
        });
    }

    /// The call's event was offered to the log: `logged` is what the append
    /// returned. Sends `committed` if it went in, `abandoned` if not, and
    /// passes the result on. (dsh: `settle`.)
    pub(crate) fn settle(
        &mut self,
        event_type: SettledAs,
        logged: Result<Seq, LogError>,
    ) -> Result<Seq, LogError> {
        match &logged {
            Ok(seq) => self.end(StreamOutcome::Committed {
                event_type,
                seq: *seq,
            }),
            Err(_) => self.end(StreamOutcome::Abandoned),
        }
        logged
    }

    fn end(&mut self, outcome: StreamOutcome) {
        self.ended = true;
        let index = self.index;
        self.emit(|attempt_id, revision, _| AssistantStreamFrame::End {
            attempt_id,
            revision,
            index,
            outcome,
        });
    }

    fn emit(&self, frame: impl FnOnce(String, u64, &Self) -> AssistantStreamFrame) {
        let revision = self.counters.revisions.fetch_add(1, Ordering::Relaxed) + 1;
        let frame = frame(self.attempt_id.clone(), revision, self);
        self.listeners.emit(&frame);
    }
}

/// An attempt that started but never settled is abandoned. (dsh:
/// `abandon`, called when `live.ended` is still false.)
impl Drop for LiveAttempt<'_> {
    fn drop(&mut self) {
        if self.started && !self.ended {
            self.end(StreamOutcome::Abandoned);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use karaka_agent::{
        AssistantStreamFrame, FrameListeners, SettledAs, StreamChunk, StreamOutcome,
    };
    use karaka_model::Event;
    use karaka_session::{LogError, Seq};

    use super::{FrameCounters, LiveAttempt};

    fn heard(
        listeners: &FrameListeners,
    ) -> (
        Arc<Mutex<Vec<AssistantStreamFrame>>>,
        karaka_agent::FrameSubscription,
    ) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let noted = Arc::clone(&frames);
        let subscription = listeners.subscribe(Arc::new(move |frame| {
            noted.lock().unwrap().push(frame.clone());
        }));
        (frames, subscription)
    }

    fn ends(frames: &[AssistantStreamFrame]) -> Vec<(String, u64, u64, StreamOutcome)> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                AssistantStreamFrame::End {
                    attempt_id,
                    revision,
                    index,
                    outcome,
                } => Some((attempt_id.clone(), *revision, *index, *outcome)),
                _ => None,
            })
            .collect()
    }

    /// dsh: "abandons the live row when durable Assistant settlement is
    /// rejected" (`loop.spec.ts:123`).
    #[test]
    fn a_refused_append_abandons_the_attempt() {
        let listeners = FrameListeners::new();
        let counters = FrameCounters::default();
        let (frames, _subscription) = heard(&listeners);
        let mut live = LiveAttempt::new(&listeners, &counters, "s", 1, 1);
        live.start();
        live.push(StreamChunk::Event(Event::TextDelta {
            index: 0,
            text: "hi".into(),
        }));
        let refused = Err(LogError::Invalid {
            seq: Seq(4),
            reason: "refused".into(),
        });
        assert!(live.settle(SettledAs::AssistantMessage, refused).is_err());
        drop(live);

        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            ends(&frames),
            [("s:1".into(), 3, 1, StreamOutcome::Abandoned)]
        );
    }

    /// dsh: "abandons a started live attempt when final block assembly
    /// fails" (`loop.spec.ts:152`): any exit after the start that doesn't
    /// settle.
    #[test]
    fn a_started_attempt_dropped_unsettled_is_abandoned() {
        let listeners = FrameListeners::new();
        let counters = FrameCounters::default();
        let (frames, _subscription) = heard(&listeners);
        {
            let mut live = LiveAttempt::new(&listeners, &counters, "s", 1, 1);
            live.start();
        }
        assert_eq!(
            ends(&frames.lock().unwrap()),
            [("s:1".into(), 2, 0, StreamOutcome::Abandoned)]
        );
    }

    /// dsh: "does not emit an end frame when prepared dispatch throws
    /// before start" (`loop.spec.ts:211`). The attempt still takes its
    /// number.
    #[test]
    fn an_attempt_that_never_started_sends_nothing() {
        let listeners = FrameListeners::new();
        let counters = FrameCounters::default();
        let (frames, _subscription) = heard(&listeners);
        drop(LiveAttempt::new(&listeners, &counters, "s", 1, 1));
        assert!(frames.lock().unwrap().is_empty());

        let mut next = LiveAttempt::new(&listeners, &counters, "s", 1, 1);
        next.start();
        assert_eq!(frames.lock().unwrap()[0].attempt_id(), "s:2");
        let committed = next.settle(SettledAs::AssistantAttempt, Ok(Seq(9)));
        assert_eq!(committed, Ok(Seq(9)));
    }
}
