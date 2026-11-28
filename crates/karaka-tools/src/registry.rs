use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, Weak};

use karaka_agent::Agent;
use karaka_model::{ToolCall, ToolSchema};
use karaka_scope::{ScopeChain, ScopeKey, ScopedLayers};
use serde_json::{Map, Value};

use crate::gate::{
    Gate, Passage, PreExecuteListener, PreExecuteRegistration, ToolApproval, ToolExecution, pass,
};
use crate::json_schema::{JsonSchemaError, check_schema, validate};
use crate::{Tool, ToolFuture, ToolOutput, ToolRun};

/// Every tool in a runtime, by name and scope. (dsh: the `tools` service,
/// `ToolRuntime` in `tools/src/index.ts`, with `register`, `restrict`,
/// `get`, `schemas` and `execute`.)
///
/// Like the model registry, it is a port of its own, provided empty by
/// [`ToolsPlugin`](crate::ToolsPlugin). Plugins register their tools in it
/// during setup, and the loop lists the schemas each agent sees for each
/// request and runs the calls the model makes.
///
/// # Scopes
///
/// A tool is global, or filed under a scope (`karaka-scope`): an agent
/// preset's or one session's. Each scope is a layer, and an agent sees the
/// tools through its chain, global, then preset, then session (dsh:
/// `view`):
///
/// 1. What it inherits: the global tools, then each farther scope's on its
///    chain, a nearer scope's tool shadowing a farther one of its name.
///    Its own scope, the nearest on the chain, isn't inherited.
/// 2. The restrictions of every scope on the chain mask inherited names,
///    each one intersecting the others ([`restrict`](Self::restrict)).
/// 3. Its own scope's tools, last, shadowing an inherited name and outside
///    every restriction, so a scope can't filter out what it registered
///    itself.
///
/// A call resolves its tool through its agent's view, so a tool an agent
/// doesn't see is an unknown tool to it.
///
/// # What the model gets back
///
/// A call never crashes the loop. A call the gate stops (`gate.rs`), or to
/// a name the caller doesn't see, with arguments that aren't a JSON object,
/// or with arguments that don't fit the tool's parameters, gets an error
/// result instead, worded as dsh words it, and the model reads that and can
/// try again.
///
/// # Locking
///
/// One `RwLock` guards the layers, held only to read a view or change a
/// layer. A call clones the tool's `Arc` out and lets go of the lock before
/// the tool runs, so a slow tool never holds up a lookup or a registration.
#[derive(Default)]
pub struct ToolRegistry {
    /// Each layer's tools in the order they were registered, which is the
    /// order the model sees them in, as in dsh, and its restrictions.
    layers: Arc<RwLock<Layers>>,
    /// The `tools/pre-execute` listeners and the approval seam.
    gate: Gate,
}

#[derive(Default)]
struct Layers {
    next: u64,
    layers: ScopedLayers<ToolLayer>,
}

/// One scope's tools and restrictions, or the global tools. (dsh:
/// `ToolLayer`.)
#[derive(Default)]
struct ToolLayer {
    /// Names are unique within a layer.
    tools: Vec<Arc<Registered>>,
    restrictions: Vec<(u64, Compiled)>,
}

impl ToolLayer {
    fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.restrictions.is_empty()
    }

    /// Whether every restriction of this layer lets an inherited `name`
    /// through. (dsh: `admits`.)
    fn admits(&self, name: &str) -> bool {
        self.restrictions.iter().all(|(_, filter)| {
            filter
                .allow
                .as_ref()
                .is_none_or(|allow| allow.contains(name))
                && filter.deny.as_ref().is_none_or(|deny| !deny.contains(name))
        })
    }
}

struct Registered {
    /// Read once, when the tool was registered.
    schema: ToolSchema,
    /// The parameters, parsed, that the arguments are checked against;
    /// `None` for a tool that checks its own.
    parameters: Option<Arc<Value>>,
    tool: Arc<dyn Tool>,
}

