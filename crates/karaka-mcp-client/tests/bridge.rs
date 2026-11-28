//! The tool bridge and the plugin's start, against scripted servers. Ported
//! from dsh's `mcp-client.spec.ts` ("publicToolName", "syncTools", "tool
//! execution", "non-object args fallback"), `tool-definition.spec.ts` and
//! `apply.spec.ts`.
//!
//! Not ported: the image-admission cases that need an attachment store and
//! an image-capable route (Karaka has neither, so every image is refused
//! as dsh refuses it without a store, which `images_are_refused_as_text`
//! shows); the post-execute replacement and `projectContent` cases, which
//! belong to dsh's richer results; `createTransport`'s cases, which are the
//! transport tests of step A; `apply.spec`'s Schemastery cases, which are
//! `McpServerConfig::from_json`'s here; and the Cordis lifecycle cases
//! (an owner unloading during the initial connection, the effect
//! disposer's identity).

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Factory, Knobs, Reply, Script, legacy_server, wait_for};
use karaka_mcp_client::{
    McpClients, McpServer, McpServerConfig, McpStartError, StdioParams, TransportConfig,
    public_tool_name,
};
use karaka_model::{ToolCall, ToolSchema};
use karaka_scope::{PresetId, ScopeChain, ScopeKey};
use karaka_system_prompt::{AssembleContext, SystemPrompt, SystemPromptConfig};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun};
use serde_json::{Value, json};

struct Host {
    clients: Arc<McpClients>,
    tools: Arc<ToolRegistry>,
    prompt: Arc<SystemPrompt>,
}

fn host() -> Host {
    Host {
        clients: Arc::new(McpClients::new(None)),
        tools: Arc::new(ToolRegistry::new()),
        prompt: Arc::new(SystemPrompt::new(SystemPromptConfig::default()).unwrap()),
    }
}

fn config(name: &str) -> McpServerConfig {
    McpServerConfig::new(
        name,
        TransportConfig::Stdio(StdioParams {
            command: "unused".into(),
            ..StdioParams::default()
        }),
    )
}

impl Host {
    async fn start(
        &self,
        scope: Option<ScopeKey>,
        config: McpServerConfig,
        factory: &Factory,
    ) -> Result<McpServer, McpStartError> {
        self.clients
            .start_with(
                scope,
                config,
                factory.factory(),
                Arc::clone(&self.tools),
                Arc::clone(&self.prompt),
            )
            .await
    }

    fn names(&self) -> Vec<String> {
        self.tools
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect()
    }

    async fn call(&self, name: &str, arguments: Value) -> ToolOutput {
        let call = ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.to_string(),
        };
        self.tools.execute(&call, None).await
    }

    fn lines(&self, part: &str) -> bool {
        self.clients
            .lines()
            .iter()
            .any(|(_, line)| line.contains(part))
    }
}

/// A 2025-era server listing `tools`, answering `tools/call` with `result`.
fn server(tools: Value, result: Value) -> Script {
    Arc::new(
        move |message: &Value| match message["method"].as_str().unwrap_or_default() {
            "server/discover" => Reply::Error(-32601, "Method not found"),
            "initialize" => Reply::Result(json!({
                "protocolVersion": "2025-11-25",
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "fixture", "version": "1" },
            })),
            "tools/list" => Reply::Result(json!({ "tools": tools.clone() })),
            "tools/call" => Reply::Result(result.clone()),
            _ => Reply::Silent,
        },
    )
}

fn tool(name: &str) -> Value {
    json!({ "name": name, "inputSchema": { "type": "object" } })
}

fn text(value: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": value }] })
}

