//! Presets on a real runtime: their plugins set up under their scopes, the
//! registry, selection, and agents composed from their sessions. (dsh:
//! `agent-preset-registry/tests/registry.spec.ts`, `session.spec.ts` and
//! `mount.spec.ts`, with the fixture row `contribute`.)
//!
//! Not ported, as the crate docs explain: every case about revisions,
//! retiring, leases and rebinding a live tree ("retains a replaced
//! revision", "removes definitions without revoking live bindings",
//! "rebinds a blank scope", "retains a retired revision for an in-flight
//! cold read", "retains a newly acquired generation", "recomposes and logs
//! a repeated selection once the preset definition was replaced", "does
//! not bind a definition removed while acquisition yields"); `composeFrom`
//! ("refuses to inherit a revision owned by a different registry", the
//! child half of "refuses an unscoped binding"), which waits on child
//! agents; the Loader's and Cordis's ("allows an isolated service",
//! "reports every process-global service", the `global-service` case,
//! "waits for a Host provider still activating", "keeps a row waiting for
//! an absent service mounted", the inventory, YAML and `entryListProblem`
//! cases, and all of `mount.spec.ts` but the failure lines); the settings
//! cases (`selectedDefault`, `modeSelectionEnabled`, Settings pages); and
//! "does not lose the durable selection when a tools observer rejects",
//! since there is no `tools/change` event.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use karaka_agent::{Agent, Agents, OpenError};
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_agent_presets::{
    AgentPresetProjection, AgentPresets, AgentPresetsConfig, AgentPresetsPlugin, PresetContext,
    PresetDefinition, PresetError, PresetFuture, PresetPlugin, SELECTED_EVENT,
};
use karaka_kernel::{PortId, Runtime};
use karaka_model::{
    Block, ErrorKind, Event, EventStream, FinishReason, Model, ModelError, ModelRegistry,
    ModelsPlugin, Request, Usage, stream_of,
};
use karaka_scope::ScopeChain;
use karaka_session::{
    Session, SessionEvent, SessionHeader, SessionId, SessionStore, SessionsPlugin, UserMessage,
    UserSource,
};
use karaka_session_projection::{SessionProjections, SessionProjectionsPlugin};
use karaka_system_prompt::{
    AssembleContext, Section, SystemPrompt, SystemPromptConfig, SystemPromptPlugin,
};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun, ToolsPlugin};
use serde_json::{Value, json};

const ROUTE: &str = "mock-presets";

