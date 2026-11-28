//! A preset's definition, and the plugins it is made of. (dsh:
//! `PresetDefinition` in `agent-preset-registry/src/definition.ts`, and the
//! `@deepseek-ai/dsh-agent-preset` row that declares one.)
//!
//! dsh declares a preset as a YAML row whose `plugins` are Cordis entries,
//! loaded into a registry-owned scope at run time. A Karaka preset is
//! compiled in: its plugins are Rust values, set up once when the runtime
//! starts.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use karaka_kernel::{BoxError, PluginContext, PortId};
use karaka_scope::{PresetId, ScopeKey};

/// One preset: its identity, display fields and plugins. (dsh:
/// `PresetDefinition`.)
pub struct PresetDefinition {
    /// Stable, and unique among the runtime's presets.
    pub id: String,
    /// What a client shows; the id when absent.
    pub name: Option<String>,
    /// One sentence on what the preset is for.
    pub description: Option<String>,
    /// Where it lists: lower first, presets with none last.
    pub order: Option<f64>,
    /// Set up in this order, each against the preset's scope.
    pub plugins: Vec<Box<dyn PresetPlugin>>,
}

impl PresetDefinition {
    /// A preset of `plugins`, with no display fields.
    pub fn new(id: impl Into<String>, plugins: Vec<Box<dyn PresetPlugin>>) -> Self {
        Self {
            id: id.into(),
            name: None,
            description: None,
            order: None,
            plugins,
        }
    }
}

impl fmt::Debug for PresetDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plugins: Vec<&str> = self.plugins.iter().map(|plugin| plugin.name()).collect();
        f.debug_struct("PresetDefinition")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("description", &self.description)
            .field("order", &self.order)
            .field("plugins", &plugins)
            .finish()
    }
}

/// What a preset plugin's setup resolves to.
pub type PresetFuture<'a> = Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'a>>;

/// One plugin of a preset: a dsh preset row. It is set up once, when the
/// runtime starts, and registers what the preset's agents see under the
/// preset's scope: tools, prompt sections, skills, listeners.
///
/// It reads host ports through its [`PresetContext`], and can't provide
/// one. That rules out by construction what dsh's mount audits after the
/// fact (`leakedServices`): a preset publishing a process-global service.
pub trait PresetPlugin: Send + Sync {
    /// The plugin's name, as a diagnostic names it. (dsh: the row's
    /// `name`.)
    fn name(&self) -> &str;

    /// The row's id in a diagnostic; its name by default. (dsh: the row's
    /// `id`, which the Loader makes up when the row gives none.)
    fn id(&self) -> &str {
        self.name()
    }

    /// The host ports its setup reads. The registry's plugin requires them
    /// all, so the kernel starts their providers first, and a port no
    /// plugin provides fails the start.
    fn requires(&self) -> Vec<PortId> {
        Vec::new()
    }

    /// Registers the plugin's contributions, each under
    /// [`PresetContext::scope`]. A failure breaks the preset.
    fn setup<'a, 'p>(&'a self, ctx: &'a PresetContext<'a, 'p>) -> PresetFuture<'a>;
}

/// What a preset plugin sees while it is set up: the host ports it listed
/// in [`PresetPlugin::requires`], and its preset's scope. It can't provide a
/// port.
pub struct PresetContext<'a, 'p> {
    host: &'a PluginContext<'p>,
    preset: PresetId,
    scope: ScopeKey,
    plugin: String,
    requires: Vec<PortId>,
}

impl<'a, 'p> PresetContext<'a, 'p> {
    pub(crate) fn new(
        host: &'a PluginContext<'p>,
        preset: &str,
        plugin: &dyn PresetPlugin,
    ) -> Self {
        let preset = PresetId::new(preset);
        Self {
            host,
            scope: ScopeKey::Preset(preset.clone()),
            preset,
            plugin: plugin.name().to_owned(),
            requires: plugin.requires(),
        }
    }

    /// Reads a host port this plugin listed in `requires`.
    ///
    /// # Panics
    ///
    /// If the port isn't listed: a bug in the plugin, as the kernel's
    /// `PluginContext::get` treats it.
    pub fn get<T: ?Sized + Send + Sync + 'static>(&self) -> Arc<T> {
        let port = PortId::of::<T>();
        assert!(
            self.requires.contains(&port),
            "preset plugin \"{}\" reads {port:?} without listing it in requires()",
            self.plugin,
        );
        self.host.get::<T>()
    }

    /// The preset's scope: what every registration of the plugin is filed
    /// under.
    pub fn scope(&self) -> &ScopeKey {
        &self.scope
    }

    /// The preset being set up.
    pub fn preset(&self) -> &PresetId {
        &self.preset
    }
}
