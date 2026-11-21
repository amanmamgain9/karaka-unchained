//! Building the model's history from a session's events.
//! (dsh v4: `deriveMessages` in `index.ts` and `deriveEventMessage` in
//! `surface.ts`, then the pi-ai adapter's `toPiContext` in
//! `llm-pi-ai/src/context.ts`.)
//!
//! Derivation walks the surface (see `surface.rs`), so only surface events
//! count, in surface order. Lifecycle and passthrough events never change
//! the result, and neither does a range a compaction replaced. Derivation is
//! pure: it never adds anything the log doesn't hold. Closing calls that a
//! crash left open is repair's job, and repair appends its closers to the
//! log (see `repair.rs`).
//!
//! # System and developer messages
//!
//! v4 logs the system prompt as surface node 0, and both system and
//! developer messages are messages in its history. Its pi-ai adapter folds
//! only the *leading* run of system messages into the request's system
//! prompt (`splitSystemPrompt`, `llm-pi-ai/src/context.ts:148-158`). Every
//! system or developer message after that keeps its place
//! (`context.ts:224-231`). Derivation does the same:
//!
//! - The leading run is every system message before the first message of
//!   any other role. Their text, each one's blocks joined with nothing
//!   between them (dsh's `flattenText`), becomes [`History::system`], the
//!   non-empty parts joined with a blank line. No parts means no system
//!   prompt.
//! - A later system message becomes a [`Role::System`] message holding that
//!   text, even when it is empty. Which providers can take it in place,
//!   and what to send the others, is each provider's call.
//! - A developer message becomes a [`Role::Developer`] message holding its
//!   non-empty text blocks (pi-ai's `appendDeveloper`). It also ends the
//!   leading run.
//!
//! A system or developer message with no content at all derives to nothing,
//! as in v4 (`surface.ts:135-143`), and doesn't end the leading run.
//!
//! A developer message can also add and remove tools. v4 sends those changes
//! where they happened (`context.ts:171-201`), but Karaka's history has no
//! block for them, and its request lists the tools in effect itself. So
//! they add nothing here. A developer message with only tool changes still
//! ends the leading run, as in v4, but leaves no message.
//!
//! A system or developer message logged while a tool round is open, after
//! the calls and before the next user or assistant message, comes after that
//! round's results. pi-ai moves it there too (`transform-messages.js:130-
//! 183`): providers want a tool's results right after its call.
//!
//! # User, assistant and tool messages
//!
//! - A user message becomes one user message, even with no content, as in
//!   v4. A user message right after tool results stays its own message,
//!   after the results' one.
//! - An assistant message becomes one assistant message. One with no
//!   content is left out, as in v4: it exists to carry a step's usage, and a
//!   provider would refuse an empty assistant turn.
//! - Tool results in a row become one user message, in surface order. v4
//!   keeps one message per result and its adapters group them on the wire;
//!   Karaka groups them here, so every provider gets a round's results
//!   together, as Anthropic wants. An empty assistant message between two
//!   results doesn't split them, since it reaches no history, and neither
//!   does a system or developer message, which waits until after them (see
//!   above).

use std::mem;

use karaka_model::{Block, Message, Role};

use crate::convert::joined_text;
use crate::event::{Entry, Seq, SessionEvent};
use crate::message::ContentBlock;

/// What a model call needs from a session: the system prompt and the
/// conversation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    /// The system prompt the conversation starts with. Goes in
    /// [`Request::system`](karaka_model::Request::system).
    pub system: Option<String>,
    /// The conversation, with any later system and developer messages in
    /// their places.
    pub messages: Vec<Message>,
}

