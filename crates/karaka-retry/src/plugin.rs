//! [`RetryPlugin`], which registers the [`RetryHandler`] in the loop's
//! request-error hook. (dsh: `llm-retry`'s `apply`.)

use std::sync::Arc;

use karaka_agent::RequestErrorHooks;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

use crate::{Random, RetryHandler};

/// Registers a [`RetryHandler`] in the [`RequestErrorHooks`] port.
///
/// It has no config: each provider owns its retry policy, as in dsh, whose
/// plugin refuses a `retryPolicy` of its own. The loop's plugin provides
/// the port, so this one starts after it. A handler registered before this
/// one's runs first, and one registered after runs when this one hands a
/// failure on.
pub struct RetryPlugin {
    random: Option<Random>,
}

impl RetryPlugin {
    /// A plugin whose jitter is random.
    pub fn new() -> Self {
        Self { random: None }
    }

    /// A plugin whose jitter draws from `random`, a number from 0 to 1, so
    /// tests can fix the delays. (dsh: `RetryInternals.random`.)
    pub fn with_random(random: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self {
            random: Some(Arc::new(random)),
        }
    }
}

impl Default for RetryPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RetryPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryPlugin")
            .field("fixed_random", &self.random.is_some())
            .finish()
    }
}

impl Plugin for RetryPlugin {
    fn name(&self) -> &str {
        "retry"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<RequestErrorHooks>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let handler = match &self.random {
            Some(random) => RetryHandler::new(Arc::clone(random)),
            None => RetryHandler::with_system_random(),
        };
        ctx.get::<RequestErrorHooks>().register(Arc::new(handler));
        Ok(())
    }
}
