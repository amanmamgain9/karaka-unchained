//! Agent presets: compositions of tools, prompt and skills that an agent
//! runs, a registry that lists and resolves them, and each session's
//! choice of one. (dsh: `@deepseek-ai/dsh-agent-preset-registry`, and
//! `@deepseek-ai/dsh-agent-preset`, the row that declares a preset.)
//!
//! A preset is compiled in: a [`PresetDefinition`] whose [`PresetPlugin`]s
//! are set up once, when the runtime starts, against the preset's scope
//! (`karaka-scope`'s `ScopeKey::Preset`). They register what the preset's
//! agents see under that scope. An agent's chain names its session's
//! preset, so it sees the global registrations, its preset's, then its
//! session's own.
//!
//! - `definition.rs`: a definition, a preset plugin, and what it sees.
//! - `session.rs`: the `agent-preset/selected` event and the `agentPreset`
//!   projection.
//! - `registry.rs`: the registry: list, resolve, select, and the composer
//!   the loop asks.
//! - `plugin.rs`: the plugin that sets presets up.
//!
//! # What dsh has that this doesn't
//!
//! dsh's presets are YAML rows a running process can add, replace and
//! remove. Each change makes a new revision of the preset's plugin tree;
//! live agents keep the revision they joined, and a retired revision is
//! collected when its last user goes. Karaka's plugins are fixed when the
//! runtime starts, so there is one revision per preset, for the runtime's
//! life, and none of this is ported: revisions and generations, `retire`
//! and `collect`, rebinding a live tree, `acquireScope`'s lease. Nor is
//! what reads a Cordis tree: `readDocument`'s YAML dump of the rows,
//! `compositionInventory`, `inspectCompositions` and `serviceFor`.

mod definition;
mod plugin;
mod registry;
mod session;

pub use definition::{PresetContext, PresetDefinition, PresetFuture, PresetPlugin};
pub use plugin::AgentPresetsPlugin;
pub use registry::{
    AgentPreset, AgentPresetRoster, AgentPresetRow, AgentPresets, AgentPresetsConfig, PresetError,
};
pub use session::{AgentPresetProjection, SELECTED_EVENT, selected};