/// The history of `entries`, read at the surface `nodes`.
pub(crate) fn history(entries: &[Entry], nodes: &[Seq]) -> History {
    let mut builder = Builder::default();
    for seq in nodes {
        // A node is always the seq of an admitted event, which is its index.
        let entry = &entries[seq.0 as usize];
        match &entry.event {
            // Empty content derives to no message (`surface.ts:138-141`).
            SessionEvent::SystemMessage { data, .. } if data.message.content.is_empty() => {}
            SessionEvent::SystemMessage { data, .. } => {
                builder.system(joined_text(&data.message.content, ""));
            }
            SessionEvent::DeveloperMessage { data, .. } if data.message.content.is_empty() => {}
            SessionEvent::DeveloperMessage { data, .. } => {
                builder.developer(developer_texts(&data.message.content));
            }
            SessionEvent::UserMessage { message, .. } => {
                builder.push(Role::User, message.blocks());
            }
            SessionEvent::AssistantMessage { data, .. } => {
                if !data.message.content.is_empty() {
                    builder.push(Role::Assistant, data.message.blocks());
                }
            }
            SessionEvent::ToolResult { data, .. } => {
                builder.leading = false;
                builder
                    .results
                    .push(Block::ToolResult(data.message.to_model()));
            }
            // Only surface events are ever on the surface.
            _ => {}
        }
    }
    builder.finish()
}

/// pi-ai's `appendDeveloper` keeps the non-empty text blocks
/// (`context.ts:177-179`).
fn developer_texts(content: &[ContentBlock]) -> Vec<Block> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) if !text.text.is_empty() => {
                Some(Block::Text(text.text.clone()))
            }
            _ => None,
        })
        .collect()
}

struct Builder {
    /// The text of each system message in the leading run.
    prompt: Vec<String>,
    /// Whether every message so far was a system message, so that the next
    /// system message still belongs to the prompt.
    leading: bool,
    messages: Vec<Message>,
    /// Results seen since the last message, waiting to become one message.
    results: Vec<Block>,
    /// Whether the last assistant message called tools, and no user or
    /// assistant message has come since. pi-ai's `pendingToolCalls`.
    round_open: bool,
    /// System and developer messages that came while the round was open.
    /// They go out after its results.
    held: Vec<Message>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            prompt: Vec::new(),
            leading: true,
            messages: Vec::new(),
            results: Vec::new(),
            round_open: false,
            held: Vec::new(),
        }
    }
}

impl Builder {
    fn system(&mut self, text: String) {
        if self.leading {
            self.prompt.push(text);
        } else {
            self.instruction(Message {
                role: Role::System,
                content: vec![Block::Text(text)],
            });
        }
    }

    fn developer(&mut self, content: Vec<Block>) {
        self.leading = false;
        if !content.is_empty() {
            self.instruction(Message {
                role: Role::Developer,
                content,
            });
        }
    }

    /// A system or developer message after the leading run.
    fn instruction(&mut self, message: Message) {
        if self.round_open {
            self.held.push(message);
        } else {
            self.end_tool_round();
            self.messages.push(message);
        }
    }

    fn push(&mut self, role: Role, content: Vec<Block>) {
        self.leading = false;
        self.end_tool_round();
        self.release_held();
        self.round_open =
            role == Role::Assistant && content.iter().any(|b| matches!(b, Block::ToolCall(_)));
        self.messages.push(Message { role, content });
    }

    /// Sends the round's results out as one user message.
    fn end_tool_round(&mut self) {
        if !self.results.is_empty() {
            let content = mem::take(&mut self.results);
            self.messages.push(Message {
                role: Role::User,
                content,
            });
        }
    }

    fn release_held(&mut self) {
        self.messages.append(&mut self.held);
    }

    fn finish(mut self) -> History {
        self.end_tool_round();
        self.release_held();
        let prompt: Vec<String> = self.prompt.into_iter().filter(|p| !p.is_empty()).collect();
        History {
            system: (!prompt.is_empty()).then(|| prompt.join("\n\n")),
            messages: self.messages,
        }
    }
}

#[cfg(test)]
mod tests {
    use karaka_model::{Block, Message, Role, ToolCall, ToolResult};
    use serde_json::json;

