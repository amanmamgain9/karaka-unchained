//! Scoped tools, restrictions and scoped gate listeners. Ported from dsh's
//! `core/tools/tests/scoped.spec.ts`: "scoped tool registration",
//! "restrict()", "restrict() over an inherited scope layer", and the first
//! case of "scoped execution dispatch".
//!
//! dsh's nested scope (a child bound under a parent) is a session under its
//! preset here: Karaka's chain is fixed at global, preset, then session.
//!
//! Not ported:
//!
//! - "keeps final-result observers synchronous" and the other "scoped
//!   execution dispatch" cases: guards, `tools/result` observers and
//!   argument snapshots aren't ported (see open ends).
//! - "disposing the scope unwinds its registrations": the kernel unloads no
//!   plugin and a tool is never removed. A restriction is, and
//!   `restrictions_intersect_and_lift_one_by_one` lifts one.
//! - "compiles the readonly filter values at registration": a Rust
//!   restriction is moved into the registry, so the caller can't change it
//!   afterwards.

use std::sync::Arc;

use karaka_agent::{Agent, FrameListener, FrameListeners, FrameSubscription, InboxError, WhenIdle};
use karaka_model::{ToolCall, ToolSchema};
use karaka_scope::{PresetId, ScopeChain, ScopeKey};
use karaka_session::{CancelCause, SessionId, UserMessage};
use karaka_tools::{
    PreExecuteFuture, PreExecuteListener, PreExecuteNext, PreToolDecision, RestrictionRegistration,
    Tool, ToolExecution, ToolFuture, ToolOutput, ToolRegistry, ToolRegistryError, ToolRestriction,
    ToolRun,
};
use serde_json::Value;

/// An agent seeing `chain`.
struct Stub {
    id: SessionId,
    chain: ScopeChain,
    frames: FrameListeners,
}

impl Agent for Stub {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn followup(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn steer(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn inject(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn cancel(&self, _: CancelCause) {}

    fn when_idle(&self) -> WhenIdle<'_> {
        Box::pin(async {})
    }

    fn subscribe(&self, listener: FrameListener) -> FrameSubscription {
        self.frames.subscribe(listener)
    }

    fn scope(&self) -> ScopeChain {
        self.chain.clone()
    }
}

/// A session agent with no preset.
fn agent(id: &str) -> Arc<dyn Agent> {
    in_preset(None, id)
}

/// A session agent in `preset`, as dsh's child bound under a parent.
fn in_preset(preset: Option<&str>, id: &str) -> Arc<dyn Agent> {
    Arc::new(Stub {
        id: SessionId::new(id),
        chain: ScopeChain::new(preset.map(PresetId::new), Some(SessionId::new(id))),
        frames: FrameListeners::new(),
    })
}

fn session(id: &str) -> ScopeKey {
    ScopeKey::Session(SessionId::new(id))
}

fn preset(id: &str) -> ScopeKey {
    ScopeKey::Preset(PresetId::new(id))
}

fn preset_chain(id: &str) -> ScopeChain {
    ScopeChain::new(Some(PresetId::new(id)), None)
}

/// dsh's `tool(name, reply)`: answers `reply`.
struct Named {
    name: String,
    reply: String,
}

impl Tool for Named {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: format!("tool {}", self.name),
            parameters: r#"{"type":"object","properties":{}}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move { ToolOutput::ok(self.reply.clone()) })
    }
}

fn tool(name: &str) -> Arc<dyn Tool> {
    replying(name, &format!("ran:{name}"))
}

fn replying(name: &str, reply: &str) -> Arc<dyn Tool> {
    Arc::new(Named {
        name: name.into(),
        reply: reply.into(),
    })
}

/// dsh's `run`: the result's text.
async fn run(registry: &ToolRegistry, name: &str, agent: Option<&Arc<dyn Agent>>) -> String {
    let call = ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: "{}".into(),
    };
    registry.execute(&call, agent.cloned()).await.content
}

fn names(registry: &ToolRegistry, viewer: &ScopeChain) -> Vec<String> {
    let mut names: Vec<String> = registry
        .schemas_for(viewer)
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    names.sort();
    names
}

fn chain_of(agent: &Arc<dyn Agent>) -> ScopeChain {
    agent.scope()
}

fn allow(names: &[&str]) -> ToolRestriction {
    ToolRestriction {
        allow: Some(names.iter().map(|name| (*name).to_owned()).collect()),
        deny: None,
    }
}

fn deny(names: &[&str]) -> ToolRestriction {
    ToolRestriction {
        allow: None,
        deny: Some(names.iter().map(|name| (*name).to_owned()).collect()),
    }
}

