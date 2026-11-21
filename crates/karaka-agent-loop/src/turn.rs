//! One turn and its steps, and the events they log.
//! (dsh: `ReactLoopAgent.turn` and `.step` in `agent-loop/src/agent.ts`,
//! and `executeToolCalls` in `agent-loop/src/tool-calls.ts`.)
//!
//! # The events, in order
//!
//! ```text
//! turn/start
//!   step/start
//!     system/message      when the prompt first appears or changes
//!     user/message        the queued message, in the first step only
//!     assistant/message   the model's answer, with its usage
//!     tool/call           for each call, in the model's order:
//!     tool/result           logged as it starts, then its result
//!   step/end              always, even when the step fails
//!   ... another step if the last one called tools ...
//! turn/end                completed, max-tokens, aborted or error
//! ```
//!
//! Each model request is derived from the session's log at the moment it
//! is sent, with [`Session::history`], never from anything the loop keeps.
//! That is dsh's rule that every request is derived from the session log,
//! and it is why a session restored from storage continues exactly where it
//! stopped.
//!
//! # When a step fails
//!
//! A cancel or an error can stop a step anywhere. Before its `step/end`,
//! every call the model made in it that has no result yet gets one, worked
//! out by [`ToolCallRecovery`] from the step's events, as dsh's `turn` does
//! (`agent.ts:333-359`). A provider refuses a history with a call that has
//! no result, so this keeps the session able to continue.
//!
//! A cancel while the answer streams keeps the text and reasoning that had
//! arrived, as an assistant message marked `interrupted`, so the model sees
//! next time what the user saw. A failed call keeps nothing: its partial
//! answer belongs to a call that didn't succeed.
//!
//! # Message ids
//!
//! dsh gives every message a random id. Karaka has no id generator yet, so
//! the loop names each message after the seq its event gets, such as
//! `assistant-7`. Only the agent appends to its session, so the next seq is
//! known before the append, and the ids are unique within the session.

use std::future::{pending, poll_fn};

use karaka_model::{
    Block, ErrorKind, Event, Message, Model, ModelError, Request, Response, Role, StopReason,
    ToolCall,
};
use karaka_session::{
    AssistantMessage, AssistantMessageData, CancelCause, CloseCause, ContentBlock, Extra, Failure,
    History, LogError, Seq, SessionEvent, SurfaceIntent, SurfaceOp, SystemMessage,
    SystemMessageData, ToolCallRecovery, ToolResultData, TurnEndReason, UserMessage,
};

use crate::agent::{Cancel, Inner};
use crate::answer::Answer;
use crate::failure::{model_failure, no_model, other_failure};

/// Why a step stopped before it finished.
enum Stop {
    /// Someone cancelled the turn.
    Aborted(CancelCause),
    /// The model call failed, or the log refused an event.
    Failed(Failure),
}

impl From<LogError> for Stop {
    fn from(error: LogError) -> Self {
        Self::Failed(other_failure(&error))
    }
}

/// What became of one turn, as far as the driver cares.
pub(crate) enum Ran {
    /// The log refused its `turn/start`, so nothing was logged.
    NotStarted,
    /// It ended with reason `error`. dsh's `turn` throws here, and `kick`
    /// catches it and stops the driver (`agent.ts:252-265,375-383`).
    Failed,
    /// It ended any other way: completed, max-tokens or aborted.
    Ended,
}

/// How a model call ended.
enum Streamed {
    Finished(Response),
    Failed(ModelError),
    /// Cancelled, with what is worth keeping of the answer so far.
    Cancelled(CancelCause, Vec<Block>),
}

