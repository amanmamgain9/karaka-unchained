//! [`AbortController`] and [`AbortSignal`]: a cancel a filesystem call
//! checks at its own points. (dsh: the web `AbortController` and
//! `AbortSignal` every primitive takes.)
//!
//! Dropping a call's future cancels it too, as elsewhere in Karaka, and the
//! local backend cleans up after a dropped write. A signal adds what
//! dropping can't: the call ends with `FS_ABORTED`, at a point it chose,
//! such as before a write is published.
//!
//! The agent seam's `Cancellation` is a turn's, with a cause; this one has
//! none, so the filesystem seam needn't depend on the agent seam.

use tokio::sync::watch;

/// Aborts the signals it hands out, once. (dsh: `AbortController`.)
#[derive(Debug)]
pub struct AbortController(watch::Sender<bool>);

impl AbortController {
    pub fn new() -> Self {
        Self(watch::channel(false).0)
    }

    /// A signal this controller aborts.
    pub fn signal(&self) -> AbortSignal {
        AbortSignal(Some(self.0.subscribe()))
    }

    /// Aborts every signal. Only the first call counts: it returns false,
    /// changing nothing, once aborted.
    pub fn abort(&self) -> bool {
        self.0
            .send_if_modified(|aborted| !std::mem::replace(aborted, true))
    }
}

impl Default for AbortController {
    fn default() -> Self {
        Self::new()
    }
}

/// Watches an [`AbortController`]. Cheap to clone. (dsh: `AbortSignal`.)
#[derive(Clone, Debug)]
pub struct AbortSignal(Option<watch::Receiver<bool>>);

impl AbortSignal {
    /// A signal already aborted. (dsh: `AbortSignal.abort()`.)
    pub fn aborted() -> Self {
        let controller = AbortController::new();
        controller.abort();
        controller.signal()
    }

    /// A signal that never aborts.
    pub fn never() -> Self {
        Self(None)
    }

    pub fn is_aborted(&self) -> bool {
        self.0.as_ref().is_some_and(|receiver| *receiver.borrow())
    }

    /// Waits until the signal aborts; forever, if its controller is dropped
    /// first.
    pub async fn wait(&self) {
        let Some(receiver) = &self.0 else {
            return std::future::pending().await;
        };
        let mut receiver = receiver.clone();
        if receiver.wait_for(|aborted| *aborted).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}
