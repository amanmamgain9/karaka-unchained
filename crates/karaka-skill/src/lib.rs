//! Skills: reusable task instructions an agent loads only when it needs
//! them. (dsh: `packages/skill/skill`, `dsh-skill`.)
//!
//! [`SkillRegistry`] merges the catalogs of its providers, such as local
//! skill directories, and of skills registered at runtime, resolves the
//! winning skill for each name, and loads its body on demand. Where skills
//! come from is the providers' business; how the model sees them is the
//! `skill` tool's.
//!
//! - `types.rs`: names, summaries, candidates, definitions, registrations,
//!   and the model-facing `<skill_content>` rendering.
//! - `registry.rs`: the registry, its provider trait, layers and cache.
//! - `plugin.rs`: the plugin that provides it.

mod error;
mod plugin;
mod registry;
mod types;

pub use error::{ABORTED, SkillError};
pub use plugin::SkillsPlugin;
pub use registry::{
    ChangeListener, ChangeRegistration, DEFAULT_COLLECT_CACHE_ENTRIES, GetFuture, ListFuture,
    ProviderError, ProviderRegistration, SkillConfig, SkillHandle, SkillProvider,
    SkillProviderControl, SkillRegistry,
};
pub use types::{
    BUNDLED_SKILL_RANK, Invocable, RUNTIME_PROVIDER, RUNTIME_RANK, SKILL_INVOCATION_KIND,
    SkillCandidate, SkillCatalogSnapshot, SkillDefinition, SkillInvocationPolicy, SkillLocator,
    SkillLookupOptions, SkillProviderObservation, SkillRegistration, SkillResourceBase,
    SkillSummary, escape_text, is_skill_name, render_skill_content, skill_invocation_source,
    source,
};
