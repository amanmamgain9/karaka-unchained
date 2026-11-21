//! Closing a turn that a crash or a fork left open.
//! (dsh v4: `packages/core/session/src/repair.ts`.)
//!
//! A turn can stop anywhere: after the model asked for tools but before
//! their results were logged, or between a step's start and its end.
//! Providers refuse a history with a tool call that has no result, so such a
//! session could never continue.
//!
//! Repair works out the events that close the open turn, and the caller
//! appends them to the log, durably. Derivation never invents them, so a
//! live run's history never shows a made-up result for a tool that is still
//! running, and a result that arrives late can't give a call two results.
//!
//! # The rules, as in v4
//!
//! - Only the last turn, if it has no `turn/end`, is repaired
//!   (`openTurnClosers`, `repair.ts:53`).
//! - Within it, only calls made since the last step or turn boundary are
//!   pending: a `step/end`, `turn/start` or `turn/end` forgets every call
//!   (`ToolCallRecovery.observe`, `repair.ts:116`). So a call left
//!   unanswered by a step that did close stays unanswered, as v4 leaves it.
//! - A result answers a call only if it was appended, not a replacement,
//!   and comes from the call's own turn and step (`repair.ts:136`).
//! - Each pending call gets a failed result, in the order the model made
//!   the calls. If a `tool/call` shows the tool started, the result says its
//!   outcome is unknown and cites that event; otherwise it says the tool
//!   never started (`ToolCallRecovery.results`, `repair.ts:157`).
//! - Then a `step/end` if a step is open, then a `turn/end` whose reason is
//!   the cause (`repair.ts:92`).
//! - Seqs continue the log, and every closer reuses the last event's time,
//!   so repair is deterministic and never invents a future time
//!   (`repair.ts:85`).

use crate::data::{ToolError, ToolResultData, TurnEndReason};
use crate::event::{Entry, Seq, SessionEvent, SurfaceIntent, SurfaceOp, Time};
use crate::message::{ContentBlock, Extra, ToolKind, ToolResultMessage, ToolRole, ToolSource};

/// The error code of a result for a call that was asked for but never
/// recorded as started. (dsh v4: `TOOL_NOT_STARTED`.)
pub const TOOL_NOT_STARTED: &str = "TOOL_NOT_STARTED";

/// The error code of a result for a call that started but whose outcome
/// was never recorded. (dsh v4: `TOOL_OUTCOME_UNKNOWN`.)
pub const TOOL_OUTCOME_UNKNOWN: &str = "TOOL_OUTCOME_UNKNOWN";

/// What the model reads for a call that started before a crash.
pub const INTERRUPTED_STARTED: &str = "The tool call was interrupted after it was recorded, \
    but no result was durably recorded. Its outcome is unknown. Decide whether to retry from \
    the tool semantics: retry only if the operation is read-only or idempotent; if it may have \
    side effects, first verify external state or ask the user. Do not retry blindly.";

/// What the model reads for a call that hadn't started before a crash.
pub const INTERRUPTED_NOT_STARTED: &str = "The tool call was interrupted before the Harness recorded it as started. Retry it if it \
     is still needed.";

/// What the model reads in a fork for a call the parent had started.
pub const FORKED_STARTED: &str = "The history inherited by this branch records this tool call \
    starting but does not include its result. The parent session may have completed it after \
    the fork point. Decide whether to retry from the tool semantics: retry only if the \
    operation is read-only or idempotent; if it may have side effects, first verify external \
    state or ask the user. Do not retry blindly.";

/// What the model reads in a fork for a call the parent hadn't started.
pub const FORKED_NOT_STARTED: &str = "The history inherited by this branch has no record of \
    this tool call starting. The parent session may have executed it after the fork point. \
    Decide whether to retry from the tool semantics: retry only if the operation is read-only \
    or idempotent; if it may have side effects, first verify external state or ask the user. \
    Do not retry blindly.";

