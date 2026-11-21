//! The tools seam: the [`Tool`] trait every tool implements, and the
//! [`ToolRegistry`] port the loop finds and runs tools through.
//! (dsh: `packages/core/tools`, `dsh-tools`.)
//!
//! It is kept much smaller than dsh's. A tool here is a schema and an async
//! function from parsed arguments to model-facing text. dsh's tools also
//! declare an output schema, presentation views for a UI, timeouts,
//! concurrency and approval policy; none of those exist yet.

mod plugin;
mod registry;
mod tool;

pub use plugin::ToolsPlugin;
pub use registry::{ToolRegistry, ToolRegistryError};
pub use tool::{Tool, ToolFuture, ToolOutput, ToolRun};