/// dsh's fixture row `contribute`: one tool and one prompt section, both
/// named from its config, under the preset's scope.
struct Contribute(&'static str);

struct Named(&'static str);

impl Tool for Named {
    fn schema(&self) -> karaka_model::ToolSchema {
        karaka_model::ToolSchema {
            name: self.0.into(),
            description: format!("fixture tool {}", self.0),
            parameters: r#"{"type":"object","properties":{},"additionalProperties":false}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move { ToolOutput::ok(self.0) })
    }
}

impl PresetPlugin for Contribute {
    fn name(&self) -> &str {
        "contribute"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ToolRegistry>(), PortId::of::<SystemPrompt>()]
    }

    fn setup<'a, 'p>(&'a self, ctx: &'a PresetContext<'a, 'p>) -> PresetFuture<'a> {
        Box::pin(async move {
            ctx.get::<ToolRegistry>()
                .register_in(Some(ctx.scope()), Arc::new(Named(self.0)))?;
            let _ = ctx.get::<SystemPrompt>().section(
                Some(ctx.scope()),
                Section::new(
                    format!("preset:{}", self.0),
                    10.0,
                    format!("section for {}", self.0),
                ),
            )?;
            Ok(())
        })
    }
}

/// dsh's fixture row `throws`: its activation rejects.
struct Throws(&'static str);

impl PresetPlugin for Throws {
    fn name(&self) -> &str {
        "throws"
    }

    fn id(&self) -> &str {
        self.0
    }

    fn setup<'a, 'p>(&'a self, _: &'a PresetContext<'a, 'p>) -> PresetFuture<'a> {
        Box::pin(async { Err("refused".into()) })
    }
}

/// A port nobody provides.
struct Absent;

/// dsh's fixture row `needs-missing`: it needs a service nothing provides.
struct NeedsMissing;

impl PresetPlugin for NeedsMissing {
    fn name(&self) -> &str {
        "needs-missing"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<Absent>()]
    }

    fn setup<'a, 'p>(&'a self, _: &'a PresetContext<'a, 'p>) -> PresetFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

/// dsh's `contribution(tool)`: a preset named for its tool.
fn contribution(tool: &'static str) -> PresetDefinition {
    PresetDefinition::new(tool, vec![Box::new(Contribute(tool))])
}

/// Plays one scripted reply per call, and keeps every request.
struct Scripted {
    replies: Mutex<VecDeque<Vec<Block>>>,
    requests: Mutex<Vec<Request>>,
}

impl Model for Scripted {
    fn provider(&self) -> &str {
        "mock"
    }

    fn stream(&self, request: Request) -> EventStream {
        self.requests.lock().unwrap().push(request);
        let events: Vec<Result<Event, ModelError>> = match self.replies.lock().unwrap().pop_front()
        {
            Some(blocks) => stream_of(&blocks, FinishReason::Stop, Usage::default())
                .into_iter()
                .map(Ok)
                .collect(),
            None => vec![Err(ModelError::new(ErrorKind::InvalidRequest, "exhausted"))],
        };
        Box::pin(Playback(events.into()))
    }
}

struct Playback(VecDeque<Result<Event, ModelError>>);

impl Stream for Playback {
    type Item = Result<Event, ModelError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.pop_front())
    }
}

fn host(presets: AgentPresetsPlugin) -> Vec<Box<dyn karaka_kernel::DynPlugin>> {
    vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig {
            include_harness_identity: false,
            ..SystemPromptConfig::default()
        })),
        Box::new(ToolsPlugin),
        Box::new(ModelsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new(ROUTE, "mock-model"))),
        Box::new(presets),
    ]
}

/// dsh's `harness`, with `default: standard`.
fn registry() -> AgentPresetsPlugin {
    AgentPresetsPlugin::new(AgentPresetsConfig::new("standard"))
}

struct Fixture {
    presets: Arc<AgentPresets>,
    agents: Arc<dyn Agents>,
    sessions: Arc<SessionStore>,
    tools: Arc<ToolRegistry>,
    prompt: Arc<SystemPrompt>,
    projections: Arc<SessionProjections>,
    model: Arc<Scripted>,
    _runtime: Runtime,
}

async fn fixture(presets: AgentPresetsPlugin, replies: Vec<Vec<Block>>) -> Fixture {
    let runtime = Runtime::start(host(presets)).await.unwrap();
    let model = Arc::new(Scripted {
        replies: Mutex::new(replies.into()),
        requests: Mutex::new(Vec::new()),
    });
    runtime
        .get::<ModelRegistry>()
        .register(ROUTE, Arc::clone(&model) as Arc<dyn Model>)
        .unwrap();
    Fixture {
        presets: runtime.get::<AgentPresets>(),
        agents: runtime.get::<dyn Agents>(),
        sessions: runtime.get::<SessionStore>(),
        tools: runtime.get::<ToolRegistry>(),
        prompt: runtime.get::<SystemPrompt>(),
        projections: runtime.get::<SessionProjections>(),
        model,
        _runtime: runtime,
    }
}

impl Fixture {
    /// dsh's `agentOn`: an agent for a new session, naming `preset` in its
    /// header, or created by the loop with the default.
    fn agent_on(&self, id: &str, preset: Option<&str>) -> Result<Arc<dyn Agent>, OpenError> {
        if let Some(preset) = preset {
            let mut header = SessionHeader::new(SessionId::new(id), 1);
            header.agent_preset = Some(preset.into());
            self.sessions.create_with(header).unwrap();
        }
        self.agents.open(SessionId::new(id))
    }

