//! The model an agent uses when nothing chose another. (dsh:
//! `@deepseek-ai/dsh-agent-default-model`, `core/agent-default-model`.)
//!
//! dsh's config is volatile and settings-backed: the selection can change
//! while the runtime runs, and `saveSelection` writes it back through the
//! config editor. Neither is ported; the selection is fixed at start
//! (open ends).
//!
//! It also holds the two `llm` checks Karaka's application routes make,
//! over Karaka's model registry: [`list_providers`] (`listProviders`) and
//! [`resolve_call_config`] (`resolveCallConfig`).

use std::fmt;
use std::sync::Arc;

use karaka_agent::ModelSelection;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::ModelRegistry;

/// The port. (dsh: `AgentDefaultModelConfig`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefaultModel {
    selection: ModelSelection,
}

impl AgentDefaultModel {
    pub fn new(selection: ModelSelection) -> Self {
        Self { selection }
    }

    /// A detached copy of the selection. (dsh: `currentSelection`.)
    pub fn current_selection(&self) -> ModelSelection {
        self.selection.clone()
    }
}

/// Provides [`AgentDefaultModel`]. (dsh: the `agent-default-model` row,
/// with `provider`, `model` and `reasoningEffort`.)
#[derive(Debug)]
pub struct AgentDefaultModelPlugin {
    selection: ModelSelection,
}

impl AgentDefaultModelPlugin {
    pub fn new(selection: ModelSelection) -> Self {
        Self { selection }
    }
}

impl Plugin for AgentDefaultModelPlugin {
    fn name(&self) -> &str {
        "agent-default-model"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<AgentDefaultModel>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(Arc::new(AgentDefaultModel::new(self.selection.clone())));
        Ok(())
    }
}

/// The provider routes with a model registered, in order. (dsh:
/// `llm.listProviders`, its ids.)
pub fn list_providers(registry: &ModelRegistry) -> Vec<String> {
    registry.routes()
}

/// Why a call config doesn't resolve. (dsh: `LlmError`, its message and
/// code.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmError {
    pub message: String,
    pub code: &'static str,
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LlmError {}

/// The efforts Karaka's providers take, by their dsh names.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Checks a selection against the registry, as dsh's `resolveCallConfig`
/// checks it against its adapters: the provider must have a model
/// registered, and an explicit effort must be one Karaka can send.
///
/// Karaka's models publish no catalog, so the model id isn't checked and
/// no default effort is filled in: dsh materializes the adapter's default
/// effort into the result, and rejects an effort the exact model lacks.
///
/// # Errors
///
/// In dsh's words: `no adapter registered for provider "x"` (`NO_ADAPTER`),
/// or `provider "x" model "y" does not support reasoning effort "z"`
/// (`UNSUPPORTED_REASONING_EFFORT`).
pub fn resolve_call_config(
    registry: &ModelRegistry,
    selection: ModelSelection,
) -> Result<ModelSelection, LlmError> {
    if registry.get(&selection.provider).is_none() {
        return Err(LlmError {
            message: format!(
                "no adapter registered for provider \"{}\"",
                selection.provider
            ),
            code: "NO_ADAPTER",
        });
    }
    if let Some(effort) = &selection.reasoning_effort
        && !EFFORTS.contains(&effort.as_str())
    {
        return Err(LlmError {
            message: format!(
                "provider \"{}\" model \"{}\" does not support reasoning effort \"{effort}\"",
                selection.provider, selection.model
            ),
            code: "UNSUPPORTED_REASONING_EFFORT",
        });
    }
    Ok(selection)
}
