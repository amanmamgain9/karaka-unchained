//! An application tool's input check, before its call is authorized or
//! sent. (Karaka: `mcp-application/src/tools.ts`, `prepareApplicationTool`.)

use std::sync::Arc;

use karaka_mcp_client::McpTool;
use karaka_model::ToolSchema;
use karaka_tools::json_schema::check_object_schema;
use karaka_tools::{Tool, ToolFuture, ToolRun};
use serde_json::Value;

/// An MCP tool whose arguments the registry checks, against its schema
/// without the `$schema` dialect.
pub struct ApplicationTool {
    tool: McpTool,
    /// A tool that needs tasks is refused before its arguments are read,
    /// as the upstream bridge refuses it.
    checked: bool,
}

/// The tool to publish for `tool`. (Karaka: `prepareApplicationTool`.)
///
/// # Errors
///
/// The input schema isn't an object schema in dsh's enforced subset, in
/// dsh's words (`assertObjectJsonSchema`).
pub fn prepare_application_tool(tool: McpTool) -> Result<Arc<dyn Tool>, String> {
    let mut parameters = tool.input_schema().clone();
    if let Value::Object(map) = &mut parameters {
        map.shift_remove("$schema");
    }
    check_object_schema(&parameters).map_err(|error| error.to_string())?;
    let checked = !tool.task_required();
    Ok(Arc::new(ApplicationTool {
        tool: tool.with_input_schema(parameters),
        checked,
    }))
}

impl Tool for ApplicationTool {
    fn schema(&self) -> ToolSchema {
        self.tool.schema()
    }

    fn registry_checks_arguments(&self) -> bool {
        self.checked
    }

    fn execute<'a>(&'a self, arguments: Value, run: ToolRun) -> ToolFuture<'a> {
        self.tool.execute(arguments, run)
    }
}
