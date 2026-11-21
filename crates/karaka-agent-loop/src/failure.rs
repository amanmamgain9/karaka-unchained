//! A failed model call, or a log that refused an event, as the [`Failure`]
//! a turn ends with. (dsh: the `catch` in `ReactLoopAgent.turn`,
//! `agent-loop/src/agent.ts:375-382`.)

use std::error::Error;

use karaka_model::{ErrorKind, ModelError};
use karaka_session::Failure;

/// dsh's code for a failure that isn't a model call's (`agent.ts:381`).
const UNKNOWN: &str = "UNKNOWN";

/// dsh's code for a route no model is registered under
/// (`llm/src/index.ts:1018`).
const NO_ADAPTER: &str = "NO_ADAPTER";

/// A failed model call, with its kind as one of dsh's error codes. dsh keeps
/// an `LlmError`'s own facts (`error.failure`); a [`ModelError`] carries the
/// message and the wait the provider asked for, but no HTTP status or
/// request id.
pub(crate) fn model_failure(error: &ModelError) -> Failure {
    Failure {
        message: error.message.clone(),
        code: code(error.kind).into(),
        status: None,
        provider_retry_after_ms: error
            .retry_after
            .map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX))
            .filter(|&millis| millis > 0),
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

/// dsh's code for each kind, as `ErrorKind`'s table in `karaka-model` maps
/// them. Where a kind stands for several codes, this is the one dsh's
/// adapters write most.
///
/// A refusal has no dsh code: pi-ai reports one as a plain error, which dsh
/// files as `PI_AI_ERROR`, a code that names an adapter Karaka doesn't
/// have. Karaka writes `REFUSED`, so a reader can tell a refusal from a
/// breakage.
fn code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Auth => "AUTH",
        ErrorKind::Quota => "QUOTA",
        ErrorKind::InvalidRequest => "INVALID_REQUEST",
        ErrorKind::ContextWindowExceeded => "CONTEXT_WINDOW_EXCEEDED",
        ErrorKind::Refused => "REFUSED",
        ErrorKind::Malformed => "MALFORMED_RESPONSE",
        ErrorKind::EmptyResponse => "EMPTY_RESPONSE",
        ErrorKind::RateLimited => "RATE_LIMIT",
        ErrorKind::Unavailable => "SERVER",
        ErrorKind::Network => "TRANSPORT",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use karaka_model::{ErrorKind, ModelError};

    use super::{code, model_failure};

    #[test]
    fn each_kind_gets_the_code_dsh_gives_it() {
        let codes = [
            (ErrorKind::Auth, "AUTH"),
            (ErrorKind::Quota, "QUOTA"),
            (ErrorKind::InvalidRequest, "INVALID_REQUEST"),
            (ErrorKind::ContextWindowExceeded, "CONTEXT_WINDOW_EXCEEDED"),
            (ErrorKind::Malformed, "MALFORMED_RESPONSE"),
            (ErrorKind::EmptyResponse, "EMPTY_RESPONSE"),
            (ErrorKind::RateLimited, "RATE_LIMIT"),
            (ErrorKind::Unavailable, "SERVER"),
            (ErrorKind::Network, "TRANSPORT"),
            (ErrorKind::Refused, "REFUSED"),
        ];
        for (kind, expected) in codes {
            assert_eq!(code(kind), expected, "{kind:?}");
        }
    }

    #[test]
    fn a_failure_keeps_the_message_and_the_wait() {
        let mut error = ModelError::new(ErrorKind::RateLimited, "Slow down.");
        error.retry_after = Some(Duration::from_millis(1500));
        let failure = model_failure(&error);
        assert_eq!(failure.message, "Slow down.");
        assert_eq!(failure.code, "RATE_LIMIT");
        assert_eq!(failure.provider_retry_after_ms, Some(1500));
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