/// A mask over the tools a scope inherits. (dsh: `ToolRestriction`.)
///
/// Restrictions intersect: a name stays only if every restriction on the
/// viewer's chain lets it through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolRestriction {
    /// The inherited names that stay visible; every other one is masked.
    pub allow: Option<Vec<String>>,
    /// The inherited names masked.
    pub deny: Option<Vec<String>>,
}

/// A restriction, its names read once when it was made. (dsh:
/// `CompiledToolRestriction`.)
struct Compiled {
    allow: Option<HashSet<String>>,
    deny: Option<HashSet<String>>,
}

/// A restriction in the registry, which [`remove`](Self::remove) lifts.
/// Dropping it leaves the restriction in place, as a dsh restriction stays
/// until its disposer runs.
#[must_use = "dropping a registration keeps the restriction; call `remove` to lift it"]
pub struct RestrictionRegistration {
    layers: Weak<RwLock<Layers>>,
    scope: ScopeKey,
    id: u64,
}

impl RestrictionRegistration {
    /// Lifts the restriction; the scope's others stay. (dsh: the disposer
    /// `restrict` returns.)
    pub fn remove(self) {
        let Some(layers) = self.layers.upgrade() else {
            return;
        };
        let mut layers = layers.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(layer) = layers.layers.get_mut(Some(&self.scope)) {
            layer.restrictions.retain(|(id, _)| *id != self.id);
        }
        layers.layers.prune(Some(&self.scope), ToolLayer::is_empty);
    }
}

impl fmt::Debug for RestrictionRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestrictionRegistration")
            .field("scope", &self.scope)
            .field("id", &self.id)
            .finish()
    }
}

/// One viewer's tools, worked out in one pass over its chain. (dsh:
/// `ToolView`.)
struct View {
    /// What the viewer sees and may call, in order.
    visible: Vec<Arc<Registered>>,
    /// Every name the viewer knows, restricted or not: what a configured
    /// tool order may name.
    known: Vec<String>,
    /// The names the viewer inherits: what a restriction may name.
    inherited: Vec<String>,
}

