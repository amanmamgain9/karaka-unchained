//! The `request/header` and `request/context` the loop logs before each
//! model call. (dsh: `prepareRequest` and `buildRequest` in
//! `agent-loop/src/agent.ts:562-705`.)
//!
//! # The header
//!
//! The header records what a request is sent with: the provider, the model,
//! the effort and output limit the loop asked for, and every tool's schema.
//! The latest one is the header in force, so the log alone tells what each
//! request was built under. A loop logs one:
//!
//! - on its first request, with reason `initial` if the log has none yet,
//!   or `resume` if it has one, as after a restart;
//! - later, with reason `change`, when the header differs from the one in
//!   force;
//! - with reason `series`, when nothing changed but a new series of
//!   messages starts.
//!
//! A new series starts when part of the history was replaced since the last
//! request (dsh: the surface's `contentGeneration`). In Karaka today only
//! one thing replaces history: clearing the system prompt (see
//! `update_system_prompt`). dsh's other source, a `startsRequestSeries`
//! decision from the `agent/pre-step` hook, isn't ported. A `change` or
//! `resume` that coincides with a new series carries `startsSeries`.
//!
//! The header names the provider by its own name, as an answer's
//! `source.provider` does, where dsh names the route.
//!
//! # The context
//!
//! The context records the route's facts: its provider and model, its
//! context window, and how it takes a changed system prompt. It is logged
//! only when one of them differs from the last. Karaka's models advertise no
//! context window, so it is left out. The system prompt update mode is
//! always `in-history`, because that is how the loop updates a changed
//! prompt: it appends it where it changed.

use std::sync::atomic::Ordering;

use karaka_model::{Effort, ToolSchema};
use karaka_session::{
    CallConfig, Extra, IN_HISTORY, RequestContext, RequestHeader, RequestHeaderData,
    RequestHeaderReason, SessionEvent, header_equals,
};

use crate::agent::Inner;
use crate::failure::other_failure;
use crate::turn::Stop;

impl Inner {
    /// The config of the next request to `provider`, and the effort to ask
    /// for. (dsh: `prepareRequest`, `agent.ts:569-592`.)
    ///
    /// With no effort in the loop's config, the effort of the header in
    /// force carries on, if that header was for this provider and model and
    /// its effort wasn't a provider default. That is how a restarted loop
    /// keeps the effort a session ran at. An effort Karaka has no level for,
    /// such as DeepSeek's `off`, can't be sent, so it doesn't carry on.
    pub(crate) fn call_config(&self, provider: &str) -> (CallConfig, Option<Effort>) {
        let config = &self.shared.config;
        let (_, model, explicit) = self.selected();
        let effort = explicit.or_else(|| self.restored_effort(provider));
        let mut call = CallConfig::new(provider, &model);
        call.reasoning_effort = effort.map(|effort| effort_name(effort).to_owned());
        call.max_tokens = config.max_tokens.map(u64::from);
        (call, effort)
    }

    /// The route, model and explicit effort requests go to: the agent's
    /// selection, or the loop's config.
    pub(crate) fn selected(&self) -> (String, String, Option<Effort>) {
        let selection = self
            .selection
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match selection {
            Some(selection) => (
                selection.provider,
                selection.model,
                selection.reasoning_effort.as_deref().and_then(effort_named),
            ),
            None => (
                self.shared.config.route.clone(),
                self.shared.config.model.clone(),
                self.shared.config.effort,
            ),
        }
    }

    fn restored_effort(&self, provider: &str) -> Option<Effort> {
        let header = self.session.request_header()?;
        let (_, model, _) = self.selected();
        let same_route = header.config.provider == provider && header.config.model == model;
        let defaulted = header
            .adapter_defaults
            .is_some_and(|defaults| defaults.reasoning_effort);
        if !same_route || defaulted {
            return None;
        }
        effort_named(header.config.reasoning_effort.as_deref()?)
    }

    /// Logs the header and the context for a request with `call` and
    /// `tools`, where they are due. (dsh: `buildRequest`,
    /// `agent.ts:616-684`.)
    pub(crate) fn log_request(&self, call: CallConfig, tools: &[ToolSchema]) -> Result<(), Stop> {
        let provider = call.provider.clone();
        let model = call.model.clone();
        let header =
            RequestHeader::new(call, tools).map_err(|error| Stop::Failed(other_failure(&error)))?;
        let baseline = self.session.request_header();
        let generation = self.session.surface_generation();
        let starts_series = generation != self.request_generation.load(Ordering::Relaxed);
        let reason = if !self.header_logged.load(Ordering::Relaxed) {
            Some(match baseline {
                None => RequestHeaderReason::Initial,
                Some(_) => RequestHeaderReason::Resume,
            })
        } else if baseline.is_none_or(|baseline| !header_equals(&baseline, &header)) {
            Some(RequestHeaderReason::Change)
        } else if starts_series {
            Some(RequestHeaderReason::Series)
        } else {
            None
        };
        if let Some(reason) = reason {
            // dsh's tool updates go here: a `developer/message` with a
            // `tool-addition` or `tool-removal` block for each tool that
            // came or went since the header before. Karaka's history has no
            // block for them yet, so only the header records the change.
            self.session
                .append(SessionEvent::RequestHeader(RequestHeaderData {
                    header,
                    reason,
                    // `series` says it already.
                    starts_series: starts_series && reason != RequestHeaderReason::Series,
                    extra: Extra::new(),
                }))?;
        }
        self.header_logged.store(true, Ordering::Relaxed);
        self.request_generation.store(generation, Ordering::Relaxed);

        let context = RequestContext {
            provider,
            model,
            context_window: None,
            system_prompt_update: Some(IN_HISTORY.into()),
            extra: Extra::new(),
        };
        let changed = self.session.request_context().is_none_or(|last| {
            last.provider != context.provider
                || last.model != context.model
                || last.context_window != context.context_window
                || last.system_prompt_update != context.system_prompt_update
        });
        if changed {
            self.session.append(SessionEvent::RequestContext(context))?;
        }
        Ok(())
    }
}

/// dsh's name for an effort level, as its adapters write `reasoningEffort`:
/// `low`, `medium`, `high`, `xhigh` and `max`.
fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// The level dsh names `name`, if Karaka has one.
pub(crate) fn effort_named(name: &str) -> Option<Effort> {
    [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ]
    .into_iter()
    .find(|&effort| effort_name(effort) == name)
}

#[cfg(test)]
mod tests {
    use karaka_model::Effort;

    use super::{effort_name, effort_named};

    #[test]
    fn each_effort_reads_back_from_its_name() {
        for effort in [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ] {
            assert_eq!(effort_named(effort_name(effort)), Some(effort));
        }
        // DeepSeek's "thinking off" has no Karaka level.
        assert_eq!(effort_named("off"), None);
    }
}
