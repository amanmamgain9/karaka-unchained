//! The tool registry, started in a runtime by `ToolsPlugin`, with fake tools.

use std::future::{pending, poll_fn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use karaka_agent::{Agent, FrameListener, FrameListeners, FrameSubscription, InboxError, WhenIdle};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId, Runtime};
use karaka_model::{ToolCall, ToolSchema};
use karaka_session::{CancelCause, SessionId, UserMessage};
use karaka_tools::{
    Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRegistryError, ToolRun, ToolsPlugin,
};
use serde_json::Value;

fn schema(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: format!("The {name} tool."),
        parameters: r#"{"type":"object","properties":{}}"#.into(),
    }
}

/// Answers with its arguments and which call it ran for, so a test can see
/// both arrived.
struct Echo;

impl Tool for Echo {
    fn schema(&self) -> ToolSchema {
        schema("echo")
    }

    fn execute<'a>(&'a self, arguments: Value, run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move { ToolOutput::ok(format!("{} {} {arguments}", run.call_id, run.name)) })
    }
}

/// Always fails, as a tool does when the thing it calls is down.
struct Broken;

impl Tool for Broken {
    fn schema(&self) -> ToolSchema {
        schema("broken")
    }

    fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async { ToolOutput::error("the weather service is down") })
    }
}

/// Never finishes. It sets `dropped` when its future is dropped, the way a
/// tool would kill a child process it started.
struct Stuck {
    dropped: Arc<AtomicBool>,
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Tool for Stuck {
    fn schema(&self) -> ToolSchema {
        schema("stuck")
    }

    fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        let guard = SetOnDrop(Arc::clone(&self.dropped));
        Box::pin(async move {
            let _guard = guard;
            pending::<ToolOutput>().await
        })
    }
}

fn call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: "call_1".into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

#[tokio::test]
async fn a_registered_tool_runs_with_its_parsed_arguments() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Echo)).unwrap();

    let output = registry
        .execute(&call("echo", r#"{"text": "ping"}"#), None)
        .await;
    assert_eq!(output, ToolOutput::ok(r#"call_1 echo {"text":"ping"}"#));
}

#[tokio::test]
async fn empty_arguments_reach_the_tool_as_an_empty_object() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Echo)).unwrap();

    let output = registry.execute(&call("echo", ""), None).await;
    assert_eq!(output, ToolOutput::ok("call_1 echo {}"));
}

#[test]
fn schemas_come_in_the_order_the_tools_were_registered() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Broken)).unwrap();
    registry.register(Arc::new(Echo)).unwrap();
    assert_eq!(registry.schemas(), [schema("broken"), schema("echo")]);
    assert_eq!(
        format!("{registry:?}"),
        r#"ToolRegistry { tools: ["broken", "echo"], scopes: 0 }"#
    );
}

#[test]
fn a_second_tool_with_the_same_name_is_rejected() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Echo)).unwrap();

    let error = registry
        .register(Arc::new(Echo))
        .expect_err("the name is taken");
    assert_eq!(error, ToolRegistryError::DuplicateName("echo".into()));
    assert_eq!(
        error.to_string(),
        "tool \"echo\" is already registered (for a per-agent variant, register it in that \
         agent's scope instead)"
    );
    assert_eq!(registry.schemas().len(), 1);
}

/// dsh's wording for a `ToolNotFoundError`.
#[tokio::test]
async fn an_unknown_tool_is_an_error_for_the_model() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Echo)).unwrap();

    let output = registry.execute(&call("weather", "{}"), None).await;
    assert_eq!(output, ToolOutput::error("Error: unknown tool \"weather\""));
}

/// dsh's wording for the `ToolArgsError` a typed tool throws on arguments
/// that aren't an object.
#[tokio::test]
async fn arguments_that_arent_a_json_object_are_an_error_for_the_model() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Echo)).unwrap();

    for raw in ["not json", r#"{"text": "pi"#, "[1]"] {
        let output = registry.execute(&call("echo", raw), None).await;
        assert_eq!(
            output,
            ToolOutput::error("Error: invalid arguments: \"arguments\" must be an object"),
            "{raw:?}"
        );
    }
}

