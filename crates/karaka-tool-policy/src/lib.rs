//! Karaka's tool policy: a preset's allow and deny lists, over the global
//! tools its agents inherit and the application tools they're given.
//! (Karaka's own: `@karaka-ai/agent/tool-policy`,
//! `karaka/mcp-application/src/tool-policy.ts`.)
//!
//! The lists are split by the application catalog's names. The global
//! names become a restriction on the preset's scope: `allow` keeps only
//! the names it lists; `deny` masks the names it lists, and wins over
//! `allow`. The whole lists become the preset's application policy, which
//! an application tool needs an explicit allow from
//! (`karaka-mcp-application`'s [`ApplicationPolicies`]). An omitted list
//! stays omitted, so it doesn't mask anything; with neither, the plugin
//! files nothing.
//!
//! The global names are checked when the preset is set up, against the
//! global tools registered by then, with `restrict`'s errors: a name no
//! tool has breaks the preset. So the presets' plugin has to start after
//! every plugin registering a tool a policy names, and after every
//! application endpoint: Karaka's policy waits for the catalog
//! (`karakaApplicationTools`), and here an unsettled catalog breaks the
//! preset instead, as the kernel starts plugins one at a time and a
//! catalog unsettled by then never settles.
//!
//! Only the `native` presentation is ported. Karaka's `ptc` mode waits on
//! dsh's tool presentation.

use karaka_agent_presets::{PresetContext, PresetFuture, PresetPlugin};
use karaka_kernel::PortId;
use karaka_mcp_application::{ApplicationCatalog, ApplicationPolicies, ApplicationToolPolicy};
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
        vec![
            PortId::of::<ToolRegistry>(),
            PortId::of::<ApplicationCatalog>(),
            PortId::of::<ApplicationPolicies>(),
        ]
    }

    fn setup<'a, 'p>(&'a self, ctx: &'a PresetContext<'a, 'p>) -> PresetFuture<'a> {
        Box::pin(async move {
            if self.allow.is_none() && self.deny.is_none() {
                return Ok(());
            }
            let catalog = ctx.get::<ApplicationCatalog>();
            let Some(application) = catalog.tools() else {
                let pending: Vec<String> = catalog
                    .pending()
                    .iter()
                    .map(|server| format!("\"{server}\""))
                    .collect();
                return Err(format!(
                    "tool-policy: application catalog servers have not settled: {}",
                    pending.join(", ")
                )
                .into());
            };
            let global = |names: &Vec<String>| -> Vec<String> {
                names
                    .iter()
                    .filter(|name| !application.contains(*name))
                    .cloned()
                    .collect()
            };
            let preset = ScopeChain::new(Some(ctx.preset().clone()), None);
            // For the runtime's life, as the preset's other registrations.
            let _ = ctx.get::<ToolRegistry>().restrict(
                &preset,
                ToolRestriction {
                    allow: self.allow.as_ref().map(global),
                    deny: self.deny.as_ref().map(global),
                },
            )?;
            let _ = ctx.get::<ApplicationPolicies>().register(
                Some(ctx.scope()),
                ApplicationToolPolicy {
                    allow: self.allow.clone(),
                    deny: self.deny.clone(),
                },
            )?;
            Ok(())
        })
    }
}