    fn session(&self, agent: &Arc<dyn Agent>) -> Session {
        self.sessions.get(agent.id()).unwrap()
    }

    fn tool_names(&self, agent: &Arc<dyn Agent>) -> Vec<String> {
        self.tools
            .schemas_for(&agent.scope())
            .into_iter()
            .map(|schema| schema.name)
            .collect()
    }

    fn recorded(&self, agent: &Arc<dyn Agent>) -> Option<String> {
        self.projections
            .state_of::<AgentPresetProjection>(&self.session(agent))
            .and_then(|state| (*state).clone())
    }

    fn selections(&self, agent: &Arc<dyn Agent>) -> usize {
        self.session(agent)
            .events()
            .iter()
            .filter(|entry| entry.event.event_type() == SELECTED_EVENT)
            .count()
    }
}

/// Why the open was refused.
fn compose_error(opened: Result<Arc<dyn Agent>, OpenError>) -> karaka_agent::ComposeError {
    match opened {
        Err(OpenError::Compose(error)) => error,
        Err(other) => panic!("expected a compose error, got {other:?}"),
        Ok(_) => panic!("expected the open to be refused"),
    }
}

fn header_preset(f: &Fixture, id: &str) -> Option<String> {
    f.sessions
        .get(&SessionId::new(id))
        .unwrap()
        .header()
        .agent_preset
        .clone()
}

// "eagerly scopes tool and prompt registrations and shares one revision
// across Agents"
#[tokio::test]
async fn each_presets_tools_and_sections_reach_only_its_agents() {
    let f = fixture(
        registry()
            .preset(contribution("standard"))
            .preset(contribution("minimal")),
        Vec::new(),
    )
    .await;
    let first = f.agent_on("first", None).unwrap();
    let second = f.agent_on("second", Some("minimal")).unwrap();
    let third = f.agent_on("third", None).unwrap();

    assert_eq!(first.scope().preset.unwrap().as_str(), "standard");
    assert_eq!(f.tool_names(&first), ["standard"]);
    assert_eq!(f.tool_names(&second), ["minimal"]);
    assert_eq!(f.tool_names(&third), ["standard"]);
    assert!(f.tools.schemas().is_empty());
    let prompt = f
        .prompt
        .assemble(&AssembleContext::scoped(first.scope()))
        .await
        .unwrap();
    let sections: Vec<&str> = prompt
        .sections
        .iter()
        .map(|row| row.name.as_str())
        .collect();
    assert!(sections.contains(&"preset:standard"), "{sections:?}");
    assert!(!sections.contains(&"preset:minimal"), "{sections:?}");
    // A session the loop creates names the default in its header.
    assert_eq!(header_preset(&f, "first").as_deref(), Some("standard"));
}

// End to end: two presets, each with a scoped tool and section, and two
// sessions whose requests each list only their own preset's.
#[tokio::test]
async fn each_sessions_request_lists_its_own_presets_tool_and_section() {
    let f = fixture(
        registry()
            .preset(contribution("standard"))
            .preset(contribution("minimal")),
        vec![
            vec![Block::Text("one".into())],
            vec![Block::Text("two".into())],
        ],
    )
    .await;
    for (id, preset) in [("s-standard", None), ("s-minimal", Some("minimal"))] {
        let agent = f.agent_on(id, preset).unwrap();
        agent
            .followup(UserMessage::text("hi", UserSource::user(), "hi"))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), agent.when_idle())
            .await
            .unwrap();
    }
    let requests = f.model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for (request, own, other) in [
        (&requests[0], "standard", "minimal"),
        (&requests[1], "minimal", "standard"),
    ] {
        let tools: Vec<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(tools, [own]);
        let system = request.system.as_deref().unwrap_or_default();
        assert!(system.contains(&format!("section for {own}")), "{system}");
        assert!(
            !system.contains(&format!("section for {other}")),
            "{system}"
        );
    }
}