    use crate::data::{AssistantMessageData, DeveloperMessageData, ToolResultData};
    use crate::message::{
        AssistantMessage, ContentBlock, DeveloperMessage, DeveloperRole, Extra, SystemMessage,
        UserMessage, UserSource,
    };
    use crate::{Seq, SessionEvent, SessionLog, SurfaceIntent, SurfaceOp, TurnEndReason};

    fn user(text: &str) -> SessionEvent {
        SessionEvent::user(UserMessage::text(
            format!("user-{text}"),
            UserSource::user(),
            text,
        ))
    }

    fn assistant(content: Vec<Block>) -> SessionEvent {
        SessionEvent::assistant(AssistantMessageData {
            turn: 1,
            step: 1,
            message: AssistantMessage::from_model("assistant", "mock", "mock", &content),
            stream: Vec::new(),
            usage: None,
            interrupted: false,
            extra: Extra::new(),
        })
    }

    fn answer(text: &str) -> SessionEvent {
        assistant(vec![Block::Text(text.into())])
    }

    fn call(id: &str) -> Block {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "clock".into(),
            arguments: "{}".into(),
        })
    }

    fn result(call_id: &str, content: &str) -> ToolResult {
        ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    fn result_event(call_id: &str, content: &str) -> SessionEvent {
        SessionEvent::ToolResult {
            data: ToolResultData::new(format!("result-{call_id}"), 1, 1, &result(call_id, content)),
            surface: SurfaceIntent::append(),
        }
    }

    fn system(text: &str) -> SessionEvent {
        SessionEvent::system(1, 1, SystemMessage::new(format!("system-{text}"), text))
    }

    fn developer(content: Vec<ContentBlock>) -> SessionEvent {
        SessionEvent::DeveloperMessage {
            data: DeveloperMessageData {
                turn: 1,
                step: 1,
                message: DeveloperMessage {
                    id: "developer".into(),
                    role: DeveloperRole::Developer,
                    content,
                    source: json!({ "kind": "tool-updates" }),
                    extra: Extra::new(),
                },
                header_seq: None,
                extra: Extra::new(),
            },
            surface: SurfaceIntent::append(),
        }
    }

    fn replace(event: SessionEvent, start: u64, end: u64, sources: &[u64]) -> SessionEvent {
        let surface = SurfaceIntent {
            op: SurfaceOp::Replace {
                start: Seq(start),
                end: Seq(end),
            },
            sources: sources.iter().copied().map(Seq).collect(),
        };
        match event {
            SessionEvent::UserMessage { message, .. } => {
                SessionEvent::UserMessage { message, surface }
            }
            SessionEvent::SystemMessage { data, .. } => {
                SessionEvent::SystemMessage { data, surface }
            }
            other => panic!("not a replaceable test event: {other:?}"),
        }
    }

    fn message(role: Role, content: Vec<Block>) -> Message {
        Message { role, content }
    }

    fn log_of(events: Vec<SessionEvent>) -> SessionLog {
        let mut log = SessionLog::new();
        for event in events {
            log.append(event).expect("the test event is valid");
        }
        log
    }

    fn attempt() -> SessionEvent {
        SessionEvent::Passthrough(crate::Passthrough {
            event_type: "assistant/attempt".into(),
            data: json!({ "turn": 1, "step": 1, "stream": [] }),
            surface_op: None,
            source_event_seqs: None,
        })
    }

    // dsh v4 session.spec.ts: "derives message history from the event log".
    #[test]
    fn a_tool_round_derives_user_assistant_and_results_but_no_attempt_or_boundary() {
        let log = log_of(vec![
            SessionEvent::turn_start(1),
            user("hello"),
            attempt(),
            assistant(vec![Block::Text("let me check".into()), call("c1")]),
            result_event("c1", "ok"),
            SessionEvent::turn_end(1, TurnEndReason::Completed),
        ]);
        assert_eq!(
            log.messages(),
            [
                Message::user("hello"),
                message(
                    Role::Assistant,
                    vec![Block::Text("let me check".into()), call("c1")]
                ),
                message(Role::User, vec![Block::ToolResult(result("c1", "ok"))]),
            ]
        );
    }

    // dsh v4 session.spec.ts: "renders injected-context and user messages as
    // plain user content".
    #[test]
    fn injected_context_and_a_prompt_are_both_plain_user_messages() {
        let log = log_of(vec![
            SessionEvent::user(UserMessage::text(
                "context",
                UserSource::Other(json!({ "kind": "watcher" })),
                "file changed: a.ts",
            )),
            SessionEvent::user(UserMessage::text(
                "prompt",
                UserSource::prompt("rpc-1"),
                "focus on tests",
            )),
        ]);
        assert_eq!(
            log.messages(),
            [
                Message::user("file changed: a.ts"),
                Message::user("focus on tests")
            ]
        );
    }

    #[test]
    fn two_parallel_results_become_one_user_message_in_log_order() {
        let log = log_of(vec![
            user("time in two places?"),
            assistant(vec![call("call_1"), call("call_2")]),
            // The second call finished first, so its result was logged first.
            result_event("call_2", "18:00"),
            result_event("call_1", "12:00"),
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
    fn lifecycle_events_and_a_developer_message_between_results_do_not_split_them() {
        let log = log_of(vec![
            user("go"),
            assistant(vec![call("a"), call("b")]),
            SessionEvent::tool_call(
                1,
                1,
                &ToolCall {
                    id: "a".into(),
                    name: "clock".into(),
                    arguments: "{}".into(),
                },
            ),
            result_event("a", "1"),
            developer(vec![ContentBlock::text("a tool was added")]),
            attempt(),
            result_event("b", "2"),
        ]);
        assert_eq!(
            log.history().messages[2],
            message(
                Role::User,
                vec![
                    Block::ToolResult(result("a", "1")),
                    Block::ToolResult(result("b", "2")),
                ]
            )
        );
    }

    #[test]
    fn a_user_message_after_tool_results_stays_its_own_message() {
        let log = log_of(vec![
            user("what time is it?"),
            assistant(vec![call("call_1")]),
            result_event("call_1", "12:00"),
            user("and in Tokyo?"),
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

    // dsh v4 surface.spec.ts: "deriveMessages skips a surface node that
    // derives to null (empty assistant/message)".
    #[test]
    fn an_empty_assistant_message_is_left_out() {
        let log = log_of(vec![user("hi"), assistant(Vec::new())]);
        assert_eq!(log.messages(), [Message::user("hi")]);
    }

    #[test]
    fn derivation_never_invents_a_result_for_a_call_still_running() {
        // A live run between the model's answer and the tool's result.
        let log = log_of(vec![
            SessionEvent::turn_start(1),
            SessionEvent::step_start(1, 1),
            user("time?"),
            assistant(vec![call("call_1")]),
        ]);
        assert_eq!(
            log.messages(),
            [
                Message::user("time?"),
                message(Role::Assistant, vec![call("call_1")])
            ]
        );
    }

    // dsh v4 surface.spec.ts: "projects a system node as the leading
    // system-role message and an empty one as no message".
    #[test]
    fn the_system_prompt_becomes_the_system_field_and_an_empty_head_clears_it() {
        let mut log = log_of(vec![
            SessionEvent::turn_start(1),
            SessionEvent::step_start(1, 1),
            system("be brief"),
            user("hi"),
        ]);
        assert_eq!(log.history().system.as_deref(), Some("be brief"));
        assert_eq!(log.messages(), [Message::user("hi")]);

        log.append(replace(system(""), 2, 2, &[2])).unwrap();
        assert_eq!(log.history().system, None);
        assert_eq!(log.messages(), [Message::user("hi")]);
    }

    /// A developer message's non-empty text blocks stay separate, and a
    /// later system message keeps its place. Before, both were joined into
    /// the prompt.
    #[test]
    fn later_system_and_developer_messages_keep_their_place() {
        let log = log_of(vec![
            system("You are terse."),
            user("hi"),
            developer(vec![
                ContentBlock::text("first"),
                ContentBlock::text(""),
                ContentBlock::text("second"),
                ContentBlock::Other(json!({ "type": "tool-removal", "toolName": "old" })),
            ]),
            system("Also polite."),
        ]);
        assert_eq!(
            log.history(),
            crate::History {
                system: Some("You are terse.".into()),
                messages: vec![
                    Message::user("hi"),
                    message(
                        Role::Developer,
                        vec![Block::Text("first".into()), Block::Text("second".into())]
                    ),
                    Message::system("Also polite."),
                ],
            }
        );
    }

    /// The test the brief asks for: system "A", user, assistant, system
    /// "B", user. Only "A" is the prompt; "B" stays where it was logged.
    #[test]
    fn only_the_leading_system_message_becomes_the_prompt() {
        let log = log_of(vec![
            system("A"),
            user("first"),
            answer("one"),
            system("B"),
            user("second"),
        ]);
        assert_eq!(
            log.history(),
            crate::History {
                system: Some("A".into()),
                messages: vec![
                    Message::user("first"),
                    message(Role::Assistant, vec![Block::Text("one".into())]),
                    Message::system("B"),
                    Message::user("second"),
                ],
            }
        );
    }

    // dsh v4 llm-pi-ai context.spec.ts: "maps a leading system message to
    // systemPrompt" and "preserves later system messages for pi-ai to
    // resolve model support". An empty system node derives to nothing, so
    // it neither adds to the prompt nor ends the leading run.
    #[test]
    fn every_system_message_before_the_first_other_message_joins_the_prompt() {
        let log = log_of(vec![
            SessionEvent::system(1, 1, SystemMessage::new("empty", "")),
            system("lead"),
            system("rule"),
            user("hi"),
            system("additional rule"),
            system("lead"),
            SessionEvent::system(1, 1, SystemMessage::new("empty-2", "")),
        ]);
        assert_eq!(
            log.history(),
            crate::History {
                system: Some("lead\n\nrule".into()),
                messages: vec![
                    Message::user("hi"),
                    Message::system("additional rule"),
                    Message::system("lead"),
                ],
            }
        );
    }

    // dsh v4 llm-pi-ai context.spec.ts: "keeps a later update distinct from
    // an absent initial prompt".
    #[test]
    fn a_system_message_after_a_user_message_is_not_the_prompt() {
        let log = log_of(vec![user("hi"), system("lead rule")]);
        assert_eq!(log.history().system, None);
        assert_eq!(
            log.messages(),
            [Message::user("hi"), Message::system("lead rule")]
        );
    }

    // dsh v4 llm-pi-ai context.spec.ts: "keeps developer text and tool
    // changes together at their recorded position", for the text, and
    // `splitSystemPrompt`, which stops at the first message that isn't a
    // system message. A developer message with only tool changes leaves no
    // message, but the system message after it isn't the prompt.
    #[test]
    fn a_developer_message_ends_the_leading_run() {
        let log = log_of(vec![
            system("lead"),
            developer(vec![ContentBlock::Other(
                json!({ "type": "tool-addition", "toolName": "search" }),
            )]),
            system("later"),
            user("hi"),
        ]);
        assert_eq!(
            log.history(),
            crate::History {
                system: Some("lead".into()),
                messages: vec![Message::system("later"), Message::user("hi")],
            }
        );
    }

    // pi-ai's transform holds a system message that comes between a tool
    // call and its results until the results are in
    // (`transform-messages.js:130-183`); dsh's DeepSeek adapter expects
    // the same order ("places system updates after all parallel tool
    // results and before the next assistant").
    #[test]
    fn system_messages_logged_inside_a_tool_round_come_after_its_results() {
        let log = log_of(vec![
            system("original"),
            user("go"),
            assistant(vec![call("a"), call("b")]),
            system("first update"),
            result_event("a", "1"),
            system("second update"),
            result_event("b", "2"),
            user("more input"),
            answer("done"),
        ]);
        assert_eq!(
            log.messages(),
            [
                Message::user("go"),
                message(Role::Assistant, vec![call("a"), call("b")]),
                message(
                    Role::User,
                    vec![
                        Block::ToolResult(result("a", "1")),
                        Block::ToolResult(result("b", "2")),
                    ]
                ),
                Message::system("first update"),
                Message::system("second update"),
                Message::user("more input"),
                message(Role::Assistant, vec![Block::Text("done".into())]),
            ]
        );
    }

    // A system message whose text is empty is still a message in v4, and
    // each provider decides what to do with it: pi-ai skips it, and dsh's
    // DeepSeek adapter refuses or clears the prompt.
    #[test]
    fn a_later_system_message_with_empty_text_is_kept() {
        let mut empty_text = SystemMessage::new("system-empty", "");
        empty_text.content.push(ContentBlock::text(""));
        let log = log_of(vec![user("hi"), SessionEvent::system(1, 1, empty_text)]);
        assert_eq!(log.messages(), [Message::user("hi"), Message::system("")]);
    }

    // dsh v4 surface.spec.ts: "retains an empty developer node while
    // projecting no message".
    #[test]
    fn a_developer_message_with_only_tool_changes_adds_no_system_text() {
        let log = log_of(vec![developer(vec![ContentBlock::Other(
            json!({ "type": "tool-removal", "toolName": "old" }),
        )])]);
        assert_eq!(log.history(), crate::History::default());
        assert_eq!(log.surface_nodes(), [Seq(0)]);
    }

    // dsh v4 surface.spec.ts: "deriveMessages via surface respects replace
    // (shadowed nodes are excluded)".
    #[test]
    fn a_replaced_range_leaves_the_history() {
        let log = log_of(vec![
            user("original"),
            replace(user("compacted"), 0, 0, &[0]),
        ]);
        assert_eq!(log.messages(), [Message::user("compacted")]);
    }

    // dsh v4 surface.spec.ts: "replace with both ends at real nodes splices
    // only the range" and "single-node replacement (start === end)".
    #[test]
    fn a_replacement_takes_the_place_of_exactly_its_range() {
        let range = log_of(vec![
            user("a"),
            user("b"),
            user("c"),
            replace(user("summary"), 0, 1, &[0, 1]),
        ]);
        assert_eq!(range.surface_nodes(), [Seq(3), Seq(2)]);
        assert_eq!(
            range.messages(),
            [Message::user("summary"), Message::user("c")]
        );

        let single = log_of(vec![user("a"), user("b"), replace(user("x"), 1, 1, &[1])]);
        assert_eq!(single.surface_nodes(), [Seq(0), Seq(2)]);
    }

    fn rejection(events: Vec<SessionEvent>, last: SessionEvent) -> String {
        let mut log = log_of(events);
        let before = log.surface_nodes().to_vec();
        let error = log.append(last).expect_err("the append is refused");
        // A refused append changes nothing.
        assert_eq!(log.surface_nodes(), before);
        error.to_string()
    }

    // dsh v4 surface.spec.ts: "throws when replace start is not found",
    // "... end is not found" and "throws when start is after end".
    #[test]
    fn a_replacement_must_name_surface_nodes_in_order() {
        let start = rejection(
            vec![user("a"), SessionEvent::turn_start(1)],
            replace(user("y"), 1, 0, &[0]),
        );
        assert!(
            start.contains("start seq 1 not found in surface"),
            "{start}"
        );

        let end = rejection(
            vec![user("a"), SessionEvent::turn_start(1)],
            replace(user("y"), 0, 1, &[0]),
        );
        assert!(end.contains("end seq 1 not found in surface"), "{end}");

        let reversed = rejection(
            vec![user("a"), user("b")],
            replace(user("y"), 1, 0, &[1, 0]),
        );
        assert!(
            reversed.contains("start seq 1 (index 1) is after end seq 0"),
            "{reversed}"
        );
    }

    #[test]
    fn a_replacement_must_cite_every_node_it_takes_out() {
        let error = rejection(vec![user("a"), user("b")], replace(user("y"), 0, 1, &[0]));
        assert!(error.contains("missing 1"), "{error}");
    }

    // dsh v4 surface.spec.ts: "replaces node 0 with a new system node and
    // rejects other rewrites of the head".
    #[test]
    fn only_a_system_message_over_exactly_node_0_may_replace_the_system_prompt() {
        let replaced = log_of(vec![
            system("v1"),
            user("u1"),
            replace(system("v2"), 0, 0, &[0]),
        ]);
        assert_eq!(replaced.surface_nodes(), [Seq(2), Seq(1)]);
        assert_eq!(replaced.history().system.as_deref(), Some("v2"));

        for last in [
            replace(user("u2"), 0, 1, &[0, 1]),
            replace(system("v2"), 0, 1, &[0, 1]),
        ] {
            let error = rejection(vec![system("v1"), user("u1")], last);
            assert!(error.contains("node 0 holds the system prompt"), "{error}");
        }
    }

    // dsh v4 surface.spec.ts: "leaves later system nodes and a non-system
    // head unprotected".
    #[test]
    fn later_system_nodes_and_a_user_head_may_be_replaced() {
        let later = log_of(vec![
            system("v1"),
            user("u1"),
            system("v2"),
            user("u3"),
            replace(user("u4"), 1, 3, &[1, 2, 3]),
        ]);
        assert_eq!(later.surface_nodes(), [Seq(0), Seq(4)]);
        assert_eq!(later.history().system.as_deref(), Some("v1"));

        let plain_head = log_of(vec![
            user("u0"),
            user("u1"),
            replace(user("u2"), 0, 1, &[0, 1]),
        ]);
        assert_eq!(plain_head.surface_nodes(), [Seq(2)]);
    }

    // dsh v4 surface.spec.ts, "foldSurface tool-result rewrites".
    #[test]
    fn a_tool_result_may_replace_only_one_result_and_only_its_content() {
        let rewrite = |content: &str, is_error: bool| {
            let mut data = ToolResultData::new("result-a", 1, 1, &result("a", content));
            data.message.is_error = Some(is_error);
            SessionEvent::ToolResult {
                data,
                surface: SurfaceIntent {
                    op: SurfaceOp::Replace {
                        start: Seq(0),
                        end: Seq(0),
                    },
                    sources: vec![Seq(0)],
                },
            }
        };
        let shortened = log_of(vec![
            result_event("a", "a long output"),
            rewrite("short", false),
        ]);
        assert_eq!(
            shortened.messages(),
            [message(
                Role::User,
                vec![Block::ToolResult(result("a", "short"))]
            )]
        );

        let error = rejection(vec![result_event("a", "out")], rewrite("out", true));
        assert!(error.contains("may change only content"), "{error}");
        let error = rejection(vec![user("a")], rewrite("out", false));
        assert!(
            error.contains("must target a current tool/result"),
            "{error}"
        );
    }

    #[test]
    fn blocks_a_model_cannot_read_are_left_out_of_the_history() {
        let mut message = UserMessage::text("u", UserSource::user(), "look");
        message.content.push(ContentBlock::Other(
            json!({ "type": "image", "attachment": { "attachmentId": "img-1" } }),
        ));
        let log = log_of(vec![SessionEvent::user(message)]);
        assert_eq!(log.messages(), [Message::user("look")]);
    }
}
