//! The service's errors, with dsh's codes and wording. (dsh:
//! `UserQuestionError` and the messages `user-questions/src/index.ts`
//! throws.)

use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// The request was cancelled before the user answered.
pub const ASK_ABORTED: &str = "ASK_ABORTED";
/// A request with no questions.
pub const EMPTY_QUESTIONS: &str = "EMPTY_QUESTIONS";
/// The calling agent isn't the live one its registry holds.
pub const CALLER_NOT_LIVE: &str = "CALLER_NOT_LIVE";
/// The calling agent is owned by another live agent. Karaka has no child
/// agents yet, so nothing raises it; it is here for the answerers that
/// match on it.
pub const DELEGATED_CALLER: &str = "DELEGATED_CALLER";
/// A question's intent doesn't fit the question.
pub const BAD_INTENT: &str = "BAD_INTENT";
/// No answerer took the request.
pub const NO_PROVIDER: &str = "NO_PROVIDER";
/// A timed ask's window that no timer can hold.
pub const BAD_TIMEOUT: &str = "BAD_TIMEOUT";
/// A second timed ask in a call that already waits.
pub const DUPLICATE_WAIT: &str = "DUPLICATE_WAIT";
/// A timed ask's window closed. An answerer fails with it to end the wait;
/// the service turns it into a pending result.
pub const ASK_TIMED_OUT: &str = "ASK_TIMED_OUT";
/// A late reply while another for the same call is still queued.
pub const REPLY_QUEUED: &str = "REPLY_QUEUED";
/// A late reply that doesn't answer each of its call's questions once.
pub const BAD_ANSWER: &str = "BAD_ANSWER";

/// Why a cause failed, shared so errors stay cheap to clone.
pub type Cause = Arc<dyn Error + Send + Sync>;

/// A failure in the service's own terms: a code, dsh's message, and what
/// caused it, if anything. (dsh: `UserQuestionError`, a `HarnessError`.)
///
/// Answerers use it too, with codes of their own, such as `ASK_CANCELLED`
/// when the user dismisses a question.
#[derive(Clone)]
pub struct UserQuestionError {
    code: String,
    message: String,
    cause: Option<Cause>,
}

impl UserQuestionError {
    /// dsh's `name` for these errors.
    pub const NAME: &'static str = "UserQuestionError";

    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            cause: None,
        }
    }

    /// The same error, caused by `cause`.
    pub fn caused_by(mut self, cause: Cause) -> Self {
        self.cause = Some(cause);
        self
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn cause(&self) -> Option<&Cause> {
        self.cause.as_ref()
    }

    /// dsh: `abortedQuestion`.
    pub(crate) fn aborted(cause: Option<Cause>) -> Self {
        Self {
            cause,
            ..Self::new(
                ASK_ABORTED,
                "ask_user_question was aborted before the user answered",
            )
        }
    }

    pub(crate) fn caller_not_live() -> Self {
        Self::new(
            CALLER_NOT_LIVE,
            "human interaction requires the exact live calling agent when an agent is supplied",
        )
    }

    pub(crate) fn no_provider() -> Self {
        Self::new(
            NO_PROVIDER,
            "no user-questions answerer accepted the request",
        )
    }

    pub(crate) fn timed_out() -> Self {
        Self::new(
            ASK_TIMED_OUT,
            "ask_user_question timed out before the user answered",
        )
    }
}

impl fmt::Debug for UserQuestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserQuestionError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("cause", &self.cause.as_ref().map(ToString::to_string))
            .finish()
    }
}

impl fmt::Display for UserQuestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for UserQuestionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause
            .as_deref()
            .map(|cause| cause as &(dyn Error + 'static))
    }
}

/// What an ask, or an answerer, fails with: an error in the service's
/// terms, or any other, which the service passes on as it was. (dsh: a
/// rejection is a `UserQuestionError` or anything else.)
#[derive(Clone, Debug)]
pub enum QuestionError {
    Question(UserQuestionError),
    Other(Cause),
}

impl QuestionError {
    /// Wraps any other error.
    pub fn other(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Other(Arc::new(error))
    }

    /// The code, for an error in the service's terms.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Question(error) => Some(error.code()),
            Self::Other(_) => None,
        }
    }
}

impl From<UserQuestionError> for QuestionError {
    fn from(error: UserQuestionError) -> Self {
        Self::Question(error)
    }
}

impl fmt::Display for QuestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Question(error) => error.fmt(f),
            Self::Other(error) => error.fmt(f),
        }
    }
}

impl Error for QuestionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Question(error) => error.source(),
            Self::Other(error) => error.source(),
        }
    }
}
