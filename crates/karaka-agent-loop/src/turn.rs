//! One turn and its steps, and the events they log.
//! (dsh: `ReactLoopAgent.turn` and `.step` in `agent-loop/src/agent.ts`,
//! and `executeToolCalls` in `agent-loop/src/tool-calls.ts`.)
//!
//! # The events, in order
//!
//! ```text
//! agent/inbox/spliced     a followup, steer or inject queued, any time
//! turn/start
//!   agent/inbox/spliced   the step taking its input: all of next-step, and
//!                         in a turn's first step the oldest of next-turn
//!   step/start
//!     system/message      when the prompt first appears or changes
//!     user/message        each message the step took, in its first attempt
//!     request/header      when the settings or tools change, or a loop
//!                         makes its first request (see `request.rs`)
//!     request/context     when the route changes
//!     assistant/message   the model's answer, with its usage, or else
//!     assistant/attempt   for a call that failed or was cancelled early;
//!                         after a failure the request-error hook may ask
//!                         for the call to be made again, in the same
//!                         step, and a handler such as `karaka-retry` may
//!                         log its own events first
//!     tool/call           for each call, in the model's order:
//!     tool/result           logged as it starts, then its result
//!   step/end              always, even when the step fails
//!   ... another step if the last one called tools, or next-step holds
//!       input ...
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
//! next time what the user saw. A failed call keeps no message: its partial
//! answer belongs to a call that didn't succeed. It, and a cancel before
//! anything worth keeping arrived, log an `assistant/attempt` instead, as
//! dsh does, so the log shows the call was made. A call that never started,
//! because no model serves the route or a cancel came first, logs none.
//!
//! # Message ids
//!
//! dsh gives every message a random id. Karaka has no id generator yet, so
//! the loop names each message after the seq its event gets, such as
//! `assistant-7`. Only the agent appends to its session, so the next seq is
//! known before the append, and the ids are unique within the session.

use std::future::{Future, poll_fn};
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, PoisonError};
use std::task::{Context, Poll};

use karaka_agent::{
    Agent, PreStep, PreStepDecision, RequestError, RequestErrorAction, SettledAs, StreamChunk,
};
use karaka_tools::ToolOutput;
use serde_json::Value;

use karaka_model::{
    Block, BlockAssembler, ErrorKind, FinishReason, Message, Model, ModelError, Request, Response,
    Role, ToolCall, Usage,
};
use karaka_session::{
    AssistantAttemptData, AssistantMessage, AssistantMessageData, CancelCause, CloseCause,
    ContentBlock, Extra, Failure, History, InboxTarget, LogError, Seq, SessionEvent, SurfaceIntent,
    SurfaceOp, SystemMessage, SystemMessageData, ToolCallRecovery, ToolResultData, TurnEndReason,
    UserMessage,
};
use karaka_system_prompt::{
    AssembleContext, PromptAssembly, Scope, join_context_sections, render_context_sections,
    render_prompt,
};

use crate::agent::{Cancel, Inner};
use crate::failure::{handler_panicked, model_failure, no_model, other_failure, panicked};
use crate::live::LiveAttempt;

/// Why a step stopped before it finished.
pub(crate) enum Stop {
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
    /// It ended with reason `aborted`. dsh's `turn` throws here too, and
    /// the driver goes on only for a latched wake.
    Aborted,
    /// It ended any other way: completed or max-tokens.
    Ended,
}

/// How a model call ended.
enum Streamed {
    Finished(Response),
    Failed(ModelError),
    /// Cancelled, with what is worth keeping of the answer so far, and the
    /// usage if it had come.
    Cancelled(CancelCause, Vec<Block>, Option<Usage>),
    /// The model's code panicked, with the panic's message. `started` says
    /// whether the stream had opened.
    Panicked {
        message: String,
        started: bool,
    },
}

impl Inner {
    /// Runs turn `turn` from `turn/start` to `turn/end`, and says whether
    /// the driver may go on to the next queued message.
    /// `caller` is the agent its tool calls are made by.
    pub(crate) async fn turn(&self, turn: u64, cancel: &Cancel, caller: &Arc<dyn Agent>) -> Ran {
        if self.session.append(SessionEvent::turn_start(turn)).is_err() {
            return Ran::NotStarted;
        }
        let (reason, ran) = match self.steps(turn, cancel, caller).await {
            Ok(reason) => (reason, Ran::Ended),
            Err(Stop::Aborted(cause)) => (TurnEndReason::Aborted(cause), Ran::Aborted),
            Err(Stop::Failed(failure)) => (TurnEndReason::Error(failure), Ran::Failed),
        };
        // If the log refuses even this, the turn stays open, and repair
        // closes it when the session is next opened.
        let _ = self.session.append(SessionEvent::turn_end(turn, reason));
        ran
    }

