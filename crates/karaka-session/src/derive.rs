//! Building the model's history from a session's events.
//! (dsh: `deriveMessages` in `index.ts` and `deriveEventMessage` in
//! `surface.ts`.)
//!
//! dsh walks its "surface": the events that produce messages, in order.
//! Events join the surface by being appended, or by replacing a range of
//! earlier ones, which is how compaction shortens a history without deleting
//! anything. Karaka has no replacing yet, so its surface is simply every
//! event in log order, and this walk reads the log directly. When compaction
//! arrives, the surface order will be worked out first and this walk will
//! read that instead.
//!
//! The rules, and how each compares with dsh:
//!
//! - A user message becomes one user message, as logged. The same holds
//!   right after tool results: the text is its own message after the
//!   results' message, not merged into it. dsh does the same.
//! - An assistant message becomes one assistant message. One with no content
//!   is left out, as in dsh, where it exists only to record a call's usage:
//!   a provider would refuse an empty assistant turn.
//! - Tool results in a row become one user message. dsh's derivation keeps
//!   one message per result, and its provider adapters group them on the
//!   wire. Karaka's `Message` can hold many results, and Anthropic wants a
//!   round's results together in one message, so grouping happens here,
//!   where every provider gets it.
//! - A call with no result gets a failed one, added after the results that
//!   did arrive. That's dsh's crash repair, applied to the derived history
//!   only; see `repair.rs`.
//!
//! Nothing here checks that the log makes sense, such as a result for a call
//! nobody made. In dsh a separate invariant checker does that
//! (`invariant.ts`). Such events are passed through as they are.

use std::mem;

use karaka_model::{Block, Message, Role};

use crate::event::{Entry, SessionEvent};
use crate::repair::interrupted_result;

pub(crate) fn messages(entries: &[Entry]) -> Vec<Message> {
    let mut history = History::default();
    for entry in entries {
        match &entry.event {
            SessionEvent::UserMessage { content } => {
                history.end_tool_round();
                history.push(Role::User, content.clone());
            }
            SessionEvent::AssistantMessage { content, .. } => {
                history.end_tool_round();
                // dsh: an empty `assistant/message` derives to no message.
                if content.is_empty() {
                    continue;
                }
                history.unanswered = content
                    .iter()
                    .filter_map(|block| match block {
                        Block::ToolCall(call) => Some(call.id.clone()),
                        _ => None,
                    })
                    .collect();
                history.push(Role::Assistant, content.clone());
            }
            SessionEvent::ToolResult(result) => {
                history.unanswered.retain(|id| *id != result.call_id);
                history.results.push(Block::ToolResult(result.clone()));
            }
        }
    }
    // The log can end mid-round: after the calls, or partway through their
    // results. That round is closed here too.
    history.end_tool_round();
    history.messages
}

/// The history built so far, and the tool round still open in it.
#[derive(Default)]
struct History {
    messages: Vec<Message>,
    /// Results seen since the last message, waiting to become one message.
    results: Vec<Block>,
    /// The last assistant message's calls that have no result yet, in the
    /// order the model made them.
    unanswered: Vec<String>,
}

impl History {
    fn push(&mut self, role: Role, content: Vec<Block>) {
        self.messages.push(Message { role, content });
    }

    /// Called when something other than a tool result comes next, or the log
    /// ends. Any call still unanswered was interrupted, so it gets a failed
    /// result. Then all of the round's results go out as one user message.
    fn end_tool_round(&mut self) {
        let interrupted = self
            .unanswered
            .drain(..)
            .map(|id| Block::ToolResult(interrupted_result(id)));
        self.results.extend(interrupted);
        if !self.results.is_empty() {
            let results = mem::take(&mut self.results);
            self.push(Role::User, results);
        }
    }
}

#[cfg(test)]
mod tests {
    use karaka_model::{Block, Message, Role, StopReason, ToolCall, ToolResult, Usage};

    use crate::{INTERRUPTED_TOOL_RESULT, SessionEvent, SessionLog};

    fn answer(text: &str) -> SessionEvent {
        SessionEvent::AssistantMessage {
            content: vec![Block::Text(text.into())],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn call(id: &str) -> Block {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "clock".into(),
            arguments: "{}".into(),
        })
    }

    fn calls(blocks: Vec<Block>) -> SessionEvent {
        SessionEvent::AssistantMessage {
            content: blocks,
            stop: StopReason::ToolCalls,
            usage: Usage::default(),
        }
    }

