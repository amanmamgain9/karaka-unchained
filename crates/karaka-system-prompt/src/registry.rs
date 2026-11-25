//! [`SystemPrompt`], the registry every prompt contribution lands in, and
//! its assembly. (dsh: `SystemPrompt` in `core/system-prompt/src/index.ts`.)

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_model::ToolSchema;

use crate::assembly::{
    AssembledContext, AssembledSection, PromptAssembly, PromptError, Variables, valid_variable_name,
};

/// The fixed opener, at [`SectionOrder::HarnessIdentity`]. dsh's names DeepSeek
/// Harness; Karaka is provider-neutral, so it names Karaka.
pub const HARNESS_IDENTITY: &str = "You are an AI agent powered by Karaka.";

/// The deployment persona prefix's section. A scope that registers a
/// section of this name replaces the deployment's persona for that agent.
/// (dsh: `PERSONA_PREFIX_SECTION`.)
pub const PERSONA_PREFIX_SECTION: &str = "deployment:persona-prefix";

/// The deployment persona suffix's section. (dsh: `PERSONA_SUFFIX_SECTION`.)
pub const PERSONA_SUFFIX_SECTION: &str = "deployment:persona-suffix";

/// The harness identity's section.
pub const HARNESS_IDENTITY_SECTION: &str = "harness:identity";

/// The rest entry of [`SystemPromptConfig::tool_order`]: where every tool
/// it doesn't list goes, by name. (dsh: `TOOL_ORDER_REST`.)
pub const TOOL_ORDER_REST: &str = "<unlisted-tools>";

/// Where a section of a contributor that exists in Karaka goes. dsh
/// allocates more, for contributors not ported. (dsh: `SECTION_ORDERS` and
/// `getSectionOrder`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionOrder {
    HarnessIdentity,
    DeploymentPersonaPrefix,
    DeploymentPersonaSuffix,
}

impl SectionOrder {
    /// Its place among sections.
    pub fn order(self) -> f64 {
        match self {
            Self::HarnessIdentity => -1000.0,
            Self::DeploymentPersonaPrefix => 0.0,
            Self::DeploymentPersonaSuffix => 10200.0,
        }
    }
}

/// Where a runtime context of a contributor coming to Karaka goes. dsh
/// allocates more, for contributors not ported. (dsh: `CONTEXT_ORDERS` and
/// `getContextOrder`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextOrder {
    /// The working-directory service's context.
    WorkingDirectory,
}

impl ContextOrder {
    /// Its place among runtime contexts.
    pub fn order(self) -> f64 {
        match self {
            Self::WorkingDirectory => 100.0,
        }
    }
}

/// Whose contributions an assembly takes, besides the global ones: one
/// agent's, by the id of its session. (dsh: `ScopeKey`.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scope(String);

impl Scope {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What one assembly is for. (dsh: `AssembleContext`, without its signal
/// and its merge-extensible fields.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssembleContext {
    /// The agent whose contributions join the global ones; `None` for the
    /// global ones alone.
    pub scope: Option<Scope>,
}

impl AssembleContext {
    /// An assembly for the agent `scope`.
    pub fn scoped(scope: Scope) -> Self {
        Self { scope: Some(scope) }
    }
}

/// A section's or a context's text: as it is, or worked out at each
/// assembly.
#[derive(Clone)]
pub enum Text {
    Static(String),
    Dynamic(Arc<dyn Fn(&AssembleContext) -> String + Send + Sync>),
}

impl Text {
    /// Text worked out at each assembly by `provider`.
    pub fn dynamic(provider: impl Fn(&AssembleContext) -> String + Send + Sync + 'static) -> Self {
        Self::Dynamic(Arc::new(provider))
    }

    fn resolve(&self, context: &AssembleContext) -> String {
        match self {
            Self::Static(text) => text.clone(),
            Self::Dynamic(provider) => provider(context),
        }
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self::Static(text.to_owned())
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Self::Static(text)
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(text) => f.debug_tuple("Static").field(text).finish(),
            Self::Dynamic(_) => f.write_str("Dynamic(..)"),
        }
    }
}