    /// Runs steps until one ends the turn with nothing left in next-step,
    /// and says how it ended. (dsh: the `while (true)` in `turn`,
    /// `agent.ts:310-352`.)
    async fn steps(
        &self,
        turn: u64,
        cancel: &Cancel,
        caller: &Arc<dyn Agent>,
    ) -> Result<TurnEndReason, Stop> {
        let mut step = 0;
        let mut target = InboxTarget::NextTurn;
        let mut ending: Option<TurnEndReason> = None;
        loop {
            if let Some(cause) = cancel.cause() {
                return Err(Stop::Aborted(cause));
            }
            // Each step takes all of next-step; a turn's first also takes
            // the oldest of next-turn (`agent.ts:271`).
            let input = self
                .claim(target, turn, caller.as_ref())
                .map_err(|error| Stop::Failed(other_failure(&error)))?;
            // The step's prompt and tools, assembled once, before the step
            // starts, as dsh's pre-step does (`agent.ts:272`). A failure
            // ends the turn with no step.
            let assembly = self
                .shared
                .prompt
                .assemble(&self.assemble_context())
                .await
                .map_err(|error| Stop::Failed(other_failure(&error)))?;
            // dsh's pre-step renders the context for its default decision
            // before the step starts, so a context that doesn't render ends
            // the turn here too (`agent.ts:277`).
            render_context_sections(&assembly)
                .map_err(|error| Stop::Failed(other_failure(&error)))?;
            if let Some(cause) = cancel.cause() {
                return Err(Stop::Aborted(cause));
            }
            // The pre-step hook may reject the step, or change what enters
            // it (`agent.ts:274-289`).
            let input = match self.pre_step(turn, step + 1, input, cancel, caller).await? {
                PreStepDecision::Reject => return Ok(TurnEndReason::Blocked),
                PreStepDecision::Enter { messages } => messages,
            };
            if let Some(reason) = ending.take_if(|_| input.is_empty()) {
                return Ok(reason);
            }
            if step == 0 && input.is_empty() {
                // A cancel drops what is pending, so one that lands before
                // the claim leaves nothing to take. It has flipped the switch
                // by then: both happen under the agent's lock.
                return match cancel.cause() {
                    Some(cause) => Err(Stop::Aborted(cause)),
                    // Nothing pending and nothing cancelled. dsh ends such
                    // a turn as completed, without a step
                    // (`agent.ts:326-329`).
                    None => Ok(TurnEndReason::Completed),
                };
            }
            step += 1;
            let started = self.session.append(SessionEvent::step_start(turn, step))?;
            let outcome = self
                .step(turn, step, input, &assembly, cancel, caller)
                .await;
            if outcome.is_err() {
                // If these are refused too, the step's own failure is still
                // the one to report, as in dsh's `AggregateError`.
                let _ = self.answer_pending_calls(started);
            }
            let ended = self.session.append(SessionEvent::step_end(turn, step));
            let step_ending = outcome?;
            ended?;
            if let Some(cause) = cancel.cause() {
                return Err(Stop::Aborted(cause));
            }
            // A `max-tokens` ending sticks: a later step of the turn that
            // completes, or calls tools, doesn't change it
            // (`agent.ts:338-343`).
            if !matches!(ending, Some(TurnEndReason::MaxTokens)) {
                ending = step_ending;
            }
            // Steering that arrived during the step keeps the turn going.
            // (dsh's `agent/turn-stopping` hook, which may add some, isn't
            // ported.)
            if let Some(reason) = ending.take_if(|_| !self.has_next_step()) {
                return Ok(reason);
            }
            target = InboxTarget::NextStep;
        }
    }