// "publicToolName"
#[test]
fn public_names_follow_dshs_rules() {
    assert_eq!(
        public_tool_name("github", "create_issue"),
        "mcp__github__create_issue"
    );
    assert_eq!(
        public_tool_name("everything", "get-sum"),
        "mcp__everything__get-sum"
    );

    let dotted = public_tool_name("srv", "admin.reset");
    let (base, hash) = dotted.rsplit_once('_').unwrap();
    assert_eq!(base, "mcp__srv__admin_reset");
    assert_eq!(hash.len(), 12);
    assert!(
        hash.bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );

    let long = public_tool_name("srv", &"a".repeat(80));
    assert_eq!(long.len(), 64);
    assert!(long.starts_with("mcp__srv__aaa"));
    assert_eq!(long.as_bytes()[51], b'_');

    assert_eq!(dotted, public_tool_name("srv", "admin.reset"));
    assert_ne!(dotted, public_tool_name("srv", "admin_reset"));
    assert_eq!(
        public_tool_name("srv", "admin_reset"),
        "mcp__srv__admin_reset"
    );
    // JavaScript replaces each UTF-16 unit.
    assert!(public_tool_name("srv", "😀").starts_with("mcp__srv_____"));
}

// "registers tools under server-qualified public names", "lets two servers
// publish the same raw name side by side", "coexists with a native tool of
// the same raw name"
#[tokio::test]
async fn tools_are_registered_under_their_servers_names() {
    struct Native;
    impl Tool for Native {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "search".into(),
                description: "native".into(),
                parameters: "{}".into(),
            }
        }
        fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
            Box::pin(async { ToolOutput::ok("native") })
        }
    }
    let host = host();
    host.tools.register(Arc::new(Native)).unwrap();
    let one = Factory::new(
        server(json!([tool("search")]), text("one")),
        Knobs::default(),
    );
    let two = Factory::new(
        server(json!([tool("search")]), text("two")),
        Knobs::default(),
    );
    let _one = host.start(None, config("one"), &one).await.unwrap();
    let _two = host.start(None, config("two"), &two).await.unwrap();
    let mut names = host.names();
    names.sort();
    assert_eq!(names, ["mcp__one__search", "mcp__two__search", "search"]);
    assert_eq!(
        host.call("mcp__one__search", json!({})).await,
        ToolOutput::ok("one")
    );
    assert_eq!(
        host.call("mcp__two__search", json!({})).await,
        ToolOutput::ok("two")
    );
    assert_eq!(
        host.call("search", json!({})).await,
        ToolOutput::ok("native")
    );
}

// "rejects a tool list where one raw name appears twice", and the strict
// start it fails.
#[tokio::test]
async fn a_raw_name_listed_twice_is_an_invalid_list() {
    let host = host();
    let factory = Factory::new(
        server(json!([tool("x"), tool("x")]), text("")),
        Knobs::default(),
    );
    let mut strict = config("srv");
    strict.fail_on_startup_error = true;
    strict.reconnect = Some(json!({ "enabled": false }));
    let error = host.start(None, strict, &factory).await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "mcp-client(srv): initial connection or tool synchronization failed: mcp-client(srv): server listed tool \"x\" more than once — invalid tool list"
    );
    assert!(host.names().is_empty());
    // A failed strict start closed its transport.
    assert!(factory.made()[0].closes.load(Ordering::SeqCst) > 0);
}