// "reports a failed definition while allowing healthy definitions and the
// host to start", with `contains a throws plugin activation failure`, and
// dsh's failure line per row.
#[tokio::test]
async fn a_broken_preset_stays_listed_and_refuses_sessions() {
    let f = fixture(
        registry()
            .preset(PresetDefinition::new(
                "broken",
                vec![
                    Box::new(Throws("first-row")),
                    Box::new(Contribute("half")),
                    Box::new(Throws("second-row")),
                ],
            ))
            .preset(contribution("standard")),
        Vec::new(),
    )
    .await;
    let broken = f.presets.resolve(Some("broken")).unwrap();
    assert_eq!(
        broken.broken.as_deref(),
        Some("first-row (throws): refused\nsecond-row (throws): refused")
    );
    assert_eq!(f.presets.resolve(None).unwrap().broken, None);

    let error = compose_error(f.agent_on("on-broken", Some("broken")));
    assert_eq!(error.code, "agent-preset/invalid");
    assert_eq!(
        error.message,
        "first-row (throws): refused\nsecond-row (throws): refused"
    );
    assert_eq!(
        error.details,
        json!({
            "agentPreset": "broken",
            "reason": "first-row (throws): refused\nsecond-row (throws): refused",
        })
    );
    let healthy = f.agent_on("healthy", None).unwrap();
    assert_eq!(f.tool_names(&healthy), ["standard"]);

    let roster = f.presets.roster();
    let standard = roster
        .presets
        .iter()
        .find(|row| row.id == "standard")
        .unwrap();
    assert!(standard.is_default);
    let row = roster
        .presets
        .iter()
        .find(|row| row.id == "broken")
        .unwrap();
    assert!(!row.is_default);
    assert!(row.broken.is_some());
    // What the broken preset's plugins registered stays under its scope,
    // where no agent's chain reaches it.
    assert_eq!(
        f.tools
            .schemas_for(&ScopeChain::new(
                Some(karaka_scope::PresetId::new("broken")),
                None
            ))
            .len(),
        1
    );
    assert!(!f.tool_names(&healthy).contains(&"half".to_owned()));
}

// A session naming a preset nobody defined refuses to open, with dsh's
// error and details.
#[tokio::test]
async fn an_unknown_preset_refuses_its_session() {
    let f = fixture(
        registry()
            .preset(contribution("standard"))
            .preset(contribution("minimal")),
        Vec::new(),
    )
    .await;
    let error = compose_error(f.agent_on("lost", Some("absent")));
    assert_eq!(error.code, "agent-preset/not-found");
    assert_eq!(error.message, "Unknown agent preset: absent");
    assert_eq!(
        error.details,
        json!({ "agentPreset": "absent", "available": ["standard", "minimal"] })
    );
    assert_eq!(
        f.presets.resolve(Some("absent")).unwrap_err(),
        PresetError::NotFound {
            agent_preset: "absent".into(),
            available: vec!["standard".into(), "minimal".into()],
        }
    );
}

// With no preset for the default, a new session names none and refuses to
// open.
#[tokio::test]
async fn a_missing_default_refuses_new_sessions() {
    let f = fixture(registry().preset(contribution("minimal")), Vec::new()).await;
    let error = compose_error(f.agent_on("fresh", None));
    assert_eq!(error.message, "Unknown agent preset: standard");
    assert_eq!(header_preset(&f, "fresh"), None);
    assert!(f.agent_on("chosen", Some("minimal")).is_ok());
}

