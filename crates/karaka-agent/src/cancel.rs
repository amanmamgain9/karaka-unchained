//! A turn's cancel switch, and the side that watches it. (dsh: the turn's
//! `AbortController` and its `AbortSignal`.)
//!
//! It is part of the seam so a hook, such as the request-error hook, can
//! notice a cancel while it waits. Work that only needs to stop doesn't
//! watch it: the loop drops that work's future when the switch flips.

use karaka_session::CancelCause;
use tokio::sync::watch;

/// A new switch, and the side that watches it.
pub fn cancellation() -> (CancelSwitch, Cancellation) {
    let (sender, receiver) = watch::channel(None);
    (CancelSwitch(sender), Cancellation(receiver))
}

/// Flips once, with a cause. (dsh: `AbortController.abort(cause)`.)
#[derive(Debug)]
pub struct CancelSwitch(watch::Sender<Option<CancelCause>>);

impl CancelSwitch {
    /// Cancels with `cause`. Only the first cause counts, as in dsh:
    /// returns false, changing nothing, if the switch has already flipped.
    pub fn cancel(&self, cause: CancelCause) -> bool {
        self.0.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(cause);
            true
        })
    }
}

/// Watches a [`CancelSwitch`]. Cheap to clone; every clone sees the same
/// switch. (dsh: `AbortSignal`.)
#[derive(Clone, Debug)]
pub struct Cancellation(watch::Receiver<Option<CancelCause>>);

impl Cancellation {
    /// The cause, once the switch has flipped. (dsh: `signal.aborted` and
    /// `signal.reason`.)
    pub fn cause(&self) -> Option<CancelCause> {
        self.0.borrow().clone()
    }

    /// Whether the switch has flipped.
    pub fn is_cancelled(&self) -> bool {
        self.0.borrow().is_some()
    }

    /// Waits for the switch to flip, and gives the cause. Never resolves if
    /// the switch is dropped unflipped.
    pub async fn cancelled(&self) -> CancelCause {
        let mut receiver = self.0.clone();
        // Cloned out in its own statement, so the lock `wait_for` holds
        // isn't kept across the `.await` below, and the future stays `Send`.
        let cause = receiver
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|cause| cause.clone());
        match cause {
            Some(cause) => cause,
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use karaka_session::CancelCause;

    use super::cancellation;

    #[tokio::test]
    async fn the_first_cause_wins_and_every_watcher_sees_it() {
        let (switch, watching) = cancellation();
        let other = watching.clone();
        assert!(!watching.is_cancelled());
        assert!(switch.cancel(CancelCause::User));
        assert!(!switch.cancel(CancelCause::Parent));
        assert_eq!(watching.cause(), Some(CancelCause::User));
        assert_eq!(other.cancelled().await, CancelCause::User);
    }
}
