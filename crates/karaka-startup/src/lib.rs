//! Karaka's startup verdict: whether the ingress admits requests. (Karaka:
//! `karakaStartup`, `transport-http/src/startup.ts`.)
//!
//! Karaka's gate waits for the launcher's readiness signal, then audits
//! the Loader's settled tree: every enabled entry must be active, or it
//! logs `Karaka startup failed:` with each failure and exits 1. The kernel
//! starts every plugin or fails as a whole, so `Runtime::start` returning
//! is that audit passing, and a plugin that failed fails the start with its
//! error. The host [`commit`](KarakaStartup::commit)s the gate once
//! `Runtime::start` returns, and [`withdraw`](KarakaStartup::withdraw)s it
//! as it shuts down, as Karaka's disposal does.
//!
//! Until then the ingress answers 503 with [`STARTING_BODY`]
//! (`transport-http/src/index.ts` and `browser-routes.ts`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

/// The status a request gets before startup completes.
pub const STARTING_STATUS: u16 = 503;

/// Its content type, as Karaka's `json` helper writes it.
pub const STARTING_CONTENT_TYPE: &str = "application/json; charset=utf-8";

/// Its body, as `JSON.stringify` writes Karaka's
/// `{ code: 'STARTING', message: 'Application is starting' }`.
pub const STARTING_BODY: &str = r#"{"code":"STARTING","message":"Application is starting"}"#;

/// The port. (Karaka: `KarakaStartup`.)
#[derive(Debug, Default)]
pub struct KarakaStartup {
    ready: AtomicBool,
}

impl KarakaStartup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the ingress admits requests: false before startup and while
    /// shutting down. (Karaka: `ready`.)
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// Opens the gate: every plugin has started. (Karaka: the `onReady`
    /// callback, its audit passed.)
    pub fn commit(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    /// Closes it again, for shutdown. (Karaka: the effect's disposal.)
    pub fn withdraw(&self) {
        self.ready.store(false, Ordering::SeqCst);
    }
}

/// Provides [`KarakaStartup`], closed. (Karaka: the `karaka-startup`
/// plugin.)
#[derive(Debug, Default)]
pub struct KarakaStartupPlugin;

impl Plugin for KarakaStartupPlugin {
    fn name(&self) -> &str {
        "karaka-startup"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<KarakaStartup>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(Arc::new(KarakaStartup::new()));
        Ok(())
    }
}