/// Puts `entry` in place of the entry of its name, or last.
fn shadow(list: &mut Vec<Arc<Registered>>, entry: &Arc<Registered>) {
    match list
        .iter()
        .position(|known| known.schema.name == entry.schema.name)
    {
        Some(at) => list[at] = Arc::clone(entry),
        None => list.push(Arc::clone(entry)),
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `tool` globally, under the name in its schema. See
    /// [`register_in`](Self::register_in).
    pub fn register(&self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        self.register_in(None, tool)
    }

    /// Adds `tool` under the name in its schema: globally when `scope` is
    /// `None`, or in `scope`'s layer, where it shadows a tool of its name
    /// that the scope would inherit. (dsh: `register`, filed by the calling
    /// context's scope.)
    ///
    /// Fails if that layer has a tool of that name already: a tool is never
    /// replaced, as in dsh. Fails too when the registry is to check the
    /// tool's arguments and its parameters aren't JSON in dsh's enforced
    /// subset, as dsh's `defineTool` refuses them.
    pub fn register_in(
        &self,
        scope: Option<&ScopeKey>,
        tool: Arc<dyn Tool>,
    ) -> Result<(), ToolRegistryError> {
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
        let mut layers = self.write();
        let layer = layers.layers.layer_mut(scope);
        if layer
            .tools
            .iter()
            .any(|entry| entry.schema.name == schema.name)
        {
            let name = schema.name;
            return Err(match scope {
                None => ToolRegistryError::DuplicateName(name),
                Some(_) => ToolRegistryError::DuplicateInScope(name),
            });
        }
        layer.tools.push(Arc::new(Registered {
            schema,
            parameters,
            tool,
        }));
        Ok(())
    }

    /// Masks tools that the nearest scope of `viewer` inherits, for that
    /// scope and every scope nested in it, until the registration is
    /// removed. The scope's own tools stay outside it. (dsh: `restrict`.)
    ///
    /// It takes the viewer's whole chain, where dsh takes the scope alone,
    /// because a Karaka scope doesn't know its parent: what a session
    /// inherits depends on its preset, which only its chain names.
    ///
    /// # Errors
    ///
    /// In dsh's words: a chain with no scope, since a global restriction
    /// would mask every agent; a restriction with neither list; or a name
    /// the scope doesn't inherit, its own tools' included.
    pub fn restrict(
        &self,
        viewer: &ScopeChain,
        restriction: ToolRestriction,
    ) -> Result<RestrictionRegistration, ToolRegistryError> {
        let Some(scope) = viewer.nearest() else {
            return Err(ToolRegistryError::RestrictUnscoped);
        };
        let ToolRestriction { allow, deny } = restriction;
        if allow.is_none() && deny.is_none() {
            return Err(ToolRegistryError::RestrictEmpty);
        }
        let mut layers = self.write();
        let inherited = view(&layers, viewer).inherited;
        let unknown: Vec<String> = allow
            .iter()
            .chain(deny.iter())
            .flatten()
            .filter(|name| !inherited.contains(name))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            let mut known = inherited;
            known.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            return Err(ToolRegistryError::RestrictUnknown { unknown, known });
        }
        layers.next += 1;
        let id = layers.next;
        let compiled = Compiled {
            allow: allow.map(|names| names.into_iter().collect()),
            deny: deny.map(|names| names.into_iter().collect()),
        };
        layers
            .layers
            .layer_mut(Some(&scope))
            .restrictions
            .push((id, compiled));
        Ok(RestrictionRegistration {
            layers: Arc::downgrade(&self.layers),
            scope,
            id,
        })
    }

    /// Adds `listener` at the end of the `tools/pre-execute` chain: for
    /// every call when `scope` is `None`, or for the calls of the agents
    /// whose chain holds `scope`. (dsh: a `tools/pre-execute` listener.)
    pub fn on_pre_execute(
        &self,
        scope: Option<&ScopeKey>,
        listener: Arc<dyn PreExecuteListener>,
    ) -> PreExecuteRegistration {
        self.gate.listen(scope, listener)
    }

    /// Makes `approval` the seam an `ask` decision goes through, in place of
    /// any before. Without one, an `ask` is denied. (dsh: the `approval`
    /// service, which the registry takes when it is there.)
    pub fn set_approval(&self, approval: Arc<dyn ToolApproval>) {
        *self
            .gate
            .approval
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(approval);
    }

    /// The global tools' schemas, in the order they were registered: what a
    /// viewer with no scope sees. (dsh: `schemas()`.)
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.schemas_for(&ScopeChain::global())
    }

    /// The schemas of the tools `viewer` sees. (dsh: `schemas(scope)`.)
    pub fn schemas_for(&self, viewer: &ScopeChain) -> Vec<ToolSchema> {
        view(&self.read(), viewer)
            .visible
            .iter()
            .map(|entry| entry.schema.clone())
            .collect()
    }

    /// The schemas `viewer` sees, and every name it knows, restricted ones
    /// included, for the system prompt's tool order. (dsh: `wireSchemas`.)
    pub(crate) fn wire(&self, viewer: &ScopeChain) -> (Vec<ToolSchema>, Vec<String>) {
        let view = view(&self.read(), viewer);
        let schemas = view
            .visible
            .iter()
            .map(|entry| entry.schema.clone())
            .collect();
        (schemas, view.known)
    }

    /// The tool named `name` as `viewer` sees it: a nearer scope's shadowing
    /// a farther one's, and a masked one absent. (dsh: `get(name, scope)`.)
    pub fn get(&self, name: &str, viewer: &ScopeChain) -> Option<Arc<dyn Tool>> {
        self.find(name, viewer).map(|(tool, _)| tool)
    }

    /// Runs the call `call` with the tool it names, and resolves to what the
    /// model should read. (dsh: `execute`.)
    ///
    /// In dsh's order (`prepareExecution`, then the dispatch):
    ///
    /// 1. The arguments are parsed, as dsh's loop parses them
    ///    (`parseArguments`, `agent-loop/src/tool-calls.ts:105-111`): empty
    ///    text means no arguments, `{}`, and text that isn't JSON stays a
    ///    string.
    /// 2. The gate: the `tools/pre-execute` chain the caller's scopes
    ///    admit, and an `ask` resolved through the approval seam
    ///    (`gate.rs`). It sees every call, an unknown name's and bad
    ///    arguments' too.
    /// 3. A name the caller doesn't see is an error result.
    /// 4. Arguments that aren't a JSON object, or that don't fit the tool's
    ///    parameters, are an error result: `Error: invalid arguments: ` and
    ///    every violation, joined by `; `, as dsh's `ToolArgsError` shows.
    /// 5. The tool runs.
    ///
    /// The tool is looked up when the call starts, through the caller's
    /// view. dsh looks it up after the gate, so a tool registered while the
    /// gate waits would run there.
    ///
    /// `agent` is the agent whose turn made the call, which the tool gets
    /// in its [`ToolRun`], and whose chain the call sees tools and listeners
    /// through; `None` for a call made outside any agent, which sees the
    /// global ones alone. (dsh: the input's `agent`.)
    ///
    /// The future owns everything it needs, so it outlives this borrow of
    /// the registry. Dropping it cancels the call.
    pub fn execute(&self, call: &ToolCall, agent: Option<Arc<dyn Agent>>) -> ToolFuture<'static> {
        let viewer = agent
            .as_ref()
            .map_or_else(ScopeChain::global, |agent| agent.scope());
        let found = self.find(&call.name, &viewer);
        let execution = ToolExecution {
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: parse_arguments(&call.arguments),
            agent,
        };
        let chain = self.gate.chain(&viewer);
        let approval = self.gate.approval();
        // The block owns everything, so the future `execute` returns can
        // borrow the tool for as long as the call runs.
        Box::pin(async move {
            // An empty chain allows, so the gate is skipped for it.
            if !chain.is_empty()
                && let Passage::Stop(text) = pass(chain, approval, &execution).await
            {
                return ToolOutput::error(text);
            }
            let Some((tool, parameters)) = found else {
                return unknown_tool(&execution.name);
            };
            let ToolExecution {
                call_id,
                name,
                arguments,
                agent,
            } = execution;
            if !arguments.is_object() {
                return invalid_arguments();
            }
            if let Some(parameters) = parameters {
                let violations = validate(&parameters, &arguments, "");
                if !violations.is_empty() {
                    return ToolOutput::error(format!(
                        "Error: invalid arguments: {}",
                        violations.join("; ")
                    ));
                }
            }
            let run = ToolRun {
                call_id,
                name,
                agent,
            };
            tool.execute(arguments, run).await
        })
    }

    /// The tool `viewer` sees as `name`, and the parameters its arguments
    /// are checked against.
    fn find(&self, name: &str, viewer: &ScopeChain) -> Option<Found> {
        view(&self.read(), viewer)
            .visible
            .iter()
            .find(|entry| entry.schema.name == name)
            .map(|entry| (Arc::clone(&entry.tool), entry.parameters.clone()))
    }

    // A lock is "poisoned" when a thread panicked while holding it. Every
    // change is one push or removal, which either happened or didn't, so
    // the layers are still whole, and it's safe to carry on with them.
    fn read(&self) -> RwLockReadGuard<'_, Layers> {
        self.layers.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Layers> {
        self.layers.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `viewer`'s tools, in one pass over its chain. (dsh: `view`.)
fn view(layers: &Layers, viewer: &ScopeChain) -> View {
    let chain = layers.layers.chain_layers(viewer);
    // The viewer's own scope, whose tools it owns rather than inherits.
    let own = viewer.nearest();
    let mut inherited: Vec<Arc<Registered>> = layers.layers.global().tools.clone();
    for (key, layer) in &chain {
        if Some(key) == own.as_ref() {
            continue;
        }
        for entry in &layer.tools {
            shadow(&mut inherited, entry);
        }
    }
    let inherited_names: Vec<String> = inherited
        .iter()
        .map(|entry| entry.schema.name.clone())
        .collect();
    let mut known = inherited_names.clone();
    // Restrictions intersect across the whole chain: any scope on it may
    // mask an inherited name for every scope nested in it.
    let mut visible: Vec<Arc<Registered>> = inherited
        .into_iter()
        .filter(|entry| {
            chain
                .iter()
                .all(|(_, layer)| layer.admits(&entry.schema.name))
        })
        .collect();
    if let Some(own) = own
        && let Some(layer) = layers.layers.peek(&own)
    {
        for entry in &layer.tools {
            if !known.contains(&entry.schema.name) {
                known.push(entry.schema.name.clone());
            }
            shadow(&mut visible, entry);
        }
    }
    View {
        visible,
        known,
        inherited: inherited_names,
    }
}

/// A tool found by name, with its checked parameters, if any.
type Found = (Arc<dyn Tool>, Option<Arc<Value>>);

/// Lists the global tools' names, and how many scopes have layers. A `dyn
/// Tool` has no `Debug` of its own to show.
impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let layers = self.read();
        let names: Vec<String> = layers
            .layers
            .global()
            .tools
            .iter()
            .map(|entry| entry.schema.name.clone())
            .collect();
        f.debug_struct("ToolRegistry")
            .field("tools", &names)
            .field("scopes", &layers.layers.scoped_len())
            .finish()
    }
}