/// One contributed section of the prompt. (dsh: `PromptSection`.)
#[derive(Clone, Debug)]
pub struct Section {
    /// Unique in its layer.
    pub name: String,
    /// Sections join in ascending order; equal orders by name, in UTF-16
    /// code-unit order.
    pub order: f64,
    pub text: Text,
    /// `false` keeps the text literal. Defaults to `true`.
    pub interpolate: bool,
    /// The whole prompt: the waterfall still runs, then this section alone
    /// is the prompt. Two at once fail the assembly.
    pub complete: bool,
}

impl Section {
    pub fn new(name: impl Into<String>, order: f64, text: impl Into<Text>) -> Self {
        Self {
            name: name.into(),
            order,
            text: text.into(),
            interpolate: true,
            complete: false,
        }
    }

    /// The same, kept literal.
    pub fn literal(mut self) -> Self {
        self.interpolate = false;
        self
    }

    /// The same, as the whole prompt.
    pub fn complete(mut self) -> Self {
        self.complete = true;
        self
    }
}

/// Runtime context, which the loop turns into a user-role snapshot.
/// (dsh: `PromptContext`.)
#[derive(Clone, Debug)]
pub struct PromptContext {
    pub name: String,
    pub order: f64,
    pub text: Text,
    pub interpolate: bool,
    /// Kept when optional runtime context is suppressed.
    pub required: bool,
}

impl PromptContext {
    pub fn new(name: impl Into<String>, order: f64, text: impl Into<Text>) -> Self {
        Self {
            name: name.into(),
            order,
            text: text.into(),
            interpolate: true,
            required: false,
        }
    }

    pub fn literal(mut self) -> Self {
        self.interpolate = false;
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    fn resolve(&self, context: &AssembleContext) -> AssembledContext {
        AssembledContext {
            name: self.name.clone(),
            text: self.text.resolve(context),
            interpolate: self.interpolate,
        }
    }
}

/// What a tool source gives one assembly. (dsh: `ToolProviderResult`.)
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolProviderResult {
    /// The tools visible in this assembly.
    pub schemas: Vec<ToolSchema>,
    /// Every name the source knows, visible or not, which
    /// [`SystemPromptConfig::tool_order`] is checked against; the visible
    /// names when `None`.
    pub known_names: Option<Vec<String>>,
}

type ToolProvider = Arc<dyn Fn(&AssembleContext) -> ToolProviderResult + Send + Sync>;
type VariableProvider = Arc<dyn Fn(&AssembleContext) -> Option<String> + Send + Sync>;

/// What a listener's future gives.
pub type AssembleFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PromptAssembly, PromptError>> + Send + 'a>>;

/// Shapes assemblies before they are returned. (dsh: a
/// `system-prompt/assemble` waterfall listener.)
///
/// It gets the assembly and a [`AssembleNext`] that runs the listeners
/// after it; what it returns is the assembly. It may return without
/// running them. A complete section is restored after the whole chain.
pub trait AssembleListener: Send + Sync {
    fn assemble<'a>(
        &'a self,
        assembly: PromptAssembly,
        context: &'a AssembleContext,
        next: AssembleNext<'a>,
    ) -> AssembleFuture<'a>;
}

/// The listeners after this one.
pub struct AssembleNext<'a> {
    rest: &'a [Arc<dyn AssembleListener>],
    context: &'a AssembleContext,
}

impl<'a> AssembleNext<'a> {
    /// Runs the rest of the chain on `assembly`. Its end gives the
    /// assembly back.
    pub fn run(self, assembly: PromptAssembly) -> AssembleFuture<'a> {
        match self.rest.split_first() {
            None => Box::pin(ready(Ok(assembly))),
            Some((first, rest)) => first.assemble(
                assembly,
                self.context,
                AssembleNext {
                    rest,
                    context: self.context,
                },
            ),
        }
    }
}