#[tokio::test]
async fn a_name_nobody_registered_is_reported_before_its_arguments() {
    let registry = ToolRegistry::new();
    let output = registry.execute(&call("weather", "not json"), None).await;
    assert_eq!(output, ToolOutput::error("Error: unknown tool \"weather\""));
}

#[tokio::test]
async fn a_tools_own_failure_reaches_the_model_unchanged() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Broken)).unwrap();

    let output = registry.execute(&call("broken", "{}"), None).await;
    assert_eq!(output, ToolOutput::error("the weather service is down"));
    let result = output.into_result("call_1");
    assert!(result.is_error);
    assert_eq!(result.call_id, "call_1");
}

#[tokio::test]
async fn dropping_the_call_cancels_the_tool() {
    let dropped = Arc::new(AtomicBool::new(false));
    let registry = ToolRegistry::new();
    registry
        .register(Arc::new(Stuck {
            dropped: Arc::clone(&dropped),
        }))
        .unwrap();

    let mut running = registry.execute(&call("stuck", "{}"), None);
    // Polled once, so the tool has started, and it is still waiting.
    assert!(poll_once(&mut running).await.is_pending());
    assert!(!dropped.load(Ordering::SeqCst));
    drop(running);
    assert!(dropped.load(Ordering::SeqCst));
}

/// Polls `future` once, and says whether it was ready.
async fn poll_once(future: &mut ToolFuture<'static>) -> Poll<ToolOutput> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

/// A plugin with a tool, as a real one would register it.
struct EchoPlugin;

impl Plugin for EchoPlugin {
    fn name(&self) -> &str {
        "echo"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ToolRegistry>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.get::<ToolRegistry>().register(Arc::new(Echo))?;
        Ok(())
    }
}

#[tokio::test]
async fn a_plugin_registers_its_tool_in_the_port() {
    let runtime = Runtime::start(vec![
        Box::new(EchoPlugin),
        Box::new(ToolsPlugin),
        Box::new(karaka_system_prompt::SystemPromptPlugin::default()),
    ])
    .await
    .expect("the runtime starts");
    // The registry's plugin starts before the tool's, though it was listed
    // second, and after the system prompt it requires.
    assert_eq!(runtime.plugins(), ["system-prompt", "tools", "echo"]);

    let registry = runtime.get::<ToolRegistry>();
    assert_eq!(registry.schemas(), [schema("echo")]);
    let output = registry.execute(&call("echo", "{}"), None).await;
    assert_eq!(output.content, "call_1 echo {}");

    // The registry is a tool source of the system prompt, as dsh's tool
    // runtime is.
    let assembly = runtime
        .get::<karaka_system_prompt::SystemPrompt>()
        .assemble(&karaka_system_prompt::AssembleContext::default())
        .await
        .unwrap();
    assert_eq!(assembly.tools, [schema("echo")]);
}

/// Compiles only if the registry can be shared between threads, as a port
/// must be, and a call can move between them.
#[test]
fn the_registry_and_its_calls_work_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>(_: T) {}

    assert_send_sync::<ToolRegistry>();
    let registry = ToolRegistry::new();
    assert_send(registry.execute(&call("echo", "{}"), None));
}

/// Says which agent called it, if any.
struct Caller;

impl Tool for Caller {
    fn schema(&self) -> ToolSchema {
        schema("caller")
    }

    fn execute<'a>(&'a self, _arguments: Value, run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            ToolOutput::ok(match &run.agent {
                Some(agent) => agent.id().to_string(),
                None => "no agent".into(),
            })
        })
    }
}

/// An agent that only has an id.
struct Idle {
    id: SessionId,
    frames: FrameListeners,
}

impl Agent for Idle {
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
}