// "keeps the previous generation when the fetch phase fails", "unregisters
// previous tools before re-syncing", and "re-syncs tools on
// ToolListChanged notification".
#[tokio::test]
async fn a_change_replaces_the_generation_and_a_failed_fetch_keeps_it() {
    let host = host();
    let factory = Factory::new(server(json!([tool("first")]), text("")), Knobs::default());
    let server_handle = host.start(None, config("srv"), &factory).await.unwrap();
    assert_eq!(host.names(), ["mcp__srv__first"]);
    let scripted = factory.made()[0].clone();
    scripted.set_script(server(json!([tool("second")]), text("")));
    scripted.push(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
    wait_for(|| host.names() == ["mcp__srv__second"]).await;
    assert_eq!(server_handle.tools(), ["mcp__srv__second"]);

    scripted.set_script(Arc::new(|message: &Value| {
        match message["method"].as_str() {
            Some("tools/list") => Reply::Error(-32603, "list failed"),
            _ => Reply::Silent,
        }
    }));
    scripted.push(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
    wait_for(|| host.lines("tool re-sync failed")).await;
    assert!(host.lines("mcp-client(srv): tool re-sync failed: ProtocolError: list failed"));
    assert_eq!(host.names(), ["mcp__srv__second"]);
}

// "rolls back the whole generation when a foreign tool squats on the
// namespace", and "rejects strict startup when the initial tool generation
// cannot be registered".
#[tokio::test]
async fn a_squatted_name_rolls_the_generation_back() {
    struct Squatter;
    impl Tool for Squatter {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "mcp__srv__b".into(),
                description: String::new(),
                parameters: "{}".into(),
            }
        }
        fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
            Box::pin(async { ToolOutput::ok("squat") })
        }
    }
    let host = host();
    host.tools.register(Arc::new(Squatter)).unwrap();
    let factory = Factory::new(
        server(json!([tool("a"), tool("b")]), text("")),
        Knobs::default(),
    );
    let _server = host.start(None, config("srv"), &factory).await.unwrap();
    assert_eq!(host.names(), ["mcp__srv__b"]);
    assert!(host.lines(
        "mcp-client(srv): tool registration failed, no tools registered: Error: tool \"mcp__srv__b\" is already registered"
    ));

    let strict_host = self::host();
    strict_host.tools.register(Arc::new(Squatter)).unwrap();
    let mut strict = config("srv");
    strict.fail_on_startup_error = true;
    strict.reconnect = Some(json!({ "enabled": false }));
    let factory = Factory::new(
        server(json!([tool("a"), tool("b")]), text("")),
        Knobs::default(),
    );
    let error = strict_host
        .start(None, strict, &factory)
        .await
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .starts_with("mcp-client(srv): initial connection or tool synchronization failed: tool \"mcp__srv__b\" is already registered"),
        "{error}"
    );
}

// "calls MCP callTool with the RAW name and returns text content", "sends
// the raw name for normalized public names", "joins multiple text blocks
// with newline", and the non-object arguments fallback.
#[tokio::test]
async fn calls_send_the_raw_name_and_read_the_text() {
    let host = host();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&seen);
    let base = server(
        json!([tool("admin.reset")]),
        json!({ "content": [{ "type": "text", "text": "one" }, { "type": "text", "text": "two" }] }),
    );
    let factory = Factory::new(
        Arc::new(move |message: &Value| {
            if message["method"] == "tools/call" {
                kept.lock().unwrap().push(message["params"].clone());
            }
            base(message)
        }),
        Knobs::default(),
    );
    let _server = host.start(None, config("srv"), &factory).await.unwrap();
    let name = public_tool_name("srv", "admin.reset");
    assert_eq!(
        host.call(&name, json!({ "a": 1 })).await,
        ToolOutput::ok("one\ntwo")
    );
    let call = ToolCall {
        id: "c2".into(),
        name: name.clone(),
        arguments: "null".into(),
    };
    host.tools.execute(&call, None).await;
    let call = ToolCall {
        id: "c3".into(),
        name,
        arguments: "\"just text\"".into(),
    };
    host.tools.execute(&call, None).await;
    assert_eq!(
        *seen.lock().unwrap(),
        [
            json!({ "name": "admin.reset", "arguments": { "a": 1 } }),
            json!({ "name": "admin.reset", "arguments": {} }),
            json!({ "name": "admin.reset", "arguments": {} }),
        ]
    );
}

/// Starts a server with one tool answering `result`, and calls it.
async fn call_with(definition: Value, result: Value) -> ToolOutput {
    let host = host();
    let name = definition["name"].as_str().unwrap().to_owned();
    let factory = Factory::new(server(json!([definition]), result), Knobs::default());
    let _server = host.start(None, config("srv"), &factory).await.unwrap();
    host.call(&public_tool_name("srv", &name), json!({})).await
}