// "rejects duplicate IDs", and the empty id of "refuses an unscoped
// binding": here a definition mistake fails the start.
#[tokio::test]
async fn a_duplicate_or_empty_id_fails_the_start() {
    for (plugin, message) in [
        (
            registry()
                .preset(contribution("standard"))
                .preset(contribution("standard")),
            "Duplicate agent preset: standard",
        ),
        (
            registry().preset(PresetDefinition::new(" ", Vec::new())),
            "Preset id must not be empty",
        ),
    ] {
        let error = Runtime::start(host(plugin))
            .await
            .err()
            .expect("the start fails");
        assert!(error.to_string().contains(message), "{error}");
    }
}

// "keeps a row waiting for an absent service mounted": here the kernel
// finds the missing port before anything starts.
#[tokio::test]
async fn a_preset_plugin_needing_a_missing_port_fails_the_start() {
    let plugin = registry().preset(PresetDefinition::new(
        "invalid",
        vec![Box::new(NeedsMissing)],
    ));
    let error = Runtime::start(host(plugin))
        .await
        .err()
        .expect("the start fails");
    assert!(error.to_string().contains("Absent"), "{error}");
}

// "logs selection and rejects preset changes after the first turn", and a
// selection changes the agent's chain at once.
#[tokio::test]
async fn a_blank_session_selects_and_a_started_one_is_locked() {
    let f = fixture(
        registry()
            .preset(contribution("standard"))
            .preset(contribution("minimal")),
        Vec::new(),
    )
    .await;
    let agent = f.agent_on("selection", None).unwrap();
    assert_eq!(f.tool_names(&agent), ["standard"]);
    assert_eq!(
        f.presets.select(&f.session(&agent), "minimal").unwrap(),
        "minimal"
    );
    assert_eq!(f.recorded(&agent).as_deref(), Some("minimal"));
    assert_eq!(f.tool_names(&agent), ["minimal"]);

    f.session(&agent)
        .append(SessionEvent::turn_start(1))
        .unwrap();
    let error = f
        .presets
        .select(&f.session(&agent), "standard")
        .unwrap_err();
    assert_eq!(error.to_string(), "This session has already started");
    assert_eq!(error.code(), Some("agent-preset/locked"));
    assert_eq!(
        error.details(),
        json!({ "sessionId": "selection", "agentPreset": "standard" })
    );
}

// "serializes competing blank-session selections and recovers after a
// failed selection"
#[tokio::test]
async fn a_failed_selection_leaves_the_session_selectable() {
    let f = fixture(
        registry()
            .preset(contribution("standard"))
            .preset(contribution("minimal"))
            .preset(PresetDefinition::new(
                "broken",
                vec![Box::new(Throws("row"))],
            )),
        Vec::new(),
    )
    .await;
    let agent = f.agent_on("competing", None).unwrap();
    let session = f.session(&agent);
    assert_eq!(
        f.presets.select(&session, "absent").unwrap_err().code(),
        Some("agent-preset/not-found")
    );
    assert_eq!(
        f.presets.select(&session, "broken").unwrap_err(),
        PresetError::Invalid {
            agent_preset: "broken".into(),
            reason: "row (throws): refused".into(),
        }
    );
    assert_eq!(f.presets.select(&session, "minimal").unwrap(), "minimal");
    assert_eq!(f.presets.select(&session, "standard").unwrap(), "standard");
    assert_eq!(f.recorded(&agent).as_deref(), Some("standard"));
}

// "keeps a blank Session that already runs the current revision of the
// selected preset without logging"
#[tokio::test]
async fn selecting_the_recorded_preset_logs_nothing() {
    let f = fixture(
        registry()
            .preset(contribution("standard"))
            .preset(contribution("minimal")),
        Vec::new(),
    )
    .await;
    let agent = f.agent_on("repeated", None).unwrap();
    assert_eq!(
        f.presets.select(&f.session(&agent), "minimal").unwrap(),
        "minimal"
    );
    assert_eq!(
        f.presets.select(&f.session(&agent), "minimal").unwrap(),
        "minimal"
    );
    assert_eq!(f.selections(&agent), 1);
    // The header's preset counts as recorded too.
    assert_eq!(
        f.presets.select(&f.session(&agent), "standard").unwrap(),
        "standard"
    );
    let created = f.agent_on("created", Some("minimal")).unwrap();
    assert_eq!(
        f.presets.select(&f.session(&created), "minimal").unwrap(),
        "minimal"
    );
    assert_eq!(f.selections(&created), 0);
}

