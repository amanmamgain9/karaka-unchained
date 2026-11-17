//! Karaka's kernel: ports, plugins, and the runtime that starts them.
//!
//! Every other Karaka crate builds on this one. Backends, model providers and
//! the main loop are all plugins that provide or use ports.

mod plugin;
mod port;
mod runtime;

pub use plugin::{BoxError, Plugin, PluginContext};
pub use port::PortId;
pub use runtime::{Runtime, StartError};