// The text projection of each kind of block, in dsh's words.
#[tokio::test]
async fn each_block_kind_reads_as_dshs_text() {
    for (content, expected) in [
        (
            json!([{ "type": "audio", "data": "AAAA", "mimeType": "audio/wav" }]),
            "[audio result unsupported: audio/wav; raw audio data remains available to programmatic callers]",
        ),
        (
            json!([{ "type": "resource", "resource": { "uri": "memo://a", "text": "x" } }]),
            "[embedded resource unsupported; raw resource data remains available to programmatic callers]",
        ),
        (
            json!([{ "type": "resource_link", "name": "readme", "uri": "memo://readme" }]),
            "Resource link: readme (memo://readme)",
        ),
        (json!([]), "(t returned no model-visible content)"),
    ] {
        assert_eq!(
            call_with(tool("t"), json!({ "content": content })).await,
            ToolOutput::ok(expected)
        );
    }
}

// Karaka has no attachment store, so every image is refused as dsh refuses
// it without one, and an invalid batch with each image's own reason.
// (dsh: "keeps a valid raw image result while explicitly refusing it
// without a durable route", "rejects a malformed image batch before
// storing any member", "rejects unsupported media and base64 aliases".)
#[tokio::test]
async fn images_are_refused_as_text() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    let valid = call_with(
        tool("image"),
        json!({ "content": [
            { "type": "text", "text": "Here is an image:" },
            { "type": "image", "data": png, "mimeType": "image/png" },
            { "type": "text", "text": "End of image." },
        ] }),
    )
    .await;
    assert_eq!(
        valid,
        ToolOutput::ok(
            "Here is an image:\n[image unavailable: image/png; no attachment store is mounted; raw image data remains available to programmatic callers]\nEnd of image."
        )
    );
    let invalid = call_with(
        tool("image"),
        json!({ "content": [
            { "type": "image", "data": png, "mimeType": "image/png" },
            { "type": "image", "data": "a-b_", "mimeType": "image/png" },
            { "type": "image", "data": png, "mimeType": "image/tiff" },
        ] }),
    )
    .await;
    assert_eq!(
        invalid.content,
        [
            "[image unavailable: image/png; another image in the same result was invalid; raw image data remains available to programmatic callers]",
            "[image unavailable: image/png; the image data is not canonical base64; raw image data remains available to programmatic callers]",
            "[image unavailable: image/tiff; the declared media type is not PNG, JPEG, WebP, or GIF; raw image data remains available to programmatic callers]",
        ]
        .join("\n")
    );
}

// "maps isError to an error result via throw", "handles isError with
// non-text content", "rejects tools that require task-based execution",
// "uses tool description when provided" and its empty twin, and a result
// that isn't one. (tool-definition.spec: the invalid results.)
#[tokio::test]
async fn errors_tasks_descriptions_and_invalid_results() {
    assert_eq!(
        call_with(tool("fail"), json!({ "content": [{ "type": "text", "text": "something went wrong" }], "isError": true })).await,
        ToolOutput::error("Error: something went wrong")
    );
    assert_eq!(
        call_with(tool("fail"), json!({ "content": [], "isError": true })).await,
        ToolOutput::error("Error: (fail returned no model-visible content)")
    );
    let task = json!({ "name": "task-only", "inputSchema": { "type": "object" }, "execution": { "taskSupport": "required" } });
    let output = call_with(task, text("never")).await;
    assert_eq!(
        output,
        ToolOutput::error(
            "Error: Tool \"task-only\" requires task-based execution, which this bridge does not support"
        )
    );
    for result in [
        json!(null),
        json!({ "content": [42] }),
        json!({ "content": [{ "type": "image" }] }),
    ] {
        let output = call_with(tool("bad"), result).await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .starts_with("Error: Tool \"bad\" returned an invalid MCP result: "),
            "{}",
            output.content
        );
    }

    let host = host();
    let factory = Factory::new(
        server(
            json!([
                { "name": "described", "description": "Does a thing.", "inputSchema": { "type": "object", "properties": { "x": { "type": "string" } } } },
                tool("bare"),
            ]),
            text(""),
        ),
        Knobs::default(),
    );
    let _server = host.start(None, config("srv"), &factory).await.unwrap();
    let schemas = host.tools.schemas();
    assert_eq!(schemas[0].description, "Does a thing.");
    assert_eq!(
        schemas[0].parameters,
        r#"{"type":"object","properties":{"x":{"type":"string"}}}"#
    );
    assert_eq!(schemas[1].description, "");
}