// "files a scoped tool in its layer: visible/executable for that scope only"
#[tokio::test]
async fn a_scoped_tool_is_seen_and_run_by_its_scope_only() {
    let registry = ToolRegistry::new();
    let a = agent("a");
    let other = agent("other");
    registry.register(tool("shared")).unwrap();
    registry
        .register_in(Some(&session("a")), tool("mine"))
        .unwrap();

    assert_eq!(names(&registry, &chain_of(&a)), ["mine", "shared"]);
    assert_eq!(names(&registry, &ScopeChain::global()), ["shared"]);
    assert_eq!(names(&registry, &chain_of(&other)), ["shared"]);
    assert_eq!(
        registry.schemas(),
        registry.schemas_for(&ScopeChain::global())
    );

    assert_eq!(run(&registry, "mine", Some(&a)).await, "ran:mine");
    // Out of view, a tool is indistinguishable from one that doesn't exist.
    assert_eq!(
        run(&registry, "mine", Some(&other)).await,
        "Error: unknown tool \"mine\""
    );
    assert_eq!(
        run(&registry, "mine", None).await,
        "Error: unknown tool \"mine\""
    );
}

// "scoped shadows global on a name conflict, in either registration order"
#[tokio::test]
async fn a_scoped_tool_shadows_a_global_one_either_way_round() {
    let registry = ToolRegistry::new();
    let a = agent("a");
    registry
        .register_in(Some(&session("a")), replying("bash", "restricted-bash"))
        .unwrap();
    registry.register(replying("bash", "global-bash")).unwrap();
    assert_eq!(run(&registry, "bash", Some(&a)).await, "restricted-bash");
    assert_eq!(run(&registry, "bash", None).await, "global-bash");
    // Exactly one `bash` in the scope's view: the shadow, not both.
    assert_eq!(names(&registry, &chain_of(&a)), ["bash"]);
    let shadow = registry.get("bash", &chain_of(&a)).unwrap();
    let global = registry.get("bash", &ScopeChain::global()).unwrap();
    assert!(!Arc::ptr_eq(&shadow, &global));
}

// "rejects a duplicate name within one layer, naming agent.ctx for the
// global case"
#[test]
fn a_duplicate_within_one_layer_is_refused() {
    let registry = ToolRegistry::new();
    registry.register(tool("x")).unwrap();
    let error = registry.register(tool("x")).unwrap_err();
    assert!(error.to_string().contains("that agent's scope"), "{error}");
    registry
        .register_in(Some(&session("a")), tool("y"))
        .unwrap();
    let error = registry
        .register_in(Some(&session("a")), tool("y"))
        .unwrap_err();
    assert_eq!(error, ToolRegistryError::DuplicateInScope("y".into()));
    assert_eq!(
        error.to_string(),
        "tool \"y\" is already registered in this scope"
    );
    // Another scope's layer is another layer.
    registry
        .register_in(Some(&session("b")), tool("y"))
        .unwrap();
    registry
        .register_in(Some(&preset("coach")), tool("y"))
        .unwrap();
}

// A preset's tool is seen through every chain naming the preset, and by no
// other viewer.
#[tokio::test]
async fn a_presets_tool_is_seen_through_chains_naming_the_preset() {
    let registry = ToolRegistry::new();
    registry.register(tool("shared")).unwrap();
    registry
        .register_in(Some(&preset("coach")), tool("coach_notes"))
        .unwrap();
    let coached = in_preset(Some("coach"), "s1");
    let elsewhere = in_preset(Some("other"), "s2");
    let alone = agent("s3");
    // A session named like the preset isn't in it.
    let lookalike = agent("coach");

    assert_eq!(
        names(&registry, &chain_of(&coached)),
        ["coach_notes", "shared"]
    );
    assert_eq!(
        names(&registry, &preset_chain("coach")),
        ["coach_notes", "shared"]
    );
    for viewer in [&elsewhere, &alone, &lookalike] {
        assert_eq!(names(&registry, &chain_of(viewer)), ["shared"]);
        assert_eq!(
            run(&registry, "coach_notes", Some(viewer)).await,
            "Error: unknown tool \"coach_notes\""
        );
    }
    assert_eq!(
        run(&registry, "coach_notes", Some(&coached)).await,
        "ran:coach_notes"
    );
}

// "masks global tools, merges scope-local tools afterward, and keeps
// assembly with execution"
#[tokio::test]
async fn a_restriction_masks_inherited_tools_but_not_the_scopes_own() {
    let registry = ToolRegistry::new();
    let a = agent("a");
    registry.register(tool("read")).unwrap();
    registry.register(tool("bash")).unwrap();
    registry
        .register_in(Some(&session("a")), tool("capture"))
        .unwrap();
    let _restriction = registry.restrict(&chain_of(&a), allow(&["read"])).unwrap();

    assert_eq!(names(&registry, &chain_of(&a)), ["capture", "read"]);
    assert_eq!(
        run(&registry, "bash", Some(&a)).await,
        "Error: unknown tool \"bash\""
    );
    assert_eq!(run(&registry, "read", Some(&a)).await, "ran:read");
    assert_eq!(run(&registry, "capture", Some(&a)).await, "ran:capture");
    // Other scopes and the global view are untouched.
    assert_eq!(names(&registry, &ScopeChain::global()), ["bash", "read"]);
    assert_eq!(names(&registry, &chain_of(&agent("b"))), ["bash", "read"]);
}