impl Inner {
    /// Runs turn `turn` from `turn/start` to `turn/end`, and says whether
    /// the driver may go on to the next queued message.
    pub(crate) async fn turn(&self, turn: u64, cancel: &mut Cancel) -> Ran {
        if self.session.append(SessionEvent::turn_start(turn)).is_err() {
            return Ran::NotStarted;
        }
        let (reason, ran) = match self.steps(turn, cancel).await {
            Ok(reason) => (reason, Ran::Ended),
            Err(Stop::Aborted(cause)) => (TurnEndReason::Aborted(cause), Ran::Ended),
            Err(Stop::Failed(failure)) => (TurnEndReason::Error(failure), Ran::Failed),
        };
        // If the log refuses even this, the turn stays open, and repair
        // closes it when the session is next opened.
        let _ = self.session.append(SessionEvent::turn_end(turn, reason));
        ran
    }

    /// Runs steps until one ends the turn, and says how it ended.
    async fn steps(&self, turn: u64, cancel: &mut Cancel) -> Result<TurnEndReason, Stop> {
        let mut step = 0;
        loop {
            if let Some(cause) = cancelled(cancel) {
                return Err(Stop::Aborted(cause));
            }
            // Only the first step takes input: the queued message the turn
            // is for. dsh's later steps take steering, which isn't ported.
            let input = if step == 0 {
                match self.claim() {
                    Some(message) => Some(message),
                    // A cancel empties the queue, so a cancel that lands
                    // between the check above and the claim leaves nothing
                    // to claim. It has flipped the switch by then: both
                    // happen under the agent's lock.
                    None => {
                        return match cancelled(cancel) {
                            Some(cause) => Err(Stop::Aborted(cause)),
                            // Nothing queued and nothing cancelled. dsh ends
                            // such a turn as completed, without a step
                            // (`agent.ts:326-329`).
                            None => Ok(TurnEndReason::Completed),
                        };
                    }
                }
            } else {
                None
            };
            step += 1;
            let started = self.session.append(SessionEvent::step_start(turn, step))?;
            let outcome = self.step(turn, step, input, cancel).await;
            if outcome.is_err() {
                // If these are refused too, the step's own failure is still
                // the one to report, as in dsh's `AggregateError`.
                let _ = self.answer_pending_calls(started);
            }
            let ended = self.session.append(SessionEvent::step_end(turn, step));
            let ending = outcome?;
            ended?;
            if let Some(cause) = cancelled(cancel) {
                return Err(Stop::Aborted(cause));
            }
            // dsh keeps a `max-tokens` ending even if a later step of the
            // turn completes (`agent.ts:338-343`). Only steering can run a
            // step after one that ended the turn, and it isn't ported, so
            // here the first ending is always the last step's.
            if let Some(reason) = ending {
                return Ok(reason);
            }
        }
    }

    /// One step: the model call, then its tools. Returns how the turn ends,
    /// or `None` when tools ran and another step must follow.
    async fn step(
        &self,
        turn: u64,
        step: u64,
        input: Option<UserMessage>,
        cancel: &mut Cancel,
    ) -> Result<Option<TurnEndReason>, Stop> {
        self.update_system_prompt(turn, step)?;
        if let Some(message) = input {
            self.session.append(SessionEvent::user(message))?;
        }

        let config = &self.shared.config;
        let model = self
            .shared
            .models
            .get(&config.route)
            .ok_or_else(|| Stop::Failed(no_model(&config.route)))?;
        let History { system, messages } = self.session.history();
        let mut request = Request::new(config.model.clone(), messages);
        request.system = system;
        request.tools = self.shared.tools.schemas();
        request.max_tokens = config.max_tokens;
        request.effort = config.effort;

        let response = match stream(model.as_ref(), request, cancel).await {
            Streamed::Finished(response) => response,
            Streamed::Failed(error) => return Err(Stop::Failed(model_failure(&error))),
            Streamed::Cancelled(cause, content) => {
                if !content.is_empty() {
                    self.append_interrupted(turn, step, model.provider(), &content)?;
                }
                return Err(Stop::Aborted(cause));
            }
        };
        let data = AssistantMessageData::from_response(
            self.next_id("assistant"),
            turn,
            step,
            model.provider(),
            &config.model,
            &response,
        );
        self.session.append(SessionEvent::assistant(data))?;

        // `karaka_model::StopReason::Paused` says to treat a paused answer as
        // unfinished, like one cut off by the limit, until Karaka can resume
        // server-side tools.
        if matches!(response.stop, StopReason::MaxTokens | StopReason::Paused) {
            return Ok(Some(TurnEndReason::MaxTokens));
        }
        let calls: Vec<&ToolCall> = response
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                Block::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            return Ok(Some(TurnEndReason::Completed));
        }

