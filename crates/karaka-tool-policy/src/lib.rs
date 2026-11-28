//! Karaka's tool policy: a preset's allow and deny lists, as a restriction
//! on the preset's scope. (Karaka's own: `@karaka-ai/agent/tool-policy`,
//! `karaka/mcp-application/src/tool-policy.ts`.)
//!
//! The lists name tools the preset's agents inherit, the global ones.
//! `allow` keeps only the names it lists; `deny` masks the names it lists,
//! and wins over `allow`. An omitted list stays omitted, so it doesn't mask
//! anything; with neither, the plugin files nothing. The restriction reaches
//! every agent of the preset, and stays outside their own scopes' tools
//! (`karaka-tools`' `restrict`).
//!
//! The names are checked when the preset is set up, against the global
//! tools registered by then, with `restrict`'s errors: a name no tool has
//! breaks the preset. So the presets' plugin has to start after every plugin
//! registering a tool a policy names.
//!
//! Only the `native` presentation is ported. Karaka's `ptc` mode, and the
//! split of application tools out of the lists (`karakaApplicationTools`,
//! `registerPolicy`), wait on the MCP client.

use karaka_agent_presets::{PresetContext, PresetFuture, PresetPlugin};
use karaka_kernel::PortId;
use karaka_scope::ScopeChain;
use karaka_tools::{ToolRegistry, ToolRestriction};

/// How the preset's agents are shown their tools. (Karaka's `mode`.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolMode {
    /// Each tool as its own schema, the only mode ported.
    #[default]
    Native,
}

/// The policy one preset's agents run under. (Karaka's `Config`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    pub mode: ToolMode,
    /// The inherited tools that stay visible; `None` keeps them all.
    pub allow: Option<Vec<String>>,
    /// The inherited tools masked; `None` masks none.
    pub deny: Option<Vec<String>>,
}

impl PresetPlugin for ToolPolicy {
    fn name(&self) -> &str {
        "tool-policy"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ToolRegistry>()]
    }

    fn setup<'a, 'p>(&'a self, ctx: &'a PresetContext<'a, 'p>) -> PresetFuture<'a> {
        Box::pin(async move {
            if self.allow.is_none() && self.deny.is_none() {
                return Ok(());
            }
            let preset = ScopeChain::new(Some(ctx.preset().clone()), None);
            // For the runtime's life, as the preset's other registrations.
            let _ = ctx.get::<ToolRegistry>().restrict(
                &preset,
                ToolRestriction {
                    allow: self.allow.clone(),
                    deny: self.deny.clone(),
                },
            )?;
            Ok(())
        })
    }
}
