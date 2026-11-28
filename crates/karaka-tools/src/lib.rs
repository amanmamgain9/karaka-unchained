//! The tools seam: the [`Tool`] trait every tool implements, and the
//! [`ToolRegistry`] port the loop finds and runs tools through.
//! (dsh: `packages/core/tools`, `dsh-tools`.)
//!
//! The registry checks each call's arguments against its tool's parameters
//! before the tool runs, with dsh's validator (`json_schema.rs`), after the
//! gate, which lets policy listeners allow, deny, cancel, or ask for
//! approval first (`gate.rs`).
//!
//! It is kept much smaller than dsh's. A tool here is a schema and an async
//! function from parsed arguments to model-facing text. dsh's tools also
//! declare an output schema, presentation views for a UI, timeouts,
//! concurrency and approval policy; none of those exist yet.

mod gate;
pub mod json_schema;
mod plugin;
mod registry;
mod tool;

pub use gate::{
    DisplayReason, PreExecuteError, PreExecuteFuture, PreExecuteListener, PreExecuteNext,
    PreExecuteRegistration, PreToolDecision, ToolApproval, ToolApprovalFuture, ToolApprovalOutcome,
    ToolApprovalRequest, ToolExecution,
};
pub use json_schema::JsonSchemaError;
pub use plugin::ToolsPlugin;
pub use registry::{RestrictionRegistration, ToolRegistry, ToolRegistryError, ToolRestriction};
pub use tool::{Tool, ToolFuture, ToolOutput, ToolRun};
