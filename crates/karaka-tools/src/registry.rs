use std::error::Error;
use std::fmt;
use std::future::ready;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use karaka_agent::Agent;
use karaka_model::{ToolCall, ToolSchema};
use serde_json::{Map, Value};

use crate::json_schema::{JsonSchemaError, check_schema, validate};
use crate::{Tool, ToolFuture, ToolOutput, ToolRun};

/// Every tool in a runtime, by name. (dsh: the `tools` service,
/// `ToolRuntime` in `tools/src/index.ts`, with `register`, `schemas` and
/// `execute`.)
///
/// Like the model registry, it is a port of its own, provided empty by
/// [`ToolsPlugin`](crate::ToolsPlugin). Plugins register their tools in it
/// during setup, and the loop lists their schemas for each request and runs
/// the calls the model makes.
///
/// # What the model gets back
///
/// A call never crashes the loop. A call to a name nobody registered, with
/// arguments that aren't a JSON object, or with arguments that don't fit
/// the tool's parameters, gets an error result instead, worded as dsh words
/// it, and the model reads that and can try again.
///
/// # Locking
///
/// One `RwLock` guards the list, held only to read or add an entry. A call
/// clones the tool's `Arc` out and lets go of the lock before the tool runs,
/// so a slow tool never holds up a lookup or a registration.
#[derive(Default)]
pub struct ToolRegistry {
    /// In the order they were registered, which is the order the model sees
    /// them in, as in dsh. A runtime has a handful of tools, so walking the
    /// list is as fast as a map.
    tools: RwLock<Vec<Registered>>,
}

struct Registered {
    /// Read once, when the tool was registered.
    schema: ToolSchema,
    /// The parameters, parsed, that the arguments are checked against;
    /// `None` for a tool that checks its own.
    parameters: Option<Arc<Value>>,
    tool: Arc<dyn Tool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `tool` under the name in its schema. Fails if another tool has
    /// that name already: a tool is never replaced, as in dsh. Fails too
    /// when the registry is to check the tool's arguments and its
    /// parameters aren't JSON in dsh's enforced subset, as dsh's
    /// `defineTool` refuses them.
    pub fn register(&self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let schema = tool.schema();
        let parameters = if tool.registry_checks_arguments() {
            let parameters: Value = serde_json::from_str(&schema.parameters).map_err(|error| {
                ToolRegistryError::ParametersNotJson {
                    name: schema.name.clone(),
                    reason: error.to_string(),
                }
            })?;
            check_schema(&parameters).map_err(|error| ToolRegistryError::UnsupportedSchema {
                name: schema.name.clone(),
                error,
            })?;
            Some(Arc::new(parameters))
        } else {
            None
        };
        // Checking and adding happen under one write lock, so two threads
        // registering the same name can't both succeed.
        let mut tools = self.tools.write().unwrap_or_else(PoisonError::into_inner);
        if tools.iter().any(|entry| entry.schema.name == schema.name) {
            return Err(ToolRegistryError::DuplicateName(schema.name));
        }
        tools.push(Registered {
            schema,
            parameters,
            tool,
        });
        Ok(())
    }

    /// Every tool's schema, in the order they were registered.
    /// (dsh: `schemas()`.)
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.read()
            .iter()
            .map(|entry| entry.schema.clone())
            .collect()
    }

    /// Runs the call `call` with the tool it names, and resolves to what the
    /// model should read. (dsh: `execute`.)
    ///
    /// The arguments are parsed first, as dsh's loop parses them
    /// (`parseArguments`, `agent-loop/src/tool-calls.ts:105-111`): empty
    /// text means no arguments, `{}`. A name nobody registered, arguments
    /// that aren't a JSON object, or arguments that don't fit the tool's
    /// parameters, resolve at once to an error result, and no tool runs:
    /// `Error: invalid arguments: ` and every violation, joined by `; `, as
    /// dsh's `ToolArgsError` shows. An unknown name is checked first, as in
    /// dsh, where the registry rejects it before the tool's own argument
    /// check could run.
    ///
    /// `agent` is the agent whose turn made the call, which the tool gets
    /// in its [`ToolRun`]; `None` for a call made outside any agent. (dsh:
    /// the input's `agent`.)
    ///
    /// The future owns everything it needs, so it outlives this borrow of
    /// the registry. Dropping it cancels the call.
    pub fn execute(&self, call: &ToolCall, agent: Option<Arc<dyn Agent>>) -> ToolFuture<'static> {
        let Some((tool, parameters)) = self.find(&call.name) else {
            return Box::pin(ready(unknown_tool(&call.name)));
        };
        let Some(arguments) = parse_arguments(&call.arguments) else {
            return Box::pin(ready(invalid_arguments()));
        };
        if let Some(parameters) = parameters {
            let violations = validate(&parameters, &arguments, "");
            if !violations.is_empty() {
                return Box::pin(ready(ToolOutput::error(format!(
                    "Error: invalid arguments: {}",
                    violations.join("; ")
                ))));
            }
        }
        let run = ToolRun {
            call_id: call.id.clone(),
            name: call.name.clone(),
            agent,
        };
        // The block owns `tool`, so the future `execute` returns can borrow
        // it for as long as the call runs.
        Box::pin(async move { tool.execute(arguments, run).await })
    }

    /// The tool named `name`, and the parameters its arguments are checked
    /// against.
    fn find(&self, name: &str) -> Option<Found> {
        self.read()
            .iter()
            .find(|entry| entry.schema.name == name)
            .map(|entry| (Arc::clone(&entry.tool), entry.parameters.clone()))
    }

    // A lock is "poisoned" when a thread panicked while holding it. The only
    // change to the list is one push, which either happened or didn't, so
    // the list is still whole, and it's safe to carry on with it.
    fn read(&self) -> RwLockReadGuard<'_, Vec<Registered>> {
        self.tools.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A tool found by name, with its checked parameters, if any.
