//! Each session's working directory, durable and shown to the model.
//! (dsh: `@deepseek-ai/dsh-working-directory`, 0.2.1-rc.2.)
//!
//! [`WorkingDirectory`] owns each session's directory. It is the session's
//! project, from its header, until a [`set`](WorkingDirectory::set) logs a
//! `working-directory/change`; the state is the `workingDirectory`
//! projection of the log, so it survives a restore and clients see it.
//! [`ensure`](WorkingDirectory::ensure) checks the directory still exists,
//! and returns to the project when it doesn't. Each change queues a notice
//! for the agent's next step with `inject`.
//!
//! [`WorkingDirectoryPlugin`] provides the service, and shows the model
//! `Current working directory: "…".` as required runtime context, checked
//! before every assembly. Directories are resolved and checked through the
//! mounted `FileSystem`.
//!
//! - `service.rs`: the service and its projection.
//! - `plugin.rs`: the plugin, the context and the assembly listener.
//! - `error.rs`: the errors, in dsh's words.

mod error;
mod plugin;
mod service;

pub use error::WorkingDirectoryError;
pub use plugin::WorkingDirectoryPlugin;
pub use service::{
    CHANGE_EVENT, CONTEXT_NAME, WorkingDirectory, WorkingDirectoryConfig,
    WorkingDirectoryProjection,
};