        // One at a time, in the model's order. dsh runs calls that are safe
        // to overlap in parallel (`tool-calls.ts`); that isn't ported.
        for call in calls {
            if let Some(cause) = cancelled(cancel) {
                return Err(Stop::Aborted(cause));
            }
            // Logged as the call starts, so repair can tell a call that ran
            // from one that never started.
            let started = self
                .session
                .append(SessionEvent::tool_call(turn, step, call))?;
            let output = tokio::select! {
                biased;
                cause = cancellation(cancel) => return Err(Stop::Aborted(cause)),
                output = self.shared.tools.execute(call) => output,
            };
            let result = output.into_result(&call.id);
            let data = ToolResultData::new(self.next_id("tool-result"), turn, step, &result);
            self.session
                .append(SessionEvent::tool_result(data, started))?;
        }
        Ok(None)
    }

    /// Logs the system prompt when the session has none yet, or when the
    /// configured one differs from the one in effect.
    /// (dsh: `SystemPromptProjection.project`, `agent-loop/src/runtime-context.ts:88-103`.)
    ///
    /// The prompt in effect is read from the history, by `karaka-session`'s
    /// rules: the newest system message after the leading run, or else the
    /// leading run itself. That matches dsh's for every log this loop
    /// writes.
    ///
    /// - No system message on the surface: one is appended, empty if there
    ///   is no prompt, as dsh appends one on a session's first step.
    /// - A different prompt: a new system message is appended in place.
    ///   This is dsh's path for a route that takes system prompts in its
    ///   history, which dsh's DeepSeek and pi-ai adapters both declare.
    ///   Karaka's providers decide for themselves how to send one.
    /// - No prompt, when one is in effect: each system message on the
    ///   surface is replaced by an empty one, the later ones first, as dsh
    ///   clears a prompt. An appended empty message would derive to
    ///   nothing, and leave the old prompt in effect.
    fn update_system_prompt(&self, turn: u64, step: u64) -> Result<(), LogError> {
        let wanted = self.shared.config.system.as_deref().unwrap_or("");
        let events = self.session.events();
        let nodes: Vec<(Seq, String)> = self
            .session
            .surface_nodes()
            .into_iter()
            .filter_map(|seq| match &events[seq.0 as usize].event {
                SessionEvent::SystemMessage { data, .. } => Some((seq, text(&data.message))),
                _ => None,
            })
            .collect();

        let Some((head, head_text)) = nodes.first() else {
            return self.append_system(turn, step, wanted, SurfaceIntent::append());
        };
        if wanted.is_empty() {
            for (seq, text) in &nodes[1..] {
                if !text.is_empty() {
                    self.append_system(turn, step, "", replacing(*seq))?;
                }
            }
            if !head_text.is_empty() {
                self.append_system(turn, step, "", replacing(*head))?;
            }
            return Ok(());
        }
        if in_effect(&self.session.history()) != wanted {
            self.append_system(turn, step, wanted, SurfaceIntent::append())?;
        }
        Ok(())
    }

    fn append_system(
        &self,
        turn: u64,
        step: u64,
        text: &str,
        surface: SurfaceIntent,
    ) -> Result<(), LogError> {
        let message = SystemMessage::new(self.next_id("system"), text);
        self.session.append(SessionEvent::SystemMessage {
            data: SystemMessageData {
                turn,
                step,
                message,
                extra: Extra::new(),
            },
            surface,
        })?;
        Ok(())
    }

    /// Logs the part of an answer that arrived before a cancel.
    /// (dsh: `agent.ts:465-482`.) It has no usage: Karaka's models report
    /// usage only when they finish.
    fn append_interrupted(
        &self,
        turn: u64,
        step: u64,
        provider: &str,
        content: &[Block],
    ) -> Result<(), LogError> {
        let message = AssistantMessage::from_model(
            self.next_id("assistant"),
            provider,
            &self.shared.config.model,
            content,
        );
        self.session
            .append(SessionEvent::assistant(AssistantMessageData {
                turn,
                step,
                message,
                stream: Vec::new(),
                usage: None,
                interrupted: true,
                extra: Extra::new(),
            }))?;
        Ok(())
    }

    /// Gives a result to every call made since `step_start` that has none,
    /// so the log stays one a provider accepts. (dsh: the `toolRecovery` in
    /// `turn`, `agent.ts:333-355`.)
    ///
    /// A call whose `tool/call` was logged gets "outcome unknown"; one that
    /// never started gets "not started". The wording is v4's for a turn that
    /// was interrupted, which dsh's loop uses for a failed step too.
    fn answer_pending_calls(&self, step_start: Seq) -> Result<(), LogError> {
        let mut recovery = ToolCallRecovery::new(CloseCause::Interrupted);
        for entry in self.session.events_since(step_start) {
            recovery.observe(&entry);
        }
        let results = recovery.results();
        if !results.is_empty() {
            self.session
                .append_all(results.into_iter().map(|entry| entry.event))?;
        }
        Ok(())
    }

    /// An id for the message the next appended event will hold.
    fn next_id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.session.len())
    }
}

