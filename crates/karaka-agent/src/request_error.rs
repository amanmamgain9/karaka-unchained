//! The request-error hook: what a failed model request runs before its turn
//! gives up. (dsh: the `agent/request-error` waterfall, declared in
//! `agent/src/runtime-types.ts:353` and run by the loop in
//! `agent-loop/src/agent.ts:505-527`.)
//!
//! Karaka's kernel has no hooks yet, so this one is a small registry port,
//! as tools are: plugins register handlers into [`RequestErrorHooks`]
//! during their setup, and the loop runs them in order, as dsh runs a
//! waterfall. A general hook mechanism in the kernel could replace it later.
//!
//! # Why it lives here
//!
//! The hook is the agent's extension point, not the model's or the
//! session's: dsh declares it beside the `Agent` it is dispatched for. Its
//! payload uses this seam's [`Cancellation`] and the types of the seams
//! below, the model's [`RetryPolicy`] and the session's [`Failure`]. The
//! loop that runs it provides the port, and a plugin that registers a
//! handler, such as `karaka-retry`, depends only on this seam.
//!
//! # The chain
//!
//! Each handler gets the failed request and a [`Next`] that runs the rest
//! of the chain. It returns [`RequestErrorAction::Retry`] to have the
//! request tried again, or `None` to leave the failure as it is, with or
//! without asking the rest of the chain first. Handlers run in the order
//! they were registered. The end of the chain answers `None`, as dsh's
//! waterfall ends in `undefined`.
//!
//! A handler that fails, by returning an error or by panicking, ends the
//! turn with that error, and nothing is retried, as when a dsh listener
//! rejects ("does not retry when the recovery listener fails before
//! returning its action", `agent-loop/tests/request-error.spec.ts:125`).
//!
//! A cancel wins over everything: the loop stops waiting for the chain the
//! moment the turn's switch flips, and the turn ends aborted, even if a
//! handler would have asked for a retry.

use std::error::Error;
use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};

use karaka_model::RetryPolicy;
use karaka_session::{Failure, Session};

use crate::Cancellation;

/// One failed model request. (dsh: the `agent/request-error` payload.)
#[derive(Clone, Debug)]
pub struct RequestError {
    /// The session the request was for, to read and append to. Its last
    /// event is the failed call's `assistant/attempt`.
    pub session: Session,
    pub turn: u64,
    pub step: u64,
    /// The provider's own name, as the request's header names it, such as
    /// `anthropic`. dsh gives the route here.
    pub provider: String,
    /// Why the request failed, with the wait the provider asked for, if
    /// any, in `provider_retry_after_ms`.
    pub failure: Failure,
    /// How the provider wants its failed requests retried.
    pub retry_policy: RetryPolicy,
    /// The turn's cancel switch, for a handler that waits.
    pub cancel: Cancellation,
}

/// What a handler asks for. (dsh: `RequestErrorAction`, `{ kind: 'retry' }`;
/// its `undefined` is `None` here.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestErrorAction {
    /// Try the request again.
    Retry,
}

/// Why a handler failed.
pub type HandlerError = Box<dyn Error + Send + Sync>;

/// What a handler, and the rest of a chain, resolve to.
pub type HookFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<RequestErrorAction>, HandlerError>> + Send + 'a>>;

/// One handler of the request-error hook. (dsh: an `agent/request-error`
/// listener.)
///
/// Like a tool, it returns its future boxed, so handlers of different types
/// can share one list as `Arc<dyn RequestErrorHandler>`.
pub trait RequestErrorHandler: Send + Sync {
    /// Decides about `error`. Call `next.run()` to ask the rest of the chain
    /// first, or don't, to decide alone.
    fn handle<'a>(&'a self, error: &'a RequestError, next: Next<'a>) -> HookFuture<'a>;
}

/// The rest of a chain, after the handler it was given to.
pub struct Next<'a> {
    rest: &'a [Arc<dyn RequestErrorHandler>],
    error: &'a RequestError,
}

impl<'a> Next<'a> {
    /// Runs the rest of the chain, which answers `None` once it is empty.
    pub fn run(self) -> HookFuture<'a> {
        match self.rest.split_first() {
            Some((first, rest)) => first.handle(
                self.error,
                Next {
                    rest,
                    error: self.error,
                },
            ),
            None => Box::pin(ready(Ok(None))),
        }
    }
}

/// The port the request-error handlers are registered in.
///
/// The loop provides it, so a plugin that registers a handler requires it,
/// and starts after the loop's plugin.
///
/// # Locking
///
/// One `RwLock` guards the list, held only to add a handler or to copy the
/// list out when a chain starts. Handlers registered while a chain runs
/// join the next one.
#[derive(Default)]
pub struct RequestErrorHooks {
    handlers: RwLock<Vec<Arc<dyn RequestErrorHandler>>>,
}

impl RequestErrorHooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `handler` at the end of the chain.
    pub fn register(&self, handler: Arc<dyn RequestErrorHandler>) {
        self.handlers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(handler);
    }

    /// How many handlers there are.
    pub fn len(&self) -> usize {
        self.handlers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Runs the whole chain for `error`, first handler first.
    pub fn run<'a>(&self, error: &'a RequestError) -> HookFuture<'a> {
        // A copy of the list, so no lock is held while handlers run.
        let handlers = self
            .handlers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Box::pin(async move {
            Next {
                rest: &handlers,
                error,
            }
            .run()
            .await
        })
    }
}

impl fmt::Debug for RequestErrorHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestErrorHooks")
            .field("handlers", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use karaka_model::RetryPolicy;
    use karaka_session::{Failure, SessionId, SessionStore};

    use super::{
        HookFuture, Next, RequestError, RequestErrorAction, RequestErrorHandler, RequestErrorHooks,
    };
    use crate::cancellation;

    fn error() -> RequestError {
        RequestError {
            session: SessionStore::new().open_or_create(SessionId::new("s")),
            turn: 1,
            step: 1,
            provider: "mock".into(),
            failure: Failure {
                message: "busy".into(),
                code: "RATE_LIMIT".into(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
                offload_images: None,
            },
            retry_policy: RetryPolicy::default(),
            cancel: cancellation().1,
        }
    }

    /// Notes its name, then either asks the rest of the chain or answers.
    struct Named {
        name: &'static str,
        seen: Arc<Mutex<Vec<&'static str>>>,
        answer: Option<Option<RequestErrorAction>>,
    }

    impl RequestErrorHandler for Named {
        fn handle<'a>(&'a self, _error: &'a RequestError, next: Next<'a>) -> HookFuture<'a> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(self.name);
                match self.answer {
                    Some(answer) => Ok(answer),
                    None => next.run().await,
                }
            })
        }
    }

    #[tokio::test]
    async fn an_empty_chain_answers_none() {
        let hooks = RequestErrorHooks::new();
        assert!(hooks.is_empty());
        assert_eq!(hooks.run(&error()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn handlers_run_in_order_until_one_answers() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hooks = RequestErrorHooks::new();
        for (name, answer) in [
            ("first", None),
            ("second", Some(Some(RequestErrorAction::Retry))),
            ("third", Some(None)),
        ] {
            hooks.register(Arc::new(Named {
                name,
                seen: Arc::clone(&seen),
                answer,
            }));
        }
        assert_eq!(
            hooks.run(&error()).await.unwrap(),
            Some(RequestErrorAction::Retry)
        );
        assert_eq!(*seen.lock().unwrap(), ["first", "second"]);
    }
}