// "validates structuredContent when the advertised output schema is
// supported", "falls back to JsonValue for unsupported advertised output
// schemas", and the SDK's missing-structured-content check ("validates
// outputs with the complete SDK definition after paginated discovery").
#[tokio::test]
async fn structured_content_is_checked_against_the_output_schema() {
    let schema = json!({
        "type": "object", "additionalProperties": false,
        "properties": { "answer": { "type": "integer" } }, "required": ["answer"],
    });
    let structured = json!({ "name": "structured", "inputSchema": { "type": "object" }, "outputSchema": schema });
    assert_eq!(
        call_with(structured.clone(), json!({ "content": [{ "type": "text", "text": "42" }], "structuredContent": { "answer": 42 } })).await,
        ToolOutput::ok("42")
    );
    let wrong = call_with(structured.clone(), json!({ "content": [{ "type": "text", "text": "wrong" }], "structuredContent": { "answer": "forty-two" } })).await;
    assert!(wrong.is_error);
    assert!(
        wrong
            .content
            .starts_with("Error: tool \"mcp__srv__structured\" returned invalid output: "),
        "{}",
        wrong.content
    );
    assert!(
        wrong.content.contains("value.structuredContent.answer"),
        "{}",
        wrong.content
    );
    assert_eq!(
        call_with(structured, text("missing structured content")).await,
        ToolOutput::error(
            "Error: Tool structured has an output schema but did not return structured content"
        )
    );
    let future = json!({
        "name": "future-schema", "inputSchema": { "type": "object" },
        "outputSchema": { "type": "object", "patternProperties": { "^x-": { "type": "string" } } },
    });
    assert_eq!(
        call_with(future, json!({ "content": [{ "type": "text", "text": "kept" }], "structuredContent": { "x-value": "kept" } })).await,
        ToolOutput::ok("kept")
    );
}

// "passes abort signal to callTool": a dropped call tells the server it
// was cancelled; and `toolCallTimeoutMs` bounds a call.
#[tokio::test]
async fn a_dropped_call_is_cancelled_and_a_slow_one_times_out() {
    let host = host();
    let base = legacy_server(&["wait"]);
    let factory = Factory::new(
        Arc::new(move |message: &Value| match message["method"].as_str() {
            Some("tools/call") => Reply::Silent,
            _ => base(message),
        }),
        Knobs::default(),
    );
    let mut slow = config("srv");
    slow.tool_call_timeout_ms = 30;
    let _server = host.start(None, slow, &factory).await.unwrap();
    assert_eq!(
        host.call("mcp__srv__wait", json!({})).await,
        ToolOutput::error("Error: Request timed out")
    );
    let call = ToolCall {
        id: "c9".into(),
        name: "mcp__srv__wait".into(),
        arguments: "{}".into(),
    };
    let dropped =
        tokio::time::timeout(Duration::from_millis(5), host.tools.execute(&call, None)).await;
    assert!(dropped.is_err());
    let scripted = factory.made()[0].clone();
    wait_for(|| {
        scripted.sent().iter().any(|message| {
            message["method"] == "notifications/cancelled"
                && message["params"]["reason"] == "AbortError: This operation was aborted"
        })
    })
    .await;
}

// apply.spec: "rejects a duplicate serverName at load and leaves the first
// instance intact", "allows one serverName in each independent
// registration scope", "releases the serverName reservation on dispose".
#[tokio::test]
async fn server_names_are_reserved_per_scope() {
    let host = host();
    let factory = Factory::new(legacy_server(&["t"]), Knobs::default());
    let first = host.start(None, config("srv"), &factory).await.unwrap();
    let error = host
        .start(None, config("srv"), &factory)
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "mcp-client: serverName \"srv\" is already in use by another mcp-client instance — pick a unique serverName in cordis.yml"
    );
    assert_eq!(host.names(), ["mcp__srv__t"]);
    let coach = ScopeKey::Preset(PresetId::new("coach"));
    let scoped = host
        .start(Some(coach.clone()), config("srv"), &factory)
        .await
        .unwrap();
    first.dispose().await;
    assert!(host.names().is_empty());
    let again = host.start(None, config("srv"), &factory).await.unwrap();
    again.dispose().await;
    scoped.dispose().await;
}