// "applies snapshotted filters to the live global registry before merging
// later scope-local tools"
#[tokio::test]
async fn a_restriction_applies_to_tools_registered_after_it() {
    let registry = ToolRegistry::new();
    let denied = agent("denied");
    let allowed = agent("allowed");
    registry.register(tool("read")).unwrap();
    registry.register(tool("bash")).unwrap();
    let _deny = registry
        .restrict(&chain_of(&denied), deny(&["bash"]))
        .unwrap();
    let _allow = registry
        .restrict(&chain_of(&allowed), allow(&["read"]))
        .unwrap();

    registry.register(tool("web")).unwrap();
    registry
        .register_in(Some(&session("denied")), tool("denied-local"))
        .unwrap();
    registry
        .register_in(Some(&session("allowed")), tool("allowed-local"))
        .unwrap();

    assert_eq!(
        names(&registry, &chain_of(&denied)),
        ["denied-local", "read", "web"]
    );
    assert_eq!(
        names(&registry, &chain_of(&allowed)),
        ["allowed-local", "read"]
    );
    assert_eq!(run(&registry, "web", Some(&denied)).await, "ran:web");
    assert_eq!(
        run(&registry, "web", Some(&allowed)).await,
        "Error: unknown tool \"web\""
    );
    assert_eq!(
        run(&registry, "denied-local", Some(&denied)).await,
        "ran:denied-local"
    );
    assert_eq!(
        run(&registry, "allowed-local", Some(&allowed)).await,
        "ran:allowed-local"
    );
}

// "composes multiple restrictions by intersection and lifts each
// independently"
#[test]
fn restrictions_intersect_and_lift_one_by_one() {
    let registry = ToolRegistry::new();
    let a = agent("a");
    for name in ["a", "b", "c"] {
        registry.register(tool(name)).unwrap();
    }
    let lift_allow = registry
        .restrict(&chain_of(&a), allow(&["a", "b"]))
        .unwrap();
    let _deny = registry.restrict(&chain_of(&a), deny(&["b"])).unwrap();
    assert_eq!(names(&registry, &chain_of(&a)), ["a"]);
    lift_allow.remove();
    // The deny stays after the allow list is lifted.
    assert_eq!(names(&registry, &chain_of(&a)), ["a", "c"]);
}

// "fails loud on an unscoped call, an empty filter, and names it does not
// inherit"
#[test]
fn a_bad_restriction_is_refused_in_dshs_words() {
    let registry = ToolRegistry::new();
    let a = agent("a");
    registry.register(tool("real")).unwrap();
    registry
        .register_in(Some(&session("a")), tool("local"))
        .unwrap();
    let refused = |viewer: &ScopeChain, restriction: ToolRestriction| {
        registry
            .restrict(viewer, restriction)
            .map(RestrictionRegistration::remove)
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        refused(&ScopeChain::global(), deny(&["real"])),
        "tools.restrict() requires a scoped context (agent.ctx): a context-global restriction \
         would mask every agent — deny the tool for the intended agent instead"
    );
    assert_eq!(
        refused(&chain_of(&a), ToolRestriction::default()),
        "tools.restrict({}) is a no-op: pass `allow` and/or `deny` (an empty filter is almost \
         always a materialized-empty-config bug)"
    );
    // A scope's own tool is outside its own filter, so naming it is a
    // mistake, not a silent no-op.
    assert_eq!(
        refused(&chain_of(&a), allow(&["local"])),
        "tools.restrict() names unknown global tool \"local\"; known global tools: real"
    );
    assert_eq!(
        refused(&chain_of(&a), allow(&["reall"])),
        "tools.restrict() names unknown global tool \"reall\"; known global tools: real"
    );
    assert_eq!(
        refused(&chain_of(&a), deny(&["ghost", "wraith"])),
        "tools.restrict() names unknown global tools \"ghost\", \"wraith\"; known global tools: \
         real"
    );

    let empty = ToolRegistry::new();
    let error = empty
        .restrict(&chain_of(&agent("empty")), deny(&["ghost"]))
        .map(RestrictionRegistration::remove)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "tools.restrict() names unknown global tool \"ghost\"; known global tools: (none)"
    );
}

