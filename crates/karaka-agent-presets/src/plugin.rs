//! [`AgentPresetsPlugin`]: sets every preset up when the runtime starts,
//! and provides the registry. (dsh: the registry's constructor, `register`
//! and `activate`, and `mountPreset` in `mount.ts`.)

use std::sync::Arc;

use karaka_agent::AgentComposition;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session_projection::SessionProjections;

use crate::definition::{PresetContext, PresetDefinition};
use crate::registry::{AgentPresets, AgentPresetsConfig, PresetError, Record};
use crate::session::AgentPresetProjection;

/// Sets up each preset's plugins under its scope, provides [`AgentPresets`],
/// registers the `agentPreset` projection, and makes the registry the
/// loop's agent composer.
///
/// It requires the session projections, the loop's [`AgentComposition`],
/// and every port its presets' plugins require, so the kernel starts all
/// of their providers first. A port no plugin provides fails the start.
/// dsh's mount instead keeps a row waiting for a missing service and
/// reports the preset broken until the service appears.
///
/// # Startup
///
/// Definitions are checked first: an empty id, or two definitions with one
/// id, fails the start with dsh's error. dsh's `register` throws them to
/// the declaring row and leaves the other presets standing; a compiled-in
/// definition is a programming mistake, so here the runtime doesn't start.
///
/// Then each preset's plugins are set up in order. A plugin that fails
/// breaks its preset, with one diagnostic line per failed plugin, `id
/// (name): message`, as dsh's audit writes them; the others are still set
/// up, as dsh's tree mounts every row, and every other preset stands. A
/// broken preset stays in the roster, and no session can run it.
///
/// dsh disposes a broken preset's scope, which takes back everything its
/// rows registered. Karaka's kernel has no teardown, so what a broken
/// preset's plugins registered before failing stays: under the preset's
/// scope, unreachable, since no agent's chain can name a broken preset;
/// and anything a plugin registered globally, which stays live.
pub struct AgentPresetsPlugin {
    config: AgentPresetsConfig,
    definitions: Vec<PresetDefinition>,
}

impl AgentPresetsPlugin {
    pub fn new(config: AgentPresetsConfig) -> Self {
        Self {
            config,
            definitions: Vec::new(),
        }
    }

    /// Adds `definition`, after those added before.
    #[must_use]
    pub fn preset(mut self, definition: PresetDefinition) -> Self {
        self.definitions.push(definition);
        self
    }
}

impl Plugin for AgentPresetsPlugin {
    fn name(&self) -> &str {
        "agent-preset-registry"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<AgentPresets>()]
    }

    fn requires(&self) -> Vec<PortId> {
        let mut ports = vec![
            PortId::of::<SessionProjections>(),
            PortId::of::<AgentComposition>(),
        ];
        for plugin in self
            .definitions
            .iter()
            .flat_map(|definition| &definition.plugins)
        {
            for port in plugin.requires() {
                if !ports.contains(&port) {
                    ports.push(port);
                }
            }
        }
        ports
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let mut seen: Vec<&str> = Vec::new();
        for definition in &self.definitions {
            if definition.id.trim().is_empty() {
                return Err(PresetError::EmptyId.into());
            }
            if seen.contains(&definition.id.as_str()) {
                return Err(PresetError::Duplicate(definition.id.clone()).into());
            }
            seen.push(&definition.id);
        }
        let projections = ctx.get::<SessionProjections>();
        // For the runtime's life, as dsh's registry registers it.
        let _ = projections.register(AgentPresetProjection)?;
        let mut records = Vec::new();
        for definition in &self.definitions {
            let mut failed = Vec::new();
            for plugin in &definition.plugins {
                let preset = PresetContext::new(ctx, &definition.id, plugin.as_ref());
                if let Err(error) = plugin.setup(&preset).await {
                    failed.push(format!("{} ({}): {error}", plugin.id(), plugin.name()));
                }
            }
            records.push(Record {
                id: definition.id.clone(),
                name: definition.name.clone(),
                description: definition.description.clone(),
                order: definition.order,
                broken: (!failed.is_empty()).then(|| failed.join("\n")),
            });
        }
        let presets = Arc::new(AgentPresets::new(self.config.clone(), records, projections));
        ctx.get::<AgentComposition>()
            .set(Arc::clone(&presets) as Arc<dyn karaka_agent::AgentComposer>)?;
        ctx.provide(presets);
        Ok(())
    }
}
