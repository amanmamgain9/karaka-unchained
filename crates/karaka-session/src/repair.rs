//! Answers for tool calls that never got a result.
//! (dsh: `interruptedTurnClosers` in `repair.ts`.)
//!
//! A turn can stop between the model asking for tools and the results being
//! logged: the process crashes, or the turn is cancelled. The log then holds
//! an assistant message whose calls have no results. Providers refuse a
//! history like that, so the session could never be continued.
//!
//! dsh and Karaka fix it with the same answers, in the same place: a failed
//! result for each unanswered call, right after the calls' own results. They
//! differ in where the answers live:
//!
//! - dsh repairs a log when a persistence backend loads it after a crash, and
//!   appends the answers to the log durably, with a closing `turn/end`.
//! - Karaka has no load path yet, so [`SessionLog::messages`] adds the
//!   answers to the derived history only. The log is never changed.
//!
//! The two agree. When persistence arrives, its load path can append
//! [`interrupted_result`] for each unanswered call, as dsh does. After that
//! the calls are answered in the log, so derivation adds nothing and the
//! model sees the same history either way.
//!
//! [`SessionLog::messages`]: crate::SessionLog::messages

use karaka_model::ToolResult;

/// What the model reads for a call that was interrupted.
///
/// This is dsh's `TOOL_OUTCOME_UNKNOWN` text. dsh has a second, milder text,
/// `TOOL_NOT_STARTED`, for a call it knows never started, because no
/// `tool/call` event was logged for it. Karaka doesn't log `tool/call` yet,
/// so it can't know that, and it always says the outcome is unknown. Telling
/// the model a call never ran when it might have would invite a blind retry
/// of something with side effects.
pub const INTERRUPTED_TOOL_RESULT: &str = "The tool call was interrupted after it was recorded, \
    but no result was durably recorded. Its outcome is unknown. Decide whether to retry from \
    the tool semantics: retry only if the operation is read-only or idempotent; if it may have \
    side effects, first verify external state or ask the user. Do not retry blindly.";

/// A failed result for the call `call_id`, which was interrupted before it
/// returned.
pub fn interrupted_result(call_id: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        content: INTERRUPTED_TOOL_RESULT.into(),
        is_error: true,
    }
}