// A preset's server: its tools and its instructions in the preset's scope
// only. (dsh: an `mcp-client` row inside an agent preset.)
#[tokio::test]
async fn a_presets_server_is_seen_through_its_chain_only() {
    let host = host();
    let factory = Factory::new(
        Arc::new(|message: &Value| match message["method"].as_str() {
            Some("server/discover") => Reply::Error(-32601, "Method not found"),
            Some("initialize") => Reply::Result(json!({
                "protocolVersion": "2025-11-25", "capabilities": { "tools": {} }, "serverInfo": {},
                "instructions": "Use add.",
            })),
            Some("tools/list") => Reply::Result(json!({ "tools": [{ "name": "add" }] })),
            _ => Reply::Silent,
        }),
        Knobs::default(),
    );
    let coach = ScopeChain::new(Some(PresetId::new("coach")), None);
    let _server = host
        .start(
            Some(ScopeKey::Preset(PresetId::new("coach"))),
            config("fixture"),
            &factory,
        )
        .await
        .unwrap();
    assert!(host.tools.schemas().is_empty());
    let names: Vec<String> = host
        .tools
        .schemas_for(&coach)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["mcp__fixture__add"]);
    let section = |context: &AssembleContext| {
        let host = &host;
        let context = context.clone();
        async move {
            host.prompt
                .assemble(&context)
                .await
                .unwrap()
                .sections
                .into_iter()
                .find(|section| section.name == "mcp:fixture")
                .map(|section| (section.text, section.interpolate))
        }
    };
    assert_eq!(
        section(&AssembleContext::scoped(coach)).await,
        Some(("### MCP server: fixture\n\nUse add.".to_owned(), false))
    );
    assert_eq!(section(&AssembleContext::default()).await, None);
}

// "logs error and registers no tools when connect fails", "rejects
// activation and still closes the client when startup failure is
// configured as fatal", and a misconfigured reconnect failing the start.
#[tokio::test]
async fn startup_failures_follow_fail_on_startup_error() {
    let host = host();
    let failing = Factory::new(common::failing_server("refused"), Knobs::default());
    let mut lenient = config("lenient");
    lenient.reconnect = Some(json!({ "enabled": false }));
    let server = host.start(None, lenient, &failing).await.unwrap();
    assert!(host.names().is_empty());
    assert!(host.lines("mcp-client(lenient): connection attempt failed: ProtocolError: refused"));
    server.dispose().await;

    let mut strict = config("strict");
    strict.fail_on_startup_error = true;
    strict.reconnect = Some(json!({ "enabled": false }));
    let error = host.start(None, strict, &failing).await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "mcp-client(strict): initial connection or tool synchronization failed: refused"
    );
    // The name was freed again.
    let mut retry = config("strict");
    retry.reconnect = Some(json!({ "enabled": false }));
    host.start(None, retry, &failing)
        .await
        .unwrap()
        .dispose()
        .await;

    let mut misconfigured = config("srv");
    misconfigured.reconnect = Some(json!({ "initialDelayMs": 100, "maxDelayMs": 5 }));
    assert_eq!(
        host.start(None, misconfigured, &failing)
            .await
            .err()
            .unwrap()
            .to_string(),
        "mcp-client(srv): reconnect.initialDelayMs must be less than or equal to maxDelayMs"
    );
}

