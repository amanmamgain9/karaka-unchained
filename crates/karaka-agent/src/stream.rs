//! Live frames of each model call, for a view that shows the answer as it
//! is written. (dsh: `AssistantStreamFrame` in
//! `agent/src/runtime-types.ts:128-167`, emitted as `agent/assistant-stream`,
//! `:355-363`.)
//!
//! Frames are process-local: they are never logged. The log keeps only how
//! each call settled, an `assistant/message` or an `assistant/attempt`, and
//! the `end` frame names the seq it settled at, so a view can swap the live
//! text for the logged message.
//!
//! # Subscribing
//!
//! [`Agent::subscribe`](crate::Agent::subscribe) takes a listener, a plain
//! function called with each frame, in order, on the task that runs the
//! turn, as dsh's emit calls its listeners synchronously. It returns a
//! [`FrameSubscription`]; dropping it unsubscribes.
//!
//! A plain function was chosen over a channel so a listener sees each frame
//! the moment it happens, before the turn goes on: a `chunk` before the
//! next event is read, an `end` right after its event is logged. With a
//! channel the turn could finish, and `when_idle` resolve, while frames were
//! still queued. The price is that a listener runs on the turn's task, so it
//! must be quick: one that blocks holds the turn up, as it would in dsh. A
//! listener with slow work to do should hand the frame on, for example into
//! a channel of its own.
//!
//! A listener that panics doesn't stop the turn: the panic is caught, and
//! that listener is dropped, so it isn't called again. The others carry on.

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_model::{Event, ModelError};
use karaka_session::{Seq, Time};

/// One live frame of a model call. (dsh: `AssistantStreamFrame`.)
///
/// Every frame carries the call's attempt id, and a revision that counts up
/// across every frame of the agent, so a view can order frames from several
/// calls.
#[derive(Clone, Debug, PartialEq)]
pub enum AssistantStreamFrame {
    /// The call's stream has opened. A call that fails before that sends no
    /// frames at all.
    Start {
        attempt_id: String,
        revision: u64,
        turn: u64,
        step: u64,
    },
    /// One event of the stream, as the model sent it.
    Chunk {
        attempt_id: String,
        revision: u64,
        /// Counts the call's chunks from 0, with no gaps.
        index: u64,
        /// When the event arrived.
        time: Time,
        chunk: StreamChunk,
    },
    /// The call has settled, and nothing more will come for this attempt.
    End {
        attempt_id: String,
        revision: u64,
        /// How many chunks the call sent.
        index: u64,
        outcome: StreamOutcome,
    },
}

impl AssistantStreamFrame {
    /// The id of the call the frame belongs to.
    pub fn attempt_id(&self) -> &str {
        match self {
            Self::Start { attempt_id, .. }
            | Self::Chunk { attempt_id, .. }
            | Self::End { attempt_id, .. } => attempt_id,
        }
    }

    /// The frame's place among all the agent's frames.
    pub fn revision(&self) -> u64 {
        match self {
            Self::Start { revision, .. }
            | Self::Chunk { revision, .. }
            | Self::End { revision, .. } => *revision,
        }
    }
}

/// What a chunk carries: one item of the model's stream. (dsh:
/// `StreamChunk`.)
///
/// Each [`Event`] is one of dsh's chunks, and goes as it is. A stream that
/// fails ends with its error instead, and that is the attempt's last chunk
/// before its `End`, as dsh's failed stream ends with a `finish` chunk of
/// kind `error` carrying the failure.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamChunk {
    /// An event, as the model sent it.
    Event(Event),
    /// Why the call failed, exactly as the model said. A stream that ended
    /// with neither its last event nor an error fails as
    /// [`ErrorKind::Network`](karaka_model::ErrorKind::Network), "the stream
    /// ended before the model finished", as Karaka's providers report a
    /// body that breaks off.
    Failed(ModelError),
}

/// How a call settled. (dsh: the `end` frame's `outcome`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamOutcome {
    /// Its event is in the log, at `seq`.
    Committed { event_type: SettledAs, seq: Seq },
    /// Nothing was logged for it: the log refused the event, or the turn
    /// stopped before the call could settle.
    Abandoned,
}

/// The event a call settled as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettledAs {
    /// An answer, or the part of one a cancel cut short.
    /// (dsh: `assistant/message`.)
    AssistantMessage,
    /// A call that left no message. (dsh: `assistant/attempt`.)
    AssistantAttempt,
}

impl SettledAs {
    /// The v4 event type, such as `assistant/message`.
    pub fn event_type(self) -> &'static str {
        match self {
            Self::AssistantMessage => "assistant/message",
            Self::AssistantAttempt => "assistant/attempt",
        }
    }
}