    /// Runs the pre-step hook for step `step`, which `input` was claimed
    /// for, raced against the turn's cancel. With no listener, the step
    /// enters with its input. (dsh: the `agent/pre-step` waterfall in
    /// `preStep`.)
    async fn pre_step(
        &self,
        turn: u64,
        step: u64,
        input: Vec<UserMessage>,
        cancel: &Cancel,
        caller: &Arc<dyn Agent>,
    ) -> Result<PreStepDecision, Stop> {
        let hooks = &self.shared.pre_step;
        if hooks.is_empty_for(self.session.id()) {
            return Ok(PreStepDecision::Enter { messages: input });
        }
        let proposed = PreStep {
            agent: Arc::clone(caller),
            messages: input,
            turn,
            step,
            cancel: cancel.clone(),
        };
        let decided = tokio::select! {
            biased;
            cause = cancel.cancelled() => return Err(Stop::Aborted(cause)),
            decided = CatchUnwind(hooks.run(&proposed)) => decided,
        };
        match decided {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(_)) | Err(_) if cancel.cause().is_some() => {
                Err(Stop::Aborted(cancel.cause().unwrap_or(CancelCause::User)))
            }
            Ok(Err(error)) => Err(Stop::Failed(other_failure(&*error))),
            // A dsh listener that throws ends the turn with its text.
            Err(Panicked(message)) => Err(Stop::Failed(panicked(message))),
        }
    }

    /// One step: the model call, then its tools. Returns how the turn ends,
    /// or `None` when tools ran and another step must follow.
    async fn step(
        &self,
        turn: u64,
        step: u64,
        input: Vec<UserMessage>,
        assembly: &PromptAssembly,
        cancel: &Cancel,
        caller: &Arc<dyn Agent>,
    ) -> Result<Option<TurnEndReason>, Stop> {
        // Rendered once for the step's attempts (`agent.ts:407`).
        let prompt =
            render_prompt(assembly).map_err(|error| Stop::Failed(other_failure(&error)))?;
        let response = self
            .call_model(turn, step, input, &prompt, assembly, cancel)
            .await?;

        // `karaka_model::FinishReason::Paused` says to treat a paused answer as
        // unfinished, like one cut off by the limit, until Karaka can resume
        // server-side tools.
        if matches!(
            response.finish,
            FinishReason::MaxTokens | FinishReason::Paused
        ) {
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
            if let Some(cause) = cancel.cause() {
                return Err(Stop::Aborted(cause));
            }
            // Logged as the call starts, so repair can tell a call that ran
            // from one that never started.
            let started = self
                .session
                .append(SessionEvent::tool_call(turn, step, call))?;
            let output = tokio::select! {
                biased;
                cause = cancel.cancelled() => return Err(Stop::Aborted(cause)),
                // A tool that panics gets an error result, as a dsh tool
                // that throws gets `Error: <message>`
                // (`core/tools/src/index.ts:1917-1924`).
                output = CatchUnwind(self.shared.tools.execute(call, Some(Arc::clone(caller)))) => output
                    .unwrap_or_else(|Panicked(message)| ToolOutput::error(format!("Error: {message}"))),
            };
            let result = output.into_result(&call.id);
            let data = ToolResultData::new(self.next_id("tool-result"), turn, step, &result);
            self.session
                .append(SessionEvent::tool_result(data, started))?;
        }
        Ok(None)
    }

    /// The step's model call, tried again for as long as the request-error
    /// hook asks. (dsh: the `while (true)` in `step`, `agent.ts:409-560`.)
    ///
    /// Each attempt does again what dsh's does: it updates the system prompt,
    /// looks up the model, logs the header and context if they are due, and
    /// builds the request from the log anew. Only the step's input is
    /// logged once, on the first attempt (`agent.ts:426-437`). A failed
    /// attempt is logged as an `assistant/attempt`, then the hook decides.
    async fn call_model(
        &self,
        turn: u64,
        step: u64,
        mut input: Vec<UserMessage>,
        prompt: &str,
        assembly: &PromptAssembly,
        cancel: &Cancel,
    ) -> Result<Response, Stop> {
        let tools = &assembly.tools;
        loop {
            // The runtime context, read again for this attempt; the rest of
            // the assembly stays as the step accepted it (`agent.ts:411`).
            let current = self
                .shared
                .prompt
                .refresh_context(assembly, &self.assemble_context());
            let sections = render_context_sections(&current)
                .map_err(|error| Stop::Failed(other_failure(&error)))?;
            // A cancel here, from a context provider or anyone, leaves the
            // step with nothing logged (`agent.ts:414`).
            if let Some(cause) = cancel.cause() {
                return Err(Stop::Aborted(cause));
            }
            self.update_system_prompt(turn, step, prompt)?;
            for message in std::mem::take(&mut input) {
                self.session.append(SessionEvent::user(message))?;
            }
            // After the input, as dsh's default pre-step places it, and only
            // when it changed (`agent.ts:278`, `413` and `425-438`).
            let snapshot = self
                .runtime_context
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .project(
                    &self.session.events(),
                    &join_context_sections(&sections),
                    &sections,
                    self.next_id("runtime-context"),
                );
            if let Some(snapshot) = snapshot {
                self.session.append(SessionEvent::user(snapshot))?;
            }

            let config = &self.shared.config;
            let model = self
                .shared
                .models
                .get(&config.route)
                .ok_or_else(|| Stop::Failed(no_model(&config.route)))?;
            let (call, effort) = self.call_config(model.provider());
            let History { system, messages } = self.session.history();
            let mut request = Request::new(config.model.clone(), messages);
            request.system = system;
            request.tools = tools.to_vec();
            request.max_tokens = config.max_tokens;
            request.effort = effort;
            self.log_request(call, &request.tools)?;
            // A cancel that came before the call started leaves no attempt,
            // as in dsh, where `buildRequest` checks the signal before the
            // stream starts (`agent.ts:684`).
            if let Some(cause) = cancel.cause() {
                return Err(Stop::Aborted(cause));
            }

            // Each attempt is one of its own, with an id of its own, for the
            // live frames (`live.rs`). Each settles by logging its event,
            // and only then sends its `end` frame.
            let mut live = LiveAttempt::new(
                &self.frames,
                &self.frame_counters,
                self.session.id().as_str(),
                turn,
                step,
            );
            match stream(model.as_ref(), request, cancel, &mut live).await {
                Streamed::Finished(response) => {
                    let mut data = AssistantMessageData::from_response(
                        self.next_id("assistant"),
                        turn,
                        step,
                        model.provider(),
                        &config.model,
                        &response,
                    );
                    // The finish record carries the replay state the message's
                    // source has, built once, from the assembled blocks.
                    data.stream = live.stream(data.message.source.replay_state.as_ref());
                    let logged = self.session.append(SessionEvent::assistant(data));
                    live.settle(SettledAs::AssistantMessage, logged)?;
                    return Ok(response);
                }
                // A call that left no message is logged as an attempt, so the
                // log shows it was made. (dsh: `agent.ts:489-510`.)
                Streamed::Failed(error) => {
                    let logged = self.session.append(attempt(turn, step, live.stream(None)));
                    live.settle(SettledAs::AssistantAttempt, logged)?;
                    drop(live);
                    self.recover(turn, step, model.as_ref(), model_failure(&error), cancel)
                        .await?;
                }
                // A stream that panics fails the turn under `UNKNOWN`, as one
                // that throws does in dsh, after logging the attempt if the
                // stream had opened (`agent.ts:462-502`). No handler is asked:
                // dsh asks only about a failure the model reported.
                Streamed::Panicked { message, started } => {
                    if started {
                        let logged = self.session.append(attempt(turn, step, live.stream(None)));
                        live.settle(SettledAs::AssistantAttempt, logged)?;
                    }
                    return Err(Stop::Failed(panicked(message)));
                }
                Streamed::Cancelled(cause, content, usage) => {
                    if content.is_empty() {
                        let logged = self.session.append(attempt(turn, step, live.stream(None)));
                        live.settle(SettledAs::AssistantAttempt, logged)?;
                    } else {
                        let logged = self.append_interrupted(
                            turn,
                            step,
                            model.provider(),
                            &content,
                            usage,
                            live.stream(None),
                        );
                        live.settle(SettledAs::AssistantMessage, logged)?;
                    }
                    return Err(Stop::Aborted(cause));
                }
            }
        }
    }

    /// Runs the request-error hook for a failed call. `Ok` means try again;
    /// otherwise the error is how the step ends. (dsh: `agent.ts:511-526`.)
    ///
    /// A cancel wins, during the chain or after it, even over a retry, as in
    /// dsh ("lets cancellation win over a retry action",
    /// `request-error.spec.ts:105`). A handler that fails or panics ends the
    /// turn with its own error, as a dsh listener that rejects does.
    async fn recover(
        &self,
        turn: u64,
        step: u64,
        model: &dyn Model,
        failure: Failure,
        cancel: &Cancel,
    ) -> Result<(), Stop> {
        let error = RequestError {
            session: self.session.clone(),
            turn,
            step,
            provider: model.provider().to_owned(),
            failure,
            retry_policy: model.retry_policy(),
            cancel: cancel.clone(),
        };
        let outcome = tokio::select! {
            biased;
            cause = cancel.cancelled() => return Err(Stop::Aborted(cause)),
            outcome = CatchUnwind(self.shared.hooks.run(&error)) => outcome,
        };
        if let Some(cause) = cancel.cause() {
            return Err(Stop::Aborted(cause));
        }
        match outcome {
            Ok(Ok(Some(RequestErrorAction::Retry))) => Ok(()),
            Ok(Ok(None)) => Err(Stop::Failed(error.failure)),
            Ok(Err(handler)) => Err(Stop::Failed(other_failure(handler.as_ref()))),
            Err(Panicked(_)) => Err(Stop::Failed(handler_panicked())),
        }
    }

    /// Logs the system prompt when the session has none yet, or when
    /// `wanted`, the step's rendered prompt, differs from the one in
    /// effect.
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
    fn update_system_prompt(&self, turn: u64, step: u64, wanted: &str) -> Result<(), LogError> {
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

    /// Logs the part of an answer that arrived before a cancel, with the
    /// usage if its chunk had come. (dsh: `agent.ts:465-482`.)
    fn append_interrupted(
        &self,
        turn: u64,
        step: u64,
        provider: &str,
        content: &[Block],
        usage: Option<Usage>,
        stream: Vec<Value>,
    ) -> Result<Seq, LogError> {
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
                stream,
                usage: usage.map(Into::into),
                interrupted: true,
                extra: Extra::new(),
            }))
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

    /// What this agent's assemblies are for: its session's scope.
    fn assemble_context(&self) -> AssembleContext {
        AssembleContext::scoped(Scope::new(self.session.id().as_str()))
    }

    /// An id for the message the next appended event will hold.
    fn next_id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.session.len())
    }
}