// "filters tools the child inherits from an ancestor scope, not only global
// ones"
#[tokio::test]
async fn a_session_restriction_masks_what_it_inherits_from_its_preset() {
    let registry = ToolRegistry::new();
    registry
        .register_in(Some(&preset("parent")), tool("bash"))
        .unwrap();
    registry
        .register_in(Some(&preset("parent")), tool("read"))
        .unwrap();
    let child = in_preset(Some("parent"), "child");

    assert_eq!(names(&registry, &chain_of(&child)), ["bash", "read"]);
    let _restriction = registry
        .restrict(&chain_of(&child), deny(&["bash"]))
        .unwrap();

    assert_eq!(names(&registry, &chain_of(&child)), ["read"]);
    assert_eq!(
        run(&registry, "bash", Some(&child)).await,
        "Error: unknown tool \"bash\""
    );
    // The preset keeps its whole surface: the child's filter is its own.
    assert_eq!(names(&registry, &preset_chain("parent")), ["bash", "read"]);
    assert_eq!(
        names(&registry, &chain_of(&in_preset(Some("parent"), "sibling"))),
        ["bash", "read"]
    );
}

// "keeps the child's own registrations outside its own filter"
#[tokio::test]
async fn a_sessions_own_tools_stay_outside_its_restriction() {
    let registry = ToolRegistry::new();
    registry
        .register_in(Some(&preset("parent")), tool("bash"))
        .unwrap();
    registry
        .register_in(Some(&preset("parent")), tool("read"))
        .unwrap();
    let child = in_preset(Some("parent"), "child");
    registry
        .register_in(Some(&session("child")), tool("report"))
        .unwrap();

    let _restriction = registry
        .restrict(&chain_of(&child), allow(&["read"]))
        .unwrap();

    assert_eq!(names(&registry, &chain_of(&child)), ["read", "report"]);
    assert_eq!(run(&registry, "report", Some(&child)).await, "ran:report");
}

// "lets an ancestor's restriction reach every scope nested inside it"
#[test]
fn a_presets_restriction_reaches_its_sessions() {
    let registry = ToolRegistry::new();
    registry.register(tool("web")).unwrap();
    registry
        .register_in(Some(&preset("parent")), tool("bash"))
        .unwrap();
    let child = in_preset(Some("parent"), "child");
    let _restriction = registry
        .restrict(&preset_chain("parent"), deny(&["web"]))
        .unwrap();

    assert_eq!(names(&registry, &chain_of(&child)), ["bash"]);
    assert_eq!(names(&registry, &preset_chain("parent")), ["bash"]);
    // Outside the preset, nothing is masked.
    assert_eq!(names(&registry, &chain_of(&agent("free"))), ["web"]);
}

/// Denies every call, noting whose.
struct Veto(std::sync::Mutex<Vec<Option<String>>>);

impl PreExecuteListener for Veto {
    fn decide<'a>(
        &'a self,
        call: &'a ToolExecution,
        _: PreExecuteNext<'a>,
    ) -> PreExecuteFuture<'a> {
        self.0
            .lock()
            .unwrap()
            .push(call.agent.as_ref().map(|agent| agent.id().to_string()));
        Box::pin(async {
            Ok(PreToolDecision::Deny {
                reason: "scoped veto".into(),
            })
        })
    }
}

// "an agent.ctx pre-execute listener gates only its own agent (and never
// subject-less calls)", and a preset's listener gates each of its agents.
#[tokio::test]
async fn a_scoped_listener_gates_only_the_agents_whose_chain_holds_it() {
    let registry = ToolRegistry::new();
    registry.register(tool("t")).unwrap();
    let a = agent("a");
    let other = agent("other");
    let veto = Arc::new(Veto(std::sync::Mutex::new(Vec::new())));
    let _scoped = registry.on_pre_execute(Some(&session("a")), Arc::clone(&veto) as _);

    assert_eq!(run(&registry, "t", Some(&a)).await, "Error: scoped veto");
    assert_eq!(run(&registry, "t", Some(&other)).await, "ran:t");
    assert_eq!(run(&registry, "t", None).await, "ran:t");
    assert_eq!(*veto.0.lock().unwrap(), [Some("a".to_owned())]);

    let coach = Arc::new(Veto(std::sync::Mutex::new(Vec::new())));
    let _preset = registry.on_pre_execute(Some(&preset("coach")), Arc::clone(&coach) as _);
    let coached = in_preset(Some("coach"), "s1");
    assert_eq!(
        run(&registry, "t", Some(&coached)).await,
        "Error: scoped veto"
    );
    assert_eq!(run(&registry, "t", Some(&other)).await, "ran:t");
    assert_eq!(*coach.0.lock().unwrap(), [Some("s1".to_owned())]);
}