/// Why an open turn is being closed. (dsh v4: `OpenTurnCloseCause`.)
///
/// It picks the wording of the failed results, the `turn/end` reason, and
/// the prefix of the made-up message ids. The error codes are the same for
/// both: they state the same fact about the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseCause {
    /// Recovering a stored log after a crash.
    Interrupted,
    /// Cutting a fork inside the source's open turn.
    Forked,
}

impl CloseCause {
    fn name(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Forked => "forked",
        }
    }

    fn text(self, started: bool) -> &'static str {
        match (self, started) {
            (Self::Interrupted, true) => INTERRUPTED_STARTED,
            (Self::Interrupted, false) => INTERRUPTED_NOT_STARTED,
            (Self::Forked, true) => FORKED_STARTED,
            (Self::Forked, false) => FORKED_NOT_STARTED,
        }
    }

    fn reason(self) -> TurnEndReason {
        match self {
            Self::Interrupted => TurnEndReason::Interrupted,
            Self::Forked => TurnEndReason::Forked,
        }
    }
}

/// The events that close a log's open last turn after a crash, to append
/// after `entries`. Empty when every turn is closed.
/// (dsh v4: `interruptedTurnClosers`.)
pub fn interrupted_turn_closers(entries: &[Entry]) -> Vec<Entry> {
    open_turn_closers(entries, CloseCause::Interrupted)
}

/// The events that close a log's open last turn for `cause`, to append
/// after `entries`. Empty when every turn is closed.
/// (dsh v4: `openTurnClosers`.)
pub fn open_turn_closers(entries: &[Entry], cause: CloseCause) -> Vec<Entry> {
    let mut open_turn = None;
    let mut open_step = None;
    let mut recovery = ToolCallRecovery::new(cause);
    for entry in entries {
        recovery.observe(entry);
        match &entry.event {
            SessionEvent::TurnStart(start) => {
                open_turn = Some(start.turn);
                open_step = None;
            }
            SessionEvent::TurnEnd(_) => {
                open_turn = None;
                open_step = None;
            }
            SessionEvent::StepStart(step) => open_step = Some(step.step),
            SessionEvent::StepEnd(_) => open_step = None,
            _ => {}
        }
    }
    // An open turn means its `turn/start` was logged, so there is a last
    // event.
    let (Some(turn), Some(last)) = (open_turn, entries.last()) else {
        return Vec::new();
    };

    let mut closers = recovery.results();
    let mut seq = Seq(last.seq.0 + closers.len() as u64 + 1);
    let mut close = |event| {
        let entry = Entry {
            seq,
            time: last.time,
            ignorable: false,
            event,
        };
        seq = seq.next();
        entry
    };
    if let Some(step) = open_step {
        closers.push(close(SessionEvent::step_end(turn, step)));
    }
    closers.push(close(SessionEvent::turn_end(turn, cause.reason())));
    closers
}

/// The tool calls of one step that have no result yet, tracked from the
/// events as they are logged. (dsh v4: `ToolCallRecovery`.)
///
/// The loop observes each event of a step, and if the step fails, appends
/// [`results`](Self::results) before closing it, so no call is left without
/// a result. Repair uses it the same way over a stored log.
#[derive(Clone, Debug)]
pub struct ToolCallRecovery {
    cause: CloseCause,
    /// In the order the model made the calls, as v4's `Map` keeps them.
    pending: Vec<PendingCall>,
    /// The seq and time of the last event observed.
    last: Option<(Seq, Time)>,
}

#[derive(Clone, Debug)]
struct PendingCall {
    call_id: String,
    turn: u64,
    step: u64,
    /// The `tool/call` event that shows the tool started, if one was logged.
    started_at: Option<Seq>,
}

impl ToolCallRecovery {
    pub fn new(cause: CloseCause) -> Self {
        Self {
            cause,
            pending: Vec::new(),
            last: None,
        }
    }