    fn result(call_id: &str, content: &str) -> ToolResult {
        ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    fn message(role: Role, content: Vec<Block>) -> Message {
        Message { role, content }
    }

    fn log_of(events: Vec<SessionEvent>) -> SessionLog {
        let mut log = SessionLog::new();
        for event in events {
            log.append(event);
        }
        log
    }

    #[test]
    fn a_plain_exchange_derives_one_message_per_event() {
        let log = log_of(vec![
            SessionEvent::user("hi"),
            answer("Hello"),
            SessionEvent::user("bye"),
            answer("Goodbye"),
        ]);
        assert_eq!(
            log.messages(),
            [
                Message::user("hi"),
                message(Role::Assistant, vec![Block::Text("Hello".into())]),
                Message::user("bye"),
                message(Role::Assistant, vec![Block::Text("Goodbye".into())]),
            ]
        );
    }

    #[test]
    fn one_tool_round_trip_puts_the_result_in_a_user_message() {
        let log = log_of(vec![
            SessionEvent::user("what time is it?"),
            calls(vec![Block::Text("Let me check.".into()), call("call_1")]),
            SessionEvent::ToolResult(result("call_1", "12:00")),
            answer("It's noon."),
        ]);
        assert_eq!(
            log.messages(),
            [
                Message::user("what time is it?"),
                message(
                    Role::Assistant,
                    vec![Block::Text("Let me check.".into()), call("call_1")]
                ),
                message(
                    Role::User,
                    vec![Block::ToolResult(result("call_1", "12:00"))]
                ),
                message(Role::Assistant, vec![Block::Text("It's noon.".into())]),
            ]
        );
    }

    #[test]
    fn two_parallel_results_become_one_user_message() {
        let log = log_of(vec![
            SessionEvent::user("time in two places?"),
            calls(vec![call("call_1"), call("call_2")]),
            // The second call finished first, so its result was logged first.
            SessionEvent::ToolResult(result("call_2", "18:00")),
            SessionEvent::ToolResult(result("call_1", "12:00")),
            answer("Noon and six."),
        ]);
        let messages = log.messages();
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[2],
            message(
                Role::User,
                vec![
                    Block::ToolResult(result("call_2", "18:00")),
                    Block::ToolResult(result("call_1", "12:00")),
                ]
            )
        );
    }

    #[test]
    fn a_user_message_after_tool_results_stays_its_own_message() {
        // dsh keeps them apart too: the results' message, then the text.
        let log = log_of(vec![
            SessionEvent::user("what time is it?"),
            calls(vec![call("call_1")]),
            SessionEvent::ToolResult(result("call_1", "12:00")),
            SessionEvent::user("and in Tokyo?"),
        ]);
        assert_eq!(
            log.messages()[2..],
            [
                message(
                    Role::User,
                    vec![Block::ToolResult(result("call_1", "12:00"))]
                ),
                Message::user("and in Tokyo?"),
            ]
        );
    }

    #[test]
    fn an_empty_assistant_message_is_left_out() {
        let log = log_of(vec![
            SessionEvent::user("hi"),
            SessionEvent::AssistantMessage {
                content: Vec::new(),
                stop: StopReason::MaxTokens,
                usage: Usage::default(),
            },
        ]);
        assert_eq!(log.messages(), [Message::user("hi")]);
    }

    fn interrupted(call_id: &str) -> Block {
        Block::ToolResult(ToolResult {
            call_id: call_id.into(),
            content: INTERRUPTED_TOOL_RESULT.into(),
            is_error: true,
        })
    }

    #[test]
    fn calls_left_unanswered_at_the_end_get_interrupted_results() {
        // The turn stopped after one of two results was logged.
        let log = log_of(vec![
            SessionEvent::user("time in two places?"),
            calls(vec![call("call_1"), call("call_2")]),
            SessionEvent::ToolResult(result("call_1", "12:00")),
        ]);
        let before = log.events().to_vec();

        let messages = log.messages();
        assert_eq!(
            messages.last(),
            Some(&message(
                Role::User,
                vec![
                    Block::ToolResult(result("call_1", "12:00")),
                    interrupted("call_2"),
                ]
            ))
        );
        // The answer exists only in the derived history.
        assert_eq!(log.events(), before);
    }

    #[test]
    fn an_interrupted_round_inside_the_log_is_answered_before_what_follows() {
        // No result was logged before the user wrote again.
        let log = log_of(vec![
            SessionEvent::user("what time is it?"),
            calls(vec![call("call_1")]),
            SessionEvent::user("hello?"),
            answer("Sorry, the clock was interrupted."),
        ]);
        let messages = log.messages();
        assert_eq!(
            messages[1..3],
            [
                message(Role::Assistant, vec![call("call_1")]),
                message(Role::User, vec![interrupted("call_1")]),
            ]
        );
        assert_eq!(messages[3], Message::user("hello?"));
        assert_eq!(log.len(), 4);
    }

    #[test]
    fn every_call_in_the_derived_history_has_a_result_right_after_it() {
        // The rule providers enforce, checked over the repaired history.
        let log = log_of(vec![
            SessionEvent::user("go"),
            calls(vec![call("a"), call("b")]),
            SessionEvent::ToolResult(result("b", "done")),
            calls(vec![call("c")]),
        ]);
        let messages = log.messages();
        for (index, message) in messages.iter().enumerate() {
            for block in &message.content {
                let Block::ToolCall(call) = block else {
                    continue;
                };
                let next = &messages[index + 1];
                assert_eq!(next.role, Role::User);
                assert!(
                    next.content
                        .iter()
                        .any(|block| matches!(block, Block::ToolResult(r) if r.call_id == call.id))
                );
            }
        }
    }
}
