//! Karaka's kernel: ports, plugins, and the runtime that starts them.
//!
//! Every other Karaka crate builds on this one. Backends, model providers and
//! the main loop are all plugins that provide or use ports.

mod port;

pub use port::PortId;
