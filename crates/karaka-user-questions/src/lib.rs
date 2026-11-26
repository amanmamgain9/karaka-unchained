//! Questions an agent asks the user, and the answers that come back.
//! (dsh: `packages/interaction/user-questions`, `dsh-user-questions`.)
//!
//! The service, [`UserQuestions`], puts a request to a chain of answerers,
//! the UIs that show it, and waits for the user's answer (`service.rs`). A
//! timed ask waits only for a window, which an attached UI holds open
//! (`timed_wait.rs`); a question whose window closed stays answerable, and
//! a late reply is steered into the agent. The `userQuestions` projection
//! folds the session's `ask_user_question` calls into the questions the
//! user can still answer, for every client to show (`projection.rs`).
//!
//! The model-facing `ask_user_question` tool, and the answerers, are other
//! crates' (dsh: `dsh-tool-ask-user`, and the UI packages).

mod error;
mod plugin;
mod projection;
mod service;
mod timed_wait;
mod types;

pub use error::{
    ASK_ABORTED, ASK_TIMED_OUT, BAD_ANSWER, BAD_INTENT, BAD_TIMEOUT, CALLER_NOT_LIVE, Cause,
    DELEGATED_CALLER, DUPLICATE_WAIT, EMPTY_QUESTIONS, NO_PROVIDER, QuestionError, REPLY_QUEUED,
    UserQuestionError,
};
pub use plugin::UserQuestionsPlugin;
pub use projection::{
    ASK_USER_QUESTION_TOOL, TIMED_WAIT_PARAMETER, UserQuestionFold, UserQuestionsProjection,
    UserQuestionsState, apply_user_question_event, fold_user_questions,
    is_timed_ask_user_question_schema, questions_of,
};
pub use service::{
    AnswerFuture, AnswerNext, AnswererRegistration, MAX_TIMEOUT_MS, QuestionAnswerer, UserQuestions,
};
pub use timed_wait::{TimedQuestionWait, WaitAttachment, WaitClaim, WaitEnd};
pub use types::{
    AskUserQuestionAnswer, AskUserQuestionAnswerItem, AskUserQuestionIntent, AskUserQuestionItem,
    AskUserQuestionOption, PendingUserQuestion, QuestionRequest, QuestionWait, REPLY_SOURCE_KIND,
    SettledUserQuestion, TimedUserQuestionResult, UserQuestionProjectionView, UserQuestionState,
    reply_call_id, reply_source,
};