type Found = (Arc<dyn Tool>, Option<Arc<Value>>);

/// Lists the tools' names. A `dyn Tool` has no `Debug` of its own to show.
impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<String> = self
            .read()
            .iter()
            .map(|entry| entry.schema.name.clone())
            .collect();
        f.debug_struct("ToolRegistry")
            .field("tools", &names)
            .finish()
    }
}

/// The model's arguments as a JSON object, or `None` if they aren't one.
///
/// dsh hands text that isn't JSON to the tool as a string, and every typed
/// tool then rejects it, because its arguments must be an object
/// (`tools/src/schema.ts`, `validateArgs`). The registry makes that check
/// for every tool, a raw one too, which dsh would hand the string to.
fn parse_arguments(raw: &str) -> Option<Value> {
    if raw.is_empty() {
        return Some(Value::Object(Map::new()));
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_object)
}

/// dsh's result for a name nobody registered: a `ToolNotFoundError` caught
/// and shown as `Error: <message>` (`tools/src/index.ts`, `toolErrorResult`).
fn unknown_tool(name: &str) -> ToolOutput {
    ToolOutput::error(format!("Error: unknown tool \"{name}\""))
}

/// dsh's result for arguments that aren't an object: the `ToolArgsError` a
/// typed tool throws, whose violation names the root `arguments`
/// (`tools/src/json-schema.ts`, `diagnosticPath`).
fn invalid_arguments() -> ToolOutput {
    ToolOutput::error("Error: invalid arguments: \"arguments\" must be an object")
}

/// Why [`ToolRegistry::register`] refused a tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRegistryError {
    /// Another tool has this name already.
    DuplicateName(String),
    /// The tool's parameters aren't JSON.
    ParametersNotJson { name: String, reason: String },
    /// The tool's parameters use JSON Schema outside the subset the
    /// registry checks arguments against.
    UnsupportedSchema {
        name: String,
        error: JsonSchemaError,
    },
}

impl fmt::Display for ToolRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // dsh's wording, without its advice about per-agent tools,
            // which Karaka doesn't have.
            Self::DuplicateName(name) => write!(f, "tool \"{name}\" is already registered"),
            Self::ParametersNotJson { name, reason } => {
                write!(f, "tool \"{name}\" parameters are not JSON: {reason}")
            }
            Self::UnsupportedSchema { name, error } => write!(f, "tool \"{name}\": {error}"),
        }
    }
}

impl Error for ToolRegistryError {}

#[cfg(test)]
mod tests {
    use super::parse_arguments;

    #[test]
    fn empty_arguments_are_an_empty_object() {
        assert_eq!(parse_arguments(""), Some(serde_json::json!({})));
    }

    #[test]
    fn only_a_json_object_is_accepted() {
        assert_eq!(
            parse_arguments(r#"{"city": "Oslo"}"#),
            Some(serde_json::json!({ "city": "Oslo" }))
        );
        for raw in [
            "not json",
            r#"{"city": "Os"#,
            "[1, 2]",
            "\"Oslo\"",
            "null",
            " ",
        ] {
            assert_eq!(parse_arguments(raw), None, "{raw:?}");
        }
    }
}