    /// Takes in the next event of the session, in seq order.
    pub fn observe(&mut self, entry: &Entry) {
        self.last = Some((entry.seq, entry.time));
        match &entry.event {
            SessionEvent::TurnStart(_) | SessionEvent::TurnEnd(_) | SessionEvent::StepEnd(_) => {
                self.pending.clear();
            }
            SessionEvent::AssistantMessage { data, .. } => {
                for block in &data.message.content {
                    let ContentBlock::ToolCall(call) = block else {
                        continue;
                    };
                    let call = PendingCall {
                        call_id: call.id.clone(),
                        turn: data.turn,
                        step: data.step,
                        started_at: None,
                    };
                    // Like v4's `Map.set`: a repeated id keeps its place and
                    // takes the new value.
                    match self.position(&call.call_id) {
                        Some(index) => self.pending[index] = call,
                        None => self.pending.push(call),
                    }
                }
            }
            SessionEvent::ToolCall(call) => {
                if let Some(index) = self.position(&call.call_id) {
                    self.pending[index].started_at = Some(entry.seq);
                }
            }
            SessionEvent::ToolResult { data, surface } => {
                let call_id = &data.message.source.call_id;
                if let Some(index) = self.position(call_id) {
                    let pending = &self.pending[index];
                    if surface.op == SurfaceOp::Append
                        && pending.turn == data.turn
                        && pending.step == data.step
                    {
                        self.pending.remove(index);
                    }
                }
            }
            _ => {}
        }
    }

    /// A failed result for each pending call, in the order the model made
    /// them, numbered after the last observed event and stamped with its
    /// time. Doesn't change what is pending: the caller appends the results
    /// and observes them like any other event.
    pub fn results(&self) -> Vec<Entry> {
        let Some((last_seq, time)) = self.last else {
            return Vec::new();
        };
        let mut seq = last_seq;
        self.pending
            .iter()
            .map(|call| {
                seq = seq.next();
                let started = call.started_at.is_some();
                let error = if started {
                    ("ToolOutcomeUnknownError", TOOL_OUTCOME_UNKNOWN)
                } else {
                    ("ToolNotStartedError", TOOL_NOT_STARTED)
                };
                let message = ToolResultMessage {
                    id: format!("{}-tool-result-{}-{}", self.cause.name(), call.call_id, seq),
                    role: ToolRole::Tool,
                    source: ToolSource {
                        kind: ToolKind::Tool,
                        call_id: call.call_id.clone(),
                        extra: Extra::new(),
                    },
                    tool_call_id: call.call_id.clone(),
                    content: vec![ContentBlock::text(self.cause.text(started))],
                    is_error: Some(true),
                    extra: Extra::new(),
                };
                Entry {
                    seq,
                    time,
                    ignorable: false,
                    event: SessionEvent::ToolResult {
                        data: ToolResultData {
                            turn: call.turn,
                            step: call.step,
                            message,
                            error: Some(ToolError {
                                name: error.0.into(),
                                code: error.1.into(),
                                reason: None,
                                extra: Extra::new(),
                            }),
                            meta: None,
                            extra: Extra::new(),
                        },
                        surface: SurfaceIntent {
                            op: SurfaceOp::Append,
                            sources: call.started_at.into_iter().collect(),
                        },
                    },
                }
            })
            .collect()
    }

    fn position(&self, call_id: &str) -> Option<usize> {
        self.pending.iter().position(|call| call.call_id == call_id)
    }
}

/// Ported from dsh v4 `packages/core/session/tests/repair.spec.ts`. Each
/// `openTurnClosers` case runs for both causes, as v4's `describe.each`
/// does.
#[cfg(test)]
mod tests {
    use karaka_model::{Block, ToolCall, ToolResult};
    use serde_json::json;

    use super::*;
    use crate::data::AssistantMessageData;
    use crate::message::AssistantMessage;

    const CAUSES: [CloseCause; 2] = [CloseCause::Interrupted, CloseCause::Forked];

    /// Numbers `events` from 0, each at a time equal to its seq, as v4's
    /// fixtures do.
    fn log(events: Vec<SessionEvent>) -> Vec<Entry> {
        events
            .into_iter()
            .enumerate()
            .map(|(index, event)| at(index as u64, index as i64, event))
            .collect()
    }

    fn at(seq: u64, time: i64, event: SessionEvent) -> Entry {
        Entry {
            seq: Seq(seq),
            time: Time(time),
            ignorable: false,
            event,
        }
    }