/// What a listener is: a function called with each frame.
pub type FrameListener = Arc<dyn Fn(&AssistantStreamFrame) + Send + Sync>;

/// An agent's frame listeners. A loop keeps one and emits through it; it is
/// in this seam so every loop subscribes and catches panics the same way.
#[derive(Clone, Default)]
pub struct FrameListeners {
    list: Arc<Mutex<List>>,
}

#[derive(Default)]
struct List {
    next_id: u64,
    listeners: Vec<(u64, FrameListener)>,
}

impl FrameListeners {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `listener` after the others. Dropping the subscription removes
    /// it.
    pub fn subscribe(&self, listener: FrameListener) -> FrameSubscription {
        let mut list = lock(&self.list);
        let id = list.next_id;
        list.next_id += 1;
        list.listeners.push((id, listener));
        FrameSubscription {
            list: Arc::downgrade(&self.list),
            id,
        }
    }

    /// Calls every listener with `frame`, in the order they subscribed. A
    /// listener that panics is dropped.
    pub fn emit(&self, frame: &AssistantStreamFrame) {
        // A copy, so a listener can subscribe or unsubscribe while it runs
        // without waiting on the lock this call would hold.
        let listeners = lock(&self.list).listeners.clone();
        for (id, listener) in listeners {
            let called = panic::catch_unwind(AssertUnwindSafe(|| listener(frame)));
            if called.is_err() {
                lock(&self.list).listeners.retain(|(other, _)| *other != id);
            }
        }
    }

    /// How many listeners there are.
    pub fn len(&self) -> usize {
        lock(&self.list).listeners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for FrameListeners {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameListeners")
            .field("listeners", &self.len())
            .finish()
    }
}

/// A listener's place in an agent's list. Dropping it unsubscribes the
/// listener; it is still called for a frame being emitted at that moment.
#[must_use = "dropping the subscription unsubscribes at once"]
#[derive(Debug)]
pub struct FrameSubscription {
    list: Weak<Mutex<List>>,
    id: u64,
}

impl Drop for FrameSubscription {
    fn drop(&mut self) {
        if let Some(list) = self.list.upgrade() {
            lock(&list).listeners.retain(|(id, _)| *id != self.id);
        }
    }
}

// Poisoning is ignored: each change to the list is one push or one retain,
// and listeners run without the lock held.
fn lock(list: &Mutex<List>) -> MutexGuard<'_, List> {
    list.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{AssistantStreamFrame, FrameListeners};

    fn start(revision: u64) -> AssistantStreamFrame {
        AssistantStreamFrame::Start {
            attempt_id: "s:1".into(),
            revision,
            turn: 1,
            step: 1,
        }
    }

    #[test]
    fn listeners_hear_each_frame_in_order_until_they_unsubscribe() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let listeners = FrameListeners::new();
        let first = Arc::clone(&heard);
        let one = listeners.subscribe(Arc::new(move |frame| {
            first.lock().unwrap().push(("one", frame.revision()));
        }));
        let second = Arc::clone(&heard);
        let _two = listeners.subscribe(Arc::new(move |frame| {
            second.lock().unwrap().push(("two", frame.revision()));
        }));
        listeners.emit(&start(1));
        drop(one);
        listeners.emit(&start(2));

        assert_eq!(*heard.lock().unwrap(), [("one", 1), ("two", 1), ("two", 2)]);
        assert_eq!(listeners.len(), 1);
    }

    #[test]
    fn a_listener_that_panics_is_dropped_and_the_others_carry_on() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let listeners = FrameListeners::new();
        let _broken = listeners.subscribe(Arc::new(|_frame| panic!("a broken view")));
        let noted = Arc::clone(&heard);
        let _working = listeners.subscribe(Arc::new(move |frame| {
            noted.lock().unwrap().push(frame.revision());
        }));
        listeners.emit(&start(1));
        listeners.emit(&start(2));

        assert_eq!(*heard.lock().unwrap(), [1, 2]);
        assert_eq!(listeners.len(), 1);
    }

    #[test]
    fn a_listener_can_unsubscribe_itself_while_it_runs() {
        let listeners = FrameListeners::new();
        let slot: Arc<Mutex<Option<super::FrameSubscription>>> = Arc::new(Mutex::new(None));
        let held = Arc::clone(&slot);
        let subscription = listeners.subscribe(Arc::new(move |_frame| {
            held.lock().unwrap().take();
        }));
        *slot.lock().unwrap() = Some(subscription);
        listeners.emit(&start(1));
        assert!(listeners.is_empty());
    }
}