/// The plugin's settings. (dsh: its `Config`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemPromptConfig {
    /// Open with [`HARNESS_IDENTITY`]. Defaults to `true`.
    pub include_harness_identity: bool,
    /// Include optional runtime context. Defaults to `true`; required
    /// context stays either way.
    pub include_runtime_context: bool,
    /// The deployment's persona, before everything else but the identity.
    pub persona_prefix: String,
    /// The deployment's persona suffix, after everything else.
    pub persona_suffix: String,
    /// The tools' order, with [`TOOL_ORDER_REST`] exactly once; by name
    /// when `None`.
    pub tool_order: Option<Vec<String>>,
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            include_harness_identity: true,
            include_runtime_context: true,
            persona_prefix: String::new(),
            persona_suffix: String::new(),
            tool_order: None,
        }
    }
}

/// One registry layer: the global one, or one agent's.
#[derive(Default)]
struct Layer {
    sections: Vec<(u64, Section)>,
    contexts: Vec<(u64, PromptContext)>,
    suppressors: Vec<u64>,
    tools: Vec<(u64, ToolProvider)>,
    variables: Vec<(u64, String, VariableProvider)>,
}

impl Layer {
    fn is_empty(&self) -> bool {
        self.sections.is_empty()
            && self.contexts.is_empty()
            && self.suppressors.is_empty()
            && self.tools.is_empty()
            && self.variables.is_empty()
    }

    fn remove(&mut self, id: u64) {
        self.sections.retain(|(entry, _)| *entry != id);
        self.contexts.retain(|(entry, _)| *entry != id);
        self.suppressors.retain(|entry| *entry != id);
        self.tools.retain(|(entry, _)| *entry != id);
        self.variables.retain(|(entry, _, _)| *entry != id);
    }
}

#[derive(Default)]
struct State {
    next: u64,
    global: Layer,
    scoped: HashMap<Scope, Layer>,
    listeners: Vec<(u64, Option<Scope>, Arc<dyn AssembleListener>)>,
}

impl State {
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn layer(&mut self, scope: Option<&Scope>) -> &mut Layer {
        match scope {
            None => &mut self.global,
            Some(scope) => self.scoped.entry(scope.clone()).or_default(),
        }
    }

    fn scope_layer(&self, scope: Option<&Scope>) -> Option<&Layer> {
        scope.and_then(|scope| self.scoped.get(scope))
    }

    fn suppressed(&self, scope: Option<&Scope>) -> bool {
        !self.global.suppressors.is_empty()
            || self
                .scope_layer(scope)
                .is_some_and(|layer| !layer.suppressors.is_empty())
    }

    /// The effective sections: the global ones, each replaced by the
    /// scope's of the same name, then the scope's own.
    fn sections(&self, scope: Option<&Scope>) -> Vec<Section> {
        merge(
            self.global.sections.iter().map(|(_, section)| section),
            self.scope_layer(scope)
                .into_iter()
                .flat_map(|layer| layer.sections.iter().map(|(_, section)| section)),
            |section| &section.name,
        )
    }

    fn contexts(&self, scope: Option<&Scope>) -> Vec<PromptContext> {
        merge(
            self.global.contexts.iter().map(|(_, context)| context),
            self.scope_layer(scope)
                .into_iter()
                .flat_map(|layer| layer.contexts.iter().map(|(_, context)| context)),
            |context| &context.name,
        )
    }
}

/// `scoped` over `global`, by name: a scoped entry takes its global twin's
/// place, a new one goes last.
fn merge<'a, T: Clone + 'a>(
    global: impl Iterator<Item = &'a T>,
    scoped: impl Iterator<Item = &'a T>,
    name: impl Fn(&T) -> &String,
) -> Vec<T> {
    let mut merged: Vec<T> = global.cloned().collect();
    for entry in scoped {
        match merged.iter().position(|known| name(known) == name(entry)) {
            Some(at) => merged[at] = entry.clone(),
            None => merged.push(entry.clone()),
        }
    }
    merged
}

/// A contribution, which [`remove`](Self::remove) takes back. Dropping it
/// leaves the contribution in place, as a dsh registration stays until its
/// disposer runs.
#[must_use = "dropping a registration keeps the contribution; call `remove` to take it back"]
pub struct Registration {
    state: Weak<Mutex<State>>,
    scope: Option<Scope>,
    id: u64,
}