// dsh's `ToolExecutionInput.agent`: the calling agent reaches the tool, and
// a call made outside any agent has none.
#[tokio::test]
async fn a_tool_is_told_which_agent_called_it() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Caller)).unwrap();
    let agent: Arc<dyn Agent> = Arc::new(Idle {
        id: SessionId::new("a1"),
        frames: FrameListeners::new(),
    });
    let output = registry
        .execute(&call("caller", "{}"), Some(Arc::clone(&agent)))
        .await;
    assert_eq!(output, ToolOutput::ok("a1"));
    let output = registry.execute(&call("caller", "{}"), None).await;
    assert_eq!(output, ToolOutput::ok("no agent"));
    // The call keeps no hold on the agent once it is done.
    assert_eq!(Arc::strong_count(&agent), 1);
}

/// A tool whose parameters require a string `path`, and which answers with
/// it. (dsh: the `reader` fixture of `tools.spec.ts`.)
struct Reader {
    /// Whether the registry checks its arguments.
    checked: bool,
    parameters: &'static str,
}

impl Reader {
    const PARAMETERS: &'static str = r#"{"type":"object","properties":{"path":{"type":"string"},"count":{"type":"integer"}},"required":["path"]}"#;

    fn checked() -> Self {
        Self {
            checked: true,
            parameters: Self::PARAMETERS,
        }
    }
}

impl Tool for Reader {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "reader".into(),
            description: "reads a path".into(),
            parameters: self.parameters.into(),
        }
    }

    fn registry_checks_arguments(&self) -> bool {
        self.checked
    }

    fn execute<'a>(&'a self, arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move { ToolOutput::ok(format!("read {arguments}")) })
    }
}

// tools.spec: "returns an isError result with the violations when the
// model sends bad args", and "runs execute normally when args are valid"
#[tokio::test]
async fn arguments_are_checked_against_the_parameters_before_the_tool_runs() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(Reader::checked())).unwrap();
    let output = registry.execute(&call("reader", "{}"), None).await;
    assert_eq!(
        output,
        ToolOutput::error("Error: invalid arguments: missing required property \"path\"")
    );
    let output = registry
        .execute(&call("reader", r#"{"count":1.5,"path":2}"#), None)
        .await;
    assert_eq!(
        output,
        ToolOutput::error(
            "Error: invalid arguments: \"path\" must be a string; \"count\" must be an integer"
        )
    );
    let output = registry
        .execute(&call("reader", r#"{"path":"/x"}"#), None)
        .await;
    assert_eq!(output, ToolOutput::ok(r#"read {"path":"/x"}"#));
}

// A tool that checks its own arguments, as dsh's raw `ToolDefinition`s
// (MCP's among them) do, gets them unchecked, and may use any JSON Schema.
#[tokio::test]
async fn a_tool_that_opts_out_gets_its_arguments_unchecked() {
    let registry = ToolRegistry::new();
    registry
        .register(Arc::new(Reader {
            checked: false,
            parameters: r#"{"type":"object","properties":{"path":{"type":"string","minLength":1}}}"#,
        }))
        .unwrap();
    let output = registry
        .execute(&call("reader", r#"{"path":2}"#), None)
        .await;
    assert_eq!(output, ToolOutput::ok(r#"read {"path":2}"#));
    // Arguments that aren't an object are still refused.
    let output = registry.execute(&call("reader", "[1]"), None).await;
    assert!(output.is_error);
}

// dsh's `defineTool` refuses parameters outside the subset when the tool is
// defined; here, when it is registered.
#[test]
fn a_checked_tool_needs_parameters_in_the_subset() {
    let registry = ToolRegistry::new();
    let error = registry
        .register(Arc::new(Reader {
            checked: true,
            parameters: r#"{"type":"object","properties":{"path":{"type":"string","minLength":1}}}"#,
        }))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "tool \"reader\": unsupported JSON schema: schema.properties.path.minLength is not a \
         supported keyword (subset: \
         type/oneOf/properties/required/additionalProperties/items/enum/const + annotations)"
    );
    let error = registry
        .register(Arc::new(Reader {
            checked: true,
            parameters: "not json",
        }))
        .unwrap_err();
    assert!(
        matches!(&error, ToolRegistryError::ParametersNotJson { name, .. } if name == "reader"),
        "{error}"
    );
    assert!(registry.schemas().is_empty());
}