// Giving up after the reconnect budget takes the server's tools out of the
// registry; disposal does too. (dsh: "stops at the failure cap, unregisters
// the tools".)
#[tokio::test]
async fn giving_up_or_disposing_unregisters_the_tools() {
    let host = host();
    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let mut config = config("srv");
    config.reconnect = Some(json!({ "initialDelayMs": 1, "maxDelayMs": 2, "maxAttempts": 1 }));
    let server = host.start(None, config, &factory).await.unwrap();
    assert_eq!(host.names(), ["mcp__srv__remote"]);
    factory.set_script(common::failing_server("gone"));
    factory.made()[0].drop_connection();
    wait_for(|| host.names().is_empty()).await;
    assert!(host.lines("giving up after 1 consecutive failed reconnect attempts"));
    server.dispose().await;

    let factory = Factory::new(legacy_server(&["remote"]), Knobs::default());
    let server = host
        .start(None, self::config("other"), &factory)
        .await
        .unwrap();
    assert_eq!(host.names(), ["mcp__other__remote"]);
    server.dispose().await;
    assert!(host.names().is_empty());
}

// The config of dsh's plugin row, and its refusals. (apply.spec: "Config
// schema rejects a missing serverName", "rejects an invalid serverName",
// "accepts a valid serverName".)
#[test]
fn a_config_reads_dshs_row_shape() {
    let stdio = McpServerConfig::from_json(
        &json!({
            "transport": "stdio", "serverName": "fixture", "command": "sh",
            "args": ["server.sh"], "env": { "MODE": "test" }, "cwd": "/tmp",
            "toolCallTimeoutMs": 5000, "failOnStartupError": true, "maxInstructionBytes": 100,
            "reconnect": { "maxAttempts": 2 },
        }),
        "server",
    )
    .unwrap();
    assert_eq!(stdio.server_name, "fixture");
    assert_eq!(stdio.tool_call_timeout_ms, 5000);
    assert!(stdio.fail_on_startup_error);
    assert_eq!(stdio.max_instruction_bytes, 100);
    assert_eq!(stdio.reconnect, Some(json!({ "maxAttempts": 2 })));
    let TransportConfig::Stdio(params) = &stdio.transport else {
        panic!("expected stdio");
    };
    assert_eq!(params.args, ["server.sh"]);
    assert_eq!(params.env.get("MODE").map(String::as_str), Some("test"));

    let http = McpServerConfig::from_json(
        &json!({ "transport": "streamable-http", "serverName": "web", "url": "http://localhost/mcp", "headers": { "x-key": "v" } }),
        "server",
    )
    .unwrap();
    assert_eq!(http.tool_call_timeout_ms, 60_000);
    assert!(!http.fail_on_startup_error);
    assert_eq!(http.max_instruction_bytes, 32_768);

    for (value, message) in [
        (
            json!({ "transport": "stdio", "command": "sh" }),
            "server needs a \"serverName\"",
        ),
        (
            json!({ "transport": "stdio", "serverName": "bad name!", "command": "sh" }),
            "server: \"serverName\" must match /^[A-Za-z0-9_-]{1,32}$/, not \"bad name!\"",
        ),
        (
            json!({ "transport": "pipe", "serverName": "a" }),
            "server: \"transport\" must be \"stdio\" or \"streamable-http\"",
        ),
        (
            json!({ "transport": "stdio", "serverName": "a" }),
            "server needs a \"command\"",
        ),
        (
            json!({ "transport": "stdio", "serverName": "a", "command": "sh", "url": "x" }),
            "server: unknown key \"url\"",
        ),
        (
            json!({ "transport": "streamable-http", "serverName": "a" }),
            "server needs a \"url\"",
        ),
    ] {
        assert_eq!(
            McpServerConfig::from_json(&value, "server").unwrap_err(),
            message
        );
    }
    assert!(
        McpServerConfig::from_json(
            &json!({ "transport": "stdio", "serverName": "a".repeat(32), "command": "sh" }),
            "server"
        )
        .is_ok()
    );
    assert!(
        McpServerConfig::from_json(
            &json!({ "transport": "stdio", "serverName": "a".repeat(33), "command": "sh" }),
            "server"
        )
        .is_err()
    );
}