    fn calls(turn: u64, step: u64, ids: &[&str]) -> SessionEvent {
        let content: Vec<Block> = ids
            .iter()
            .map(|id| {
                Block::ToolCall(ToolCall {
                    id: (*id).into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                })
            })
            .collect();
        SessionEvent::assistant(AssistantMessageData {
            turn,
            step,
            message: AssistantMessage::from_model("assistant", "mock", "mock", &content),
            stream: Vec::new(),
            usage: None,
            interrupted: false,
            extra: Extra::new(),
        })
    }

    fn started(turn: u64, step: u64, id: &str) -> SessionEvent {
        SessionEvent::tool_call(
            turn,
            step,
            &ToolCall {
                id: id.into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
        )
    }

    fn result(turn: u64, step: u64, id: &str, op: SurfaceOp) -> SessionEvent {
        let result = ToolResult {
            call_id: id.into(),
            content: "ok".into(),
            is_error: false,
        };
        SessionEvent::ToolResult {
            data: ToolResultData::new(format!("result-{id}"), turn, step, &result),
            surface: SurfaceIntent {
                op,
                sources: match op {
                    SurfaceOp::Append => Vec::new(),
                    SurfaceOp::Replace { start, .. } => vec![start],
                },
            },
        }
    }

    fn types(closers: &[Entry]) -> Vec<&str> {
        closers
            .iter()
            .map(|entry| entry.event.event_type())
            .collect()
    }

    fn seqs(closers: &[Entry]) -> Vec<u64> {
        closers.iter().map(|entry| entry.seq.0).collect()
    }

    fn result_data(entry: &Entry) -> (&ToolResultData, &SurfaceIntent) {
        match &entry.event {
            SessionEvent::ToolResult { data, surface } => (data, surface),
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    fn result_text(entry: &Entry) -> &str {
        match result_data(entry).0.message.content.as_slice() {
            [ContentBlock::Text(text)] => &text.text,
            other => panic!("expected one text block, got {other:?}"),
        }
    }

    fn error_code(entry: &Entry) -> &str {
        &result_data(entry).0.error.as_ref().expect("an error").code
    }

    // "observes a live step without earlier history and retains repairs
    // until their commits arrive"
    #[test]
    fn recovery_answers_a_live_step_and_forgets_each_call_once_its_result_is_observed() {
        let mut recovery = ToolCallRecovery::new(CloseCause::Interrupted);
        assert!(recovery.results().is_empty());
        recovery.observe(&at(20, 100, SessionEvent::step_start(3, 2)));
        recovery.observe(&at(21, 110, calls(3, 2, &["started", "pending"])));
        recovery.observe(&at(22, 120, started(3, 2, "started")));

        let results = recovery.results();
        let summary: Vec<_> = results
            .iter()
            .map(|entry| {
                let (data, surface) = result_data(entry);
                (
                    entry.seq.0,
                    entry.time.0,
                    data.turn,
                    data.step,
                    data.message.tool_call_id.as_str(),
                    error_code(entry),
                    surface.sources.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    23,
                    120,
                    3,
                    2,
                    "started",
                    TOOL_OUTCOME_UNKNOWN,
                    vec![Seq(22)]
                ),
                (24, 120, 3, 2, "pending", TOOL_NOT_STARTED, vec![]),
            ]
        );
        // Asking again changes nothing.
        assert_eq!(recovery.results(), results);
        for (index, entry) in results.iter().enumerate() {
            recovery.observe(entry);
            assert_eq!(recovery.results(), results[index + 1..]);
        }
    }

    // "does not acknowledge a pending call with $kind result reusing its id"
    #[test]
    fn a_replacement_or_another_steps_result_does_not_answer_a_pending_call() {
        let cases = [
            (
                "replacement",
                3,
                2,
                SurfaceOp::Replace {
                    start: Seq(10),
                    end: Seq(10),
                },
            ),
            ("other turn", 2, 2, SurfaceOp::Append),
            ("other step", 3, 1, SurfaceOp::Append),
        ];
        for (kind, turn, step, op) in cases {
            let mut recovery = ToolCallRecovery::new(CloseCause::Interrupted);
            recovery.observe(&at(21, 110, calls(3, 2, &["started", "pending"])));
            recovery.observe(&at(22, 120, result(turn, step, "started", op)));
            let ids: Vec<_> = recovery
                .results()
                .iter()
                .map(|entry| result_data(entry).0.message.tool_call_id.clone())
                .collect();
            assert_eq!(ids, ["started", "pending"], "{kind}");
        }
    }

    // "returns nothing for a balanced log (ends on turn/end)" and "returns
    // nothing for an empty log"
    #[test]
    fn a_balanced_or_empty_log_needs_no_closers() {
        for cause in CAUSES {
            let balanced = log(vec![
                SessionEvent::turn_start(1),
                SessionEvent::turn_end(1, TurnEndReason::Completed),
            ]);
            assert_eq!(open_turn_closers(&balanced, cause), []);
            assert_eq!(open_turn_closers(&[], cause), []);
        }
    }

    // "closes an open turn with no open step (turn/end with the cause
    // reason only)"
    #[test]
    fn an_open_turn_with_no_step_gets_only_a_turn_end_with_the_cause() {
        for cause in CAUSES {
            let closers = open_turn_closers(&log(vec![SessionEvent::turn_start(1)]), cause);
            assert_eq!(types(&closers), ["turn/end"]);
            assert_eq!(seqs(&closers), [1]);
            assert_eq!(closers[0].event, SessionEvent::turn_end(1, cause.reason()));
        }
    }

    // "closes an open step before the turn (step/end then turn/end)"
    #[test]
    fn an_open_step_is_closed_before_its_turn() {
        for cause in CAUSES {
            let events = log(vec![
                SessionEvent::turn_start(1),
                SessionEvent::step_start(1, 1),
            ]);
            let closers = open_turn_closers(&events, cause);
            assert_eq!(types(&closers), ["step/end", "turn/end"]);
            assert_eq!(seqs(&closers), [2, 3]);
            assert_eq!(closers[0].event, SessionEvent::step_end(1, 1));
        }
    }

    // "marks an assistant tool request with no recorded call as not started"
    #[test]
    fn a_call_with_no_tool_call_event_is_answered_as_not_started() {
        for cause in CAUSES {
            let events = log(vec![
                SessionEvent::turn_start(2),
                SessionEvent::step_start(2, 1),
                calls(2, 1, &["call-1"]),
            ]);
            let closers = open_turn_closers(&events, cause);
            assert_eq!(types(&closers), ["tool/result", "step/end", "turn/end"]);
            assert_eq!(seqs(&closers), [3, 4, 5]);

            let (data, surface) = result_data(&closers[0]);
            assert_eq!((data.turn, data.step), (2, 1));
            let message = &data.message;
            assert_eq!(message.id, format!("{}-tool-result-call-1-3", cause.name()));
            assert_eq!(message.source.call_id, "call-1");
            assert_eq!(message.tool_call_id, "call-1");
            assert_eq!(message.is_error, Some(true));
            assert_eq!(error_code(&closers[0]), TOOL_NOT_STARTED);
            assert_eq!(surface, &SurfaceIntent::append());
            let expected = match cause {
                CloseCause::Interrupted => INTERRUPTED_NOT_STARTED,
                CloseCause::Forked => FORKED_NOT_STARTED,
            };
            assert_eq!(result_text(&closers[0]), expected);
        }
    }

    // "does NOT synthesize a result for a tool-call that already has one"
    #[test]
    fn an_answered_call_gets_no_second_result() {
        for cause in CAUSES {
            let events = log(vec![
                SessionEvent::turn_start(2),
                SessionEvent::step_start(2, 1),
                calls(2, 1, &["call-1"]),
                result(2, 1, "call-1", SurfaceOp::Append),
            ]);
            let closers = open_turn_closers(&events, cause);
            assert_eq!(types(&closers), ["step/end", "turn/end"]);
        }
    }

    // "preserves an unanswered assistant call after the owning step already
    // closed"
    #[test]
    fn a_call_left_unanswered_by_a_closed_step_stays_unanswered() {
        for cause in CAUSES {
            let events = log(vec![
                SessionEvent::turn_start(2),
                SessionEvent::step_start(2, 1),
                calls(2, 1, &["call-1"]),
                SessionEvent::step_end(2, 1),
            ]);
            assert_eq!(
                open_turn_closers(&events, cause),
                [at(4, 3, SessionEvent::turn_end(2, cause.reason()))]
            );
        }
    }

    // "synthesizes results only for the still-open turn, not a committed
    // earlier turn"
    #[test]
    fn only_the_open_turns_calls_get_results() {
        for cause in CAUSES {
            let events = log(vec![
                SessionEvent::turn_start(1),
                SessionEvent::step_start(1, 1),
                calls(1, 1, &["old-call"]),
                result(1, 1, "old-call", SurfaceOp::Append),
                SessionEvent::step_end(1, 1),
                SessionEvent::turn_end(1, TurnEndReason::Completed),
                SessionEvent::turn_start(2),
                SessionEvent::step_start(2, 1),
                calls(2, 1, &["new-call"]),
            ]);
            let closers = open_turn_closers(&events, cause);
            assert_eq!(types(&closers), ["tool/result", "step/end", "turn/end"]);
            assert_eq!(
                result_data(&closers[0]).0.message.source.call_id,
                "new-call"
            );
        }
    }

    // "synthesizes a result for each of multiple unanswered calls, in log
    // order"
    #[test]
    fn only_the_unanswered_one_of_two_calls_gets_a_result() {
        for cause in CAUSES {
            let events = log(vec![
                SessionEvent::turn_start(1),
                SessionEvent::step_start(1, 1),
                calls(1, 1, &["call-a", "call-b"]),
                result(1, 1, "call-a", SurfaceOp::Append),
            ]);
            let closers = open_turn_closers(&events, cause);
            assert_eq!(types(&closers), ["tool/result", "step/end", "turn/end"]);
            assert_eq!(result_data(&closers[0]).0.message.source.call_id, "call-b");
        }
    }

    #[test]
    fn several_unanswered_calls_are_answered_in_the_order_the_model_made_them() {
        let events = log(vec![
            SessionEvent::turn_start(1),
            SessionEvent::step_start(1, 1),
            calls(1, 1, &["call-z", "call-a", "call-m"]),
        ]);
        let closers = interrupted_turn_closers(&events);
        let ids: Vec<_> = closers[..3]
            .iter()
            .map(|entry| result_data(entry).0.message.tool_call_id.as_str())
            .collect();
        assert_eq!(ids, ["call-z", "call-a", "call-m"]);
        assert_eq!(seqs(&closers), [3, 4, 5, 6, 7]);
    }

    // "synthesized tool/result carries surfaceOp and sourceEventSeqs when
    // tool/call was logged"
    #[test]
    fn a_started_calls_result_cites_its_tool_call_event() {
        for cause in CAUSES {
            let events = log(vec![
                SessionEvent::turn_start(1),
                SessionEvent::step_start(1, 1),
                calls(1, 1, &["call-1"]),
                started(1, 1, "call-1"),
            ]);
            let closers = open_turn_closers(&events, cause);
            assert_eq!(types(&closers), ["tool/result", "step/end", "turn/end"]);
            let (data, surface) = result_data(&closers[0]);
            assert_eq!(surface.op, SurfaceOp::Append);
            assert_eq!(surface.sources, [Seq(3)]);
            assert_eq!(
                serde_json::to_value(&data.error).unwrap(),
                json!({ "name": "ToolOutcomeUnknownError", "code": TOOL_OUTCOME_UNKNOWN })
            );
            let text = result_text(&closers[0]);
            assert!(text.contains("retry only if the operation is read-only or idempotent"));
            assert!(text.contains("first verify external state or ask the user"));
        }
    }

    // "handles tool/call without a matching assistant/message entry
    // gracefully"
    #[test]
    fn a_tool_call_event_with_no_assistant_request_needs_no_result() {
        for cause in CAUSES {
            let events = log(vec![
                SessionEvent::turn_start(1),
                SessionEvent::step_start(1, 1),
                started(1, 1, "orphan"),
            ]);
            let closers = open_turn_closers(&events, cause);
            assert_eq!(types(&closers), ["step/end", "turn/end"]);
        }
    }

    fn open_call(started_too: bool) -> Vec<Entry> {
        let mut events = vec![
            SessionEvent::turn_start(1),
            SessionEvent::step_start(1, 1),
            calls(1, 1, &["call-1"]),
        ];
        if started_too {
            events.push(started(1, 1, "call-1"));
        }
        log(events)
    }

    // "cause %o, started=%s pins its exact text": the wording is what the
    // model reads, so each is checked word for word.
    #[test]
    fn each_cause_and_lifecycle_has_its_exact_wording() {
        let cases = [
            (
                CloseCause::Interrupted,
                false,
                "The tool call was interrupted before the Harness recorded it as started. \
                 Retry it if it is still needed.",
            ),
            (
                CloseCause::Interrupted,
                true,
                "The tool call was interrupted after it was recorded, but no result was \
                 durably recorded. Its outcome is unknown. Decide whether to retry from the \
                 tool semantics: retry only if the operation is read-only or idempotent; if \
                 it may have side effects, first verify external state or ask the user. Do \
                 not retry blindly.",
            ),
            (
                CloseCause::Forked,
                false,
                "The history inherited by this branch has no record of this tool call \
                 starting. The parent session may have executed it after the fork point. \
                 Decide whether to retry from the tool semantics: retry only if the operation \
                 is read-only or idempotent; if it may have side effects, first verify \
                 external state or ask the user. Do not retry blindly.",
            ),
            (
                CloseCause::Forked,
                true,
                "The history inherited by this branch records this tool call starting but \
                 does not include its result. The parent session may have completed it after \
                 the fork point. Decide whether to retry from the tool semantics: retry only \
                 if the operation is read-only or idempotent; if it may have side effects, \
                 first verify external state or ask the user. Do not retry blindly.",
            ),
        ];
        for (cause, started_too, text) in cases {
            let closers = open_turn_closers(&open_call(started_too), cause);
            assert_eq!(
                result_text(&closers[0]),
                text,
                "{cause:?}, started={started_too}"
            );
        }
    }

    // "shares the error codes across causes: they state the same call
    // lifecycle fact"
    #[test]
    fn both_causes_use_the_same_error_codes() {
        for cause in CAUSES {
            let not_started = &open_turn_closers(&open_call(false), cause)[0];
            assert_eq!(
                result_data(not_started)
                    .0
                    .error
                    .as_ref()
                    .map(|e| (&*e.name, &*e.code)),
                Some(("ToolNotStartedError", TOOL_NOT_STARTED))
            );
            let started_call = &open_turn_closers(&open_call(true), cause)[0];
            assert_eq!(
                result_data(started_call)
                    .0
                    .error
                    .as_ref()
                    .map(|e| (&*e.name, &*e.code)),
                Some(("ToolOutcomeUnknownError", TOOL_OUTCOME_UNKNOWN))
            );
        }
    }

    // "interruptedTurnClosers is the public interrupted-cause entry"
    #[test]
    fn interrupted_turn_closers_is_the_interrupted_cause() {
        let events = open_call(true);
        let closers = interrupted_turn_closers(&events);
        assert_eq!(closers, open_turn_closers(&events, CloseCause::Interrupted));
        assert_eq!(
            closers.last().map(|entry| &entry.event),
            Some(&SessionEvent::turn_end(1, TurnEndReason::Interrupted))
        );
    }

    #[test]
    fn closers_reuse_the_last_events_time() {
        let events = vec![
            at(0, 1_000, SessionEvent::turn_start(1)),
            at(1, 2_000, SessionEvent::step_start(1, 1)),
            at(2, 3_000, calls(1, 1, &["call-1"])),
        ];
        let times: Vec<i64> = interrupted_turn_closers(&events)
            .iter()
            .map(|entry| entry.time.0)
            .collect();
        assert_eq!(times, [3_000, 3_000, 3_000]);
    }
}
