//! The `skill` tool, the session's skill catalog, and `/name` invocations.
//! (dsh: `@deepseek-ai/dsh-tool-skill`, 0.2.1-rc.2, `src/index.ts`.)
//!
//! Three things, all through the skill registry and as the calling agent
//! sees it, from its working directory and its scope:
//!
//! - The [`SkillTool`], named `skill`: the model loads one skill's full
//!   instructions, which come back as the tool's result, framed by
//!   [`render_skill_content`](karaka_skill::render_skill_content).
//! - The catalog: before each step, the model-invocable skills, one line
//!   each, join the step as a `skill-catalog` user message, when they
//!   differ from what the session last saw. It is a message, not a section
//!   of the system prompt, so the prompt stays the same as skills come and
//!   go.
//! - Invocations: a `/name` a person writes in a prompt loads that skill,
//!   when they may invoke it, into the step, as a `skill-invocation` user
//!   message after every other.
//!
//! Both of the last two are listeners of the pre-step hook. The invocation
//! listener is registered before the catalog listener, so the catalog's
//! answer is what it extends, and an invoked skill comes last, closest to
//! the model's answer.
//!
//! dsh publishes the catalog only while the calling agent resolves this
//! plugin's own `skill` tool, so an agent whose tools are restricted, or
//! that has a scoped tool of the same name, gets no catalog. Karaka's tool
//! registry has no per-agent tools yet, and never replaces a tool, so the
//! tool is always this one and the catalog is always published.

mod catalog;
mod tool;

use std::fmt;
use std::sync::Arc;

use karaka_agent::PreStepHooks;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::SessionStore;
use karaka_skill::SkillRegistry;
use karaka_tools::ToolRegistry;
use karaka_working_directory::WorkingDirectory;

pub use catalog::{
    CATALOG_KIND, Entry, catalog_description, catalog_message, catalog_update, invoked_skill_names,
};
pub use tool::{DESCRIPTION, NAME, PARAMETERS, SkillTool};

use catalog::{CatalogListener, InvocationListener, Shared};

/// The catalog's default description length. (dsh:
/// `DEFAULT_CATALOG_DESCRIPTION_MAX_LENGTH`.)
pub const DEFAULT_CATALOG_DESCRIPTION_MAX_LENGTH: usize = 500;

/// The shortest description length allowed: room for `...`.
const MIN_CATALOG_DESCRIPTION_MAX_LENGTH: usize = 3;

/// The plugin's settings. (dsh: `Config`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolSkillConfig {
    /// The longest description a catalog line shows, after its whitespace
    /// is folded, in UTF-16 units; at least 3. (dsh:
    /// `catalogDescriptionMaxLength`.)
    pub catalog_description_max_length: usize,
}

impl Default for ToolSkillConfig {
    fn default() -> Self {
        Self {
            catalog_description_max_length: DEFAULT_CATALOG_DESCRIPTION_MAX_LENGTH,
        }
    }
}

/// Why the plugin refused its config. (dsh: `assertPositiveInteger`'s
/// error; a Rust `usize` is always an integer, so only the minimum is
/// left to check.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError;

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tool-skill: catalogDescriptionMaxLength must be an integer greater than or equal to \
             {MIN_CATALOG_DESCRIPTION_MAX_LENGTH}"
        )
    }
}

impl std::error::Error for ConfigError {}

/// Registers the `skill` tool, then the invocation listener, then the
/// catalog listener. (dsh: the plugin's `apply`, with `inject` of `agents`,
/// `tools`, `skills` and `workingDirectory`.)
///
/// dsh's registrations are undone when its plugin unloads. Karaka's kernel
/// unloads no plugin, so they stay for the runtime's life.
#[derive(Clone, Copy, Debug, Default)]
pub struct ToolSkillPlugin {
    config: ToolSkillConfig,
}

impl ToolSkillPlugin {
    pub fn new(config: ToolSkillConfig) -> Self {
        Self { config }
    }
}

impl Plugin for ToolSkillPlugin {
    fn name(&self) -> &str {
        "tool-skill"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<ToolRegistry>(),
            PortId::of::<SkillRegistry>(),
            PortId::of::<WorkingDirectory>(),
            PortId::of::<PreStepHooks>(),
            PortId::of::<SessionStore>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let max_length = self.config.catalog_description_max_length;
        if max_length < MIN_CATALOG_DESCRIPTION_MAX_LENGTH {
            return Err(ConfigError.into());
        }
        let skills = ctx.get::<SkillRegistry>();
        let directories = ctx.get::<WorkingDirectory>();
        ctx.get::<ToolRegistry>().register(Arc::new(SkillTool::new(
            Arc::clone(&skills),
            Arc::clone(&directories),
        )))?;
        let shared = Arc::new(Shared {
            skills,
            directories,
            sessions: ctx.get::<SessionStore>(),
            description_max_length: max_length,
        });
        let hooks = ctx.get::<PreStepHooks>();
        hooks.register(None, Arc::new(InvocationListener(Arc::clone(&shared))));
        hooks.register(None, Arc::new(CatalogListener(shared)));
        Ok(())
    }
}
