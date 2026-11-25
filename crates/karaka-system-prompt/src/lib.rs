//! The system prompt seam: the registry of what each model step is told.
//! (dsh: `@deepseek-ai/dsh-system-prompt`, 0.2.1-rc.2.)
//!
//! [`SystemPrompt`] collects contributions from every plugin:
//!
//! - **Sections** of the prompt, static or worked out per assembly, each
//!   with an `order`. They join in ascending order, equal orders by name
//!   in UTF-16 code-unit order. A complete section is the whole prompt.
//! - **Variables**, written `{{name}}` in sections and context. Every
//!   reference must resolve, or rendering fails.
//! - **Tool sources**, whose schemas the assembly lists by name, or in the
//!   configured order.
//! - **Runtime context**, which the loop shows the model as a user-role
//!   snapshot, and can be suppressed.
//! - **Listeners**, which shape each assembly before it is returned.
//!
//! A contribution is global, or belongs to one agent's [`Scope`], where it
//! shadows a global one of the same name for that agent alone.
//!
//! The plugin's own sections are the identity line, [`HARNESS_IDENTITY`],
//! and the deployment persona's prefix and suffix, from
//! [`SystemPromptConfig`].
//!
//! One crate, the seam and its only implementation together: the registry
//! is concrete, like the session store, with nothing behind it to swap.
//!
//! - `registry.rs`: the registry, its contributions, and assembly.
//! - `assembly.rs`: an assembly, and how it renders.
//! - `plugin.rs`: the plugin that provides the port.

mod assembly;
mod plugin;
mod registry;

pub use assembly::{
    AssembledContext, AssembledSection, ContextSection, PromptAssembly, PromptError, Variables,
    join_context_sections, render_context_sections, render_context_snapshot, render_prompt,
};
pub use plugin::SystemPromptPlugin;
pub use registry::{
    AssembleContext, AssembleFuture, AssembleListener, AssembleNext, ContextOrder,
    HARNESS_IDENTITY, HARNESS_IDENTITY_SECTION, PERSONA_PREFIX_SECTION, PERSONA_SUFFIX_SECTION,
    PromptContext, Registration, Scope, Section, SectionOrder, SystemPrompt, SystemPromptConfig,
    TOOL_ORDER_REST, Text, ToolProviderResult,
};