/// The model's arguments, as dsh's loop parses them: empty text is `{}`,
/// and text that isn't JSON is the string itself.
///
/// dsh hands such a string, or any JSON that isn't an object, to the tool,
/// and every typed tool then rejects it, because its arguments must be an
/// object (`tools/src/schema.ts`, `validateArgs`). The registry makes that
/// check after the gate for every tool, a raw one too, which dsh would hand
/// the value to.
fn parse_arguments(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::Object(Map::new());
    }
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
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

/// Why [`ToolRegistry::register`] refused a tool, or
/// [`ToolRegistry::restrict`] a restriction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRegistryError {
    /// Another global tool has this name already.
    DuplicateName(String),
    /// Another tool of this scope has this name already.
    DuplicateInScope(String),
    /// A restriction for a chain with no scope.
    RestrictUnscoped,
    /// A restriction with neither an allow nor a deny list.
    RestrictEmpty,
    /// A restriction naming tools the scope doesn't inherit, and the names
    /// it does, sorted.
    RestrictUnknown {
        unknown: Vec<String>,
        known: Vec<String>,
    },
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
            // dsh's wording, with its advice in Karaka's terms: dsh's says
            // to register through that agent's `agent.ctx`.
            Self::DuplicateName(name) => write!(
                f,
                "tool \"{name}\" is already registered (for a per-agent variant, register it in \
                 that agent's scope instead)"
            ),
            Self::DuplicateInScope(name) => {
                write!(f, "tool \"{name}\" is already registered in this scope")
            }
            Self::RestrictUnscoped => f.write_str(
                "tools.restrict() requires a scoped context (agent.ctx): a context-global \
                 restriction would mask every agent — deny the tool for the intended agent instead",
            ),
            Self::RestrictEmpty => f.write_str(
                "tools.restrict({}) is a no-op: pass `allow` and/or `deny` (an empty filter is \
                 almost always a materialized-empty-config bug)",
            ),
            Self::RestrictUnknown { unknown, known } => {
                let names: Vec<String> = unknown.iter().map(|name| format!("\"{name}\"")).collect();
                let known = if known.is_empty() {
                    "(none)".to_owned()
                } else {
                    known.join(", ")
                };
                write!(
                    f,
                    "tools.restrict() names unknown global tool{} {}; known global tools: {known}",
                    if unknown.len() > 1 { "s" } else { "" },
                    names.join(", ")
                )
            }
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

    use serde_json::json;

    #[test]
    fn empty_arguments_are_an_empty_object() {
        assert_eq!(parse_arguments(""), json!({}));
    }

    #[test]
    fn json_is_parsed_and_anything_else_stays_text() {
        assert_eq!(
            parse_arguments(r#"{"city": "Oslo"}"#),
            json!({ "city": "Oslo" })
        );
        assert_eq!(parse_arguments("[1, 2]"), json!([1, 2]));
        assert_eq!(parse_arguments("null"), json!(null));
        for raw in ["not json", r#"{"city": "Os"#, " "] {
            assert_eq!(parse_arguments(raw), json!(raw), "{raw:?}");
        }
    }
}