// "logs the default preset a blank Session runs once it is selected
// without a recorded preset"
#[tokio::test]
async fn a_session_recording_no_preset_runs_the_default_until_one_is_selected() {
    let f = fixture(registry().preset(contribution("standard")), Vec::new()).await;
    // Made outside the loop, so its header names no preset.
    f.sessions.create(SessionId::new("unrecorded")).unwrap();
    let agent = f.agents.open(SessionId::new("unrecorded")).unwrap();
    assert_eq!(f.recorded(&agent), None);
    assert_eq!(f.tool_names(&agent), ["standard"]);

    assert_eq!(
        f.presets.select(&f.session(&agent), "standard").unwrap(),
        "standard"
    );

    assert_eq!(f.recorded(&agent).as_deref(), Some("standard"));
    assert_eq!(f.selections(&agent), 1);
}

// "inventories ... declared display metadata": the list, by order then id.
#[tokio::test]
async fn presets_list_by_order_then_id() {
    let mut standard = contribution("standard");
    standard.name = Some("Standard".into());
    standard.description = Some("General".into());
    standard.order = Some(2.0);
    let mut empty = PresetDefinition::new("empty", Vec::new());
    empty.order = Some(1.0);
    let f = fixture(
        registry()
            .preset(contribution("zeta"))
            .preset(standard)
            .preset(contribution("alpha"))
            .preset(empty),
        Vec::new(),
    )
    .await;
    let ids: Vec<String> = f.presets.list().into_iter().map(|row| row.id).collect();
    assert_eq!(ids, ["empty", "standard", "alpha", "zeta"]);
    assert_eq!(f.presets.default_id(), "standard");
    assert_eq!(
        serde_json::to_value(f.presets.roster()).unwrap()["presets"][1],
        json!({ "id": "standard", "isDefault": true, "name": "Standard", "description": "General" })
    );
    // `resolve` gives only the id and why it's broken, as dsh's does.
    assert_eq!(
        serde_json::to_value(f.presets.resolve(None).unwrap()).unwrap(),
        json!({ "id": "standard" })
    );
}

// session.spec.ts: "starts from the creation header, including no
// configured preset" and "starts from the header and keeps the latest
// selected preset".
#[tokio::test]
async fn the_projection_folds_the_header_and_the_latest_selection() {
    let f = fixture(registry().preset(contribution("standard")), Vec::new()).await;
    let none = f.sessions.create(SessionId::new("none")).unwrap();
    assert_eq!(
        f.projections
            .state_of::<AgentPresetProjection>(&none)
            .map(|state| (*state).clone()),
        Some(None)
    );
    let mut header = SessionHeader::new(SessionId::new("named"), 1);
    header.agent_preset = Some("standard".into());
    let named = f.sessions.create_with(header).unwrap();
    let state = |session: &Session| {
        f.projections
            .state_of::<AgentPresetProjection>(session)
            .and_then(|state| (*state).clone())
    };
    assert_eq!(state(&named).as_deref(), Some("standard"));
    named
        .append(karaka_agent_presets::selected("minimal"))
        .unwrap();
    named.append(SessionEvent::turn_start(1)).unwrap();
    named
        .append(karaka_agent_presets::selected("cordis"))
        .unwrap();
    assert_eq!(state(&named).as_deref(), Some("cordis"));
    let logged = serde_json::to_value(&named.events()[0]).unwrap();
    assert_eq!(logged["type"], SELECTED_EVENT);
    assert_eq!(logged["data"], json!({ "agentPreset": "minimal" }));
}