impl Registration {
    /// Takes the contribution back. (dsh: the disposer.)
    pub fn remove(self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        state.listeners.retain(|(id, _, _)| *id != self.id);
        match &self.scope {
            None => state.global.remove(self.id),
            Some(scope) => {
                if let Some(layer) = state.scoped.get_mut(scope) {
                    layer.remove(self.id);
                    if layer.is_empty() {
                        state.scoped.remove(scope);
                    }
                }
            }
        }
    }
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registration")
            .field("scope", &self.scope)
            .field("id", &self.id)
            .finish()
    }
}

/// The registry of the prompt's sections, runtime context, tools and
/// variables, which the loop assembles before each model step. (dsh:
/// `SystemPrompt`, `ctx.systemPrompt`.)
///
/// Each contribution is global, or belongs to one agent's [`Scope`], where
/// it shadows a global one of the same name for that agent alone.
pub struct SystemPrompt {
    state: Arc<Mutex<State>>,
    tool_order: Option<Vec<String>>,
}

/// UTF-16 code-unit order, as JavaScript compares strings, so the order is
/// the same as dsh's on every machine.
fn compare_names(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Refuses a duplicate or a missing rest entry. (dsh: `validateToolOrder`.)
fn validate_tool_order(tool_order: &[String]) -> Result<(), PromptError> {
    let mut seen = Vec::new();
    for name in tool_order {
        if seen.contains(&name) {
            return Err(PromptError::new(format!(
                "toolOrder lists \"{name}\" more than once"
            )));
        }
        seen.push(name);
    }
    if !tool_order.iter().any(|name| name == TOOL_ORDER_REST) {
        return Err(PromptError::new(format!(
            "toolOrder must contain the \"{TOOL_ORDER_REST}\" rest entry (where unlisted tools \
             are inserted)"
        )));
    }
    Ok(())
}

/// The configured order, the rest by name at the rest entry; by name alone
/// without one. A sort that keeps equal names in collection order. (dsh:
/// `orderTools`.)
fn order_tools(
    mut tools: Vec<ToolSchema>,
    tool_order: Option<&[String]>,
    known: &[String],
) -> Result<Vec<ToolSchema>, PromptError> {
    if tools.iter().any(|tool| tool.name == TOOL_ORDER_REST) {
        return Err(PromptError::new(format!(
            "tool provider returned reserved tool name \"{TOOL_ORDER_REST}\" (reserved for \
             toolOrder's rest entry)"
        )));
    }
    let Some(tool_order) = tool_order else {
        tools.sort_by(|a, b| compare_names(&a.name, &b.name));
        return Ok(tools);
    };
    let unknown: Vec<&String> = tool_order
        .iter()
        .filter(|name| *name != TOOL_ORDER_REST && !known.contains(name))
        .collect();
    if !unknown.is_empty() {
        let mut names: Vec<&String> = known.iter().collect();
        names.sort_by(|a, b| compare_names(a, b));
        names.dedup();
        let names: Vec<&str> = names.iter().map(|name| name.as_str()).collect();
        let listed: Vec<String> = unknown.iter().map(|name| format!("\"{name}\"")).collect();
        return Err(PromptError::new(format!(
            "toolOrder lists unregistered tool{} {}; known tools: {}",
            if unknown.len() > 1 { "s" } else { "" },
            listed.join(", "),
            if names.is_empty() {
                "(none)".to_owned()
            } else {
                names.join(", ")
            }
        )));
    }
    let mut rest: Vec<ToolSchema> = tools
        .iter()
        .filter(|tool| !tool_order.contains(&tool.name))
        .cloned()
        .collect();
    rest.sort_by(|a, b| compare_names(&a.name, &b.name));
    let mut ordered = Vec::new();
    for name in tool_order {
        if name == TOOL_ORDER_REST {
            ordered.extend(rest.iter().cloned());
        } else {
            ordered.extend(tools.iter().filter(|tool| &tool.name == name).cloned());
        }
    }
    Ok(ordered)
}

impl SystemPrompt {
    /// A registry with the plugin's own sections: the identity, unless
    /// turned off, and the persona prefix and suffix.
    ///
    /// # Errors
    ///
    /// When `tool_order` lists a name twice, or lacks the rest entry.
    pub fn new(config: SystemPromptConfig) -> Result<Self, PromptError> {
        if let Some(order) = &config.tool_order {
            validate_tool_order(order)?;
        }
        let prompt = Self {
            state: Arc::new(Mutex::new(State::default())),
            tool_order: config.tool_order,
        };
        let builtins = [
            config.include_harness_identity.then(|| {
                Section::new(
                    HARNESS_IDENTITY_SECTION,
                    SectionOrder::HarnessIdentity.order(),
                    HARNESS_IDENTITY,
                )
            }),
            Some(Section::new(
                PERSONA_PREFIX_SECTION,
                SectionOrder::DeploymentPersonaPrefix.order(),
                config.persona_prefix,
            )),
            Some(Section::new(
                PERSONA_SUFFIX_SECTION,
                SectionOrder::DeploymentPersonaSuffix.order(),
                config.persona_suffix,
            )),
        ];
        for section in builtins.into_iter().flatten() {
            let _ = prompt.section(None, section)?;
        }
        if !config.include_runtime_context {
            let _ = prompt.suppress_runtime_context(None);
        }
        Ok(prompt)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every change is whole before anything that could panic, so a
        // poisoned lock still holds a whole registry.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn registration(&self, scope: Option<&Scope>, id: u64) -> Registration {
        Registration {
            state: Arc::downgrade(&self.state),
            scope: scope.cloned(),
            id,
        }
    }

    fn duplicate(kind: &str, name: &str, scope: Option<&Scope>, what: &str) -> PromptError {
        PromptError::new(match scope {
            None => format!(
                "prompt {kind} \"{name}\" is already registered (for a per-agent {what}, register \
                 it in that agent's scope instead)"
            ),
            Some(_) => format!("prompt {kind} \"{name}\" is already registered in this scope"),
        })
    }

    /// Adds a section, globally or for one agent. (dsh: `section`.)
    ///
    /// # Errors
    ///
    /// A name already in that layer, or an order that isn't finite.
    pub fn section(
        &self,
        scope: Option<&Scope>,
        section: Section,
    ) -> Result<Registration, PromptError> {
        if !section.order.is_finite() {
            return Err(PromptError::new(format!(
                "prompt section \"{}\" order must be a finite number",
                section.name
            )));
        }
        let mut state = self.lock();
        if state
            .layer(scope)
            .sections
            .iter()
            .any(|(_, known)| known.name == section.name)
        {
            return Err(Self::duplicate("section", &section.name, scope, "override"));
        }
        let id = state.id();
        state.layer(scope).sections.push((id, section));
        Ok(self.registration(scope, id))
    }

    /// Adds runtime context, globally or for one agent. (dsh: `context`.)
    ///
    /// # Errors
    ///
    /// As [`section`](Self::section).
    pub fn context(
        &self,
        scope: Option<&Scope>,
        context: PromptContext,
    ) -> Result<Registration, PromptError> {
        if !context.order.is_finite() {
            return Err(PromptError::new(format!(
                "prompt context \"{}\" order must be a finite number",
                context.name
            )));
        }
        let mut state = self.lock();
        if state
            .layer(scope)
            .contexts
            .iter()
            .any(|(_, known)| known.name == context.name)
        {
            return Err(Self::duplicate("context", &context.name, scope, "override"));
        }
        let id = state.id();
        state.layer(scope).contexts.push((id, context));
        Ok(self.registration(scope, id))
    }

    /// Leaves optional runtime context out, globally or for one agent,
    /// until removed. Several suppressors compose. (dsh:
    /// `suppressRuntimeContext`.)
    pub fn suppress_runtime_context(&self, scope: Option<&Scope>) -> Registration {
        let mut state = self.lock();
        let id = state.id();
        state.layer(scope).suppressors.push(id);
        self.registration(scope, id)
    }

    /// Adds a source of tool schemas, evaluated at each assembly. (dsh:
    /// `tools`.)
    pub fn tools(
        &self,
        scope: Option<&Scope>,
        provider: impl Fn(&AssembleContext) -> ToolProviderResult + Send + Sync + 'static,
    ) -> Registration {
        let mut state = self.lock();
        let id = state.id();
        state.layer(scope).tools.push((id, Arc::new(provider)));
        self.registration(scope, id)
    }

    /// Adds a variable, referenced as `{{name}}`, evaluated at each
    /// assembly. A provider may give `None`, but rendering a reference to
    /// it then fails. (dsh: `variable`.)
    ///
    /// # Errors
    ///
    /// A name that isn't `[a-z][a-z0-9_]*`, or one already in that layer.
    pub fn variable(
        &self,
        scope: Option<&Scope>,
        name: &str,
        provider: impl Fn(&AssembleContext) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Registration, PromptError> {
        if !valid_variable_name(name) {
            return Err(PromptError::new(format!(
                "invalid prompt variable name \"{name}\" (must match /^[a-z][a-z0-9_]*$/)"
            )));
        }
        let mut state = self.lock();
        if state
            .layer(scope)
            .variables
            .iter()
            .any(|(_, known, _)| known == name)
        {
            return Err(Self::duplicate("variable", name, scope, "value"));
        }
        let id = state.id();
        state
            .layer(scope)
            .variables
            .push((id, name.to_owned(), Arc::new(provider)));
        Ok(self.registration(scope, id))
    }

    /// Adds a listener that shapes assemblies: every one when global, one
    /// agent's when scoped. Listeners run in the order they were added.
    /// (dsh: a `system-prompt/assemble` listener.)
    pub fn on_assemble(
        &self,
        scope: Option<&Scope>,
        listener: Arc<dyn AssembleListener>,
    ) -> Registration {
        let mut state = self.lock();
        let id = state.id();
        state.listeners.push((id, scope.cloned(), listener));
        self.registration(scope, id)
    }

    /// The variables, each provider called with no lock held. Providers
    /// added meanwhile are called too; one removed before its turn isn't.
    fn variables(&self, context: &AssembleContext) -> Variables {
        let scope = context.scope.as_ref();
        let mut variables = Variables::new();
        for scoped in [false, true] {
            if scoped && scope.is_none() {
                break;
            }
            let mut index = 0;
            loop {
                let entry = {
                    let state = self.lock();
                    let layer = if scoped {
                        state.scope_layer(scope)
                    } else {
                        Some(&state.global)
                    };
                    layer.and_then(|layer| layer.variables.get(index).cloned())
                };
                let Some((_, name, provider)) = entry else {
                    break;
                };
                variables.set(name, provider(context));
                index += 1;
            }
        }
        variables
    }

    /// Assembles the prompt for `context`: the global and the scope's
    /// contributions, scoped ones shadowing global twins; sections by
    /// order, then name; tools in their canonical order; then the
    /// listeners. A complete section is restored after them, and
    /// suppressed context stays out. (dsh: `assemble`.)
    ///
    /// # Errors
    ///
    /// Two complete sections, a tool named like the rest entry, a
    /// configured tool no source knows, or a listener's failure.
    pub async fn assemble(&self, context: &AssembleContext) -> Result<PromptAssembly, PromptError> {
        let scope = context.scope.as_ref();
        let variables = self.variables(context);
        let (mut sections, contexts, providers, suppressed, listeners) = {
            let state = self.lock();
            let providers: Vec<ToolProvider> = state
                .global
                .tools
                .iter()
                .chain(
                    state
                        .scope_layer(scope)
                        .into_iter()
                        .flat_map(|layer| layer.tools.iter()),
                )
                .map(|(_, provider)| Arc::clone(provider))
                .collect();
            let listeners: Vec<Arc<dyn AssembleListener>> = state
                .listeners
                .iter()
                .filter(|(_, owner, _)| owner.is_none() || owner.as_ref() == scope)
                .map(|(_, _, listener)| Arc::clone(listener))
                .collect();
            (
                state.sections(scope),
                state.contexts(scope),
                providers,
                state.suppressed(scope),
                listeners,
            )
        };

        let mut collected = Vec::new();
        let mut known: Vec<String> = Vec::new();
        for provider in providers {
            let result = provider(context);
            let names = result.known_names.unwrap_or_else(|| {
                result
                    .schemas
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect()
            });
            collected.extend(result.schemas);
            for name in names {
                if !known.contains(&name) {
                    known.push(name);
                }
            }
        }

        sections.sort_by(|a, b| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(Ordering::Equal)
                .then_with(|| compare_names(&a.name, &b.name))
        });
        let complete: Vec<&Section> = sections.iter().filter(|section| section.complete).collect();
        if complete.len() > 1 {
            let names: Vec<String> = complete
                .iter()
                .map(|section| quoted(&section.name))
                .collect();
            return Err(PromptError::new(format!(
                "multiple complete prompt sections are active: {}",
                names.join(", ")
            )));
        }
        let mut complete_section = None;
        let assembled: Vec<AssembledSection> = sections
            .iter()
            .map(|section| {
                let assembled = AssembledSection {
                    name: section.name.clone(),
                    text: section.text.resolve(context),
                    interpolate: section.interpolate,
                };
                if section.complete {
                    complete_section = Some(assembled.clone());
                }
                assembled
            })
            .collect();

        let mut active: Vec<&PromptContext> = contexts
            .iter()
            .filter(|entry| !suppressed || entry.required)
            .collect();
        active.sort_by(|a, b| a.order.partial_cmp(&b.order).unwrap_or(Ordering::Equal));
        let assembly = PromptAssembly {
            sections: assembled,
            contexts: active.iter().map(|entry| entry.resolve(context)).collect(),
            tools: order_tools(collected, self.tool_order.as_deref(), &known)?,
            variables,
        };
        let mut transformed = AssembleNext {
            rest: &listeners,
            context,
        }
        .run(assembly)
        .await?;
        if let Some(section) = complete_section {
            transformed.sections = vec![section];
        }
        if suppressed {
            transformed.contexts.retain(|entry| {
                contexts
                    .iter()
                    .any(|known| known.required && known.name == entry.name)
            });
        }
        Ok(transformed)
    }

    /// `assembly` with its registered runtime context read again, for a
    /// later attempt of the same step. Context the listeners added stays
    /// as accepted; current suppression applies; required context missing
    /// from it comes back in order. Sections, tools and variables are kept
    /// as they are. (dsh: `refreshContext`.)
    pub fn refresh_context(
        &self,
        assembly: &PromptAssembly,
        context: &AssembleContext,
    ) -> PromptAssembly {
        let scope = context.scope.as_ref();
        let (providers, suppressed) = {
            let state = self.lock();
            (state.contexts(scope), state.suppressed(scope))
        };
        let provider = |name: &str| providers.iter().find(|entry| entry.name == name);
        let mut required: Vec<&PromptContext> =
            providers.iter().filter(|entry| entry.required).collect();
        let mut contexts: Vec<AssembledContext> = Vec::new();
        for accepted in &assembly.contexts {
            let current = provider(&accepted.name);
            required.retain(|entry| entry.name != accepted.name);
            if suppressed && !current.is_some_and(|entry| entry.required) {
                continue;
            }
            contexts.push(match current {
                Some(entry) => entry.resolve(context),
                None => accepted.clone(),
            });
        }
        for missing in required {
            let at = contexts
                .iter()
                .position(|entry| {
                    provider(&entry.name).is_some_and(|next| next.order > missing.order)
                })
                .unwrap_or(contexts.len());
            contexts.insert(at, missing.resolve(context));
        }
        PromptAssembly {
            contexts,
            ..assembly.clone()
        }
    }
}

impl fmt::Debug for SystemPrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemPrompt")
            .field("tool_order", &self.tool_order)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_compare_by_utf16_code_units() {
        // U+FF5E sorts before U+1F600 in UTF-16, after it in UTF-8.
        assert_eq!(compare_names("\u{FF5E}", "\u{1F600}"), Ordering::Greater);
        assert_eq!(compare_names("zeta", "äther"), Ordering::Less);
    }

    #[test]
    fn the_working_directorys_context_goes_where_dshs_does() {
        assert_eq!(ContextOrder::WorkingDirectory.order(), 100.0);
    }

    #[test]
    fn karakas_placements_are_dshs() {
        assert_eq!(SectionOrder::HarnessIdentity.order(), -1000.0);
        assert_eq!(SectionOrder::DeploymentPersonaPrefix.order(), 0.0);
        assert_eq!(SectionOrder::DeploymentPersonaSuffix.order(), 10200.0);
    }
}