/// Streams one model call to its end, or until a cancel. A cancel drops the
/// stream, which closes its connection.
///
/// `live` sends the call's frames: `start` once the stream is open, then a
/// `chunk` for each event, before the loop acts on it, and one for the
/// failure that ends a failed stream.
async fn stream(
    model: &dyn Model,
    request: Request,
    cancel: &Cancel,
    live: &mut LiveAttempt<'_>,
) -> Streamed {
    let mut events = match panic::catch_unwind(AssertUnwindSafe(|| model.stream(request))) {
        Ok(events) => events,
        Err(payload) => {
            return Streamed::Panicked {
                message: panic_message(payload.as_ref()),
                started: false,
            };
        }
    };
    live.start();
    // dsh's assembler, which `complete` uses too, so the loop keeps the
    // same blocks and drops the same ones (`llm/src/assembler.ts`).
    let mut answer = BlockAssembler::new();
    loop {
        // `biased` checks the switch first, so a cancel wins over an event
        // that is ready at the same moment.
        let event = tokio::select! {
            biased;
            cause = cancel.cancelled() => {
                return Streamed::Cancelled(cause, answer.interrupted_blocks(), answer.usage());
            }
            event = CatchUnwind(poll_fn(|cx| events.as_mut().poll_next(cx))) => event,
        };
        let event = match event {
            Ok(event) => event,
            Err(Panicked(message)) => {
                return Streamed::Panicked {
                    message,
                    started: true,
                };
            }
        };
        // Every item goes out as a chunk before the loop acts on it: an
        // event, or the failure that ends the stream.
        let event = event.unwrap_or_else(|| {
            Err(ModelError::new(
                ErrorKind::Network,
                "the stream ended before the model finished",
            ))
        });
        live.push(match &event {
            Ok(event) => StreamChunk::Event(event.clone()),
            Err(error) => StreamChunk::Failed(error.clone()),
        });
        match event {
            Ok(event) => {
                answer.push(&event);
                // The finish is always the last chunk.
                if let Some(response) = answer.response() {
                    return Streamed::Finished(response);
                }
            }
            Err(error) => return Streamed::Failed(error),
        }
    }
}

/// The `assistant/attempt` for a call that left no message, with its
/// stream.
fn attempt(turn: u64, step: u64, stream: Vec<Value>) -> SessionEvent {
    SessionEvent::AssistantAttempt(AssistantAttemptData {
        turn,
        step,
        stream,
        extra: Extra::new(),
    })
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

/// Code the loop called panicked, with the panic's message.
struct Panicked(String);

/// The text a panic was raised with.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic with no message".to_owned()
    }
}

/// Runs a future, turning a panic in it into [`Panicked`], so a broken
/// handler, tool or model fails its call or its turn rather than the
/// agent's driver. The panic message is still printed by the panic hook.
struct CatchUnwind<F>(F);

impl<F: Future + Unpin> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Panicked>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.0;
        // Nothing of the future is used again after a panic, so whatever
        // state it was left in can't be seen.
        match panic::catch_unwind(AssertUnwindSafe(|| Pin::new(inner).poll(cx))) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(Panicked(panic_message(payload.as_ref())))),
        }
    }
}
