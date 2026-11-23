//! A failed model call, or a log that refused an event, as the [`Failure`]
//! a turn ends with. (dsh: the `catch` in `ReactLoopAgent.turn`,
//! `agent-loop/src/agent.ts:375-382`.)

use std::error::Error;

use karaka_model::ModelError;
use karaka_session::Failure;

/// dsh's code for a failure that isn't a model call's (`agent.ts:381`).
const UNKNOWN: &str = "UNKNOWN";

/// dsh's code for a route no model is registered under
/// (`llm/src/index.ts:1018`).
const NO_ADAPTER: &str = "NO_ADAPTER";

/// A failed model call, with its kind as one of dsh's error codes
/// ([`ErrorKind::code`](karaka_model::ErrorKind::code)). dsh keeps
/// an `LlmError`'s own facts (`error.failure`); a [`ModelError`] carries the
/// message and the wait the provider asked for, but no HTTP status or
/// request id.
pub(crate) fn model_failure(error: &ModelError) -> Failure {
    Failure {
        message: error.message.clone(),
        code: error.kind.code().into(),
        status: None,
        provider_retry_after_ms: error
            .retry_after
            .map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX))
            .filter(|&millis| millis > 0)
            .map(Into::into),
        request_id: None,
        offload_images: None,
    }
}

/// Anything else that ends a turn, such as the log refusing an event. dsh
/// flattens such an error to its text under `UNKNOWN`.
pub(crate) fn other_failure(error: &dyn Error) -> Failure {
    plain(error.to_string(), UNKNOWN)
}

/// No model is registered under the loop's route. dsh's wording names its
/// adapter registry; this names Karaka's routes, under dsh's code.
pub(crate) fn no_model(route: &str) -> Failure {
    plain(
        format!("no model is registered under the route \"{route}\""),
        NO_ADAPTER,
    )
}

/// A request-error handler panicked. Like any other failure outside a
/// model call, it is `UNKNOWN`.
pub(crate) fn handler_panicked() -> Failure {
    plain("a request-error handler panicked".into(), UNKNOWN)
}

/// A model's stream panicked. dsh ends a turn whose stream throws with the
/// error's text under `UNKNOWN`; this is the panic's message.
pub(crate) fn panicked(message: String) -> Failure {
    plain(message, UNKNOWN)
}

fn plain(message: String, code: &str) -> Failure {
    Failure {
        message,
        code: code.into(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
        offload_images: None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use karaka_model::{ErrorKind, ModelError};

    use super::model_failure;

    #[test]
    fn a_failure_keeps_the_message_and_the_wait() {
        let mut error = ModelError::new(ErrorKind::RateLimited, "Slow down.");
        error.retry_after = Some(Duration::from_millis(1500));
        let failure = model_failure(&error);
        assert_eq!(failure.message, "Slow down.");
        assert_eq!(failure.code, "RATE_LIMIT");
        assert_eq!(failure.provider_retry_after_ms, Some(1500.into()));
        assert_eq!(failure.status, None);
    }

    /// dsh writes a wait only when it is more than nothing.
    #[test]
    fn a_zero_wait_is_left_out() {
        let mut error = ModelError::new(ErrorKind::RateLimited, "Slow down.");
        error.retry_after = Some(Duration::ZERO);
        assert_eq!(model_failure(&error).provider_retry_after_ms, None);
    }
}