/// Streams one model call to its end, or until a cancel. A cancel drops the
/// stream, which closes its connection.
async fn stream(model: &dyn Model, request: Request, cancel: &mut Cancel) -> Streamed {
    let mut events = model.stream(request);
    let mut answer = Answer::default();
    loop {
        // `biased` checks the switch first, so a cancel wins over an event
        // that is ready at the same moment.
        let event = tokio::select! {
            biased;
            cause = cancellation(cancel) => return Streamed::Cancelled(cause, answer.interrupted()),
            event = poll_fn(|cx| events.as_mut().poll_next(cx)) => event,
        };
        match event {
            Some(Ok(Event::Done { stop, usage })) => {
                let message = Message {
                    role: Role::Assistant,
                    content: answer.finish(stop),
                };
                return Streamed::Finished(Response {
                    message,
                    stop,
                    usage,
                });
            }
            Some(Ok(event)) => answer.push(event),
            Some(Err(error)) => return Streamed::Failed(error),
            None => {
                return Streamed::Failed(ModelError::new(
                    ErrorKind::Network,
                    "the stream ended before the model finished",
                ));
            }
        }
    }
}

/// The cause, if the turn has been cancelled.
fn cancelled(cancel: &Cancel) -> Option<CancelCause> {
    cancel.borrow().clone()
}

/// Waits for the turn to be cancelled, and gives the cause.
async fn cancellation(cancel: &mut Cancel) -> CancelCause {
    // The value is cloned out in its own statement, so the lock `wait_for`
    // holds isn't kept across the `.await` below.
    let cause = cancel
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|cause| cause.clone());
    match cause {
        Some(cause) => cause,
        // The switch was dropped, so nothing can cancel the turn any more.
        None => pending().await,
    }
}

/// A system message's text: its text blocks joined with nothing between,
/// as `karaka-session` and dsh's `textOf` read it.
fn text(message: &SystemMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// The system prompt in effect at the end of `history`.
fn in_effect(history: &History) -> String {
    history
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::System)
        .map(Message::text)
        .or_else(|| history.system.clone())
        .unwrap_or_default()
}

/// A surface intent that takes the place of the node `seq`, as dsh's
/// `SystemPromptProjection.replace` writes it.
fn replacing(seq: Seq) -> SurfaceIntent {
    SurfaceIntent {
        op: SurfaceOp::Replace {
            start: seq,
            end: seq,
        },
        sources: vec![seq],
    }
}
