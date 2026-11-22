//! [`LiveWriter`]: a session's write handle with dsh's live write path in
//! front of it. Appends to the session stay synchronous; the writer buffers
//! the events they publish and writes them in batches. (dsh: the routed
//! live-event buffer of `JsonlSessionHandle`, `enqueueLive` and
//! `drainLive`, in `session-persistence-jsonl/src/storage.ts`.)
//!
//! dsh calls the batching window "the seam's internal scheduling policy",
//! but writes it inside each backend's handle. Here it is written once, in
//! the seam, around any [`SessionHandle`], so every backend gets the same
//! policy.
//!
//! # The window
//!
//! The first event to arrive while nothing is pending starts a fixed window
//! of [`BATCH_WINDOW`]; when it ends, everything buffered is written as one
//! batch. [`flush`](LiveWriter::flush) doesn't wait for the window: it
//! drains at once, then flushes the handle. A failed batch stays buffered,
//! in order, and the window stays quiet until the next drain, which
//! retries it and reports the failure. dsh also warns through its logger
//! when a background write fails; Karaka has no logger yet, so the failure
//! waits for that next drain.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use karaka_session::Entry;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::{PersistenceError, SessionHandle};

/// The longest a live event waits before its batch starts writing. (dsh:
/// `LIVE_WRITE_BATCH_MAX_DELAY_MS`, 200 ms.)
pub const BATCH_WINDOW: Duration = Duration::from_millis(200);

/// A write handle that takes live events one at a time, synchronously, and
/// writes them in batches.
///
/// Cloning it is cheap, and every clone is the same writer.
#[derive(Clone)]
pub struct LiveWriter {
    inner: Arc<Inner>,
}

struct Inner {
    handle: Box<dyn SessionHandle>,
    buffer: Mutex<Buffer>,
    /// One drain at a time, so batches are written in order.
    drain: tokio::sync::Mutex<()>,
    runtime: Handle,
    window: Duration,
}

#[derive(Default)]
struct Buffer {
    events: Vec<Entry>,
    /// The window's timer, numbered so a timer that fires late can tell
    /// whether it is still the current one.
    timer: Option<(u64, JoinHandle<()>)>,
    timers: u64,
    /// Set by a failed drain: the window doesn't start again until the
    /// next drain.
    paused: bool,
}

impl LiveWriter {
    /// A writer in front of `handle`, which must be a write handle, with
    /// dsh's [`BATCH_WINDOW`].
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime, which runs the window's timer.
    pub fn new(handle: Box<dyn SessionHandle>) -> Self {
        Self::with_window(handle, BATCH_WINDOW)
    }

    /// The same, with another window; for tests.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime.
    pub fn with_window(handle: Box<dyn SessionHandle>, window: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                handle,
                buffer: Mutex::new(Buffer::default()),
                drain: tokio::sync::Mutex::new(()),
                runtime: Handle::current(),
                window,
            }),
        }
    }

    /// The handle the writer writes through.
    pub fn handle(&self) -> &dyn SessionHandle {
        self.inner.handle.as_ref()
    }

    /// Buffers one live event, and starts the window if nothing was
    /// pending. Never blocks.
    pub fn enqueue(&self, entry: &Entry) {
        let mut buffer = self.inner.lock();
        buffer.events.push(entry.clone());
        if buffer.timer.is_some() || buffer.paused {
            return;
        }
        buffer.timers += 1;
        let number = buffer.timers;
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        // The window starts now, not when the task first runs.
        let deadline = tokio::time::Instant::now() + self.inner.window;
        let timer = self.inner.runtime.spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            {
                let mut buffer = inner.lock();
                if buffer.timer.as_ref().is_some_and(|(n, _)| *n == number) {
                    buffer.timer = None;
                }
            }
            // A failure stays buffered for the next drain to report.
            let _ = inner.drain().await;
        });
        buffer.timer = Some((number, timer));
    }

    /// Writes everything buffered now, without waiting for the window.
    pub async fn drain(&self) -> Result<(), PersistenceError> {
        self.inner.drain().await
    }

    /// Drains, then flushes the handle: everything enqueued before the call
    /// is durable when it returns. (dsh: the `session/flush` listener.)
    pub async fn flush(&self) -> Result<(), PersistenceError> {
        self.inner.drain().await?;
        self.inner.handle.flush().await
    }

    /// Drains until nothing is left, then closes the handle. The handle is
    /// closed, and its write ownership given up, even when the drain fails;
    /// both failures are reported together. (dsh: `JsonlSessionHandle.close`.)
    pub async fn close(&self) -> Result<(), PersistenceError> {
        let mut failures = Vec::new();
        loop {
            if let Err(error) = self.inner.drain().await {
                failures.push(error);
                break;
            }
            if self.inner.lock().events.is_empty() {
                break;
            }
        }
        if let Some((_, timer)) = self.inner.lock().timer.take() {
            timer.abort();
        }
        if let Err(error) = self.inner.handle.close().await {
            failures.push(error);
        }
        let id = self.inner.handle.id().clone();
        match failures.len() {
            0 => Ok(()),
            1 => Err(failures.remove(0)),
            _ => Err(PersistenceError::Aggregate {
                message: format!(
                    "session \"{id}\": close failed to drain and to release its write lock"
                ),
                errors: failures,
            }),
        }
    }

    /// How many events are waiting to be written.
    pub fn pending(&self) -> usize {
        self.inner.lock().events.len()
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Buffer> {
        // Every change to the buffer is finished before anything that
        // could panic, so a poisoned lock still holds a whole buffer.
        self.buffer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn drain(&self) -> Result<(), PersistenceError> {
        let _one_at_a_time = self.drain.lock().await;
        {
            let mut buffer = self.lock();
            if let Some((_, timer)) = buffer.timer.take() {
                timer.abort();
            }
            buffer.paused = false;
        }
        loop {
            // Taken at each turn, so events that land while a batch writes
            // join the next one, in order.
            let batch = std::mem::take(&mut self.lock().events);
            if batch.is_empty() {
                return Ok(());
            }
            if let Err(error) = self.handle.append(batch.clone()).await {
                let mut buffer = self.lock();
                let later = std::mem::replace(&mut buffer.events, batch);
                buffer.events.extend(later);
                buffer.paused = true;
                return Err(error);
            }
        }
    }
}

impl std::fmt::Debug for LiveWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveWriter")
            .field("session", self.inner.handle.id())
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}
