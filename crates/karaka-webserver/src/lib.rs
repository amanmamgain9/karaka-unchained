//! The HTTP server other plugins mount routes on. (dsh:
//! `@deepseek-ai/dsh-host-webserver`, `host/webserver/src/index.ts`.)
//!
//! It knows nothing of agents or sessions and serves no files. Plugins
//! register named routes, `exact` or `prefix`, upgrade routes, and one
//! fallback, on the [`WebServer`] port. A request is matched exactly, then
//! by the longest prefix, then by the fallback, and with none it is a 404
//! with no body. An upgrade matches its path exactly, and with no route
//! its connection is closed.
//!
//! A handler owns its response: it returns one, whose body may stay open
//! ([`body::channel`], for SSE). An error answers 400 with no body, or cuts
//! the connection once the body is under way, and is logged; the server
//! keeps serving. An upgrade handler answers `101` and gives the
//! connection to [`Upgrades::adopt`], which a WebSocket takes over.
//!
//! The listener is HTTP/1.1, as `node:http`'s. dsh's TLS listener, gzip,
//! and index injections and taps, which serve the web shell's SPA, aren't
//! ported (open ends).

mod address;
pub mod body;
mod server;

use std::sync::{Mutex, PoisonError};

use karaka_kernel::{Plugin, PluginContext, PortId};

pub use address::{
    bind_url_host, check_host, is_loopback_host, is_wildcard_host, normalize_bind_address,
};
pub use body::{Body, BodySender, Closed};
pub use hyper::upgrade::{OnUpgrade, Upgraded, on as on_upgrade};
pub use hyper_util::rt::TokioIo;
pub use server::{
    Handler, HandlerFuture, Log, Request, Response, RouteKind, RouteRegistration, UpgradeHandler,
    UpgradeRoute, Upgrades, WebRoute, WebServer, WebServerConfig, pathname,
};

/// Any error a handler fails with.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Listens, and provides [`WebServer`]. Its setup binds the socket, so a
/// port in use fails the start. (dsh: the `webserver` row.)
pub struct WebServerPlugin {
    config: WebServerConfig,
    log: Mutex<Option<Log>>,
}

impl WebServerPlugin {
    pub fn new(config: WebServerConfig) -> Self {
        Self {
            config,
            log: Mutex::new(None),
        }
    }

    /// Tells `log` the server's warnings, which are dropped otherwise.
    #[must_use]
    pub fn on_log(self, log: Log) -> Self {
        *self.log.lock().unwrap_or_else(PoisonError::into_inner) = Some(log);
        self
    }
}

impl Plugin for WebServerPlugin {
    fn name(&self) -> &str {
        "webserver"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<WebServer>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), karaka_kernel::BoxError> {
        let log = self
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let server = WebServer::listen(self.config.clone(), log).await?;
        ctx.provide(std::sync::Arc::new(server));
        Ok(())
    }
}
